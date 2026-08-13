use tiv_core::trace::CompiledTrace;
use tiv_runtime::replay::{
    ReferenceReplayScript, ReferenceReplayScriptError, ReplayOperation, ReplayPlan,
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

fn committed_replay_plan() -> ReplayPlan {
    replay_plan_from_json(include_str!("../../../spike/compiled-trace-v1.json"))
}

fn replay_plan_from_json(document: &str) -> ReplayPlan {
    let trace: CompiledTrace = serde_json::from_str(document).expect("the trace is valid");
    ReplayPlan::from_trace(&trace).expect("the trace compiles into runtime replay")
}
