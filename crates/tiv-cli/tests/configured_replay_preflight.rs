use std::process::Command;

#[test]
fn configured_replay_rejects_missing_source_evidence_before_config_or_stack_access() {
    let root =
        std::env::temp_dir().join(format!("tiv-missing-replay-source-{}", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args([
            "replay",
            "configured",
            "--artifact",
            root.to_str().unwrap(),
            "--config",
            "/definitely/not/a/project/tiv.toml",
            "--case",
            "1",
        ])
        .env_remove("TIV_POSTGRES_ADMIN_PASSWORD")
        .env_remove("TIV_POSTGRES_APPLICATION_PASSWORD")
        .env_remove("TIV_FIXTURE_CONTROL_TOKEN")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("configured replay source artifact is invalid"));
    assert!(!stderr.contains("project configuration"));
    assert!(!stderr.contains("docker"));
}

#[test]
fn configured_replay_rejects_an_out_of_range_case_before_reading_the_artifact() {
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args([
            "replay",
            "configured",
            "--artifact",
            "/definitely/not/a/run",
            "--case",
            "0",
        ])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("configured replay options are invalid")
    );
}
