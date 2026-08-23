use std::{collections::BTreeMap, process::Command};

use tiv_runtime::{
    baseline::{BaselineError, ConfiguredBaselineSession},
    config::{EnvironmentLookup, load_resolved_config},
};
use tokio_postgres::NoTls;
use uuid::Uuid;

const ADMIN_URL: &str = "postgresql://tiv_admin:tiv-local-only-password@127.0.0.1:15432/postgres";
const LIMITED_ADMIN_URL: &str =
    "postgresql://tiv_test_admin:tiv-test-admin-password@127.0.0.1:15432/postgres";
const CASE_URL: &str =
    "postgresql://tiv_app:tiv-app-local-only-password@127.0.0.1:15432/tiv_case_checkout";

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn baseline_rejects_an_application_role_that_can_tamper_with_its_marker() {
    prepare_case_database().await;
    let original_oid = database_oid("tiv_case_checkout").await;
    set_application_superuser(true).await;
    let config = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );

    let rejected = baseline_command(config)
        .output()
        .expect("the unsafe-role challenge executes");
    set_application_superuser(false).await;

    assert_eq!(rejected.status.code(), Some(2));
    assert_eq!(database_oid("tiv_case_checkout").await, original_oid);
    assert!(!database_exists("tiv_base_checkout").await);
    cleanup_databases().await;
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn baseline_rejects_an_application_role_that_owns_the_case_database() {
    prepare_case_database().await;
    set_case_owner("tiv_app").await;
    let original_oid = database_oid("tiv_case_checkout").await;
    let config = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );

    let rejected = baseline_command(config)
        .output()
        .expect("the unsafe-owner challenge executes");
    set_case_owner("tiv_admin").await;

    assert_eq!(rejected.status.code(), Some(2));
    assert_eq!(database_oid("tiv_case_checkout").await, original_oid);
    assert!(!database_exists("tiv_base_checkout").await);
    cleanup_databases().await;
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn baseline_challenge_is_read_only_then_exact_consent_proves_a_real_reset() {
    prepare_case_database().await;
    let original_oid = database_oid("tiv_case_checkout").await;
    let config = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );

    let challenge = baseline_command(config)
        .output()
        .expect("the baseline challenge executes");
    assert!(
        challenge.status.success(),
        "challenge failed: {}",
        String::from_utf8_lossy(&challenge.stderr)
    );
    let challenge: serde_json::Value =
        serde_json::from_slice(&challenge.stdout).expect("challenge stdout is JSON");
    assert_eq!(challenge["status"], "acknowledgement_required");
    assert_eq!(challenge["mutation_authorized"], false);
    assert_eq!(database_oid("tiv_case_checkout").await, original_oid);
    assert!(!database_exists("tiv_base_checkout").await);

    let phrase = challenge["reset_acknowledgement"]
        .as_str()
        .expect("the exact reset phrase is emitted");
    let rejected = baseline_command(config)
        .args(["--acknowledge-reset", "RESET approximate identity"])
        .output()
        .expect("the rejected acknowledgement executes");
    assert_eq!(rejected.status.code(), Some(2));
    assert_eq!(database_oid("tiv_case_checkout").await, original_oid);
    assert!(!database_exists("tiv_base_checkout").await);

    let completed = baseline_command(config)
        .args(["--acknowledge-reset", phrase])
        .output()
        .expect("the acknowledged baseline executes");
    assert!(
        completed.status.success(),
        "baseline failed: {}",
        String::from_utf8_lossy(&completed.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&completed.stdout).expect("baseline stdout is JSON");
    assert_eq!(report["status"], "baseline_ready");
    assert_eq!(report["mutation_authorized"], true);
    assert_eq!(report["reset_proof"]["probe_removed"], true);
    assert_eq!(report["case_database"]["before_oid"], original_oid);
    assert_ne!(report["case_database"]["after_oid"], original_oid);

    let (client, connection) = tokio_postgres::connect(CASE_URL, NoTls)
        .await
        .expect("the reset application database accepts its configured role");
    let connection = tokio::spawn(connection);
    let seed = client
        .query_one("SELECT value FROM customer_seed", &[])
        .await
        .expect("the customer seed survives the reset")
        .get::<_, String>(0);
    assert_eq!(seed, "seeded-before-baseline");
    assert!(
        client
            .query_opt("SELECT to_regclass('public.tiv_reset_probe')::text", &[])
            .await
            .expect("the reset probe can be inspected")
            .and_then(|row| row.get::<_, Option<String>>(0))
            .is_none(),
        "the private reset probe must not survive"
    );
    drop(client);
    connection.await.unwrap().unwrap();

    let baseline = database_catalog("tiv_base_checkout").await;
    assert!(baseline.1, "the baseline is marked as a template");
    assert!(!baseline.2, "the sealed baseline rejects connections");
    cleanup_databases().await;
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn baseline_restores_case_connections_when_cloning_fails() {
    prepare_case_database().await;
    install_limited_admin().await;
    let config = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );
    let challenge = baseline_command(config)
        .env("TIV_POSTGRES_ADMIN_URL", LIMITED_ADMIN_URL)
        .output()
        .expect("the baseline challenge executes");
    assert!(
        challenge.status.success(),
        "challenge failed: {}",
        String::from_utf8_lossy(&challenge.stderr)
    );
    let challenge: serde_json::Value = serde_json::from_slice(&challenge.stdout).unwrap();
    let phrase = challenge["reset_acknowledgement"].as_str().unwrap();

    set_limited_admin_createdb(false).await;
    let failed = baseline_command(config)
        .env("TIV_POSTGRES_ADMIN_URL", LIMITED_ADMIN_URL)
        .args(["--acknowledge-reset", phrase])
        .output()
        .expect("the deliberately failed clone executes");
    let case_connections_were_enabled = restore_case_owner_and_remove_limited_admin().await;

    assert_eq!(failed.status.code(), Some(3));
    assert!(
        case_connections_were_enabled,
        "a failed clone must not strand the customer case with connections disabled"
    );
    assert!(!database_exists("tiv_base_checkout").await);
    cleanup_databases().await;
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn sealed_customer_baseline_supports_two_fresh_attested_case_resets() {
    prepare_case_database().await;
    let config_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );
    let challenge = baseline_command(config_path)
        .output()
        .expect("the baseline challenge executes");
    assert!(challenge.status.success());
    let challenge: serde_json::Value = serde_json::from_slice(&challenge.stdout).unwrap();
    let completed = baseline_command(config_path)
        .args([
            "--acknowledge-reset",
            challenge["reset_acknowledgement"].as_str().unwrap(),
        ])
        .output()
        .expect("the acknowledged baseline executes");
    assert!(
        completed.status.success(),
        "baseline failed: {}",
        String::from_utf8_lossy(&completed.stderr)
    );

    let config = load_resolved_config(config_path.as_ref(), &test_environment())
        .expect("the live customer config resolves");
    let sealed_before = database_catalog_with_comment("tiv_base_checkout").await;
    let session = ConfiguredBaselineSession::attest(&config)
        .await
        .expect("the sealed baseline and current case are freshly attested");
    assert_eq!(
        database_catalog_with_comment("tiv_base_checkout").await,
        sealed_before,
        "internal attestation must not change the sealed baseline catalog identity"
    );
    assert_eq!(temporary_database_count().await, 0);
    let first_oid = session.case_identity().database_oid();
    let first_marker = session.case_identity().marker().marker_uuid().to_string();

    install_dirty_case_state("dirty-before-first-reset").await;
    let (session, first_reset) = session
        .reset_case()
        .await
        .expect("the first configured case reset succeeds");
    assert_eq!(first_reset.before_database_oid(), first_oid);
    assert_ne!(first_reset.after_database_oid(), first_oid);
    assert_eq!(first_reset.before_marker_uuid(), first_marker);
    assert_ne!(first_reset.after_marker_uuid(), first_marker);
    assert_seeded_clean_case().await;

    install_dirty_case_state("dirty-before-second-reset").await;
    let second_before_oid = session.case_identity().database_oid();
    let second_before_marker = session.case_identity().marker().marker_uuid().to_string();
    let (session, second_reset) = session
        .reset_case()
        .await
        .expect("the second configured case reset succeeds");
    assert_eq!(second_reset.before_database_oid(), second_before_oid);
    assert_ne!(second_reset.after_database_oid(), second_before_oid);
    assert_eq!(second_reset.before_marker_uuid(), second_before_marker);
    assert_ne!(second_reset.after_marker_uuid(), second_before_marker);
    assert_eq!(
        session.case_identity().database_oid(),
        second_reset.after_database_oid()
    );
    assert_seeded_clean_case().await;

    assert_eq!(
        database_catalog_with_comment("tiv_base_checkout").await,
        sealed_before,
        "automatic resets must not mutate or replace the sealed baseline"
    );
    cleanup_databases().await;
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn tampered_sealed_baseline_is_rejected_before_case_mutation_and_services_recover() {
    prepare_case_database().await;
    let config_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );
    let challenge = baseline_command(config_path).output().unwrap();
    let challenge: serde_json::Value = serde_json::from_slice(&challenge.stdout).unwrap();
    let completed = baseline_command(config_path)
        .args([
            "--acknowledge-reset",
            challenge["reset_acknowledgement"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(completed.status.success());

    let config = load_resolved_config(config_path.as_ref(), &test_environment()).unwrap();
    let session = ConfiguredBaselineSession::attest(&config).await.unwrap();
    install_dirty_case_state("must-survive-rejected-reset").await;
    let case_oid = database_oid("tiv_case_checkout").await;
    set_database_comment("tiv_base_checkout", "tampered-baseline-marker").await;

    assert!(matches!(
        session.reset_case().await,
        Err(BaselineError::BaselineIdentityMismatch)
    ));
    assert_eq!(database_oid("tiv_case_checkout").await, case_oid);
    assert!(relation_exists("customer_dirty").await);
    assert!(application_service_is_healthy());
    cleanup_databases().await;
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn in_database_baseline_marker_tampering_is_rejected_before_case_mutation() {
    prepare_case_database().await;
    let config_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );
    seal_customer_baseline(config_path);

    let config = load_resolved_config(config_path.as_ref(), &test_environment()).unwrap();
    let session = ConfiguredBaselineSession::attest(&config).await.unwrap();
    install_dirty_case_state("must-survive-in-database-baseline-tampering").await;
    let case_oid = database_oid("tiv_case_checkout").await;
    let baseline_catalog = database_catalog_with_comment("tiv_base_checkout").await;
    tamper_sealed_baseline_marker().await;
    assert_eq!(
        database_catalog_with_comment("tiv_base_checkout").await,
        baseline_catalog,
        "the poisoned baseline deliberately preserves its catalog attestation"
    );

    assert!(matches!(
        session.reset_case().await,
        Err(BaselineError::BaselineIdentityMismatch)
    ));
    assert_eq!(database_oid("tiv_case_checkout").await, case_oid);
    assert!(relation_exists("customer_dirty").await);
    assert_eq!(temporary_database_count().await, 0);
    assert!(application_service_is_healthy());
    cleanup_databases().await;
}

#[tokio::test]
#[ignore = "requires the isolated reference-app Compose project"]
async fn failed_replacement_restores_the_exact_original_case_before_services_restart() {
    prepare_case_database().await;
    let config_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/golden/doctor-project/tiv.toml"
    );
    seal_customer_baseline(config_path);

    let config = load_resolved_config(config_path.as_ref(), &test_environment()).unwrap();
    let session = ConfiguredBaselineSession::attest(&config).await.unwrap();
    install_dirty_case_state("must-survive-replacement-failure").await;
    let case_oid = database_oid("tiv_case_checkout").await;
    let case_marker = session.case_identity().marker().marker_uuid();
    install_baseline_marker_update_failure().await;

    assert!(matches!(
        session.reset_case().await,
        Err(BaselineError::Database(_))
    ));
    assert_eq!(database_oid("tiv_case_checkout").await, case_oid);
    assert_eq!(case_marker_uuid().await, case_marker);
    assert!(relation_exists("customer_dirty").await);
    assert_eq!(temporary_database_count().await, 0);
    assert!(application_service_is_healthy());
    cleanup_databases().await;
}

fn baseline_command(config: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tiv"));
    command
        .args(["baseline", "--config", config])
        .env("TIV_POSTGRES_ADMIN_URL", ADMIN_URL)
        .env("DATABASE_URL", CASE_URL)
        .env("TIV_STRIPE_WEBHOOK_SECRET", "whsec_test_secret")
        .env("DOCKER_HOST", "tcp://127.0.0.1:9")
        .env("DOCKER_CONTEXT", "intentionally-remote")
        .env("DOCKER_TLS_VERIFY", "1")
        .env("DOCKER_CERT_PATH", "/definitely/not/a/docker/certificate");
    command
}

fn seal_customer_baseline(config: &str) {
    let challenge = baseline_command(config).output().unwrap();
    assert!(
        challenge.status.success(),
        "baseline challenge failed: {}",
        String::from_utf8_lossy(&challenge.stderr)
    );
    let challenge: serde_json::Value = serde_json::from_slice(&challenge.stdout).unwrap();
    let completed = baseline_command(config)
        .args([
            "--acknowledge-reset",
            challenge["reset_acknowledgement"].as_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        completed.status.success(),
        "baseline failed: {}",
        String::from_utf8_lossy(&completed.stderr)
    );
}

async fn install_limited_admin() {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE ROLE tiv_test_admin LOGIN CREATEDB INHERIT \
                 PASSWORD 'tiv-test-admin-password'; \
             GRANT pg_monitor, pg_signal_backend TO tiv_test_admin; \
             ALTER DATABASE tiv_case_checkout OWNER TO tiv_test_admin",
        )
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();

    let admin_case_url = ADMIN_URL.replace("/postgres", "/tiv_case_checkout");
    let (client, connection) = tokio_postgres::connect(&admin_case_url, NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute("GRANT SELECT ON tiv_verifier_marker TO tiv_test_admin")
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn set_limited_admin_createdb(enabled: bool) {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let attribute = if enabled { "CREATEDB" } else { "NOCREATEDB" };
    client
        .batch_execute(&format!("ALTER ROLE tiv_test_admin {attribute}"))
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn restore_case_owner_and_remove_limited_admin() -> bool {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let connections_were_enabled = client
        .query_one(
            "SELECT datallowconn FROM pg_database WHERE datname = 'tiv_case_checkout'",
            &[],
        )
        .await
        .unwrap()
        .get::<_, bool>(0);
    client
        .batch_execute("ALTER DATABASE tiv_case_checkout ALLOW_CONNECTIONS true")
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();

    let admin_case_url = ADMIN_URL.replace("/postgres", "/tiv_case_checkout");
    let (client, connection) = tokio_postgres::connect(&admin_case_url, NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute("REVOKE ALL ON tiv_verifier_marker FROM tiv_test_admin")
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();

    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute(
            "ALTER DATABASE tiv_case_checkout OWNER TO tiv_admin; \
             DROP ROLE tiv_test_admin",
        )
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
    connections_were_enabled
}

async fn prepare_case_database() {
    cleanup_databases().await;
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute(
            "DO $$ BEGIN \
                 IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tiv_app') THEN \
                   CREATE ROLE tiv_app LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE \
                     NOREPLICATION NOBYPASSRLS PASSWORD 'tiv-app-local-only-password'; \
                 END IF; \
             END $$;",
        )
        .await
        .unwrap();
    client
        .batch_execute(
            "DO $$ BEGIN \
                 IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'tiv_invariant') THEN \
                   CREATE ROLE tiv_invariant NOLOGIN NOINHERIT NOSUPERUSER NOCREATEDB \
                     NOCREATEROLE NOREPLICATION NOBYPASSRLS; \
                 END IF; \
             END $$;",
        )
        .await
        .unwrap();
    client
        .batch_execute(
            "ALTER ROLE tiv_app WITH LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE \
             NOREPLICATION NOBYPASSRLS PASSWORD 'tiv-app-local-only-password'",
        )
        .await
        .unwrap();
    client
        .batch_execute("CREATE DATABASE tiv_case_checkout WITH OWNER tiv_admin")
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();

    let admin_case_url = ADMIN_URL.replace("/postgres", "/tiv_case_checkout");
    let (client, connection) = tokio_postgres::connect(&admin_case_url, NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE TABLE tiv_verifier_marker ( \
                 singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton), \
                 marker_uuid uuid NOT NULL, \
                 marker_kind text NOT NULL CHECK (marker_kind IN ('baseline', 'case')), \
                 compose_project text NOT NULL, \
                 application_role text NOT NULL \
             ); \
             CREATE TABLE customer_seed (value text NOT NULL); \
             INSERT INTO customer_seed VALUES ('seeded-before-baseline'); \
             REVOKE ALL ON SCHEMA public FROM PUBLIC; \
             GRANT USAGE ON SCHEMA public TO tiv_app; \
             GRANT USAGE ON SCHEMA public TO tiv_invariant; \
             GRANT SELECT ON customer_seed TO tiv_app, tiv_invariant;",
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO tiv_verifier_marker \
                 (marker_uuid, marker_kind, compose_project, application_role) \
             VALUES ($1, 'case', 'tiv-reference-app-spike', 'tiv_app')",
            &[&Uuid::new_v4()],
        )
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn set_application_superuser(enabled: bool) {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let attribute = if enabled { "SUPERUSER" } else { "NOSUPERUSER" };
    client
        .batch_execute(&format!("ALTER ROLE tiv_app WITH {attribute}"))
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn set_case_owner(owner: &str) {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute(&format!(
            "ALTER DATABASE tiv_case_checkout OWNER TO {owner}"
        ))
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn cleanup_databases() {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute(
            "ALTER DATABASE tiv_base_checkout IS_TEMPLATE false; \
             ALTER DATABASE tiv_base_checkout ALLOW_CONNECTIONS true;",
        )
        .await
        .ok();
    for database in ["tiv_case_checkout", "tiv_base_checkout"] {
        client
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datname = $1 AND pid <> pg_backend_pid()",
                &[&database],
            )
            .await
            .unwrap();
        client
            .batch_execute(&format!("DROP DATABASE IF EXISTS {database}"))
            .await
            .unwrap();
    }
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn database_exists(database: &str) -> bool {
    database_catalog_optional(database).await.is_some()
}

async fn database_oid(database: &str) -> i64 {
    database_catalog(database).await.0
}

async fn database_catalog(database: &str) -> (i64, bool, bool) {
    database_catalog_optional(database)
        .await
        .expect("the database exists")
}

async fn database_catalog_optional(database: &str) -> Option<(i64, bool, bool)> {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let row = client
        .query_opt(
            "SELECT oid::bigint, datistemplate, datallowconn \
             FROM pg_database WHERE datname = $1",
            &[&database],
        )
        .await
        .unwrap()
        .map(|row| (row.get(0), row.get(1), row.get(2)));
    drop(client);
    connection.await.unwrap().unwrap();
    row
}

async fn database_catalog_with_comment(database: &str) -> (i64, bool, bool, String) {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let row = client
        .query_one(
            "SELECT oid::bigint, datistemplate, datallowconn, \
                    COALESCE(shobj_description(oid, 'pg_database'), '') \
             FROM pg_database WHERE datname = $1",
            &[&database],
        )
        .await
        .unwrap();
    let result = (row.get(0), row.get(1), row.get(2), row.get(3));
    drop(client);
    connection.await.unwrap().unwrap();
    result
}

async fn install_dirty_case_state(value: &str) {
    let admin_case_url = ADMIN_URL.replace("/postgres", "/tiv_case_checkout");
    let (client, connection) = tokio_postgres::connect(&admin_case_url, NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute("CREATE TABLE customer_dirty (value text NOT NULL)")
        .await
        .unwrap();
    client
        .execute("INSERT INTO customer_dirty VALUES ($1)", &[&value])
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn assert_seeded_clean_case() {
    let (client, connection) = tokio_postgres::connect(CASE_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    assert_eq!(
        client
            .query_one("SELECT value FROM customer_seed", &[])
            .await
            .unwrap()
            .get::<_, String>(0),
        "seeded-before-baseline"
    );
    assert!(
        client
            .query_one("SELECT to_regclass('public.customer_dirty') IS NULL", &[])
            .await
            .unwrap()
            .get::<_, bool>(0),
        "dirty case state must not survive a reset"
    );
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn relation_exists(relation: &str) -> bool {
    let admin_case_url = ADMIN_URL.replace("/postgres", "/tiv_case_checkout");
    let (client, connection) = tokio_postgres::connect(&admin_case_url, NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let exists = client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&relation])
        .await
        .unwrap()
        .get::<_, bool>(0);
    drop(client);
    connection.await.unwrap().unwrap();
    exists
}

async fn set_database_comment(database: &str, comment: &str) {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let quoted = client
        .query_one("SELECT quote_literal($1::text)", &[&comment])
        .await
        .unwrap()
        .get::<_, String>(0);
    client
        .batch_execute(&format!("COMMENT ON DATABASE {database} IS {quoted}"))
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn tamper_sealed_baseline_marker() {
    set_database_connections("tiv_base_checkout", true).await;
    let baseline_url = ADMIN_URL.replace("/postgres", "/tiv_base_checkout");
    let (client, connection) = tokio_postgres::connect(&baseline_url, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    client
        .execute(
            "UPDATE tiv_verifier_marker SET marker_uuid = $1",
            &[&Uuid::new_v4()],
        )
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
    set_database_connections("tiv_base_checkout", false).await;
}

async fn install_baseline_marker_update_failure() {
    set_database_connections("tiv_base_checkout", true).await;
    let baseline_url = ADMIN_URL.replace("/postgres", "/tiv_base_checkout");
    let (client, connection) = tokio_postgres::connect(&baseline_url, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    client
        .batch_execute(
            "CREATE FUNCTION tiv_reject_marker_update() RETURNS trigger LANGUAGE plpgsql AS $$ \
                 BEGIN RAISE EXCEPTION 'injected marker update failure'; END \
             $$; \
             CREATE TRIGGER tiv_reject_marker_update \
                 BEFORE UPDATE ON tiv_verifier_marker \
                 FOR EACH ROW EXECUTE FUNCTION tiv_reject_marker_update()",
        )
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
    set_database_connections("tiv_base_checkout", false).await;
}

async fn set_database_connections(database: &str, enabled: bool) {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let setting = if enabled { "true" } else { "false" };
    client
        .batch_execute(&format!(
            "ALTER DATABASE {database} ALLOW_CONNECTIONS {setting}"
        ))
        .await
        .unwrap();
    drop(client);
    connection.await.unwrap().unwrap();
}

async fn case_marker_uuid() -> Uuid {
    let admin_case_url = ADMIN_URL.replace("/postgres", "/tiv_case_checkout");
    let (client, connection) = tokio_postgres::connect(&admin_case_url, NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let marker = client
        .query_one("SELECT marker_uuid FROM tiv_verifier_marker", &[])
        .await
        .unwrap()
        .get(0);
    drop(client);
    connection.await.unwrap().unwrap();
    marker
}

async fn temporary_database_count() -> i64 {
    let (client, connection) = tokio_postgres::connect(ADMIN_URL, NoTls).await.unwrap();
    let connection = tokio::spawn(connection);
    let count = client
        .query_one(
            "SELECT count(*) FROM pg_database \
             WHERE datname LIKE 'tiv_case_attest_%' \
                OR datname LIKE 'tiv_case_recovery_%'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    drop(client);
    connection.await.unwrap().unwrap();
    count
}

fn application_service_is_healthy() -> bool {
    let container = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "ps",
            "--filter",
            "label=com.docker.compose.project=tiv-reference-app-spike",
            "--filter",
            "label=com.docker.compose.service=reference-app",
            "--format",
            "{{.ID}}",
        ])
        .output()
        .unwrap();
    if !container.status.success() {
        return false;
    }
    let container = String::from_utf8_lossy(&container.stdout);
    let ids = container.lines().collect::<Vec<_>>();
    let [container_id] = ids.as_slice() else {
        return false;
    };
    let output = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "inspect",
            "--format",
            "{{.State.Running}} {{if .State.Health}}{{.State.Health.Status}}{{end}}",
            container_id,
        ])
        .output()
        .unwrap();
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true healthy"
}

struct TestEnvironment(BTreeMap<String, String>);

impl EnvironmentLookup for TestEnvironment {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

fn test_environment() -> TestEnvironment {
    TestEnvironment(BTreeMap::from([
        ("TIV_POSTGRES_ADMIN_URL".to_owned(), ADMIN_URL.to_owned()),
        ("DATABASE_URL".to_owned(), CASE_URL.to_owned()),
        (
            "TIV_STRIPE_WEBHOOK_SECRET".to_owned(),
            "whsec_test_secret".to_owned(),
        ),
    ]))
}
