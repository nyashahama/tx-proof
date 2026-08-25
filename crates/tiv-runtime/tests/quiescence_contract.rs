use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use tiv_runtime::{
    config::{EnvironmentLookup, load_resolved_config, resolve_config_document},
    postgres::quiescence::{QuiescenceQuery, load_configured_quiescence},
};

#[test]
fn configured_quiescence_uses_the_repository_query_and_bounded_stable_window() {
    let config = load_resolved_config(&config_path(), &test_environment())
        .expect("the checked-in execution config resolves");

    let quiescence = load_configured_quiescence(&config)
        .expect("the repository-owned quiescence query satisfies the contract");

    assert_eq!(quiescence.query().sql(), "SELECT TRUE AS is_quiescent");
    assert_eq!(quiescence.role().as_str(), "tiv_invariant");
    assert_eq!(
        quiescence.budgets().statement_timeout(),
        Duration::from_secs(2)
    );
    assert_eq!(quiescence.stable_for(), Duration::from_millis(500));
    assert_eq!(quiescence.timeout(), Duration::from_secs(10));
}

#[test]
fn quiescence_query_rejects_mutation_multiple_statements_and_unbounded_input() {
    assert!(QuiescenceQuery::new("SELECT TRUE;").is_ok());
    assert!(QuiescenceQuery::new("WITH ready AS (SELECT TRUE) SELECT * FROM ready").is_ok());

    for invalid in [
        "UPDATE jobs SET ready = TRUE RETURNING ready",
        "SELECT TRUE; SELECT FALSE",
        "SELECT pg_terminate_backend(1)",
    ] {
        assert!(QuiescenceQuery::new(invalid).is_err());
    }
    assert!(QuiescenceQuery::new("x".repeat(65 * 1024)).is_err());
}

#[tokio::test]
#[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
async fn configured_quiescence_requires_one_continuous_true_stable_window() {
    let (mut observer, observer_connection) = connect_test_postgres().await;
    let (writer, writer_connection) = connect_test_postgres().await;
    install_test_invariant_role(&observer).await;
    observer
        .batch_execute(
            "DROP TABLE IF EXISTS tiv_quiescence_signal; \
             CREATE TABLE tiv_quiescence_signal (id bigint PRIMARY KEY); \
             REVOKE ALL ON TABLE tiv_quiescence_signal FROM PUBLIC, tiv_invariant; \
             GRANT SELECT ON TABLE tiv_quiescence_signal TO tiv_invariant",
        )
        .await
        .expect("the isolated quiescence relation is ready");
    let document = std::fs::read_to_string(config_path())
        .expect("the checked-in config exists")
        .replace("quiescence.sql", "transition_quiescence.sql")
        .replace(
            "quiescence_stable_for = \"500ms\"",
            "quiescence_stable_for = \"100ms\"",
        )
        .replace(
            "quiescence_timeout = \"10s\"",
            "quiescence_timeout = \"2s\"",
        );
    let root = config_path()
        .parent()
        .expect("the checked-in config has a parent")
        .to_owned();
    let config = resolve_config_document(&document, &root, &test_environment())
        .expect("the bounded quiescence config resolves");
    let quiescence = load_configured_quiescence(&config)
        .expect("the transition query satisfies the static contract");

    let (completion, inserted) = tokio::join!(
        quiescence.await_stable(&mut observer, Duration::from_millis(10)),
        async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            writer
                .execute("INSERT INTO tiv_quiescence_signal (id) VALUES (1)", &[])
                .await
        },
    );
    inserted.expect("the application-side signal commits");
    completion.expect("the predicate remains true for the complete stable window");

    observer
        .batch_execute("DROP TABLE tiv_quiescence_signal")
        .await
        .expect("the isolated quiescence relation is removed");
    drop(observer);
    drop(writer);
    observer_connection
        .await
        .expect("the observer task exits")
        .expect("the observer connection closes cleanly");
    writer_connection
        .await
        .expect("the writer task exits")
        .expect("the writer connection closes cleanly");
}

fn config_path() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    ))
}

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
            (
                "TIV_POSTGRES_ADMIN_URL".to_owned(),
                "postgresql://tiv_admin:admin-canary@127.0.0.1:15432/postgres".to_owned(),
            ),
            (
                "DATABASE_URL".to_owned(),
                "postgresql://tiv_app:application-canary@127.0.0.1:15432/tiv_case_checkout"
                    .to_owned(),
            ),
            (
                "TIV_STRIPE_WEBHOOK_SECRET".to_owned(),
                "whsec_webhook-canary".to_owned(),
            ),
            (
                "TIV_FIXTURE_CONTROL_TOKEN".to_owned(),
                "fixture-control-canary".to_owned(),
            ),
        ]),
    }
}

async fn connect_test_postgres() -> (
    tokio_postgres::Client,
    tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
) {
    let port = std::env::var("TIV_POSTGRES_TEST_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(15_432);
    let mut config = tokio_postgres::Config::new();
    config
        .host("127.0.0.1")
        .port(port)
        .user("tiv_admin")
        .password("tiv-local-only-password")
        .dbname("postgres");
    let (client, connection) = config
        .connect(tokio_postgres::NoTls)
        .await
        .expect("the isolated truth-spike PostgreSQL accepts the admin test role");
    (client, tokio::spawn(connection))
}

async fn install_test_invariant_role(client: &tokio_postgres::Client) {
    client
        .batch_execute(
            "REVOKE CREATE, TEMPORARY ON DATABASE postgres FROM PUBLIC; \
             DO $$ \
             BEGIN \
                 CREATE ROLE tiv_invariant WITH \
                     NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT \
                     NOREPLICATION NOBYPASSRLS PASSWORD NULL; \
             EXCEPTION WHEN duplicate_object THEN \
                 NULL; \
             END \
             $$; \
             ALTER ROLE tiv_invariant WITH \
                 NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT \
                 NOREPLICATION NOBYPASSRLS PASSWORD NULL",
        )
        .await
        .expect("the isolated least-privilege invariant role is installed");
}
