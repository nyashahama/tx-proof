use tiv_cli::{TraceSummary, validate_trace_json};

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
