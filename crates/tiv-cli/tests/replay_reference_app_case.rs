use std::{process::Command, time::SystemTime};

#[test]
#[ignore = "requires a fresh isolated reference-app Compose project"]
fn planned_reference_case_runs_live_http_quiescence_journal_and_oracle_boundaries() {
    let plan_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spike/planned-case-http-v1.json"
    );
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let journal_path = std::env::temp_dir().join(format!(
        "tiv-reference-case-{}-{nonce}.jsonl",
        std::process::id()
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args([
            "replay",
            "reference-app-case",
            "--plan",
            plan_path,
            "--journal",
            journal_path.to_str().unwrap(),
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
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["seed"], 3);
    assert_eq!(value["planned_action_count"], 8);
    assert_eq!(value["executed_action_count"], 8);
    assert_eq!(value["provider_object_count"], 1);
    assert!(value["database_oid"].as_u64().is_some_and(|oid| oid > 0));
    assert!(
        value["journal_record_count"]
            .as_u64()
            .is_some_and(|count| count >= 16)
    );
    assert!(
        value["journal_last_record_hash"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
    );
    let outcomes = value["invariant_outcomes"]
        .as_array()
        .expect("invariant outcomes are an array");
    assert_eq!(outcomes.len(), 5);
    assert!(outcomes.iter().all(|outcome| outcome["verdict"] == "held"));
    assert!(journal_path.exists());
    std::fs::remove_file(journal_path).unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("password"));
}
