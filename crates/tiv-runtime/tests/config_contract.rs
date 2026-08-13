use std::{collections::BTreeMap, fs, path::PathBuf};

use tiv_runtime::config::{
    ConfigError, EnvironmentLookup, config_schema_json, load_resolved_config,
};

const ADMIN_URL: &str = "postgresql://tiv_admin:admin-canary@127.0.0.1:15432/postgres";
const CASE_URL: &str = "postgresql://tiv_app:application-canary@127.0.0.1:15432/tiv_case_checkout";
const WEBHOOK_SECRET: &str = "whsec_webhook-canary";

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
