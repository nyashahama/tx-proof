use tiv_core::trace::CompiledTrace;
use tiv_runtime::replay::{ReplayOperation, ReplayPlan};

#[test]
fn runtime_replay_plan_consumes_the_committed_compiled_trace() {
    let trace: CompiledTrace =
        serde_json::from_str(include_str!("../../../spike/compiled-trace-v1.json"))
            .expect("the committed trace is valid");

    let plan = ReplayPlan::from_trace(&trace).expect("the trace compiles into runtime replay");

    assert_eq!(plan.schema_version(), 1);
    assert_eq!(plan.action_count(), 2);
    assert_eq!(plan.steps().len(), 2);
    assert_eq!(
        plan.steps()[0].operation(),
        &ReplayOperation::DriveCheckout {
            captured_payment_intent_id: "pi_tiv_7_1".to_owned(),
        }
    );
    assert_eq!(
        plan.steps()[1].operation(),
        &ReplayOperation::ConfirmPaymentIntent {
            payment_intent_id: "pi_tiv_7_1".to_owned(),
        }
    );
}
