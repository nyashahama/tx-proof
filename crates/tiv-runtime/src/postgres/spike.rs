use std::time::Duration;

use thiserror::Error;
use tokio::task::{JoinError, JoinHandle};
use tokio_postgres::{Client, NoTls};
use uuid::Uuid;

use super::{
    archive::{ArchiveError, BaselineArchive, PostgresArchiveToolchain},
    oracle::{
        OracleError, ProviderPaymentIntent, QuiescencePermit, SnapshotReport, run_reference_oracle,
    },
    probe::{ConfiguredSqlProbe, SqlProbe, SqlProbeError},
    safety::{
        ComposeProjectId, DatabaseEndpoint, DatabaseIdentity, DatabaseKind, DatabaseMarker,
        DatabaseName, DatabaseTarget, InvalidDatabaseIdentity, InvalidDatabaseName, MarkerKind,
        MutationPermit, SafetyError, Unverified, Verified,
    },
};

const APPLICATION_ROLE: &str = "tiv_app";
const INVARIANT_ROLE: &str = "tiv_invariant";
#[cfg(test)]
const TRUTH_SPIKE_CLUSTER_NAME: &str = "tiv-truth-spike-postgres";
const REFERENCE_APP_CLUSTER_NAME: &str = "tiv-reference-app-postgres";

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
    expected_cluster_name: &'static str,
    archive_container_id: Option<String>,
}

impl SpikePostgresConfig {
    #[must_use]
    #[cfg(test)]
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
            expected_cluster_name: TRUTH_SPIKE_CLUSTER_NAME,
            archive_container_id: None,
        }
    }

    #[must_use]
    #[cfg(test)]
    pub fn loopback_with_archive_container(
        port: u16,
        admin_role: impl Into<String>,
        admin_password: impl Into<String>,
        application_password: impl Into<String>,
        archive_container_id: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: DatabaseEndpoint::loopback(port),
            admin_role: admin_role.into(),
            admin_password: admin_password.into(),
            application_password: application_password.into(),
            expected_cluster_name: TRUTH_SPIKE_CLUSTER_NAME,
            archive_container_id: Some(archive_container_id.into()),
        }
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn loopback_reference_app(
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
            expected_cluster_name: REFERENCE_APP_CLUSTER_NAME,
            archive_container_id: None,
        }
    }

    #[must_use]
    pub(crate) fn loopback_reference_app_with_archive(
        port: u16,
        admin_role: impl Into<String>,
        admin_password: impl Into<String>,
        application_password: impl Into<String>,
        archive_container_id: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: DatabaseEndpoint::loopback(port),
            admin_role: admin_role.into(),
            admin_password: admin_password.into(),
            application_password: application_password.into(),
            expected_cluster_name: REFERENCE_APP_CLUSTER_NAME,
            archive_container_id: Some(archive_container_id.into()),
        }
    }
}

pub struct TruthSpikePostgres {
    config: SpikePostgresConfig,
    archive_toolchain: Option<PostgresArchiveToolchain>,
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
        let mut postgres = Self {
            config,
            archive_toolchain: None,
        };
        let mut session = postgres.connect_database("postgres").await?;
        let probe = session
            .client()
            .query_one(
                "SELECT 1::integer, current_setting('cluster_name'), current_user, \
                        current_setting('server_version_num')::integer",
                &[],
            )
            .await;
        session.close().await?;
        let row = probe?;
        if row.get::<_, i32>(0) != 1
            || row.get::<_, &str>(1) != postgres.config.expected_cluster_name
            || row.get::<_, &str>(2) != postgres.config.admin_role
        {
            return Err(SpikePostgresError::UnexpectedServerIdentity);
        }
        let server_version_num = row.get::<_, i32>(3);
        let server_major = u16::try_from(server_version_num / 10_000)
            .map_err(|_| SpikePostgresError::UnexpectedServerIdentity)?;
        if let Some(container_id) = postgres.config.archive_container_id.clone() {
            postgres.archive_toolchain = Some(
                PostgresArchiveToolchain::attest(
                    container_id,
                    postgres.config.admin_role.clone(),
                    server_major,
                )
                .await?,
            );
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
        self.ensure_invariant_role().await?;
        self.create_empty_database(&baseline_name).await?;
        let operation_id = format!("op_{suffix}");
        self.initialize_reference_baseline(
            &baseline_name,
            baseline_marker,
            &compose_project,
            &operation_id,
        )
        .await?;
        let baseline_archive = if let Some(toolchain) = &self.archive_toolchain {
            Some(toolchain.capture(&baseline_name).await?)
        } else {
            None
        };
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
            baseline_archive,
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
    #[cfg(test)]
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

    #[cfg(test)]
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

    /// Opens the configured repository-owned predicate against one freshly
    /// attested reference case database.
    pub(crate) async fn configured_sql_probe(
        &self,
        expected: &DatabaseTarget<Unverified>,
        configured: ConfiguredSqlProbe,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<ReferenceSqlProbe, SpikePostgresError> {
        let case_name = expected.identity().database_name();
        if case_name.kind() != DatabaseKind::Case
            || configured.role().as_str() != INVARIANT_ROLE
            || timeout.is_zero()
            || poll_interval.is_zero()
            || poll_interval >= timeout
        {
            return Err(SpikePostgresError::InvalidSqlProbeConfiguration);
        }
        let observed = self.observe_identity(case_name).await?;
        if &observed != expected.identity() {
            return Err(SpikePostgresError::PostResetIdentityMismatch);
        }
        let mut session = self.connect_database(case_name.as_str()).await?;
        let identity = session
            .client()
            .query_one(
                "SELECT current_database(), current_setting('cluster_name'), current_user",
                &[],
            )
            .await?;
        if identity.get::<_, &str>(0) != case_name.as_str()
            || identity.get::<_, &str>(1) != REFERENCE_APP_CLUSTER_NAME
            || identity.get::<_, &str>(2) != self.config.admin_role
        {
            return Err(SpikePostgresError::UnexpectedServerIdentity);
        }
        Ok(ReferenceSqlProbe {
            session,
            probe: configured.into_probe(),
            timeout,
            poll_interval,
        })
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

    /// Rechecks the exact case and baseline identities, consumes a mutation
    /// permit, and restores the trusted custom-format baseline archive into a
    /// fresh database created from `template0`.
    ///
    /// # Errors
    ///
    /// Returns [`SpikePostgresError`] before mutation when the archive,
    /// toolchain, case, or baseline does not match this isolated run, or when a
    /// bounded drop, restore, privilege, marker, or identity step fails.
    pub async fn reset_case_from_archive(
        &self,
        expected: DatabaseTarget<Unverified>,
        expected_baseline: &BaselineTarget,
        archive: &BaselineArchive,
        new_marker_uuid: Uuid,
    ) -> Result<DatabaseTarget<Unverified>, SpikePostgresError> {
        if expected_baseline.identity.database_name().kind() != DatabaseKind::Baseline
            || !expected_baseline.is_sealed()
        {
            return Err(SpikePostgresError::ExpectedBaselineName);
        }
        let toolchain = self
            .archive_toolchain
            .as_ref()
            .ok_or(SpikePostgresError::ArchiveUnavailable)?;
        toolchain.preflight(archive, expected_baseline.identity.database_name())?;
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
        self.reset_verified_case_from_archive(verified, permit, archive, new_marker_uuid)
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
            let unsafe_membership = session
                .client()
                .query_opt(
                    "SELECT 1::integer \
                     FROM pg_auth_members AS membership \
                     JOIN pg_roles AS granted_role ON granted_role.oid = membership.roleid \
                     JOIN pg_roles AS member_role ON member_role.oid = membership.member \
                     WHERE granted_role.rolname = $1 OR member_role.rolname = $1 \
                     LIMIT 1",
                    &[&APPLICATION_ROLE],
                )
                .await?
                .is_some();
            if unsafe_membership {
                return Err(SpikePostgresError::UnsafeApplicationRole);
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
                .await?;
            Ok::<(), SpikePostgresError>(())
        }
        .await;
        session.close().await?;
        result?;
        Ok(())
    }

    async fn ensure_invariant_role(&self) -> Result<(), SpikePostgresError> {
        let mut session = self.connect_database("postgres").await?;
        let result = async {
            let role = session
                .client()
                .query_opt(
                    "SELECT NOT role.rolsuper \
                                AND NOT role.rolcreatedb \
                                AND NOT role.rolcreaterole \
                                AND NOT role.rolinherit \
                                AND NOT role.rolcanlogin \
                                AND NOT role.rolreplication \
                                AND NOT role.rolbypassrls \
                                AND role.rolconnlimit = -1 \
                                AND role.rolvaliduntil IS NULL \
                                AND role.rolconfig IS NULL, \
                            NOT EXISTS ( \
                                SELECT 1 \
                                FROM pg_auth_members AS membership \
                                WHERE membership.roleid = role.oid \
                                   OR membership.member = role.oid \
                            ), \
                            NOT EXISTS ( \
                                SELECT 1 FROM pg_database \
                                WHERE datdba = role.oid \
                            ) \
                     FROM pg_roles AS role \
                     WHERE role.rolname = $1",
                    &[&INVARIANT_ROLE],
                )
                .await?;
            match role {
                Some(row)
                    if row.get::<_, bool>(0) && row.get::<_, bool>(1) && row.get::<_, bool>(2) =>
                {
                    Ok(())
                }
                Some(_) => Err(SpikePostgresError::UnsafeInvariantRole),
                None => {
                    session
                        .client()
                        .batch_execute(
                            "CREATE ROLE tiv_invariant WITH \
                                 NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT \
                                 NOREPLICATION NOBYPASSRLS CONNECTION LIMIT -1 PASSWORD NULL",
                        )
                        .await?;
                    Ok(())
                }
            }
        }
        .await;
        session.close().await?;
        result
    }

    async fn initialize_reference_baseline(
        &self,
        baseline_name: &DatabaseName,
        marker_uuid: Uuid,
        compose_project: &ComposeProjectId,
        operation_id: &str,
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
                     REVOKE ALL ON SCHEMA public FROM PUBLIC; \
                     GRANT USAGE ON SCHEMA public TO tiv_app, tiv_invariant; \
                     REVOKE ALL ON TABLE tiv_verifier_marker, orders, payments \
                         FROM PUBLIC, tiv_app, tiv_invariant; \
                     REVOKE ALL ON SEQUENCE orders_id_seq, payments_id_seq \
                         FROM PUBLIC, tiv_app, tiv_invariant; \
                     GRANT SELECT (operation_id, amount_minor, currency) ON TABLE orders TO tiv_app; \
                     GRANT INSERT ON TABLE payments TO tiv_app; \
                     GRANT SELECT (operation_id, stripe_payment_intent_id), \
                           UPDATE (status) ON TABLE payments TO tiv_app; \
                     GRANT USAGE ON SEQUENCE payments_id_seq TO tiv_app; \
                     GRANT SELECT ON TABLE orders, payments TO tiv_invariant;",
                )
                .await?;
            session
                .client()
                .execute(
                    "INSERT INTO orders (operation_id, amount_minor, currency, status) \
                     VALUES ($1, 2500, 'usd', 'pending')",
                    &[&operation_id],
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
                 REVOKE ALL ON DATABASE {} FROM {INVARIANT_ROLE}; \
                 GRANT CONNECT ON DATABASE {} TO {APPLICATION_ROLE}",
                case_name.as_str(),
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

    async fn reset_verified_case_from_archive(
        &self,
        verified: DatabaseTarget<Verified>,
        _permit: MutationPermit,
        archive: &BaselineArchive,
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
        let create_case = format!(
            "CREATE DATABASE {} WITH OWNER = {} TEMPLATE = template0",
            case_name.as_str(),
            self.config.admin_role,
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
            maintenance.client().batch_execute(&create_case).await?;
            maintenance
                .client()
                .query_one(
                    "SELECT oid::bigint FROM pg_database WHERE datname = $1",
                    &[&case_name.as_str()],
                )
                .await
                .map(|row| row.get::<_, i64>(0))
        }
        .await;
        maintenance.close().await?;
        let created_database_oid = reset_result?;

        let restore_result = self
            .archive_toolchain
            .as_ref()
            .ok_or(SpikePostgresError::ArchiveUnavailable)?
            .restore(archive, &case_name)
            .await;
        if let Err(error) = restore_result {
            self.remove_failed_archive_case(&case_name, created_database_oid, prior_owner_oid)
                .await?;
            return Err(error.into());
        }
        let mut maintenance = self.connect_database("postgres").await?;
        let configure_result = self
            .configure_case_connect(maintenance.client(), &case_name)
            .await;
        maintenance.close().await?;
        configure_result?;

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

    async fn remove_failed_archive_case(
        &self,
        case_name: &DatabaseName,
        expected_database_oid: i64,
        expected_owner_oid: u32,
    ) -> Result<(), SpikePostgresError> {
        let mut maintenance = self.connect_database("postgres").await?;
        let catalog = maintenance
            .client()
            .query_opt(
                "SELECT oid::bigint, datdba::bigint FROM pg_database WHERE datname = $1",
                &[&case_name.as_str()],
            )
            .await?;
        let cleanup_result = if let Some(catalog) = catalog {
            let observed_database_oid = catalog.get::<_, i64>(0);
            let observed_owner_oid = u32::try_from(catalog.get::<_, i64>(1))
                .map_err(|_| SpikePostgresError::ArchiveCleanupIdentityMismatch)?;
            if observed_database_oid != expected_database_oid
                || observed_owner_oid != expected_owner_oid
            {
                Err(SpikePostgresError::ArchiveCleanupIdentityMismatch)
            } else {
                maintenance
                    .client()
                    .execute(
                        "SELECT pg_terminate_backend(pid) \
                         FROM pg_stat_activity \
                         WHERE datid::bigint = $1 AND pid <> pg_backend_pid()",
                        &[&expected_database_oid],
                    )
                    .await?;
                let drop_case = format!("DROP DATABASE {}", case_name.as_str());
                maintenance
                    .client()
                    .batch_execute(&drop_case)
                    .await
                    .map_err(SpikePostgresError::Postgres)
            }
        } else {
            Ok(())
        };
        maintenance.close().await?;
        cleanup_result?;
        Ok(())
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

    #[cfg(test)]
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
    baseline_archive: Option<BaselineArchive>,
}

impl ReferenceProvisioning {
    #[must_use]
    #[cfg(test)]
    pub const fn baseline_target(&self) -> &BaselineTarget {
        &self.baseline_target
    }

    #[must_use]
    #[cfg(test)]
    pub const fn case_name(&self) -> &DatabaseName {
        &self.case_name
    }

    #[must_use]
    #[cfg(test)]
    pub const fn case_target(&self) -> &DatabaseTarget<Unverified> {
        &self.case_target
    }

    #[must_use]
    #[cfg(test)]
    pub fn into_case_target(self) -> DatabaseTarget<Unverified> {
        self.case_target
    }

    #[must_use]
    pub fn into_archive_parts(
        self,
    ) -> Option<(
        BaselineTarget,
        DatabaseName,
        DatabaseTarget<Unverified>,
        BaselineArchive,
    )> {
        let Self {
            baseline_target,
            case_name,
            case_target,
            baseline_archive,
        } = self;
        Some((baseline_target, case_name, case_target, baseline_archive?))
    }
}

struct PostgresSession {
    client: Option<Client>,
    connection: Option<JoinHandle<Result<(), tokio_postgres::Error>>>,
}

/// One bounded configured `PostgreSQL` observer for a reference case.
pub(crate) struct ReferenceSqlProbe {
    session: PostgresSession,
    probe: SqlProbe,
    timeout: Duration,
    poll_interval: Duration,
}

impl ReferenceSqlProbe {
    /// Proves the repository predicate is false immediately before the owning
    /// application action begins.
    pub(crate) async fn require_false(&mut self) -> Result<(), SpikePostgresError> {
        self.probe
            .require_false(self.session.client())
            .await
            .map_err(SpikePostgresError::SqlProbe)
    }

    /// Observes the repository predicate's first committed true value.
    pub(crate) async fn observe_true(&mut self) -> Result<(), SpikePostgresError> {
        self.probe
            .observe_true(self.session.client(), self.timeout, self.poll_interval)
            .await
            .map_err(SpikePostgresError::SqlProbe)
    }

    pub(crate) const fn observed(&self) -> bool {
        self.probe.observed()
    }
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
    #[error("the application role has unexpected inherited memberships")]
    UnsafeApplicationRole,
    #[error("the invariant role exceeds the isolated least-privilege contract")]
    UnsafeInvariantRole,
    #[error("invalid reference SQL probe configuration")]
    InvalidSqlProbeConfiguration,
    #[error("the configured reference SQL probe failed: {0}")]
    SqlProbe(#[source] SqlProbeError),
    #[cfg(test)]
    #[error("the synthetic bug requires two distinct provider objects")]
    InvalidBugState,
    #[error("PostgreSQL server is not the isolated truth-spike cluster")]
    UnexpectedServerIdentity,
    #[error("the matching PostgreSQL archive toolchain was not attested")]
    ArchiveUnavailable,
    #[error("failed archive cleanup refused a substituted database identity")]
    ArchiveCleanupIdentityMismatch,
    #[error(transparent)]
    Archive(#[from] ArchiveError),
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
    use std::{
        os::unix::fs::PermissionsExt,
        process::Command as StdCommand,
        sync::Arc,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use reqwest::StatusCode;
    use tiv_core::{decision::Seed, result::AttemptResult, trace::CompiledTrace};
    use tiv_stripe_pi::{FaultOutcome, PaymentIntentFixture, http::serve_http1_connection};

    use crate::replay::{
        ReferenceAppReplayConfig, ReferenceReplayScript, ReferenceReplayScriptError, ReplayPlan,
        run_reference_app_replay,
    };
    use tokio::{net::TcpListener, sync::Mutex, time::timeout};

    use super::*;
    use crate::{
        evidence::TruthSpikeEvidence,
        postgres::oracle::{InvariantOutcome, InvariantVerdict},
    };

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn provisioned_reference_case_owns_its_case_derived_operation() {
        let postgres = test_postgres().await;
        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let provisioned = postgres
            .provision_reference_databases(&suffix, test_project())
            .await
            .expect("the baseline and case are provisioned");
        let mut session = postgres
            .connect_database(provisioned.case_name().as_str())
            .await
            .expect("the generated case is reachable");
        let operation_id: String = session
            .client()
            .query_one("SELECT operation_id FROM orders", &[])
            .await
            .expect("the seed order is readable")
            .get(0);
        session.close().await.expect("the session closes cleanly");

        assert_eq!(operation_id, format!("op_{suffix}"));
    }

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
        let operation_id = format!("op_{suffix}");

        let stale_case_target = DatabaseTarget::new(provisioned.case_target().identity().clone());
        let dirty_case_target = postgres
            .insert_buggy_payment_pair(
                provisioned.into_case_target(),
                &operation_id,
                &provider_objects,
            )
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
            .insert_buggy_payment_pair(stale_case_target, &operation_id, &provider_objects)
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
            .insert_buggy_payment_pair(reset_target, &operation_id, &provider_objects)
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn custom_archive_fallback_restores_a_private_fresh_case() {
        let postgres = test_postgres_with_archive().await;
        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let provisioned = postgres
            .provision_reference_databases(&suffix, test_project())
            .await
            .expect("the baseline, archive, and first case are provisioned");
        let (baseline_target, case_name, first_case_target, archive) = provisioned
            .into_archive_parts()
            .expect("the matching container toolchain captured a baseline archive");
        let archive_path = archive.path().to_owned();
        let archive_metadata = archive_path
            .metadata()
            .expect("archive metadata is readable");
        assert_eq!(
            archive_metadata.permissions().mode() & 0o777,
            0o600,
            "the host archive is never group- or world-readable"
        );
        assert!(archive_metadata.len() > 0, "the archive is not empty");
        let original_oid = first_case_target.identity().database_oid();
        let provider_objects = provider_objects();
        let operation_id = format!("op_{suffix}");
        let dirty_case_target = postgres
            .insert_buggy_payment_pair(first_case_target, &operation_id, &provider_objects)
            .await
            .expect("the synthetic bug state is inserted before fallback reset");

        let reset_target = postgres
            .reset_case_from_archive(
                dirty_case_target,
                &baseline_target,
                &archive,
                Uuid::new_v4(),
            )
            .await
            .expect("the custom archive restores into a fresh template0 database");

        assert_ne!(reset_target.identity().database_oid(), original_oid);
        let clean_report = postgres
            .check_reference_invariants(&case_name, &provider_objects, quiescence())
            .await
            .expect("the restored case is inspectable");
        assert!(matches!(
            clean_report
                .outcome("provider-object-unique")
                .expect("the invariant ran")
                .verdict(),
            InvariantVerdict::Held
        ));
        assert_application_role_after_reset(&postgres, reset_target.identity().database_name())
            .await;
        assert_invariant_role_contract(&postgres, reset_target.identity().database_name()).await;

        drop(archive);
        assert!(
            !archive_path.exists(),
            "drop deletes the trusted local archive"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn failed_archive_restore_removes_the_new_empty_case() {
        let postgres = test_postgres_with_archive().await;
        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let provisioned = postgres
            .provision_reference_databases(&suffix, test_project())
            .await
            .expect("the baseline archive and first case are provisioned");
        let (baseline_target, case_name, case_target, mut archive) = provisioned
            .into_archive_parts()
            .expect("the baseline archive exists");
        archive
            .truncate_after_magic_for_test()
            .expect("the test leaves only a valid custom-archive magic prefix");

        let result = postgres
            .reset_case_from_archive(case_target, &baseline_target, &archive, Uuid::new_v4())
            .await;

        assert!(matches!(
            result,
            Err(SpikePostgresError::Archive(
                ArchiveError::ToolCommandFailed(crate::postgres::archive::PostgresTool::Restore)
            ))
        ));
        let mut maintenance = postgres
            .connect_database("postgres")
            .await
            .expect("the maintenance database remains reachable");
        let case_count = maintenance
            .client()
            .query_one(
                "SELECT COUNT(*)::bigint FROM pg_database WHERE datname = $1",
                &[&case_name.as_str()],
            )
            .await
            .expect("the failed case name can be checked")
            .get::<_, i64>(0);
        maintenance
            .close()
            .await
            .expect("the maintenance session closes");
        assert_eq!(case_count, 0, "a failed restore leaves no usable case");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn full_commit_close_three_attempt_chain_has_one_failure_identity() {
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
        let second_case_target = postgres
            .reset_case_from_template(dirty_case_target, &baseline_target, Uuid::new_v4())
            .await
            .expect("the marked case resets from the sealed template");
        let second_database_oid = second_case_target.identity().database_oid();
        let (second_report, second_dirty_case_target) =
            run_buggy_checkout(&postgres, second_case_target).await;
        let second_identity = provider_uniqueness_failure(&second_report)
            .identity()
            .clone();
        let third_case_target = postgres
            .reset_case_from_template(second_dirty_case_target, &baseline_target, Uuid::new_v4())
            .await
            .expect("the second marked case resets from the sealed template");
        let third_database_oid = third_case_target.identity().database_oid();
        let (third_report, _third_dirty_case_target) =
            run_buggy_checkout(&postgres, third_case_target).await;
        let third_identity = provider_uniqueness_failure(&third_report)
            .identity()
            .clone();
        let attempts = [
            AttemptResult::Violation(first_identity.clone()),
            AttemptResult::Violation(second_identity),
            AttemptResult::Violation(third_identity),
        ];

        let evidence = TruthSpikeEvidence::new(
            [2, 2, 2],
            [first_database_oid, second_database_oid, third_database_oid],
            &first_identity,
            &attempts,
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
        assert_invariant_role_contract(&postgres, &case_name).await;

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
        assert_invariant_role_contract(&postgres, reset_target.identity().database_name()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn preexisting_application_role_membership_blocks_provisioning() {
        let postgres = test_postgres().await;
        postgres
            .ensure_application_role()
            .await
            .expect("the initial isolated application role is safe");
        let mut maintenance = postgres
            .connect_database("postgres")
            .await
            .expect("the isolated maintenance database is reachable");
        maintenance
            .client()
            .batch_execute(
                "DROP ROLE IF EXISTS tiv_unexpected_member_role; \
                 CREATE ROLE tiv_unexpected_member_role; \
                 GRANT tiv_unexpected_member_role TO tiv_app;",
            )
            .await
            .expect("the test installs one unexpected membership");
        maintenance
            .close()
            .await
            .expect("the maintenance session closes");

        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let port = std::env::var("TIV_POSTGRES_TEST_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(15_432);
        let rotating_postgres = TruthSpikePostgres::connect(SpikePostgresConfig::loopback(
            port,
            "tiv_admin",
            "tiv-local-only-password",
            "unexpected-new-password",
        ))
        .await
        .expect("the isolated maintenance identity is unchanged");
        let result = rotating_postgres
            .provision_reference_databases(&suffix, test_project())
            .await;

        let mut cleanup = postgres
            .connect_database("postgres")
            .await
            .expect("the isolated maintenance database remains reachable");
        let generated_database_count = cleanup
            .client()
            .query_one(
                "SELECT COUNT(*)::bigint FROM pg_database \
                 WHERE datname IN ($1, $2)",
                &[&format!("tiv_base_{suffix}"), &format!("tiv_case_{suffix}")],
            )
            .await
            .expect("the generated database names are observable")
            .get::<_, i64>(0);
        cleanup
            .client()
            .batch_execute(
                "REVOKE tiv_unexpected_member_role FROM tiv_app; \
                 DROP ROLE tiv_unexpected_member_role;",
            )
            .await
            .expect("the test membership is removed");
        cleanup.close().await.expect("the cleanup session closes");

        let old_password_session = postgres
            .connect_database_as("postgres", APPLICATION_ROLE, "tiv-app-local-only-password")
            .await
            .expect("the rejected provisioning did not rotate the application password");
        old_password_session
            .close()
            .await
            .expect("the application session closes");
        assert!(
            postgres
                .connect_database_as("postgres", APPLICATION_ROLE, "unexpected-new-password")
                .await
                .is_err(),
            "the rejected provisioning must not install the proposed password"
        );

        assert!(matches!(
            result,
            Err(SpikePostgresError::UnsafeApplicationRole)
        ));
        assert_eq!(generated_database_count, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn preexisting_member_of_application_role_blocks_provisioning() {
        let postgres = test_postgres().await;
        postgres
            .ensure_application_role()
            .await
            .expect("the initial isolated application role is safe");
        let mut maintenance = postgres
            .connect_database("postgres")
            .await
            .expect("the isolated maintenance database is reachable");
        maintenance
            .client()
            .batch_execute(
                "DROP ROLE IF EXISTS tiv_unexpected_app_member; \
                 CREATE ROLE tiv_unexpected_app_member; \
                 GRANT tiv_app TO tiv_unexpected_app_member;",
            )
            .await
            .expect("the test installs one unexpected application-role member");
        maintenance
            .close()
            .await
            .expect("the maintenance session closes");

        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let result = postgres
            .provision_reference_databases(&suffix, test_project())
            .await;

        let mut cleanup = postgres
            .connect_database("postgres")
            .await
            .expect("the isolated maintenance database remains reachable");
        let generated_database_count = cleanup
            .client()
            .query_one(
                "SELECT COUNT(*)::bigint FROM pg_database \
                 WHERE datname IN ($1, $2)",
                &[&format!("tiv_base_{suffix}"), &format!("tiv_case_{suffix}")],
            )
            .await
            .expect("the generated database names are observable")
            .get::<_, i64>(0);
        cleanup
            .client()
            .batch_execute(
                "REVOKE tiv_app FROM tiv_unexpected_app_member; \
                 DROP ROLE tiv_unexpected_app_member;",
            )
            .await
            .expect("the test membership is removed");
        cleanup.close().await.expect("the cleanup session closes");

        assert!(matches!(
            result,
            Err(SpikePostgresError::UnsafeApplicationRole)
        ));
        assert_eq!(generated_database_count, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn expanded_invariant_role_blocks_provisioning_without_automatic_repair() {
        let postgres = test_postgres().await;
        postgres
            .ensure_invariant_role()
            .await
            .expect("the initial isolated invariant role is safe");
        let mut maintenance = postgres
            .connect_database("postgres")
            .await
            .expect("the isolated maintenance database is reachable");
        maintenance
            .client()
            .batch_execute("ALTER ROLE tiv_invariant LOGIN")
            .await
            .expect("the test expands the invariant role");
        maintenance
            .close()
            .await
            .expect("the maintenance session closes");

        let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
        let result = postgres
            .provision_reference_databases(&suffix, test_project())
            .await;

        let mut cleanup = postgres
            .connect_database("postgres")
            .await
            .expect("the isolated maintenance database remains reachable");
        let state = cleanup
            .client()
            .query_one(
                "SELECT role.rolcanlogin, \
                        COUNT(database.oid)::bigint \
                 FROM pg_roles AS role \
                 LEFT JOIN pg_database AS database \
                   ON database.datname IN ($1, $2) \
                 WHERE role.rolname = 'tiv_invariant' \
                 GROUP BY role.rolcanlogin",
                &[&format!("tiv_base_{suffix}"), &format!("tiv_case_{suffix}")],
            )
            .await
            .expect("the rejected provisioning state is observable");
        cleanup
            .client()
            .batch_execute("ALTER ROLE tiv_invariant NOLOGIN")
            .await
            .expect("the test restores the invariant role");
        cleanup.close().await.expect("the cleanup session closes");

        assert!(matches!(
            result,
            Err(SpikePostgresError::UnsafeInvariantRole)
        ));
        assert!(
            state.get::<_, bool>(0),
            "provisioning must not repair an expanded role"
        );
        assert_eq!(state.get::<_, i64>(1), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn reference_app_connection_rejects_the_generic_truth_spike_cluster() {
        let port = std::env::var("TIV_POSTGRES_TEST_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(15_432);
        let result = TruthSpikePostgres::connect(SpikePostgresConfig::loopback_reference_app(
            port,
            "tiv_admin",
            "tiv-local-only-password",
            "tiv-app-local-only-password",
        ))
        .await;

        assert!(matches!(
            result,
            Err(SpikePostgresError::UnexpectedServerIdentity)
        ));
    }

    async fn assert_application_role_contract(
        postgres: &TruthSpikePostgres,
        case_name: &DatabaseName,
    ) {
        let operation_id = reference_operation_id(case_name);
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
                 VALUES ($1, 'pi_tiv_role_test', 2500, 'usd', 'succeeded')",
                &[&operation_id],
            )
            .await
            .expect("the app can persist its one required relation");
        assert_durable_operation_read(app.client(), &operation_id).await;
        let marker_read = app
            .client()
            .query("SELECT marker_uuid FROM tiv_verifier_marker", &[])
            .await;
        let reconciled = app
            .client()
            .execute(
                "UPDATE payments SET status = 'succeeded' \
                 WHERE operation_id = $1 \
                   AND stripe_payment_intent_id = 'pi_tiv_role_test'",
                &[&operation_id],
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
        let ungranted_order_read = app.client().query("SELECT status FROM orders", &[]).await;
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
        assert!(
            ungranted_order_read.is_err(),
            "the app cannot read mutable order state while recovering routing"
        );
    }

    async fn assert_durable_operation_read(client: &Client, operation_id: &str) {
        let order = client
            .query_one(
                "SELECT operation_id, amount_minor, currency FROM orders WHERE operation_id = $1",
                &[&operation_id],
            )
            .await
            .expect("the app can recover its durable operation relation");
        assert_eq!(order.get::<_, String>(0), operation_id);
        assert_eq!(order.get::<_, i64>(1), 2_500);
        assert_eq!(order.get::<_, String>(2), "usd");
    }

    async fn assert_application_role_after_reset(
        postgres: &TruthSpikePostgres,
        case_name: &DatabaseName,
    ) {
        let operation_id = reference_operation_id(case_name);
        let mut reset_app = postgres
            .connect_application_database(case_name)
            .await
            .expect("the app role can connect after reset");
        let reset_insert = reset_app
            .client()
            .execute(
                "INSERT INTO payments \
                     (operation_id, stripe_payment_intent_id, amount_minor, currency, status) \
                 VALUES ($1, 'pi_tiv_role_test_after_reset', 2500, 'usd', 'succeeded')",
                &[&operation_id],
            )
            .await
            .expect("the narrow grant survives a template reset");
        reset_app
            .close()
            .await
            .expect("the reset application connection closes cleanly");
        assert_eq!(reset_insert, 1);
    }

    fn reference_operation_id(case_name: &DatabaseName) -> String {
        format!(
            "op_{}",
            case_name
                .as_str()
                .strip_prefix("tiv_case_")
                .expect("generated case keeps its validated prefix")
        )
    }

    async fn assert_invariant_role_contract(
        postgres: &TruthSpikePostgres,
        case_name: &DatabaseName,
    ) {
        use crate::postgres::snapshot::{InvariantRoleName, attest_invariant_role};

        let mut admin = postgres
            .connect_database(case_name.as_str())
            .await
            .expect("the isolated case database accepts its admin role");
        let role = InvariantRoleName::new("tiv_invariant")
            .expect("the fixed truth-spike invariant role is valid");
        let attestation = attest_invariant_role(admin.client(), role).await;
        assert!(
            attestation.is_ok(),
            "the provisioned invariant role must satisfy fresh attestation"
        );

        let grants = admin
            .client()
            .query_one(
                "SELECT has_schema_privilege('tiv_invariant', 'public', 'USAGE'), \
                        has_schema_privilege('tiv_invariant', 'public', 'CREATE'), \
                        has_table_privilege('tiv_invariant', 'orders', 'SELECT'), \
                        has_table_privilege('tiv_invariant', 'payments', 'SELECT'), \
                        has_table_privilege( \
                            'tiv_invariant', 'tiv_verifier_marker', 'SELECT' \
                        ), \
                        has_table_privilege('tiv_invariant', 'payments', 'INSERT'), \
                        has_database_privilege( \
                            'tiv_invariant', current_database(), 'TEMP' \
                        )",
                &[],
            )
            .await
            .expect("the invariant role grants are observable");
        assert!(grants.get::<_, bool>(0));
        assert!(!grants.get::<_, bool>(1));
        assert!(grants.get::<_, bool>(2));
        assert!(grants.get::<_, bool>(3));
        assert!(!grants.get::<_, bool>(4));
        assert!(!grants.get::<_, bool>(5));
        assert!(!grants.get::<_, bool>(6));

        let transaction = admin
            .client()
            .build_transaction()
            .read_only(true)
            .start()
            .await
            .expect("the invariant proof transaction starts read-only");
        transaction
            .batch_execute("SET LOCAL ROLE tiv_invariant")
            .await
            .expect("the isolated admin can drop to the invariant role");
        let effective = transaction
            .query_one(
                "SELECT current_user::text, COUNT(*)::bigint FROM orders",
                &[],
            )
            .await
            .expect("the invariant role can read application state");
        assert_eq!(effective.get::<_, &str>(0), "tiv_invariant");
        assert_eq!(effective.get::<_, i64>(1), 1);
        transaction
            .rollback()
            .await
            .expect("the invariant proof transaction rolls back");
        admin
            .close()
            .await
            .expect("the invariant-role admin session closes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires the isolated reference-app Compose project"]
    async fn real_reference_app_runs_three_fresh_commit_close_attempts() {
        let postgres = test_reference_postgres().await;
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
        let second_case_target = postgres
            .reset_case_from_template(
                provisioned.into_case_target(),
                &baseline_target,
                Uuid::new_v4(),
            )
            .await
            .expect("the real app case resets from the sealed template");
        let second_database_oid = second_case_target.identity().database_oid();

        let second_report = run_reference_app_checkout(&postgres, &case_name, &plan, 3).await;
        let second_identity = provider_uniqueness_failure(&second_report)
            .identity()
            .clone();
        let third_case_target = postgres
            .reset_case_from_template(second_case_target, &baseline_target, Uuid::new_v4())
            .await
            .expect("the second real app case resets from the sealed template");
        let third_database_oid = third_case_target.identity().database_oid();
        let third_report = run_reference_app_checkout(&postgres, &case_name, &plan, 5).await;
        let third_identity = provider_uniqueness_failure(&third_report)
            .identity()
            .clone();
        let attempts = [
            AttemptResult::Violation(first_identity.clone()),
            AttemptResult::Violation(second_identity),
            AttemptResult::Violation(third_identity),
        ];
        let evidence = TruthSpikeEvidence::new(
            [2, 2, 2],
            [first_database_oid, second_database_oid, third_database_oid],
            &first_identity,
            &attempts,
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

    async fn test_postgres_with_archive() -> TruthSpikePostgres {
        let port = std::env::var("TIV_POSTGRES_TEST_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(15_432);
        let mut command = StdCommand::new("docker");
        let output = command
            .args([
                "--host",
                "unix:///var/run/docker.sock",
                "ps",
                "--filter",
                "label=com.docker.compose.project=tiv-truth-spike-postgres",
                "--filter",
                "label=com.docker.compose.service=postgres",
                "--format",
                "{{.ID}}",
            ])
            .env_remove("DOCKER_HOST")
            .env_remove("DOCKER_CONTEXT")
            .env_remove("DOCKER_TLS_VERIFY")
            .env_remove("DOCKER_CERT_PATH")
            .output()
            .expect("local Docker is available");
        assert!(
            output.status.success(),
            "the isolated container is discoverable"
        );
        let container_id = String::from_utf8(output.stdout)
            .expect("Docker emits UTF-8")
            .trim()
            .to_owned();
        assert!(
            !container_id.contains('\n'),
            "exactly one container is running"
        );
        TruthSpikePostgres::connect(SpikePostgresConfig::loopback_with_archive_container(
            port,
            "tiv_admin",
            "tiv-local-only-password",
            "tiv-app-local-only-password",
            container_id,
        ))
        .await
        .expect("the isolated PostgreSQL archive toolchain is compatible")
    }

    async fn test_reference_postgres() -> TruthSpikePostgres {
        let port = std::env::var("TIV_POSTGRES_TEST_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(15_432);
        TruthSpikePostgres::connect(SpikePostgresConfig::loopback_reference_app(
            port,
            "tiv_admin",
            "tiv-local-only-password",
            "tiv-app-local-only-password",
        ))
        .await
        .expect("the isolated reference PostgreSQL fixture is healthy")
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
        let operation_id = format!(
            "op_{}",
            case_name
                .as_str()
                .strip_prefix("tiv_case_")
                .expect("generated case keeps its validated prefix")
        );
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
            .insert_buggy_payment_pair(case_target, &operation_id, &provider_objects)
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
        let control_base = std::env::var("TIV_FIXTURE_CONTROL_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:12112".to_owned());
        let app_base = std::env::var("TIV_REFERENCE_APP_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:18080".to_owned());
        let config = ReferenceAppReplayConfig::new(
            case_name.clone(),
            &app_base,
            &control_base,
            "run-scoped-control-token",
            reset_sequence,
            reset_sequence + 1,
            current_unix_timestamp(),
        )
        .expect("the reference app replay config is valid");
        let receipt = run_reference_app_replay(plan, &config)
            .await
            .expect("the trace drives the real reference app");
        let provider_objects = receipt
            .provider_payment_intents()
            .iter()
            .map(|payment_intent| {
                ProviderPaymentIntent::new(
                    payment_intent.id(),
                    payment_intent.amount_minor(),
                    payment_intent.currency(),
                    payment_intent.status(),
                )
                .expect("the fixture projection is valid")
            })
            .collect::<Vec<_>>();

        postgres
            .check_reference_invariants(case_name, &provider_objects, quiescence())
            .await
            .expect("the real app path reaches the five-query oracle")
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

    fn current_unix_timestamp() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("the system clock is after the Unix epoch")
                .as_secs(),
        )
        .expect("the current Unix timestamp fits in i64")
    }
}
