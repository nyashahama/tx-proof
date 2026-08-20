use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{
    Method, Request, Response, StatusCode, body::Incoming, header::CONNECTION, server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec, ProcessFaultSpec,
        ProviderOutcome, WebhookFaultSpec,
    },
};
use tiv_reference_app::{CheckoutOperation, create_with_changed_retry_key};
use tiv_runtime::{
    campaign::{CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest, execute_planned_case},
    case_http::{CaseHttpAdapter, CaseHttpError},
    provider_http::{ProviderHttpAdapter, ProviderHttpConfig},
    webhook_http::{WebhookHttpAdapter, WebhookHttpConfig},
};
use tiv_stripe_pi::{
    FaultOutcome, ManagedFixture,
    control::{
        ControlToken, WebhookSigningSecret, WebhookTarget,
        serve_http1_connection as serve_control_connection,
    },
    http::serve_managed_http1_connection,
};
use tokio::{net::TcpListener, sync::Mutex, sync::mpsc, task::JoinHandle, time::timeout};
use uuid::Uuid;

struct CompletingHttpAdapter {
    http: CaseHttpAdapter,
}

impl CaseEffectAdapter for CompletingHttpAdapter {
    type Error = CaseHttpError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        if !matches!(
            request.action().kind(),
            PlanActionKind::WaitForQuiescence | PlanActionKind::CheckCheckpoint { .. }
        ) {
            return self.http.execute(request);
        }

        Box::pin(async move {
            if request.expected_outputs().is_empty() {
                Ok(Vec::new())
            } else {
                Err(CaseHttpError::UnsupportedAction)
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_gates_and_webhooks_share_one_fixture_control_sequence() {
    let plan = held_provider_then_webhook_plan();
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(3))));
    fixture
        .lock()
        .await
        .reset(
            1,
            Seed::new(3),
            vec![FaultOutcome::CommitThenDelay, FaultOutcome::CommitThenDelay],
        )
        .unwrap();
    let (target_address, mut deliveries, target_server) = start_webhook_target().await;
    let (data_address, data_server) = start_data_server(Arc::clone(&fixture), 2).await;
    let (control_address, control_server) =
        start_control_server(Arc::clone(&fixture), target_address).await;
    let (driver_address, driver_server) = start_driver_server(data_address).await;
    let provider_config = ProviderHttpConfig::new(
        format!("http://{driver_address}/checkout"),
        serde_json::json!({
            "database": "tiv_case_0123456789abcdef",
            "operation_id": "op_3",
            "amount_minor": 2500,
            "currency": "usd"
        }),
        format!("http://{driver_address}"),
        format!("http://{control_address}"),
        "case-control-token",
        1,
        Duration::from_secs(2),
        Duration::from_millis(2),
    )
    .unwrap();
    let webhook_config = WebhookHttpConfig::new(
        format!("http://{control_address}"),
        "case-control-token",
        1,
        1_800_000_000,
        Duration::from_secs(2),
    )
    .unwrap();
    let mut adapter = CompletingHttpAdapter {
        http: CaseHttpAdapter::new(
            ProviderHttpAdapter::new(provider_config).unwrap(),
            WebhookHttpAdapter::new(webhook_config).unwrap(),
        ),
    };
    let journal_path = journal_path();

    let executed = execute_planned_case("run_3", "case_3", &plan, &journal_path, &mut adapter)
        .await
        .expect("one sequenced fixture control plane executes the complete HTTP case");

    assert_eq!(executed.trace().action_count(), plan.actions().len());
    timeout(Duration::from_secs(2), deliveries.recv())
        .await
        .unwrap()
        .expect("the exact generated event reaches the application target");
    assert_eq!(fixture.lock().await.snapshot().command_sequence(), 5);

    drop(adapter);
    control_server.abort();
    let _ = control_server.await;
    target_server.abort();
    let _ = target_server.await;
    timeout(Duration::from_secs(2), data_server)
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(2), driver_server)
        .await
        .unwrap()
        .unwrap();
    tokio::fs::remove_file(journal_path).await.unwrap();
}

fn held_provider_then_webhook_plan() -> tiv_core::plan::PlannedCase {
    let spec = PlanSpec::new_payment_intent_v1(
        Seed::new(3),
        ActionBudget::new(40).unwrap(),
        [ProviderOutcome::Normal, ProviderOutcome::CommitThenDelay],
        WebhookFaultSpec::new(0, [], false, false).unwrap(),
        ProcessFaultSpec::new([], 0).unwrap(),
    )
    .unwrap();
    let plan = CasePlanCompiler::compile(&spec).unwrap();
    assert_eq!(plan.actions().len(), 8);
    assert!(matches!(
        plan.actions()[0].kind(),
        PlanActionKind::DriveCheckout { provider_script }
            if provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay
    ));
    assert!(matches!(
        plan.actions()[1].kind(),
        PlanActionKind::ReleaseProviderGate
    ));
    assert!(matches!(
        plan.actions()[2].kind(),
        PlanActionKind::ConfirmPaymentIntent { provider_script }
            if provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay
    ));
    assert!(matches!(
        plan.actions()[3].kind(),
        PlanActionKind::ReleaseProviderGate
    ));
    assert!(matches!(
        plan.actions()[4].kind(),
        PlanActionKind::GenerateProviderEvent
    ));
    assert!(matches!(
        plan.actions()[5].kind(),
        PlanActionKind::DeliverWebhook
    ));
    plan
}

async fn start_data_server(
    fixture: Arc<Mutex<ManagedFixture>>,
    connections: usize,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..connections {
            let (stream, _) = listener.accept().await.unwrap();
            let _result = serve_managed_http1_connection(stream, Arc::clone(&fixture)).await;
        }
    });
    (address, server)
}

async fn start_control_server(
    fixture: Arc<Mutex<ManagedFixture>>,
    target_address: SocketAddr,
) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let target = WebhookTarget::new(
        format!("http://{target_address}/webhooks/stripe"),
        Duration::from_secs(2),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fixture = Arc::clone(&fixture);
            let target = target.clone();
            tokio::spawn(async move {
                let _result = serve_control_connection(
                    stream,
                    fixture,
                    ControlToken::new("case-control-token").unwrap(),
                    WebhookSigningSecret::new(b"whsec_case_test").unwrap(),
                    target,
                )
                .await;
            });
        }
    });
    (address, server)
}

async fn start_driver_server(data_address: SocketAddr) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let fixture_url = format!("http://{data_address}");
            http1::Builder::new()
                .keep_alive(false)
                .serve_connection(
                    TokioIo::new(stream),
                    service_fn(move |request| synthetic_application(request, fixture_url.clone())),
                )
                .await
                .unwrap();
        }
    });
    (address, server)
}

async fn synthetic_application(
    request: Request<Incoming>,
    fixture_url: String,
) -> Result<Response<Full<Bytes>>, SyntheticApplicationError> {
    if request.uri().path().starts_with("/v1/payment_intents/") {
        return proxy_provider_request(request, &fixture_url).await;
    }
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.uri().path(), "/checkout");
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let command: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(command["operation_id"], "op_3");
    let operation = CheckoutOperation::new("op_3", 2_500, "usd").unwrap();
    let result =
        create_with_changed_retry_key(&reqwest::Client::new(), &fixture_url, &operation).await;
    let mut response = if let Ok(payment_intent) = result {
        Response::new(Full::new(Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "payment_intent_id": payment_intent.id(),
                "operation_id": "op_3"
            }))
            .unwrap(),
        )))
    } else {
        let mut response = Response::new(Full::new(Bytes::from_static(b"provider failure")));
        *response.status_mut() = StatusCode::BAD_GATEWAY;
        response
    };
    response
        .headers_mut()
        .insert(CONNECTION, "close".parse().unwrap());
    Ok(response)
}

async fn proxy_provider_request(
    request: Request<Incoming>,
    fixture_url: &str,
) -> Result<Response<Full<Bytes>>, SyntheticApplicationError> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let upstream = reqwest::Client::new()
        .request(method, format!("{fixture_url}{path}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|_| SyntheticApplicationError)?;
    let status = upstream.status();
    let body = upstream
        .bytes()
        .await
        .map_err(|_| SyntheticApplicationError)?;
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    Ok(response)
}

#[derive(Debug)]
struct SyntheticApplicationError;

impl std::fmt::Display for SyntheticApplicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("synthetic application provider proxy failed")
    }
}

impl std::error::Error for SyntheticApplicationError {}

async fn start_webhook_target() -> (SocketAddr, mpsc::Receiver<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel(1);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        http1::Builder::new()
            .serve_connection(
                TokioIo::new(stream),
                service_fn(move |request| record_delivery(request, sender.clone())),
            )
            .await
            .unwrap();
    });
    (address, receiver, server)
}

async fn record_delivery(
    request: Request<Incoming>,
    sender: mpsc::Sender<()>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.uri().path(), "/webhooks/stripe");
    assert!(request.headers().contains_key("stripe-signature"));
    let _body = request.into_body().collect().await.unwrap().to_bytes();
    sender.send(()).await.unwrap();
    Ok(Response::new(Full::new(Bytes::from_static(b"accepted"))))
}

fn journal_path() -> PathBuf {
    std::env::temp_dir().join(format!("tiv-case-http-{}.ndjson", Uuid::new_v4()))
}
