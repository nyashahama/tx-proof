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
        ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec, ProviderOutcome,
        ProviderOutcomeScript,
    },
    trace::{CaseCapturedValue, CaseOutputRef, CaseOutputSlot},
};
use tiv_reference_app::{CheckoutOperation, create_with_changed_retry_key};
use tiv_runtime::{
    campaign::{
        CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest, CaseExecutionCause,
        CaseExecutionError, execute_planned_case,
    },
    provider_http::{
        ProviderHttpAdapter, ProviderHttpConfig, ProviderHttpConfigError, ProviderHttpError,
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
    provider_http: ProviderHttpAdapter,
    real_business_actions_remaining: usize,
    real_confirm_actions_remaining: usize,
    real_retrieves_remaining: usize,
    real_gate_pending: bool,
}

struct GateSubstitutionAdapter {
    provider_http: ProviderHttpAdapter,
    substitute_next_gate: bool,
}

struct HttpHarness {
    fixture: Arc<Mutex<ManagedFixture>>,
    driver_address: SocketAddr,
    data_address: SocketAddr,
    control_address: SocketAddr,
    data_server: JoinHandle<()>,
    driver_server: JoinHandle<()>,
    control_server: JoinHandle<()>,
}

impl HttpHarness {
    async fn start(outcomes: Vec<FaultOutcome>, data_connections: usize) -> Self {
        let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(73))));
        fixture
            .lock()
            .await
            .reset(1, Seed::new(73), outcomes)
            .expect("the bounded fault plan is installed");
        let (data_address, data_server) =
            start_data_server(Arc::clone(&fixture), data_connections).await;
        let (control_address, control_server) = start_control_server(Arc::clone(&fixture)).await;
        let (driver_address, driver_server) = start_driver_server(data_address).await;
        Self {
            fixture,
            driver_address,
            data_address,
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
fn provider_http_configuration_rejects_non_loopback_and_ambiguous_inputs() {
    const DRIVER: &str = "http://127.0.0.1:8080/checkout";
    const DATA: &str = "http://127.0.0.1:9091";
    const CONTROL: &str = "http://127.0.0.1:9090";
    let body = serde_json::json!({
        "operation_id": "op_73",
        "amount_minor": 2500,
        "currency": "usd"
    });
    for (driver_url, data_url, control_url) in [
        ("https://127.0.0.1:8080/checkout", DATA, CONTROL),
        ("http://192.0.2.1:8080/checkout", DATA, CONTROL),
        ("http://user@127.0.0.1:8080/checkout", DATA, CONTROL),
        (
            "http://127.0.0.1:8080/checkout?next=http://example.com",
            DATA,
            CONTROL,
        ),
        (DRIVER, "http://192.0.2.1:9091", CONTROL),
        (DRIVER, DATA, "http://127.0.0.1:9090/control"),
    ] {
        assert!(matches!(
            ProviderHttpConfig::new(
                driver_url,
                body.clone(),
                data_url,
                control_url,
                "token",
                1,
                Duration::from_secs(1),
                Duration::from_millis(1),
            ),
            Err(ProviderHttpConfigError::NonLoopbackUrl)
        ));
    }
    assert!(matches!(
        ProviderHttpConfig::new(
            DRIVER,
            serde_json::json!({"amount_minor": 2500}),
            DATA,
            CONTROL,
            "token",
            1,
            Duration::from_secs(1),
            Duration::from_millis(1),
        ),
        Err(ProviderHttpConfigError::InvalidDriverBody)
    ));
    assert!(matches!(
        ProviderHttpConfig::new(
            DRIVER,
            serde_json::json!({"operation_id": "op_73"}),
            DATA,
            CONTROL,
            "token",
            1,
            Duration::from_secs(1),
            Duration::from_millis(1),
        ),
        Err(ProviderHttpConfigError::InvalidDriverBody)
    ));
    assert!(matches!(
        ProviderHttpConfig::new(
            DRIVER,
            body.clone(),
            DATA,
            CONTROL,
            "bad\nheader",
            0,
            Duration::ZERO,
            Duration::ZERO,
        ),
        Err(ProviderHttpConfigError::InvalidControlToken)
    ));
    assert!(matches!(
        ProviderHttpConfig::new(
            DRIVER,
            body.clone(),
            DATA,
            CONTROL,
            "token",
            0,
            Duration::ZERO,
            Duration::ZERO,
        ),
        Err(ProviderHttpConfigError::InvalidBounds)
    ));
    assert!(matches!(
        ProviderHttpConfig::new(
            DRIVER,
            body,
            DATA,
            CONTROL,
            "token",
            u64::MAX,
            Duration::from_secs(1),
            Duration::from_millis(1),
        ),
        Err(ProviderHttpConfigError::InvalidBounds)
    ));
}

impl CaseEffectAdapter for GateSubstitutionAdapter {
    type Error = ProviderHttpError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        let substitute_gate = self.substitute_next_gate
            && matches!(
                request.action().kind(),
                PlanActionKind::DriveCheckout { .. } | PlanActionKind::RetryBusinessRequest { .. }
            );
        if substitute_gate {
            self.substitute_next_gate = false;
        }
        Box::pin(async move {
            let mut captured = self.provider_http.execute(request).await?;
            if substitute_gate {
                for (_, value) in &mut captured {
                    if matches!(value, CaseCapturedValue::ProviderGateId(_)) {
                        *value = CaseCapturedValue::provider_gate_id(99).unwrap();
                    }
                }
            }
            Ok(captured)
        })
    }
}

impl CaseEffectAdapter for CompletingAdapter {
    type Error = ProviderHttpError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        let real_business_action = matches!(
            request.action().kind(),
            PlanActionKind::DriveCheckout { .. } | PlanActionKind::RetryBusinessRequest { .. }
        ) && self.real_business_actions_remaining > 0;
        if real_business_action {
            self.real_business_actions_remaining -= 1;
            self.real_gate_pending = match request.action().kind() {
                PlanActionKind::DriveCheckout { provider_script }
                | PlanActionKind::RetryBusinessRequest { provider_script } => {
                    provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay
                }
                _ => false,
            };
            return self.provider_http.execute(request);
        }
        let real_confirm_action = matches!(
            request.action().kind(),
            PlanActionKind::ConfirmPaymentIntent { .. }
                | PlanActionKind::RetryProviderRequest { .. }
        ) && self.real_confirm_actions_remaining > 0;
        if real_confirm_action {
            self.real_confirm_actions_remaining -= 1;
            self.real_gate_pending = match request.action().kind() {
                PlanActionKind::ConfirmPaymentIntent { provider_script }
                | PlanActionKind::RetryProviderRequest { provider_script } => {
                    provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay
                }
                _ => false,
            };
            return self.provider_http.execute(request);
        }
        if matches!(
            request.action().kind(),
            PlanActionKind::RetrievePaymentIntent
        ) && self.real_retrieves_remaining > 0
        {
            self.real_retrieves_remaining -= 1;
            return self.provider_http.execute(request);
        }
        if self.real_gate_pending
            && matches!(request.action().kind(), PlanActionKind::ReleaseProviderGate)
        {
            self.real_gate_pending = false;
            return self.provider_http.execute(request);
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
    let harness = HttpHarness::start(vec![FaultOutcome::CommitThenDelay], 1).await;
    let plan = provider_http_plan();
    let first_action_id = plan.actions()[0].id();
    let journal_path = journal_path();
    let config = ProviderHttpConfig::new(
        format!("http://{}/checkout", harness.driver_address),
        serde_json::json!({
            "database": "tiv_case_0123456789abcdef",
            "operation_id": "op_73",
            "amount_minor": 2500,
            "currency": "usd"
        }),
        format!("http://{}", harness.data_address),
        format!("http://{}", harness.control_address),
        "case-control-token",
        1,
        Duration::from_secs(2),
        Duration::from_millis(2),
    )
    .expect("all HTTP targets are bounded loopback endpoints");
    let mut adapter = CompletingAdapter {
        provider_http: ProviderHttpAdapter::new(config).unwrap(),
        real_business_actions_remaining: 1,
        real_confirm_actions_remaining: 0,
        real_retrieves_remaining: 0,
        real_gate_pending: false,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_rejects_a_gate_other_than_the_exact_trace_binding() {
    let harness = HttpHarness::start(vec![FaultOutcome::CommitThenDelay], 1).await;
    let plan = provider_http_plan();
    let journal_path = journal_path();
    let config = ProviderHttpConfig::new(
        format!("http://{}/checkout", harness.driver_address),
        serde_json::json!({
            "database": "tiv_case_0123456789abcdef",
            "operation_id": "op_73",
            "amount_minor": 2500,
            "currency": "usd"
        }),
        format!("http://{}", harness.data_address),
        format!("http://{}", harness.control_address),
        "case-control-token",
        1,
        Duration::from_secs(2),
        Duration::from_millis(2),
    )
    .unwrap();
    let mut adapter = GateSubstitutionAdapter {
        provider_http: ProviderHttpAdapter::new(config).unwrap(),
        substitute_next_gate: true,
    };

    let error = execute_planned_case("run_73", "case_73", &plan, &journal_path, &mut adapter)
        .await
        .expect_err("release must use the exact gate captured by its planned dependency");
    let CaseExecutionError::Failed(failure) = error else {
        panic!("the provider input mismatch must be an execution failure");
    };
    assert!(matches!(
        failure.cause(),
        CaseExecutionCause::Effect(ProviderHttpError::UnexpectedInputContract)
    ));
    assert_eq!(
        harness.fixture.lock().await.snapshot().held_gates().len(),
        1
    );

    let release = reqwest::Client::new()
        .post(format!(
            "http://{}/v1/control/release-gate",
            harness.control_address
        ))
        .header("X-Tiv-Control-Token", "case-control-token")
        .json(&serde_json::json!({
            "command_sequence": 2,
            "gate_id": 1,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(release.status(), StatusCode::OK);
    harness.finish().await;
    drop(adapter);
    tokio::fs::remove_file(journal_path).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_checkout_captures_both_commits_before_releasing_its_retry_response() {
    let harness = HttpHarness::start(
        vec![FaultOutcome::CommitThenClose, FaultOutcome::CommitThenDelay],
        2,
    )
    .await;
    let plan = two_call_provider_http_plan();
    let first_action_id = plan.actions()[0].id();
    let journal_path = journal_path();
    let config = ProviderHttpConfig::new(
        format!("http://{}/checkout", harness.driver_address),
        serde_json::json!({
            "database": "tiv_case_0123456789abcdef",
            "operation_id": "op_73",
            "amount_minor": 2500,
            "currency": "usd"
        }),
        format!("http://{}", harness.data_address),
        format!("http://{}", harness.control_address),
        "case-control-token",
        1,
        Duration::from_secs(2),
        Duration::from_millis(2),
    )
    .unwrap();
    let mut adapter = CompletingAdapter {
        provider_http: ProviderHttpAdapter::new(config).unwrap(),
        real_business_actions_remaining: 1,
        real_confirm_actions_remaining: 0,
        real_retrieves_remaining: 0,
        real_gate_pending: false,
    };

    let execution = execute_planned_case("run_73", "case_73", &plan, &journal_path, &mut adapter)
        .await
        .expect("one checkout reaches both provider commits and the held retry boundary");
    let first = execution
        .trace()
        .resolve(CaseOutputRef::new(
            first_action_id,
            CaseOutputSlot::PaymentIntentId,
        ))
        .unwrap();
    let second = execution
        .trace()
        .resolve(CaseOutputRef::for_occurrence(
            first_action_id,
            CaseOutputSlot::PaymentIntentId,
            1,
        ))
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(
        execution.trace().resolve(CaseOutputRef::new(
            first_action_id,
            CaseOutputSlot::ProviderGateId,
        )),
        Some(&CaseCapturedValue::provider_gate_id(1).unwrap())
    );

    drop(adapter);
    harness.finish().await;
    tokio::fs::remove_file(journal_path).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn business_outcome_scripts_match_real_http_results_and_fixture_commits() {
    let scripts = [
        ProviderOutcomeScript::single(ProviderOutcome::Normal),
        ProviderOutcomeScript::single(ProviderOutcome::PreExecute429),
        ProviderOutcomeScript::single(ProviderOutcome::PreExecute500),
        ProviderOutcomeScript::single(ProviderOutcome::PostExecute500),
        ProviderOutcomeScript::with_transport_retry(
            ProviderOutcome::CommitThenClose,
            ProviderOutcome::Normal,
        )
        .unwrap(),
        ProviderOutcomeScript::with_transport_retry(
            ProviderOutcome::CommitThenClose,
            ProviderOutcome::PreExecute500,
        )
        .unwrap(),
        ProviderOutcomeScript::with_transport_retry(
            ProviderOutcome::CommitThenClose,
            ProviderOutcome::CommitThenClose,
        )
        .unwrap(),
    ];

    for script in scripts {
        let harness = HttpHarness::start(
            script.outcomes().map(fixture_outcome).collect(),
            script.outcomes().count(),
        )
        .await;
        let plan = plan_starting_with(script);
        let first_action_id = plan.actions()[0].id();
        let journal_path = journal_path();
        let config = ProviderHttpConfig::new(
            format!("http://{}/checkout", harness.driver_address),
            serde_json::json!({
                "database": "tiv_case_0123456789abcdef",
                "operation_id": "op_73",
                "amount_minor": 2500,
                "currency": "usd"
            }),
            format!("http://{}", harness.data_address),
            format!("http://{}", harness.control_address),
            "case-control-token",
            1,
            Duration::from_secs(2),
            Duration::from_millis(2),
        )
        .unwrap();
        let mut adapter = CompletingAdapter {
            provider_http: ProviderHttpAdapter::new(config).unwrap(),
            real_business_actions_remaining: 1,
            real_confirm_actions_remaining: 0,
            real_retrieves_remaining: 0,
            real_gate_pending: false,
        };

        let execution =
            execute_planned_case("run_73", "case_73", &plan, &journal_path, &mut adapter)
                .await
                .unwrap_or_else(|error| panic!("script {script:?} must execute: {error:?}"));
        let captured = (0..script.committed_count())
            .map(|occurrence| {
                execution
                    .trace()
                    .resolve(CaseOutputRef::for_occurrence(
                        first_action_id,
                        CaseOutputSlot::PaymentIntentId,
                        occurrence,
                    ))
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(captured.len(), usize::from(script.committed_count()));
        for (index, value) in captured.iter().enumerate() {
            assert!(!captured[..index].contains(value));
        }

        drop(adapter);
        harness.finish().await;
        tokio::fs::remove_file(journal_path).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn confirmation_outcomes_match_real_http_status_transport_and_state() {
    for outcome in [
        ProviderOutcome::Normal,
        ProviderOutcome::PreExecute429,
        ProviderOutcome::PreExecute500,
        ProviderOutcome::PostExecute500,
        ProviderOutcome::CommitThenClose,
        ProviderOutcome::CommitThenDelay,
    ] {
        let script = ProviderOutcomeScript::single(outcome);
        let harness =
            HttpHarness::start(vec![FaultOutcome::Normal, fixture_outcome(outcome)], 2).await;
        let plan = plan_starting_normal_with_confirm(script);
        let journal_path = journal_path();
        let config = ProviderHttpConfig::new(
            format!("http://{}/checkout", harness.driver_address),
            serde_json::json!({
                "database": "tiv_case_0123456789abcdef",
                "operation_id": "op_73",
                "amount_minor": 2500,
                "currency": "usd"
            }),
            format!("http://{}", harness.data_address),
            format!("http://{}", harness.control_address),
            "case-control-token",
            1,
            Duration::from_secs(2),
            Duration::from_millis(2),
        )
        .unwrap();
        let mut adapter = CompletingAdapter {
            provider_http: ProviderHttpAdapter::new(config).unwrap(),
            real_business_actions_remaining: 1,
            real_confirm_actions_remaining: 1,
            real_retrieves_remaining: 0,
            real_gate_pending: false,
        };

        execute_planned_case("run_73", "case_73", &plan, &journal_path, &mut adapter)
            .await
            .unwrap_or_else(|error| panic!("confirm {outcome:?} must execute: {error:?}"));
        let snapshot = harness.fixture.lock().await.snapshot();
        assert_eq!(snapshot.remaining_outcomes(), 0);
        assert_eq!(snapshot.payment_intents().len(), 1);
        let expected_status = if matches!(
            outcome,
            ProviderOutcome::PreExecute429 | ProviderOutcome::PreExecute500
        ) {
            "requires_confirmation"
        } else {
            "succeeded"
        };
        assert_eq!(snapshot.payment_intents()[0].status(), expected_status);

        drop(adapter);
        harness.finish().await;
        tokio::fs::remove_file(journal_path).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ambiguous_create_retrieves_the_exact_captured_payment_intent_without_consuming_a_fault() {
    let harness = HttpHarness::start(vec![FaultOutcome::PostExecute500], 2).await;
    let plan = plan_retrieving_after_ambiguous_create();
    let journal_path = journal_path();
    let config = ProviderHttpConfig::new(
        format!("http://{}/checkout", harness.driver_address),
        serde_json::json!({
            "database": "tiv_case_0123456789abcdef",
            "operation_id": "op_73",
            "amount_minor": 2500,
            "currency": "usd"
        }),
        format!("http://{}", harness.data_address),
        format!("http://{}", harness.control_address),
        "case-control-token",
        1,
        Duration::from_secs(2),
        Duration::from_millis(2),
    )
    .unwrap();
    let mut adapter = CompletingAdapter {
        provider_http: ProviderHttpAdapter::new(config).unwrap(),
        real_business_actions_remaining: 1,
        real_confirm_actions_remaining: 0,
        real_retrieves_remaining: 1,
        real_gate_pending: false,
    };

    execute_planned_case("run_73", "case_73", &plan, &journal_path, &mut adapter)
        .await
        .expect(
            "retrieve observes the concrete provider object captured from the ambiguous create",
        );
    let snapshot = harness.fixture.lock().await.snapshot();
    assert_eq!(snapshot.remaining_outcomes(), 0);
    assert_eq!(snapshot.payment_intents().len(), 1);
    assert_eq!(
        snapshot.payment_intents()[0].status(),
        "requires_confirmation"
    );

    drop(adapter);
    harness.finish().await;
    tokio::fs::remove_file(journal_path).await.unwrap();
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

fn provider_http_plan() -> tiv_core::plan::PlannedCase {
    let plan = compile_plan(34);
    assert!(matches!(
        plan.actions().first().map(tiv_core::plan::PlannedAction::kind),
        Some(PlanActionKind::DriveCheckout { provider_script })
            if *provider_script
                == ProviderOutcomeScript::single(ProviderOutcome::CommitThenDelay)
    ));
    assert!(matches!(
        plan.actions()
            .get(1)
            .map(tiv_core::plan::PlannedAction::kind),
        Some(PlanActionKind::ReleaseProviderGate)
    ));
    plan
}

fn two_call_provider_http_plan() -> tiv_core::plan::PlannedCase {
    let script = ProviderOutcomeScript::with_transport_retry(
        ProviderOutcome::CommitThenClose,
        ProviderOutcome::CommitThenDelay,
    )
    .unwrap();
    let plan = compile_plan(42);
    assert!(matches!(
        plan.actions().first().map(tiv_core::plan::PlannedAction::kind),
        Some(PlanActionKind::DriveCheckout { provider_script }) if *provider_script == script
    ));
    plan
}

fn plan_starting_with(provider_script: ProviderOutcomeScript) -> tiv_core::plan::PlannedCase {
    let outcomes = provider_script.outcomes().collect::<Vec<_>>();
    let seed = match outcomes.as_slice() {
        [ProviderOutcome::Normal] => 6,
        [ProviderOutcome::PreExecute429] => 7,
        [ProviderOutcome::PreExecute500] => 12,
        [ProviderOutcome::PostExecute500] => 1,
        [ProviderOutcome::CommitThenClose, ProviderOutcome::Normal] => 9,
        [
            ProviderOutcome::CommitThenClose,
            ProviderOutcome::PreExecute500,
        ] => 3,
        [
            ProviderOutcome::CommitThenClose,
            ProviderOutcome::CommitThenClose,
        ] => 15,
        _ => panic!("no pinned start seed for {provider_script:?}"),
    };
    let plan = compile_plan(seed);
    assert!(matches!(
        plan.actions().first().map(tiv_core::plan::PlannedAction::kind),
        Some(PlanActionKind::DriveCheckout { provider_script: candidate })
            if *candidate == provider_script
    ));
    plan
}

fn plan_starting_normal_with_confirm(
    confirm_script: ProviderOutcomeScript,
) -> tiv_core::plan::PlannedCase {
    let outcome = confirm_script.outcomes().next().unwrap();
    let seed = match outcome {
        ProviderOutcome::Normal => 101,
        ProviderOutcome::PreExecute429 => 18,
        ProviderOutcome::PreExecute500 => 10,
        ProviderOutcome::PostExecute500 => 20,
        ProviderOutcome::CommitThenClose => 6,
        ProviderOutcome::CommitThenDelay => 138,
    };
    let plan = compile_plan(seed);
    assert!(matches!(
        plan.actions().first().map(tiv_core::plan::PlannedAction::kind),
        Some(PlanActionKind::DriveCheckout { provider_script })
            if *provider_script == ProviderOutcomeScript::single(ProviderOutcome::Normal)
    ));
    assert!(plan.actions().iter().any(|action| {
        matches!(
            action.kind(),
            PlanActionKind::ConfirmPaymentIntent { provider_script }
                if *provider_script == confirm_script
        )
    }));
    plan
}

fn plan_retrieving_after_ambiguous_create() -> tiv_core::plan::PlannedCase {
    let post_execute_500 = ProviderOutcomeScript::single(ProviderOutcome::PostExecute500);
    let plan = compile_plan(25);
    assert!(matches!(
        plan.actions().first().map(tiv_core::plan::PlannedAction::kind),
        Some(PlanActionKind::DriveCheckout { provider_script })
            if *provider_script == post_execute_500
    ));
    let first_resolution = plan.actions().iter().find(|action| {
        matches!(
            action.kind(),
            PlanActionKind::RetrievePaymentIntent | PlanActionKind::RetryBusinessRequest { .. }
        )
    });
    assert!(
        first_resolution
            .is_some_and(|action| matches!(action.kind(), PlanActionKind::RetrievePaymentIntent))
    );
    plan
}

fn compile_plan(seed: u64) -> tiv_core::plan::PlannedCase {
    CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
        Seed::new(seed),
        ActionBudget::new(40).unwrap(),
    ))
    .unwrap()
}

const fn fixture_outcome(outcome: ProviderOutcome) -> FaultOutcome {
    match outcome {
        ProviderOutcome::Normal => FaultOutcome::Normal,
        ProviderOutcome::PreExecute429 => FaultOutcome::PreExecute429,
        ProviderOutcome::PreExecute500 => FaultOutcome::PreExecute500,
        ProviderOutcome::PostExecute500 => FaultOutcome::PostExecute500,
        ProviderOutcome::CommitThenClose => FaultOutcome::CommitThenClose,
        ProviderOutcome::CommitThenDelay => FaultOutcome::CommitThenDelay,
    }
}

fn journal_path() -> PathBuf {
    std::env::temp_dir().join(format!("tiv-provider-http-{}.ndjson", Uuid::new_v4()))
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
    let operation = CheckoutOperation::new("op_73", 2_500, "usd").unwrap();
    let result =
        create_with_changed_retry_key(&reqwest::Client::new(), &fixture_url, &operation).await;
    let mut response = if let Ok(payment_intent) = result {
        Response::new(Full::new(Bytes::from(
            serde_json::to_vec(&serde_json::json!({
                "payment_intent_id": payment_intent.id(),
                "operation_id": "op_73"
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
