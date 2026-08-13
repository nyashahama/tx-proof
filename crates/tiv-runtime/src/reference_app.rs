//! Self-contained evidence execution for the isolated reference application.

use std::{
    collections::BTreeMap,
    process::{ExitStatus, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use thiserror::Error;
use tiv_core::result::FailureIdentity;
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
        safety::{ComposeProjectId, DatabaseName},
        spike::{SpikePostgresConfig, TruthSpikePostgres},
    },
    replay::{
        ReferenceAppReplayConfig, ReferenceAppReplayConfigError, ReferenceAppReplayError,
        ReferenceAppReplayReceipt, ReplayPlan, run_reference_app_replay,
    },
};

const PROVIDER_UNIQUENESS_ID: &str = "provider-object-unique";
const OPERATION_ID: &str = "op_1";
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

/// Provisions one reference database pair, runs both replay legs around a
/// template reset, and returns allowlisted same-failure evidence.
///
/// The known reference application completes database work synchronously
/// before each HTTP response. Therefore completion of the replay driver's
/// checkout, webhook delivery, and final provider-state read is the runtime's
/// quiescence boundary for this synthetic application only.
///
/// # Errors
///
/// Returns [`ReferenceAppEvidenceError`] when `PostgreSQL` safety checks, replay,
/// oracle evaluation, reset identity, or evidence coherence fails.
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
    let postgres = TruthSpikePostgres::connect(SpikePostgresConfig::loopback_reference_app(
        config.postgres_port,
        config.postgres_admin_role.clone(),
        config.postgres_admin_password.clone(),
        config.postgres_application_password.clone(),
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
    let suffix = Uuid::new_v4().simple().to_string()[..16].to_owned();
    let provisioned = postgres
        .provision_reference_databases(&suffix, config.compose_project.clone())
        .await
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let baseline_target = provisioned.baseline_target().clone();
    let case_name = provisioned.case_name().clone();
    let first_database_oid = provisioned.case_target().identity().database_oid();

    let first_timestamp = current_unix_timestamp()?;
    let first_config = config.replay_config(case_name.clone(), 1, 2, first_timestamp)?;
    let first_receipt = run_reference_app_replay(plan, &first_config).await?;
    let first_provider_object_count = first_receipt.provider_payment_intents().len();
    let first_report = evaluate_completed_replay(&postgres, &case_name, first_receipt).await?;
    let first_failure = provider_uniqueness_failure(&first_report)?;

    let reset_target = postgres
        .reset_case_from_template(
            provisioned.into_case_target(),
            &baseline_target,
            Uuid::new_v4(),
        )
        .await
        .map_err(ReferenceAppEvidenceError::postgres)?;
    let reset_database_oid = reset_target.identity().database_oid();

    let replay_timestamp = current_unix_timestamp()?;
    let replay_config = config.replay_config(case_name.clone(), 3, 4, replay_timestamp)?;
    let replay_receipt = run_reference_app_replay(plan, &replay_config).await?;
    let replay_report = evaluate_completed_replay(&postgres, &case_name, replay_receipt).await?;
    let replay_failure = provider_uniqueness_failure(&replay_report)?;

    TruthSpikeEvidence::new(
        first_provider_object_count,
        first_database_oid,
        reset_database_oid,
        &first_failure,
        &replay_failure,
    )
    .map_err(ReferenceAppEvidenceError::Evidence)
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

fn provider_uniqueness_failure(
    report: &SnapshotReport,
) -> Result<FailureIdentity, ReferenceAppEvidenceError> {
    let outcome = report
        .outcome(PROVIDER_UNIQUENESS_ID)
        .ok_or(ReferenceAppEvidenceError::MissingProviderUniquenessFailure)?;
    let InvariantVerdict::Violated(witnesses) = outcome.verdict() else {
        return Err(ReferenceAppEvidenceError::MissingProviderUniquenessFailure);
    };
    if witnesses.len() != 1
        || witnesses[0].operation_id() != OPERATION_ID
        || witnesses[0].provider_object_count() != 2
    {
        return Err(ReferenceAppEvidenceError::UnexpectedProviderUniquenessWitness);
    }
    Ok(outcome.identity().clone())
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
    #[error("reference replay returned an invalid provider projection")]
    InvalidProviderProjection,
    #[error("the system clock could not produce a valid webhook timestamp")]
    InvalidSystemTime,
    #[error("provider-object-unique did not fail after reference replay")]
    MissingProviderUniquenessFailure,
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
