use std::{collections::BTreeMap, path::PathBuf};

use tiv_runtime::{
    config::{EnvironmentLookup, load_resolved_config},
    doctor::{DoctorError, compose_probe_plan, evaluate_compose_config},
};

#[test]
fn compose_probe_is_exact_local_argv_and_contains_no_mutating_command() {
    let config = load_resolved_config(&config_path(), &test_environment())
        .expect("the blueprint config resolves");
    let plan = compose_probe_plan(&config).expect("the Compose probe plan is valid");
    let commands = plan.commands();

    assert_eq!(commands.len(), 3);
    assert!(commands.iter().all(|command| command.program() == "docker"));
    assert!(commands.iter().all(|command| {
        command.args().starts_with(&[
            "--host".to_owned(),
            "unix:///var/run/docker.sock".to_owned(),
            "compose".to_owned(),
        ])
    }));
    assert!(commands.iter().any(|command| {
        command
            .args()
            .ends_with(&["version".to_owned(), "--short".to_owned()])
    }));
    assert!(commands.iter().any(|command| {
        command
            .args()
            .ends_with(&["config".to_owned(), "--hash".to_owned(), "*".to_owned()])
    }));
    assert!(commands.iter().any(|command| {
        command.args().ends_with(&[
            "config".to_owned(),
            "--format".to_owned(),
            "json".to_owned(),
        ])
    }));
    for forbidden in ["up", "down", "run", "exec", "start", "stop", "kill", "rm"] {
        assert!(
            commands
                .iter()
                .all(|command| !command.args().iter().any(|arg| arg == forbidden))
        );
    }
}

#[test]
fn compose_evaluation_requires_mapped_services_and_rejects_live_material() {
    let config = load_resolved_config(&config_path(), &test_environment())
        .expect("the blueprint config resolves");
    let safe = r#"{
      "name": "tiv-doctor-probe",
      "services": {
        "postgres": {"image": "postgres:18.4-bookworm", "environment": {"POSTGRES_PASSWORD": "canary", "UNCLASSIFIED_VALUE": "first-canary"}},
        "reference-app": {"image": "reference-app", "environment": {"DATABASE_URL": "postgresql://app:canary@postgres/tiv_case_checkout"}},
        "stripe-fixture": {"image": "stripe-fixture"}
      }
    }"#;
    let service_hashes = concat!(
        "postgres aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        "reference-app bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        "stripe-fixture cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\n",
    );
    let facts = evaluate_compose_config(&config, "5.4.0", safe, service_hashes)
        .expect("the mapped disposable Compose graph is accepted");
    assert_eq!(
        facts.services(),
        ["postgres", "reference-app", "stripe-fixture"]
    );
    assert_eq!(
        facts.service_config_hash("reference-app"),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    );
    let encoded = serde_json::to_string(&facts).expect("the facts serialize");
    assert!(encoded.contains("reference-app"));
    assert!(encoded.contains("postgres"));
    assert!(!encoded.contains("canary"));

    let changed_environment = evaluate_compose_config(
        &config,
        "5.4.0",
        &safe.replace("first-canary", "second-canary"),
        service_hashes,
    )
    .expect("all Compose environment values are redacted before hashing");
    assert_eq!(
        serde_json::to_value(&facts).expect("the first facts encode")["resolved_redacted_hash"],
        serde_json::to_value(&changed_environment).expect("the second facts encode")["resolved_redacted_hash"]
    );

    assert!(matches!(
        evaluate_compose_config(
            &config,
            "5.4.0",
            &safe.replace("reference-app", "missing-app"),
            service_hashes,
        ),
        Err(DoctorError::MissingComposeService(_))
    ));
    assert!(matches!(
        evaluate_compose_config(
            &config,
            "5.4.0",
            &safe.replace(
                "\"stripe-fixture\": {\"image\": \"stripe-fixture\"}",
                "\"missing-stripe\": {\"image\": \"stripe-fixture\"}"
            ),
            service_hashes,
        ),
        Err(DoctorError::MissingComposeService(_))
    ));
    assert!(matches!(
        evaluate_compose_config(
            &config,
            "5.4.0",
            &safe.replace("canary@postgres", "sk_live_secret@postgres"),
            service_hashes,
        ),
        Err(DoctorError::LiveStripeMaterial)
    ));
    assert!(matches!(
        evaluate_compose_config(
            &config,
            "5.4.0",
            &safe.replace("canary@postgres", "canary@db.example.com"),
            service_hashes,
        ),
        Err(DoctorError::PublicDatabaseTarget)
    ));
    assert!(matches!(
        evaluate_compose_config(
            &config,
            "5.4.0",
            safe,
            service_hashes
                .replace("reference-app ", "other-service ")
                .as_str(),
        ),
        Err(DoctorError::InvalidServiceConfigHashes)
    ));
}

fn config_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    ))
}

struct TestEnvironment(BTreeMap<String, String>);

impl EnvironmentLookup for TestEnvironment {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

fn test_environment() -> TestEnvironment {
    TestEnvironment(BTreeMap::from([
        (
            "TIV_POSTGRES_ADMIN_URL".to_owned(),
            "postgresql://tiv_admin:canary@127.0.0.1:15432/postgres".to_owned(),
        ),
        (
            "DATABASE_URL".to_owned(),
            "postgresql://tiv_app:canary@127.0.0.1:15432/tiv_case_checkout".to_owned(),
        ),
        (
            "TIV_STRIPE_WEBHOOK_SECRET".to_owned(),
            "whsec_canary".to_owned(),
        ),
        (
            "TIV_FIXTURE_CONTROL_TOKEN".to_owned(),
            "fixture-control-canary".to_owned(),
        ),
    ]))
}
