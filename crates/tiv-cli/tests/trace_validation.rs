use tiv_cli::{TraceSummary, validate_trace_json};
use tiv_core::trace::{
    ActionId, ActionKind, CapturedValue, CompiledTrace, InputSlot, OutputRef, OutputSlot,
};

#[test]
fn a_valid_compiled_trace_returns_a_machine_readable_summary() {
    let trace = r#"{
        "schema_version": 1,
        "seed": 7,
        "actions": [],
        "captured": []
    }"#;

    let summary = validate_trace_json(trace).expect("the compiled trace is valid");

    assert_eq!(
        summary,
        TraceSummary {
            schema_version: 1,
            action_count: 0,
        }
    );
}

#[test]
fn the_committed_spike_trace_replays_the_commit_then_close_checkout_path() {
    let trace = include_str!("../../../spike/compiled-trace-v1.json");
    let summary = validate_trace_json(trace).expect("the committed spike trace is valid");

    assert_eq!(
        summary,
        TraceSummary {
            schema_version: 1,
            action_count: 2,
        }
    );

    let compiled: CompiledTrace =
        serde_json::from_str(trace).expect("the committed spike trace deserializes");
    let payment_intent_output = OutputRef::new(ActionId::new(1), OutputSlot::PaymentIntentId);
    let captured_payment_intent =
        CapturedValue::payment_intent_id("pi_tiv_7dc6fb6eb37270c34d739b91")
            .expect("the fixture ID is valid");
    let confirm = compiled
        .replay_action(ActionId::new(2))
        .expect("the confirm step is present");

    assert_eq!(
        compiled.resolve(payment_intent_output),
        Some(&captured_payment_intent)
    );
    assert_eq!(confirm.kind(), ActionKind::ConfirmPaymentIntent);
    assert_eq!(
        confirm.input(InputSlot::PaymentIntentId),
        Some(&captured_payment_intent)
    );
}

#[test]
fn an_unsupported_or_semantically_invalid_trace_is_rejected() {
    let unsupported = r#"{
        "schema_version": 2,
        "seed": 7,
        "actions": [],
        "captured": []
    }"#;
    let unresolved = r#"{
        "schema_version": 1,
        "seed": 7,
        "actions": [{
            "id": 1,
            "kind": "DriveCheckout",
            "dependencies": [],
            "inputs": [],
            "declared_outputs": ["PaymentIntentId"]
        }],
        "captured": []
    }"#;

    assert!(validate_trace_json(unsupported).is_err());
    assert!(validate_trace_json(unresolved).is_err());
}
