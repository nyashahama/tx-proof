use std::{collections::BTreeMap, fs, path::PathBuf};

use tiv_core::plan::CampaignPlanner;
use tiv_runtime::config::{
    ConfigError, EnvironmentLookup, config_schema_json, load_resolved_config,
};

const ADMIN_URL: &str = "postgresql://tiv_admin:admin-canary@127.0.0.1:15432/postgres";
const CASE_URL: &str = "postgresql://tiv_app:application-canary@127.0.0.1:15432/tiv_case_checkout";
const WEBHOOK_SECRET: &str = "whsec_webhook-canary";
const FIXTURE_CONTROL_TOKEN: &str = "fixture-control-canary";

#[test]
fn the_blueprint_config_resolves_and_redacts_every_secret_value() {
    let config = load_resolved_config(&config_path(), &test_environment())
        .expect("the checked-in blueprint config is valid");
    let encoded = serde_json::to_string_pretty(&config.redacted())
        .expect("the allowlisted redacted config serializes");

    assert!(encoded.contains("TIV_POSTGRES_ADMIN_URL"));
    assert!(encoded.contains("DATABASE_URL"));
    assert!(encoded.contains("TIV_STRIPE_WEBHOOK_SECRET"));
    assert!(!encoded.contains("admin-canary"));
    assert!(!encoded.contains("application-canary"));
    assert!(!encoded.contains("webhook-canary"));
    assert!(!encoded.contains("127.0.0.1:15432"));
}

#[test]
fn config_binds_mutating_commands_to_one_explicit_compose_project() {
    let document = blueprint_document();
    let config = resolve_document(&document).expect("the explicit Compose project resolves");
    let encoded = serde_json::to_value(config.redacted()).unwrap();

    assert_eq!(
        encoded["compose"]["project_name"],
        serde_json::json!("tiv-reference-app-spike")
    );
}

#[test]
fn config_rejects_database_names_outside_the_runtime_reset_grammar() {
    let document = blueprint_document();

    for invalid in ["tiv_case_", "tiv_case_short", "tiv_case_has-dash"] {
        assert!(matches!(
            resolve_document(&document.replace("tiv_case_checkout", invalid)),
            Err(ConfigError::InvalidDatabaseName)
        ));
    }
    for invalid in ["tiv_base_", "tiv_base_short", "tiv_base_has-dash"] {
        assert!(matches!(
            resolve_document(&document.replace("tiv_base_checkout", invalid)),
            Err(ConfigError::InvalidDatabaseName)
        ));
    }
}

#[test]
fn config_rejects_unknown_fields_unsupported_versions_and_non_serial_execution() {
    let document = blueprint_document();
    assert!(matches!(
        resolve_document(&" ".repeat(1_048_577)),
        Err(ConfigError::ConfigTooLarge)
    ));
    assert!(matches!(
        resolve_document(&document.replace("schema_version = 1", "schema_version = 2")),
        Err(ConfigError::UnsupportedSchemaVersion { actual: 2 })
    ));
    assert!(matches!(
        resolve_document(&document.replace("parallelism = 1", "parallelism = 2")),
        Err(ConfigError::UnsafeParallelism)
    ));
    assert!(matches!(
        resolve_document(&document.replace("cases = 20", "cases = 20\nunknown = true")),
        Err(ConfigError::Parse)
    ));

    let inline_secret = "sk_live_inline-canary";
    let Err(error) = resolve_document(&document.replace(
        "cases = 20",
        &format!("cases = 20\nunknown_secret = \"{inline_secret}\""),
    )) else {
        panic!("unknown secret-bearing fields must be rejected");
    };
    assert!(!error.to_string().contains(inline_secret));
}

#[test]
fn config_rejects_public_databases_and_any_invariant_count_other_than_five() {
    let mut public_environment = test_environment();
    public_environment.values.insert(
        "TIV_POSTGRES_ADMIN_URL".to_owned(),
        database_url("203.0.113.10", "postgres", "tiv_admin", "canary"),
    );
    assert!(matches!(
        load_resolved_config(&config_path(), &public_environment),
        Err(ConfigError::NonLocalDatabaseUrl)
    ));

    let mut live_environment = test_environment();
    live_environment.values.insert(
        "TIV_STRIPE_WEBHOOK_SECRET".to_owned(),
        "pk_live_publishable-canary".to_owned(),
    );
    assert!(matches!(
        load_resolved_config(&config_path(), &live_environment),
        Err(ConfigError::LiveStripeKey)
    ));

    let document = blueprint_document();
    assert!(matches!(
        resolve_document(
            &document.replace("http://stripe-fixture:12111", "http://payments.example.com")
        ),
        Err(ConfigError::InvalidStripeTarget)
    ));

    let fifth = document
        .find("[[invariants]]\nid = \"balanced-ledger\"")
        .expect("the fifth invariant exists");
    assert!(matches!(
        resolve_document(&document[..fifth]),
        Err(ConfigError::InvariantCount { actual: 4 })
    ));
    assert!(matches!(
        resolve_document(&document.replace(
            "id = \"provider-object-unique\"",
            "id = \"invented-invariant\""
        )),
        Err(ConfigError::UnsupportedInvariantSet)
    ));
}

#[test]
fn generated_schema_matches_the_checked_in_v1_contract() {
    let generated = config_schema_json().expect("the generated schema serializes");
    let checked_in = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../schemas/tiv-config-v1.schema.json"
    ))
    .expect("the checked-in schema exists");

    assert_eq!(generated, checked_in);
}

#[test]
fn resolved_config_preserves_the_exact_serial_campaign_contract() {
    let document = blueprint_document()
        .replace("cases = 20", "cases = 2")
        .replace("seed = 424242", "seed = 99")
        .replace("duplicate_max = 3", "duplicate_max = 0")
        .replace("delay_ms = [0, 10, 100, 1000, 5000]", "delay_ms = [17]");
    let config = resolve_document(&document).expect("the narrowed campaign config resolves");

    let campaign = CampaignPlanner::compile(config.campaign_spec())
        .expect("the resolved campaign is feasible");
    let encoded = serde_json::to_value(campaign).unwrap();

    assert_eq!(encoded["spec"]["campaign_seed"], serde_json::json!(99));
    assert_eq!(encoded["spec"]["case_count"], serde_json::json!(2));
    assert_eq!(
        encoded["spec"]["webhook_faults"]["duplicate_max"],
        serde_json::json!(0)
    );
    assert_eq!(
        encoded["spec"]["webhook_faults"]["delays_millis"],
        serde_json::json!([17])
    );
}

#[test]
fn payment_intent_fixture_control_is_loopback_bounded_and_secret_redacted() {
    let document = blueprint_document();

    let config = resolve_document(&document).expect("the local fixture control contract resolves");
    let encoded = serde_json::to_string_pretty(config.redacted())
        .expect("the fixture control projection serializes");

    assert!(encoded.contains("http://127.0.0.1:12112/"));
    assert!(encoded.contains("TIV_FIXTURE_CONTROL_TOKEN"));
    assert!(encoded.contains("\"poll_interval_ms\": 10"));
    assert!(!encoded.contains(FIXTURE_CONTROL_TOKEN));
}

#[test]
fn payment_intent_fixture_control_and_driver_body_fail_closed_before_execution() {
    let document = blueprint_document();

    assert!(matches!(
        resolve_document(&document.replace(
            "http://127.0.0.1:12112",
            "http://fixture-control.example.com:12112"
        )),
        Err(ConfigError::NonLocalHttpUrl)
    ));

    let mut missing_token = test_environment();
    missing_token.values.remove("TIV_FIXTURE_CONTROL_TOKEN");
    assert!(matches!(
        tiv_runtime::config::resolve_config_document(
            &document,
            config_path().parent().expect("the config fixture has a parent"),
            &missing_token,
        ),
        Err(ConfigError::MissingEnvironment(name)) if name == "TIV_FIXTURE_CONTROL_TOKEN"
    ));

    assert!(matches!(
        resolve_document(&document.replace(
            "body_file = \"checkout.json\"",
            "body_file = \"quiescence.sql\""
        )),
        Err(ConfigError::InvalidDriverBody)
    ));
}

fn resolve_document(document: &str) -> Result<tiv_runtime::config::ResolvedConfig, ConfigError> {
    let path = config_path();
    let root = path.parent().expect("the config fixture has a parent");
    tiv_runtime::config::resolve_config_document(document, root, &test_environment())
}

fn blueprint_document() -> String {
    fs::read_to_string(config_path()).expect("the blueprint config fixture exists")
}

fn config_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    ))
}

#[derive(Default)]
struct TestEnvironment {
    values: BTreeMap<String, String>,
}

impl EnvironmentLookup for TestEnvironment {
    fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }
}

fn test_environment() -> TestEnvironment {
    TestEnvironment {
        values: BTreeMap::from([
            ("TIV_POSTGRES_ADMIN_URL".to_owned(), ADMIN_URL.to_owned()),
            ("DATABASE_URL".to_owned(), CASE_URL.to_owned()),
            (
                "TIV_STRIPE_WEBHOOK_SECRET".to_owned(),
                WEBHOOK_SECRET.to_owned(),
            ),
            (
                "TIV_FIXTURE_CONTROL_TOKEN".to_owned(),
                FIXTURE_CONTROL_TOKEN.to_owned(),
            ),
        ]),
    }
}

fn database_url(host: &str, database: &str, role: &str, password: &str) -> String {
    let mut url = url::Url::parse(&format!("postgresql://{host}:5432/{database}"))
        .expect("the test database address is valid");
    url.set_username(role)
        .expect("the test role is URL-compatible");
    url.set_password(Some(password))
        .expect("the test password is URL-compatible");
    url.into()
}
