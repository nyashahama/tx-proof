use clap::Parser;
use tiv_cli::{Cli, execute_async};

#[tokio::test]
async fn replay_reference_app_rejects_non_loopback_targets_before_execution() {
    let trace_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spike/compiled-trace-v1.json"
    );
    let cli = Cli::try_parse_from([
        "tiv",
        "replay",
        "reference-app",
        "--trace",
        trace_path,
        "--case-database",
        "tiv_case_7dc6fb6e",
        "--reference-app-url",
        "https://example.com",
        "--fixture-control-url",
        "http://127.0.0.1:12112",
        "--fixture-control-token",
        "run-scoped-control-token",
    ])
    .expect("the reference-app replay command parses");

    let error = execute_async(cli)
        .await
        .expect_err("the CLI rejects non-loopback replay targets");

    assert!(
        error
            .to_string()
            .contains("reference app replay configuration is invalid"),
        "unexpected error: {error}"
    );
}
