use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{
    Method, Request, Response, StatusCode, body::Incoming, header::CONNECTION, server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec, ProviderOutcome,
        ProviderOutcomeScript,
    },
    trace::{CaseCapturedValue, CaseOutputRef, CaseOutputSlot},
};
use tiv_runtime::{
    campaign::{CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest, execute_planned_case},
    held_checkout::{
        HeldCheckoutHttpAdapter, HeldCheckoutHttpConfig, HeldCheckoutHttpConfigError,
        HeldCheckoutHttpError,
    },
};
use tiv_stripe_pi::{
    FaultOutcome, ManagedFixture,
    control::{
        ControlToken, WebhookSigningSecret, serve_http1_connection as serve_control_connection,
    },
    http::serve_managed_http1_connection,
};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle, time::timeout};
use uuid::Uuid;

struct CompletingAdapter {
    held_checkout: HeldCheckoutHttpAdapter,
}

struct HttpHarness {
    driver_address: SocketAddr,
    control_address: SocketAddr,
    data_server: JoinHandle<()>,
    driver_server: JoinHandle<()>,
    control_server: JoinHandle<()>,
}

impl HttpHarness {
    async fn start() -> Self {
        let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(73))));
        fixture
            .lock()
            .await
            .reset(1, Seed::new(73), vec![FaultOutcome::CommitThenDelay])
            .expect("the one-response fault plan is installed");
        let (data_address, data_server) = start_data_server(Arc::clone(&fixture)).await;
        let (control_address, control_server) = start_control_server(fixture).await;
        let (driver_address, driver_server) = start_driver_server(data_address).await;
        Self {
            driver_address,
            control_address,
            data_server,
            driver_server,
            control_server,
        }
    }

    async fn finish(self) {
        timeout(Duration::from_secs(2), self.data_server)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), self.driver_server)
            .await
            .unwrap()
            .unwrap();
        self.control_server.abort();
        let _ = self.control_server.await;
    }
}

#[test]
fn held_checkout_configuration_rejects_non_loopback_and_ambiguous_inputs() {
    let body = serde_json::json!({
        "operation_id": "op_73",
        "amount_minor": 2500,
        "currency": "usd"
    });
    for driver_url in [
        "https://127.0.0.1:8080/checkout",
        "http://192.0.2.1:8080/checkout",
        "http://user@127.0.0.1:8080/checkout",
        "http://127.0.0.1:8080/checkout?next=http://example.com",
    ] {
        assert!(matches!(
            HeldCheckoutHttpConfig::new(
                driver_url,
                body.clone(),
                "http://127.0.0.1:9090",
                "token",
                1,
                Duration::from_secs(1),
                Duration::from_millis(1),
            ),
            Err(HeldCheckoutHttpConfigError::NonLoopbackUrl)
        ));
    }
    assert!(matches!(
        HeldCheckoutHttpConfig::new(
            "http://127.0.0.1:8080/checkout",
            body.clone(),
            "http://127.0.0.1:9090/control",
            "token",
            1,
            Duration::from_secs(1),
            Duration::from_millis(1),
        ),
        Err(HeldCheckoutHttpConfigError::NonLoopbackUrl)
    ));
    assert!(matches!(
        HeldCheckoutHttpConfig::new(
            "http://127.0.0.1:8080/checkout",
            serde_json::json!({"amount_minor": 2500}),
            "http://127.0.0.1:9090",
            "token",
            1,
            Duration::from_secs(1),
            Duration::from_millis(1),
        ),
        Err(HeldCheckoutHttpConfigError::InvalidDriverBody)
    ));
    assert!(matches!(
        HeldCheckoutHttpConfig::new(
            "http://127.0.0.1:8080/checkout",
            serde_json::json!({"operation_id": "op_73"}),
            "http://127.0.0.1:9090",
            "token",
            1,
            Duration::from_secs(1),
            Duration::from_millis(1),
        ),
        Err(HeldCheckoutHttpConfigError::InvalidDriverBody)
    ));
    assert!(matches!(
        HeldCheckoutHttpConfig::new(
            "http://127.0.0.1:8080/checkout",
            body.clone(),
            "http://127.0.0.1:9090",
            "bad\nheader",
            0,
            Duration::ZERO,
            Duration::ZERO,
        ),
        Err(HeldCheckoutHttpConfigError::InvalidControlToken)
    ));
    assert!(matches!(
        HeldCheckoutHttpConfig::new(
            "http://127.0.0.1:8080/checkout",
            body,
            "http://127.0.0.1:9090",
            "token",
            0,
            Duration::ZERO,
            Duration::ZERO,
        ),
        Err(HeldCheckoutHttpConfigError::InvalidBounds)
    ));
}

impl CaseEffectAdapter for CompletingAdapter {
    type Error = HeldCheckoutHttpError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        let held_business_action = match request.action().kind() {
            PlanActionKind::DriveCheckout { provider_script }
            | PlanActionKind::RetryBusinessRequest { provider_script } => {
                *provider_script == ProviderOutcomeScript::single(ProviderOutcome::CommitThenDelay)
            }
            _ => false,
        };
        if held_business_action
            || matches!(request.action().kind(), PlanActionKind::ReleaseProviderGate)
        {
            return self.held_checkout.execute(request);
        }

        Box::pin(async move {
            Ok(request
                .expected_outputs()
                .iter()
                .copied()
                .map(|output| {
                    let value = match output.slot() {
                        CaseOutputSlot::PaymentIntentId => {
                            CaseCapturedValue::payment_intent_id("pi_tiv_fallback").unwrap()
                        }
                        CaseOutputSlot::EventId => {
                            CaseCapturedValue::event_id("evt_tiv_fallback").unwrap()
                        }
                        CaseOutputSlot::ProviderGateId => {
                            CaseCapturedValue::provider_gate_id(99).unwrap()
                        }
                    };
                    (output, value)
                })
                .collect())
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_real_held_provider_response_is_journaled_before_release_and_driver_completion() {
    let harness = HttpHarness::start().await;
    let plan = held_checkout_plan();
    let first_action_id = plan.actions()[0].id();
    let journal_path = journal_path();
    let config = HeldCheckoutHttpConfig::new(
        format!("http://{}/checkout", harness.driver_address),
        serde_json::json!({
            "database": "tiv_case_0123456789abcdef",
            "operation_id": "op_73",
            "amount_minor": 2500,
            "currency": "usd"
        }),
        format!("http://{}", harness.control_address),
        "case-control-token",
        1,
        Duration::from_secs(2),
        Duration::from_millis(2),
    )
    .expect("all HTTP targets are bounded loopback endpoints");
    let mut adapter = CompletingAdapter {
        held_checkout: HeldCheckoutHttpAdapter::new(config).unwrap(),
    };

    let execution = execute_planned_case("run_73", "case_73", &plan, &journal_path, &mut adapter)
        .await
        .expect("the held checkout reaches every planned boundary");

    let payment_intent = execution
        .trace()
        .resolve(CaseOutputRef::new(
            first_action_id,
            CaseOutputSlot::PaymentIntentId,
        ))
        .expect("the held provider object is captured");
    assert!(matches!(
        payment_intent,
        CaseCapturedValue::PaymentIntentId(id) if id.as_str().starts_with("pi_tiv_")
    ));
    assert_eq!(
        execution.trace().resolve(CaseOutputRef::new(
            first_action_id,
            CaseOutputSlot::ProviderGateId,
        )),
        Some(&CaseCapturedValue::provider_gate_id(1).unwrap())
    );
    assert_eq!(
        execution.journal_summary().record_count(),
        plan.actions().len() * 2 + 2
    );

    let journal = tokio::fs::read_to_string(&journal_path).await.unwrap();
    let records = journal
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records[0]["observation_kind"], "action_intent");
    assert_eq!(records[1]["observation_kind"], "provider_response_held");
    assert_eq!(records[1]["payload"]["gate_id"], 1);
    assert_eq!(records[2]["observation_kind"], "action_outcome");
    assert_eq!(records[3]["observation_kind"], "action_intent");
    assert_eq!(records[4]["observation_kind"], "provider_response_released");
    assert_eq!(records[4]["payload"]["gate_id"], 1);
    assert_eq!(records[5]["observation_kind"], "action_outcome");

    drop(adapter);
    harness.finish().await;
    tokio::fs::remove_file(journal_path).await.unwrap();
}

async fn start_data_server(fixture: Arc<Mutex<ManagedFixture>>) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_managed_http1_connection(stream, fixture)
            .await
            .unwrap();
    });
    (address, server)
}

async fn start_control_server(fixture: Arc<Mutex<ManagedFixture>>) -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fixture = Arc::clone(&fixture);
            tokio::spawn(async move {
                let _result = serve_control_connection(
                    stream,
                    fixture,
                    ControlToken::new("case-control-token").unwrap(),
                    WebhookSigningSecret::new(b"whsec_case_test").unwrap(),
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
        let (stream, _) = listener.accept().await.unwrap();
        let fixture_url = format!("http://{data_address}");
        http1::Builder::new()
            .keep_alive(false)
            .serve_connection(
                TokioIo::new(stream),
                service_fn(move |request| synthetic_checkout(request, fixture_url.clone())),
            )
            .await
            .unwrap();
    });
    (address, server)
}

fn held_checkout_plan() -> tiv_core::plan::PlannedCase {
    let plan = (0..4_096)
        .find_map(|seed| {
            let plan = CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
                Seed::new(seed),
                ActionBudget::new(40).unwrap(),
            ))
            .expect("the candidate held-checkout plan is feasible");
            let held = match plan
                .actions()
                .first()
                .map(tiv_core::plan::PlannedAction::kind)
            {
                Some(PlanActionKind::DriveCheckout { provider_script }) => {
                    *provider_script
                        == ProviderOutcomeScript::single(ProviderOutcome::CommitThenDelay)
                }
                _ => false,
            };
            let immediately_released = matches!(
                plan.actions()
                    .get(1)
                    .map(tiv_core::plan::PlannedAction::kind),
                Some(PlanActionKind::ReleaseProviderGate)
            );
            (held && immediately_released).then_some(plan)
        })
        .expect("the deterministic seed corpus contains a held-checkout plan");
    assert!(matches!(
        plan.actions()
            .get(1)
            .map(tiv_core::plan::PlannedAction::kind),
        Some(PlanActionKind::ReleaseProviderGate)
    ));
    plan
}

fn journal_path() -> PathBuf {
    std::env::temp_dir().join(format!("tiv-held-checkout-{}.ndjson", Uuid::new_v4()))
}

#[derive(Deserialize)]
struct ProviderPaymentIntent {
    id: String,
}

async fn synthetic_checkout(
    request: Request<Incoming>,
    fixture_url: String,
) -> Result<Response<Full<Bytes>>, Infallible> {
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.uri().path(), "/checkout");
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let command: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(command["operation_id"], "op_73");
    let response = reqwest::Client::new()
        .post(format!("{fixture_url}/v1/payment_intents"))
        .header("Connection", "close")
        .header("Idempotency-Key", "op-73-attempt-1")
        .form(&[
            ("amount", "2500"),
            ("currency", "usd"),
            ("metadata[operation_id]", "op_73"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payment_intent = response.json::<ProviderPaymentIntent>().await.unwrap();
    let body = serde_json::to_vec(&serde_json::json!({
        "payment_intent_id": payment_intent.id,
        "operation_id": "op_73"
    }))
    .unwrap();
    let mut response = Response::new(Full::new(Bytes::from(body)));
    response
        .headers_mut()
        .insert(CONNECTION, "close".parse().unwrap());
    Ok(response)
}
