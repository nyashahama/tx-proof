use std::process::Command;

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
             GRANT SELECT ON customer_seed TO tiv_app;",
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
