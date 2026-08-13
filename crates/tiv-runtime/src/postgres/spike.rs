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
    admin_password: String,
    application_password: String,
}

impl SpikePostgresConfig {
    #[must_use]
    pub fn loopback(
        port: u16,
        admin_role: impl Into<String>,
        admin_password: impl Into<String>,
        application_password: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: DatabaseEndpoint::loopback(port),
            admin_role: admin_role.into(),
            admin_password: admin_password.into(),
            application_password: application_password.into(),
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
        if config.endpoint.port() == 0
            || config.admin_password.is_empty()
            || config.application_password.is_empty()
        {
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

        self.ensure_application_role().await?;
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

    /// Freshly verifies the marked case, then inserts the narrow
    /// two-provider-object state produced by the buggy app.
    ///
    /// # Errors
    ///
    /// Returns [`SpikePostgresError`] unless the expected case identity still
    /// matches, exactly two distinct provider objects are supplied, and both
    /// local rows commit atomically. The returned target must be freshly
    /// verified again before another persistent mutation.
    pub async fn insert_buggy_payment_pair(
        &self,
        expected: DatabaseTarget<Unverified>,
        operation_id: &str,
        provider_objects: &[ProviderPaymentIntent],
    ) -> Result<DatabaseTarget<Unverified>, SpikePostgresError> {
        if expected.identity().database_name().kind() != DatabaseKind::Case
            || operation_id.trim().is_empty()
            || provider_objects.len() != 2
            || provider_objects[0].id() == provider_objects[1].id()
        {
            return Err(SpikePostgresError::InvalidBugState);
        }
        let case_name = expected.identity().database_name().clone();
        let observed = self.observe_identity(&case_name).await?;
        let (verified, permit) = expected
            .verify(&observed)
            .map_err(SpikePostgresError::Safety)?;
        self.insert_verified_buggy_payment_pair(verified, permit, operation_id, provider_objects)
            .await
    }

    async fn insert_verified_buggy_payment_pair(
        &self,
        verified: DatabaseTarget<Verified>,
        _permit: MutationPermit,
        operation_id: &str,
        provider_objects: &[ProviderPaymentIntent],
    ) -> Result<DatabaseTarget<Unverified>, SpikePostgresError> {
        let expected_after_mutation = verified.identity().clone();
        let case_name = expected_after_mutation.database_name().clone();
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
        Ok(DatabaseTarget::new(expected_after_mutation))
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

    async fn ensure_application_role(&self) -> Result<(), SpikePostgresError> {
        let mut session = self.connect_database("postgres").await?;
        let result = async {
            let exists = session
                .client()
                .query_opt(
                    "SELECT 1::integer FROM pg_roles WHERE rolname = $1",
                    &[&APPLICATION_ROLE],
                )
                .await?
                .is_some();
            if !exists {
                session
                    .client()
                    .batch_execute(
                        "CREATE ROLE tiv_app WITH \
                             LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT \
                             NOREPLICATION NOBYPASSRLS",
                    )
                    .await?;
            }
            session
                .client()
                .batch_execute(
                    "ALTER ROLE tiv_app WITH \
                         LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT \
                         NOREPLICATION NOBYPASSRLS",
                )
                .await?;
            let quoted_password = session
                .client()
                .query_one(
                    "SELECT quote_literal($1::text)",
                    &[&self.config.application_password],
                )
                .await?
                .get::<_, String>(0);
            session
                .client()
                .batch_execute(&format!(
                    "ALTER ROLE {APPLICATION_ROLE} PASSWORD {quoted_password}"
                ))
                .await
        }
        .await;
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
                     VALUES ('op_1', 2500, 'usd', 'pending'); \
                     REVOKE ALL ON SCHEMA public FROM PUBLIC; \
                     GRANT USAGE ON SCHEMA public TO tiv_app; \
                     REVOKE ALL ON TABLE tiv_verifier_marker, orders, payments \
                         FROM PUBLIC, tiv_app; \
                     REVOKE ALL ON SEQUENCE orders_id_seq, payments_id_seq \
                         FROM PUBLIC, tiv_app; \
                     GRANT INSERT ON TABLE payments TO tiv_app; \
                     GRANT SELECT (operation_id, stripe_payment_intent_id), \
                           UPDATE (status) ON TABLE payments TO tiv_app; \
                     GRANT USAGE ON SEQUENCE payments_id_seq TO tiv_app;",
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
            session.client().batch_execute(&clone).await?;
            self.configure_case_connect(session.client(), case_name)
                .await
        }
        .await;
        session.close().await?;
        result?;
        Ok(())
    }

    async fn configure_case_connect(
        &self,
        client: &Client,
        case_name: &DatabaseName,
    ) -> Result<(), tokio_postgres::Error> {
        client
            .batch_execute(&format!(
                "REVOKE ALL ON DATABASE {} FROM PUBLIC; \
                 REVOKE ALL ON DATABASE {} FROM {APPLICATION_ROLE}; \
                 GRANT CONNECT ON DATABASE {} TO {APPLICATION_ROLE}",
                case_name.as_str(),
                case_name.as_str(),
                case_name.as_str(),
            ))
            .await
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
            maintenance.client().batch_execute(&clone_case).await?;
            self.configure_case_connect(maintenance.client(), &case_name)
                .await
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
        self.connect_database_as(
            database_name,
            &self.config.admin_role,
            &self.config.admin_password,
        )
        .await
    }

    async fn connect_application_database(
        &self,
        database_name: &DatabaseName,
    ) -> Result<PostgresSession, SpikePostgresError> {
        if database_name.kind() != DatabaseKind::Case {
            return Err(SpikePostgresError::InvalidConfiguration);
        }
        self.connect_database_as(
            database_name.as_str(),
            APPLICATION_ROLE,
            &self.config.application_password,
        )
        .await
    }

    async fn connect_database_as(
        &self,
        database_name: &str,
        role: &str,
        password: &str,
    ) -> Result<PostgresSession, SpikePostgresError> {
        let mut config = tokio_postgres::Config::new();
        config
            .host("127.0.0.1")
            .port(self.config.endpoint.port())
            .user(role)
            .password(password)
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
    use tiv_core::{decision::Seed, trace::CompiledTrace};
    use tiv_stripe_pi::{FaultOutcome, PaymentIntentFixture, http::serve_http1_connection};

    use crate::replay::{ReferenceReplayScript, ReferenceReplayScriptError, ReplayPlan};
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

        let stale_case_target = DatabaseTarget::new(provisioned.case_target().identity().clone());
        let dirty_case_target = postgres
            .insert_buggy_payment_pair(provisioned.into_case_target(), "op_1", &provider_objects)
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
            .reset_case_from_template(dirty_case_target, &baseline_target, Uuid::new_v4())
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

        let stale_write = postgres
            .insert_buggy_payment_pair(stale_case_target, "op_1", &provider_objects)
            .await;
        assert!(matches!(
            stale_write,
            Err(SpikePostgresError::Safety(SafetyError::IdentityMismatch(
                crate::postgres::safety::IdentityField::DatabaseOid
            )))
        ));
        let still_clean_report = postgres
            .check_reference_invariants(&case_name, &provider_objects, quiescence())
            .await
            .expect("the rejected stale write leaves the reset baseline inspectable");
        assert!(matches!(
            still_clean_report
                .outcome("provider-object-unique")
                .expect("the invariant ran")
                .verdict(),
            InvariantVerdict::Held
        ));

        let _replayed_case_target = postgres
            .insert_buggy_payment_pair(reset_target, "op_1", &provider_objects)
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
        let first_database_oid = provisioned.case_target().identity().database_oid();

        let (first_report, dirty_case_target) =
            run_buggy_checkout(&postgres, provisioned.into_case_target()).await;
        let first_identity = provider_uniqueness_failure(&first_report)
            .identity()
            .clone();
        let reset_target = postgres
            .reset_case_from_template(dirty_case_target, &baseline_target, Uuid::new_v4())
            .await
            .expect("the marked case resets from the sealed template");
        let reset_database_oid = reset_target.identity().database_oid();
        let (replay_report, _replayed_case_target) =
            run_buggy_checkout(&postgres, reset_target).await;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn intended_application_role_has_only_the_required_case_write_capability() {
        let postgres = test_postgres().await;
        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let provisioned = postgres
            .provision_reference_databases(&suffix, test_project())
            .await
            .expect("the baseline and first case are provisioned");
        let baseline_target = provisioned.baseline_target().clone();
        let case_name = provisioned.case_name().clone();
        assert_application_role_contract(&postgres, &case_name).await;

        let reset_target = postgres
            .reset_case_from_template(
                provisioned.into_case_target(),
                &baseline_target,
                Uuid::new_v4(),
            )
            .await
            .expect("the marked case resets from the template");
        assert_application_role_after_reset(&postgres, reset_target.identity().database_name())
            .await;
    }

    async fn assert_application_role_contract(
        postgres: &TruthSpikePostgres,
        case_name: &DatabaseName,
    ) {
        let mut app = postgres
            .connect_application_database(case_name)
            .await
            .expect("the intended application role can connect to the case");

        let current_user = app
            .client()
            .query_one("SELECT current_user", &[])
            .await
            .expect("the role identity is observable")
            .get::<_, String>(0);
        let inserted = app
            .client()
            .execute(
                "INSERT INTO payments \
                     (operation_id, stripe_payment_intent_id, amount_minor, currency, status) \
                 VALUES ('op_1', 'pi_tiv_role_test', 2500, 'usd', 'succeeded')",
                &[],
            )
            .await
            .expect("the app can persist its one required relation");
        let marker_read = app
            .client()
            .query("SELECT marker_uuid FROM tiv_verifier_marker", &[])
            .await;
        let reconciled = app
            .client()
            .execute(
                "UPDATE payments SET status = 'succeeded' \
                 WHERE operation_id = 'op_1' \
                   AND stripe_payment_intent_id = 'pi_tiv_role_test'",
                &[],
            )
            .await
            .expect("the app can reconcile the exact provider relation it wrote");
        let marker_write = app
            .client()
            .execute(
                "UPDATE tiv_verifier_marker SET application_role = 'tampered'",
                &[],
            )
            .await;
        let schema_write = app
            .client()
            .batch_execute("CREATE TABLE unauthorized_app_table (id integer)")
            .await;
        let temporary_write = app
            .client()
            .batch_execute("CREATE TEMP TABLE unauthorized_temp_table (id integer)")
            .await;
        let payment_delete = app
            .client()
            .execute(
                "DELETE FROM payments \
                 WHERE stripe_payment_intent_id = 'pi_tiv_role_test'",
                &[],
            )
            .await;
        let ungranted_payment_read = app.client().query("SELECT status FROM payments", &[]).await;
        app.close()
            .await
            .expect("the application connection closes cleanly");

        assert_eq!(current_user, APPLICATION_ROLE);
        assert_eq!(inserted, 1);
        assert_eq!(reconciled, 1);
        assert!(
            marker_read.is_err(),
            "the app cannot inspect safety identity"
        );
        assert!(
            marker_write.is_err(),
            "the app cannot mutate safety identity"
        );
        assert!(
            schema_write.is_err(),
            "the app cannot create schema objects"
        );
        assert!(
            temporary_write.is_err(),
            "the app cannot create temporary schema objects"
        );
        assert!(payment_delete.is_err(), "the app cannot delete payments");
        assert!(
            ungranted_payment_read.is_err(),
            "the app cannot read columns outside its reconciliation predicate"
        );
    }

    async fn assert_application_role_after_reset(
        postgres: &TruthSpikePostgres,
        case_name: &DatabaseName,
    ) {
        let mut reset_app = postgres
            .connect_application_database(case_name)
            .await
            .expect("the app role can connect after reset");
        let reset_insert = reset_app
            .client()
            .execute(
                "INSERT INTO payments \
                     (operation_id, stripe_payment_intent_id, amount_minor, currency, status) \
                 VALUES ('op_1', 'pi_tiv_role_test_after_reset', 2500, 'usd', 'succeeded')",
                &[],
            )
            .await
            .expect("the narrow grant survives a template reset");
        reset_app
            .close()
            .await
            .expect("the reset application connection closes cleanly");
        assert_eq!(reset_insert, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires the isolated reference-app Compose project"]
    async fn real_reference_app_replays_commit_close_with_the_same_failure_identity() {
        let postgres = test_postgres().await;
        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let provisioned = postgres
            .provision_reference_databases(&suffix, test_project())
            .await
            .expect("the baseline and first case are provisioned");
        let baseline_target = provisioned.baseline_target().clone();
        let case_name = provisioned.case_name().clone();
        let first_database_oid = provisioned.case_target().identity().database_oid();

        let plan = committed_replay_plan();
        let first_report = run_reference_app_checkout(&postgres, &case_name, &plan, 1).await;
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
            .expect("the real app case resets from the sealed template");
        let reset_database_oid = reset_target.identity().database_oid();

        let replay_report = run_reference_app_checkout(&postgres, &case_name, &plan, 3).await;
        let replay_identity = provider_uniqueness_failure(&replay_report).identity();
        let evidence = TruthSpikeEvidence::new(
            2,
            first_database_oid,
            reset_database_oid,
            &first_identity,
            replay_identity,
        )
        .expect("the real application path forms coherent bounded evidence");
        let encoded = evidence
            .to_pretty_json()
            .expect("the evidence document serializes");

        assert!(encoded.contains("provider-object-unique"));
        assert!(!encoded.contains("run-scoped-control-token"));
        assert!(!encoded.contains("tiv-app-local-only-password"));
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
            "tiv-app-local-only-password",
        ))
        .await
        .expect("the isolated PostgreSQL fixture is healthy")
    }

    fn test_project() -> ComposeProjectId {
        let project = std::env::var("TIV_COMPOSE_PROJECT")
            .unwrap_or_else(|_| "tiv-truth-spike-postgres".to_owned());
        ComposeProjectId::new(project).expect("the test project name is valid")
    }

    fn provider_objects() -> [ProviderPaymentIntent; 2] {
        [
            ProviderPaymentIntent::new("pi_tiv_first", 2_500, "usd", "requires_confirmation")
                .expect("the first provider projection is valid"),
            ProviderPaymentIntent::new("pi_tiv_retry", 2_500, "usd", "requires_confirmation")
                .expect("the retry provider projection is valid"),
        ]
    }

    fn committed_replay_plan() -> ReplayPlan {
        replay_plan_from_json(include_str!("../../../../spike/compiled-trace-v1.json"))
    }

    fn replay_plan_from_json(document: &str) -> ReplayPlan {
        let trace: CompiledTrace = serde_json::from_str(document).expect("the trace is valid");
        ReplayPlan::from_trace(&trace).expect("the trace compiles into a replay plan")
    }

    #[test]
    fn reference_app_replay_script_is_derived_from_the_committed_trace_plan() {
        let plan = committed_replay_plan();
        let script =
            ReferenceReplayScript::from_plan(&plan).expect("the committed trace is executable");

        assert_eq!(script.fixture_seed(), Seed::new(7));
        assert_eq!(
            script.expected_payment_intent_id(),
            "pi_tiv_7dc6fb6eb37270c34d739b91"
        );

        let incomplete_plan = replay_plan_from_json(
            r#"{
            "schema_version": 1,
            "seed": 7,
            "actions": [{
                "id": 1,
                "kind": "DriveCheckout",
                "dependencies": [],
                "inputs": [],
                "declared_outputs": ["PaymentIntentId"]
            }],
            "captured": [{
                "output_ref": {"action_id": 1, "slot": "PaymentIntentId"},
                "value": {"PaymentIntentId": "pi_tiv_7dc6fb6eb37270c34d739b91"}
            }]
        }"#,
        );

        assert_eq!(
            ReferenceReplayScript::from_plan(&incomplete_plan),
            Err(ReferenceReplayScriptError::UnexpectedStepCount { actual: 1 })
        );
    }

    async fn run_buggy_checkout(
        postgres: &TruthSpikePostgres,
        case_target: DatabaseTarget<Unverified>,
    ) -> (SnapshotReport, DatabaseTarget<Unverified>) {
        let case_name = case_target.identity().database_name().clone();
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
        let case_target = postgres
            .insert_buggy_payment_pair(case_target, "op_1", &provider_objects)
            .await
            .expect("reconciliation persists both provider objects for one operation");
        let report = postgres
            .check_reference_invariants(&case_name, &provider_objects, quiescence())
            .await
            .expect("the five-query snapshot completes");
        (report, case_target)
    }

    async fn run_reference_app_checkout(
        postgres: &TruthSpikePostgres,
        case_name: &DatabaseName,
        plan: &ReplayPlan,
        reset_sequence: u64,
    ) -> SnapshotReport {
        let script =
            ReferenceReplayScript::from_plan(plan).expect("the trace has a supported script");
        let client = reqwest::Client::new();
        let control_base = std::env::var("TIV_FIXTURE_CONTROL_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:12112".to_owned());
        let app_base = std::env::var("TIV_REFERENCE_APP_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:18080".to_owned());
        reset_fixture(
            &client,
            &control_base,
            reset_sequence,
            script.fixture_seed(),
        )
        .await;
        drive_reference_checkout(&client, &app_base, case_name, &script).await;
        assert_fixture_control_is_isolated(&client, &app_base).await;
        let attempts = confirm_fixture(&client, &control_base, reset_sequence + 1).await;
        deliver_webhook_attempts(&client, &app_base, &attempts).await;
        let provider_objects = fixture_provider_projection(&client, &control_base).await;
        assert!(
            provider_objects
                .iter()
                .any(|provider| provider.id() == script.expected_payment_intent_id()),
            "the fixture projection must include the trace-bound PaymentIntent"
        );

        postgres
            .check_reference_invariants(case_name, &provider_objects, quiescence())
            .await
            .expect("the real app path reaches the five-query oracle")
    }

    async fn reset_fixture(
        client: &reqwest::Client,
        control_base: &str,
        sequence: u64,
        seed: Seed,
    ) {
        const CONTROL_TOKEN: &str = "run-scoped-control-token";
        let reset = client
            .post(format!("{control_base}/v1/control/reset"))
            .header("X-Tiv-Control-Token", CONTROL_TOKEN)
            .json(&serde_json::json!({
                "command_sequence": sequence,
                "seed": seed.value(),
                "outcomes": ["commit_then_close", "normal"]
            }))
            .send()
            .await
            .expect("the host reaches the loopback-only fixture control listener");
        assert_eq!(reset.status(), StatusCode::OK);
    }

    async fn drive_reference_checkout(
        client: &reqwest::Client,
        app_base: &str,
        case_name: &DatabaseName,
        script: &ReferenceReplayScript,
    ) {
        let checkout = client
            .post(format!("{app_base}/checkout"))
            .json(&serde_json::json!({
                "database": case_name.as_str(),
                "operation_id": "op_1",
                "amount_minor": 2500,
                "currency": "usd"
            }))
            .send()
            .await
            .expect("the host reaches the real reference application");
        assert_eq!(checkout.status(), StatusCode::OK);
        let checkout: serde_json::Value = checkout.json().await.expect("checkout returns JSON");
        assert!(
            checkout["payment_intent_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("pi_tiv_"))
        );
        assert_eq!(
            checkout["payment_intent_id"].as_str(),
            Some(script.expected_payment_intent_id())
        );
    }

    async fn assert_fixture_control_is_isolated(client: &reqwest::Client, app_base: &str) {
        let isolation_probe = client
            .get(format!("{app_base}/probe-fixture-control"))
            .send()
            .await
            .expect("the app reports its control-network probe");
        assert_eq!(isolation_probe.status(), StatusCode::OK);
        let isolation_probe: serde_json::Value = isolation_probe
            .json()
            .await
            .expect("the isolation probe is JSON");
        assert_eq!(isolation_probe["reachable"], false);
    }

    async fn confirm_fixture(
        client: &reqwest::Client,
        control_base: &str,
        sequence: u64,
    ) -> Vec<serde_json::Value> {
        const CONTROL_TOKEN: &str = "run-scoped-control-token";
        let confirmation = client
            .post(format!("{control_base}/v1/control/confirm-all"))
            .header("X-Tiv-Control-Token", CONTROL_TOKEN)
            .json(&serde_json::json!({
                "command_sequence": sequence,
                "timestamp": 1_700_000_000
            }))
            .send()
            .await
            .expect("the host confirms the fixture objects");
        assert_eq!(confirmation.status(), StatusCode::OK);
        let confirmation: serde_json::Value = confirmation
            .json()
            .await
            .expect("the signed attempts are JSON");
        let attempts = confirmation["attempts"]
            .as_array()
            .expect("confirmation exports attempts");
        assert_eq!(attempts.len(), 2);
        attempts.clone()
    }

    async fn deliver_webhook_attempts(
        client: &reqwest::Client,
        app_base: &str,
        attempts: &[serde_json::Value],
    ) {
        for attempt in attempts {
            let raw_body = hex::decode(
                attempt["raw_body_hex"]
                    .as_str()
                    .expect("the attempt carries raw bytes"),
            )
            .expect("the raw-body transport is valid hex");
            let delivery = client
                .post(format!("{app_base}/webhooks/stripe"))
                .header(
                    "Stripe-Signature",
                    attempt["signature_header"]
                        .as_str()
                        .expect("the attempt carries a signature"),
                )
                .body(raw_body)
                .send()
                .await
                .expect("the exact signed bytes reach the app handler");
            assert_eq!(delivery.status(), StatusCode::OK);
        }
    }

    async fn fixture_provider_projection(
        client: &reqwest::Client,
        control_base: &str,
    ) -> Vec<ProviderPaymentIntent> {
        const CONTROL_TOKEN: &str = "run-scoped-control-token";
        let state = client
            .get(format!("{control_base}/v1/control/state"))
            .header("X-Tiv-Control-Token", CONTROL_TOKEN)
            .send()
            .await
            .expect("the host reads the bounded provider projection");
        assert_eq!(state.status(), StatusCode::OK);
        let state: serde_json::Value = state.json().await.expect("fixture state is JSON");
        let provider_objects = state["payment_intents"]
            .as_array()
            .expect("fixture state carries provider objects")
            .iter()
            .map(|payment_intent| {
                ProviderPaymentIntent::new(
                    payment_intent["id"]
                        .as_str()
                        .expect("the provider ID is present"),
                    payment_intent["amount_minor"]
                        .as_i64()
                        .expect("the amount is present"),
                    payment_intent["currency"]
                        .as_str()
                        .expect("the currency is present"),
                    payment_intent["status"]
                        .as_str()
                        .expect("the status is present"),
                )
                .expect("the fixture projection is valid")
            })
            .collect::<Vec<_>>();
        assert_eq!(provider_objects.len(), 2);
        provider_objects
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
