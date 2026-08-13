use std::process::Command;

#[test]
#[ignore = "requires the local Docker Compose plugin"]
fn doctor_ignores_remote_context_emits_no_secrets_and_starts_no_containers() {
    let config_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args(["doctor", "--config", config_path])
        .env(
            "TIV_POSTGRES_ADMIN_URL",
            "postgresql://tiv_admin:admin-canary@127.0.0.1:15432/postgres",
        )
        .env(
            "DATABASE_URL",
            "postgresql://tiv_app:application-canary@127.0.0.1:15432/tiv_case_checkout",
        )
        .env("TIV_STRIPE_WEBHOOK_SECRET", "whsec_webhook-canary")
        .env("DOCKER_HOST", "tcp://127.0.0.1:9")
        .env("DOCKER_CONTEXT", "intentionally-remote")
        .output()
        .expect("the tiv binary executes");

    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("doctor output is UTF-8");
    for canary in ["admin-canary", "application-canary", "webhook-canary"] {
        assert!(!stdout.contains(canary), "doctor leaked {canary}");
    }
    let report: serde_json::Value =
        serde_json::from_str(&stdout).expect("doctor stdout is one JSON report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["status"], "ready");
    assert_eq!(report["mutation_authorized"], false);

    let project = report["probe_project_name"]
        .as_str()
        .expect("the report contains its isolated probe project");
    let containers = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "ps",
            "--all",
            "--quiet",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
        ])
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .output()
        .expect("local Docker can attest the probe project");
    assert!(containers.status.success());
    assert!(
        containers.stdout.is_empty(),
        "doctor must not create containers"
    );
}
