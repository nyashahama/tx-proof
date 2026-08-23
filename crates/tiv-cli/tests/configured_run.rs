use std::{
    collections::BTreeMap, fmt::Write as _, fs, os::unix::fs::PermissionsExt, path::Path,
    process::Command,
};

use tiv_runtime::{artifacts::verify_complete_run_artifact, compatibility::RunCompatibilityV1};
use tokio_postgres::NoTls;
use uuid::Uuid;

static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const ADMIN_URL: &str = "postgresql://tiv_admin:tiv-local-only-password@127.0.0.1:15432/postgres";
const CASE_URL: &str =
    "postgresql://tiv_app:tiv-app-local-only-password@127.0.0.1:15432/tiv_case_deadbeef";
const CONFIG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/configured-run-project/tiv.toml"
);
const ARTIFACT_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/configured-run-project/.tiv"
);

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn configured_run_executes_a_real_case_and_finalizes_private_evidence() {
    let _guard = E2E_LOCK.lock().await;
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    let output = run_configured_command(4);

    let code = output.status.code();
    assert!(
        matches!(code, Some(0 | 10)),
        "configured run failed with {code:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("configured run stdout is JSON");
    assert_eq!(receipt["status"], "campaign_complete");
    assert_eq!(receipt["completed_cases"], 1);
    assert_eq!(
        code,
        Some(if receipt["verdict"] == "violated" {
            10
        } else {
            0
        })
    );

    let artifact_path = Path::new(receipt["artifact_path"].as_str().unwrap());
    assert!(artifact_path.join("manifest.json").is_file());
    assert!(artifact_path.join("compatibility.json").is_file());
    assert!(artifact_path.join("campaign-plan.json").is_file());
    assert!(artifact_path.join("cases/case_0001/trace.json").is_file());
    assert!(
        artifact_path
            .join("cases/case_0001/observations.ndjson")
            .is_file()
    );
    assert_eq!(
        fs::metadata(artifact_path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["configured_cases"], 1);
    assert_eq!(summary["completed_cases"], 1);
    assert_eq!(
        summary["cases"][0]["invariants"].as_array().unwrap().len(),
        5
    );
    assert_ne!(
        summary["cases"][0]["before_database_oid"],
        summary["cases"][0]["after_database_oid"]
    );
    let compatibility_bytes = fs::read(artifact_path.join("compatibility.json")).unwrap();
    let compatibility = RunCompatibilityV1::from_json(&compatibility_bytes)
        .expect("the run persists a valid replay compatibility contract");
    let verified = verify_complete_run_artifact(artifact_path)
        .expect("the finalized artifact and every indexed digest verify");
    assert_eq!(verified.run_id(), receipt["run_id"].as_str().unwrap());
    assert_eq!(verified.compatibility(), &compatibility);
    let compatibility_json: serde_json::Value =
        serde_json::from_slice(&compatibility_bytes).unwrap();
    assert_eq!(
        compatibility_json["config_digest"]
            .as_str()
            .expect("the redacted config digest is recorded")
            .len(),
        64
    );
    assert_eq!(
        compatibility_json["services"]
            .as_array()
            .expect("service images are recorded")
            .len(),
        3
    );
    assert!(
        compatibility_json["services"]
            .as_array()
            .unwrap()
            .iter()
            .all(|service| {
                service["image_id"]
                    .as_str()
                    .is_some_and(|image| image.starts_with("sha256:") && image.len() == 71)
                    && service["compose_config_hash"]
                        .as_str()
                        .is_some_and(|digest| digest.len() == 64)
            })
    );
    let artifact_bytes = read_artifact_tree(artifact_path);
    for secret in [
        "tiv-local-only-password",
        "tiv-app-local-only-password",
        "whsec_test_secret",
        "run-scoped-control-token",
    ] {
        assert!(
            !artifact_bytes.contains(secret),
            "secret material must not enter finalized artifacts"
        );
    }

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn configured_replay_reproduces_one_verified_failure_three_times() {
    let _guard = E2E_LOCK.lock().await;
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    let source_output = run_configured_command(8);
    assert_eq!(
        source_output.status.code(),
        Some(10),
        "seed 8 must record a violating source case: {}",
        String::from_utf8_lossy(&source_output.stderr)
    );
    let source_receipt: serde_json::Value = serde_json::from_slice(&source_output.stdout).unwrap();
    let source_path = Path::new(source_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(source_path).unwrap();

    let replay_output = configured_replay_command(source_path)
        .output()
        .expect("the configured replay command executes");
    assert_eq!(
        replay_output.status.code(),
        Some(10),
        "configured replay failed: {}",
        String::from_utf8_lossy(&replay_output.stderr)
    );
    let replay_receipt: serde_json::Value = serde_json::from_slice(&replay_output.stdout).unwrap();
    assert_eq!(replay_receipt["status"], "configured_replay_complete");
    assert_eq!(replay_receipt["source_run_id"], source_receipt["run_id"]);
    assert_eq!(replay_receipt["case_id"], "case_0001");
    assert_eq!(replay_receipt["attempt_count"], 3);
    assert_eq!(replay_receipt["matching_failure_count"], 3);
    assert_eq!(replay_receipt["classification"], "stable");

    let replay_path = Path::new(replay_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(replay_path).unwrap();
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(replay_path.join("summary.json")).unwrap()).unwrap();
    let attempts = summary["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 3);
    assert!(attempts.iter().all(|attempt| {
        attempt["trace_matches_source"] == true
            && attempt["verdict"] == "expected_violation"
            && attempt["invariants"]
                .as_array()
                .is_some_and(|items| items.len() == 5)
            && attempt["before_database_oid"] != attempt["after_database_oid"]
    }));
    assert_ne!(
        attempts[0]["after_database_oid"],
        attempts[1]["after_database_oid"]
    );
    assert_ne!(
        attempts[1]["after_database_oid"],
        attempts[2]["after_database_oid"]
    );
    for attempt in 1..=3 {
        assert!(
            replay_path
                .join(format!("attempts/attempt_{attempt:04}/trace.json"))
                .is_file()
        );
        assert!(
            replay_path
                .join(format!("attempts/attempt_{attempt:04}/observations.ndjson"))
                .is_file()
        );
    }
    let artifact_bytes = read_artifact_tree(replay_path);
    for secret in [
        "tiv-local-only-password",
        "tiv-app-local-only-password",
        "whsec_test_secret",
        "run-scoped-control-token",
    ] {
        assert!(!artifact_bytes.contains(secret));
    }
    assert_reference_app_healthy();

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn configured_replay_rejects_compatibility_drift_before_case_reset() {
    let _guard = E2E_LOCK.lock().await;
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    let source_output = run_configured_command(8);
    assert_eq!(source_output.status.code(), Some(10));
    let source_receipt: serde_json::Value = serde_json::from_slice(&source_output.stdout).unwrap();
    let source_path = Path::new(source_receipt["artifact_path"].as_str().unwrap());
    rewrite_compatibility_digest_and_reseal(source_path);
    verify_complete_run_artifact(source_path).unwrap();
    let identity_before = configured_case_identity().await;

    let replay_output = configured_replay_command(source_path).output().unwrap();

    assert_eq!(replay_output.status.code(), Some(2));
    assert!(replay_output.stdout.is_empty());
    assert!(
        String::from_utf8(replay_output.stderr)
            .unwrap()
            .contains("compatibility gate failed")
    );
    assert_eq!(configured_case_identity().await, identity_before);
    assert_eq!(
        fs::read_dir(Path::new(ARTIFACT_ROOT).join("runs"))
            .unwrap()
            .count(),
        1,
        "incompatible replay must not enter replay evidence staging"
    );
    assert_reference_app_healthy();

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn configured_replay_rechecks_compatibility_before_every_attempt_reset() {
    let _guard = E2E_LOCK.lock().await;
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let (project_root, config, compose_extension) = prepare_replay_drift_project();
    let artifact_root = project_root.join(".tiv");

    let source_output = configured_command_with_config(8, 1, &config)
        .output()
        .unwrap();
    assert_eq!(
        source_output.status.code(),
        Some(10),
        "drift fixture source run failed: {}",
        String::from_utf8_lossy(&source_output.stderr)
    );
    let source_receipt: serde_json::Value = serde_json::from_slice(&source_output.stdout).unwrap();
    let source_path = Path::new(source_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(source_path).unwrap();

    let mut child = configured_replay_command_with_config(source_path, &config)
        .spawn()
        .expect("the configured replay command starts");
    let staging = wait_for_replay_observation_in(&mut child, "action_intent", &artifact_root).await;
    fs::write(&compose_extension, "x-tiv-replay-drift: changed\n").unwrap();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .expect("the compatibility-gated replay exits within its case budget")
    .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "mid-replay compatibility drift was accepted: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("compatibility gate failed")
    );
    let final_path = staging.parent().unwrap().join(
        staging
            .file_name()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches('.')
            .trim_end_matches(".staging"),
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(final_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["failure_code"], "compatibility_mismatch");
    assert_eq!(summary["completed_attempts"], 1);
    assert!(
        !final_path
            .join("attempts/attempt_0002/observations.ndjson")
            .exists()
    );
    assert_reference_app_healthy();
    verify_complete_run_artifact(source_path).unwrap();

    cleanup_reference_databases().await;
    fs::remove_dir_all(project_root).unwrap();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn configured_replay_interrupts_with_recovery_and_partial_evidence() {
    let _guard = E2E_LOCK.lock().await;
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    let source_output = run_configured_command(8);
    assert_eq!(source_output.status.code(), Some(10));
    let source_receipt: serde_json::Value = serde_json::from_slice(&source_output.stdout).unwrap();
    let source_path = Path::new(source_receipt["artifact_path"].as_str().unwrap());
    let mut child = configured_replay_command(source_path)
        .spawn()
        .expect("the configured replay command starts");
    let staging = wait_for_replay_observation(&mut child, "action_intent").await;
    let interrupted = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .output()
        .unwrap();
    assert!(interrupted.status.success());
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .expect("interrupted replay exits within its recovery budget")
    .unwrap();
    assert_eq!(output.status.code(), Some(130));

    let final_path = staging.parent().unwrap().join(
        staging
            .file_name()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches('.')
            .trim_end_matches(".staging"),
    );
    assert!(final_path.is_dir());
    assert!(!staging.exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(final_path.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["complete"], false);
    assert_eq!(manifest["failure_class"], "interrupted");
    assert_reference_app_healthy();
    verify_complete_run_artifact(source_path).unwrap();

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn configured_run_kills_at_webhook_ingress_and_never_acknowledges_it() {
    let _guard = E2E_LOCK.lock().await;
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    let output = run_configured_command(67);
    let code = output.status.code();
    assert!(
        matches!(code, Some(0 | 10)),
        "configured ingress-cut run failed with {code:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("configured run stdout is JSON");
    let artifact_path = Path::new(receipt["artifact_path"].as_str().unwrap());
    let observations =
        fs::read_to_string(artifact_path.join("cases/case_0001/observations.ndjson")).unwrap();
    let records = observations
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let forwarded = records
        .iter()
        .position(|record| record["observation_kind"] == "webhook_request_forwarded")
        .expect("the journal records the accepted request before application persistence");
    let discarded = records
        .iter()
        .position(|record| record["observation_kind"] == "webhook_request_discarded")
        .expect("the journal records that the killed request was never acknowledged");
    assert!(forwarded < discarded);
    assert_eq!(
        records[forwarded]["payload"]["gate_id"],
        records[discarded]["payload"]["gate_id"]
    );

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn configured_run_resets_and_recovers_across_two_fresh_serial_cases() {
    let _guard = E2E_LOCK.lock().await;
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    let output = run_configured_command_with_cases(48, 2);
    let code = output.status.code();
    assert!(
        matches!(code, Some(0 | 10)),
        "two-case configured run failed with {code:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(receipt["completed_cases"], 2);
    let artifact_path = Path::new(receipt["artifact_path"].as_str().unwrap());
    assert!(artifact_path.join("cases/case_0002/trace.json").is_file());
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact_path.join("summary.json")).unwrap()).unwrap();
    let cases = summary["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 2);
    for case in cases {
        assert_ne!(case["before_database_oid"], case["after_database_oid"]);
    }
    assert_ne!(
        cases[0]["after_database_oid"],
        cases[1]["after_database_oid"]
    );
    assert_ne!(cases[0]["after_marker_uuid"], cases[1]["after_marker_uuid"]);
    assert_reference_app_healthy();

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn configured_run_interrupts_with_recovery_and_finalized_partial_evidence() {
    let _guard = E2E_LOCK.lock().await;
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    let mut child = configured_command(67, 1)
        .spawn()
        .expect("the configured campaign command starts");
    let staging = wait_for_observation(&mut child, "webhook_request_forwarded").await;
    let interrupted = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .output()
        .unwrap();
    assert!(interrupted.status.success());
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .expect("the interrupted command exits within its cleanup budget")
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "interrupted run returned {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let final_path = staging.parent().unwrap().join(
        staging
            .file_name()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches('.')
            .trim_end_matches(".staging"),
    );
    assert!(final_path.is_dir());
    assert!(!staging.exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(final_path.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["complete"], false);
    assert_eq!(manifest["failure_class"], "interrupted");
    assert_reference_app_healthy();

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
}

fn run_configured_command(seed: u64) -> std::process::Output {
    run_configured_command_with_cases(seed, 1)
}

fn run_configured_command_with_cases(seed: u64, cases: u32) -> std::process::Output {
    configured_command(seed, cases)
        .env("TIV_POSTGRES_ADMIN_URL", ADMIN_URL)
        .output()
        .expect("the configured campaign command executes")
}

fn configured_command(seed: u64, cases: u32) -> Command {
    configured_command_with_config(seed, cases, Path::new(CONFIG))
}

fn configured_command_with_config(seed: u64, cases: u32, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tiv"));
    command
        .args(["run", "--config"])
        .arg(config)
        .arg("--seed")
        .arg(seed.to_string())
        .arg("--cases")
        .arg(cases.to_string())
        .arg("--ci")
        .env("TIV_POSTGRES_ADMIN_URL", ADMIN_URL)
        .env("DATABASE_URL", CASE_URL)
        .env("TIV_STRIPE_WEBHOOK_SECRET", "whsec_test_secret")
        .env("TIV_FIXTURE_CONTROL_TOKEN", "run-scoped-control-token")
        .env("DOCKER_HOST", "tcp://127.0.0.1:9")
        .env("DOCKER_CONTEXT", "intentionally-remote")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

fn configured_replay_command(artifact: &Path) -> Command {
    configured_replay_command_with_config(artifact, Path::new(CONFIG))
}

fn configured_replay_command_with_config(artifact: &Path, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tiv"));
    command
        .args(["replay", "configured", "--artifact"])
        .arg(artifact)
        .arg("--config")
        .arg(config)
        .args(["--case", "1"])
        .env("TIV_POSTGRES_ADMIN_URL", ADMIN_URL)
        .env("DATABASE_URL", CASE_URL)
        .env("TIV_STRIPE_WEBHOOK_SECRET", "whsec_test_secret")
        .env("TIV_FIXTURE_CONTROL_TOKEN", "run-scoped-control-token")
        .env("DOCKER_HOST", "tcp://127.0.0.1:9")
        .env("DOCKER_CONTEXT", "intentionally-remote")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

async fn wait_for_observation(
    child: &mut std::process::Child,
    observation: &str,
) -> std::path::PathBuf {
    for _ in 0..6_000 {
        assert!(
            child.try_wait().unwrap().is_none(),
            "configured run exited before the requested observation"
        );
        let runs = Path::new(ARTIFACT_ROOT).join("runs");
        if let Ok(entries) = fs::read_dir(runs) {
            for entry in entries.flatten() {
                let staging = entry.path();
                if !entry.file_name().to_string_lossy().ends_with(".staging") {
                    continue;
                }
                let journal = staging.join("cases/case_0001/observations.ndjson");
                if fs::read_to_string(journal).is_ok_and(|contents| contents.contains(observation))
                {
                    return staging;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("configured run did not durably record {observation} before the test timeout");
}

async fn wait_for_replay_observation(
    child: &mut std::process::Child,
    observation: &str,
) -> std::path::PathBuf {
    wait_for_replay_observation_in(child, observation, Path::new(ARTIFACT_ROOT)).await
}

async fn wait_for_replay_observation_in(
    child: &mut std::process::Child,
    observation: &str,
    artifact_root: &Path,
) -> std::path::PathBuf {
    for _ in 0..6_000 {
        assert!(
            child.try_wait().unwrap().is_none(),
            "configured replay exited before the requested observation"
        );
        let runs = artifact_root.join("runs");
        if let Ok(entries) = fs::read_dir(runs) {
            for entry in entries.flatten() {
                let staging = entry.path();
                if !entry.file_name().to_string_lossy().ends_with(".staging") {
                    continue;
                }
                let journal = staging.join("attempts/attempt_0001/observations.ndjson");
                if fs::read_to_string(journal).is_ok_and(|contents| contents.contains(observation))
                {
                    return staging;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("configured replay did not record {observation} before the test timeout");
}

fn prepare_replay_drift_project() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests")
        .join(format!(".configured-replay-drift-{}", Uuid::new_v4()));
    fs::create_dir(&project_root).unwrap();
    let config = project_root.join("tiv.toml");
    let compose_extension = project_root.join("compose-drift.yaml");
    let contents = fs::read_to_string(CONFIG)
        .unwrap()
        .replace(
            "files = [\"../../spike/reference-app.compose.yaml\"]",
            "files = [\"../../spike/reference-app.compose.yaml\", \"compose-drift.yaml\"]",
        )
        .replace(
            "quiescence_sql = \"quiescence.sql\"",
            "quiescence_sql = \"../configured-run-project/quiescence.sql\"",
        )
        .replace(
            "body_file = \"checkout.json\"",
            "body_file = \"../configured-run-project/checkout.json\"",
        )
        .replace(
            "sql_probe_file = \"kill_probe.sql\"",
            "sql_probe_file = \"../configured-run-project/kill_probe.sql\"",
        )
        .replace(
            "sql_file = \"invariants/",
            "sql_file = \"../configured-run-project/invariants/",
        );
    fs::write(&config, contents).unwrap();
    fs::write(&compose_extension, "x-tiv-replay-drift: initial\n").unwrap();
    (project_root, config, compose_extension)
}

fn assert_reference_app_healthy() {
    let inspection = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "inspect",
            "--format",
            "{{.State.Running}} {{if .State.Health}}{{.State.Health.Status}}{{end}}",
            "tiv-reference-app-spike-reference-app-1",
        ])
        .output()
        .unwrap();
    assert!(inspection.status.success());
    assert_eq!(
        String::from_utf8_lossy(&inspection.stdout).trim(),
        "true healthy"
    );
}

async fn reset_fixture_process() {
    let container = "tiv-reference-app-spike-stripe-fixture-1";
    let restarted = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "restart",
            container,
        ])
        .output()
        .unwrap();
    assert!(restarted.status.success());
    assert_eq!(String::from_utf8_lossy(&restarted.stdout).trim(), container);
    for _ in 0..100 {
        let health = Command::new("docker")
            .args([
                "--host",
                "unix:///var/run/docker.sock",
                "inspect",
                "--format",
                "{{if .State.Health}}{{.State.Health.Status}}{{end}}",
                container,
            ])
            .output()
            .unwrap();
        if health.status.success() && String::from_utf8_lossy(&health.stdout).trim() == "healthy" {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the isolated fixture did not become healthy after reset");
}

#[allow(clippy::too_many_lines)]
async fn prepare_reference_baseline() {
    cleanup_reference_databases().await;
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute(
            "DO $$ BEGIN \
                 IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tiv_app') THEN \
                   CREATE ROLE tiv_app LOGIN PASSWORD 'tiv-app-local-only-password' \
                     NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS; \
                 ELSE \
                   ALTER ROLE tiv_app LOGIN PASSWORD 'tiv-app-local-only-password' \
                     NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS; \
                 END IF; \
                 IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tiv_invariant') THEN \
                   CREATE ROLE tiv_invariant NOLOGIN PASSWORD NULL \
                     NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS; \
                 ELSE \
                   ALTER ROLE tiv_invariant NOLOGIN PASSWORD NULL \
                     NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT NOREPLICATION NOBYPASSRLS; \
                 END IF; \
               END $$",
        )
        .await
        .unwrap();
    client
        .batch_execute("CREATE DATABASE tiv_base_deadbeef WITH OWNER tiv_admin")
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();

    let baseline_url = ADMIN_URL.replace("/postgres", "/tiv_base_deadbeef");
    let (baseline, connection) = tokio_postgres::connect(&baseline_url, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let marker = Uuid::new_v4();
    baseline
        .batch_execute(
            "CREATE TABLE tiv_verifier_marker ( \
                 singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton), \
                 marker_uuid uuid NOT NULL, \
                 marker_kind text NOT NULL CHECK (marker_kind IN ('baseline', 'case')), \
                 compose_project text NOT NULL, \
                 application_role text NOT NULL \
             ); \
             CREATE TABLE orders ( \
                 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                 operation_id text NOT NULL UNIQUE, \
                 amount_minor bigint NOT NULL CHECK (amount_minor > 0), \
                 currency text NOT NULL CHECK (currency ~ '^[a-z]{3}$'), \
                 status text NOT NULL CHECK (status IN ('pending', 'paid')) \
             ); \
             CREATE TABLE payments ( \
                 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                 operation_id text NOT NULL REFERENCES orders(operation_id), \
                 stripe_payment_intent_id text NOT NULL, \
                 amount_minor bigint NOT NULL CHECK (amount_minor > 0), \
                 currency text NOT NULL CHECK (currency ~ '^[a-z]{3}$'), \
                 status text NOT NULL CHECK (status IN ('pending', 'succeeded')) \
             ); \
             CREATE INDEX payments_operation_id_idx ON payments (operation_id); \
             CREATE INDEX payments_provider_id_idx ON payments (stripe_payment_intent_id); \
             REVOKE ALL ON SCHEMA public FROM PUBLIC; \
             GRANT USAGE ON SCHEMA public TO tiv_app, tiv_invariant; \
             REVOKE ALL ON TABLE tiv_verifier_marker, orders, payments \
                 FROM PUBLIC, tiv_app, tiv_invariant; \
             REVOKE ALL ON SEQUENCE orders_id_seq, payments_id_seq \
                 FROM PUBLIC, tiv_app, tiv_invariant; \
             GRANT SELECT (operation_id, amount_minor, currency) ON TABLE orders TO tiv_app; \
             GRANT INSERT ON TABLE payments TO tiv_app; \
             GRANT SELECT (operation_id, stripe_payment_intent_id), \
                   UPDATE (status) ON TABLE payments TO tiv_app; \
             GRANT USAGE ON SEQUENCE payments_id_seq TO tiv_app; \
             GRANT SELECT ON TABLE orders, payments TO tiv_invariant; \
             INSERT INTO orders (operation_id, amount_minor, currency, status) \
                 VALUES ('op_deadbeef', 2500, 'usd', 'pending')",
        )
        .await
        .unwrap();
    baseline
        .execute(
            "INSERT INTO tiv_verifier_marker \
                 (marker_uuid, marker_kind, compose_project, application_role) \
             VALUES ($1, 'baseline', 'tiv-reference-app-spike', 'tiv_app')",
            &[&marker],
        )
        .await
        .unwrap();
    drop(baseline);
    connection.await.unwrap().unwrap();

    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let seal = format!(
        "COMMENT ON DATABASE tiv_base_deadbeef IS \
         'tiv-baseline:v1:{marker}:tiv-reference-app-spike:tiv_app'; \
         ALTER DATABASE tiv_base_deadbeef IS_TEMPLATE true; \
         ALTER DATABASE tiv_base_deadbeef ALLOW_CONNECTIONS false"
    );
    client.batch_execute(&seal).await.unwrap();
    client
        .batch_execute(
            "CREATE DATABASE tiv_case_deadbeef WITH OWNER tiv_admin TEMPLATE tiv_base_deadbeef",
        )
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();

    let case_admin_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (case, connection) = tokio_postgres::connect(&case_admin_url, NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    case.execute(
        "UPDATE tiv_verifier_marker SET marker_uuid = $1, marker_kind = 'case'",
        &[&Uuid::new_v4()],
    )
    .await
    .unwrap();
    drop(case);
    connection.await.unwrap().unwrap();
}

async fn cleanup_reference_databases() {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    for database in ["tiv_case_deadbeef", "tiv_base_deadbeef"] {
        if client
            .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&database])
            .await
            .unwrap()
            .is_some()
        {
            client
                .batch_execute(&format!(
                    "ALTER DATABASE {database} IS_TEMPLATE false; \
                     ALTER DATABASE {database} ALLOW_CONNECTIONS false"
                ))
                .await
                .unwrap();
            client
                .execute(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = $1 AND pid <> pg_backend_pid()",
                    &[&database],
                )
                .await
                .unwrap();
            client
                .batch_execute(&format!("DROP DATABASE {database}"))
                .await
                .unwrap();
        }
    }
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn configured_case_identity() -> (u32, String) {
    let (admin, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let oid = admin
        .query_one(
            "SELECT oid::bigint FROM pg_database WHERE datname = 'tiv_case_deadbeef'",
            &[],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    drop(admin);
    connection.await.unwrap().unwrap();
    let case_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (case, connection) = tokio_postgres::connect(&case_url, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let marker = case
        .query_one(
            "SELECT marker_uuid::text FROM tiv_verifier_marker WHERE singleton",
            &[],
        )
        .await
        .unwrap()
        .get::<_, String>(0);
    drop(case);
    connection.await.unwrap().unwrap();
    (u32::try_from(oid).unwrap(), marker)
}

fn rewrite_compatibility_digest_and_reseal(artifact: &Path) {
    let compatibility_path = artifact.join("compatibility.json");
    let mut compatibility: serde_json::Value =
        serde_json::from_slice(&fs::read(&compatibility_path).unwrap()).unwrap();
    compatibility["config_digest"] = serde_json::Value::String("b".repeat(64));
    write_pretty_json(&compatibility_path, &compatibility);

    let manifest_path = artifact.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["required_files"]["compatibility.json"] = serde_json::Value::String(
        blake3::hash(&fs::read(&compatibility_path).unwrap())
            .to_hex()
            .to_string(),
    );
    let required =
        serde_json::from_value::<BTreeMap<String, String>>(manifest["required_files"].clone())
            .unwrap();
    let mut checksums = String::new();
    for (path, digest) in required {
        writeln!(checksums, "{digest}  {path}").unwrap();
    }
    fs::write(artifact.join("checksums.txt"), checksums.as_bytes()).unwrap();
    manifest["checksums_digest"] =
        serde_json::Value::String(blake3::hash(checksums.as_bytes()).to_hex().to_string());
    write_pretty_json(&manifest_path, &manifest);
}

fn write_pretty_json(path: &Path, value: &serde_json::Value) {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap();
    bytes.push(b'\n');
    fs::write(path, bytes).unwrap();
}

fn read_artifact_tree(root: &Path) -> String {
    let mut paths = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    let mut contents = String::new();
    for path in paths {
        if path.is_dir() {
            contents.push_str(&read_artifact_tree(&path));
        } else {
            contents.push_str(&String::from_utf8_lossy(&fs::read(path).unwrap()));
        }
    }
    contents
}
