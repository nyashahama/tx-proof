//! Self-contained evidence execution for the isolated reference application.

use std::{
    collections::BTreeMap,
    path::Path,
    process::{ExitStatus, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiv_core::{
    plan::PlannedCase,
    result::{AttemptResult, FailureIdentity},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

use crate::{
    evidence::{EvidenceError, TruthSpikeEvidence},
    postgres::{
        oracle::{InvariantVerdict, ProviderPaymentIntent, QuiescencePermit, SnapshotReport},
        probe::ConfiguredSqlProbe,
        safety::{ComposeProjectId, DatabaseName, DatabaseTarget, Unverified},
        spike::{ReferenceSqlProbe, SpikePostgresConfig, SpikePostgresError, TruthSpikePostgres},
    },
    reference_case::{
        CaseSqlProbe, ReferenceCaseRunConfig, ReferenceCaseRunConfigError, ReferenceCaseRunError,
        ReferenceProcessControl, ReferenceProcessControlError, ReferenceProcessControlFuture,
        preflight_reference_planned_case, run_reference_planned_case_with_process,
    },
    replay::{
        ReferenceAppReplayConfig, ReferenceAppReplayConfigError, ReferenceAppReplayError,
        ReferenceAppReplayReceipt, ReplayPlan, run_reference_app_replay,
    },
};

const PROVIDER_UNIQUENESS_ID: &str = "provider-object-unique";
const REFERENCE_APP_COMPOSE_PROJECT: &str = "tiv-reference-app-spike";
const REFERENCE_POSTGRES_PURPOSE: &str = "disposable-reference-app-postgres";
const REFERENCE_POSTGRES_IMAGE: &str = "postgres:18.4-bookworm";
const REFERENCE_POSTGRES_CLUSTER_SETTING: &str = "cluster_name=tiv-reference-app-postgres";
const REFERENCE_APP_PURPOSE: &str = "disposable-known-bug-reference-application";
const REFERENCE_APP_IMAGE: &str = "tiv-reference-app-spike-reference-app";
const FIXTURE_PURPOSE: &str = "disposable-stripe-payment-intent-fixture";
const FIXTURE_IMAGE: &str = "tiv-reference-app-spike-stripe-fixture";
const LOCAL_DOCKER_HOST: &str = "unix:///var/run/docker.sock";
const DOCKER_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const DOCKER_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_DOCKER_OUTPUT_BYTES: u64 = 64 * 1024;
const MAX_FIXTURE_STATE_BYTES: usize = 16 * 1024;

/// Attested inputs for one provision-reset-replay evidence run.
pub struct ReferenceAppEvidenceConfig {
    postgres_port: u16,
    postgres_admin_role: String,
    postgres_admin_password: String,
    postgres_application_password: String,
    compose_project: ComposeProjectId,
    reference_app_url: String,
    fixture_control_url: String,
    fixture_control_token: String,
    stack_attestation: ReferenceStackAttestation,
}

impl ReferenceAppEvidenceConfig {
    /// Validates local inputs and attests all three running reference-stack
    /// containers before any database credential is used.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceAppEvidenceConfigError`] unless the HTTP targets,
    /// fixture-control contract, and isolated Docker boundary match the
    /// committed reference stack.
    pub async fn attest(
        postgres_port: u16,
        postgres_admin_role: impl Into<String>,
        postgres_admin_password: impl Into<String>,
        postgres_application_password: impl Into<String>,
        reference_app_url: impl Into<String>,
        fixture_control_url: impl Into<String>,
        fixture_control_token: impl Into<String>,
    ) -> Result<Self, ReferenceAppEvidenceConfigError> {
        let postgres_admin_role = postgres_admin_role.into();
        let postgres_admin_password = postgres_admin_password.into();
        let postgres_application_password = postgres_application_password.into();
        let reference_app_url = reference_app_url.into();
        let fixture_control_url = fixture_control_url.into();
        let fixture_control_token = fixture_control_token.into();
        if postgres_port == 0
            || !valid_role_name(&postgres_admin_role)
            || postgres_admin_password.trim().is_empty()
            || postgres_application_password.trim().is_empty()
        {
            return Err(ReferenceAppEvidenceConfigError::InvalidPostgresConfiguration);
        }
        let compose_project = ComposeProjectId::new(REFERENCE_APP_COMPOSE_PROJECT)
            .map_err(|_| ReferenceAppEvidenceConfigError::InvalidPostgresConfiguration)?;
        let validation_case = DatabaseName::parse("tiv_case_00000000")
            .map_err(|_| ReferenceAppEvidenceConfigError::InvalidPostgresConfiguration)?;
        let validated = ReferenceAppReplayConfig::new(
            validation_case,
            reference_app_url.clone(),
            fixture_control_url.clone(),
            fixture_control_token.clone(),
            1,
            2,
            1,
        )?;
        let stack_attestation = attest_reference_stack(
            postgres_port,
            validated.reference_app_url(),
            validated.fixture_control_url(),
        )
        .await?;
        Ok(Self {
            postgres_port,
            postgres_admin_role,
            postgres_admin_password,
            postgres_application_password,
            compose_project,
            reference_app_url,
            fixture_control_url,
            fixture_control_token,
            stack_attestation,
        })
    }

    fn replay_config(
        &self,
        case_database: DatabaseName,
        reset_sequence: u64,
        confirm_sequence: u64,
        webhook_timestamp: i64,
    ) -> Result<ReferenceAppReplayConfig, ReferenceAppEvidenceConfigError> {
        ReferenceAppReplayConfig::new(
            case_database,
            self.reference_app_url.clone(),
            self.fixture_control_url.clone(),
            self.fixture_control_token.clone(),
            reset_sequence,
            confirm_sequence,
            webhook_timestamp,
        )
        .map_err(ReferenceAppEvidenceConfigError::Replay)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReferenceStackAttestation {
    postgres: String,
    reference_app: String,
    fixture: String,
}

#[derive(Clone, Copy)]
struct ReferenceService {
    compose_service: &'static str,
    purpose: &'static str,
    image: &'static str,
    command: &'static [&'static str],
    container_port: u16,
    host_port: u16,
}

const fn postgres_service(host_port: u16) -> ReferenceService {
    ReferenceService {
        compose_service: "postgres",
        purpose: REFERENCE_POSTGRES_PURPOSE,
        image: REFERENCE_POSTGRES_IMAGE,
        command: &["postgres", "-c", REFERENCE_POSTGRES_CLUSTER_SETTING],
        container_port: 5_432,
        host_port,
    }
}

const fn reference_app_service(host_port: u16) -> ReferenceService {
    ReferenceService {
        compose_service: "reference-app",
        purpose: REFERENCE_APP_PURPOSE,
        image: REFERENCE_APP_IMAGE,
        command: &["tiv-reference-app"],
        container_port: 18_080,
        host_port,
    }
}

const fn fixture_service(host_port: u16) -> ReferenceService {
    ReferenceService {
        compose_service: "stripe-fixture",
        purpose: FIXTURE_PURPOSE,
        image: FIXTURE_IMAGE,
        command: &["tiv-stripe-pi-fixture"],
        container_port: 12_112,
        host_port,
    }
}

async fn attest_reference_stack(
    postgres_port: u16,
    reference_app_url: &str,
    fixture_control_url: &str,
) -> Result<ReferenceStackAttestation, ReferenceAppEvidenceConfigError> {
    let reference_app_port = exact_ipv4_loopback_port(reference_app_url)?;
    let fixture_control_port = exact_ipv4_loopback_port(fixture_control_url)?;
    let postgres_container_id = attest_reference_service(&postgres_service(postgres_port)).await?;
    let reference_app_container_id =
        attest_reference_service(&reference_app_service(reference_app_port)).await?;
    attest_reference_app_evidence_modes(&reference_app_container_id).await?;
    let fixture_container_id =
        attest_reference_service(&fixture_service(fixture_control_port)).await?;
    if postgres_container_id == reference_app_container_id
        || postgres_container_id == fixture_container_id
        || reference_app_container_id == fixture_container_id
    {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch);
    }
    Ok(ReferenceStackAttestation {
        postgres: postgres_container_id,
        reference_app: reference_app_container_id,
        fixture: fixture_container_id,
    })
}

async fn attest_reference_app_evidence_modes(
    container_id: &str,
) -> Result<(), ReferenceAppEvidenceConfigError> {
    let inspection = docker_output(&[
        "inspect",
        "--format",
        concat!(
            "{{range .Config.Env}}",
            "{{if eq . \"TIV_REFERENCE_APP_RETRY_KEY_MODE=faulty_changed_key\"}}",
            "retry ",
            "{{end}}",
            "{{if eq . \"TIV_REFERENCE_APP_RETRY_KEY_MODE=repaired_same_key\"}}",
            "retry_conflict ",
            "{{end}}",
            "{{if eq . \"TIV_REFERENCE_APP_CALLER_RETRY_MODE=faulty_per_request\"}}",
            "caller ",
            "{{end}}",
            "{{if eq . \"TIV_REFERENCE_APP_CALLER_RETRY_MODE=repaired_recover_operation\"}}",
            "caller_conflict ",
            "{{end}}",
            "{{if eq . \"TIV_REFERENCE_APP_RECONCILIATION_MODE=faulty_webhook_only\"}}",
            "reconciliation ",
            "{{end}}",
            "{{if eq . \"TIV_REFERENCE_APP_RECONCILIATION_MODE=repaired_provider_reconcile\"}}",
            "reconciliation_conflict ",
            "{{end}}",
            "{{if eq . \"TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE=repaired_deduplicate\"}}",
            "webhook ",
            "{{end}}",
            "{{if eq . \"TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE=faulty_duplicate_effect\"}}",
            "webhook_conflict ",
            "{{end}}",
            "{{if eq . \"TIV_REFERENCE_APP_LEDGER_MODE=repaired_balanced_once\"}}",
            "ledger ",
            "{{end}}",
            "{{if eq . \"TIV_REFERENCE_APP_LEDGER_MODE=faulty_one_sided_duplicate\"}}",
            "ledger_conflict ",
            "{{end}}",
            "{{end}}"
        ),
        container_id,
    ])
    .await?;
    validate_reference_app_evidence_mode_inspection(&inspection)
}

fn validate_reference_app_evidence_mode_inspection(
    inspection: &str,
) -> Result<(), ReferenceAppEvidenceConfigError> {
    let mut markers = inspection.split_whitespace().collect::<Vec<_>>();
    markers.sort_unstable();
    if markers == ["caller", "ledger", "reconciliation", "retry", "webhook"] {
        Ok(())
    } else {
        Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch)
    }
}

fn exact_ipv4_loopback_port(url: &str) -> Result<u16, ReferenceAppEvidenceConfigError> {
    let url = reqwest::Url::parse(url)
        .map_err(|_| ReferenceAppEvidenceConfigError::ReferenceStackMismatch)?;
    if url.host_str() != Some("127.0.0.1") {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch);
    }
    url.port()
        .ok_or(ReferenceAppEvidenceConfigError::ReferenceStackMismatch)
}

async fn attest_reference_service(
    service: &ReferenceService,
) -> Result<String, ReferenceAppEvidenceConfigError> {
    let project_filter =
        format!("label=com.docker.compose.project={REFERENCE_APP_COMPOSE_PROJECT}");
    let service_filter = format!(
        "label=com.docker.compose.service={}",
        service.compose_service
    );
    let container_ids = docker_output(&[
        "ps",
        "--filter",
        &project_filter,
        "--filter",
        &service_filter,
        "--format",
        "{{.ID}}",
    ])
    .await?;
    let ids = container_ids.lines().collect::<Vec<_>>();
    let [container_id] = ids.as_slice() else {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch);
    };
    if !(12..=64).contains(&container_id.len())
        || !container_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch);
    }
    let inspection = docker_output(&[
        "inspect",
        "--format",
        concat!(
            "{{.State.Running}}\n",
            "{{if .State.Health}}{{.State.Health.Status}}{{end}}\n",
            "{{index .Config.Labels \"io.txproof.purpose\"}}\n",
            "{{json .NetworkSettings.Ports}}\n",
            "{{json .Config.Cmd}}\n",
            "{{.Config.Image}}"
        ),
        container_id,
    ])
    .await?;
    validate_reference_service_inspection(&inspection, service)?;
    Ok((*container_id).to_owned())
}

async fn docker_output(args: &[&str]) -> Result<String, ReferenceAppEvidenceConfigError> {
    let mut command = Command::new("docker");
    command
        .args(["--host", LOCAL_DOCKER_HOST])
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .args(args);
    let output = bounded_output(command, DOCKER_COMMAND_TIMEOUT)
        .await
        .map_err(|_| ReferenceAppEvidenceConfigError::ReferenceStackInspectionUnavailable)?;
    if !output.status.success() {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackInspectionUnavailable);
    }
    String::from_utf8(output.stdout)
        .map_err(|_| ReferenceAppEvidenceConfigError::ReferenceStackMismatch)
}

struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundedProcessError {
    Start,
    TimedOut,
    Wait,
    Output,
    OutputTooLarge,
}

async fn bounded_output(
    mut command: Command,
    command_timeout: Duration,
) -> Result<BoundedOutput, BoundedProcessError> {
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| BoundedProcessError::Start)?;
    let stdout = child.stdout.take().ok_or(BoundedProcessError::Start)?;
    let stderr = child.stderr.take().ok_or(BoundedProcessError::Start)?;
    let stdout_reader = tokio::spawn(read_bounded_output(stdout));
    let stderr_reader = tokio::spawn(read_bounded_output(stderr));

    let status = match timeout(command_timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            drop(child);
            finish_output_readers(stdout_reader, stderr_reader).await?;
            return Err(BoundedProcessError::Wait);
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = timeout(DOCKER_CLEANUP_TIMEOUT, child.wait()).await;
            drop(child);
            finish_output_readers(stdout_reader, stderr_reader).await?;
            return Err(BoundedProcessError::TimedOut);
        }
    };
    let (stdout, _stderr) = finish_output_readers(stdout_reader, stderr_reader).await?;
    Ok(BoundedOutput { status, stdout })
}

async fn read_bounded_output(
    reader: impl AsyncRead + Unpin,
) -> Result<Vec<u8>, BoundedProcessError> {
    let mut output = Vec::new();
    reader
        .take(MAX_DOCKER_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)
        .await
        .map_err(|_| BoundedProcessError::Output)?;
    if output.len() as u64 > MAX_DOCKER_OUTPUT_BYTES {
        return Err(BoundedProcessError::OutputTooLarge);
    }
    Ok(output)
}

async fn finish_output_readers(
    mut stdout_reader: JoinHandle<Result<Vec<u8>, BoundedProcessError>>,
    mut stderr_reader: JoinHandle<Result<Vec<u8>, BoundedProcessError>>,
) -> Result<(Vec<u8>, Vec<u8>), BoundedProcessError> {
    let read_result = timeout(DOCKER_CLEANUP_TIMEOUT, async {
        let stdout = (&mut stdout_reader)
            .await
            .map_err(|_| BoundedProcessError::Output)??;
        let stderr = (&mut stderr_reader)
            .await
            .map_err(|_| BoundedProcessError::Output)??;
        Ok((stdout, stderr))
    })
    .await;
    if let Ok(result) = read_result {
        result
    } else {
        stdout_reader.abort();
        stderr_reader.abort();
        let _ = stdout_reader.await;
        let _ = stderr_reader.await;
        Err(BoundedProcessError::Output)
    }
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

fn validate_reference_service_inspection(
    inspection: &str,
    service: &ReferenceService,
) -> Result<(), ReferenceAppEvidenceConfigError> {
    let mut lines = inspection.lines();
    let running = lines.next();
    let health = lines.next();
    let purpose = lines.next();
    let ports = lines.next();
    let command = lines.next();
    let image = lines.next();
    if running != Some("true")
        || health != Some("healthy")
        || purpose != Some(service.purpose)
        || image != Some(service.image)
        || lines.next().is_some()
    {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch);
    }
    let ports = serde_json::from_str::<BTreeMap<String, Option<Vec<PortBinding>>>>(
        ports.ok_or(ReferenceAppEvidenceConfigError::ReferenceStackMismatch)?,
    )
    .map_err(|_| ReferenceAppEvidenceConfigError::ReferenceStackMismatch)?;
    let container_port = format!("{}/tcp", service.container_port);
    let bindings = ports
        .get(&container_port)
        .and_then(Option::as_deref)
        .ok_or(ReferenceAppEvidenceConfigError::ReferenceStackMismatch)?;
    if ports.len() != 1
        || bindings.len() != 1
        || bindings[0].host_ip != "127.0.0.1"
        || bindings[0].host_port != service.host_port.to_string()
    {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch);
    }
    let command = serde_json::from_str::<Vec<String>>(
        command.ok_or(ReferenceAppEvidenceConfigError::ReferenceStackMismatch)?,
    )
    .map_err(|_| ReferenceAppEvidenceConfigError::ReferenceStackMismatch)?;
    if command != service.command {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortBinding {
    #[serde(rename = "HostIp")]
    host_ip: String,
    #[serde(rename = "HostPort")]
    host_port: String,
}

/// Provisions one reference database pair, runs three replay attempts from
/// fresh baseline clones, and returns allowlisted reproduction evidence.
///
/// The known reference application completes database work synchronously
/// before each HTTP response. Therefore completion of the replay driver's
/// checkout, webhook delivery, and final provider-state read is the runtime's
/// quiescence boundary for this synthetic application only.
///
/// # Errors
///
/// Returns [`ReferenceAppEvidenceError`] when `PostgreSQL` safety checks,
/// replay, oracle evaluation, either reset identity, or evidence coherence
/// fails. Operational failures abort without an evidence document; only
/// completed oracle outcomes contribute to reproduction classification.
pub async fn run_reference_app_evidence(
    plan: &ReplayPlan,
    config: &ReferenceAppEvidenceConfig,
) -> Result<TruthSpikeEvidence, ReferenceAppEvidenceError> {
    let observed_stack = attest_reference_stack(
        config.postgres_port,
        &config.reference_app_url,
        &config.fixture_control_url,
    )
    .await?;
    if observed_stack != config.stack_attestation {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch.into());
    }
    restart_reference_application(config).await?;
    let postgres =
        TruthSpikePostgres::connect(SpikePostgresConfig::loopback_reference_app_with_archive(
            config.postgres_port,
            config.postgres_admin_role.clone(),
            config.postgres_admin_password.clone(),
            config.postgres_application_password.clone(),
            config.stack_attestation.postgres.clone(),
        ))
        .await
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let post_connect_stack = attest_reference_stack(
        config.postgres_port,
        &config.reference_app_url,
        &config.fixture_control_url,
    )
    .await?;
    if post_connect_stack != config.stack_attestation {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch.into());
    }
    let initial_control_sequence = fixture_control_sequence(config).await?;
    let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
    let provisioned = postgres
        .provision_reference_databases(&suffix, config.compose_project.clone())
        .await
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let (baseline_target, case_name, first_case_target, baseline_archive) = provisioned
        .into_archive_parts()
        .ok_or(SpikePostgresError::ArchiveUnavailable)
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let first_database_oid = first_case_target.identity().database_oid();

    let (first_provider_object_count, first_report) = run_reference_app_attempt_at_offset(
        &postgres,
        plan,
        config,
        &case_name,
        initial_control_sequence,
        0,
    )
    .await?;
    let (expected_failure, first_attempt) = uniqueness_attempt(&first_report, &case_name)?;

    let first_reset_target = postgres
        .reset_case_from_template(first_case_target, &baseline_target, Uuid::new_v4())
        .await
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let second_database_oid = first_reset_target.identity().database_oid();

    let (second_provider_object_count, second_report) = run_reference_app_attempt_at_offset(
        &postgres,
        plan,
        config,
        &case_name,
        initial_control_sequence,
        2,
    )
    .await?;
    let (_, second_attempt) = uniqueness_attempt(&second_report, &case_name)?;

    let second_reset_target = postgres
        .reset_case_from_archive(
            first_reset_target,
            &baseline_target,
            &baseline_archive,
            Uuid::new_v4(),
        )
        .await
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let third_database_oid = second_reset_target.identity().database_oid();

    let (third_provider_object_count, third_report) = run_reference_app_attempt_at_offset(
        &postgres,
        plan,
        config,
        &case_name,
        initial_control_sequence,
        4,
    )
    .await?;
    let (_, third_attempt) = uniqueness_attempt(&third_report, &case_name)?;

    let attempts = [first_attempt, second_attempt, third_attempt];

    TruthSpikeEvidence::new(
        [
            first_provider_object_count,
            second_provider_object_count,
            third_provider_object_count,
        ],
        [first_database_oid, second_database_oid, third_database_oid],
        &expected_failure,
        &attempts,
    )
    .map_err(ReferenceAppEvidenceError::Evidence)
}

/// One attested, journaled planned-case execution and its final oracle result.
#[derive(Serialize)]
pub struct ReferencePlannedCaseEvidence {
    schema_version: u16,
    seed: u64,
    planned_action_count: usize,
    executed_action_count: usize,
    journal_record_count: usize,
    journal_last_record_hash: Option<String>,
    database_oid: u32,
    provider_object_count: usize,
    invariant_outcomes: Vec<ReferenceInvariantEvidence>,
}

impl ReferencePlannedCaseEvidence {
    /// Encodes the allowlisted evidence document.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if serialization fails.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[derive(Serialize)]
struct ReferenceInvariantEvidence {
    invariant_id: String,
    checkpoint_id: String,
    verdict: &'static str,
    witness_count: usize,
}

/// Provisions one isolated case database, runs a compiled serial case through
/// the real reference stack, and evaluates its final `PostgreSQL` checkpoint.
///
/// The current slice executes the supported client, webhook-response, and
/// checkout-owned SQL-probe application kills against the exact attested
/// container. Other process-cut placements are rejected before stack or
/// database mutation. Provider, gate, webhook, SQL, quiescence, journal, and
/// oracle boundaries are live.
///
/// # Errors
///
/// Returns [`ReferenceAppEvidenceError`] when stack attestation, provisioning,
/// planned execution, or oracle evaluation fails.
#[allow(clippy::too_many_lines)]
pub async fn run_reference_app_planned_case(
    planned_case: &PlannedCase,
    config: &ReferenceAppEvidenceConfig,
    journal_path: impl AsRef<Path>,
    configured_sql_probe: Option<ConfiguredSqlProbe>,
) -> Result<ReferencePlannedCaseEvidence, ReferenceAppEvidenceError> {
    preflight_reference_planned_case(planned_case, true, configured_sql_probe.is_some())?;
    let observed_stack = attest_reference_stack(
        config.postgres_port,
        &config.reference_app_url,
        &config.fixture_control_url,
    )
    .await?;
    if observed_stack != config.stack_attestation {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch.into());
    }
    restart_reference_application(config).await?;
    let postgres =
        TruthSpikePostgres::connect(SpikePostgresConfig::loopback_reference_app_with_archive(
            config.postgres_port,
            config.postgres_admin_role.clone(),
            config.postgres_admin_password.clone(),
            config.postgres_application_password.clone(),
            config.stack_attestation.postgres.clone(),
        ))
        .await
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let post_connect_stack = attest_reference_stack(
        config.postgres_port,
        &config.reference_app_url,
        &config.fixture_control_url,
    )
    .await?;
    if post_connect_stack != config.stack_attestation {
        return Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch.into());
    }
    let reset_sequence = fixture_control_sequence(config)
        .await?
        .checked_add(1)
        .ok_or(ReferenceAppEvidenceError::FixtureSequenceExhausted)?;
    let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
    let provisioned = postgres
        .provision_reference_databases(&suffix, config.compose_project.clone())
        .await
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let (_baseline_target, case_name, case_target, _baseline_archive) = provisioned
        .into_archive_parts()
        .ok_or(SpikePostgresError::ArchiveUnavailable)
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let database_oid = case_target.identity().database_oid();
    let operation_id = reference_operation_id(&case_name);
    let mut sql_probe =
        reference_case_sql_probe(&postgres, &case_target, configured_sql_probe).await?;
    let timestamp = current_unix_timestamp()?;
    let run_config = ReferenceCaseRunConfig::new(
        &case_name,
        &config.reference_app_url,
        &config.fixture_control_url,
        config.fixture_control_token.clone(),
        reset_sequence,
        timestamp,
        operation_id,
        2_500,
        "usd",
        Duration::from_secs(10),
        Duration::from_millis(10),
    )?;
    let mut process = AttestedReferenceProcessControl { config };
    let receipt = run_reference_planned_case_with_process(
        format!("run_{suffix}"),
        format!("case_{suffix}"),
        planned_case,
        journal_path,
        run_config,
        &mut process,
        sql_probe
            .as_mut()
            .map(|probe| probe as &mut dyn CaseSqlProbe),
    )
    .await?;
    let executed_action_count = receipt.executed().trace().action_count();
    let journal_record_count = receipt.executed().journal_summary().record_count();
    let journal_last_record_hash = receipt
        .executed()
        .journal_summary()
        .last_record_hash()
        .map(str::to_owned);
    let (_executed, checkpoint) = receipt.into_parts();
    let (provider_payment_intents, quiescence) = checkpoint.into_oracle_parts();
    let provider_object_count = provider_payment_intents.len();
    let report = postgres
        .check_reference_invariants(&case_name, &provider_payment_intents, quiescence)
        .await
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let invariant_outcomes = reference_invariant_evidence(&report);
    Ok(ReferencePlannedCaseEvidence {
        schema_version: 1,
        seed: planned_case.seed().value(),
        planned_action_count: planned_case.actions().len(),
        executed_action_count,
        journal_record_count,
        journal_last_record_hash,
        database_oid,
        provider_object_count,
        invariant_outcomes,
    })
}

async fn reference_case_sql_probe(
    postgres: &TruthSpikePostgres,
    case_target: &DatabaseTarget<Unverified>,
    configured: Option<ConfiguredSqlProbe>,
) -> Result<Option<ReferenceSqlProbe>, ReferenceAppEvidenceError> {
    match configured {
        Some(configured) => postgres
            .configured_sql_probe(
                case_target,
                configured,
                Duration::from_secs(10),
                Duration::from_millis(10),
            )
            .await
            .map(Some)
            .map_err(ReferenceAppEvidenceError::postgres),
        None => Ok(None),
    }
}

async fn fixture_control_sequence(
    config: &ReferenceAppEvidenceConfig,
) -> Result<u64, ReferenceAppEvidenceError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(ReferenceAppEvidenceError::FixtureStateRequest)?;
    let response = client
        .get(format!(
            "{}/v1/control/state",
            config.fixture_control_url.trim_end_matches('/')
        ))
        .header("X-Tiv-Control-Token", &config.fixture_control_token)
        .send()
        .await
        .map_err(ReferenceAppEvidenceError::FixtureStateRequest)?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(ReferenceAppEvidenceError::UnexpectedFixtureState);
    }
    let body = response
        .bytes()
        .await
        .map_err(ReferenceAppEvidenceError::FixtureStateRequest)?;
    if body.len() > MAX_FIXTURE_STATE_BYTES {
        return Err(ReferenceAppEvidenceError::UnexpectedFixtureState);
    }
    let state = serde_json::from_slice::<ReferenceFixtureState>(&body)
        .map_err(|_| ReferenceAppEvidenceError::UnexpectedFixtureState)?;
    if !state.held_gates.is_empty() {
        return Err(ReferenceAppEvidenceError::UnexpectedFixtureState);
    }
    Ok(state.command_sequence)
}

async fn restart_reference_application(
    config: &ReferenceAppEvidenceConfig,
) -> Result<(), ReferenceAppEvidenceError> {
    let container_id = config.stack_attestation.reference_app.as_str();
    let restarted = docker_output(&["restart", container_id])
        .await
        .map_err(|_| ReferenceAppEvidenceError::ReferenceApplicationRestart)?;
    if restarted.trim() != container_id {
        return Err(ReferenceAppEvidenceError::ReferenceApplicationRestart);
    }
    await_reference_application_health(config).await
}

struct AttestedReferenceProcessControl<'a> {
    config: &'a ReferenceAppEvidenceConfig,
}

impl ReferenceProcessControl for AttestedReferenceProcessControl<'_> {
    fn kill_application(&mut self) -> ReferenceProcessControlFuture<'_> {
        Box::pin(async move {
            let container_id = self.config.stack_attestation.reference_app.as_str();
            let killed = docker_output(&["kill", "--signal", "KILL", container_id])
                .await
                .map_err(|_| ReferenceProcessControlError::Kill)?;
            if killed.trim() != container_id {
                return Err(ReferenceProcessControlError::Kill);
            }
            let running =
                docker_output(&["inspect", "--format", "{{.State.Running}}", container_id])
                    .await
                    .map_err(|_| ReferenceProcessControlError::Kill)?;
            if running.trim() != "false" {
                return Err(ReferenceProcessControlError::Kill);
            }
            Ok(())
        })
    }

    fn restart_and_await_health(&mut self) -> ReferenceProcessControlFuture<'_> {
        Box::pin(async move {
            let container_id = self.config.stack_attestation.reference_app.as_str();
            let started = docker_output(&["start", container_id])
                .await
                .map_err(|_| ReferenceProcessControlError::Restart)?;
            if started.trim() != container_id {
                return Err(ReferenceProcessControlError::Restart);
            }
            await_reference_application_health(self.config)
                .await
                .map_err(|_| ReferenceProcessControlError::HealthOrAttestation)
        })
    }
}

async fn await_reference_application_health(
    config: &ReferenceAppEvidenceConfig,
) -> Result<(), ReferenceAppEvidenceError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_millis(500))
        .timeout(Duration::from_millis(500))
        .build()
        .map_err(|_| ReferenceAppEvidenceError::ReferenceApplicationRestart)?;
    timeout(Duration::from_secs(30), async {
        loop {
            let http_healthy = client
                .get(format!(
                    "{}/health",
                    config.reference_app_url.trim_end_matches('/')
                ))
                .send()
                .await
                .is_ok_and(|response| response.status() == reqwest::StatusCode::OK);
            let fully_attested = if http_healthy {
                attest_reference_stack(
                    config.postgres_port,
                    &config.reference_app_url,
                    &config.fixture_control_url,
                )
                .await
                .is_ok_and(|observed| observed == config.stack_attestation)
            } else {
                false
            };
            if fully_attested {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| ReferenceAppEvidenceError::ReferenceApplicationHealth)?;
    Ok(())
}

fn replay_sequences(initial: u64, offset: u64) -> Result<(u64, u64), ReferenceAppEvidenceError> {
    let reset = initial
        .checked_add(offset)
        .and_then(|sequence| sequence.checked_add(1))
        .ok_or(ReferenceAppEvidenceError::FixtureSequenceExhausted)?;
    let confirm = reset
        .checked_add(1)
        .ok_or(ReferenceAppEvidenceError::FixtureSequenceExhausted)?;
    Ok((reset, confirm))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceFixtureState {
    command_sequence: u64,
    #[serde(rename = "remaining_outcomes")]
    _remaining_outcomes: usize,
    held_gates: Vec<serde_json::Value>,
    #[serde(rename = "payment_intents")]
    _payment_intents: Vec<serde_json::Value>,
}

fn reference_invariant_evidence(report: &SnapshotReport) -> Vec<ReferenceInvariantEvidence> {
    report
        .outcomes()
        .iter()
        .map(|outcome| {
            let (verdict, witness_count) = match outcome.verdict() {
                InvariantVerdict::Held => ("held", 0),
                InvariantVerdict::Violated(witnesses) => ("violated", witnesses.len()),
            };
            ReferenceInvariantEvidence {
                invariant_id: outcome.identity().invariant().as_str().to_owned(),
                checkpoint_id: outcome.identity().checkpoint().as_str().to_owned(),
                verdict,
                witness_count,
            }
        })
        .collect()
}

async fn run_reference_app_attempt(
    postgres: &TruthSpikePostgres,
    plan: &ReplayPlan,
    config: &ReferenceAppEvidenceConfig,
    case_name: &DatabaseName,
    reset_sequence: u64,
    confirm_sequence: u64,
) -> Result<(usize, SnapshotReport), ReferenceAppEvidenceError> {
    let timestamp = current_unix_timestamp()?;
    let replay_config = config.replay_config(
        case_name.clone(),
        reset_sequence,
        confirm_sequence,
        timestamp,
    )?;
    let receipt = run_reference_app_replay(plan, &replay_config).await?;
    let provider_object_count = receipt.provider_payment_intents().len();
    let report = evaluate_completed_replay(postgres, case_name, receipt).await?;
    Ok((provider_object_count, report))
}

async fn run_reference_app_attempt_at_offset(
    postgres: &TruthSpikePostgres,
    plan: &ReplayPlan,
    config: &ReferenceAppEvidenceConfig,
    case_name: &DatabaseName,
    initial_control_sequence: u64,
    offset: u64,
) -> Result<(usize, SnapshotReport), ReferenceAppEvidenceError> {
    let (reset_sequence, confirm_sequence) = replay_sequences(initial_control_sequence, offset)?;
    run_reference_app_attempt(
        postgres,
        plan,
        config,
        case_name,
        reset_sequence,
        confirm_sequence,
    )
    .await
}

fn current_unix_timestamp() -> Result<i64, ReferenceAppEvidenceError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ReferenceAppEvidenceError::InvalidSystemTime)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| ReferenceAppEvidenceError::InvalidSystemTime)
}

async fn evaluate_completed_replay(
    postgres: &TruthSpikePostgres,
    case_name: &DatabaseName,
    receipt: ReferenceAppReplayReceipt,
) -> Result<SnapshotReport, ReferenceAppEvidenceError> {
    let (provider_payment_intents, completion) = receipt.into_oracle_input();
    let provider_objects = provider_payment_intents
        .iter()
        .map(|payment_intent| {
            ProviderPaymentIntent::new(
                payment_intent.id(),
                payment_intent
                    .operation_id()
                    .ok_or(ReferenceAppEvidenceError::InvalidProviderProjection)?,
                payment_intent.amount_minor(),
                payment_intent.currency(),
                payment_intent.status(),
            )
            .map_err(|_| ReferenceAppEvidenceError::InvalidProviderProjection)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let quiescence = QuiescencePermit::after_reference_app_replay_completed(completion);
    postgres
        .check_reference_invariants(case_name, &provider_objects, quiescence)
        .await
        .map_err(ReferenceAppEvidenceError::postgres)
}

fn uniqueness_attempt(
    report: &SnapshotReport,
    case_name: &DatabaseName,
) -> Result<(FailureIdentity, AttemptResult), ReferenceAppEvidenceError> {
    let expected_operation_id = reference_operation_id(case_name);
    let outcome = report
        .outcome(PROVIDER_UNIQUENESS_ID)
        .ok_or(ReferenceAppEvidenceError::MissingProviderUniquenessOutcome)?;
    let identity = outcome.identity().clone();
    match outcome.verdict() {
        InvariantVerdict::Held => Ok((identity, AttemptResult::Held)),
        InvariantVerdict::Violated(witnesses) => {
            if witnesses.len() != 1
                || witnesses[0].operation_id() != expected_operation_id
                || witnesses[0].provider_object_count() != 2
            {
                return Err(ReferenceAppEvidenceError::UnexpectedProviderUniquenessWitness);
            }
            Ok((identity.clone(), AttemptResult::Violation(identity)))
        }
    }
}

fn reference_operation_id(case_name: &DatabaseName) -> String {
    format!("op_{}", case_name.as_str().trim_start_matches("tiv_case_"))
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ReferenceAppEvidenceConfigError {
    #[error("isolated PostgreSQL configuration is invalid")]
    InvalidPostgresConfiguration,
    #[error("the running reference containers do not match the isolated stack")]
    ReferenceStackMismatch,
    #[error("the local Docker reference stack could not be inspected")]
    ReferenceStackInspectionUnavailable,
    #[error("reference app replay configuration is invalid: {0}")]
    Replay(#[from] ReferenceAppReplayConfigError),
}

#[derive(Debug, Error)]
pub enum ReferenceAppEvidenceError {
    #[error("isolated reference PostgreSQL step failed: {0}")]
    Postgres(String),
    #[error("reference app evidence configuration is invalid: {0}")]
    Config(#[from] ReferenceAppEvidenceConfigError),
    #[error("reference app replay failed: {0}")]
    Replay(#[from] ReferenceAppReplayError),
    #[error("reference planned-case configuration is invalid: {0}")]
    PlannedCaseConfig(#[from] ReferenceCaseRunConfigError),
    #[error("reference planned-case execution failed: {0}")]
    PlannedCaseRun(#[from] ReferenceCaseRunError),
    #[error("reference fixture state request failed: {0}")]
    FixtureStateRequest(reqwest::Error),
    #[error("reference fixture state was incoherent or had a held provider gate")]
    UnexpectedFixtureState,
    #[error("reference fixture command sequence was exhausted")]
    FixtureSequenceExhausted,
    #[error("the attested reference application could not be restarted")]
    ReferenceApplicationRestart,
    #[error("the restarted reference application did not become healthy")]
    ReferenceApplicationHealth,
    #[error("reference replay returned an invalid provider projection")]
    InvalidProviderProjection,
    #[error("the system clock could not produce a valid webhook timestamp")]
    InvalidSystemTime,
    #[error("the reference oracle omitted provider-object-unique")]
    MissingProviderUniquenessOutcome,
    #[error("provider-object-unique returned an unexpected bounded witness")]
    UnexpectedProviderUniquenessWitness,
    #[error("reference replay evidence was incoherent: {0}")]
    Evidence(#[from] EvidenceError),
}

impl ReferenceAppEvidenceError {
    fn postgres(error: impl std::fmt::Display) -> Self {
        Self::Postgres(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_INSPECTION: &str = concat!(
        "true\n",
        "healthy\n",
        "disposable-reference-app-postgres\n",
        r#"{"5432/tcp":[{"HostIp":"127.0.0.1","HostPort":"15432"}]}"#,
        "\n",
        r#"["postgres","-c","cluster_name=tiv-reference-app-postgres"]"#,
        "\npostgres:18.4-bookworm\n",
    );

    #[test]
    fn postgres_container_attestation_requires_the_exact_reference_stack_boundary() {
        assert!(
            validate_reference_service_inspection(VALID_INSPECTION, &postgres_service(15_432))
                .is_ok()
        );
        assert!(matches!(
            validate_reference_service_inspection(VALID_INSPECTION, &postgres_service(54_321)),
            Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch)
        ));
        assert!(matches!(
            validate_reference_service_inspection(
                &VALID_INSPECTION.replace(
                    "disposable-reference-app-postgres",
                    "disposable-postgres-truth-spike"
                ),
                &postgres_service(15_432),
            ),
            Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch)
        ));
    }

    #[test]
    fn http_container_attestation_binds_each_loopback_endpoint_to_its_service() {
        let app_inspection = concat!(
            "true\nhealthy\n",
            "disposable-known-bug-reference-application\n",
            r#"{"18080/tcp":[{"HostIp":"127.0.0.1","HostPort":"18080"}]}"#,
            "\n",
            r#"["tiv-reference-app"]"#,
            "\ntiv-reference-app-spike-reference-app\n",
        );
        let fixture_inspection = concat!(
            "true\nhealthy\n",
            "disposable-stripe-payment-intent-fixture\n",
            r#"{"12112/tcp":[{"HostIp":"127.0.0.1","HostPort":"12112"}]}"#,
            "\n",
            r#"["tiv-stripe-pi-fixture"]"#,
            "\ntiv-reference-app-spike-stripe-fixture\n",
        );

        assert!(
            validate_reference_service_inspection(app_inspection, &reference_app_service(18_080))
                .is_ok()
        );
        assert!(
            validate_reference_service_inspection(fixture_inspection, &fixture_service(12_112))
                .is_ok()
        );
        assert!(matches!(
            validate_reference_service_inspection(app_inspection, &reference_app_service(12_112)),
            Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch)
        ));
    }

    #[test]
    fn direct_fault_evidence_accepts_only_the_intended_reference_app_mode_set() {
        assert!(
            validate_reference_app_evidence_mode_inspection(
                "retry caller reconciliation webhook ledger\n"
            )
            .is_ok()
        );
        assert!(
            validate_reference_app_evidence_mode_inspection(
                "ledger retry webhook reconciliation caller\n"
            )
            .is_ok()
        );
        for mismatched in [
            "",
            "retry\n",
            "retry caller webhook ledger\n",
            "retry caller reconciliation webhook\n",
            "retry caller reconciliation ledger\n",
            "caller reconciliation webhook ledger\n",
            "retry retry caller reconciliation webhook ledger\n",
            "retry caller reconciliation webhook ledger reconciliation_conflict\n",
            "retry caller reconciliation webhook ledger extra\n",
        ] {
            assert!(matches!(
                validate_reference_app_evidence_mode_inspection(mismatched),
                Err(ReferenceAppEvidenceConfigError::ReferenceStackMismatch)
            ));
        }
    }

    #[tokio::test]
    async fn bounded_process_kills_and_reaps_a_timed_out_child() {
        let started = std::time::Instant::now();
        let mut command = Command::new("sleep");
        command.arg("60");
        let result = bounded_output(command, Duration::from_millis(25)).await;

        assert!(matches!(result, Err(BoundedProcessError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
