use tiv_core::trace::CompiledTrace;
use tiv_runtime::{
    postgres::safety::DatabaseName,
    replay::{
        ReferenceAppReplayConfig, ReferenceAppReplayConfigError, ReferenceReplayScript,
        ReferenceReplayScriptError, ReplayOperation, ReplayPlan,
    },
};

#[test]
fn runtime_replay_plan_consumes_the_committed_compiled_trace() {
    let plan = committed_replay_plan();

    assert_eq!(plan.schema_version(), 1);
    assert_eq!(plan.seed().value(), 7);
    assert_eq!(plan.action_count(), 2);
    assert_eq!(plan.steps().len(), 2);
    assert_eq!(
        plan.steps()[0].operation(),
        &ReplayOperation::DriveCheckout {
            captured_payment_intent_id: "pi_tiv_7dc6fb6eb37270c34d739b91".to_owned(),
        }
    );
    assert_eq!(
        plan.steps()[1].operation(),
        &ReplayOperation::ConfirmPaymentIntent {
            payment_intent_id: "pi_tiv_7dc6fb6eb37270c34d739b91".to_owned(),
        }
    );
}

#[test]
fn reference_replay_script_is_derived_from_the_runtime_plan() {
    let plan = committed_replay_plan();
    let script =
        ReferenceReplayScript::from_plan(&plan).expect("the committed trace is executable");

    assert_eq!(script.fixture_seed().value(), 7);
    assert_eq!(
        script.expected_payment_intent_id(),
        "pi_tiv_7dc6fb6eb37270c34d739b91"
    );

    let incomplete_plan = replay_plan_from_json(
        r#"{
            "schema_version": 1,
            "seed": 7,
            "actions": [{
                "id": 1,
                "kind": "DriveCheckout",
                "dependencies": [],
                "inputs": [],
                "declared_outputs": ["PaymentIntentId"]
            }],
            "captured": [{
                "output_ref": {"action_id": 1, "slot": "PaymentIntentId"},
                "value": {"PaymentIntentId": "pi_tiv_7dc6fb6eb37270c34d739b91"}
            }]
        }"#,
    );

    assert_eq!(
        ReferenceReplayScript::from_plan(&incomplete_plan),
        Err(ReferenceReplayScriptError::UnexpectedStepCount { actual: 1 })
    );
}

#[test]
fn reference_app_replay_config_accepts_only_loopback_prepared_case_execution() {
    let case_database =
        DatabaseName::parse("tiv_case_7dc6fb6e").expect("the generated case name is valid");
    let config = ReferenceAppReplayConfig::new(
        case_database.clone(),
        "http://127.0.0.1:18080",
        "http://127.0.0.1:12112",
        "run-scoped-control-token",
        1,
        2,
        1_700_000_000,
    )
    .expect("the bounded loopback reference replay config is valid");

    assert_eq!(config.case_database(), &case_database);
    assert_eq!(config.operation_id(), "op_7dc6fb6e");
    assert_eq!(config.reference_app_url(), "http://127.0.0.1:18080");
    assert_eq!(config.fixture_control_url(), "http://127.0.0.1:12112");
    assert_eq!(config.fixture_control_token(), "run-scoped-control-token");
    assert_eq!(config.reset_sequence(), 1);
    assert_eq!(config.confirm_sequence(), 2);
    assert_eq!(config.webhook_timestamp(), 1_700_000_000);

    assert_eq!(
        ReferenceAppReplayConfig::new(
            case_database.clone(),
            "https://example.com",
            "http://127.0.0.1:12112",
            "run-scoped-control-token",
            1,
            2,
            1_700_000_000,
        ),
        Err(ReferenceAppReplayConfigError::NonLoopbackUrl)
    );
    assert_eq!(
        ReferenceAppReplayConfig::new(
            case_database,
            "http://127.0.0.1:18080",
            "http://127.0.0.1:12112",
            "run-scoped-control-token",
            1,
            1,
            1_700_000_000,
        ),
        Err(ReferenceAppReplayConfigError::InvalidSequence)
    );
}

fn committed_replay_plan() -> ReplayPlan {
    replay_plan_from_json(include_str!("../../../spike/compiled-trace-v1.json"))
}

fn replay_plan_from_json(document: &str) -> ReplayPlan {
    let trace: CompiledTrace = serde_json::from_str(document).expect("the trace is valid");
    ReplayPlan::from_trace(&trace).expect("the trace compiles into runtime replay")
}
