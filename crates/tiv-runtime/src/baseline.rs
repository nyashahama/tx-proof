//! Attested, acknowledgement-bound customer baseline sealing and reset proof.

use std::{
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinHandle,
    time::timeout,
};
use tokio_postgres::NoTls;
use url::Url;
use uuid::Uuid;

use crate::{
    config::{ConfigError, EnvironmentLookup, ResolvedConfig, load_resolved_config},
    postgres::safety::{
        DatabaseEndpoint, DatabaseIdentity, DatabaseMarker, DatabaseName, DatabaseTarget,
        InvalidDatabaseIdentity, InvalidDatabaseName, MarkerKind, MutationPermit,
        ResetAcknowledgementError, ResetChallenge, SafetyError, Unverified, Verified,
    },
};

const LOCAL_DOCKER_HOST: &str = "unix:///var/run/docker.sock";
const POSTGRES_CONTAINER_PORT: &str = "5432/tcp";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
const BASELINE_SCHEMA_VERSION: u16 = 1;

/// One allowlisted result from either stage of `tiv baseline`.
#[derive(Serialize)]
#[serde(untagged)]
pub enum BaselineOutput {
    Challenge(BaselineChallengeReport),
    Ready(BaselineReport),
}

impl BaselineOutput {
    /// Serializes the secret-free command result.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if the allowlisted report cannot encode.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[derive(Serialize)]
pub struct BaselineChallengeReport {
    schema_version: u16,
    status: &'static str,
    mutation_authorized: bool,
    case_database: String,
    compose_project: String,
    reset_acknowledgement: String,
}

#[derive(Serialize)]
pub struct BaselineReport {
    schema_version: u16,
    status: &'static str,
    mutation_authorized: bool,
    compose_project: String,
    baseline_database: BaselineDatabaseReport,
    case_database: CaseDatabaseReport,
    reset_proof: ResetProofReport,
}

/// A freshly attested sealed baseline and its current disposable case.
///
/// The session deliberately implements neither `Debug` nor `Serialize`: it
/// retains the secret-bearing connection and exact Compose boundary required
/// to re-attest and reset one case at a time.
pub struct ConfiguredBaselineSession {
    inputs: BaselineInputs,
    postgres_container_id: String,
    baseline_identity: DatabaseIdentity,
    case_target: DatabaseTarget<Unverified>,
}

impl ConfiguredBaselineSession {
    /// Re-attests the configured local `PostgreSQL` container, current case
    /// identity, and both catalog and in-database sealed-baseline markers.
    /// Baseline contents are inspected through a short-lived disposable clone,
    /// so the sealed baseline itself never accepts a connection.
    ///
    /// # Errors
    ///
    /// Returns [`BaselineError`] unless the complete case and baseline
    /// identities belong to the same configured disposable boundary.
    pub async fn attest(config: &ResolvedConfig) -> Result<Self, BaselineError> {
        let inputs = BaselineInputs::from_config(config)?;
        let stack = attest_postgres_container(&inputs).await?;
        let case_identity = observe_case_identity(&inputs).await?;
        let baseline_identity = observe_sealed_baseline_identity(&inputs).await?;
        require_compatible_baseline(&case_identity, &baseline_identity)?;
        Ok(Self {
            inputs,
            postgres_container_id: stack.container_id,
            baseline_identity,
            case_target: DatabaseTarget::new(case_identity),
        })
    }

    #[must_use]
    pub const fn case_identity(&self) -> &DatabaseIdentity {
        self.case_target.identity()
    }

    #[must_use]
    pub const fn baseline_identity(&self) -> &DatabaseIdentity {
        &self.baseline_identity
    }

    /// Stops configured database clients, re-attests both identities, consumes
    /// one process-local mutation permit, recreates the case from the sealed
    /// baseline, reapplies database privileges and restores the services.
    ///
    /// Consuming `self` prevents a stale pre-reset identity from authorizing a
    /// second mutation. A successful call returns the freshly attested session.
    ///
    /// # Errors
    ///
    /// Returns [`BaselineError`] for any safety, lifecycle, reset, recovery, or
    /// post-reset identity failure.
    pub async fn reset_case(self) -> Result<(Self, ConfiguredCaseResetReport), BaselineError> {
        let Self {
            inputs,
            postgres_container_id,
            baseline_identity,
            case_target,
        } = self;
        if let Err(stop_error) = stop_customer_services(&inputs).await {
            return match start_customer_services(&inputs).await {
                Ok(()) => Err(stop_error),
                Err(_) => Err(BaselineError::CustomerServicesRecoveryFailed),
            };
        }
        let execution = async {
            let fresh_stack = attest_postgres_container(&inputs).await?;
            if fresh_stack.container_id != postgres_container_id {
                return Err(BaselineError::PostgresContainerChanged);
            }
            let fresh_case = observe_case_identity(&inputs).await?;
            let (verified, permit) = case_target
                .verify(&fresh_case)
                .map_err(BaselineError::Safety)?;
            let fresh_baseline = observe_sealed_baseline_identity(&inputs).await?;
            if fresh_baseline != baseline_identity {
                return Err(BaselineError::BaselineIdentityMismatch);
            }
            require_compatible_baseline(verified.identity(), &fresh_baseline)?;
            reset_verified_case(&inputs, verified, permit, &fresh_baseline).await
        }
        .await;
        if matches!(&execution, Err(BaselineError::CaseDatabaseRecoveryFailed)) {
            // The exact original case could not be proven restored. Keeping
            // database clients stopped is safer than starting them against an
            // absent, partial, or unverified replacement.
            return Err(BaselineError::CaseDatabaseRecoveryFailed);
        }
        let restart = start_customer_services(&inputs).await;
        match (execution, restart) {
            (Ok((case_identity, report)), Ok(())) => {
                let final_stack = attest_postgres_container(&inputs).await?;
                if final_stack.container_id != postgres_container_id {
                    return Err(BaselineError::PostgresContainerChanged);
                }
                Ok((
                    Self {
                        inputs,
                        postgres_container_id,
                        baseline_identity,
                        case_target: DatabaseTarget::new(case_identity),
                    },
                    report,
                ))
            }
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(_), Err(_)) => Err(BaselineError::CustomerServicesRecoveryFailed),
        }
    }
}

/// Allowlisted identity delta proving one configured case was freshly cloned.
#[derive(Serialize)]
pub struct ConfiguredCaseResetReport {
    before_database_oid: u32,
    after_database_oid: u32,
    before_marker_uuid: String,
    after_marker_uuid: String,
}

impl ConfiguredCaseResetReport {
    #[must_use]
    pub const fn before_database_oid(&self) -> u32 {
        self.before_database_oid
    }

    #[must_use]
    pub const fn after_database_oid(&self) -> u32 {
        self.after_database_oid
    }

    #[must_use]
    pub fn before_marker_uuid(&self) -> &str {
        &self.before_marker_uuid
    }

    #[must_use]
    pub fn after_marker_uuid(&self) -> &str {
        &self.after_marker_uuid
    }
}

#[derive(Serialize)]
struct BaselineDatabaseReport {
    name: String,
    database_oid: u32,
    sealed_template: bool,
}

#[derive(Serialize)]
struct CaseDatabaseReport {
    name: String,
    before_oid: u32,
    after_oid: u32,
    before_marker_uuid: String,
    after_marker_uuid: String,
}

#[derive(Serialize)]
struct ResetProofReport {
    probe_removed: bool,
    seed_source: &'static str,
}

/// Loads one typed project, attests its running local `PostgreSQL` service, and
/// either emits an exact reset challenge or consumes that challenge to seal
/// and reset-prove the configured baseline.
///
/// # Errors
///
/// Returns [`BaselineError`] before mutation for invalid configuration,
/// Docker attestation, database identity, or acknowledgement. After consent,
/// lifecycle and `PostgreSQL` failures remain bounded and services are restarted.
pub async fn run_configured_baseline(
    path: &Path,
    environment: &impl EnvironmentLookup,
    acknowledgement: Option<&str>,
) -> Result<BaselineOutput, BaselineError> {
    let config = load_resolved_config(path, environment)?;
    let inputs = BaselineInputs::from_config(&config)?;
    let initial_stack = attest_postgres_container(&inputs).await?;
    let initial_identity = observe_case_identity(&inputs).await?;
    require_baseline_absent(&inputs).await?;
    let challenge = ResetChallenge::new(initial_identity.clone())?;

    let Some(acknowledgement) = acknowledgement else {
        return Ok(BaselineOutput::Challenge(BaselineChallengeReport {
            schema_version: BASELINE_SCHEMA_VERSION,
            status: "acknowledgement_required",
            mutation_authorized: false,
            case_database: inputs.case_name.as_str().to_owned(),
            compose_project: inputs.compose_project.clone(),
            reset_acknowledgement: challenge.phrase().to_owned(),
        }));
    };

    let authorization = challenge.acknowledge(acknowledgement)?;
    if let Err(stop_error) = stop_customer_services(&inputs).await {
        // Compose can stop a subset before returning an error. Make one
        // bounded recovery attempt rather than leaving that partial state.
        return match start_customer_services(&inputs).await {
            Ok(()) => Err(stop_error),
            Err(_) => Err(BaselineError::CustomerServicesRecoveryFailed),
        };
    }
    let execution = async {
        let fresh_stack = attest_postgres_container(&inputs).await?;
        if fresh_stack.container_id != initial_stack.container_id {
            return Err(BaselineError::PostgresContainerChanged);
        }
        let fresh_identity = observe_case_identity(&inputs).await?;
        let (verified, permit) = authorization.authorize(&fresh_identity)?;
        require_baseline_absent(&inputs).await?;
        seal_and_reset_prove(&inputs, verified, permit).await
    }
    .await;
    let restart = start_customer_services(&inputs).await;
    match (execution, restart) {
        (Ok(report), Ok(())) => {
            let final_stack = attest_postgres_container(&inputs).await?;
            if final_stack.container_id != initial_stack.container_id {
                return Err(BaselineError::PostgresContainerChanged);
            }
            Ok(BaselineOutput::Ready(report))
        }
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(_), Err(_)) => Err(BaselineError::CustomerServicesRecoveryFailed),
    }
}

struct BaselineInputs {
    admin_url: Url,
    case_name: DatabaseName,
    baseline_name: DatabaseName,
    admin_role: String,
    application_role: String,
    compose_project: String,
    root: PathBuf,
    compose_files: Vec<PathBuf>,
    application_service: String,
    postgres_service: String,
    worker_services: Vec<String>,
    invariant_role: String,
    port: u16,
    max_database_bytes: u64,
}

impl BaselineInputs {
    fn from_config(config: &ResolvedConfig) -> Result<Self, BaselineError> {
        let admin_url = config.admin_url().clone();
        let case_url = config.case_url();
        let admin_port = exact_loopback_port(&admin_url)?;
        let case_port = exact_loopback_port(case_url)?;
        if admin_port != case_port
            || admin_url.path().trim_start_matches('/') != "postgres"
            || admin_url.query().is_some()
            || case_url.query().is_some()
            || admin_url.password().is_none_or(str::is_empty)
            || case_url.password().is_none_or(str::is_empty)
            || !valid_role_name(admin_url.username())
            || !valid_role_name(case_url.username())
        {
            return Err(BaselineError::UnsafeDatabaseConnection);
        }
        let case_name = DatabaseName::parse(config.case_database())?;
        let baseline_name = DatabaseName::parse(config.baseline_database())?;
        if case_name.kind() != crate::postgres::safety::DatabaseKind::Case
            || baseline_name.kind() != crate::postgres::safety::DatabaseKind::Baseline
        {
            return Err(BaselineError::UnsafeDatabaseConnection);
        }
        Ok(Self {
            admin_url,
            case_name,
            baseline_name,
            admin_role: config.admin_url().username().to_owned(),
            application_role: case_url.username().to_owned(),
            compose_project: config.compose_project().to_owned(),
            root: config.root().to_owned(),
            compose_files: config.compose_files().to_vec(),
            application_service: config.application_service().to_owned(),
            postgres_service: config.postgres_service().to_owned(),
            worker_services: config.worker_services().to_vec(),
            invariant_role: config.invariant_role().to_owned(),
            port: admin_port,
            max_database_bytes: config.max_database_bytes(),
        })
    }

    fn database_url(&self, database: &DatabaseName) -> Url {
        let mut url = self.admin_url.clone();
        url.set_path(&format!("/{}", database.as_str()));
        url
    }
}

fn exact_loopback_port(url: &Url) -> Result<u16, BaselineError> {
    url.host_str()
        .filter(|host| matches!(*host, "127.0.0.1" | "localhost"))
        .ok_or(BaselineError::UnsafeDatabaseConnection)?;
    url.port().ok_or(BaselineError::UnsafeDatabaseConnection)
}

fn valid_role_name(role: &str) -> bool {
    let mut bytes = role.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    role.len() <= 63
        && (first.is_ascii_lowercase() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

struct PostgresContainerAttestation {
    container_id: String,
}

async fn attest_postgres_container(
    inputs: &BaselineInputs,
) -> Result<PostgresContainerAttestation, BaselineError> {
    let project_filter = format!(
        "label=com.docker.compose.project={}",
        inputs.compose_project
    );
    let service_filter = format!(
        "label=com.docker.compose.service={}",
        inputs.postgres_service
    );
    let ids = docker_output(&[
        "ps",
        "--filter",
        &project_filter,
        "--filter",
        &service_filter,
        "--format",
        "{{.ID}}",
    ])
    .await?;
    let ids = ids.lines().collect::<Vec<_>>();
    let [container_id] = ids.as_slice() else {
        return Err(BaselineError::PostgresContainerMismatch);
    };
    if !(12..=64).contains(&container_id.len())
        || !container_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(BaselineError::PostgresContainerMismatch);
    }
    let inspection = docker_output(&[
        "inspect",
        "--format",
        concat!(
            "{{.State.Running}}\n",
            "{{if .State.Health}}{{.State.Health.Status}}{{end}}\n",
            "{{index .Config.Labels \"com.docker.compose.project\"}}\n",
            "{{index .Config.Labels \"com.docker.compose.service\"}}\n",
            "{{json .NetworkSettings.Ports}}"
        ),
        container_id,
    ])
    .await?;
    validate_postgres_inspection(&inspection, inputs)?;
    Ok(PostgresContainerAttestation {
        container_id: (*container_id).to_owned(),
    })
}

fn validate_postgres_inspection(
    inspection: &str,
    inputs: &BaselineInputs,
) -> Result<(), BaselineError> {
    let mut lines = inspection.lines();
    if lines.next() != Some("true")
        || lines.next() != Some("healthy")
        || lines.next() != Some(inputs.compose_project.as_str())
        || lines.next() != Some(inputs.postgres_service.as_str())
    {
        return Err(BaselineError::PostgresContainerMismatch);
    }
    let ports: Value = serde_json::from_str(
        lines
            .next()
            .ok_or(BaselineError::PostgresContainerMismatch)?,
    )
    .map_err(|_| BaselineError::PostgresContainerMismatch)?;
    if lines.next().is_some() {
        return Err(BaselineError::PostgresContainerMismatch);
    }
    let bindings = ports
        .get(POSTGRES_CONTAINER_PORT)
        .and_then(Value::as_array)
        .ok_or(BaselineError::PostgresContainerMismatch)?;
    let [binding] = bindings.as_slice() else {
        return Err(BaselineError::PostgresContainerMismatch);
    };
    if binding.get("HostIp").and_then(Value::as_str) != Some("127.0.0.1")
        || binding
            .get("HostPort")
            .and_then(Value::as_str)
            .and_then(|port| port.parse::<u16>().ok())
            != Some(inputs.port)
    {
        return Err(BaselineError::PostgresContainerMismatch);
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "keep the cross-catalog and in-database identity attestation in one auditable sequence"
)]
async fn observe_case_identity(inputs: &BaselineInputs) -> Result<DatabaseIdentity, BaselineError> {
    let maintenance = PostgresSession::connect(&inputs.admin_url).await?;
    let row = maintenance
        .client
        .query_opt(
            "SELECT database.oid::bigint, database.datdba::bigint, \
                    pg_database_size(database.oid)::bigint, \
                    control.system_identifier::text, current_user, current_database(), \
                    owner.rolname \
             FROM pg_database AS database \
             JOIN pg_roles AS owner ON owner.oid = database.datdba \
             CROSS JOIN pg_control_system() AS control \
             WHERE database.datname = $1",
            &[&inputs.case_name.as_str()],
        )
        .await?
        .ok_or(BaselineError::CaseDatabaseMissing)?;
    if row.get::<_, &str>(4) != inputs.admin_role || row.get::<_, &str>(5) != "postgres" {
        return Err(BaselineError::UnexpectedDatabaseIdentity);
    }
    if row.get::<_, &str>(6) != inputs.admin_role || inputs.admin_role == inputs.application_role {
        return Err(BaselineError::UnsafeDatabaseOwner);
    }
    let database_oid = u32::try_from(row.get::<_, i64>(0))
        .map_err(|_| BaselineError::UnexpectedDatabaseIdentity)?;
    let owner_oid = u32::try_from(row.get::<_, i64>(1))
        .map_err(|_| BaselineError::UnexpectedDatabaseIdentity)?;
    let database_bytes = u64::try_from(row.get::<_, i64>(2))
        .map_err(|_| BaselineError::UnexpectedDatabaseIdentity)?;
    if database_bytes > inputs.max_database_bytes {
        return Err(BaselineError::DatabaseTooLarge);
    }
    let server_fingerprint = format!("postgres-system-id:{}", row.get::<_, String>(3));
    maintenance.close().await?;

    let case = PostgresSession::connect(&inputs.database_url(&inputs.case_name)).await?;
    let marker_rows = case
        .client
        .query(
            "SELECT marker_uuid, marker_kind, compose_project, application_role \
             FROM tiv_verifier_marker LIMIT 2",
            &[],
        )
        .await?;
    if marker_rows.len() != 1 {
        return Err(BaselineError::UnexpectedMarker);
    }
    let marker = &marker_rows[0];
    if marker.get::<_, &str>(1) != "case"
        || marker.get::<_, &str>(2) != inputs.compose_project
        || marker.get::<_, &str>(3) != inputs.application_role
    {
        return Err(BaselineError::UnexpectedMarker);
    }
    let role = case
        .client
        .query_opt(
            "SELECT role.rolcanlogin \
                    AND NOT role.rolsuper \
                    AND NOT role.rolcreatedb \
                    AND NOT role.rolcreaterole \
                    AND NOT role.rolinherit \
                    AND NOT role.rolreplication \
                    AND NOT role.rolbypassrls, \
                    NOT EXISTS ( \
                        SELECT 1 FROM pg_auth_members AS membership \
                        WHERE membership.roleid = role.oid OR membership.member = role.oid \
                    ), \
                    NOT has_table_privilege(role.oid, 'public.tiv_verifier_marker', 'INSERT') \
                    AND NOT has_table_privilege(role.oid, 'public.tiv_verifier_marker', 'UPDATE') \
                    AND NOT has_table_privilege(role.oid, 'public.tiv_verifier_marker', 'DELETE') \
                    AND NOT has_table_privilege(role.oid, 'public.tiv_verifier_marker', 'TRUNCATE') \
                    AND NOT has_table_privilege(role.oid, 'public.tiv_verifier_marker', 'REFERENCES') \
                    AND NOT has_table_privilege(role.oid, 'public.tiv_verifier_marker', 'TRIGGER') \
                    AND NOT has_table_privilege(role.oid, 'public.tiv_verifier_marker', 'MAINTAIN') \
             FROM pg_roles AS role WHERE role.rolname = $1",
            &[&inputs.application_role],
        )
        .await?
        .ok_or(BaselineError::UnsafeApplicationRole)?;
    if !role.get::<_, bool>(0) || !role.get::<_, bool>(1) || !role.get::<_, bool>(2) {
        return Err(BaselineError::UnsafeApplicationRole);
    }
    let reset_probe = case
        .client
        .query_one("SELECT to_regclass('public.tiv_reset_probe') IS NULL", &[])
        .await?
        .get::<_, bool>(0);
    if !reset_probe {
        return Err(BaselineError::ResetProbeAlreadyExists);
    }
    case.close().await?;

    DatabaseIdentity::new(
        server_fingerprint,
        DatabaseEndpoint::loopback(inputs.port),
        inputs.case_name.clone(),
        database_oid,
        owner_oid,
        DatabaseMarker::new(
            marker.get::<_, Uuid>(0),
            MarkerKind::Case,
            crate::postgres::safety::ComposeProjectId::new(inputs.compose_project.clone())
                .map_err(|_| BaselineError::UnexpectedMarker)?,
        ),
        inputs.application_role.clone(),
    )
    .map_err(BaselineError::InvalidIdentity)
}

async fn require_baseline_absent(inputs: &BaselineInputs) -> Result<(), BaselineError> {
    let maintenance = PostgresSession::connect(&inputs.admin_url).await?;
    let exists = maintenance
        .client
        .query_opt(
            "SELECT 1::integer FROM pg_database WHERE datname = $1",
            &[&inputs.baseline_name.as_str()],
        )
        .await?
        .is_some();
    maintenance.close().await?;
    if exists {
        Err(BaselineError::BaselineAlreadyExists)
    } else {
        Ok(())
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "keep the authorization-consuming baseline mutation and reset proof in exact execution order"
)]
async fn seal_and_reset_prove(
    inputs: &BaselineInputs,
    verified: DatabaseTarget<Verified>,
    _permit: MutationPermit,
) -> Result<BaselineReport, BaselineError> {
    let before = verified.identity();
    let before_oid = before.database_oid();
    let before_marker_uuid = before.marker().marker_uuid();
    let baseline_marker_uuid = Uuid::new_v4();
    let after_marker_uuid = Uuid::new_v4();

    let maintenance = PostgresSession::connect(&inputs.admin_url).await?;
    let case = inputs.case_name.as_str();
    let baseline = inputs.baseline_name.as_str();
    maintenance
        .client
        .batch_execute(&format!("ALTER DATABASE {case} ALLOW_CONNECTIONS false"))
        .await?;
    let clone_result = async {
        maintenance
            .client
            .execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                 WHERE datid::bigint = $1 AND pid <> pg_backend_pid()",
                &[&i64::from(before_oid)],
            )
            .await?;
        maintenance
            .client
            .batch_execute(&format!(
                "CREATE DATABASE {baseline} WITH OWNER = {} TEMPLATE = {case}",
                inputs.admin_role
            ))
            .await
    }
    .await;
    // Restore customer connectivity even when cloning fails after the case
    // database has been fenced against new sessions.
    let restore_connections = maintenance
        .client
        .batch_execute(&format!("ALTER DATABASE {case} ALLOW_CONNECTIONS true"))
        .await;
    let session_close_result = maintenance.close().await;
    clone_result?;
    restore_connections?;
    session_close_result?;

    let baseline_session =
        PostgresSession::connect(&inputs.database_url(&inputs.baseline_name)).await?;
    if baseline_session
        .client
        .execute(
            "UPDATE tiv_verifier_marker \
             SET marker_uuid = $1, marker_kind = 'baseline'",
            &[&baseline_marker_uuid],
        )
        .await?
        != 1
    {
        return Err(BaselineError::UnexpectedMarker);
    }
    baseline_session.close().await?;

    let case_session = PostgresSession::connect(&inputs.database_url(&inputs.case_name)).await?;
    case_session
        .client
        .batch_execute(
            "CREATE TABLE tiv_reset_probe (probe_uuid uuid PRIMARY KEY); \
             INSERT INTO tiv_reset_probe VALUES (gen_random_uuid())",
        )
        .await?;
    case_session.close().await?;

    let maintenance = PostgresSession::connect(&inputs.admin_url).await?;
    let catalog_marker = format!(
        "tiv-baseline:v1:{baseline_marker_uuid}:{}:{}",
        inputs.compose_project, inputs.application_role
    );
    let quoted_marker = maintenance
        .client
        .query_one("SELECT quote_literal($1::text)", &[&catalog_marker])
        .await?
        .get::<_, String>(0);
    maintenance
        .client
        .batch_execute(&format!(
            "COMMENT ON DATABASE {baseline} IS {quoted_marker}; \
             ALTER DATABASE {baseline} IS_TEMPLATE true; \
             ALTER DATABASE {baseline} ALLOW_CONNECTIONS false"
        ))
        .await?;
    maintenance
        .client
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datid::bigint = $1 AND pid <> pg_backend_pid()",
            &[&i64::from(before_oid)],
        )
        .await?;
    maintenance
        .client
        .batch_execute(&format!("DROP DATABASE {case}"))
        .await?;
    maintenance
        .client
        .batch_execute(&format!(
            "CREATE DATABASE {case} WITH OWNER = {} TEMPLATE = {baseline}",
            inputs.admin_role
        ))
        .await?;
    maintenance.close().await?;

    let reset = PostgresSession::connect(&inputs.database_url(&inputs.case_name)).await?;
    if reset
        .client
        .execute(
            "UPDATE tiv_verifier_marker SET marker_uuid = $1, marker_kind = 'case'",
            &[&after_marker_uuid],
        )
        .await?
        != 1
    {
        return Err(BaselineError::UnexpectedMarker);
    }
    let probe_removed = reset
        .client
        .query_one("SELECT to_regclass('public.tiv_reset_probe') IS NULL", &[])
        .await?
        .get::<_, bool>(0);
    reset.close().await?;
    if !probe_removed {
        return Err(BaselineError::ResetProofFailed);
    }

    let after = observe_case_identity(inputs).await?;
    if after.database_oid() == before_oid
        || after.owner_oid() != before.owner_oid()
        || after.server_fingerprint() != before.server_fingerprint()
        || after.marker().compose_project() != before.marker().compose_project()
        || after.expected_application_role() != before.expected_application_role()
        || after.marker().marker_uuid() != after_marker_uuid
    {
        return Err(BaselineError::ResetProofFailed);
    }
    let baseline_identity = observe_sealed_baseline_identity(inputs).await?;
    if baseline_identity.marker().marker_uuid() != baseline_marker_uuid
        || baseline_identity.server_fingerprint() != after.server_fingerprint()
        || baseline_identity.endpoint() != after.endpoint()
        || baseline_identity.owner_oid() != after.owner_oid()
        || baseline_identity.marker().compose_project() != after.marker().compose_project()
        || baseline_identity.expected_application_role() != after.expected_application_role()
    {
        return Err(BaselineError::ResetProofFailed);
    }
    Ok(BaselineReport {
        schema_version: BASELINE_SCHEMA_VERSION,
        status: "baseline_ready",
        mutation_authorized: true,
        compose_project: inputs.compose_project.clone(),
        baseline_database: BaselineDatabaseReport {
            name: baseline.to_owned(),
            database_oid: baseline_identity.database_oid(),
            sealed_template: true,
        },
        case_database: CaseDatabaseReport {
            name: case.to_owned(),
            before_oid,
            after_oid: after.database_oid(),
            before_marker_uuid: before_marker_uuid.to_string(),
            after_marker_uuid: after_marker_uuid.to_string(),
        },
        reset_proof: ResetProofReport {
            probe_removed: true,
            seed_source: "sealed_baseline",
        },
    })
}

async fn observe_sealed_baseline_identity(
    inputs: &BaselineInputs,
) -> Result<DatabaseIdentity, BaselineError> {
    let before = observe_sealed_baseline_catalog_identity(inputs).await?;
    attest_sealed_baseline_contents(inputs, &before).await?;
    let after = observe_sealed_baseline_catalog_identity(inputs).await?;
    if after != before {
        return Err(BaselineError::BaselineIdentityMismatch);
    }
    Ok(after)
}

async fn observe_sealed_baseline_catalog_identity(
    inputs: &BaselineInputs,
) -> Result<DatabaseIdentity, BaselineError> {
    let maintenance = PostgresSession::connect(&inputs.admin_url).await?;
    let row = maintenance
        .client
        .query_opt(
            "SELECT database.oid::bigint, database.datdba::bigint, \
                    pg_database_size(database.oid)::bigint, \
                    control.system_identifier::text, current_user, current_database(), \
                    owner.rolname, database.datistemplate, NOT database.datallowconn, \
                    COALESCE(shobj_description(database.oid, 'pg_database'), '') \
             FROM pg_database AS database \
             JOIN pg_roles AS owner ON owner.oid = database.datdba \
             CROSS JOIN pg_control_system() AS control \
             WHERE database.datname = $1",
            &[&inputs.baseline_name.as_str()],
        )
        .await?
        .ok_or(BaselineError::BaselineNotReady)?;
    if row.get::<_, &str>(4) != inputs.admin_role
        || row.get::<_, &str>(5) != "postgres"
        || row.get::<_, &str>(6) != inputs.admin_role
        || !row.get::<_, bool>(7)
        || !row.get::<_, bool>(8)
    {
        return Err(BaselineError::BaselineIdentityMismatch);
    }
    let database_oid = u32::try_from(row.get::<_, i64>(0))
        .map_err(|_| BaselineError::UnexpectedDatabaseIdentity)?;
    let owner_oid = u32::try_from(row.get::<_, i64>(1))
        .map_err(|_| BaselineError::UnexpectedDatabaseIdentity)?;
    let database_bytes = u64::try_from(row.get::<_, i64>(2))
        .map_err(|_| BaselineError::UnexpectedDatabaseIdentity)?;
    if database_bytes > inputs.max_database_bytes {
        return Err(BaselineError::DatabaseTooLarge);
    }
    let marker_uuid = parse_baseline_catalog_marker(
        row.get(9),
        &inputs.compose_project,
        &inputs.application_role,
    )?;
    let server_fingerprint = format!("postgres-system-id:{}", row.get::<_, String>(3));
    maintenance.close().await?;
    DatabaseIdentity::new(
        server_fingerprint,
        DatabaseEndpoint::loopback(inputs.port),
        inputs.baseline_name.clone(),
        database_oid,
        owner_oid,
        DatabaseMarker::new(
            marker_uuid,
            MarkerKind::Baseline,
            crate::postgres::safety::ComposeProjectId::new(inputs.compose_project.clone())
                .map_err(|_| BaselineError::BaselineIdentityMismatch)?,
        ),
        inputs.application_role.clone(),
    )
    .map_err(BaselineError::InvalidIdentity)
}

async fn attest_sealed_baseline_contents(
    inputs: &BaselineInputs,
    expected: &DatabaseIdentity,
) -> Result<(), BaselineError> {
    let clone_name = temporary_case_name("attest")?;
    let clone_identity = create_database_from_template(
        inputs,
        &clone_name,
        expected.database_name(),
        expected.owner_oid(),
    )
    .await?;
    let inspection = inspect_baseline_clone_marker(inputs, &clone_name, expected).await;
    let cleanup = drop_database_exact(inputs, &clone_name, clone_identity).await;
    match (inspection, cleanup) {
        (_, Err(_)) => Err(BaselineError::TemporaryDatabaseCleanupFailed),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn inspect_baseline_clone_marker(
    inputs: &BaselineInputs,
    clone_name: &DatabaseName,
    expected: &DatabaseIdentity,
) -> Result<(), BaselineError> {
    let clone = PostgresSession::connect(&inputs.database_url(clone_name)).await?;
    let rows = clone
        .client
        .query(
            "SELECT marker_uuid, marker_kind, compose_project, application_role \
             FROM tiv_verifier_marker LIMIT 2",
            &[],
        )
        .await
        .map_err(|_| BaselineError::BaselineIdentityMismatch)?;
    let reset_probe_absent = clone
        .client
        .query_one("SELECT to_regclass('public.tiv_reset_probe') IS NULL", &[])
        .await
        .map_err(|_| BaselineError::BaselineIdentityMismatch)?
        .get::<_, bool>(0);
    clone.close().await?;
    let [marker] = rows.as_slice() else {
        return Err(BaselineError::BaselineIdentityMismatch);
    };
    if marker.get::<_, Uuid>(0) != expected.marker().marker_uuid()
        || marker.get::<_, &str>(1) != "baseline"
        || marker.get::<_, &str>(2) != inputs.compose_project
        || marker.get::<_, &str>(3) != inputs.application_role
        || !reset_probe_absent
    {
        return Err(BaselineError::BaselineIdentityMismatch);
    }
    Ok(())
}

fn parse_baseline_catalog_marker(
    value: &str,
    expected_project: &str,
    expected_application_role: &str,
) -> Result<Uuid, BaselineError> {
    let fields = value
        .strip_prefix("tiv-baseline:v1:")
        .ok_or(BaselineError::BaselineIdentityMismatch)?
        .split(':')
        .collect::<Vec<_>>();
    let [marker_uuid, project, application_role] = fields.as_slice() else {
        return Err(BaselineError::BaselineIdentityMismatch);
    };
    if *project != expected_project || *application_role != expected_application_role {
        return Err(BaselineError::BaselineIdentityMismatch);
    }
    Uuid::parse_str(marker_uuid).map_err(|_| BaselineError::BaselineIdentityMismatch)
}

fn require_compatible_baseline(
    case: &DatabaseIdentity,
    baseline: &DatabaseIdentity,
) -> Result<(), BaselineError> {
    if case.database_name().kind() != crate::postgres::safety::DatabaseKind::Case
        || baseline.database_name().kind() != crate::postgres::safety::DatabaseKind::Baseline
        || baseline.marker().kind() != MarkerKind::Baseline
        || case.server_fingerprint() != baseline.server_fingerprint()
        || case.endpoint() != baseline.endpoint()
        || case.owner_oid() != baseline.owner_oid()
        || case.marker().compose_project() != baseline.marker().compose_project()
        || case.expected_application_role() != baseline.expected_application_role()
    {
        return Err(BaselineError::BaselineIdentityMismatch);
    }
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "keep each reset phase and its exact recovery boundary visible in one sequence"
)]
async fn reset_verified_case(
    inputs: &BaselineInputs,
    verified: DatabaseTarget<Verified>,
    _permit: MutationPermit,
    expected_baseline: &DatabaseIdentity,
) -> Result<(DatabaseIdentity, ConfiguredCaseResetReport), BaselineError> {
    let before = verified.identity();
    let before_database_oid = before.database_oid();
    let before_marker_uuid = before.marker().marker_uuid();
    let after_marker_uuid = Uuid::new_v4();
    let case = inputs.case_name.as_str();
    let baseline = expected_baseline.database_name().as_str();
    let recovery_name = temporary_case_name("recovery")?;

    if let Err(failure) =
        prepare_case_replacement(inputs, before, &recovery_name, case, baseline).await
    {
        return fail_case_reset(inputs, before, &recovery_name, failure).await;
    }

    let marker_update = replace_case_marker(inputs, after_marker_uuid).await;
    if let Err(error) = marker_update {
        return fail_case_reset(
            inputs,
            before,
            &recovery_name,
            CaseResetFailure::new(CaseResetPhase::MarkerUpdateStarted, error),
        )
        .await;
    }

    let after = match observe_case_identity(inputs).await {
        Ok(after) => after,
        Err(error) => {
            return fail_case_reset(
                inputs,
                before,
                &recovery_name,
                CaseResetFailure::new(CaseResetPhase::CaseAttestationStarted, error),
            )
            .await;
        }
    };
    if after.database_oid() == before_database_oid
        || after.owner_oid() != before.owner_oid()
        || after.server_fingerprint() != before.server_fingerprint()
        || after.endpoint() != before.endpoint()
        || after.database_name() != before.database_name()
        || after.marker().marker_uuid() != after_marker_uuid
        || after.marker().compose_project() != before.marker().compose_project()
        || after.expected_application_role() != before.expected_application_role()
    {
        return fail_case_reset(
            inputs,
            before,
            &recovery_name,
            CaseResetFailure::new(
                CaseResetPhase::CaseAttestationStarted,
                BaselineError::ResetProofFailed,
            ),
        )
        .await;
    }
    let baseline_after = match observe_sealed_baseline_identity(inputs).await {
        Ok(baseline) => baseline,
        Err(error) => {
            return fail_case_reset(
                inputs,
                before,
                &recovery_name,
                CaseResetFailure::new(CaseResetPhase::BaselineAttestationStarted, error),
            )
            .await;
        }
    };
    if &baseline_after != expected_baseline {
        return fail_case_reset(
            inputs,
            before,
            &recovery_name,
            CaseResetFailure::new(
                CaseResetPhase::BaselineAttestationStarted,
                BaselineError::BaselineIdentityMismatch,
            ),
        )
        .await;
    }
    if drop_database_exact(
        inputs,
        &recovery_name,
        CatalogDatabaseIdentity::from(before),
    )
    .await
    .is_err()
    {
        return fail_case_reset(
            inputs,
            before,
            &recovery_name,
            CaseResetFailure::new(
                CaseResetPhase::CommitStarted,
                BaselineError::CaseBackupCleanupFailed,
            ),
        )
        .await;
    }
    let report = ConfiguredCaseResetReport {
        before_database_oid,
        after_database_oid: after.database_oid(),
        before_marker_uuid: before_marker_uuid.to_string(),
        after_marker_uuid: after_marker_uuid.to_string(),
    };
    Ok((after, report))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CaseResetPhase {
    PreMutation,
    FenceStarted,
    RenameStarted,
    CloneStarted,
    MarkerUpdateStarted,
    CaseAttestationStarted,
    BaselineAttestationStarted,
    CommitStarted,
}

struct CaseResetFailure {
    phase: CaseResetPhase,
    error: BaselineError,
}

impl CaseResetFailure {
    const fn new(phase: CaseResetPhase, error: BaselineError) -> Self {
        Self { phase, error }
    }
}

async fn prepare_case_replacement(
    inputs: &BaselineInputs,
    before: &DatabaseIdentity,
    recovery_name: &DatabaseName,
    case: &str,
    baseline: &str,
) -> Result<(), CaseResetFailure> {
    let maintenance = PostgresSession::connect(&inputs.admin_url)
        .await
        .map_err(|error| CaseResetFailure::new(CaseResetPhase::PreMutation, error))?;
    let execution = async {
        require_database_absent(&maintenance.client, recovery_name)
            .await
            .map_err(|error| CaseResetFailure::new(CaseResetPhase::PreMutation, error))?;
        maintenance
            .client
            .batch_execute(&format!("ALTER DATABASE {case} ALLOW_CONNECTIONS false"))
            .await
            .map_err(|error| CaseResetFailure::new(CaseResetPhase::FenceStarted, error.into()))?;
        terminate_database_connections(&maintenance.client, before.database_oid())
            .await
            .map_err(|error| CaseResetFailure::new(CaseResetPhase::FenceStarted, error))?;
        require_database_identity(
            &maintenance.client,
            &inputs.case_name,
            CatalogDatabaseIdentity::from(before),
        )
        .await
        .map_err(|error| CaseResetFailure::new(CaseResetPhase::FenceStarted, error))?;
        maintenance
            .client
            .batch_execute(&format!(
                "ALTER DATABASE {case} RENAME TO {}",
                recovery_name.as_str()
            ))
            .await
            .map_err(|error| CaseResetFailure::new(CaseResetPhase::RenameStarted, error.into()))?;
        maintenance
            .client
            .batch_execute(&format!(
                "CREATE DATABASE {case} WITH OWNER = {} TEMPLATE = {baseline}",
                inputs.admin_role,
            ))
            .await
            .map_err(|error| CaseResetFailure::new(CaseResetPhase::CloneStarted, error.into()))?;
        configure_case_connect(&maintenance.client, inputs)
            .await
            .map_err(|error| CaseResetFailure::new(CaseResetPhase::CloneStarted, error))
    }
    .await;
    let close = maintenance.close().await;
    match (execution, close) {
        (Err(failure), _) => Err(failure),
        (Ok(()), Err(error)) => Err(CaseResetFailure::new(CaseResetPhase::CloneStarted, error)),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn replace_case_marker(
    inputs: &BaselineInputs,
    marker_uuid: Uuid,
) -> Result<(), BaselineError> {
    let reset = PostgresSession::connect(&inputs.database_url(&inputs.case_name)).await?;
    let update = reset
        .client
        .execute(
            "UPDATE tiv_verifier_marker SET marker_uuid = $1, marker_kind = 'case'",
            &[&marker_uuid],
        )
        .await;
    let close = reset.close().await;
    let updated = update?;
    close?;
    if updated != 1 {
        return Err(BaselineError::UnexpectedMarker);
    }
    Ok(())
}

async fn configure_case_connect(
    client: &tokio_postgres::Client,
    inputs: &BaselineInputs,
) -> Result<(), BaselineError> {
    let case = inputs.case_name.as_str();
    client
        .batch_execute(&format!(
            "REVOKE CONNECT ON DATABASE {case} FROM PUBLIC; \
             REVOKE CONNECT ON DATABASE {case} FROM {}; \
             REVOKE CONNECT ON DATABASE {case} FROM {}; \
             GRANT CONNECT ON DATABASE {case} TO {}; \
             GRANT CONNECT ON DATABASE {case} TO {}",
            inputs.application_role,
            inputs.invariant_role,
            inputs.application_role,
            inputs.invariant_role,
        ))
        .await?;
    Ok(())
}

async fn fail_case_reset<T>(
    inputs: &BaselineInputs,
    expected_original: &DatabaseIdentity,
    recovery_name: &DatabaseName,
    failure: CaseResetFailure,
) -> Result<T, BaselineError> {
    if failure.phase == CaseResetPhase::PreMutation {
        return Err(failure.error);
    }
    match restore_original_case(inputs, expected_original, recovery_name).await {
        Ok(()) => Err(failure.error),
        Err(_) => Err(BaselineError::CaseDatabaseRecoveryFailed),
    }
}

async fn restore_original_case(
    inputs: &BaselineInputs,
    expected: &DatabaseIdentity,
    recovery_name: &DatabaseName,
) -> Result<(), BaselineError> {
    let maintenance = PostgresSession::connect(&inputs.admin_url).await?;
    let case_identity =
        database_catalog_identity_if_present(&maintenance.client, &inputs.case_name).await?;
    let recovery_identity =
        database_catalog_identity_if_present(&maintenance.client, recovery_name).await?;
    let expected_catalog = CatalogDatabaseIdentity::from(expected);
    match (case_identity, recovery_identity) {
        (Some(case_identity), None) if case_identity == expected_catalog => {
            maintenance
                .client
                .batch_execute(&format!(
                    "ALTER DATABASE {} ALLOW_CONNECTIONS true",
                    inputs.case_name.as_str()
                ))
                .await?;
        }
        (replacement_identity, Some(recovery_identity))
            if recovery_identity == expected_catalog =>
        {
            if let Some(replacement_identity) = replacement_identity {
                if replacement_identity == expected_catalog
                    || replacement_identity.owner_oid != expected.owner_oid()
                {
                    return Err(BaselineError::CaseDatabaseRecoveryFailed);
                }
                terminate_database_connections(&maintenance.client, replacement_identity.oid)
                    .await?;
                maintenance
                    .client
                    .batch_execute(&format!("DROP DATABASE {}", inputs.case_name.as_str()))
                    .await?;
            }
            maintenance
                .client
                .batch_execute(&format!(
                    "ALTER DATABASE {} ALLOW_CONNECTIONS true",
                    recovery_name.as_str()
                ))
                .await?;
            maintenance
                .client
                .batch_execute(&format!(
                    "ALTER DATABASE {} RENAME TO {}",
                    recovery_name.as_str(),
                    inputs.case_name.as_str()
                ))
                .await?;
        }
        _ => return Err(BaselineError::CaseDatabaseRecoveryFailed),
    }
    maintenance.close().await?;
    let restored = observe_case_identity(inputs).await?;
    if &restored != expected {
        return Err(BaselineError::CaseDatabaseRecoveryFailed);
    }
    Ok(())
}

fn temporary_case_name(purpose: &str) -> Result<DatabaseName, BaselineError> {
    DatabaseName::parse(format!("tiv_case_{purpose}_{}", Uuid::new_v4().simple()))
        .map_err(BaselineError::DatabaseName)
}

async fn create_database_from_template(
    inputs: &BaselineInputs,
    database_name: &DatabaseName,
    template_name: &DatabaseName,
    expected_owner_oid: u32,
) -> Result<CatalogDatabaseIdentity, BaselineError> {
    let maintenance = PostgresSession::connect(&inputs.admin_url).await?;
    let creation = async {
        require_database_absent(&maintenance.client, database_name).await?;
        maintenance
            .client
            .batch_execute(&format!(
                "CREATE DATABASE {} WITH OWNER = {} TEMPLATE = {}",
                database_name.as_str(),
                inputs.admin_role,
                template_name.as_str()
            ))
            .await?;
        let identity = database_catalog_identity_if_present(&maintenance.client, database_name)
            .await?
            .ok_or(BaselineError::UnexpectedDatabaseIdentity)?;
        if identity.owner_oid != expected_owner_oid {
            return Err(BaselineError::UnexpectedDatabaseIdentity);
        }
        Ok(identity)
    }
    .await;
    let close = maintenance.close().await;
    match (creation, close) {
        (Ok(identity), Ok(())) => Ok(identity),
        (Err(error), _) | (Ok(_), Err(error)) => {
            match cleanup_temporary_database_if_present(inputs, database_name, expected_owner_oid)
                .await
            {
                Ok(()) => Err(error),
                Err(_) => Err(BaselineError::TemporaryDatabaseCleanupFailed),
            }
        }
    }
}

async fn cleanup_temporary_database_if_present(
    inputs: &BaselineInputs,
    database_name: &DatabaseName,
    expected_owner_oid: u32,
) -> Result<(), BaselineError> {
    let maintenance = PostgresSession::connect(&inputs.admin_url).await?;
    let identity = database_catalog_identity_if_present(&maintenance.client, database_name).await?;
    let Some(identity) = identity else {
        return maintenance.close().await;
    };
    if identity.owner_oid != expected_owner_oid {
        return Err(BaselineError::UnexpectedDatabaseIdentity);
    }
    terminate_database_connections(&maintenance.client, identity.oid).await?;
    maintenance
        .client
        .batch_execute(&format!("DROP DATABASE {}", database_name.as_str()))
        .await?;
    maintenance.close().await
}

async fn drop_database_exact(
    inputs: &BaselineInputs,
    database_name: &DatabaseName,
    expected: CatalogDatabaseIdentity,
) -> Result<(), BaselineError> {
    let maintenance = PostgresSession::connect(&inputs.admin_url).await?;
    require_database_identity(&maintenance.client, database_name, expected).await?;
    terminate_database_connections(&maintenance.client, expected.oid).await?;
    maintenance
        .client
        .batch_execute(&format!("DROP DATABASE {}", database_name.as_str()))
        .await?;
    maintenance.close().await
}

async fn require_database_absent(
    client: &tokio_postgres::Client,
    database_name: &DatabaseName,
) -> Result<(), BaselineError> {
    if database_catalog_identity_if_present(client, database_name)
        .await?
        .is_some()
    {
        return Err(BaselineError::UnexpectedDatabaseIdentity);
    }
    Ok(())
}

async fn require_database_identity(
    client: &tokio_postgres::Client,
    database_name: &DatabaseName,
    expected: CatalogDatabaseIdentity,
) -> Result<(), BaselineError> {
    if database_catalog_identity_if_present(client, database_name).await? != Some(expected) {
        return Err(BaselineError::UnexpectedDatabaseIdentity);
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct CatalogDatabaseIdentity {
    oid: u32,
    owner_oid: u32,
}

impl From<&DatabaseIdentity> for CatalogDatabaseIdentity {
    fn from(identity: &DatabaseIdentity) -> Self {
        Self {
            oid: identity.database_oid(),
            owner_oid: identity.owner_oid(),
        }
    }
}

async fn database_catalog_identity_if_present(
    client: &tokio_postgres::Client,
    database_name: &DatabaseName,
) -> Result<Option<CatalogDatabaseIdentity>, BaselineError> {
    client
        .query_opt(
            "SELECT oid::bigint, datdba::bigint FROM pg_database WHERE datname = $1",
            &[&database_name.as_str()],
        )
        .await?
        .map(|row| {
            Ok(CatalogDatabaseIdentity {
                oid: u32::try_from(row.get::<_, i64>(0))
                    .map_err(|_| BaselineError::UnexpectedDatabaseIdentity)?,
                owner_oid: u32::try_from(row.get::<_, i64>(1))
                    .map_err(|_| BaselineError::UnexpectedDatabaseIdentity)?,
            })
        })
        .transpose()
}

async fn terminate_database_connections(
    client: &tokio_postgres::Client,
    database_oid: u32,
) -> Result<(), BaselineError> {
    client
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datid::bigint = $1 AND pid <> pg_backend_pid()",
            &[&i64::from(database_oid)],
        )
        .await?;
    Ok(())
}

async fn stop_customer_services(inputs: &BaselineInputs) -> Result<(), BaselineError> {
    let services = std::iter::once(inputs.application_service.clone())
        .chain(inputs.worker_services.iter().cloned())
        .collect::<Vec<_>>();
    let mut tail = vec!["stop".to_owned(), "--timeout".to_owned(), "5".to_owned()];
    tail.extend(services);
    docker_compose(inputs, &tail).await
}

async fn start_customer_services(inputs: &BaselineInputs) -> Result<(), BaselineError> {
    let services = std::iter::once(inputs.application_service.clone())
        .chain(inputs.worker_services.iter().cloned())
        .collect::<Vec<_>>();
    let mut tail = vec![
        "up".to_owned(),
        "--detach".to_owned(),
        "--wait".to_owned(),
        "--wait-timeout".to_owned(),
        "30".to_owned(),
    ];
    tail.extend(services);
    docker_compose(inputs, &tail).await
}

async fn docker_compose(inputs: &BaselineInputs, tail: &[String]) -> Result<(), BaselineError> {
    let root = inputs.root.to_str().ok_or(BaselineError::NonUtf8Path)?;
    let mut args = vec![
        "compose".to_owned(),
        "--project-name".to_owned(),
        inputs.compose_project.clone(),
        "--project-directory".to_owned(),
        root.to_owned(),
    ];
    for file in &inputs.compose_files {
        args.push("--file".to_owned());
        args.push(file.to_str().ok_or(BaselineError::NonUtf8Path)?.to_owned());
    }
    args.extend_from_slice(tail);
    let output = run_docker(&args).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(BaselineError::DockerCommandFailed)
    }
}

async fn docker_output(args: &[&str]) -> Result<String, BaselineError> {
    let args = args
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    let output = run_docker(&args).await?;
    if !output.status.success() {
        return Err(BaselineError::DockerCommandFailed);
    }
    String::from_utf8(output.stdout).map_err(|_| BaselineError::DockerOutput)
}

async fn run_docker(args: &[String]) -> Result<BoundedOutput, BaselineError> {
    let mut command = Command::new("docker");
    command
        .args(["--host", LOCAL_DOCKER_HOST])
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .args(args)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| BaselineError::DockerUnavailable)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(BaselineError::DockerUnavailable)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(BaselineError::DockerUnavailable)?;
    let stdout_reader = tokio::spawn(read_bounded(stdout));
    let stderr_reader = tokio::spawn(read_bounded(stderr));
    let status = match timeout(COMMAND_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            drop(child);
            finish_readers(stdout_reader, stderr_reader).await?;
            return Err(BaselineError::DockerWait);
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = timeout(CLEANUP_TIMEOUT, child.wait()).await;
            drop(child);
            finish_readers(stdout_reader, stderr_reader).await?;
            return Err(BaselineError::DockerTimeout);
        }
    };
    let (stdout, _stderr) = finish_readers(stdout_reader, stderr_reader).await?;
    Ok(BoundedOutput { status, stdout })
}

async fn read_bounded(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>, BaselineError> {
    let mut output = Vec::new();
    reader
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)
        .await
        .map_err(|_| BaselineError::DockerOutput)?;
    if output.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(BaselineError::DockerOutputTooLarge);
    }
    Ok(output)
}

async fn finish_readers(
    mut stdout: JoinHandle<Result<Vec<u8>, BaselineError>>,
    mut stderr: JoinHandle<Result<Vec<u8>, BaselineError>>,
) -> Result<(Vec<u8>, Vec<u8>), BaselineError> {
    let result = timeout(CLEANUP_TIMEOUT, async {
        let stdout = (&mut stdout)
            .await
            .map_err(|_| BaselineError::DockerOutput)??;
        let stderr = (&mut stderr)
            .await
            .map_err(|_| BaselineError::DockerOutput)??;
        Ok((stdout, stderr))
    })
    .await;
    if let Ok(result) = result {
        result
    } else {
        stdout.abort();
        stderr.abort();
        let _ = stdout.await;
        let _ = stderr.await;
        Err(BaselineError::DockerOutput)
    }
}

struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

struct PostgresSession {
    client: tokio_postgres::Client,
    connection: JoinHandle<Result<(), tokio_postgres::Error>>,
}

impl PostgresSession {
    async fn connect(url: &Url) -> Result<Self, BaselineError> {
        let (client, connection) = tokio_postgres::connect(url.as_str(), NoTls).await?;
        Ok(Self {
            client,
            connection: tokio::spawn(connection),
        })
    }

    async fn close(self) -> Result<(), BaselineError> {
        drop(self.client);
        self.connection
            .await
            .map_err(|_| BaselineError::DatabaseConnectionTask)??;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum BaselineError {
    #[error("project configuration is invalid: {0}")]
    Config(#[from] ConfigError),
    #[error("configured database names are outside the safe reset grammar")]
    DatabaseName(InvalidDatabaseName),
    #[error("the configured database connection is not one exact loopback target")]
    UnsafeDatabaseConnection,
    #[error("the local Docker CLI is unavailable")]
    DockerUnavailable,
    #[error("the local Docker command timed out")]
    DockerTimeout,
    #[error("the local Docker command could not be awaited")]
    DockerWait,
    #[error("the local Docker command failed")]
    DockerCommandFailed,
    #[error("the local Docker command returned invalid output")]
    DockerOutput,
    #[error("the local Docker command exceeded its output bound")]
    DockerOutputTooLarge,
    #[error("customer services could not be restored after a baseline lifecycle failure")]
    CustomerServicesRecoveryFailed,
    #[error("the exact original case database could not be restored after replacement failed")]
    CaseDatabaseRecoveryFailed,
    #[error(
        "the verified replacement is active but its private recovery database could not be removed"
    )]
    CaseBackupCleanupFailed,
    #[error("a private baseline-attestation database could not be removed")]
    TemporaryDatabaseCleanupFailed,
    #[error("the configured PostgreSQL container does not match the local Compose boundary")]
    PostgresContainerMismatch,
    #[error("the PostgreSQL container changed across the attested reset boundary")]
    PostgresContainerChanged,
    #[error("the configured case database does not exist")]
    CaseDatabaseMissing,
    #[error("the configured baseline database already exists")]
    BaselineAlreadyExists,
    #[error("the configured database exceeds the safety size cap")]
    DatabaseTooLarge,
    #[error("the database identity did not match its configured boundary")]
    UnexpectedDatabaseIdentity,
    #[error("the case database is not owned by the distinct configured administration role")]
    UnsafeDatabaseOwner,
    #[error("the database marker did not match its configured boundary")]
    UnexpectedMarker,
    #[error("the configured application role can tamper with the reset identity boundary")]
    UnsafeApplicationRole,
    #[error("the private reset-proof relation already exists")]
    ResetProbeAlreadyExists,
    #[error("reset acknowledgement failed: {0:?}")]
    ResetAcknowledgement(ResetAcknowledgementError),
    #[error("the freshly observed case identity no longer matches the reset target: {0:?}")]
    Safety(SafetyError),
    #[error("the observed database identity is invalid")]
    InvalidIdentity(InvalidDatabaseIdentity),
    #[error("PostgreSQL baseline execution failed: {0}")]
    Database(#[from] tokio_postgres::Error),
    #[error("the PostgreSQL connection task failed")]
    DatabaseConnectionTask,
    #[error("the configured sealed baseline is not ready")]
    BaselineNotReady,
    #[error("the sealed baseline identity no longer matches the configured case boundary")]
    BaselineIdentityMismatch,
    #[error("the destructive reset proof did not restore the sealed baseline")]
    ResetProofFailed,
    #[error("a configured path is not valid UTF-8")]
    NonUtf8Path,
}

impl BaselineError {
    #[must_use]
    pub const fn is_infrastructure_failure(&self) -> bool {
        matches!(
            self,
            Self::DockerUnavailable
                | Self::DockerTimeout
                | Self::DockerWait
                | Self::DockerCommandFailed
                | Self::DockerOutput
                | Self::DockerOutputTooLarge
                | Self::CustomerServicesRecoveryFailed
                | Self::CaseDatabaseRecoveryFailed
                | Self::CaseBackupCleanupFailed
                | Self::TemporaryDatabaseCleanupFailed
                | Self::Database(_)
                | Self::DatabaseConnectionTask
        )
    }
}

impl From<InvalidDatabaseName> for BaselineError {
    fn from(error: InvalidDatabaseName) -> Self {
        Self::DatabaseName(error)
    }
}

impl From<ResetAcknowledgementError> for BaselineError {
    fn from(error: ResetAcknowledgementError) -> Self {
        Self::ResetAcknowledgement(error)
    }
}
