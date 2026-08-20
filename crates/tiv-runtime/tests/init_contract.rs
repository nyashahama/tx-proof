use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use tiv_runtime::config::{EnvironmentLookup, load_resolved_config};
use tiv_runtime::init::{InitError, initialize_project};

#[test]
fn init_writes_the_exact_fail_closed_v1_scaffold_without_secrets() {
    let repository = TestRepository::new(true);
    let report = initialize_project(repository.path()).expect("the empty repository initializes");
    let encoded = report.to_pretty_json().expect("the init report encodes");

    assert!(encoded.contains("\"status\": \"initialized\""));
    assert!(encoded.contains("tiv doctor --config tiv.toml"));
    for forbidden in ["sk_live_", "rk_live_", "pk_live_", "whsec_"] {
        assert!(!encoded.contains(forbidden));
    }

    let expected = [
        "checkout.json",
        "invariants/01_provider_object_unique.sql",
        "invariants/02_webhook_effect_at_most_once.sql",
        "invariants/03_paid_order_amount_conservation.sql",
        "invariants/04_terminal_success_monotonic.sql",
        "invariants/05_balanced_ledger.sql",
        "kill_probe.sql",
        "quiescence.sql",
        "tiv-safety-marker.sql",
        "tiv.toml",
    ];
    assert_eq!(report.files(), expected);
    assert!(
        expected
            .iter()
            .all(|path| repository.path().join(path).is_file())
    );

    let config = read(repository.path(), "tiv.toml");
    assert_eq!(config.matches("[[invariants]]").count(), 5);
    assert!(config.contains("TIV_POSTGRES_ADMIN_URL"));
    assert!(config.contains("TIV_STRIPE_WEBHOOK_SECRET"));
    assert!(config.contains("Replace every TODO mapping"));
    for forbidden in ["sk_live_", "rk_live_", "pk_live_", "whsec_"] {
        assert!(!config.contains(forbidden));
    }
    for path in &expected[1..=7] {
        assert!(
            read(repository.path(), path).contains("tiv_configuration_required"),
            "{path} must fail closed until authored"
        );
    }
    let marker = read(repository.path(), "tiv-safety-marker.sql");
    assert!(marker.contains("CREATE TABLE tiv_verifier_marker"));
    assert!(marker.contains("gen_random_uuid()"));
    assert!(marker.contains("TODO-compose-project"));
    assert!(marker.contains("TODO-application-role"));

    fs::write(repository.path().join("compose.yaml"), "services: {}\n")
        .expect("the customer-owned Compose placeholder writes");
    load_resolved_config(
        &repository.path().join("tiv.toml"),
        &GeneratedConfigEnvironment,
    )
    .expect("the generated document stays compatible with the typed v1 resolver");
}

#[test]
fn init_refuses_every_collision_before_writing_and_never_overwrites_user_work() {
    for collision in [
        "checkout.json",
        "invariants/01_provider_object_unique.sql",
        "invariants/02_webhook_effect_at_most_once.sql",
        "invariants/03_paid_order_amount_conservation.sql",
        "invariants/04_terminal_success_monotonic.sql",
        "invariants/05_balanced_ledger.sql",
        "kill_probe.sql",
        "quiescence.sql",
        "tiv-safety-marker.sql",
        "tiv.toml",
        "invariants",
    ] {
        let repository = TestRepository::new(true);
        let destination = repository.path().join(collision);
        if collision == "invariants" {
            fs::create_dir(&destination).expect("the directory collision is created");
        } else {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).expect("the collision parent is created");
            }
            fs::write(&destination, "user-owned\n").expect("the file collision is written");
        }

        let error = initialize_project(repository.path())
            .expect_err("any collision rejects the entire scaffold");
        assert!(
            matches!(error, InitError::DestinationExists(ref path) if path == Path::new(collision)),
            "unexpected error for {collision}: {error}"
        );
        if destination.is_file() {
            assert_eq!(
                fs::read_to_string(&destination).expect("the user file remains readable"),
                "user-owned\n"
            );
        }
        for generated in [
            "checkout.json",
            "kill_probe.sql",
            "quiescence.sql",
            "tiv-safety-marker.sql",
            "tiv.toml",
        ] {
            if generated != collision {
                assert!(!repository.path().join(generated).exists());
            }
        }
    }
}

#[test]
fn init_requires_the_current_directory_to_be_the_repository_root() {
    let directory = TestRepository::new(false);
    assert!(matches!(
        initialize_project(directory.path()),
        Err(InitError::NotRepositoryRoot)
    ));
    assert!(!directory.path().join("tiv.toml").exists());
}

fn read(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative)).expect("the generated file is readable")
}

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestRepository {
    path: PathBuf,
}

impl TestRepository {
    fn new(with_git_marker: bool) -> Self {
        let path = std::env::temp_dir().join(format!(
            "txproof-init-contract-{}-{}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("the unique test directory is created");
        if with_git_marker {
            fs::create_dir(path.join(".git")).expect("the Git marker is created");
        }
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestRepository {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).expect("the isolated test directory is removed");
    }
}

struct GeneratedConfigEnvironment;

impl EnvironmentLookup for GeneratedConfigEnvironment {
    fn get(&self, name: &str) -> Option<String> {
        match name {
            "TIV_POSTGRES_ADMIN_URL" => Some(database_url("postgres", "admin")),
            "DATABASE_URL" => Some(database_url("tiv_case_checkout", "app")),
            "TIV_STRIPE_WEBHOOK_SECRET" => Some("local-webhook-canary".to_owned()),
            _ => None,
        }
    }
}

fn database_url(database: &str, role: &str) -> String {
    let mut url = url::Url::parse(&format!("postgresql://127.0.0.1:5432/{database}"))
        .expect("the generated-config test address is valid");
    url.set_username(role)
        .expect("the generated-config test role is URL-compatible");
    url.set_password(Some("canary"))
        .expect("the generated-config test password is URL-compatible");
    url.into()
}
