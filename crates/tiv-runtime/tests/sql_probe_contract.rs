use std::{collections::BTreeMap, fs, path::PathBuf, time::Duration};

use tiv_runtime::{
    config::{EnvironmentLookup, load_resolved_config, resolve_config_document},
    postgres::{
        probe::{
            SqlProbe, SqlProbeContractError, SqlProbeError, SqlProbeQuery,
            load_configured_sql_probe,
        },
        snapshot::{InvariantRoleName, SnapshotBudgets},
    },
};

#[test]
fn configured_sql_probe_accepts_one_parameter_free_select_predicate() {
    let probe = SqlProbeQuery::new("SELECT FALSE AS release_kill;")
        .expect("one bounded SELECT predicate is valid");

    assert_eq!(probe.sql(), "SELECT FALSE AS release_kill");
}

#[test]
fn configured_sql_probe_rejects_unsafe_or_unbounded_query_text() {
    assert!(matches!(
        SqlProbeQuery::new("UPDATE payments SET status = 'paid' RETURNING TRUE"),
        Err(SqlProbeContractError::UnsupportedQueryShape)
    ));
    assert!(matches!(
        SqlProbeQuery::new("SELECT FALSE; SELECT TRUE"),
        Err(SqlProbeContractError::UnsupportedQueryShape)
    ));
    assert!(matches!(
        SqlProbeQuery::new(format!("SELECT FALSE /* {} */", "x".repeat(65_536))),
        Err(SqlProbeContractError::QueryTooLarge)
    ));
}

#[test]
fn configured_sql_probe_loads_the_repository_file_and_runtime_boundary() {
    let config = load_resolved_config(&config_path(), &test_environment())
        .expect("the checked-in config resolves");
    let probe = load_configured_sql_probe(&config)
        .expect("the configured repository predicate satisfies the probe contract");

    assert_eq!(
        probe.query().sql(),
        "SELECT current_user = 'tiv_invariant'\n   AND EXISTS (\n       SELECT 1\n       FROM payments\n       WHERE amount_minor = 2500\n         AND currency = 'usd'\n   ) AS release_kill"
    );
    assert_eq!(probe.role().as_str(), "tiv_invariant");
    assert_eq!(probe.budgets().statement_timeout().as_millis(), 2_000);
    assert_eq!(probe.budgets().lock_timeout().as_millis(), 500);
    assert!(!probe.into_probe().observed());
}

#[test]
fn configured_sql_probe_fails_closed_when_the_file_is_not_a_predicate_query() {
    let document = fs::read_to_string(config_path())
        .expect("the checked-in config exists")
        .replace(
            "sql_probe_file = \"kill_probe.sql\"",
            "sql_probe_file = \"../../../crates/tiv-runtime/src/lib.rs\"",
        );
    let root = config_path()
        .parent()
        .expect("the checked-in config has a parent")
        .to_owned();
    let config = resolve_config_document(&document, &root, &test_environment())
        .expect("config resolution only establishes the repository path boundary");

    assert!(load_configured_sql_probe(&config).is_err());
}

#[tokio::test]
#[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
async fn sql_probe_observes_one_committed_false_to_true_transition_as_the_invariant_role() {
    let (mut observer, observer_connection) = connect_test_postgres().await;
    let (writer, writer_connection) = connect_test_postgres().await;
    install_test_invariant_role(&observer).await;
    observer
        .batch_execute(
            "DROP TABLE IF EXISTS tiv_sql_probe_signal; \
             CREATE TABLE tiv_sql_probe_signal (id bigint PRIMARY KEY); \
             REVOKE ALL ON TABLE tiv_sql_probe_signal FROM PUBLIC, tiv_invariant; \
             GRANT SELECT ON TABLE tiv_sql_probe_signal TO tiv_invariant",
        )
        .await
        .expect("the isolated probe relation is ready");
    let document = fs::read_to_string(config_path())
        .expect("the checked-in config exists")
        .replace("kill_probe.sql", "transition_probe.sql");
    let path = config_path();
    let root = path.parent().expect("the checked-in config has a parent");
    let config = resolve_config_document(&document, root, &test_environment())
        .expect("the checked-in runtime boundary resolves");
    let mut probe = load_configured_sql_probe(&config)
        .expect("the repository predicate satisfies the configured contract")
        .into_probe();

    probe
        .require_false(&mut observer)
        .await
        .expect("the probe starts false");
    let (observation_result, inserted) = tokio::join!(
        probe.observe_true(
            &mut observer,
            Duration::from_secs(2),
            Duration::from_millis(10),
        ),
        async {
            tokio::task::yield_now().await;
            writer
                .execute("INSERT INTO tiv_sql_probe_signal (id) VALUES (1)", &[])
                .await
        },
    );
    inserted.expect("the signal commits on the independent application connection");
    observation_result.expect("the read-only observer sees the first true value");
    assert!(probe.observed());

    observer
        .batch_execute("DROP TABLE tiv_sql_probe_signal")
        .await
        .expect("the isolated probe relation is removed");
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

#[tokio::test]
#[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
async fn sql_probe_fails_closed_on_database_parsed_parameters_and_invalid_results() {
    enum ExpectedError {
        Parameters,
        ResultShape,
    }

    let (mut client, connection) = connect_test_postgres().await;
    install_test_invariant_role(&client).await;

    let cases = [
        ("SELECT $1::boolean", ExpectedError::Parameters),
        ("SELECT FALSE, FALSE", ExpectedError::ResultShape),
        (
            "SELECT value FROM (VALUES (FALSE), (TRUE)) AS result(value)",
            ExpectedError::ResultShape,
        ),
        ("SELECT NULL::boolean", ExpectedError::ResultShape),
    ];

    for (sql, expected) in cases {
        let error = test_probe(sql)
            .require_false(&mut client)
            .await
            .expect_err("an invalid database result must fail closed");
        assert!(
            matches!(
                (expected, &error),
                (
                    ExpectedError::Parameters,
                    SqlProbeError::ParametersForbidden
                ) | (
                    ExpectedError::ResultShape,
                    SqlProbeError::InvalidResultShape
                )
            ),
            "unexpected SQL probe error: {error}"
        );
    }

    drop(client);
    connection
        .await
        .expect("the client task exits")
        .expect("the client connection closes cleanly");
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

fn test_probe(sql: &str) -> SqlProbe {
    SqlProbe::new(
        SqlProbeQuery::new(sql).expect("the test query satisfies the static contract"),
        SnapshotBudgets::new(Duration::from_secs(2), Duration::from_millis(100))
            .expect("the test budgets are bounded"),
        InvariantRoleName::new("tiv_invariant").expect("the test role is valid"),
    )
}
