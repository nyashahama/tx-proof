use std::{process::Command, time::SystemTime};

use clap::Parser;
use tiv_cli::{Cli, CliError, execute_async};
use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec, PlannedCase, ProcessCutPoint,
        ProcessFaultSpec, ProviderOutcome, WebhookFaultSpec,
    },
};

#[tokio::test]
async fn sql_probe_case_requires_project_config_before_credentials_or_stack_access() {
    let plan = sql_probe_plan();
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let plan_path = std::env::temp_dir().join(format!(
        "tiv-missing-sql-probe-config-{}-{nonce}.json",
        std::process::id()
    ));
    std::fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let cli = Cli::try_parse_from([
        "tiv",
        "replay",
        "reference-app-case",
        "--plan",
        plan_path.to_str().unwrap(),
        "--journal",
        "unused.jsonl",
        "--reference-app-url",
        "http://127.0.0.1:18080",
        "--fixture-control-url",
        "http://127.0.0.1:12112",
    ])
    .unwrap();

    let error = execute_async(cli)
        .await
        .expect_err("SQL probe execution without project config must fail closed");
    assert!(matches!(error, CliError::MissingSqlProbeConfig));
    std::fs::remove_file(plan_path).unwrap();
}

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

#[test]
#[ignore = "requires a fresh isolated reference-app Compose project"]
fn client_request_forwarded_kill_restarts_and_finishes_the_live_case() {
    let plan_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../tiv-core/tests/golden/planned-case-v3.json"
    );
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let journal_path = std::env::temp_dir().join(format!(
        "tiv-reference-process-case-{}-{nonce}.jsonl",
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
    assert_eq!(value["seed"], 42);
    assert_eq!(value["planned_action_count"], 15);
    assert_eq!(value["executed_action_count"], 15);
    assert_eq!(value["provider_object_count"], 2);
    assert!(
        value["journal_record_count"]
            .as_u64()
            .is_some_and(|count| count >= 30)
    );
    assert_eq!(
        value["invariant_outcomes"]
            .as_array()
            .expect("invariant outcomes are an array")
            .len(),
        5
    );
    assert!(journal_path.exists());
    std::fs::remove_file(journal_path).unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("password"));
}

#[test]
#[ignore = "requires a fresh isolated reference-app Compose project"]
fn client_response_observed_kill_restarts_and_finishes_the_live_case() {
    let plan = (0..512)
        .find_map(|seed| {
            let spec = PlanSpec::new_payment_intent_v1(
                Seed::new(seed),
                ActionBudget::new(40).unwrap(),
                [ProviderOutcome::Normal],
                WebhookFaultSpec::new(0, [], false, false).unwrap(),
                ProcessFaultSpec::new([ProcessCutPoint::ClientResponseObserved], 1).unwrap(),
            )
            .unwrap();
            let plan = CasePlanCompiler::compile(&spec).unwrap();
            (matches!(
                plan.actions()
                    .get(1)
                    .map(tiv_core::plan::PlannedAction::kind),
                Some(PlanActionKind::KillApplication {
                    cut_point: ProcessCutPoint::ClientResponseObserved
                })
            ) && matches!(
                plan.actions()
                    .get(2)
                    .map(tiv_core::plan::PlannedAction::kind),
                Some(PlanActionKind::RestartAndAwaitHealth)
            ) && matches!(
                plan.actions()
                    .get(3)
                    .map(tiv_core::plan::PlannedAction::kind),
                Some(PlanActionKind::RetryBusinessRequest { .. })
            ))
            .then_some(plan)
        })
        .expect("the bounded seed corpus contains a response-observed first checkout");
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let plan_path = std::env::temp_dir().join(format!(
        "tiv-reference-response-plan-{}-{nonce}.json",
        std::process::id()
    ));
    let journal_path = std::env::temp_dir().join(format!(
        "tiv-reference-response-case-{}-{nonce}.jsonl",
        std::process::id()
    ));
    std::fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args([
            "replay",
            "reference-app-case",
            "--plan",
            plan_path.to_str().unwrap(),
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
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["seed"], plan.seed().value());
    assert_eq!(value["planned_action_count"], plan.actions().len());
    assert_eq!(value["executed_action_count"], plan.actions().len());
    assert_eq!(value["provider_object_count"], 2);
    assert!(
        value["journal_record_count"]
            .as_u64()
            .is_some_and(|count| count >= (plan.actions().len() * 2 + 1) as u64)
    );
    assert_provider_uniqueness_only(&value["invariant_outcomes"]);
    assert!(journal_path.exists());
    std::fs::remove_file(journal_path).unwrap();
    std::fs::remove_file(plan_path).unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("password"));
}

fn assert_provider_uniqueness_only(value: &serde_json::Value) {
    assert!(value.as_array().is_some_and(|outcomes| {
        outcomes.len() == 5
            && outcomes.iter().all(|outcome| {
                if outcome["invariant_id"] == "provider-object-unique" {
                    outcome["verdict"] == "violated" && outcome["witness_count"] == 1
                } else {
                    outcome["verdict"] == "held" && outcome["witness_count"] == 0
                }
            })
    }));
}

#[test]
#[ignore = "requires a fresh isolated reference-app Compose project"]
fn webhook_response_observed_kill_restarts_and_finishes_the_live_case() {
    let plan = (0..4_096)
        .find_map(|seed| {
            let spec = PlanSpec::new_payment_intent_v1(
                Seed::new(seed),
                ActionBudget::new(40).unwrap(),
                [ProviderOutcome::Normal],
                WebhookFaultSpec::new(1, [], false, false).unwrap(),
                ProcessFaultSpec::new([ProcessCutPoint::WebhookResponseObserved], 1).unwrap(),
            )
            .unwrap();
            let plan = CasePlanCompiler::compile(&spec).unwrap();
            plan.actions()
                .windows(2)
                .any(|actions| {
                    matches!(actions[0].kind(), PlanActionKind::DeliverWebhook)
                        && matches!(
                            actions[1].kind(),
                            PlanActionKind::KillApplication {
                                cut_point: ProcessCutPoint::WebhookResponseObserved
                            }
                        )
                })
                .then_some(plan)
        })
        .expect("the seed corpus contains a webhook-response delivery cut point");
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let plan_path = std::env::temp_dir().join(format!(
        "tiv-reference-webhook-response-plan-{}-{nonce}.json",
        std::process::id()
    ));
    let journal_path = std::env::temp_dir().join(format!(
        "tiv-reference-webhook-response-case-{}-{nonce}.jsonl",
        std::process::id()
    ));
    std::fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args([
            "replay",
            "reference-app-case",
            "--plan",
            plan_path.to_str().unwrap(),
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
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["seed"], plan.seed().value());
    assert_eq!(value["planned_action_count"], plan.actions().len());
    assert_eq!(value["executed_action_count"], plan.actions().len());
    assert_eq!(value["provider_object_count"], 1);
    assert!(
        value["journal_record_count"]
            .as_u64()
            .is_some_and(|count| count >= (plan.actions().len() * 2 + 2) as u64)
    );
    assert!(
        value["invariant_outcomes"]
            .as_array()
            .is_some_and(|outcomes| outcomes.len() == 5
                && outcomes.iter().all(|outcome| outcome["verdict"] == "held"))
    );
    assert!(journal_path.exists());
    std::fs::remove_file(journal_path).unwrap();
    std::fs::remove_file(plan_path).unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("password"));
}

#[test]
#[ignore = "requires a fresh isolated reference-app Compose project"]
fn sql_probe_kill_restarts_and_finishes_the_live_case() {
    let plan = sql_probe_plan();
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let plan_path = std::env::temp_dir().join(format!(
        "tiv-reference-sql-probe-plan-{}-{nonce}.json",
        std::process::id()
    ));
    let journal_path = std::env::temp_dir().join(format!(
        "tiv-reference-sql-probe-case-{}-{nonce}.jsonl",
        std::process::id()
    ));
    let config_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );
    std::fs::write(&plan_path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args([
            "replay",
            "reference-app-case",
            "--plan",
            plan_path.to_str().unwrap(),
            "--journal",
            journal_path.to_str().unwrap(),
            "--config",
            config_path,
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
        .env(
            "TIV_POSTGRES_ADMIN_URL",
            "postgresql://tiv_admin:tiv-local-only-password@127.0.0.1:15432/postgres",
        )
        .env(
            "DATABASE_URL",
            "postgresql://tiv_app:tiv-app-local-only-password@127.0.0.1:15432/tiv_case_checkout",
        )
        .env("TIV_STRIPE_WEBHOOK_SECRET", "whsec_reference-canary")
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
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["seed"], plan.seed().value());
    assert_eq!(value["planned_action_count"], plan.actions().len());
    assert_eq!(value["executed_action_count"], plan.actions().len());
    assert_eq!(value["provider_object_count"], 1);
    assert!(
        value["journal_record_count"]
            .as_u64()
            .is_some_and(|count| count >= (plan.actions().len() * 2 + 1) as u64)
    );
    assert!(
        value["invariant_outcomes"]
            .as_array()
            .is_some_and(|outcomes| outcomes.len() == 5
                && outcomes.iter().all(|outcome| outcome["verdict"] == "held"))
    );
    assert!(journal_path.exists());
    let journal = std::fs::read_to_string(&journal_path).unwrap();
    assert!(journal.lines().any(|line| {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        record["producer"] == "postgres" && record["observation_kind"] == "sql_probe_true"
    }));
    std::fs::remove_file(journal_path).unwrap();
    std::fs::remove_file(plan_path).unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("password"));
}

fn sql_probe_plan() -> PlannedCase {
    (0..4_096)
        .find_map(|seed| {
            let spec = PlanSpec::new_payment_intent_v1(
                Seed::new(seed),
                ActionBudget::new(40).unwrap(),
                [ProviderOutcome::Normal],
                WebhookFaultSpec::new(0, [], false, false).unwrap(),
                ProcessFaultSpec::new([ProcessCutPoint::SqlProbe], 1).unwrap(),
            )
            .unwrap();
            let plan = CasePlanCompiler::compile(&spec).unwrap();
            plan.actions()
                .windows(2)
                .any(|actions| {
                    matches!(actions[0].kind(), PlanActionKind::DriveCheckout { .. })
                        && matches!(
                            actions[1].kind(),
                            PlanActionKind::KillApplication {
                                cut_point: ProcessCutPoint::SqlProbe
                            }
                        )
                })
                .then_some(plan)
        })
        .expect("the seed corpus contains a checkout-owned SQL probe cut point")
}
