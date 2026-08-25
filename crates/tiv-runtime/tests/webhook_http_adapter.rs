use std::{
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{
    Request, Response, StatusCode, body::Incoming, server::conn::http1, service::service_fn,
};
use hyper_util::rt::TokioIo;
use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec, ProcessFaultSpec,
        ProviderOutcome, WebhookFaultSpec,
    },
    trace::{CaseCapturedValue, CaseOutputSlot},
};
use tiv_runtime::{
    campaign::{
        CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest, CaseExecutionCause,
        CaseExecutionError, execute_planned_case,
    },
    webhook_http::{
        WebhookHttpAdapter, WebhookHttpConfig, WebhookHttpConfigError, WebhookHttpError,
    },
};
use tiv_stripe_pi::{
    CreatePaymentIntent, FaultOutcome, IdempotencyKey, ManagedFixture, OperationId,
    control::{ControlToken, WebhookSigningSecret, WebhookTarget, serve_http1_connection},
};
use tokio::{net::TcpListener, sync::Mutex, sync::mpsc, task::JoinHandle, time::timeout};
use uuid::Uuid;

struct CompletingAdapter {
    webhook_http: WebhookHttpAdapter,
    payment_intent_ids: Vec<String>,
}

impl CaseEffectAdapter for CompletingAdapter {
    type Error = WebhookHttpError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        if matches!(
            request.action().kind(),
            PlanActionKind::GenerateProviderEvent
                | PlanActionKind::DeliverWebhook
                | PlanActionKind::DuplicateWebhook
                | PlanActionKind::DelayWebhook { .. }
                | PlanActionKind::ReorderWebhooks
                | PlanActionKind::DropWebhook
        ) {
            return self.webhook_http.execute(request);
        }

        Box::pin(async move {
            request
                .expected_outputs()
                .iter()
                .copied()
                .map(|output| {
                    let value = match output.slot() {
                        CaseOutputSlot::PaymentIntentId => {
                            let id = self
                                .payment_intent_ids
                                .get(usize::from(output.occurrence()))
                                .ok_or(WebhookHttpError::UnexpectedOutputContract)?;
                            CaseCapturedValue::payment_intent_id(id.clone())
                                .map_err(|_| WebhookHttpError::UnexpectedOutputContract)?
                        }
                        CaseOutputSlot::ProviderGateId => CaseCapturedValue::provider_gate_id(1)
                            .map_err(|_| WebhookHttpError::UnexpectedOutputContract)?,
                        CaseOutputSlot::EventId => {
                            return Err(WebhookHttpError::UnexpectedOutputContract);
                        }
                    };
                    Ok((output, value))
                })
                .collect()
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_planned_webhook_queue_reorders_drops_and_duplicates_exact_events() {
    let (target_address, mut deliveries, target_server) = start_webhook_target().await;
    let (fixture, payment_intent_ids) = prepared_fixture(42, 2).await;
    let (control_address, control_server) =
        start_control_server(Arc::clone(&fixture), target_address).await;
    let timestamp = current_timestamp();
    let config = WebhookHttpConfig::new(
        format!("http://{control_address}"),
        "case-control-token",
        1,
        timestamp,
        Duration::from_secs(2),
    )
    .unwrap();
    let mut adapter = CompletingAdapter {
        webhook_http: WebhookHttpAdapter::new(config).unwrap(),
        payment_intent_ids: payment_intent_ids.clone(),
    };
    let plan = seeded_plan(42);
    let event_actions = plan
        .actions()
        .iter()
        .filter(|action| matches!(action.kind(), PlanActionKind::GenerateProviderEvent))
        .map(tiv_core::plan::PlannedAction::id)
        .collect::<Vec<_>>();
    assert_eq!(event_actions.len(), 2);
    let journal_path = journal_path();

    let executed = execute_planned_case("run_42", "case_42", &plan, &journal_path, &mut adapter)
        .await
        .expect("the full webhook schedule reaches its final checkpoint");

    let first_event = executed
        .trace()
        .resolve(tiv_core::trace::CaseOutputRef::new(
            event_actions[0],
            CaseOutputSlot::EventId,
        ))
        .unwrap();
    let second_event = executed
        .trace()
        .resolve(tiv_core::trace::CaseOutputRef::new(
            event_actions[1],
            CaseOutputSlot::EventId,
        ))
        .unwrap();
    let CaseCapturedValue::EventId(first_event) = first_event else {
        panic!("the first generated output is an event ID");
    };
    let CaseCapturedValue::EventId(second_event) = second_event else {
        panic!("the second generated output is an event ID");
    };
    assert_ne!(first_event, second_event);

    let mut observed = Vec::new();
    for _ in 0..3 {
        observed.push(
            timeout(Duration::from_secs(2), deliveries.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    assert!(
        timeout(Duration::from_millis(50), deliveries.recv())
            .await
            .is_err(),
        "the dropped first event is never delivered"
    );
    assert!(observed.iter().all(|delivery| {
        delivery.event_id() == second_event.as_str()
            && delivery.payment_intent_id() == payment_intent_ids[1]
    }));
    assert_eq!(
        observed
            .iter()
            .map(ObservedDelivery::timestamp)
            .collect::<Vec<_>>(),
        vec![timestamp, timestamp + 1, timestamp + 2]
    );
    assert!(observed.windows(2).all(|pair| {
        pair[0].raw_body == pair[1].raw_body && pair[0].signature != pair[1].signature
    }));

    drop(adapter);
    control_server.abort();
    let _ = control_server.await;
    target_server.abort();
    let _ = target_server.await;
    tokio::fs::remove_file(journal_path).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aliased_payment_intent_attempts_reuse_one_immutable_provider_event() {
    let (target_address, _deliveries, target_server) = start_webhook_target().await;
    let (fixture, payment_intent_ids) = prepared_fixture(42, 1).await;
    let payment_intent_id = payment_intent_ids[0].clone();
    let (control_address, control_server) =
        start_control_server(Arc::clone(&fixture), target_address).await;
    let config = WebhookHttpConfig::new(
        format!("http://{control_address}"),
        "case-control-token",
        1,
        current_timestamp(),
        Duration::from_secs(2),
    )
    .unwrap();
    let mut adapter = CompletingAdapter {
        webhook_http: WebhookHttpAdapter::new(config).unwrap(),
        payment_intent_ids: vec![payment_intent_id.clone(), payment_intent_id],
    };
    let plan = seeded_plan(42);
    let event_actions = plan
        .actions()
        .iter()
        .filter(|action| matches!(action.kind(), PlanActionKind::GenerateProviderEvent))
        .map(tiv_core::plan::PlannedAction::id)
        .collect::<Vec<_>>();
    assert_eq!(event_actions.len(), 2);
    let journal_path = journal_path();

    let executed = execute_planned_case("run_42", "case_42", &plan, &journal_path, &mut adapter)
        .await
        .expect("aliased provider attempts reuse the fixture's immutable event");
    let first_event = executed
        .trace()
        .resolve(tiv_core::trace::CaseOutputRef::new(
            event_actions[0],
            CaseOutputSlot::EventId,
        ))
        .expect("the first event attempt is captured");
    let retry_event = executed
        .trace()
        .resolve(tiv_core::trace::CaseOutputRef::new(
            event_actions[1],
            CaseOutputSlot::EventId,
        ))
        .expect("the aliased event attempt is captured");

    assert_eq!(first_event, retry_event);
    assert_eq!(fixture.lock().await.snapshot().payment_intents().len(), 1);

    drop(adapter);
    control_server.abort();
    let _ = control_server.await;
    target_server.abort();
    let _ = target_server.await;
    tokio::fs::remove_file(journal_path).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_history_reorders_success_before_the_older_snapshot() {
    let (target_address, mut deliveries, target_server) = start_webhook_target().await;
    let (fixture, payment_intent_ids) = prepared_fixture(2, 1).await;
    let (control_address, control_server) =
        start_control_server(Arc::clone(&fixture), target_address).await;
    let config = WebhookHttpConfig::new(
        format!("http://{control_address}"),
        "case-control-token",
        1,
        current_timestamp(),
        Duration::from_secs(2),
    )
    .unwrap()
    .with_stale_event_history(true);
    let mut adapter = CompletingAdapter {
        webhook_http: WebhookHttpAdapter::new(config).unwrap(),
        payment_intent_ids,
    };
    let plan = stale_webhook_plan();
    let journal_path = journal_path();

    execute_planned_case("run_2", "case_2", &plan, &journal_path, &mut adapter)
        .await
        .expect("the reversed stale-event schedule completes");
    let first = timeout(Duration::from_secs(2), deliveries.recv())
        .await
        .unwrap()
        .unwrap()
        .json();
    let second = timeout(Duration::from_secs(2), deliveries.recv())
        .await
        .unwrap()
        .unwrap()
        .json();

    assert_eq!(first["type"], "payment_intent.succeeded");
    assert_eq!(first["data"]["object"]["status"], "succeeded");
    assert_eq!(second["type"], "payment_intent.requires_confirmation");
    assert_eq!(second["data"]["object"]["status"], "requires_confirmation");
    assert!(second["created"].as_i64() < first["created"].as_i64());

    drop(adapter);
    control_server.abort();
    let _ = control_server.await;
    target_server.abort();
    let _ = target_server.await;
    tokio::fs::remove_file(journal_path).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nonzero_planned_delay_defers_the_next_real_delivery() {
    let (target_address, mut deliveries, target_server) = start_webhook_target().await;
    let (fixture, payment_intent_ids) = prepared_fixture(7, 1).await;
    let (control_address, control_server) =
        start_control_server(Arc::clone(&fixture), target_address).await;
    let timestamp = current_timestamp();
    let config = WebhookHttpConfig::new(
        format!("http://{control_address}"),
        "case-control-token",
        1,
        timestamp,
        Duration::from_secs(2),
    )
    .unwrap();
    let mut adapter = CompletingAdapter {
        webhook_http: WebhookHttpAdapter::new(config).unwrap(),
        payment_intent_ids,
    };
    let plan = delayed_webhook_plan();
    let delay_index = plan
        .actions()
        .iter()
        .position(|action| {
            matches!(
                action.kind(),
                PlanActionKind::DelayWebhook { milliseconds: 100 }
            )
        })
        .unwrap();
    assert!(matches!(
        plan.actions()[delay_index + 1].kind(),
        PlanActionKind::DeliverWebhook
    ));
    let journal_path = journal_path();
    execute_planned_case("run_7", "case_7", &plan, &journal_path, &mut adapter)
        .await
        .expect("the delayed webhook schedule completes");
    timeout(Duration::from_secs(2), deliveries.recv())
        .await
        .unwrap()
        .unwrap();
    let delay_outcome =
        action_outcome_micros(&journal_path, plan.actions()[delay_index].id()).await;
    let deliver_outcome =
        action_outcome_micros(&journal_path, plan.actions()[delay_index + 1].id()).await;
    assert!(
        deliver_outcome.saturating_sub(delay_outcome) >= 90_000,
        "the planned 100 ms delay must remain observable between action outcomes"
    );

    drop(adapter);
    control_server.abort();
    let _ = control_server.await;
    target_server.abort();
    let _ = target_server.await;
    tokio::fs::remove_file(journal_path).await.unwrap();
}

#[test]
fn webhook_control_configuration_fails_closed() {
    assert!(matches!(
        WebhookHttpConfig::new(
            "http://fixture:18081",
            "token",
            1,
            1,
            Duration::from_secs(1)
        ),
        Err(WebhookHttpConfigError::NonLoopbackUrl)
    ));
    assert!(matches!(
        WebhookHttpConfig::new(
            "http://127.0.0.1:18081",
            "line\nbreak",
            1,
            1,
            Duration::from_secs(1)
        ),
        Err(WebhookHttpConfigError::InvalidControlToken)
    ));
    assert!(matches!(
        WebhookHttpConfig::new(
            "http://127.0.0.1:18081",
            "token",
            0,
            1,
            Duration::from_secs(1)
        ),
        Err(WebhookHttpConfigError::InvalidBounds)
    ));
    assert!(matches!(
        WebhookHttpConfig::new(
            "http://127.0.0.1:18081",
            "token",
            1,
            1,
            Duration::from_secs(31)
        ),
        Err(WebhookHttpConfigError::InvalidBounds)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_application_rejection_fails_the_delivery_action() {
    let (target_address, mut deliveries, target_server) =
        start_webhook_target_with_status(StatusCode::INTERNAL_SERVER_ERROR).await;
    let (fixture, payment_intent_ids) = prepared_fixture(7, 1).await;
    let (control_address, control_server) =
        start_control_server(Arc::clone(&fixture), target_address).await;
    let config = WebhookHttpConfig::new(
        format!("http://{control_address}"),
        "case-control-token",
        1,
        current_timestamp(),
        Duration::from_secs(2),
    )
    .unwrap();
    let mut adapter = CompletingAdapter {
        webhook_http: WebhookHttpAdapter::new(config).unwrap(),
        payment_intent_ids,
    };
    let plan = delayed_webhook_plan();
    let journal_path = journal_path();

    let error = execute_planned_case("run_7", "case_7", &plan, &journal_path, &mut adapter)
        .await
        .expect_err("the application rejection must fail the delivery action");
    assert!(matches!(
        error,
        CaseExecutionError::Failed(ref failure)
            if matches!(
                failure.cause(),
                CaseExecutionCause::Effect(WebhookHttpError::UnexpectedDeliveryStatus(500))
            )
    ));
    timeout(Duration::from_secs(2), deliveries.recv())
        .await
        .unwrap()
        .unwrap();

    drop(adapter);
    control_server.abort();
    let _ = control_server.await;
    target_server.abort();
    let _ = target_server.await;
    tokio::fs::remove_file(journal_path).await.unwrap();
}

async fn prepared_fixture(
    seed: u64,
    provider_objects: usize,
) -> (Arc<Mutex<ManagedFixture>>, Vec<String>) {
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(seed))));
    fixture
        .lock()
        .await
        .reset(
            1,
            Seed::new(seed),
            vec![FaultOutcome::Normal; provider_objects],
        )
        .unwrap();
    for index in 0..provider_objects {
        fixture
            .lock()
            .await
            .create_data_plane(
                IdempotencyKey::new(format!("op-{seed}-attempt-{index}")).unwrap(),
                CreatePaymentIntent::new(2_500, "usd")
                    .unwrap()
                    .with_operation_id(OperationId::new(format!("op_{seed}")).unwrap()),
            )
            .unwrap();
    }
    let ids = fixture
        .lock()
        .await
        .snapshot()
        .payment_intents()
        .iter()
        .map(|payment_intent| payment_intent.id().to_owned())
        .collect();
    (fixture, ids)
}

fn seeded_plan(seed: u64) -> tiv_core::plan::PlannedCase {
    CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
        Seed::new(seed),
        ActionBudget::new(40).unwrap(),
    ))
    .unwrap()
}

fn delayed_webhook_plan() -> tiv_core::plan::PlannedCase {
    let spec = PlanSpec::new_payment_intent_v1(
        Seed::new(0),
        ActionBudget::new(40).unwrap(),
        [ProviderOutcome::Normal],
        WebhookFaultSpec::new(0, [100], false, false).unwrap(),
        ProcessFaultSpec::new([], 0).unwrap(),
    )
    .unwrap();
    CasePlanCompiler::compile(&spec).unwrap()
}

fn stale_webhook_plan() -> tiv_core::plan::PlannedCase {
    let spec = PlanSpec::new_payment_intent_v1(
        Seed::new(2),
        ActionBudget::new(40).unwrap(),
        [ProviderOutcome::Normal],
        WebhookFaultSpec::new(0, [], true, false)
            .unwrap()
            .with_stale_event(true),
        ProcessFaultSpec::new([], 0).unwrap(),
    )
    .unwrap();
    CasePlanCompiler::compile(&spec).unwrap()
}

struct ObservedDelivery {
    raw_body: Vec<u8>,
    signature: String,
}

impl ObservedDelivery {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.raw_body).unwrap()
    }

    fn event_id(&self) -> String {
        self.json()["id"].as_str().unwrap().to_owned()
    }

    fn payment_intent_id(&self) -> String {
        self.json()["data"]["object"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn timestamp(&self) -> i64 {
        self.signature
            .strip_prefix("t=")
            .and_then(|value| value.split_once(",v1="))
            .and_then(|(timestamp, _)| timestamp.parse().ok())
            .unwrap()
    }
}

async fn start_webhook_target() -> (SocketAddr, mpsc::Receiver<ObservedDelivery>, JoinHandle<()>) {
    start_webhook_target_with_status(StatusCode::OK).await
}

async fn start_webhook_target_with_status(
    status: StatusCode,
) -> (SocketAddr, mpsc::Receiver<ObservedDelivery>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel(4);
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let sender = sender.clone();
            tokio::spawn(async move {
                let _result = http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        service_fn(move |request| record_delivery(request, sender.clone(), status)),
                    )
                    .await;
            });
        }
    });
    (address, receiver, server)
}

async fn record_delivery(
    request: Request<Incoming>,
    sender: mpsc::Sender<ObservedDelivery>,
    status: StatusCode,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let signature = request
        .headers()
        .get("stripe-signature")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let raw_body = request
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    sender
        .send(ObservedDelivery {
            raw_body,
            signature,
        })
        .await
        .unwrap();
    let mut response = Response::new(Full::new(Bytes::from_static(b"accepted")));
    *response.status_mut() = status;
    Ok(response)
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
                let _result = serve_http1_connection(
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

fn current_timestamp() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn journal_path() -> PathBuf {
    std::env::temp_dir().join(format!("tiv-webhook-http-{}.ndjson", Uuid::new_v4()))
}

async fn action_outcome_micros(path: &PathBuf, action_id: tiv_core::trace::ActionId) -> u64 {
    let journal = tokio::fs::read_to_string(path).await.unwrap();
    let serialized_action_id = serde_json::to_value(action_id).unwrap();
    journal
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|record| {
            record["action_id"] == serialized_action_id
                && record["observation_kind"] == "action_outcome"
        })
        .and_then(|record| record["monotonic_elapsed_micros"].as_u64())
        .expect("the action outcome is durably journaled")
}
