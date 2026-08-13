use clap::Parser;
use serde_json::json;
use tiv_cli::{Cli, execute};

#[test]
fn replay_inspect_uses_the_runtime_replay_plan_without_executing_customer_code() {
    let trace_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spike/compiled-trace-v1.json"
    );
    let cli = Cli::try_parse_from(["tiv", "replay", "inspect", trace_path])
        .expect("the replay inspect command parses");

    let output = execute(cli).expect("the committed trace compiles into a replay plan");
    let value: serde_json::Value = serde_json::from_str(&output).expect("the replay plan is JSON");

    assert_eq!(
        value,
        json!({
            "schema_version": 1,
            "action_count": 2,
            "steps": [
                {
                    "action_id": 1,
                    "operation": "drive_checkout",
                    "captured_payment_intent_id": "pi_tiv_7_1"
                },
                {
                    "action_id": 2,
                    "operation": "confirm_payment_intent",
                    "payment_intent_id": "pi_tiv_7_1"
                }
            ]
        })
    );
}
