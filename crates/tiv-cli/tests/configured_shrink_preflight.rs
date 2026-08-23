use std::process::Command;

#[test]
fn configured_shrink_rejects_missing_source_before_config_or_stack_access() {
    let root =
        std::env::temp_dir().join(format!("tiv-missing-shrink-source-{}", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args([
            "shrink",
            "configured",
            "--artifact",
            root.to_str().unwrap(),
            "--config",
            "/definitely/not/a/project/tiv.toml",
        ])
        .env_remove("TIV_POSTGRES_ADMIN_PASSWORD")
        .env_remove("TIV_POSTGRES_APPLICATION_PASSWORD")
        .env_remove("TIV_FIXTURE_CONTROL_TOKEN")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("configured shrink source replay is invalid"));
    assert!(!stderr.contains("project configuration"));
    assert!(!stderr.contains("docker"));
}

#[test]
fn configured_shrink_rejects_candidate_bounds_before_reading_the_artifact() {
    for invalid in ["0", "61"] {
        let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
            .args([
                "shrink",
                "configured",
                "--artifact",
                "/definitely/not/a/run",
                "--max-candidates",
                invalid,
            ])
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("configured shrink options are invalid")
        );
    }
}
