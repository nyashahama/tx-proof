use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use tiv_runtime::config::{EnvironmentLookup, load_resolved_config};
use tiv_runtime::postgres::snapshot::{
    InvariantQuery, InvariantRoleName, InvariantSuite, SnapshotBudgetError, SnapshotBudgets,
    SnapshotContractError, V1_INVARIANT_IDS, load_configured_snapshot,
};

#[test]
fn invariant_suite_requires_the_fixed_five_ids_in_canonical_order() {
    let valid = valid_queries();
    let suite = InvariantSuite::new(valid).expect("the fixed v1 suite is accepted");
    assert_eq!(suite.len(), 5);
    assert_eq!(suite.ids(), V1_INVARIANT_IDS);

    assert!(matches!(
        InvariantSuite::new(valid_queries().into_iter().take(4).collect()),
        Err(SnapshotContractError::InvariantCount { actual: 4 })
    ));
    let mut invented = valid_queries();
    invented[2] = InvariantQuery::new("invented-invariant", "SELECT 1 WHERE FALSE")
        .expect("the individual query syntax is valid");
    assert!(matches!(
        InvariantSuite::new(invented),
        Err(SnapshotContractError::UnsupportedInvariantSet)
    ));
}

#[test]
fn invariant_query_contract_is_bounded_and_select_only() {
    for accepted in [
        "SELECT 1 WHERE FALSE;",
        "-- repository-owned witness\nSELECT 1 WHERE FALSE",
        "/* bounded witness */ WITH witness AS (SELECT 1) SELECT * FROM witness",
        "SELECT 'DELETE is diagnostic text' AS note WHERE FALSE",
    ] {
        InvariantQuery::new(V1_INVARIANT_IDS[0], accepted)
            .expect("the narrow witness-query shape is accepted");
    }

    for rejected in [
        "",
        "-- comment only",
        "INSERT INTO payments DEFAULT VALUES RETURNING id",
        "UPDATE payments SET status = 'paid' RETURNING id",
        "SELECT 1; SELECT 2",
        "SELECT 1 INTO TEMP hidden_state",
        "WITH changed AS (DELETE FROM tiv_provider_state RETURNING 1) SELECT * FROM changed",
        "SELECT set_config('statement_timeout', '0', true)",
        "SELECT pg_advisory_lock_shared(42)",
        "SELECT pg_try_advisory_xact_lock(42)",
    ] {
        assert!(matches!(
            InvariantQuery::new(V1_INVARIANT_IDS[0], rejected),
            Err(SnapshotContractError::UnsupportedQueryShape)
        ));
    }
    assert!(matches!(
        InvariantQuery::new(V1_INVARIANT_IDS[0], " ".repeat(65_537)),
        Err(SnapshotContractError::QueryTooLarge)
    ));
}

#[test]
fn snapshot_budgets_cannot_exceed_the_v1_safety_caps() {
    let budgets = SnapshotBudgets::new(Duration::from_secs(2), Duration::from_millis(500))
        .expect("the v1 maximum budgets are accepted");
    assert_eq!(budgets.statement_timeout(), Duration::from_secs(2));
    assert_eq!(budgets.lock_timeout(), Duration::from_millis(500));

    for (statement, lock) in [
        (Duration::ZERO, Duration::from_millis(1)),
        (Duration::from_millis(1), Duration::ZERO),
        (Duration::from_millis(2_001), Duration::from_millis(500)),
        (Duration::from_secs(2), Duration::from_millis(501)),
    ] {
        assert!(matches!(
            SnapshotBudgets::new(statement, lock),
            Err(SnapshotBudgetError)
        ));
    }
}

#[test]
fn invariant_role_name_is_a_narrow_unquoted_postgres_identifier() {
    let role = InvariantRoleName::new("tiv_invariant").expect("the v1 role name is valid");
    assert_eq!(role.as_str(), "tiv_invariant");

    for rejected in [
        "",
        "TIV_INVARIANT",
        "tiv-invariant",
        "tiv_invariant; SET ROLE tiv_admin",
        "1_invariant",
        &"x".repeat(64),
    ] {
        assert!(InvariantRoleName::new(rejected).is_err());
    }
}

#[test]
fn configured_snapshot_loads_the_same_five_files_and_budgets_doctor_approved() {
    let config = load_resolved_config(&config_path(), &test_environment())
        .expect("the golden doctor config resolves");
    let snapshot =
        load_configured_snapshot(&config).expect("the five configured no-op queries load");

    assert_eq!(snapshot.suite().ids(), V1_INVARIANT_IDS);
    assert_eq!(snapshot.role().as_str(), "tiv_invariant");
    assert_eq!(
        snapshot.budgets().statement_timeout(),
        Duration::from_secs(2)
    );
    assert_eq!(
        snapshot.budgets().lock_timeout(),
        Duration::from_millis(500)
    );
}

fn valid_queries() -> Vec<InvariantQuery> {
    V1_INVARIANT_IDS
        .into_iter()
        .map(|id| {
            InvariantQuery::new(id, "SELECT 1 AS unreachable WHERE FALSE")
                .expect("the no-op query is valid")
        })
        .collect()
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
            database_url("postgres", "admin"),
        ),
        (
            "DATABASE_URL".to_owned(),
            database_url("tiv_case_checkout", "app"),
        ),
        (
            "TIV_STRIPE_WEBHOOK_SECRET".to_owned(),
            "local-webhook-canary".to_owned(),
        ),
    ]))
}

fn database_url(database: &str, role: &str) -> String {
    let mut url = url::Url::parse(&format!("postgresql://127.0.0.1:15432/{database}"))
        .expect("the test database address is valid");
    url.set_username(role)
        .expect("the test database role is URL-compatible");
    url.set_password(Some("canary"))
        .expect("the test database password is URL-compatible");
    url.into()
}
