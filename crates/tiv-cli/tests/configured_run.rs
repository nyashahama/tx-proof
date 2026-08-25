use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tiv_core::decision::Seed;
use tiv_runtime::{artifacts::verify_complete_run_artifact, compatibility::RunCompatibilityV1};
use tiv_stripe_pi::{
    CreatePaymentIntent, FaultOutcome, IdempotencyKey, OperationId, PaymentIntentFixture,
};
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
const RETRY_KEY_MODE_ENV: &str = "TIV_REFERENCE_APP_RETRY_KEY_MODE";
const FAULTY_RETRY_KEY_MODE: &str = "faulty_changed_key";
const REPAIRED_RETRY_KEY_MODE: &str = "repaired_same_key";
const CALLER_RETRY_MODE_ENV: &str = "TIV_REFERENCE_APP_CALLER_RETRY_MODE";
const FAULTY_CALLER_RETRY_MODE: &str = "faulty_per_request";
const REPAIRED_CALLER_RETRY_MODE: &str = "repaired_recover_operation";
const RECONCILIATION_MODE_ENV: &str = "TIV_REFERENCE_APP_RECONCILIATION_MODE";
const FAULTY_RECONCILIATION_MODE: &str = "faulty_webhook_only";
const REPAIRED_RECONCILIATION_MODE: &str = "repaired_provider_reconcile";
const WEBHOOK_EFFECT_MODE_ENV: &str = "TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE";
const FAULTY_WEBHOOK_EFFECT_MODE: &str = "faulty_duplicate_effect";
const REPAIRED_WEBHOOK_EFFECT_MODE: &str = "repaired_deduplicate";
const LEDGER_BALANCE_MODE_ENV: &str = "TIV_REFERENCE_APP_LEDGER_MODE";
const FAULTY_LEDGER_BALANCE_MODE: &str = "faulty_one_sided_duplicate";
const REPAIRED_LEDGER_BALANCE_MODE: &str = "repaired_balanced_once";

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
#[allow(clippy::too_many_lines)]
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
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact_path.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], 2);
    assert_eq!(manifest["artifact"]["kind"], "configured_campaign");
    assert_eq!(
        manifest["artifact"]["result"],
        if receipt["verdict"] == "violated" {
            "counterexample"
        } else {
            "held"
        }
    );
    assert_eq!(manifest["artifact"]["exit_code"], code.unwrap());
    assert_eq!(
        manifest["provenance"]["source_artifacts"],
        serde_json::json!([])
    );
    assert_eq!(
        manifest["provenance"]["safety"]["state"],
        "initial_execution_boundary_attested"
    );
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
#[allow(clippy::too_many_lines)]
async fn row_three_changed_key_fault_violates_and_same_key_repair_holds() {
    let _guard = E2E_LOCK.lock().await;
    let mut restore = ReferenceAppModeRestore::armed();
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    recreate_reference_app_in_retry_mode("faulty_changed_key");
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let faulty_output = run_configured_command_in_retry_mode(69, "faulty_changed_key");
    assert_eq!(
        faulty_output.status.code(),
        Some(10),
        "the changed-key reference fault must violate: {}",
        String::from_utf8_lossy(&faulty_output.stderr)
    );
    let faulty_receipt: serde_json::Value =
        serde_json::from_slice(&faulty_output.stdout).expect("faulty stdout is JSON");
    assert_eq!(faulty_receipt["verdict"], "violated");
    let faulty_path = Path::new(faulty_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(faulty_path).expect("the faulty artifact verifies");
    let faulty_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(faulty_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(faulty_summary["cases"][0]["provider_object_count"], 2);
    let faulty_invariants = faulty_summary["cases"][0]["invariants"]
        .as_array()
        .expect("faulty invariants are recorded");
    let uniqueness = faulty_invariants
        .iter()
        .find(|invariant| invariant["invariant_id"] == "provider-object-unique")
        .expect("provider uniqueness is evaluated");
    assert_eq!(uniqueness["verdict"], "violated");
    assert_eq!(uniqueness["witness_count"], 1);
    assert_row_three_trace(faulty_path);

    recreate_reference_app_in_retry_mode("repaired_same_key");
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let repaired_output = run_configured_command_in_retry_mode(69, "repaired_same_key");
    assert_eq!(
        repaired_output.status.code(),
        Some(0),
        "the same-key control must hold: {}",
        String::from_utf8_lossy(&repaired_output.stderr)
    );
    let repaired_receipt: serde_json::Value =
        serde_json::from_slice(&repaired_output.stdout).expect("repaired stdout is JSON");
    assert_eq!(repaired_receipt["verdict"], "held");
    let repaired_path = Path::new(repaired_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(repaired_path).expect("the repaired artifact verifies");
    let repaired_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(repaired_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(repaired_summary["cases"][0]["provider_object_count"], 1);
    assert!(
        repaired_summary["cases"][0]["invariants"]
            .as_array()
            .is_some_and(|invariants| {
                invariants.len() == 5
                    && invariants
                        .iter()
                        .all(|invariant| invariant["verdict"] == "held")
            })
    );
    assert_row_three_trace(repaired_path);

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
    recreate_reference_app_in_retry_mode("faulty_changed_key");
    restore.disarm();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
#[allow(clippy::too_many_lines)]
async fn row_four_lost_response_caller_retry_fault_violates_and_recovery_holds() {
    let _guard = E2E_LOCK.lock().await;
    let mut restore = ReferenceAppModeRestore::armed();
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);
    let faulty_modes = (
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );

    recreate_reference_app_in_all_modes(
        faulty_modes.0,
        faulty_modes.1,
        faulty_modes.2,
        faulty_modes.3,
    );
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let faulty_output = run_configured_command_in_all_modes(
        422,
        faulty_modes.0,
        faulty_modes.1,
        faulty_modes.2,
        faulty_modes.3,
    );
    assert_eq!(
        faulty_output.status.code(),
        Some(10),
        "the per-request caller retry must violate: {}",
        String::from_utf8_lossy(&faulty_output.stderr)
    );
    let faulty_receipt: serde_json::Value =
        serde_json::from_slice(&faulty_output.stdout).expect("faulty stdout is JSON");
    let faulty_path = Path::new(faulty_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(faulty_path).expect("the faulty artifact verifies");
    let faulty_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(faulty_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(faulty_summary["cases"][0]["provider_object_count"], 2);
    assert!(
        faulty_summary["cases"][0]["invariants"]
            .as_array()
            .is_some_and(|invariants| {
                invariants.len() == 5
                    && invariants.iter().all(|invariant| {
                        if invariant["invariant_id"] == "provider-object-unique" {
                            invariant["verdict"] == "violated" && invariant["witness_count"] == 1
                        } else {
                            invariant["verdict"] == "held" && invariant["witness_count"] == 0
                        }
                    })
            })
    );
    assert_row_four_trace(faulty_path);
    assert_reference_payment_relation((2, 2)).await;

    let replay_output = configured_replay_command_in_all_modes(
        faulty_path,
        faulty_modes.0,
        faulty_modes.1,
        faulty_modes.2,
        faulty_modes.3,
    )
    .output()
    .expect("the row-four configured replay executes");
    assert_eq!(
        replay_output.status.code(),
        Some(10),
        "row-four replay failed: {}",
        String::from_utf8_lossy(&replay_output.stderr)
    );
    let replay_receipt: serde_json::Value =
        serde_json::from_slice(&replay_output.stdout).expect("replay stdout is JSON");
    assert_eq!(replay_receipt["attempt_count"], 3);
    assert_eq!(replay_receipt["matching_failure_count"], 3);
    assert_eq!(replay_receipt["classification"], "stable");
    let replay_path = Path::new(replay_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(replay_path).expect("the row-four replay artifact verifies");
    let replay_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(replay_path.join("summary.json")).unwrap()).unwrap();
    assert!(
        replay_summary["attempts"]
            .as_array()
            .is_some_and(|attempts| {
                attempts.len() == 3
                    && attempts.iter().all(|attempt| {
                        attempt["verdict"] == "expected_violation"
                            && exact_invariant_vector(attempt, "provider-object-unique", true)
                    })
            })
    );

    let shrink_output = configured_shrink_command_in_all_modes(
        replay_path,
        faulty_modes.0,
        faulty_modes.1,
        faulty_modes.2,
        faulty_modes.3,
    )
    .output()
    .expect("the row-four configured shrink executes");
    assert!(
        matches!(shrink_output.status.code(), Some(10 | 11)),
        "row-four shrink failed: {}",
        String::from_utf8_lossy(&shrink_output.stderr)
    );
    let shrink_receipt: serde_json::Value =
        serde_json::from_slice(&shrink_output.stdout).expect("shrink stdout is JSON");
    assert!(
        shrink_receipt["evaluated_candidates"]
            .as_u64()
            .is_some_and(|count| (1..=3).contains(&count))
    );
    assert!(
        shrink_receipt["accepted_candidates"]
            .as_u64()
            .is_some_and(|count| count >= 1)
    );
    assert!(
        shrink_receipt["best_action_count"].as_u64()
            < shrink_receipt["original_action_count"].as_u64()
    );
    let shrink_path = Path::new(shrink_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(shrink_path).expect("the row-four shrink artifact verifies");
    let shrink_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(shrink_path.join("summary.json")).unwrap()).unwrap();
    for candidate in shrink_summary["candidates"].as_array().unwrap() {
        let candidate_id = candidate["candidate_id"].as_str().unwrap();
        let candidate_trace: serde_json::Value = serde_json::from_slice(
            &fs::read(
                shrink_path
                    .join("candidates")
                    .join(candidate_id)
                    .join("candidate.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let schedule = candidate_trace["schedule"].as_array().unwrap();
        let retains_kill = schedule.iter().any(|action| {
            action["kind"]["kind"] == "kill_application"
                && action["kind"]["cut_point"] == "client_response_observed"
        });
        let retains_retry = schedule
            .iter()
            .any(|action| action["kind"]["kind"] == "retry_business_request");
        if !retains_kill || !retains_retry {
            assert_eq!(candidate["accepted"], false);
        }
    }
    let minimized_authority: serde_json::Value =
        serde_json::from_slice(&fs::read(shrink_path.join("trace.minimized.json")).unwrap())
            .unwrap();
    let minimized_schedule = minimized_authority["candidate"]["schedule"]
        .as_array()
        .expect("the row-four minimized authority retains a schedule");
    assert!(minimized_schedule.iter().any(|action| {
        action["kind"]["kind"] == "kill_application"
            && action["kind"]["cut_point"] == "client_response_observed"
    }));
    assert!(
        minimized_schedule
            .iter()
            .any(|action| action["kind"]["kind"] == "retry_business_request")
    );

    let minimized_output = configured_minimized_replay_command_in_all_modes(
        shrink_path,
        faulty_modes.0,
        faulty_modes.1,
        faulty_modes.2,
        faulty_modes.3,
    )
    .output()
    .expect("the row-four minimized replay executes");
    assert_eq!(
        minimized_output.status.code(),
        Some(10),
        "row-four minimized replay failed: {}",
        String::from_utf8_lossy(&minimized_output.stderr)
    );
    let minimized_receipt: serde_json::Value =
        serde_json::from_slice(&minimized_output.stdout).expect("minimized stdout is JSON");
    assert_eq!(minimized_receipt["attempt_count"], 3);
    assert_eq!(minimized_receipt["matching_failure_count"], 3);
    assert_eq!(minimized_receipt["classification"], "stable");
    let minimized_path = Path::new(minimized_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(minimized_path)
        .expect("the row-four minimized replay artifact verifies");
    let minimized_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(minimized_path.join("summary.json")).unwrap()).unwrap();
    assert!(
        minimized_summary["attempts"]
            .as_array()
            .is_some_and(|attempts| {
                attempts.len() == 3
                    && attempts.iter().all(|attempt| {
                        exact_invariant_vector(attempt, "provider-object-unique", true)
                    })
            })
    );

    recreate_reference_app_in_all_modes(
        REPAIRED_RETRY_KEY_MODE,
        REPAIRED_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let repaired_output = run_configured_command_in_all_modes(
        422,
        REPAIRED_RETRY_KEY_MODE,
        REPAIRED_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    assert_eq!(
        repaired_output.status.code(),
        Some(0),
        "the operation recovery control must hold: {}",
        String::from_utf8_lossy(&repaired_output.stderr)
    );
    let repaired_receipt: serde_json::Value =
        serde_json::from_slice(&repaired_output.stdout).expect("repaired stdout is JSON");
    let repaired_path = Path::new(repaired_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(repaired_path).expect("the repaired artifact verifies");
    let repaired_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(repaired_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(repaired_summary["cases"][0]["provider_object_count"], 1);
    assert!(
        repaired_summary["cases"][0]["invariants"]
            .as_array()
            .is_some_and(|invariants| {
                invariants.len() == 5
                    && invariants
                        .iter()
                        .all(|invariant| invariant["verdict"] == "held")
            })
    );
    assert_row_four_trace(repaired_path);
    assert_reference_payment_relation((1, 1)).await;

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
    recreate_reference_app_in_all_modes(
        FAULTY_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    restore.disarm();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
#[allow(clippy::too_many_lines)]
async fn row_five_dropped_success_fault_violates_and_reconciliation_converges() {
    let _guard = E2E_LOCK.lock().await;
    let mut restore = ReferenceAppModeRestore::armed();
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    recreate_reference_app_in_reconciliation_mode(FAULTY_RECONCILIATION_MODE);
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let faulty_output =
        run_configured_command_in_reconciliation_mode(359, FAULTY_RECONCILIATION_MODE);
    assert_eq!(
        faulty_output.status.code(),
        Some(10),
        "the dropped webhook without reconciliation must violate: {}",
        String::from_utf8_lossy(&faulty_output.stderr)
    );
    let faulty_receipt: serde_json::Value =
        serde_json::from_slice(&faulty_output.stdout).expect("faulty stdout is JSON");
    let faulty_path = Path::new(faulty_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(faulty_path).expect("the faulty artifact verifies");
    let faulty_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(faulty_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(faulty_summary["cases"][0]["provider_object_count"], 1);
    assert!(
        faulty_summary["cases"][0]["invariants"]
            .as_array()
            .is_some_and(|invariants| {
                invariants.len() == 5
                    && invariants.iter().all(|invariant| {
                        if invariant["invariant_id"] == "paid-order-amount-conservation" {
                            invariant["verdict"] == "violated" && invariant["witness_count"] == 1
                        } else {
                            invariant["verdict"] == "held" && invariant["witness_count"] == 0
                        }
                    })
            })
    );
    assert_row_five_trace(faulty_path);
    assert_row_five_fault_horizon(faulty_path);
    assert_reference_payment_status((1, 0)).await;

    let replay_output =
        configured_replay_command_in_reconciliation_mode(faulty_path, FAULTY_RECONCILIATION_MODE)
            .output()
            .expect("the row-five configured replay executes");
    assert_eq!(
        replay_output.status.code(),
        Some(10),
        "row-five replay failed: {}",
        String::from_utf8_lossy(&replay_output.stderr)
    );
    let replay_receipt: serde_json::Value =
        serde_json::from_slice(&replay_output.stdout).expect("replay stdout is JSON");
    assert_eq!(replay_receipt["attempt_count"], 3);
    assert_eq!(replay_receipt["matching_failure_count"], 3);
    assert_eq!(replay_receipt["classification"], "stable");
    let replay_path = Path::new(replay_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(replay_path).expect("the row-five replay artifact verifies");
    let replay_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(replay_path.join("summary.json")).unwrap()).unwrap();
    assert!(
        replay_summary["attempts"]
            .as_array()
            .is_some_and(|attempts| {
                attempts.len() == 3
                    && attempts.iter().all(|attempt| {
                        attempt["verdict"] == "expected_violation"
                            && exact_invariant_vector(
                                attempt,
                                "paid-order-amount-conservation",
                                true,
                            )
                    })
            })
    );

    let shrink_output =
        configured_shrink_command_in_reconciliation_mode(replay_path, FAULTY_RECONCILIATION_MODE)
            .output()
            .expect("the row-five configured shrink executes");
    assert!(
        matches!(shrink_output.status.code(), Some(10 | 11)),
        "row-five shrink failed: {}",
        String::from_utf8_lossy(&shrink_output.stderr)
    );
    let shrink_receipt: serde_json::Value =
        serde_json::from_slice(&shrink_output.stdout).expect("shrink stdout is JSON");
    assert!(
        shrink_receipt["evaluated_candidates"]
            .as_u64()
            .is_some_and(|count| (1..=3).contains(&count))
    );
    assert!(
        shrink_receipt["accepted_candidates"]
            .as_u64()
            .is_some_and(|count| count >= 1)
    );
    assert!(
        shrink_receipt["best_action_count"].as_u64()
            < shrink_receipt["original_action_count"].as_u64()
    );
    let shrink_path = Path::new(shrink_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(shrink_path).expect("the row-five shrink artifact verifies");
    let shrink_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(shrink_path.join("summary.json")).unwrap()).unwrap();
    for candidate in shrink_summary["candidates"].as_array().unwrap() {
        let candidate_id = candidate["candidate_id"].as_str().unwrap();
        let candidate_trace: serde_json::Value = serde_json::from_slice(
            &fs::read(
                shrink_path
                    .join("candidates")
                    .join(candidate_id)
                    .join("candidate.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let retains_drop = candidate_trace["schedule"]
            .as_array()
            .is_some_and(|schedule| {
                schedule
                    .iter()
                    .any(|action| action["kind"]["kind"] == "drop_webhook")
            });
        if !retains_drop {
            assert_eq!(candidate["accepted"], false);
        }
    }
    let minimized_authority: serde_json::Value =
        serde_json::from_slice(&fs::read(shrink_path.join("trace.minimized.json")).unwrap())
            .unwrap();
    assert!(
        minimized_authority["candidate"]["schedule"]
            .as_array()
            .is_some_and(|schedule| {
                schedule
                    .iter()
                    .any(|action| action["kind"]["kind"] == "drop_webhook")
            }),
        "the minimized authority must retain the dropped success event"
    );

    let minimized_output = configured_minimized_replay_command_in_reconciliation_mode(
        shrink_path,
        FAULTY_RECONCILIATION_MODE,
    )
    .output()
    .expect("the row-five minimized replay executes");
    assert_eq!(
        minimized_output.status.code(),
        Some(10),
        "row-five minimized replay failed: {}",
        String::from_utf8_lossy(&minimized_output.stderr)
    );
    let minimized_receipt: serde_json::Value =
        serde_json::from_slice(&minimized_output.stdout).expect("minimized stdout is JSON");
    assert_eq!(minimized_receipt["attempt_count"], 3);
    assert_eq!(minimized_receipt["matching_failure_count"], 3);
    assert_eq!(minimized_receipt["classification"], "stable");
    let minimized_path = Path::new(minimized_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(minimized_path)
        .expect("the row-five minimized replay artifact verifies");
    let minimized_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(minimized_path.join("summary.json")).unwrap()).unwrap();
    assert!(
        minimized_summary["attempts"]
            .as_array()
            .is_some_and(|attempts| {
                attempts.len() == 3
                    && attempts.iter().all(|attempt| {
                        exact_invariant_vector(attempt, "paid-order-amount-conservation", true)
                    })
            })
    );

    recreate_reference_app_in_reconciliation_mode(REPAIRED_RECONCILIATION_MODE);
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let repaired_output =
        run_configured_command_in_reconciliation_mode(359, REPAIRED_RECONCILIATION_MODE);
    assert_eq!(
        repaired_output.status.code(),
        Some(0),
        "the provider reconciliation control must converge: {}",
        String::from_utf8_lossy(&repaired_output.stderr)
    );
    let repaired_receipt: serde_json::Value =
        serde_json::from_slice(&repaired_output.stdout).expect("repaired stdout is JSON");
    let repaired_path = Path::new(repaired_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(repaired_path).expect("the repaired artifact verifies");
    let repaired_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(repaired_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(repaired_summary["cases"][0]["provider_object_count"], 1);
    assert!(
        repaired_summary["cases"][0]["invariants"]
            .as_array()
            .is_some_and(|invariants| {
                invariants.len() == 5
                    && invariants
                        .iter()
                        .all(|invariant| invariant["verdict"] == "held")
            })
    );
    assert_row_five_trace(repaired_path);
    assert_reference_payment_status((0, 1)).await;

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
    recreate_reference_app_in_all_modes(
        FAULTY_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    restore.disarm();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
#[allow(clippy::too_many_lines)]
async fn row_one_duplicate_webhook_fault_violates_and_deduplicated_repair_holds() {
    let _guard = E2E_LOCK.lock().await;
    let mut restore = ReferenceAppModeRestore::armed();
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);

    recreate_reference_app_in_modes(REPAIRED_RETRY_KEY_MODE, FAULTY_WEBHOOK_EFFECT_MODE);
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let faulty_output =
        run_configured_command_in_modes(1_792, REPAIRED_RETRY_KEY_MODE, FAULTY_WEBHOOK_EFFECT_MODE);
    assert_eq!(
        faulty_output.status.code(),
        Some(10),
        "the duplicate-effect reference fault must violate: {}",
        String::from_utf8_lossy(&faulty_output.stderr)
    );
    let faulty_receipt: serde_json::Value =
        serde_json::from_slice(&faulty_output.stdout).expect("faulty stdout is JSON");
    assert_eq!(faulty_receipt["verdict"], "violated");
    let faulty_path = Path::new(faulty_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(faulty_path).expect("the faulty artifact verifies");
    let faulty_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(faulty_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(faulty_summary["cases"][0]["provider_object_count"], 1);
    let faulty_invariants = faulty_summary["cases"][0]["invariants"]
        .as_array()
        .expect("faulty invariants are recorded");
    let at_most_once = faulty_invariants
        .iter()
        .find(|invariant| invariant["invariant_id"] == "webhook-effect-at-most-once")
        .expect("webhook effect uniqueness is evaluated");
    assert_eq!(at_most_once["verdict"], "violated");
    assert_eq!(at_most_once["witness_count"], 1);
    assert_row_one_trace(faulty_path);
    assert_reference_webhook_delivery_count(2).await;
    assert_reference_webhook_effect_count(2).await;

    let replay_output = configured_replay_command_in_modes(
        faulty_path,
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_WEBHOOK_EFFECT_MODE,
    )
    .output()
    .expect("the row-one configured replay executes");
    assert_eq!(
        replay_output.status.code(),
        Some(10),
        "row-one replay failed: {}",
        String::from_utf8_lossy(&replay_output.stderr)
    );
    let replay_receipt: serde_json::Value =
        serde_json::from_slice(&replay_output.stdout).expect("replay stdout is JSON");
    assert_eq!(replay_receipt["attempt_count"], 3);
    assert_eq!(replay_receipt["matching_failure_count"], 3);
    assert_eq!(replay_receipt["classification"], "stable");
    let replay_path = Path::new(replay_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(replay_path).expect("the row-one replay artifact verifies");
    let replay_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(replay_path.join("summary.json")).unwrap()).unwrap();
    assert!(
        replay_summary["attempts"]
            .as_array()
            .is_some_and(|attempts| {
                attempts.len() == 3
                    && attempts.iter().all(|attempt| {
                        attempt["verdict"] == "expected_violation"
                            && recorded_invariant(
                                attempt,
                                "webhook-effect-at-most-once",
                                "violated",
                                1,
                            )
                    })
            })
    );

    let shrink_output = configured_shrink_command_in_modes(
        replay_path,
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_WEBHOOK_EFFECT_MODE,
    )
    .output()
    .expect("the row-one configured shrink executes");
    assert!(
        matches!(shrink_output.status.code(), Some(10 | 11)),
        "row-one shrink failed: {}",
        String::from_utf8_lossy(&shrink_output.stderr)
    );
    let shrink_receipt: serde_json::Value =
        serde_json::from_slice(&shrink_output.stdout).expect("shrink stdout is JSON");
    assert_eq!(shrink_receipt["evaluated_candidates"], 3);
    assert_eq!(shrink_receipt["accepted_candidates"], 1);
    assert_eq!(shrink_receipt["original_action_count"], 8);
    assert_eq!(shrink_receipt["best_action_count"], 7);
    let shrink_path = Path::new(shrink_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(shrink_path).expect("the row-one shrink artifact verifies");
    assert!(shrink_path.join("trace.minimized.json").is_file());
    let shrink_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(shrink_path.join("summary.json")).unwrap()).unwrap();
    let shrink_candidates = shrink_summary["candidates"]
        .as_array()
        .expect("the bounded row-one candidates are recorded");
    assert_eq!(shrink_candidates.len(), 3);
    assert!(
        shrink_candidates
            .iter()
            .any(|candidate| { candidate["accepted"] == true && candidate["action_count"] == 7 })
    );
    assert!(
        shrink_candidates
            .iter()
            .filter(|candidate| candidate["accepted"] == false)
            .any(|candidate| {
                let Some(candidate_id) = candidate["candidate_id"].as_str() else {
                    return false;
                };
                let candidate: serde_json::Value = serde_json::from_slice(
                    &fs::read(
                        shrink_path
                            .join("candidates")
                            .join(candidate_id)
                            .join("candidate.json"),
                    )
                    .unwrap(),
                )
                .unwrap();
                candidate["schedule"].as_array().is_some_and(|schedule| {
                    schedule
                        .iter()
                        .all(|action| action["kind"]["kind"] != "duplicate_webhook")
                })
            }),
        "removing the causal duplicate must be evaluated and rejected"
    );

    let minimized_output = configured_minimized_replay_command_in_modes(
        shrink_path,
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_WEBHOOK_EFFECT_MODE,
    )
    .output()
    .expect("the row-one minimized replay executes");
    assert_eq!(
        minimized_output.status.code(),
        Some(10),
        "row-one minimized replay failed: {}",
        String::from_utf8_lossy(&minimized_output.stderr)
    );
    let minimized_receipt: serde_json::Value =
        serde_json::from_slice(&minimized_output.stdout).expect("minimized stdout is JSON");
    assert_eq!(minimized_receipt["attempt_count"], 3);
    assert_eq!(minimized_receipt["matching_failure_count"], 3);
    assert_eq!(minimized_receipt["classification"], "stable");
    let minimized_path = Path::new(minimized_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(minimized_path)
        .expect("the row-one minimized replay artifact verifies");
    let minimized_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(minimized_path.join("summary.json")).unwrap()).unwrap();
    assert!(
        minimized_summary["attempts"]
            .as_array()
            .is_some_and(|attempts| {
                attempts.len() == 3
                    && attempts.iter().all(|attempt| {
                        recorded_invariant(attempt, "webhook-effect-at-most-once", "violated", 1)
                    })
            })
    );

    recreate_reference_app_in_modes(REPAIRED_RETRY_KEY_MODE, REPAIRED_WEBHOOK_EFFECT_MODE);
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let repaired_output = run_configured_command_in_modes(
        1_792,
        REPAIRED_RETRY_KEY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
    );
    assert_eq!(
        repaired_output.status.code(),
        Some(0),
        "the deduplicated control must hold: {}",
        String::from_utf8_lossy(&repaired_output.stderr)
    );
    let repaired_receipt: serde_json::Value =
        serde_json::from_slice(&repaired_output.stdout).expect("repaired stdout is JSON");
    assert_eq!(repaired_receipt["verdict"], "held");
    let repaired_path = Path::new(repaired_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(repaired_path).expect("the repaired artifact verifies");
    let repaired_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(repaired_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(repaired_summary["cases"][0]["provider_object_count"], 1);
    assert!(
        repaired_summary["cases"][0]["invariants"]
            .as_array()
            .is_some_and(|invariants| {
                invariants.len() == 5
                    && invariants
                        .iter()
                        .all(|invariant| invariant["verdict"] == "held")
            })
    );
    assert_row_one_trace(repaired_path);
    assert_reference_webhook_delivery_count(2).await;
    assert_reference_webhook_effect_count(1).await;

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
    recreate_reference_app_in_modes(FAULTY_RETRY_KEY_MODE, REPAIRED_WEBHOOK_EFFECT_MODE);
    restore.disarm();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
#[allow(clippy::too_many_lines)]
async fn row_six_one_sided_ledger_fault_violates_and_balanced_repair_holds() {
    let _guard = E2E_LOCK.lock().await;
    let mut restore = ReferenceAppModeRestore::armed();
    let _ = fs::remove_dir_all(ARTIFACT_ROOT);
    let faulty_modes = (
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        FAULTY_LEDGER_BALANCE_MODE,
    );

    recreate_reference_app_in_all_modes(
        faulty_modes.0,
        faulty_modes.1,
        faulty_modes.2,
        faulty_modes.3,
    );
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let faulty_output = run_configured_command_in_all_modes(
        1_792,
        faulty_modes.0,
        faulty_modes.1,
        faulty_modes.2,
        faulty_modes.3,
    );
    assert_eq!(
        faulty_output.status.code(),
        Some(10),
        "the one-sided ledger fault must violate: {}",
        String::from_utf8_lossy(&faulty_output.stderr)
    );
    let faulty_receipt: serde_json::Value =
        serde_json::from_slice(&faulty_output.stdout).expect("faulty stdout is JSON");
    assert_eq!(faulty_receipt["verdict"], "violated");
    let faulty_path = Path::new(faulty_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(faulty_path).expect("the faulty artifact verifies");
    let faulty_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(faulty_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(faulty_summary["cases"][0]["provider_object_count"], 1);
    assert!(
        faulty_summary["cases"][0]["invariants"]
            .as_array()
            .is_some_and(|invariants| {
                invariants.len() == 5
                    && invariants.iter().all(|invariant| {
                        if invariant["invariant_id"] == "balanced-ledger" {
                            invariant["verdict"] == "violated" && invariant["witness_count"] == 1
                        } else {
                            invariant["verdict"] == "held" && invariant["witness_count"] == 0
                        }
                    })
            }),
        "balanced-ledger must be the only source failure"
    );
    assert_row_one_trace(faulty_path);
    assert_reference_webhook_delivery_count(2).await;
    assert_reference_webhook_effect_count(1).await;
    assert_reference_ledger_state((2, 3, 5_000, 2_500, 1, 2_500)).await;
    assert_balanced_ledger_witness_artifact(
        &faulty_path.join("cases/case_0001/invariants/witnesses.json"),
        2_500,
    );

    prove_duplicate_fault_replay_shrink(faulty_path, faulty_modes, "balanced-ledger");

    recreate_reference_app_in_all_modes(
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    prepare_reference_baseline().await;
    reset_fixture_process().await;
    let repaired_output = run_configured_command_in_all_modes(
        1_792,
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    assert_eq!(
        repaired_output.status.code(),
        Some(0),
        "the balanced ledger control must hold: {}",
        String::from_utf8_lossy(&repaired_output.stderr)
    );
    let repaired_receipt: serde_json::Value =
        serde_json::from_slice(&repaired_output.stdout).expect("repaired stdout is JSON");
    assert_eq!(repaired_receipt["verdict"], "held");
    let repaired_path = Path::new(repaired_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(repaired_path).expect("the repaired artifact verifies");
    let repaired_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(repaired_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(repaired_summary["cases"][0]["provider_object_count"], 1);
    assert!(
        repaired_summary["cases"][0]["invariants"]
            .as_array()
            .is_some_and(|invariants| {
                invariants.len() == 5
                    && invariants
                        .iter()
                        .all(|invariant| invariant["verdict"] == "held")
            })
    );
    assert_row_one_trace(repaired_path);
    assert_reference_webhook_delivery_count(2).await;
    assert_reference_webhook_effect_count(1).await;
    assert_reference_ledger_state((1, 2, 2_500, 2_500, 0, 0)).await;
    assert!(
        !repaired_path
            .join("cases/case_0001/invariants/witnesses.json")
            .exists(),
        "a held invariant does not emit a violation witness artifact"
    );

    cleanup_reference_databases().await;
    fs::remove_dir_all(ARTIFACT_ROOT).unwrap();
    recreate_reference_app_in_all_modes(
        FAULTY_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    restore.disarm();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn webhook_event_identity_collision_returns_conflict_before_business_mutation() {
    let _guard = E2E_LOCK.lock().await;
    let mut restore = ReferenceAppModeRestore::armed();
    recreate_reference_app_in_modes(REPAIRED_RETRY_KEY_MODE, REPAIRED_WEBHOOK_EFFECT_MODE);
    prepare_reference_baseline().await;

    let mut fixture = PaymentIntentFixture::new(Seed::new(5_171));
    let payment_intent = fixture
        .create(
            IdempotencyKey::new("identity-collision-contract").unwrap(),
            CreatePaymentIntent::new(2_500, "usd")
                .unwrap()
                .with_operation_id(OperationId::new("op_deadbeef").unwrap()),
            FaultOutcome::Normal,
        )
        .expect("the collision contract creates one provider object");
    fixture
        .confirm(payment_intent.id())
        .expect("the provider object reaches succeeded");
    let event = fixture
        .events()
        .first()
        .expect("confirmation creates one immutable event");
    let event_id = event.id().to_owned();
    let attempt = event
        .webhook_attempt(current_unix_timestamp(), b"whsec_test_secret")
        .expect("the immutable event is signed for delivery");

    let case_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (admin, connection) = tokio_postgres::connect(&case_url, NoTls)
        .await
        .expect("the isolated admin connects to the generated case");
    let connection = tokio::spawn(connection);
    admin
        .execute(
            "INSERT INTO orders (operation_id, amount_minor, currency, status) \
             VALUES ('op_collision', 2500, 'usd', 'pending')",
            &[],
        )
        .await
        .expect("the collision probe has a second valid operation");
    admin
        .execute(
            "INSERT INTO processed_webhook_events (provider_event_id, operation_id) \
             VALUES ($1, 'op_collision')",
            &[&event_id],
        )
        .await
        .expect("the provider event ID is already claimed by another operation");

    let response = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .post("http://127.0.0.1:18080/webhooks/stripe")
        .header("Stripe-Signature", attempt.signature_header())
        .header("Connection", "close")
        .body(attempt.raw_body().to_vec())
        .send()
        .await
        .expect("the signed collision receives an application response");
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(response.text().await.unwrap(), "webhook identity conflict");

    let state = admin
        .query_one(
            "SELECT \
                 (SELECT COUNT(*)::bigint FROM payments \
                  WHERE operation_id IN ('op_deadbeef', 'op_collision')), \
                 (SELECT COUNT(*)::bigint FROM webhook_deliveries \
                  WHERE provider_event_id = $1 AND operation_id = 'op_deadbeef'), \
                 (SELECT COUNT(*)::bigint FROM webhook_effects \
                  WHERE provider_event_id = $1 AND operation_id = 'op_deadbeef'), \
                 (SELECT COUNT(*)::bigint FROM ledger_entries \
                  WHERE provider_event_id = $1 AND operation_id = 'op_deadbeef'), \
                 (SELECT COUNT(*)::bigint FROM ledger_postings AS postings \
                  JOIN ledger_entries AS entries USING (entry_id) \
                  WHERE entries.provider_event_id = $1 \
                    AND entries.operation_id = 'op_deadbeef')",
            &[&event_id],
        )
        .await
        .expect("the collision rollback is observable");
    assert_eq!(state.get::<_, i64>(0), 0);
    assert_eq!(state.get::<_, i64>(1), 0);
    assert_eq!(state.get::<_, i64>(2), 0);
    assert_eq!(state.get::<_, i64>(3), 0);
    assert_eq!(state.get::<_, i64>(4), 0);
    drop(admin);
    connection.await.unwrap().unwrap();

    cleanup_reference_databases().await;
    recreate_reference_app_in_modes(FAULTY_RETRY_KEY_MODE, REPAIRED_WEBHOOK_EFFECT_MODE);
    restore.disarm();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn faulty_ledger_mode_records_one_sided_duplicate_after_a_balanced_effect() {
    let _guard = E2E_LOCK.lock().await;
    let mut restore = ReferenceAppModeRestore::armed();
    recreate_reference_app_in_all_modes(
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        FAULTY_LEDGER_BALANCE_MODE,
    );
    prepare_reference_baseline().await;

    let mut fixture = PaymentIntentFixture::new(Seed::new(6_171));
    let payment_intent = fixture
        .create(
            IdempotencyKey::new("ledger-duplicate-contract").unwrap(),
            CreatePaymentIntent::new(2_500, "usd")
                .unwrap()
                .with_operation_id(OperationId::new("op_deadbeef").unwrap()),
            FaultOutcome::Normal,
        )
        .expect("the ledger contract creates one provider object");
    fixture
        .confirm(payment_intent.id())
        .expect("the provider object reaches succeeded");
    let event = fixture
        .events()
        .first()
        .expect("confirmation creates one event");
    let attempt = event
        .webhook_attempt(current_unix_timestamp(), b"whsec_test_secret")
        .expect("the immutable event is signed for delivery");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let send = || {
        client
            .post("http://127.0.0.1:18080/webhooks/stripe")
            .header("Stripe-Signature", attempt.signature_header())
            .header("Connection", "close")
            .body(attempt.raw_body().to_vec())
            .send()
    };
    let (first, second) = tokio::join!(send(), send());
    for (delivery, response) in [first, second].into_iter().enumerate() {
        let response = response.expect("the signed webhook receives an application response");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::OK,
            "delivery {} must be accepted",
            delivery + 1
        );
    }

    let case_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (admin, connection) = tokio_postgres::connect(&case_url, NoTls)
        .await
        .expect("the isolated admin connects to the generated case");
    let connection = tokio::spawn(connection);
    let state = admin
        .query_one(
            "SELECT \
                 (SELECT COUNT(*)::bigint FROM webhook_deliveries), \
                 (SELECT COUNT(*)::bigint FROM webhook_effects), \
                 (SELECT COUNT(*)::bigint FROM ledger_entries), \
                 (SELECT COUNT(*)::bigint FROM ledger_postings), \
                 (SELECT COALESCE(SUM(amount_minor), 0)::bigint \
                    FROM ledger_postings WHERE entry_side = 'debit'), \
                 (SELECT COALESCE(SUM(amount_minor), 0)::bigint \
                    FROM ledger_postings WHERE entry_side = 'credit')",
            &[],
        )
        .await
        .expect("the faulty ledger projection is inspectable");
    assert_eq!(state.get::<_, i64>(0), 2);
    assert_eq!(state.get::<_, i64>(1), 1);
    assert_eq!(state.get::<_, i64>(2), 2);
    assert_eq!(state.get::<_, i64>(3), 3);
    assert_eq!(state.get::<_, i64>(4), 5_000);
    assert_eq!(state.get::<_, i64>(5), 2_500);
    drop(admin);
    connection.await.unwrap().unwrap();

    cleanup_reference_databases().await;
    recreate_reference_app_in_all_modes(
        FAULTY_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    restore.disarm();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn rejected_credit_posting_rolls_back_the_entire_webhook_transaction() {
    let _guard = E2E_LOCK.lock().await;
    let mut restore = ReferenceAppModeRestore::armed();
    recreate_reference_app_in_all_modes(
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    prepare_reference_baseline().await;

    let case_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (admin, connection) = tokio_postgres::connect(&case_url, NoTls)
        .await
        .expect("the isolated admin connects to the generated case");
    let connection = tokio::spawn(connection);
    admin
        .batch_execute(
            "CREATE FUNCTION tiv_reject_credit_posting() RETURNS trigger \
                 LANGUAGE plpgsql AS $$ \
                 BEGIN \
                   IF NEW.entry_side = 'credit' THEN \
                     RAISE EXCEPTION 'injected credit-posting rejection'; \
                   END IF; \
                   RETURN NEW; \
                 END \
                 $$; \
             CREATE TRIGGER tiv_reject_credit_posting \
                 BEFORE INSERT ON ledger_postings \
                 FOR EACH ROW EXECUTE FUNCTION tiv_reject_credit_posting()",
        )
        .await
        .expect("the isolated case installs the second-leg rejection");

    let mut fixture = PaymentIntentFixture::new(Seed::new(6_172));
    let payment_intent = fixture
        .create(
            IdempotencyKey::new("ledger-rollback-contract").unwrap(),
            CreatePaymentIntent::new(2_500, "usd")
                .unwrap()
                .with_operation_id(OperationId::new("op_deadbeef").unwrap()),
            FaultOutcome::Normal,
        )
        .expect("the rollback contract creates one provider object");
    fixture
        .confirm(payment_intent.id())
        .expect("the provider object reaches succeeded");
    let attempt = fixture.events()[0]
        .webhook_attempt(current_unix_timestamp(), b"whsec_test_secret")
        .expect("the immutable event is signed for delivery");
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post("http://127.0.0.1:18080/webhooks/stripe")
        .header("Stripe-Signature", attempt.signature_header())
        .header("Connection", "close")
        .body(attempt.raw_body().to_vec())
        .send()
        .await
        .expect("the rejected ledger write receives an application response");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(response.text().await.unwrap(), "database failure");

    let state = admin
        .query_one(
            "SELECT \
                 (SELECT COUNT(*)::bigint FROM processed_webhook_events), \
                 (SELECT COUNT(*)::bigint FROM webhook_deliveries), \
                 (SELECT COUNT(*)::bigint FROM payments), \
                 (SELECT COUNT(*)::bigint FROM webhook_effects), \
                 (SELECT COUNT(*)::bigint FROM ledger_entries), \
                 (SELECT COUNT(*)::bigint FROM ledger_postings)",
            &[],
        )
        .await
        .expect("the rollback state is observable");
    for column in 0..6 {
        assert_eq!(state.get::<_, i64>(column), 0);
    }
    drop(admin);
    connection.await.unwrap().unwrap();

    cleanup_reference_databases().await;
    recreate_reference_app_in_all_modes(
        FAULTY_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    restore.disarm();
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
#[allow(clippy::too_many_lines)]
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
    assert_complete_report_bundle(
        source_path,
        "configured_campaign",
        "counterexample",
        "tiv replay configured",
    );

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
    assert_complete_report_bundle(
        replay_path,
        "configured_replay",
        "counterexample",
        "tiv replay configured",
    );
    let replay_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(replay_path.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(replay_manifest["schema_version"], 2);
    assert_eq!(replay_manifest["artifact"]["kind"], "configured_replay");
    assert_eq!(replay_manifest["artifact"]["result"], "counterexample");
    assert_eq!(replay_manifest["artifact"]["exit_code"], 10);
    assert_eq!(
        replay_manifest["provenance"]["source_artifacts"][0]["run_id"],
        source_receipt["run_id"]
    );
    assert_eq!(
        replay_manifest["provenance"]["source_artifacts"][0]["manifest_digest"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
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
#[allow(clippy::too_many_lines)]
async fn configured_shrink_evaluates_one_replayed_candidate_and_finalizes_evidence() {
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
    assert_complete_report_bundle(
        source_path,
        "configured_campaign",
        "counterexample",
        "tiv replay configured",
    );

    let replay_output = configured_replay_command(source_path)
        .output()
        .expect("the configured replay command executes");
    assert_eq!(
        replay_output.status.code(),
        Some(10),
        "configured replay failed before shrink: {}",
        String::from_utf8_lossy(&replay_output.stderr)
    );
    let replay_receipt: serde_json::Value = serde_json::from_slice(&replay_output.stdout).unwrap();
    assert_eq!(replay_receipt["classification"], "stable");
    assert_eq!(replay_receipt["matching_failure_count"], 3);
    let replay_path = Path::new(replay_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(replay_path).unwrap();
    assert_complete_report_bundle(
        replay_path,
        "configured_replay",
        "counterexample",
        "tiv replay configured",
    );

    let shrink_output = configured_shrink_command(replay_path)
        .output()
        .expect("the configured shrink command executes");
    assert!(
        matches!(shrink_output.status.code(), Some(10 | 11)),
        "configured shrink failed: {}",
        String::from_utf8_lossy(&shrink_output.stderr)
    );
    let shrink_receipt: serde_json::Value = serde_json::from_slice(&shrink_output.stdout).unwrap();
    assert_eq!(shrink_receipt["status"], "configured_shrink_complete");
    assert_eq!(
        shrink_receipt["source_replay_id"],
        replay_receipt["replay_id"]
    );
    assert_eq!(shrink_receipt["case_id"], "case_0001");
    assert_eq!(shrink_receipt["evaluated_candidates"], 1);
    assert!(shrink_receipt["accepted_candidates"].as_u64().unwrap() <= 1);
    assert!(
        shrink_receipt["best_action_count"].as_u64().unwrap()
            <= shrink_receipt["original_action_count"].as_u64().unwrap()
    );
    assert_eq!(
        shrink_output.status.code(),
        Some(if shrink_receipt["completion"] == "budget_exhausted" {
            11
        } else {
            10
        })
    );

    let shrink_path = Path::new(shrink_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(shrink_path).unwrap();
    assert_complete_report_bundle(
        shrink_path,
        "configured_shrink",
        if shrink_receipt["completion"] == "budget_exhausted" {
            "budget_exhausted"
        } else {
            "counterexample"
        },
        "tiv replay minimized",
    );
    let shrink_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(shrink_path.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(shrink_manifest["schema_version"], 2);
    assert_eq!(shrink_manifest["artifact"]["kind"], "configured_shrink");
    assert_eq!(
        shrink_manifest["artifact"]["result"],
        if shrink_receipt["completion"] == "budget_exhausted" {
            "budget_exhausted"
        } else {
            "counterexample"
        }
    );
    assert_eq!(
        shrink_manifest["artifact"]["exit_code"],
        shrink_output.status.code().unwrap()
    );
    assert_eq!(
        shrink_manifest["provenance"]["source_artifacts"][0]["run_id"],
        replay_receipt["replay_id"]
    );
    assert_eq!(
        fs::read(shrink_path.join("trace.original.json")).unwrap(),
        fs::read(replay_path.join("trace.original.json")).unwrap()
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(shrink_path.join("summary.json")).unwrap()).unwrap();
    assert_eq!(summary["candidate_limit"], 1);
    assert_eq!(summary["max_time_milliseconds"], 600_000);
    assert_eq!(summary["original_attempt_count"], 3);
    assert_eq!(summary["original_matching_failure_count"], 3);
    assert_eq!(summary["evaluated_candidates"], 1);
    assert!(matches!(
        summary["completion"].as_str(),
        Some("complete" | "budget_exhausted")
    ));
    assert_eq!(
        shrink_path.join("trace.minimized.json").is_file(),
        summary["minimized_trace_written"].as_bool().unwrap()
    );
    assert!(
        summary["original_attempts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|attempt| {
                attempt["trace_matches_authority"] == true
                    && attempt["verdict"] == "expected_violation"
                    && attempt["invariants"]
                        .as_array()
                        .is_some_and(|items| items.len() == 5)
                    && attempt["before_database_oid"] != attempt["after_database_oid"]
            })
    );
    let candidates = summary["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(candidate["accepted"], true);
    assert_eq!(shrink_receipt["accepted_candidates"], 1);
    let candidate_id = candidate["candidate_id"].as_str().unwrap();
    assert_eq!(candidate["attempts"].as_array().unwrap().len(), 3);
    assert!(
        candidate["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|attempt| {
                attempt["trace_matches_authority"] == true
                    && attempt["invariants"]
                        .as_array()
                        .is_some_and(|items| items.len() == 5)
                    && attempt["before_database_oid"] != attempt["after_database_oid"]
            })
    );
    assert!(
        shrink_path
            .join(format!("candidates/{candidate_id}/candidate.json"))
            .is_file()
    );
    assert!(
        shrink_path
            .join(format!("candidates/{candidate_id}/evaluation.json"))
            .is_file()
    );
    for attempt in 1..=3 {
        assert!(
            shrink_path
                .join(format!(
                    "candidates/{candidate_id}/attempts/attempt_{attempt:04}/trace.json"
                ))
                .is_file()
        );
        assert!(
            shrink_path
                .join(format!(
                    "candidates/{candidate_id}/attempts/attempt_{attempt:04}/observations.ndjson"
                ))
                .is_file()
        );
    }

    let shrink_snapshot = read_artifact_snapshot(shrink_path);
    let minimized_replay_output = configured_minimized_replay_command(shrink_path)
        .output()
        .expect("the authority-bound minimized replay command executes");
    assert_eq!(
        minimized_replay_output.status.code(),
        Some(10),
        "configured minimized replay failed: {}",
        String::from_utf8_lossy(&minimized_replay_output.stderr)
    );
    let minimized_replay_receipt: serde_json::Value =
        serde_json::from_slice(&minimized_replay_output.stdout).unwrap();
    assert_eq!(
        minimized_replay_receipt["status"],
        "configured_minimized_replay_complete"
    );
    assert_eq!(
        minimized_replay_receipt["source_shrink_id"],
        shrink_receipt["shrink_id"]
    );
    assert_eq!(
        minimized_replay_receipt["source_replay_id"],
        replay_receipt["replay_id"]
    );
    assert_eq!(minimized_replay_receipt["case_id"], "case_0001");
    assert_eq!(minimized_replay_receipt["attempt_count"], 3);
    assert!(matches!(
        minimized_replay_receipt["classification"].as_str(),
        Some("stable" | "reproducible")
    ));
    assert!(
        minimized_replay_receipt["matching_failure_count"]
            .as_u64()
            .is_some_and(|count| count >= 2)
    );
    let minimized_replay_path =
        Path::new(minimized_replay_receipt["artifact_path"].as_str().unwrap());
    assert_eq!(read_artifact_snapshot(shrink_path), shrink_snapshot);
    verify_complete_run_artifact(minimized_replay_path).unwrap();
    assert_complete_report_bundle(
        minimized_replay_path,
        "configured_minimized_replay",
        "counterexample",
        "tiv replay minimized",
    );
    let minimized_replay_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(minimized_replay_path.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(
        minimized_replay_manifest["artifact"]["kind"],
        "configured_minimized_replay"
    );
    assert_eq!(
        minimized_replay_manifest["artifact"]["result"],
        "counterexample"
    );
    assert_eq!(
        minimized_replay_manifest["provenance"]["source_artifacts"][0]["run_id"],
        shrink_receipt["shrink_id"]
    );
    assert_eq!(
        fs::read(minimized_replay_path.join("trace.minimized.json")).unwrap(),
        fs::read(shrink_path.join("trace.minimized.json")).unwrap()
    );
    let minimized_replay_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(minimized_replay_path.join("summary.json")).unwrap())
            .unwrap();
    let minimized_attempts = minimized_replay_summary["attempts"].as_array().unwrap();
    assert_eq!(minimized_attempts.len(), 3);
    assert!(minimized_attempts.iter().all(|attempt| {
        attempt["trace_matches_minimized_authority"] == true
            && attempt["invariants"]
                .as_array()
                .is_some_and(|invariants| invariants.len() == 5)
            && attempt["before_database_oid"] != attempt["after_database_oid"]
    }));
    assert_eq!(
        minimized_attempts
            .iter()
            .map(|attempt| attempt["after_database_oid"].as_u64().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        3
    );
    assert_eq!(
        minimized_attempts
            .iter()
            .map(|attempt| attempt["after_marker_uuid"].as_str().unwrap())
            .collect::<BTreeSet<_>>()
            .len(),
        3
    );
    for attempt in 1..=3 {
        assert!(
            minimized_replay_path
                .join(format!("attempts/attempt_{attempt:04}/trace.json"))
                .is_file()
        );
        assert!(
            minimized_replay_path
                .join(format!("attempts/attempt_{attempt:04}/observations.ndjson"))
                .is_file()
        );
    }
    let artifact_bytes = read_artifact_tree(shrink_path);
    let minimized_replay_bytes = read_artifact_tree(minimized_replay_path);
    for secret in [
        "tiv-local-only-password",
        "tiv-app-local-only-password",
        "whsec_test_secret",
        "run-scoped-control-token",
    ] {
        assert!(!artifact_bytes.contains(secret));
        assert!(!minimized_replay_bytes.contains(secret));
    }
    assert_reference_app_healthy();
    verify_complete_run_artifact(source_path).unwrap();
    verify_complete_run_artifact(replay_path).unwrap();

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
    assert_partial_report_bundle(
        &final_path,
        "configured_replay",
        "configuration_failure",
        "compatibility_mismatch",
        2,
        false,
    );
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
    assert_partial_report_bundle(
        &final_path,
        "configured_replay",
        "interrupted",
        "interrupted",
        130,
        false,
    );
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
    assert_partial_report_bundle(
        &final_path,
        "configured_campaign",
        "interrupted",
        "interrupted",
        130,
        false,
    );
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

fn run_configured_command_in_retry_mode(seed: u64, retry_key_mode: &str) -> std::process::Output {
    run_configured_command_in_modes(seed, retry_key_mode, REPAIRED_WEBHOOK_EFFECT_MODE)
}

fn run_configured_command_in_modes(
    seed: u64,
    retry_key_mode: &str,
    webhook_effect_mode: &str,
) -> std::process::Output {
    run_configured_command_in_all_modes(
        seed,
        retry_key_mode,
        FAULTY_CALLER_RETRY_MODE,
        webhook_effect_mode,
        REPAIRED_LEDGER_BALANCE_MODE,
    )
}

fn run_configured_command_in_all_modes(
    seed: u64,
    retry_key_mode: &str,
    caller_retry_mode: &str,
    webhook_effect_mode: &str,
    ledger_balance_mode: &str,
) -> std::process::Output {
    require_reference_app_all_modes(
        retry_key_mode,
        caller_retry_mode,
        webhook_effect_mode,
        ledger_balance_mode,
    );
    configured_command(seed, 1)
        .env(RETRY_KEY_MODE_ENV, retry_key_mode)
        .env(CALLER_RETRY_MODE_ENV, caller_retry_mode)
        .env(WEBHOOK_EFFECT_MODE_ENV, webhook_effect_mode)
        .env(LEDGER_BALANCE_MODE_ENV, ledger_balance_mode)
        .output()
        .expect("the mode-bound configured campaign command executes")
}

fn run_configured_command_in_reconciliation_mode(
    seed: u64,
    reconciliation_mode: &str,
) -> std::process::Output {
    require_reference_app_reconciliation_mode(reconciliation_mode);
    configured_command(seed, 1)
        .env(RETRY_KEY_MODE_ENV, REPAIRED_RETRY_KEY_MODE)
        .env(CALLER_RETRY_MODE_ENV, FAULTY_CALLER_RETRY_MODE)
        .env(RECONCILIATION_MODE_ENV, reconciliation_mode)
        .env(WEBHOOK_EFFECT_MODE_ENV, REPAIRED_WEBHOOK_EFFECT_MODE)
        .env(LEDGER_BALANCE_MODE_ENV, REPAIRED_LEDGER_BALANCE_MODE)
        .output()
        .expect("the reconciliation-mode configured campaign command executes")
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
        .env(RECONCILIATION_MODE_ENV, FAULTY_RECONCILIATION_MODE)
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

fn configured_replay_command_in_modes(
    artifact: &Path,
    retry_key_mode: &str,
    webhook_effect_mode: &str,
) -> Command {
    configured_replay_command_in_all_modes(
        artifact,
        retry_key_mode,
        FAULTY_CALLER_RETRY_MODE,
        webhook_effect_mode,
        REPAIRED_LEDGER_BALANCE_MODE,
    )
}

fn configured_replay_command_in_all_modes(
    artifact: &Path,
    retry_key_mode: &str,
    caller_retry_mode: &str,
    webhook_effect_mode: &str,
    ledger_balance_mode: &str,
) -> Command {
    require_reference_app_all_modes(
        retry_key_mode,
        caller_retry_mode,
        webhook_effect_mode,
        ledger_balance_mode,
    );
    let mut command = configured_replay_command(artifact);
    command
        .env(RETRY_KEY_MODE_ENV, retry_key_mode)
        .env(CALLER_RETRY_MODE_ENV, caller_retry_mode)
        .env(WEBHOOK_EFFECT_MODE_ENV, webhook_effect_mode)
        .env(LEDGER_BALANCE_MODE_ENV, ledger_balance_mode);
    command
}

fn configured_replay_command_in_reconciliation_mode(
    artifact: &Path,
    reconciliation_mode: &str,
) -> Command {
    require_reference_app_reconciliation_mode(reconciliation_mode);
    let mut command = configured_replay_command(artifact);
    command
        .env(RETRY_KEY_MODE_ENV, REPAIRED_RETRY_KEY_MODE)
        .env(CALLER_RETRY_MODE_ENV, FAULTY_CALLER_RETRY_MODE)
        .env(RECONCILIATION_MODE_ENV, reconciliation_mode)
        .env(WEBHOOK_EFFECT_MODE_ENV, REPAIRED_WEBHOOK_EFFECT_MODE)
        .env(LEDGER_BALANCE_MODE_ENV, REPAIRED_LEDGER_BALANCE_MODE);
    command
}

fn configured_shrink_command(artifact: &Path) -> Command {
    configured_shrink_command_with_limit(artifact, 1)
}

fn configured_shrink_command_with_limit(artifact: &Path, max_candidates: u32) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tiv"));
    command
        .args(["shrink", "configured", "--artifact"])
        .arg(artifact)
        .arg("--config")
        .arg(CONFIG)
        .arg("--max-candidates")
        .arg(max_candidates.to_string())
        .args(["--max-time", "10m"])
        .env("TIV_POSTGRES_ADMIN_URL", ADMIN_URL)
        .env("DATABASE_URL", CASE_URL)
        .env("TIV_STRIPE_WEBHOOK_SECRET", "whsec_test_secret")
        .env("TIV_FIXTURE_CONTROL_TOKEN", "run-scoped-control-token")
        .env(RECONCILIATION_MODE_ENV, FAULTY_RECONCILIATION_MODE)
        .env("DOCKER_HOST", "tcp://127.0.0.1:9")
        .env("DOCKER_CONTEXT", "intentionally-remote")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

fn configured_shrink_command_in_modes(
    artifact: &Path,
    retry_key_mode: &str,
    webhook_effect_mode: &str,
) -> Command {
    configured_shrink_command_in_all_modes(
        artifact,
        retry_key_mode,
        FAULTY_CALLER_RETRY_MODE,
        webhook_effect_mode,
        REPAIRED_LEDGER_BALANCE_MODE,
    )
}

fn configured_shrink_command_in_all_modes(
    artifact: &Path,
    retry_key_mode: &str,
    caller_retry_mode: &str,
    webhook_effect_mode: &str,
    ledger_balance_mode: &str,
) -> Command {
    require_reference_app_all_modes(
        retry_key_mode,
        caller_retry_mode,
        webhook_effect_mode,
        ledger_balance_mode,
    );
    let mut command = configured_shrink_command_with_limit(artifact, 3);
    command
        .env(RETRY_KEY_MODE_ENV, retry_key_mode)
        .env(CALLER_RETRY_MODE_ENV, caller_retry_mode)
        .env(WEBHOOK_EFFECT_MODE_ENV, webhook_effect_mode)
        .env(LEDGER_BALANCE_MODE_ENV, ledger_balance_mode);
    command
}

fn configured_shrink_command_in_reconciliation_mode(
    artifact: &Path,
    reconciliation_mode: &str,
) -> Command {
    require_reference_app_reconciliation_mode(reconciliation_mode);
    let mut command = configured_shrink_command_with_limit(artifact, 3);
    command
        .env(RETRY_KEY_MODE_ENV, REPAIRED_RETRY_KEY_MODE)
        .env(CALLER_RETRY_MODE_ENV, FAULTY_CALLER_RETRY_MODE)
        .env(RECONCILIATION_MODE_ENV, reconciliation_mode)
        .env(WEBHOOK_EFFECT_MODE_ENV, REPAIRED_WEBHOOK_EFFECT_MODE)
        .env(LEDGER_BALANCE_MODE_ENV, REPAIRED_LEDGER_BALANCE_MODE);
    command
}

fn configured_minimized_replay_command(artifact: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tiv"));
    command
        .args(["replay", "minimized", "--artifact"])
        .arg(artifact)
        .arg("--config")
        .arg(CONFIG)
        .env("TIV_POSTGRES_ADMIN_URL", ADMIN_URL)
        .env("DATABASE_URL", CASE_URL)
        .env("TIV_STRIPE_WEBHOOK_SECRET", "whsec_test_secret")
        .env("TIV_FIXTURE_CONTROL_TOKEN", "run-scoped-control-token")
        .env(RECONCILIATION_MODE_ENV, FAULTY_RECONCILIATION_MODE)
        .env("DOCKER_HOST", "tcp://127.0.0.1:9")
        .env("DOCKER_CONTEXT", "intentionally-remote")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

fn configured_minimized_replay_command_in_modes(
    artifact: &Path,
    retry_key_mode: &str,
    webhook_effect_mode: &str,
) -> Command {
    configured_minimized_replay_command_in_all_modes(
        artifact,
        retry_key_mode,
        FAULTY_CALLER_RETRY_MODE,
        webhook_effect_mode,
        REPAIRED_LEDGER_BALANCE_MODE,
    )
}

fn configured_minimized_replay_command_in_all_modes(
    artifact: &Path,
    retry_key_mode: &str,
    caller_retry_mode: &str,
    webhook_effect_mode: &str,
    ledger_balance_mode: &str,
) -> Command {
    require_reference_app_all_modes(
        retry_key_mode,
        caller_retry_mode,
        webhook_effect_mode,
        ledger_balance_mode,
    );
    let mut command = configured_minimized_replay_command(artifact);
    command
        .env(RETRY_KEY_MODE_ENV, retry_key_mode)
        .env(CALLER_RETRY_MODE_ENV, caller_retry_mode)
        .env(WEBHOOK_EFFECT_MODE_ENV, webhook_effect_mode)
        .env(LEDGER_BALANCE_MODE_ENV, ledger_balance_mode);
    command
}

fn configured_minimized_replay_command_in_reconciliation_mode(
    artifact: &Path,
    reconciliation_mode: &str,
) -> Command {
    require_reference_app_reconciliation_mode(reconciliation_mode);
    let mut command = configured_minimized_replay_command(artifact);
    command
        .env(RETRY_KEY_MODE_ENV, REPAIRED_RETRY_KEY_MODE)
        .env(CALLER_RETRY_MODE_ENV, FAULTY_CALLER_RETRY_MODE)
        .env(RECONCILIATION_MODE_ENV, reconciliation_mode)
        .env(WEBHOOK_EFFECT_MODE_ENV, REPAIRED_WEBHOOK_EFFECT_MODE)
        .env(LEDGER_BALANCE_MODE_ENV, REPAIRED_LEDGER_BALANCE_MODE);
    command
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
        .env(RECONCILIATION_MODE_ENV, FAULTY_RECONCILIATION_MODE)
        .env("DOCKER_HOST", "tcp://127.0.0.1:9")
        .env("DOCKER_CONTEXT", "intentionally-remote")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

fn assert_row_three_trace(artifact: &Path) {
    let campaign: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("campaign-plan.json")).unwrap())
            .expect("the campaign plan is JSON");
    assert_eq!(campaign["spec"]["campaign_seed"], 69);
    let trace: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("cases/case_0001/trace.json")).unwrap())
            .expect("the configured case trace is JSON");
    let business_scripts = trace["planned_case"]["actions"]
        .as_array()
        .expect("planned actions are recorded")
        .iter()
        .filter(|action| {
            matches!(
                action["kind"]["kind"].as_str(),
                Some("drive_checkout" | "retry_business_request")
            )
        })
        .map(|action| action["kind"]["provider_script"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        business_scripts,
        vec![serde_json::json!({
            "first": "commit_then_close",
            "retry": "normal"
        })]
    );
}

fn assert_row_four_trace(artifact: &Path) {
    let campaign: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("campaign-plan.json")).unwrap())
            .expect("the campaign plan is JSON");
    assert_eq!(campaign["spec"]["campaign_seed"], 422);
    let trace: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("cases/case_0001/trace.json")).unwrap())
            .expect("the configured case trace is JSON");
    let actions = trace["planned_case"]["actions"]
        .as_array()
        .expect("planned actions are recorded");
    assert_eq!(actions[0]["kind"]["kind"], "drive_checkout");
    assert_eq!(actions[0]["kind"]["provider_script"]["first"], "normal");
    assert_eq!(actions[1]["kind"]["kind"], "kill_application");
    assert_eq!(actions[1]["kind"]["cut_point"], "client_response_observed");
    assert_eq!(actions[2]["kind"]["kind"], "restart_and_await_health");
    assert_eq!(actions[3]["kind"]["kind"], "retry_business_request");
    assert_eq!(actions[3]["kind"]["provider_script"]["first"], "normal");
    assert_eq!(
        actions
            .iter()
            .filter(|action| action["kind"]["kind"] == "kill_application")
            .count(),
        1
    );
}

fn assert_row_five_trace(artifact: &Path) {
    let campaign: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("campaign-plan.json")).unwrap())
            .expect("the campaign plan is JSON");
    assert_eq!(campaign["spec"]["campaign_seed"], 359);
    let trace: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("cases/case_0001/trace.json")).unwrap())
            .expect("the configured case trace is JSON");
    let action_kinds = trace["planned_case"]["actions"]
        .as_array()
        .expect("planned actions are recorded")
        .iter()
        .filter_map(|action| action["kind"]["kind"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        action_kinds
            .iter()
            .filter(|kind| **kind == "generate_provider_event")
            .count(),
        1
    );
    assert_eq!(
        action_kinds
            .iter()
            .filter(|kind| **kind == "drop_webhook")
            .count(),
        1
    );
    assert!(action_kinds.iter().all(|kind| !matches!(
        *kind,
        "deliver_webhook" | "duplicate_webhook" | "kill_application"
    )));
    let generated = action_kinds
        .iter()
        .position(|kind| *kind == "generate_provider_event")
        .expect("one provider event is generated");
    let dropped = action_kinds
        .iter()
        .position(|kind| *kind == "drop_webhook")
        .expect("the success event is dropped");
    let quiescence = action_kinds
        .iter()
        .position(|kind| *kind == "wait_for_quiescence")
        .expect("the declared horizon reaches quiescence");
    assert!(generated < dropped && dropped < quiescence);
}

fn assert_row_five_fault_horizon(artifact: &Path) {
    let trace: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("cases/case_0001/trace.json")).unwrap())
            .expect("the configured case trace is JSON");
    let wait_action_id = trace["planned_case"]["actions"]
        .as_array()
        .and_then(|actions| {
            actions
                .iter()
                .find(|action| action["kind"]["kind"] == "wait_for_quiescence")
        })
        .map(|action| action["id"].clone())
        .expect("the row-five trace has a quiescence action");
    let journal = fs::read_to_string(artifact.join("cases/case_0001/observations.ndjson"))
        .expect("the row-five observation journal is readable");
    let wait_outcome_micros = journal
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|record| {
            record["action_id"] == wait_action_id && record["observation_kind"] == "action_outcome"
        })
        .and_then(|record| record["monotonic_elapsed_micros"].as_u64())
        .expect("the quiescence outcome is durably journaled");
    assert!(
        wait_outcome_micros >= 5_000_000,
        "the dropped success must not fail before the five-second horizon"
    );
}

fn assert_row_one_trace(artifact: &Path) {
    let campaign: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("campaign-plan.json")).unwrap())
            .expect("the campaign plan is JSON");
    assert_eq!(campaign["spec"]["campaign_seed"], 1_792);
    let trace: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("cases/case_0001/trace.json")).unwrap())
            .expect("the configured case trace is JSON");
    let action_kinds = trace["planned_case"]["actions"]
        .as_array()
        .expect("planned actions are recorded")
        .iter()
        .filter_map(|action| action["kind"]["kind"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        action_kinds
            .iter()
            .filter(|kind| **kind == "generate_provider_event")
            .count(),
        1
    );
    assert_eq!(
        action_kinds
            .iter()
            .filter(|kind| **kind == "deliver_webhook")
            .count(),
        1
    );
    assert_eq!(
        action_kinds
            .iter()
            .filter(|kind| **kind == "duplicate_webhook")
            .count(),
        1
    );
    let deliver_index = action_kinds
        .iter()
        .position(|kind| *kind == "deliver_webhook")
        .expect("the original event is delivered");
    let duplicate_index = action_kinds
        .iter()
        .position(|kind| *kind == "duplicate_webhook")
        .expect("the same event is duplicated");
    assert!(deliver_index < duplicate_index);
}

#[allow(clippy::too_many_lines)]
fn prove_duplicate_fault_replay_shrink(
    source_path: &Path,
    modes: (&str, &str, &str, &str),
    invariant_id: &str,
) {
    let replay_output =
        configured_replay_command_in_all_modes(source_path, modes.0, modes.1, modes.2, modes.3)
            .output()
            .expect("the duplicate-fault configured replay executes");
    assert_eq!(
        replay_output.status.code(),
        Some(10),
        "duplicate-fault replay failed: {}",
        String::from_utf8_lossy(&replay_output.stderr)
    );
    let replay_receipt: serde_json::Value =
        serde_json::from_slice(&replay_output.stdout).expect("replay stdout is JSON");
    assert_eq!(replay_receipt["attempt_count"], 3);
    assert_eq!(replay_receipt["matching_failure_count"], 3);
    assert_eq!(replay_receipt["classification"], "stable");
    let replay_path = Path::new(replay_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(replay_path).expect("the replay artifact verifies");
    let replay_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(replay_path.join("summary.json")).unwrap()).unwrap();
    assert!(
        replay_summary["attempts"]
            .as_array()
            .is_some_and(|attempts| {
                attempts.len() == 3
                    && attempts.iter().all(|attempt| {
                        attempt["verdict"] == "expected_violation"
                            && exact_invariant_vector(attempt, invariant_id, true)
                    })
            })
    );
    for attempt in 1..=3 {
        assert_balanced_ledger_witness_artifact(
            &replay_path.join(format!(
                "attempts/attempt_{attempt:04}/invariants/witnesses.json"
            )),
            2_500,
        );
    }

    let shrink_output =
        configured_shrink_command_in_all_modes(replay_path, modes.0, modes.1, modes.2, modes.3)
            .output()
            .expect("the duplicate-fault configured shrink executes");
    assert!(
        matches!(shrink_output.status.code(), Some(10 | 11)),
        "duplicate-fault shrink failed: {}",
        String::from_utf8_lossy(&shrink_output.stderr)
    );
    let shrink_receipt: serde_json::Value =
        serde_json::from_slice(&shrink_output.stdout).expect("shrink stdout is JSON");
    assert_eq!(shrink_receipt["evaluated_candidates"], 3);
    assert_eq!(shrink_receipt["accepted_candidates"], 1);
    assert_eq!(shrink_receipt["original_action_count"], 8);
    assert_eq!(shrink_receipt["best_action_count"], 7);
    let shrink_path = Path::new(shrink_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(shrink_path).expect("the shrink artifact verifies");
    let shrink_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(shrink_path.join("summary.json")).unwrap()).unwrap();
    let candidates = shrink_summary["candidates"]
        .as_array()
        .expect("the bounded candidates are recorded");
    assert_eq!(candidates.len(), 3);
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate["accepted"] == true && candidate["action_count"] == 7)
    );
    assert!(
        candidates
            .iter()
            .filter(|candidate| candidate["accepted"] == false)
            .any(|candidate| {
                let Some(candidate_id) = candidate["candidate_id"].as_str() else {
                    return false;
                };
                let candidate: serde_json::Value = serde_json::from_slice(
                    &fs::read(
                        shrink_path
                            .join("candidates")
                            .join(candidate_id)
                            .join("candidate.json"),
                    )
                    .unwrap(),
                )
                .unwrap();
                candidate["schedule"].as_array().is_some_and(|schedule| {
                    schedule
                        .iter()
                        .all(|action| action["kind"]["kind"] != "duplicate_webhook")
                })
            }),
        "removing the causal duplicate must be evaluated and rejected"
    );
    for attempt in 1..=3 {
        assert_balanced_ledger_witness_artifact(
            &shrink_path.join(format!(
                "original/attempts/attempt_{attempt:04}/invariants/witnesses.json"
            )),
            2_500,
        );
    }
    for candidate in candidates {
        let candidate_id = candidate["candidate_id"].as_str().unwrap();
        for attempt in candidate["attempts"].as_array().unwrap() {
            let attempt_number = attempt["attempt"].as_u64().unwrap();
            let expected_violation = attempt["verdict"] == "expected_violation";
            assert!(exact_invariant_vector(
                attempt,
                invariant_id,
                expected_violation
            ));
            let witness_path = shrink_path.join(format!(
                "candidates/{candidate_id}/attempts/attempt_{attempt_number:04}/invariants/witnesses.json"
            ));
            if expected_violation {
                assert_balanced_ledger_witness_artifact(&witness_path, 2_500);
            } else {
                assert!(!witness_path.exists());
            }
        }
    }
    let minimized_authority: serde_json::Value =
        serde_json::from_slice(&fs::read(shrink_path.join("trace.minimized.json")).unwrap())
            .unwrap();
    let minimized_schedule = minimized_authority["candidate"]["schedule"]
        .as_array()
        .expect("the minimized authority retains its schedule");
    assert_eq!(minimized_schedule.len(), 7);
    assert!(
        minimized_schedule
            .iter()
            .any(|action| action["kind"]["kind"] == "duplicate_webhook"),
        "the minimized authority must retain the causal duplicate"
    );

    let minimized_output = configured_minimized_replay_command_in_all_modes(
        shrink_path,
        modes.0,
        modes.1,
        modes.2,
        modes.3,
    )
    .output()
    .expect("the duplicate-fault minimized replay executes");
    assert_eq!(
        minimized_output.status.code(),
        Some(10),
        "duplicate-fault minimized replay failed: {}",
        String::from_utf8_lossy(&minimized_output.stderr)
    );
    let minimized_receipt: serde_json::Value =
        serde_json::from_slice(&minimized_output.stdout).expect("minimized stdout is JSON");
    assert_eq!(minimized_receipt["attempt_count"], 3);
    assert_eq!(minimized_receipt["matching_failure_count"], 3);
    assert_eq!(minimized_receipt["classification"], "stable");
    let minimized_path = Path::new(minimized_receipt["artifact_path"].as_str().unwrap());
    verify_complete_run_artifact(minimized_path).expect("the minimized artifact verifies");
    let minimized_summary: serde_json::Value =
        serde_json::from_slice(&fs::read(minimized_path.join("summary.json")).unwrap()).unwrap();
    assert!(
        minimized_summary["attempts"]
            .as_array()
            .is_some_and(|attempts| {
                attempts.len() == 3
                    && attempts
                        .iter()
                        .all(|attempt| exact_invariant_vector(attempt, invariant_id, true))
            })
    );
    for attempt in 1..=3 {
        assert_balanced_ledger_witness_artifact(
            &minimized_path.join(format!(
                "attempts/attempt_{attempt:04}/invariants/witnesses.json"
            )),
            2_500,
        );
    }
}

fn recorded_invariant(
    record: &serde_json::Value,
    invariant_id: &str,
    verdict: &str,
    witness_count: u64,
) -> bool {
    record["invariants"].as_array().is_some_and(|invariants| {
        invariants.iter().any(|invariant| {
            let recorded_verdict = invariant["verdict"]
                .as_str()
                .is_some_and(|value| value == verdict)
                || invariant["violated"].as_bool().is_some_and(|violated| {
                    (violated && verdict == "violated") || (!violated && verdict == "held")
                });
            invariant["invariant_id"] == invariant_id
                && recorded_verdict
                && invariant["witness_count"] == witness_count
        })
    })
}

fn exact_invariant_vector(
    record: &serde_json::Value,
    expected_invariant_id: &str,
    expected_violation: bool,
) -> bool {
    record["invariants"].as_array().is_some_and(|invariants| {
        invariants.len() == 5
            && invariants.iter().all(|invariant| {
                let Some(invariant_id) = invariant["invariant_id"].as_str() else {
                    return false;
                };
                if invariant_id == expected_invariant_id && expected_violation {
                    recorded_invariant(record, invariant_id, "violated", 1)
                } else {
                    recorded_invariant(record, invariant_id, "held", 0)
                }
            })
    })
}

async fn assert_reference_webhook_effect_count(expected: i64) {
    let case_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (client, connection) = tokio_postgres::connect(&case_url, NoTls)
        .await
        .expect("the test admin connects to the generated case");
    let connection = tokio::spawn(connection);
    let observed = client
        .query_one("SELECT COUNT(*)::bigint FROM webhook_effects", &[])
        .await
        .expect("the reference effect count is readable")
        .get::<_, i64>(0);
    drop(client);
    connection.await.unwrap().unwrap();
    assert_eq!(observed, expected);
}

async fn assert_reference_payment_relation(expected: (i64, i64)) {
    let case_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (client, connection) = tokio_postgres::connect(&case_url, NoTls)
        .await
        .expect("the test admin connects to the generated case");
    let connection = tokio::spawn(connection);
    let row = client
        .query_one(
            "SELECT COUNT(*)::bigint, \
                    COUNT(DISTINCT stripe_payment_intent_id)::bigint \
             FROM payments",
            &[],
        )
        .await
        .expect("the local payment relation is readable");
    drop(client);
    connection.await.unwrap().unwrap();
    assert_eq!((row.get::<_, i64>(0), row.get::<_, i64>(1)), expected);
}

async fn assert_reference_payment_status(expected: (i64, i64)) {
    let case_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (client, connection) = tokio_postgres::connect(&case_url, NoTls)
        .await
        .expect("the test admin connects to the generated case");
    let connection = tokio::spawn(connection);
    let row = client
        .query_one(
            "SELECT COUNT(*) FILTER (WHERE status = 'pending')::bigint, \
                    COUNT(*) FILTER (WHERE status = 'succeeded')::bigint \
             FROM payments",
            &[],
        )
        .await
        .expect("the local payment statuses are readable");
    drop(client);
    connection.await.unwrap().unwrap();
    assert_eq!((row.get::<_, i64>(0), row.get::<_, i64>(1)), expected);
}

async fn assert_reference_webhook_delivery_count(expected: i64) {
    let case_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (client, connection) = tokio_postgres::connect(&case_url, NoTls)
        .await
        .expect("the test admin connects to the generated case");
    let connection = tokio::spawn(connection);
    let observed = client
        .query_one("SELECT COUNT(*)::bigint FROM webhook_deliveries", &[])
        .await
        .expect("the reference delivery count is readable")
        .get::<_, i64>(0);
    drop(client);
    connection.await.unwrap().unwrap();
    assert_eq!(observed, expected);
}

async fn assert_reference_ledger_state(expected: (i64, i64, i64, i64, i64, i64)) {
    let case_url = ADMIN_URL.replace("/postgres", "/tiv_case_deadbeef");
    let (client, connection) = tokio_postgres::connect(&case_url, NoTls)
        .await
        .expect("the test admin connects to the generated case");
    let connection = tokio::spawn(connection);
    let state = client
        .query_one(
            "WITH balances AS ( \
                 SELECT entries.entry_id, \
                        COALESCE(SUM(postings.amount_minor) \
                            FILTER (WHERE postings.entry_side = 'debit'), 0)::bigint \
                            AS debit_total, \
                        COALESCE(SUM(postings.amount_minor) \
                            FILTER (WHERE postings.entry_side = 'credit'), 0)::bigint \
                            AS credit_total \
                 FROM ledger_entries AS entries \
                 LEFT JOIN ledger_postings AS postings USING (entry_id) \
                 GROUP BY entries.entry_id \
             ) \
             SELECT \
                 (SELECT COUNT(*)::bigint FROM ledger_entries), \
                 (SELECT COUNT(*)::bigint FROM ledger_postings), \
                 (SELECT COALESCE(SUM(amount_minor), 0)::bigint \
                    FROM ledger_postings WHERE entry_side = 'debit'), \
                 (SELECT COALESCE(SUM(amount_minor), 0)::bigint \
                    FROM ledger_postings WHERE entry_side = 'credit'), \
                 (SELECT COUNT(*)::bigint FROM balances \
                    WHERE debit_total <> credit_total), \
                 (SELECT COALESCE(MAX(debit_total - credit_total), 0)::bigint \
                    FROM balances WHERE debit_total <> credit_total)",
            &[],
        )
        .await
        .expect("the ledger state is readable");
    drop(client);
    connection.await.unwrap().unwrap();
    assert_eq!(
        (
            state.get::<_, i64>(0),
            state.get::<_, i64>(1),
            state.get::<_, i64>(2),
            state.get::<_, i64>(3),
            state.get::<_, i64>(4),
            state.get::<_, i64>(5),
        ),
        expected
    );
}

fn assert_balanced_ledger_witness_artifact(path: &Path, expected_imbalance: i64) {
    let bundle: serde_json::Value = serde_json::from_slice(
        &fs::read(path).expect("the balanced-ledger witness artifact is retained"),
    )
    .expect("the balanced-ledger witness artifact is JSON");
    assert_eq!(bundle["schema_version"], 1);
    let witness = bundle["invariants"]
        .as_array()
        .and_then(|invariants| {
            invariants
                .iter()
                .find(|invariant| invariant["invariant_id"] == "balanced-ledger")
        })
        .expect("the bundle retains the balanced-ledger violation");
    assert_eq!(witness["invariant_id"], "balanced-ledger");
    assert_eq!(witness["checkpoint_id"], "checkout-quiescent");
    assert_eq!(witness["witness_count"], 1);
    assert_eq!(witness["projection"], "reference_ledger_allowlist");
    assert_eq!(witness["retained_row_count"], 1);
    assert_eq!(witness["omitted_row_count"], 0);
    assert_eq!(witness["rows_truncated"], false);
    let rows = witness["rows"]
        .as_array()
        .expect("the bounded witness rows are an array");
    assert_eq!(rows.len(), 1);
    let digest = witness["witness_digest"]
        .as_str()
        .expect("a violation witness has a digest");
    assert_eq!(digest.len(), 64);
    assert_eq!(
        digest,
        blake3::hash(&serde_json::to_vec(rows).unwrap())
            .to_hex()
            .as_str()
    );
    let row = &rows[0];
    assert!(
        row["provider_event_id"]
            .as_str()
            .is_some_and(|value| value.starts_with("evt_tiv_"))
    );
    assert_eq!(row["operation_id"], "op_deadbeef");
    assert!(
        row["entry_id"]
            .as_str()
            .is_some_and(|value| Uuid::parse_str(value).is_ok())
    );
    assert_eq!(row["currency"], "usd");
    assert_eq!(row["posting_count"], 1);
    assert_eq!(row["debit_posting_count"], 1);
    assert_eq!(row["credit_posting_count"], 0);
    assert_eq!(row["debit_total_minor"], 2_500);
    assert_eq!(row["credit_total_minor"], 0);
    assert_eq!(row["imbalance_minor"], expected_imbalance);
}

fn current_unix_timestamp() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is after the Unix epoch")
            .as_secs(),
    )
    .expect("the current Unix timestamp fits in i64")
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

struct ReferenceAppModeRestore {
    armed: bool,
}

impl ReferenceAppModeRestore {
    const fn armed() -> Self {
        Self { armed: true }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ReferenceAppModeRestore {
    fn drop(&mut self) {
        if self.armed {
            let _ =
                reference_app_recreate_command(FAULTY_RETRY_KEY_MODE, REPAIRED_WEBHOOK_EFFECT_MODE)
                    .status();
        }
    }
}

fn recreate_reference_app_in_retry_mode(retry_key_mode: &str) {
    recreate_reference_app_in_modes(retry_key_mode, REPAIRED_WEBHOOK_EFFECT_MODE);
}

fn recreate_reference_app_in_modes(retry_key_mode: &str, webhook_effect_mode: &str) {
    recreate_reference_app_in_all_modes(
        retry_key_mode,
        FAULTY_CALLER_RETRY_MODE,
        webhook_effect_mode,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
}

fn recreate_reference_app_in_all_modes(
    retry_key_mode: &str,
    caller_retry_mode: &str,
    webhook_effect_mode: &str,
    ledger_balance_mode: &str,
) {
    let output = reference_app_recreate_command_in_all_modes(
        retry_key_mode,
        caller_retry_mode,
        webhook_effect_mode,
        ledger_balance_mode,
    )
    .output()
    .expect("Docker Compose recreates the reference application");
    assert!(
        output.status.success(),
        "reference app recreation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_reference_app_healthy();

    let expected_retry = format!("{RETRY_KEY_MODE_ENV}={retry_key_mode}");
    let expected_caller = format!("{CALLER_RETRY_MODE_ENV}={caller_retry_mode}");
    let expected_reconciliation = format!("{RECONCILIATION_MODE_ENV}={FAULTY_RECONCILIATION_MODE}");
    let expected_webhook = format!("{WEBHOOK_EFFECT_MODE_ENV}={webhook_effect_mode}");
    let expected_ledger = format!("{LEDGER_BALANCE_MODE_ENV}={ledger_balance_mode}");
    let conflicting_retry = format!(
        "{RETRY_KEY_MODE_ENV}={}",
        if retry_key_mode == FAULTY_RETRY_KEY_MODE {
            REPAIRED_RETRY_KEY_MODE
        } else {
            FAULTY_RETRY_KEY_MODE
        }
    );
    let conflicting_caller = format!(
        "{CALLER_RETRY_MODE_ENV}={}",
        if caller_retry_mode == FAULTY_CALLER_RETRY_MODE {
            REPAIRED_CALLER_RETRY_MODE
        } else {
            FAULTY_CALLER_RETRY_MODE
        }
    );
    let conflicting_reconciliation =
        format!("{RECONCILIATION_MODE_ENV}={REPAIRED_RECONCILIATION_MODE}");
    let conflicting_webhook = format!(
        "{WEBHOOK_EFFECT_MODE_ENV}={}",
        if webhook_effect_mode == FAULTY_WEBHOOK_EFFECT_MODE {
            REPAIRED_WEBHOOK_EFFECT_MODE
        } else {
            FAULTY_WEBHOOK_EFFECT_MODE
        }
    );
    let conflicting_ledger = format!(
        "{LEDGER_BALANCE_MODE_ENV}={}",
        if ledger_balance_mode == FAULTY_LEDGER_BALANCE_MODE {
            REPAIRED_LEDGER_BALANCE_MODE
        } else {
            FAULTY_LEDGER_BALANCE_MODE
        }
    );
    let template = format!(
        "{{{{range .Config.Env}}}}{{{{if eq . \"{expected_retry}\"}}}}retry {{{{end}}}}{{{{if eq . \"{conflicting_retry}\"}}}}retry_conflict {{{{end}}}}{{{{if eq . \"{expected_caller}\"}}}}caller {{{{end}}}}{{{{if eq . \"{conflicting_caller}\"}}}}caller_conflict {{{{end}}}}{{{{if eq . \"{expected_reconciliation}\"}}}}reconciliation {{{{end}}}}{{{{if eq . \"{conflicting_reconciliation}\"}}}}reconciliation_conflict {{{{end}}}}{{{{if eq . \"{expected_webhook}\"}}}}webhook {{{{end}}}}{{{{if eq . \"{conflicting_webhook}\"}}}}webhook_conflict {{{{end}}}}{{{{if eq . \"{expected_ledger}\"}}}}ledger {{{{end}}}}{{{{if eq . \"{conflicting_ledger}\"}}}}ledger_conflict {{{{end}}}}{{{{end}}}}"
    );
    let inspection = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "inspect",
            "--format",
            &template,
            "tiv-reference-app-spike-reference-app-1",
        ])
        .output()
        .expect("Docker inspects the selected non-secret reference-app modes");
    assert!(inspection.status.success());
    let inspection_stdout = String::from_utf8_lossy(&inspection.stdout);
    let mut observed = inspection_stdout.split_whitespace().collect::<Vec<_>>();
    observed.sort_unstable();
    assert_eq!(
        observed,
        ["caller", "ledger", "reconciliation", "retry", "webhook"],
        "all selected reference-app modes must be present"
    );
}

fn recreate_reference_app_in_reconciliation_mode(reconciliation_mode: &str) {
    require_reference_app_reconciliation_mode(reconciliation_mode);
    let mut command = reference_app_recreate_command_in_all_modes(
        REPAIRED_RETRY_KEY_MODE,
        FAULTY_CALLER_RETRY_MODE,
        REPAIRED_WEBHOOK_EFFECT_MODE,
        REPAIRED_LEDGER_BALANCE_MODE,
    );
    let output = command
        .env(RECONCILIATION_MODE_ENV, reconciliation_mode)
        .output()
        .expect("Docker Compose recreates the reconciliation-mode application");
    assert!(
        output.status.success(),
        "reference app recreation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_reference_app_healthy();

    let expected = format!("{RECONCILIATION_MODE_ENV}={reconciliation_mode}");
    let conflict = format!(
        "{RECONCILIATION_MODE_ENV}={}",
        if reconciliation_mode == FAULTY_RECONCILIATION_MODE {
            REPAIRED_RECONCILIATION_MODE
        } else {
            FAULTY_RECONCILIATION_MODE
        }
    );
    let template = format!(
        "{{{{range .Config.Env}}}}{{{{if eq . \"{expected}\"}}}}expected {{{{end}}}}{{{{if eq . \"{conflict}\"}}}}conflict {{{{end}}}}{{{{end}}}}"
    );
    let inspection = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "inspect",
            "--format",
            &template,
            "tiv-reference-app-spike-reference-app-1",
        ])
        .output()
        .expect("Docker inspects the selected reconciliation mode");
    assert!(inspection.status.success());
    assert_eq!(
        String::from_utf8_lossy(&inspection.stdout).trim(),
        "expected"
    );
}

fn reference_app_recreate_command(retry_key_mode: &str, webhook_effect_mode: &str) -> Command {
    reference_app_recreate_command_in_all_modes(
        retry_key_mode,
        FAULTY_CALLER_RETRY_MODE,
        webhook_effect_mode,
        REPAIRED_LEDGER_BALANCE_MODE,
    )
}

fn reference_app_recreate_command_in_all_modes(
    retry_key_mode: &str,
    caller_retry_mode: &str,
    webhook_effect_mode: &str,
    ledger_balance_mode: &str,
) -> Command {
    require_reference_app_all_modes(
        retry_key_mode,
        caller_retry_mode,
        webhook_effect_mode,
        ledger_balance_mode,
    );
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let compose_file = repository_root.join("spike/reference-app.compose.yaml");
    let mut command = Command::new("docker");
    command
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "compose",
            "--project-name",
            "tiv-reference-app-spike",
            "--project-directory",
        ])
        .arg(&repository_root)
        .arg("--file")
        .arg(compose_file)
        .args([
            "up",
            "--detach",
            "--build",
            "--no-deps",
            "--force-recreate",
            "--wait",
            "--wait-timeout",
            "30",
            "reference-app",
        ])
        .env(RETRY_KEY_MODE_ENV, retry_key_mode)
        .env(CALLER_RETRY_MODE_ENV, caller_retry_mode)
        .env(RECONCILIATION_MODE_ENV, FAULTY_RECONCILIATION_MODE)
        .env(WEBHOOK_EFFECT_MODE_ENV, webhook_effect_mode)
        .env(LEDGER_BALANCE_MODE_ENV, ledger_balance_mode)
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH");
    command
}

fn require_reference_app_all_modes(
    retry_key_mode: &str,
    caller_retry_mode: &str,
    webhook_effect_mode: &str,
    ledger_balance_mode: &str,
) {
    assert!(matches!(
        retry_key_mode,
        "faulty_changed_key" | "repaired_same_key"
    ));
    assert!(matches!(
        caller_retry_mode,
        "faulty_per_request" | "repaired_recover_operation"
    ));
    assert!(matches!(
        webhook_effect_mode,
        "faulty_duplicate_effect" | "repaired_deduplicate"
    ));
    assert!(matches!(
        ledger_balance_mode,
        "faulty_one_sided_duplicate" | "repaired_balanced_once"
    ));
    assert!(
        webhook_effect_mode != FAULTY_WEBHOOK_EFFECT_MODE
            || ledger_balance_mode != FAULTY_LEDGER_BALANCE_MODE,
        "effect-duplication and one-sided-ledger faults are mutually exclusive"
    );
}

fn require_reference_app_reconciliation_mode(reconciliation_mode: &str) {
    assert!(matches!(
        reconciliation_mode,
        "faulty_webhook_only" | "repaired_provider_reconcile"
    ));
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
                 status text NOT NULL CHECK (status IN ('pending', 'paid')), \
                 UNIQUE (operation_id, amount_minor, currency) \
             ); \
             CREATE TABLE payments ( \
                 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                 operation_id text NOT NULL REFERENCES orders(operation_id), \
                 stripe_payment_intent_id text NOT NULL, \
                 amount_minor bigint NOT NULL CHECK (amount_minor > 0), \
                 currency text NOT NULL CHECK (currency ~ '^[a-z]{3}$'), \
                 status text NOT NULL CHECK (status IN ('pending', 'succeeded')), \
                 UNIQUE (operation_id, stripe_payment_intent_id) \
             ); \
             CREATE TABLE processed_webhook_events ( \
                 provider_event_id text PRIMARY KEY \
                     CHECK (provider_event_id ~ '^evt_tiv_[A-Za-z0-9_-]+$'), \
                 operation_id text NOT NULL REFERENCES orders(operation_id), \
                 UNIQUE (provider_event_id, operation_id) \
             ); \
             CREATE TABLE webhook_deliveries ( \
                 delivery_id uuid PRIMARY KEY, \
                 provider_event_id text NOT NULL, \
                 operation_id text NOT NULL, \
                 UNIQUE (delivery_id, provider_event_id, operation_id), \
                 CONSTRAINT webhook_deliveries_event_identity_fkey \
                     FOREIGN KEY (provider_event_id, operation_id) \
                     REFERENCES processed_webhook_events \
                         (provider_event_id, operation_id) \
             ); \
             CREATE TABLE ledger_entries ( \
                 entry_id uuid PRIMARY KEY, \
                 delivery_id uuid NOT NULL, \
                 provider_event_id text NOT NULL, \
                 operation_id text NOT NULL, \
                 amount_minor bigint NOT NULL CHECK (amount_minor > 0), \
                 currency text NOT NULL CHECK (currency ~ '^[a-z]{3}$'), \
                 entry_kind text NOT NULL CHECK (entry_kind = 'payment_succeeded'), \
                 UNIQUE (delivery_id), \
                 UNIQUE (entry_id, amount_minor, currency), \
                 CONSTRAINT ledger_entries_delivery_identity_fkey \
                     FOREIGN KEY (delivery_id, provider_event_id, operation_id) \
                     REFERENCES webhook_deliveries \
                         (delivery_id, provider_event_id, operation_id), \
                 CONSTRAINT ledger_entries_order_value_fkey \
                     FOREIGN KEY (operation_id, amount_minor, currency) \
                     REFERENCES orders (operation_id, amount_minor, currency) \
             ); \
             CREATE TABLE ledger_postings ( \
                 posting_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                 entry_id uuid NOT NULL, \
                 account_code text NOT NULL, \
                 entry_side text NOT NULL CHECK (entry_side IN ('debit', 'credit')), \
                 amount_minor bigint NOT NULL CHECK (amount_minor > 0), \
                 currency text NOT NULL CHECK (currency ~ '^[a-z]{3}$'), \
                 UNIQUE (entry_id, account_code), \
                 CONSTRAINT ledger_postings_account_side_check CHECK ( \
                     ( \
                         account_code = 'processor_clearing' \
                         AND entry_side = 'debit' \
                     ) OR ( \
                         account_code = 'order_payment_liability' \
                         AND entry_side = 'credit' \
                     ) \
                 ), \
                 CONSTRAINT ledger_postings_entry_value_fkey \
                     FOREIGN KEY (entry_id, amount_minor, currency) \
                     REFERENCES ledger_entries (entry_id, amount_minor, currency) \
             ); \
             CREATE TABLE webhook_effects ( \
                 id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                 provider_event_id text NOT NULL, \
                 operation_id text NOT NULL, \
                 FOREIGN KEY (provider_event_id, operation_id) \
                     REFERENCES processed_webhook_events \
                         (provider_event_id, operation_id) \
             ); \
             CREATE INDEX payments_operation_id_idx ON payments (operation_id); \
             CREATE INDEX payments_provider_id_idx ON payments (stripe_payment_intent_id); \
             CREATE INDEX webhook_effects_event_idx \
                 ON webhook_effects (provider_event_id); \
             CREATE INDEX ledger_entries_event_idx \
                 ON ledger_entries (provider_event_id); \
             REVOKE ALL ON SCHEMA public FROM PUBLIC; \
             GRANT USAGE ON SCHEMA public TO tiv_app, tiv_invariant; \
             REVOKE ALL ON TABLE tiv_verifier_marker, orders, payments, \
                 processed_webhook_events, webhook_deliveries, webhook_effects, \
                 ledger_entries, ledger_postings \
                 FROM PUBLIC, tiv_app, tiv_invariant; \
             REVOKE ALL ON SEQUENCE orders_id_seq, payments_id_seq, \
                 webhook_effects_id_seq, ledger_postings_posting_id_seq \
                 FROM PUBLIC, tiv_app, tiv_invariant; \
             GRANT SELECT (operation_id, amount_minor, currency) ON TABLE orders TO tiv_app; \
             GRANT INSERT ON TABLE payments TO tiv_app; \
             GRANT SELECT (operation_id, stripe_payment_intent_id), \
                   UPDATE (status) ON TABLE payments TO tiv_app; \
             GRANT INSERT ON TABLE processed_webhook_events, webhook_deliveries, \
                 webhook_effects, ledger_entries, ledger_postings TO tiv_app; \
             GRANT USAGE ON SEQUENCE payments_id_seq, webhook_effects_id_seq, \
                 ledger_postings_posting_id_seq TO tiv_app; \
             GRANT SELECT ON TABLE orders, payments, processed_webhook_events, \
                 webhook_deliveries, webhook_effects, ledger_entries, ledger_postings \
                 TO tiv_invariant; \
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

fn assert_complete_report_bundle(
    artifact: &Path,
    expected_kind: &str,
    expected_result: &str,
    expected_command: &str,
) {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["artifact"]["kind"], expected_kind);
    assert_eq!(manifest["artifact"]["result"], expected_result);
    let checksums = fs::read_to_string(artifact.join("checksums.txt")).unwrap();
    for relative in ["summary.md", "junit.xml", "replay.txt"] {
        assert!(artifact.join(relative).is_file());
        assert!(
            manifest["required_files"][relative]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
        );
        assert!(checksums.contains(&format!("  {relative}\n")));
    }

    let markdown = fs::read_to_string(artifact.join("summary.md")).unwrap();
    assert!(markdown.contains(&format!("- Artifact: `{expected_kind}`")));
    assert!(markdown.contains(&format!("- Result: `{expected_result}`")));
    assert!(markdown.contains("counterexample search—not proof"));

    let replay = fs::read_to_string(artifact.join("replay.txt")).unwrap();
    assert!(replay.contains(expected_command));
    assert!(replay.contains("exact compatibility before mutation"));

    let junit = fs::read_to_string(artifact.join("junit.xml")).unwrap();
    assert!(junit.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(junit.contains(&format!("value=\"{expected_kind}\"")));
    assert!(junit.contains(&format!("value=\"{expected_result}\"")));
    assert!(junit.contains("failures=\"1\""));
}

fn assert_partial_report_bundle(
    artifact: &Path,
    expected_kind: &str,
    expected_result: &str,
    expected_failure_code: &str,
    expected_exit_code: u8,
    expected_skipped: bool,
) {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(artifact.join("manifest.json")).unwrap()).unwrap();
    let checksums = fs::read_to_string(artifact.join("checksums.txt")).unwrap();
    for relative in ["summary.md", "junit.xml", "replay.txt"] {
        assert!(artifact.join(relative).is_file());
        assert!(
            manifest["required_files"][relative]
                .as_str()
                .is_some_and(|digest| digest.len() == 64)
        );
        assert!(checksums.contains(&format!("  {relative}\n")));
    }

    let markdown = fs::read_to_string(artifact.join("summary.md")).unwrap();
    assert!(markdown.contains(&format!("- Artifact: `{expected_kind}`")));
    assert!(markdown.contains(&format!("- Result: `{expected_result}`")));
    assert!(markdown.contains(&format!("Failure code: `{expected_failure_code}`")));
    assert!(markdown.contains(&format!("- Exit code: `{expected_exit_code}`")));
    assert!(markdown.contains("counterexample search—not proof"));

    let replay = fs::read_to_string(artifact.join("replay.txt")).unwrap();
    assert!(replay.contains("No executable counterexample replay command"));

    let junit = fs::read_to_string(artifact.join("junit.xml")).unwrap();
    assert!(junit.contains(&format!("value=\"{expected_kind}\"")));
    assert!(junit.contains(&format!("value=\"{expected_result}\"")));
    assert!(junit.contains(&format!("value=\"{expected_exit_code}\"")));
    if expected_skipped {
        assert!(junit.contains("errors=\"0\""));
        assert!(junit.contains("skipped=\"1\""));
    } else {
        assert!(junit.contains("errors=\"1\""));
        assert!(junit.contains("skipped=\"0\""));
    }
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
    let compatibility_digest = blake3::hash(&fs::read(&compatibility_path).unwrap())
        .to_hex()
        .to_string();
    manifest["required_files"]["compatibility.json"] =
        serde_json::Value::String(compatibility_digest.clone());
    manifest["provenance"]["compatibility"]["digest"] =
        serde_json::Value::String(compatibility_digest);
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

fn read_artifact_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut paths = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                collect(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }

    let mut files = BTreeMap::new();
    collect(root, root, &mut files);
    files
}
