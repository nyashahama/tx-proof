use thiserror::Error;
use tokio::task::{JoinError, JoinHandle};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

use super::{
    oracle::{
        OracleError, ProviderPaymentIntent, QuiescencePermit, SnapshotReport, run_reference_oracle,
    },
    safety::{
        ComposeProjectId, DatabaseEndpoint, DatabaseIdentity, DatabaseKind, DatabaseMarker,
        DatabaseName, DatabaseTarget, InvalidDatabaseIdentity, InvalidDatabaseName, MarkerKind,
        MutationPermit, SafetyError, Unverified, Verified,
    },
};

const APPLICATION_ROLE: &str = "tiv_app";
const EXPECTED_CLUSTER_NAME: &str = "tiv-truth-spike-postgres";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BaselineTarget {
    identity: DatabaseIdentity,
    is_template: bool,
    allows_connections: bool,
}

impl BaselineTarget {
    fn is_sealed(&self) -> bool {
        self.is_template && !self.allows_connections
    }
}

pub struct SpikePostgresConfig {
    endpoint: DatabaseEndpoint,
    admin_role: String,
    password: String,
}

impl SpikePostgresConfig {
    #[must_use]
    pub fn loopback(port: u16, admin_role: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            endpoint: DatabaseEndpoint::loopback(port),
            admin_role: admin_role.into(),
            password: password.into(),
        }
    }
}

pub struct TruthSpikePostgres {
    config: SpikePostgresConfig,
}

impl TruthSpikePostgres {
    /// Connects to the isolated maintenance database and proves basic access.
    ///
    /// # Errors
    ///
    /// Returns [`SpikePostgresError`] for invalid local configuration or a
    /// failed `PostgreSQL` probe.
    pub async fn connect(config: SpikePostgresConfig) -> Result<Self, SpikePostgresError> {
        validate_role_name(&config.admin_role)?;
        if config.endpoint.port() == 0 || config.password.is_empty() {
            return Err(SpikePostgresError::InvalidConfiguration);
        }
        let postgres = Self { config };
        let mut session = postgres.connect_database("postgres").await?;
        let probe = session
            .client()
            .query_one(
                "SELECT 1::integer, current_setting('cluster_name'), current_user",
                &[],
            )
            .await;
        session.close().await?;
        let row = probe?;
        if row.get::<_, i32>(0) != 1
            || row.get::<_, &str>(1) != EXPECTED_CLUSTER_NAME
            || row.get::<_, &str>(2) != postgres.config.admin_role
        {
            return Err(SpikePostgresError::UnexpectedServerIdentity);
        }
        Ok(postgres)
    }

    /// Creates a sealed template and an isolated mutable case database.
    ///
    /// # Errors
    ///
    /// Returns [`SpikePostgresError`] when naming, DDL, marker insertion,
    /// sealing, cloning, or identity observation fails.
    pub async fn provision_reference_databases(
        &self,
        suffix: &str,
        compose_project: ComposeProjectId,
    ) -> Result<ReferenceProvisioning, SpikePostgresError> {
        let baseline_name = DatabaseName::parse(format!("tiv_base_{suffix}"))?;
        let case_name = DatabaseName::parse(format!("tiv_case_{suffix}"))?;
        let baseline_marker = Uuid::new_v4();
        let case_marker = Uuid::new_v4();

        self.create_empty_database(&baseline_name).await?;
        self.initialize_reference_baseline(&baseline_name, baseline_marker, &compose_project)
            .await?;
        self.seal_and_clone_baseline(
            &baseline_name,
            &case_name,
            baseline_marker,
            &compose_project,
        )
        .await?;
        let baseline_target = self.observe_baseline_identity(&baseline_name).await?;
        if !baseline_target.is_sealed()
            || baseline_target.identity.marker().marker_uuid() != baseline_marker
            || baseline_target.identity.marker().kind() != MarkerKind::Baseline
            || baseline_target.identity.marker().compose_project() != &compose_project
        {
            return Err(SpikePostgresError::BaselineIdentityMismatch);
        }
        self.replace_case_marker(&case_name, case_marker).await?;

        let identity = self.observe_identity(&case_name).await?;
        if identity.marker().compose_project() != &compose_project
            || identity.marker().marker_uuid() != case_marker
            || identity.marker().kind() != MarkerKind::Case
        {
            return Err(SpikePostgresError::PostResetIdentityMismatch);
        }
        Ok(ReferenceProvisioning {
            baseline_target,
            case_name,
            case_target: DatabaseTarget::new(identity),
        })
    }

    /// Inserts the narrow two-provider-object state produced by the buggy app.
    ///
    /// # Errors
    ///
    /// Returns [`SpikePostgresError`] unless exactly two distinct provider
    /// objects are supplied and both local rows commit atomically.
    pub async fn insert_buggy_payment_pair(
        &self,
        case_name: &DatabaseName,
        operation_id: &str,
        provider_objects: &[ProviderPaymentIntent],
    ) -> Result<(), SpikePostgresError> {
        if case_name.kind() != DatabaseKind::Case
            || operation_id.trim().is_empty()
            || provider_objects.len() != 2
            || provider_objects[0].id() == provider_objects[1].id()
        {
            return Err(SpikePostgresError::InvalidBugState);
        }
        let mut session = self.connect_database(case_name.as_str()).await?;
        let result = async {
            let transaction = session.client().transaction().await?;
            for provider in provider_objects {
                transaction
                    .execute(
                        "INSERT INTO payments \
                             (operation_id, stripe_payment_intent_id, amount_minor, currency, status) \
                         VALUES ($1, $2, $3, $4, 'pending')",
                        &[
                            &operation_id,
                            &provider.id(),
                            &provider.amount_minor(),
                            &provider.currency(),
                        ],
                    )
                    .await?;
            }
            transaction.commit().await
        }
        .await;
        session.close().await?;
        result?;
        Ok(())
    }

    /// Runs the exact five-query reference snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`SpikePostgresError`] when projection loading, transaction
    /// setup, a bounded invariant query, rollback, or connection shutdown
    /// fails.
    pub async fn check_reference_invariants(
        &self,
        case_name: &DatabaseName,
        provider_objects: &[ProviderPaymentIntent],
        quiescence: QuiescencePermit,
    ) -> Result<SnapshotReport, SpikePostgresError> {
        let mut session = self.connect_database(case_name.as_str()).await?;
        let result = run_reference_oracle(session.client(), provider_objects, quiescence)
            .await
            .map_err(SpikePostgresError::Oracle);
        session.close().await?;
        result
    }

    /// Rechecks the exact case identity, consumes a mutation permit, drops only
    /// that case database, and clones the sealed baseline.
    ///
    /// # Errors
    ///
    /// Returns [`SpikePostgresError`] when the fresh identity differs, the
    /// source is not a sealed baseline name, or any scoped reset step fails.
    pub async fn reset_case_from_template(
        &self,
        expected: DatabaseTarget<Unverified>,
        expected_baseline: &BaselineTarget,
        new_marker_uuid: Uuid,
    ) -> Result<DatabaseTarget<Unverified>, SpikePostgresError> {
        if expected_baseline.identity.database_name().kind() != DatabaseKind::Baseline
            || !expected_baseline.is_sealed()
        {
            return Err(SpikePostgresError::ExpectedBaselineName);
        }
        let case_name = expected.identity().database_name().clone();
        let observed = self.observe_identity(&case_name).await?;
        let (verified, permit) = expected
            .verify(&observed)
            .map_err(SpikePostgresError::Safety)?;
        let observed_baseline = self
            .observe_baseline_identity(expected_baseline.identity.database_name())
            .await?;
        if &observed_baseline != expected_baseline
            || observed_baseline.identity.server_fingerprint()
                != verified.identity().server_fingerprint()
            || observed_baseline.identity.endpoint() != verified.identity().endpoint()
            || observed_baseline.identity.marker().compose_project()
                != verified.identity().marker().compose_project()
        {
            return Err(SpikePostgresError::BaselineIdentityMismatch);
        }
        self.reset_verified_case(
            verified,
            permit,
            expected_baseline.identity.database_name(),
            new_marker_uuid,
        )
        .await
    }

    async fn create_empty_database(
        &self,
        database_name: &DatabaseName,
    ) -> Result<(), SpikePostgresError> {
        let mut session = self.connect_database("postgres").await?;
        let sql = format!(
            "CREATE DATABASE {} WITH OWNER = {}",
            database_name.as_str(),
            self.config.admin_role
        );
        let result = session.client().batch_execute(&sql).await;
        session.close().await?;
        result?;
        Ok(())
    }

    async fn initialize_reference_baseline(
        &self,
        baseline_name: &DatabaseName,
        marker_uuid: Uuid,
        compose_project: &ComposeProjectId,
    ) -> Result<(), SpikePostgresError> {
        let mut session = self.connect_database(baseline_name.as_str()).await?;
        let result = async {
            session
                .client()
                .batch_execute(
                    "CREATE TABLE tiv_verifier_marker ( \
                         singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton), \
                         marker_uuid uuid NOT NULL, \
                         marker_kind text NOT NULL CHECK (marker_kind IN ('baseline', 'case')), \
                         compose_project text NOT NULL, \
                         application_role text NOT NULL \
                     ); \
                     CREATE TABLE orders ( \
                         id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                         operation_id text NOT NULL UNIQUE, \
                         amount_minor bigint NOT NULL CHECK (amount_minor > 0), \
                         currency text NOT NULL CHECK (currency ~ '^[a-z]{3}$'), \
                         status text NOT NULL CHECK (status IN ('pending', 'paid')) \
                     ); \
                     CREATE TABLE payments ( \
                         id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, \
                         operation_id text NOT NULL REFERENCES orders(operation_id), \
                         stripe_payment_intent_id text NOT NULL, \
                         amount_minor bigint NOT NULL CHECK (amount_minor > 0), \
                         currency text NOT NULL CHECK (currency ~ '^[a-z]{3}$'), \
                         status text NOT NULL CHECK (status IN ('pending', 'succeeded')) \
                     ); \
                     CREATE INDEX payments_operation_id_idx ON payments (operation_id); \
                     CREATE INDEX payments_provider_id_idx ON payments (stripe_payment_intent_id); \
                     INSERT INTO orders (operation_id, amount_minor, currency, status) \
                     VALUES ('op_1', 2500, 'usd', 'pending');",
                )
                .await?;
            session
                .client()
                .execute(
                    "INSERT INTO tiv_verifier_marker \
                         (marker_uuid, marker_kind, compose_project, application_role) \
                     VALUES ($1, 'baseline', $2, $3)",
                    &[&marker_uuid, &compose_project.as_str(), &APPLICATION_ROLE],
                )
                .await?;
            Ok::<(), tokio_postgres::Error>(())
        }
        .await;
        session.close().await?;
        result?;
        Ok(())
    }

    async fn seal_and_clone_baseline(
        &self,
        baseline_name: &DatabaseName,
        case_name: &DatabaseName,
        marker_uuid: Uuid,
        compose_project: &ComposeProjectId,
    ) -> Result<(), SpikePostgresError> {
        let mut session = self.connect_database("postgres").await?;
        let catalog_marker = format!(
            "tiv-baseline:v1:{marker_uuid}:{}:{APPLICATION_ROLE}",
            compose_project.as_str()
        );
        let comment = format!(
            "COMMENT ON DATABASE {} IS '{}'",
            baseline_name.as_str(),
            catalog_marker
        );
        let mark_template = format!("ALTER DATABASE {} IS_TEMPLATE true", baseline_name.as_str());
        let seal_connections = format!(
            "ALTER DATABASE {} ALLOW_CONNECTIONS false",
            baseline_name.as_str()
        );
        let clone = format!(
            "CREATE DATABASE {} WITH OWNER = {} TEMPLATE = {}",
            case_name.as_str(),
            self.config.admin_role,
            baseline_name.as_str()
        );
        let result = async {
            session.client().batch_execute(&comment).await?;
            session.client().batch_execute(&mark_template).await?;
            session.client().batch_execute(&seal_connections).await?;
            session.client().batch_execute(&clone).await
        }
        .await;
        session.close().await?;
        result?;
        Ok(())
    }

    async fn replace_case_marker(
        &self,
        case_name: &DatabaseName,
        marker_uuid: Uuid,
    ) -> Result<(), SpikePostgresError> {
        let mut session = self.connect_database(case_name.as_str()).await?;
        let result = session
            .client()
            .execute(
                "UPDATE tiv_verifier_marker \
                 SET marker_uuid = $1, marker_kind = 'case'",
                &[&marker_uuid],
            )
            .await;
        session.close().await?;
        if result? != 1 {
            return Err(SpikePostgresError::UnexpectedMarkerRows);
        }
        Ok(())
    }

    async fn observe_identity(
        &self,
        database_name: &DatabaseName,
    ) -> Result<DatabaseIdentity, SpikePostgresError> {
        let mut maintenance = self.connect_database("postgres").await?;
        let catalog_result = maintenance
            .client()
            .query_opt(
                "SELECT database.oid::bigint, database.datdba::bigint, \
                        control.system_identifier::text \
                 FROM pg_database AS database \
                 CROSS JOIN pg_control_system() AS control \
                 WHERE database.datname = $1",
                &[&database_name.as_str()],
            )
            .await;
        maintenance.close().await?;
        let catalog = catalog_result?.ok_or(SpikePostgresError::DatabaseNotFound)?;
        let database_oid = u32::try_from(catalog.get::<_, i64>(0))
            .map_err(|_| SpikePostgresError::InvalidCatalogIdentity)?;
        let owner_oid = u32::try_from(catalog.get::<_, i64>(1))
            .map_err(|_| SpikePostgresError::InvalidCatalogIdentity)?;
        let system_identifier: String = catalog.get(2);

        let mut target = self.connect_database(database_name.as_str()).await?;
        let marker_result = target
            .client()
            .query(
                "SELECT marker_uuid, marker_kind, compose_project, application_role \
                 FROM tiv_verifier_marker \
                 LIMIT 2",
                &[],
            )
            .await;
        target.close().await?;
        let marker_rows = marker_result?;
        if marker_rows.len() != 1 {
            return Err(SpikePostgresError::UnexpectedMarkerRows);
        }
        let marker_row = &marker_rows[0];
        let marker_uuid = marker_row.get::<_, Uuid>(0);
        let marker_kind = match marker_row.get::<_, &str>(1) {
            "baseline" => MarkerKind::Baseline,
            "case" => MarkerKind::Case,
            _ => return Err(SpikePostgresError::InvalidMarkerKind),
        };
        let compose_project = ComposeProjectId::new(marker_row.get::<_, &str>(2))
            .map_err(|_| SpikePostgresError::InvalidCatalogIdentity)?;
        let application_role = marker_row.get::<_, String>(3);

        DatabaseIdentity::new(
            format!("postgres-system-id:{system_identifier}"),
            self.config.endpoint,
            database_name.clone(),
            database_oid,
            owner_oid,
            DatabaseMarker::new(marker_uuid, marker_kind, compose_project),
            application_role,
        )
        .map_err(SpikePostgresError::InvalidDatabaseIdentity)
    }

    async fn observe_baseline_identity(
        &self,
        database_name: &DatabaseName,
    ) -> Result<BaselineTarget, SpikePostgresError> {
        let mut maintenance = self.connect_database("postgres").await?;
        let catalog_result = maintenance
            .client()
            .query_opt(
                "SELECT database.oid::bigint, database.datdba::bigint, \
                        control.system_identifier::text, database.datistemplate, \
                        database.datallowconn, \
                        COALESCE(shobj_description(database.oid, 'pg_database'), '') \
                 FROM pg_database AS database \
                 CROSS JOIN pg_control_system() AS control \
                 WHERE database.datname = $1",
                &[&database_name.as_str()],
            )
            .await;
        maintenance.close().await?;
        let catalog = catalog_result?.ok_or(SpikePostgresError::DatabaseNotFound)?;
        let database_oid = u32::try_from(catalog.get::<_, i64>(0))
            .map_err(|_| SpikePostgresError::InvalidCatalogIdentity)?;
        let owner_oid = u32::try_from(catalog.get::<_, i64>(1))
            .map_err(|_| SpikePostgresError::InvalidCatalogIdentity)?;
        let system_identifier = catalog.get::<_, String>(2);
        let is_template = catalog.get::<_, bool>(3);
        let allows_connections = catalog.get::<_, bool>(4);
        let (marker_uuid, compose_project, application_role) =
            parse_baseline_catalog_marker(catalog.get::<_, &str>(5))?;
        let identity = DatabaseIdentity::new(
            format!("postgres-system-id:{system_identifier}"),
            self.config.endpoint,
            database_name.clone(),
            database_oid,
            owner_oid,
            DatabaseMarker::new(marker_uuid, MarkerKind::Baseline, compose_project),
            application_role,
        )
        .map_err(SpikePostgresError::InvalidDatabaseIdentity)?;
        Ok(BaselineTarget {
            identity,
            is_template,
            allows_connections,
        })
    }

    async fn reset_verified_case(
        &self,
        verified: DatabaseTarget<Verified>,
        _permit: MutationPermit,
        baseline_name: &DatabaseName,
        new_marker_uuid: Uuid,
    ) -> Result<DatabaseTarget<Unverified>, SpikePostgresError> {
        let prior = verified.identity();
        let case_name = prior.database_name().clone();
        let prior_fingerprint = prior.server_fingerprint().to_owned();
        let prior_endpoint = prior.endpoint();
        let prior_owner_oid = prior.owner_oid();
        let compose_project = prior.marker().compose_project().clone();
        let application_role = prior.expected_application_role().to_owned();
        let database_oid = i64::from(prior.database_oid());

        let mut maintenance = self.connect_database("postgres").await?;
        let drop_case = format!("DROP DATABASE {}", case_name.as_str());
        let clone_case = format!(
            "CREATE DATABASE {} WITH OWNER = {} TEMPLATE = {}",
            case_name.as_str(),
            self.config.admin_role,
            baseline_name.as_str()
        );
        let reset_result = async {
            maintenance
                .client()
                .execute(
                    "SELECT pg_terminate_backend(pid) \
                     FROM pg_stat_activity \
                     WHERE datid::bigint = $1 AND pid <> pg_backend_pid()",
                    &[&database_oid],
                )
                .await?;
            maintenance.client().batch_execute(&drop_case).await?;
            maintenance.client().batch_execute(&clone_case).await
        }
        .await;
        maintenance.close().await?;
        reset_result?;

        self.replace_case_marker(&case_name, new_marker_uuid)
            .await?;
        let observed = self.observe_identity(&case_name).await?;
        if observed.server_fingerprint() != prior_fingerprint
            || observed.endpoint() != prior_endpoint
            || observed.owner_oid() != prior_owner_oid
            || observed.marker().marker_uuid() != new_marker_uuid
            || observed.marker().kind() != MarkerKind::Case
            || observed.marker().compose_project() != &compose_project
            || observed.expected_application_role() != application_role
        {
            return Err(SpikePostgresError::PostResetIdentityMismatch);
        }
        Ok(DatabaseTarget::new(observed))
    }

    async fn connect_database(
        &self,
        database_name: &str,
    ) -> Result<PostgresSession, SpikePostgresError> {
        let mut config = tokio_postgres::Config::new();
        config
            .host("127.0.0.1")
            .port(self.config.endpoint.port())
            .user(&self.config.admin_role)
            .password(&self.config.password)
            .dbname(database_name);
        let (client, connection) = config.connect(NoTls).await?;
        let connection = tokio::spawn(connection);
        Ok(PostgresSession {
            client: Some(client),
            connection: Some(connection),
        })
    }
}

#[derive(Debug)]
pub struct ReferenceProvisioning {
    baseline_target: BaselineTarget,
    case_name: DatabaseName,
    case_target: DatabaseTarget<Unverified>,
}

impl ReferenceProvisioning {
    #[must_use]
    pub const fn baseline_target(&self) -> &BaselineTarget {
        &self.baseline_target
    }

    #[must_use]
    pub const fn case_name(&self) -> &DatabaseName {
        &self.case_name
    }

    #[must_use]
    pub const fn case_target(&self) -> &DatabaseTarget<Unverified> {
        &self.case_target
    }

    #[must_use]
    pub fn into_case_target(self) -> DatabaseTarget<Unverified> {
        self.case_target
    }
}

struct PostgresSession {
    client: Option<Client>,
    connection: Option<JoinHandle<Result<(), tokio_postgres::Error>>>,
}

impl PostgresSession {
    fn client(&mut self) -> &mut Client {
        self.client.as_mut().expect("an open session has a client")
    }

    async fn close(mut self) -> Result<(), SpikePostgresError> {
        drop(self.client.take());
        if let Some(connection) = self.connection.take() {
            connection.await??;
        }
        Ok(())
    }
}

impl Drop for PostgresSession {
    fn drop(&mut self) {
        if let Some(connection) = &self.connection {
            connection.abort();
        }
    }
}

#[derive(Debug, Error)]
pub enum SpikePostgresError {
    #[error("invalid isolated PostgreSQL configuration")]
    InvalidConfiguration,
    #[error("PostgreSQL operation failed")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("PostgreSQL connection task failed")]
    ConnectionTask(#[from] JoinError),
    #[error("invalid generated database name")]
    InvalidDatabaseName,
    #[error("invalid database identity")]
    InvalidDatabaseIdentity(InvalidDatabaseIdentity),
    #[error("database safety preflight failed: {0:?}")]
    Safety(SafetyError),
    #[error("database was not found during identity observation")]
    DatabaseNotFound,
    #[error("database catalog identity could not be represented")]
    InvalidCatalogIdentity,
    #[error("database marker row count was not exactly one")]
    UnexpectedMarkerRows,
    #[error("database marker kind is unsupported")]
    InvalidMarkerKind,
    #[error("template reset requires a generated baseline name")]
    ExpectedBaselineName,
    #[error("post-reset database identity did not match the scoped run")]
    PostResetIdentityMismatch,
    #[error("sealed baseline identity did not match the scoped run")]
    BaselineIdentityMismatch,
    #[error("the synthetic bug requires two distinct provider objects")]
    InvalidBugState,
    #[error("PostgreSQL server is not the isolated truth-spike cluster")]
    UnexpectedServerIdentity,
    #[error(transparent)]
    InvalidUuid(#[from] uuid::Error),
    #[error("reference invariant oracle failed: {0}")]
    Oracle(OracleError),
}

impl From<InvalidDatabaseName> for SpikePostgresError {
    fn from(_: InvalidDatabaseName) -> Self {
        Self::InvalidDatabaseName
    }
}

fn validate_role_name(role: &str) -> Result<(), SpikePostgresError> {
    let mut bytes = role.bytes();
    let Some(first) = bytes.next() else {
        return Err(SpikePostgresError::InvalidConfiguration);
    };
    if role.len() > 63
        || !(first.is_ascii_lowercase() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(SpikePostgresError::InvalidConfiguration);
    }
    Ok(())
}

fn parse_baseline_catalog_marker(
    marker: &str,
) -> Result<(Uuid, ComposeProjectId, String), SpikePostgresError> {
    let parts = marker.split(':').collect::<Vec<_>>();
    let [
        prefix,
        version,
        marker_uuid,
        compose_project,
        application_role,
    ] = parts.as_slice()
    else {
        return Err(SpikePostgresError::BaselineIdentityMismatch);
    };
    if *prefix != "tiv-baseline" || *version != "v1" {
        return Err(SpikePostgresError::BaselineIdentityMismatch);
    }
    let marker_uuid = Uuid::parse_str(marker_uuid)?;
    let compose_project = ComposeProjectId::new(*compose_project)
        .map_err(|_| SpikePostgresError::BaselineIdentityMismatch)?;
    validate_role_name(application_role)?;
    Ok((marker_uuid, compose_project, (*application_role).to_owned()))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use reqwest::StatusCode;
    use tiv_core::decision::Seed;
    use tiv_stripe_pi::{FaultOutcome, PaymentIntentFixture, http::serve_http1_connection};
    use tokio::{net::TcpListener, sync::Mutex, time::timeout};

    use super::*;
    use crate::{
        evidence::TruthSpikeEvidence,
        postgres::oracle::{InvariantOutcome, InvariantVerdict},
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn template_reset_and_five_query_oracle_reproduce_the_same_failure() {
        let postgres = test_postgres().await;
        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let provisioned = postgres
            .provision_reference_databases(&suffix, test_project())
            .await
            .expect("the baseline and first case are provisioned");
        let baseline_target = provisioned.baseline_target().clone();
        let case_name = provisioned.case_name().clone();
        let original_oid = provisioned.case_target().identity().database_oid();
        let provider_objects = provider_objects();

        postgres
            .insert_buggy_payment_pair(&case_name, "op_1", &provider_objects)
            .await
            .expect("the synthetic bug state is inserted");
        let first_report = postgres
            .check_reference_invariants(&case_name, &provider_objects, quiescence())
            .await
            .expect("the five-query snapshot completes");
        assert_eq!(first_report.outcomes().len(), 5);
        let first_identity = provider_uniqueness_failure(&first_report)
            .identity()
            .clone();

        let reset_target = postgres
            .reset_case_from_template(
                provisioned.into_case_target(),
                &baseline_target,
                Uuid::new_v4(),
            )
            .await
            .expect("a fresh identity check authorizes the template reset");
        assert_ne!(reset_target.identity().database_oid(), original_oid);
        let clean_report = postgres
            .check_reference_invariants(&case_name, &provider_objects, quiescence())
            .await
            .expect("the clean baseline snapshot completes");
        assert!(matches!(
            clean_report
                .outcome("provider-object-unique")
                .expect("the invariant ran")
                .verdict(),
            InvariantVerdict::Held
        ));

        postgres
            .insert_buggy_payment_pair(&case_name, "op_1", &provider_objects)
            .await
            .expect("the same compiled fault is replayed");
        let replay_report = postgres
            .check_reference_invariants(&case_name, &provider_objects, quiescence())
            .await
            .expect("the replay snapshot completes");
        assert_eq!(
            provider_uniqueness_failure(&replay_report).identity(),
            &first_identity
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn full_commit_close_reset_and_replay_chain_has_one_failure_identity() {
        let postgres = test_postgres().await;
        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let provisioned = postgres
            .provision_reference_databases(&suffix, test_project())
            .await
            .expect("the baseline and first case are provisioned");
        let baseline_target = provisioned.baseline_target().clone();
        let case_name = provisioned.case_name().clone();
        let first_database_oid = provisioned.case_target().identity().database_oid();

        let first_report = run_buggy_checkout(&postgres, &case_name).await;
        let first_identity = provider_uniqueness_failure(&first_report)
            .identity()
            .clone();
        let reset_target = postgres
            .reset_case_from_template(
                provisioned.into_case_target(),
                &baseline_target,
                Uuid::new_v4(),
            )
            .await
            .expect("the marked case resets from the sealed template");
        let reset_database_oid = reset_target.identity().database_oid();
        let replay_report = run_buggy_checkout(&postgres, &case_name).await;
        let replay_identity = provider_uniqueness_failure(&replay_report).identity();

        let evidence = TruthSpikeEvidence::new(
            2,
            first_database_oid,
            reset_database_oid,
            &first_identity,
            replay_identity,
        )
        .expect("the observed chain forms coherent bounded evidence");
        let encoded = evidence
            .to_pretty_json()
            .expect("the evidence document serializes");
        assert!(encoded.contains("provider-object-unique"));
        assert!(!encoded.contains("tiv-local-only-password"));
    }

    async fn test_postgres() -> TruthSpikePostgres {
        let port = std::env::var("TIV_POSTGRES_TEST_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(15_432);
        TruthSpikePostgres::connect(SpikePostgresConfig::loopback(
            port,
            "tiv_admin",
            "tiv-local-only-password",
        ))
        .await
        .expect("the isolated PostgreSQL fixture is healthy")
    }

    fn test_project() -> ComposeProjectId {
        ComposeProjectId::new("tiv-truth-spike-postgres").expect("the test project name is valid")
    }

    fn provider_objects() -> [ProviderPaymentIntent; 2] {
        [
            ProviderPaymentIntent::new("pi_tiv_first", 2_500, "usd", "requires_confirmation")
                .expect("the first provider projection is valid"),
            ProviderPaymentIntent::new("pi_tiv_retry", 2_500, "usd", "requires_confirmation")
                .expect("the retry provider projection is valid"),
        ]
    }

    async fn run_buggy_checkout(
        postgres: &TruthSpikePostgres,
        case_name: &DatabaseName,
    ) -> SnapshotReport {
        let fixture = Arc::new(Mutex::new(PaymentIntentFixture::new(Seed::new(42))));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback provider port is available");
        let address = listener.local_addr().expect("the listener has an address");
        let server_fixture = Arc::clone(&fixture);
        let server = tokio::spawn(async move {
            for outcome in [FaultOutcome::CommitThenClose, FaultOutcome::Normal] {
                let (stream, _) = listener.accept().await.expect("the app connects");
                let _result =
                    serve_http1_connection(stream, Arc::clone(&server_fixture), outcome).await;
            }
        });

        let client = reqwest::Client::new();
        let url = format!("http://{address}/v1/payment_intents");
        let form = [("amount", "2500"), ("currency", "usd")];
        let first = client
            .post(&url)
            .header("Idempotency-Key", "op-1-attempt-1")
            .form(&form)
            .send()
            .await;
        assert!(
            first.is_err(),
            "the provider committed but returned no response"
        );
        let retry = client
            .post(&url)
            .header("Idempotency-Key", "op-1-attempt-2")
            .form(&form)
            .send()
            .await
            .expect("the changed-key retry receives a response");
        assert_eq!(retry.status(), StatusCode::OK);
        let _body = retry.bytes().await.expect("the retry body is readable");
        drop(client);
        timeout(Duration::from_secs(2), server)
            .await
            .expect("the provider server stops")
            .expect("the provider task does not panic");

        let provider_objects = {
            let fixture = fixture.lock().await;
            assert_eq!(fixture.payment_intent_count(), 2);
            fixture
                .payment_intents()
                .iter()
                .map(|payment_intent| {
                    ProviderPaymentIntent::new(
                        payment_intent.id(),
                        2_500,
                        "usd",
                        "requires_confirmation",
                    )
                    .expect("fixture IDs are valid provider projection IDs")
                })
                .collect::<Vec<_>>()
        };
        postgres
            .insert_buggy_payment_pair(case_name, "op_1", &provider_objects)
            .await
            .expect("reconciliation persists both provider objects for one operation");
        postgres
            .check_reference_invariants(case_name, &provider_objects, quiescence())
            .await
            .expect("the five-query snapshot completes")
    }

    fn provider_uniqueness_failure(report: &SnapshotReport) -> &InvariantOutcome {
        let outcome = report
            .outcome("provider-object-unique")
            .expect("the named invariant ran");
        assert!(matches!(outcome.verdict(), InvariantVerdict::Violated(_)));
        outcome
    }

    fn quiescence() -> QuiescencePermit {
        QuiescencePermit::after_synthetic_driver_stopped()
    }
}
