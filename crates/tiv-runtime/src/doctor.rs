//! Non-mutating configuration and local Docker Compose safety preflight.

use std::{
    net::IpAddr,
    path::Path,
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
use url::Url;

use crate::config::{
    ConfigError, EnvironmentLookup, RedactedConfig, ResolvedConfig, load_resolved_config,
    reject_live_stripe_material,
};

const LOCAL_DOCKER_HOST: &str = "unix:///var/run/docker.sock";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
const DOCTOR_SCHEMA_VERSION: u16 = 1;

/// One exact argv-only process invocation.
pub struct CommandSpec {
    program: String,
    args: Vec<String>,
}

impl CommandSpec {
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// The two read-only Compose commands used by `doctor`.
pub struct ComposeProbePlan {
    project_name: String,
    commands: [CommandSpec; 2],
}

impl ComposeProbePlan {
    #[must_use]
    pub fn commands(&self) -> &[CommandSpec] {
        &self.commands
    }
}

/// Allowlisted compatibility facts from the resolved, redacted Compose graph.
#[derive(Serialize)]
pub struct ComposeFacts {
    compose_version: String,
    services: Vec<String>,
    resolved_redacted_hash: String,
}

/// Machine-readable output of one non-mutating safety preflight.
#[derive(Serialize)]
pub struct DoctorReport {
    schema_version: u16,
    status: &'static str,
    mutation_authorized: bool,
    probe_project_name: String,
    config: RedactedConfig,
    compose: ComposeFacts,
}

impl DoctorReport {
    /// Serializes the allowlisted report.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if JSON encoding fails.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Creates the exact local, read-only Compose probe plan.
///
/// # Errors
///
/// Returns [`DoctorError::NonUtf8Path`] when a canonical path cannot be safely
/// represented as one argv element.
pub fn compose_probe_plan(config: &ResolvedConfig) -> Result<ComposeProbePlan, DoctorError> {
    let root = config.root().to_str().ok_or(DoctorError::NonUtf8Path)?;
    let digest = blake3::hash(root.as_bytes()).to_hex();
    let project_name = format!("tiv-doctor-{}", &digest[..12]);
    let mut common = vec![
        "--host".to_owned(),
        LOCAL_DOCKER_HOST.to_owned(),
        "compose".to_owned(),
        "--project-name".to_owned(),
        project_name.clone(),
        "--project-directory".to_owned(),
        root.to_owned(),
    ];
    for file in config.compose_files() {
        common.push("--file".to_owned());
        common.push(file.to_str().ok_or(DoctorError::NonUtf8Path)?.to_owned());
    }
    let mut version_args = common.clone();
    version_args.extend(["version".to_owned(), "--short".to_owned()]);
    let mut config_args = common;
    config_args.extend([
        "config".to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
    ]);
    Ok(ComposeProbePlan {
        project_name,
        commands: [
            CommandSpec {
                program: "docker".to_owned(),
                args: version_args,
            },
            CommandSpec {
                program: "docker".to_owned(),
                args: config_args,
            },
        ],
    })
}

/// Evaluates one Compose JSON document into secret-free compatibility facts.
///
/// # Errors
///
/// Returns [`DoctorError`] when service mappings are missing, live Stripe
/// material or a public database is present, or the document is malformed.
pub fn evaluate_compose_config(
    config: &ResolvedConfig,
    compose_version: &str,
    document: &str,
) -> Result<ComposeFacts, DoctorError> {
    let version = compose_version.trim();
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    {
        return Err(DoctorError::InvalidComposeVersion);
    }
    let mut value: Value = serde_json::from_str(document).map_err(DoctorError::ComposeJson)?;
    let services = value
        .get("services")
        .and_then(Value::as_object)
        .ok_or(DoctorError::InvalidComposeDocument)?;
    let required = std::iter::once(config.application_service())
        .chain(std::iter::once(config.postgres_service()))
        .chain(std::iter::once(config.stripe_service()))
        .chain(config.worker_services().iter().map(String::as_str));
    for service in required {
        if !services.contains_key(service) {
            return Err(DoctorError::MissingComposeService(service.to_owned()));
        }
    }
    let service_names = services.keys().cloned().collect::<Vec<_>>();
    scan_and_redact(&mut value, config.postgres_service())?;
    let canonical = serde_json::to_vec(&value).map_err(DoctorError::ComposeEncode)?;
    Ok(ComposeFacts {
        compose_version: version.to_owned(),
        services: service_names,
        resolved_redacted_hash: blake3::hash(&canonical).to_hex().to_string(),
    })
}

/// Runs the complete non-mutating doctor preflight.
///
/// # Errors
///
/// Returns [`DoctorError`] for configuration, local Docker, Compose, output
/// bounds, or safety failures. No command in this function starts services or
/// authorizes database mutation.
pub async fn run_doctor(
    config_path: &Path,
    environment: &impl EnvironmentLookup,
) -> Result<DoctorReport, DoctorError> {
    let config = load_resolved_config(config_path, environment)?;
    let plan = compose_probe_plan(&config)?;
    let version = run_command(&plan.commands[0]).await?;
    let resolved = run_command(&plan.commands[1]).await?;
    let version = String::from_utf8(version.stdout).map_err(|_| DoctorError::NonUtf8Output)?;
    let resolved = String::from_utf8(resolved.stdout).map_err(|_| DoctorError::NonUtf8Output)?;
    let compose = evaluate_compose_config(&config, &version, &resolved)?;
    Ok(DoctorReport {
        schema_version: DOCTOR_SCHEMA_VERSION,
        status: "ready",
        mutation_authorized: false,
        probe_project_name: plan.project_name,
        config: config.into_redacted(),
        compose,
    })
}

fn scan_and_redact(value: &mut Value, postgres_service: &str) -> Result<(), DoctorError> {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key.eq_ignore_ascii_case("livemode") && child.as_bool() == Some(true) {
                    return Err(DoctorError::LiveStripeMaterial);
                }
                if key.eq_ignore_ascii_case("environment") {
                    scan_and_redact(child, postgres_service)?;
                    redact_environment(child);
                    continue;
                }
                scan_value(child, postgres_service)?;
                if sensitive_key(key) {
                    *child = Value::String("[REDACTED]".to_owned());
                } else {
                    scan_and_redact(child, postgres_service)?;
                }
            }
        }
        Value::Array(values) => {
            for child in values {
                scan_and_redact(child, postgres_service)?;
            }
        }
        _ => scan_value(value, postgres_service)?,
    }
    Ok(())
}

fn redact_environment(value: &mut Value) {
    match value {
        Value::Object(environment) => {
            for child in environment.values_mut() {
                *child = Value::String("[REDACTED]".to_owned());
            }
        }
        Value::Array(environment) => {
            for child in environment {
                *child = Value::String("[REDACTED]".to_owned());
            }
        }
        _ => *value = Value::String("[REDACTED]".to_owned()),
    }
}

fn scan_value(value: &Value, postgres_service: &str) -> Result<(), DoctorError> {
    let Some(text) = value.as_str() else {
        return Ok(());
    };
    reject_live_stripe_material(text).map_err(|_| DoctorError::LiveStripeMaterial)?;
    let lowercase = text.to_ascii_lowercase();
    if lowercase.contains("api.stripe.com") || lowercase.contains("dashboard.stripe.com") {
        return Err(DoctorError::LiveStripeMaterial);
    }
    if lowercase.starts_with("postgres://") || lowercase.starts_with("postgresql://") {
        let url = Url::parse(text).map_err(|_| DoctorError::PublicDatabaseTarget)?;
        let host = url.host_str().ok_or(DoctorError::PublicDatabaseTarget)?;
        let is_local = host == "localhost"
            || host == postgres_service
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if !is_local {
            return Err(DoctorError::PublicDatabaseTarget);
        }
    }
    Ok(())
}

fn sensitive_key(key: &str) -> bool {
    let lowercase = key.to_ascii_lowercase();
    [
        "password",
        "secret",
        "token",
        "api_key",
        "authorization",
        "cookie",
        "database_url",
        "admin_url",
        "case_url",
    ]
    .iter()
    .any(|needle| lowercase.contains(needle))
}

struct CommandOutput {
    stdout: Vec<u8>,
}

async fn run_command(spec: &CommandSpec) -> Result<CommandOutput, DoctorError> {
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| DoctorError::DockerUnavailable)?;
    let stdout = child.stdout.take().ok_or(DoctorError::DockerUnavailable)?;
    let stderr = child.stderr.take().ok_or(DoctorError::DockerUnavailable)?;
    let stdout_reader = tokio::spawn(read_bounded(stdout));
    let stderr_reader = tokio::spawn(read_bounded(stderr));
    let status = match timeout(COMMAND_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            drop(child);
            finish_readers(stdout_reader, stderr_reader).await?;
            return Err(DoctorError::DockerWait);
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = timeout(CLEANUP_TIMEOUT, child.wait()).await;
            drop(child);
            finish_readers(stdout_reader, stderr_reader).await?;
            return Err(DoctorError::DockerTimeout);
        }
    };
    let (stdout, _stderr) = finish_readers(stdout_reader, stderr_reader).await?;
    require_success(status)?;
    Ok(CommandOutput { stdout })
}

fn require_success(status: ExitStatus) -> Result<(), DoctorError> {
    if status.success() {
        Ok(())
    } else {
        Err(DoctorError::DockerCommandFailed)
    }
}

async fn read_bounded(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>, DoctorError> {
    let mut output = Vec::new();
    reader
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)
        .await
        .map_err(|_| DoctorError::DockerOutput)?;
    if output.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(DoctorError::DockerOutputTooLarge);
    }
    Ok(output)
}

async fn finish_readers(
    mut stdout: JoinHandle<Result<Vec<u8>, DoctorError>>,
    mut stderr: JoinHandle<Result<Vec<u8>, DoctorError>>,
) -> Result<(Vec<u8>, Vec<u8>), DoctorError> {
    let result = timeout(CLEANUP_TIMEOUT, async {
        let stdout = (&mut stdout)
            .await
            .map_err(|_| DoctorError::DockerOutput)??;
        let stderr = (&mut stderr)
            .await
            .map_err(|_| DoctorError::DockerOutput)??;
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
        Err(DoctorError::DockerOutput)
    }
}

#[derive(Debug, Error)]
pub enum DoctorError {
    #[error("configuration safety preflight failed: {0}")]
    Config(#[from] ConfigError),
    #[error("a canonical Compose path is not valid UTF-8")]
    NonUtf8Path,
    #[error("Docker is unavailable on the required local socket")]
    DockerUnavailable,
    #[error("the local Docker process could not be awaited")]
    DockerWait,
    #[error("the local Docker command exceeded its deadline")]
    DockerTimeout,
    #[error("the local Docker command failed")]
    DockerCommandFailed,
    #[error("the local Docker command returned invalid output")]
    DockerOutput,
    #[error("the local Docker command exceeded its output limit")]
    DockerOutputTooLarge,
    #[error("the local Docker command output was not UTF-8")]
    NonUtf8Output,
    #[error("the Docker Compose version was invalid")]
    InvalidComposeVersion,
    #[error("the resolved Compose JSON was invalid: {0}")]
    ComposeJson(serde_json::Error),
    #[error("the resolved Compose document did not contain services")]
    InvalidComposeDocument,
    #[error("the resolved Compose graph is missing service {0}")]
    MissingComposeService(String),
    #[error("the resolved Compose graph contains live Stripe material")]
    LiveStripeMaterial,
    #[error("the resolved Compose graph contains a public database target")]
    PublicDatabaseTarget,
    #[error("the redacted Compose document could not be encoded: {0}")]
    ComposeEncode(serde_json::Error),
}

impl DoctorError {
    /// Distinguishes setup failures from configuration and safety failures at
    /// the CLI boundary.
    #[must_use]
    pub fn is_infrastructure_failure(&self) -> bool {
        matches!(
            self,
            Self::DockerUnavailable
                | Self::DockerWait
                | Self::DockerTimeout
                | Self::DockerCommandFailed
                | Self::DockerOutput
                | Self::DockerOutputTooLarge
                | Self::NonUtf8Output
                | Self::InvalidComposeVersion
                | Self::ComposeJson(_)
                | Self::ComposeEncode(_)
        )
    }
}
