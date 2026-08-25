use std::process::Command;

#[test]
#[ignore = "requires a fresh isolated reference-app Compose project"]
fn reference_app_evidence_command_ignores_remote_docker_context_and_emits_the_complete_proof() {
    let trace_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spike/compiled-trace-v1.json"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args([
            "replay",
            "reference-app-evidence",
            "--trace",
            trace_path,
            "--postgres-port",
            "15432",
            "--reference-app-url",
            "http://127.0.0.1:18080",
            "--fixture-control-url",
            "http://127.0.0.1:12112",
        ])
        .env("TIV_FIXTURE_CONTROL_TOKEN", "run-scoped-control-token")
        .env("TIV_POSTGRES_ADMIN_PASSWORD", "tiv-local-only-password")
        .env(
            "TIV_POSTGRES_APPLICATION_PASSWORD",
            "tiv-app-local-only-password",
        )
        .env("DOCKER_HOST", "tcp://127.0.0.1:9")
        .env("DOCKER_CONTEXT", "intentionally-remote")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .expect("the tiv binary executes");

    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON evidence document");
    assert_eq!(value["schema_version"], 2);
    assert_eq!(
        value["provider_object_counts"],
        serde_json::json!([2, 2, 2])
    );
    assert_eq!(
        value["failure_identity"]["invariant_id"],
        "provider-object-unique"
    );
    assert_eq!(value["reproduction"]["attempt_count"], 3);
    assert_eq!(value["reproduction"]["matching_failure_count"], 3);
    assert_eq!(value["reproduction"]["classification"], "stable");
    assert_eq!(
        value["database_resets"]
            .as_array()
            .expect("database resets are an array")
            .len(),
        2
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("password"));
}
