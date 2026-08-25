//! Command-line composition root for `TxProof`.

use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use thiserror::Error;
use tiv_core::{
    plan::{PlanActionKind, PlannedCase, ProcessCutPoint},
    trace::CompiledTrace,
};
use tiv_runtime::{
    artifacts::{ArtifactError, verify_complete_run_artifact},
    baseline::{BaselineError, run_configured_baseline},
    cleanup::{CleanupError, CleanupOptions, cleanup_configured_run},
    config::{ConfigError, ProcessEnvironment, load_resolved_config},
    configured_campaign::{
        ConfiguredCampaignError, ConfiguredCampaignOptions, ConfiguredCampaignOptionsError,
        ConfiguredCampaignVerdict, RunCancellation, run_configured_campaign_with_cancellation,
    },
    configured_minimized_replay::{
        ConfiguredMinimizedReplayError, run_configured_minimized_replay_with_cancellation,
    },
    configured_replay::{
        ConfiguredReplayError, ConfiguredReplayOptions, ConfiguredReplayOptionsError,
        run_configured_replay_with_cancellation,
    },
    configured_shrink::{
        ConfiguredShrinkError, ConfiguredShrinkOptions, ConfiguredShrinkOptionsError,
        run_configured_shrink_with_cancellation,
    },
    doctor::{DoctorError, run_doctor},
    init::{InitError, initialize_project},
    postgres::{
        probe::{ConfiguredSqlProbeError, load_configured_sql_probe},
        safety::DatabaseName,
    },
    reference_app::{
        ReferenceAppEvidenceConfig, ReferenceAppEvidenceConfigError, ReferenceAppEvidenceError,
        run_reference_app_evidence, run_reference_app_planned_case,
    },
    replay::{
        ReferenceAppReplayConfig, ReferenceAppReplayConfigError, ReferenceAppReplayError,
        ReplayPlan, ReplayPlanError, run_reference_app_replay,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct TraceSummary {
    pub schema_version: u16,
    pub action_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ArtifactInspectionReceipt<'a> {
    schema_version: u16,
    status: &'static str,
    run_id: &'a str,
    complete: bool,
    checksums_verified: bool,
    compatibility_verified: bool,
    indexed_file_count: usize,
}

#[derive(Debug)]
pub struct CliOutput {
    body: String,
    exit_code: u8,
}

impl CliOutput {
    fn success(body: String) -> Self {
        Self { body, exit_code: 0 }
    }

    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        self.exit_code
    }
}

#[derive(Debug, Parser, PartialEq)]
#[command(
    name = "tiv",
    version,
    about = "Local transactional invariant verifier"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, PartialEq, Subcommand)]
pub enum Command {
    /// Write a fail-closed version-one project scaffold without overwriting files.
    Init,
    /// Validate configuration and inspect local Compose readiness without mutation.
    Doctor(DoctorArgs),
    /// Seal and reset-prove one attested disposable `PostgreSQL` baseline.
    Baseline(BaselineArgs),
    /// Execute a serial configured campaign against the attested disposable stack.
    Run(RunArgs),
    /// Verify one complete run artifact without executing customer code.
    Inspect { path: PathBuf },
    /// Remove one exact, verified, unreferenced complete run artifact.
    Cleanup(CleanupArgs),
    /// Inspect replay readiness without executing customer code.
    Replay {
        #[command(subcommand)]
        command: ReplayCommand,
    },
    /// Minimize one reproducible configured replay under fixed v1 budgets.
    Shrink {
        #[command(subcommand)]
        command: ShrinkCommand,
    },
    /// Inspect and validate compiled replay traces.
    Trace {
        #[command(subcommand)]
        command: TraceCommand,
    },
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct CleanupArgs {
    /// Typed TOML configuration that owns the private artifact directory.
    #[arg(long, default_value = "tiv.toml")]
    pub config: PathBuf,
    /// Exact run identifier; artifact paths and staging names are rejected.
    #[arg(long)]
    pub run: String,
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct BaselineArgs {
    /// Typed TOML configuration for the disposable customer stack.
    #[arg(long, default_value = "tiv.toml")]
    pub config: PathBuf,
    /// Exact identity-bound phrase emitted by the challenge stage.
    #[arg(long)]
    pub acknowledge_reset: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct DoctorArgs {
    /// Typed TOML configuration to validate and inspect.
    #[arg(long, default_value = "tiv.toml")]
    pub config: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct RunArgs {
    /// Typed TOML configuration for the disposable customer stack.
    #[arg(long, default_value = "tiv.toml")]
    pub config: PathBuf,
    /// Optional deterministic campaign-seed override.
    #[arg(long)]
    pub seed: Option<u64>,
    /// Optional serial case-count override within the v1 maximum.
    #[arg(long)]
    pub cases: Option<u32>,
    /// Emit CI-oriented status and preserve the run verdict exit contract.
    #[arg(long)]
    pub ci: bool,
}

#[derive(Debug, PartialEq, Subcommand)]
pub enum ReplayCommand {
    /// Compile a trace into the runtime replay plan without executing it.
    Inspect { path: PathBuf },
    /// Replay one violating case from a verified configured-run artifact.
    Configured(ConfiguredReplayArgs),
    /// Replay the authority-bound minimized trace from a verified shrink artifact.
    Minimized(MinimizedReplayArgs),
    /// Execute the committed reference-app checkout path against loopback services.
    ReferenceApp(ReferenceAppReplayArgs),
    /// Run three fresh-baseline reference attempts into bounded evidence.
    ReferenceAppEvidence(ReferenceAppEvidenceArgs),
    /// Execute one compiled serial case through the attested reference stack.
    ReferenceAppCase(ReferenceAppCaseArgs),
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct ConfiguredReplayArgs {
    /// Complete configured-run artifact containing the recorded case.
    #[arg(long)]
    pub artifact: PathBuf,
    /// Typed TOML configuration for the same disposable customer stack.
    #[arg(long, default_value = "tiv.toml")]
    pub config: PathBuf,
    /// One-based recorded campaign case to replay exactly three times.
    #[arg(long)]
    pub case: u32,
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct MinimizedReplayArgs {
    /// Complete configured-shrink artifact containing minimized trace authority.
    #[arg(long)]
    pub artifact: PathBuf,
    /// Typed TOML configuration for the same disposable customer stack.
    #[arg(long, default_value = "tiv.toml")]
    pub config: PathBuf,
}

#[derive(Debug, PartialEq, Subcommand)]
pub enum ShrinkCommand {
    /// Minimize one verified configured replay without changing failure identity.
    Configured(ConfiguredShrinkArgs),
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct ConfiguredShrinkArgs {
    /// Complete configured-replay artifact containing reproducible source evidence.
    #[arg(long)]
    pub artifact: PathBuf,
    /// Typed TOML configuration for the same disposable customer stack.
    #[arg(long, default_value = "tiv.toml")]
    pub config: PathBuf,
    /// Maximum unique candidates evaluated, capped at the v1 limit of 60.
    #[arg(long, default_value_t = 60)]
    pub max_candidates: u8,
    /// Shared source-recheck and candidate-search budget (`ms`, `s`, or `m`; maximum 10m).
    ///
    /// A safety recovery already in progress may finish after this deadline.
    #[arg(long, default_value = "10m", value_parser = parse_shrink_duration)]
    pub max_time: Duration,
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct ReferenceAppReplayArgs {
    /// Compiled trace JSON document to replay.
    #[arg(long)]
    pub trace: PathBuf,
    /// Generated case database already provisioned in the isolated reference stack.
    #[arg(long)]
    pub case_database: String,
    /// Loopback URL for the real reference application.
    #[arg(long)]
    pub reference_app_url: String,
    /// Loopback URL for the fixture control listener.
    #[arg(long)]
    pub fixture_control_url: String,
    /// Run-scoped fixture control token.
    #[arg(long)]
    pub fixture_control_token: String,
    /// Fixture reset command sequence.
    #[arg(long, default_value_t = 1)]
    pub reset_sequence: u64,
    /// Fixture confirm command sequence.
    #[arg(long, default_value_t = 2)]
    pub confirm_sequence: u64,
    /// Synthetic webhook timestamp used by the fixture.
    #[arg(long)]
    pub webhook_timestamp: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct ReferenceAppEvidenceArgs {
    /// Compiled trace JSON document to replay three times.
    #[arg(long)]
    pub trace: PathBuf,
    /// Loopback host port for the isolated `PostgreSQL` service.
    #[arg(long, default_value_t = 15_432)]
    pub postgres_port: u16,
    /// Administrative role for the isolated `PostgreSQL` service.
    #[arg(long, default_value = "tiv_admin")]
    pub postgres_admin_role: String,
    /// Loopback URL for the real reference application.
    #[arg(long)]
    pub reference_app_url: String,
    /// Loopback URL for the fixture control listener.
    #[arg(long)]
    pub fixture_control_url: String,
}

#[derive(Clone, Debug, PartialEq, Args)]
pub struct ReferenceAppCaseArgs {
    /// Validated compiled planned-case JSON document to execute.
    #[arg(long)]
    pub plan: PathBuf,
    /// New JSON-lines observation journal path for this case.
    #[arg(long)]
    pub journal: PathBuf,
    /// Typed project configuration required by configured SQL-probe cut points.
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Loopback host port for the isolated `PostgreSQL` service.
    #[arg(long, default_value_t = 15_432)]
    pub postgres_port: u16,
    /// Administrative role for the isolated `PostgreSQL` service.
    #[arg(long, default_value = "tiv_admin")]
    pub postgres_admin_role: String,
    /// Loopback URL for the real reference application.
    #[arg(long)]
    pub reference_app_url: String,
    /// Loopback URL for the fixture control listener.
    #[arg(long)]
    pub fixture_control_url: String,
}

#[derive(Debug, PartialEq, Subcommand)]
pub enum TraceCommand {
    /// Validate schema, graph, captured values, and replay bindings.
    Validate { path: PathBuf },
}

/// Executes one synchronous CLI command.
///
/// # Errors
///
/// Returns [`CliError`] when the input cannot be read, validated, or encoded.
pub fn execute(cli: Cli) -> Result<String, CliError> {
    match cli.command {
        Command::Init => {
            let root = std::env::current_dir().map_err(CliError::CurrentDirectory)?;
            let report = initialize_project(&root)?;
            report.to_pretty_json().map_err(CliError::Encode)
        }
        Command::Replay {
            command: ReplayCommand::Inspect { path },
        } => {
            let plan = replay_plan_from_path(&path)?;
            serde_json::to_string(&plan).map_err(CliError::Encode)
        }
        Command::Inspect { path } => {
            let verified = verify_complete_run_artifact(&path)?;
            let receipt = ArtifactInspectionReceipt {
                schema_version: 1,
                status: "complete_artifact_verified",
                run_id: verified.run_id(),
                complete: true,
                checksums_verified: true,
                compatibility_verified: true,
                indexed_file_count: verified.indexed_file_count(),
            };
            serde_json::to_string(&receipt).map_err(CliError::Encode)
        }
        Command::Cleanup(args) => {
            let options = CleanupOptions::new(args.run)?;
            let receipt = cleanup_configured_run(&args.config, &ProcessEnvironment, &options)?;
            receipt.to_pretty_json().map_err(CliError::Encode)
        }
        Command::Doctor(_)
        | Command::Baseline(_)
        | Command::Run(_)
        | Command::Shrink { .. }
        | Command::Replay {
            command:
                ReplayCommand::ReferenceApp(_)
                | ReplayCommand::ReferenceAppEvidence(_)
                | ReplayCommand::ReferenceAppCase(_)
                | ReplayCommand::Configured(_)
                | ReplayCommand::Minimized(_),
        } => Err(CliError::AsyncCommand),
        Command::Trace {
            command: TraceCommand::Validate { path },
        } => {
            let document = read_trace_document(&path)?;
            let summary = validate_trace_json(&document)?;
            serde_json::to_string(&summary).map_err(CliError::Encode)
        }
    }
}

/// Executes one CLI command, including bounded async replay execution commands.
///
/// # Errors
///
/// Returns [`CliError`] when input validation, replay execution, or JSON
/// encoding fails.
pub async fn execute_async(cli: Cli) -> Result<CliOutput, CliError> {
    Box::pin(execute_async_with_cancellation(
        cli,
        &RunCancellation::new(),
    ))
    .await
}

/// Executes one CLI command with a root cancellation capability.
///
/// # Errors
///
/// Returns [`CliError`] after configured-run recovery and partial-evidence
/// finalization when cancellation interrupts a mutable campaign.
pub async fn execute_async_with_cancellation(
    cli: Cli,
    cancellation: &RunCancellation,
) -> Result<CliOutput, CliError> {
    match cli.command {
        Command::Run(args) => {
            let options = ConfiguredCampaignOptions::new(args.seed, args.cases, args.ci)?;
            let output = Box::pin(run_configured_campaign_with_cancellation(
                &args.config,
                &ProcessEnvironment,
                options,
                cancellation,
            ))
            .await?;
            let exit_code = run_verdict_exit_code(output.verdict());
            let body = output.to_pretty_json().map_err(CliError::Encode)?;
            Ok(CliOutput { body, exit_code })
        }
        Command::Replay {
            command: ReplayCommand::Configured(args),
        } => {
            let options = ConfiguredReplayOptions::new(args.case)?;
            let output = Box::pin(run_configured_replay_with_cancellation(
                &args.artifact,
                &args.config,
                &ProcessEnvironment,
                options,
                cancellation,
            ))
            .await?;
            let exit_code = output.classification().exit_code();
            let body = output.to_pretty_json().map_err(CliError::Encode)?;
            Ok(CliOutput { body, exit_code })
        }
        Command::Replay {
            command: ReplayCommand::Minimized(args),
        } => {
            let output = Box::pin(run_configured_minimized_replay_with_cancellation(
                &args.artifact,
                &args.config,
                &ProcessEnvironment,
                cancellation,
            ))
            .await?;
            let exit_code = output.classification().exit_code();
            let body = output.to_pretty_json().map_err(CliError::Encode)?;
            Ok(CliOutput { body, exit_code })
        }
        Command::Shrink {
            command: ShrinkCommand::Configured(args),
        } => {
            let options = ConfiguredShrinkOptions::new(args.max_candidates, args.max_time)?;
            let output = Box::pin(run_configured_shrink_with_cancellation(
                &args.artifact,
                &args.config,
                &ProcessEnvironment,
                options,
                cancellation,
            ))
            .await?;
            let exit_code = output.completion().exit_code();
            let body = output.to_pretty_json().map_err(CliError::Encode)?;
            Ok(CliOutput { body, exit_code })
        }
        command => execute_async_text(Cli { command })
            .await
            .map(CliOutput::success),
    }
}

async fn execute_async_text(cli: Cli) -> Result<String, CliError> {
    match cli.command {
        Command::Doctor(args) => {
            let report = run_doctor(&args.config, &ProcessEnvironment).await?;
            report.to_pretty_json().map_err(CliError::Encode)
        }
        Command::Baseline(args) => {
            run_doctor(&args.config, &ProcessEnvironment).await?;
            let output = run_configured_baseline(
                &args.config,
                &ProcessEnvironment,
                args.acknowledge_reset.as_deref(),
            )
            .await?;
            output.to_pretty_json().map_err(CliError::Encode)
        }
        Command::Run(_)
        | Command::Shrink { .. }
        | Command::Replay {
            command: ReplayCommand::Configured(_) | ReplayCommand::Minimized(_),
        } => Err(CliError::AsyncCommand),
        Command::Replay {
            command: ReplayCommand::ReferenceApp(args),
        } => {
            let plan = replay_plan_from_path(&args.trace)?;
            let case_database = DatabaseName::parse(args.case_database)
                .map_err(|_| CliError::InvalidCaseDatabase)?;
            let webhook_timestamp = args
                .webhook_timestamp
                .map_or_else(current_unix_timestamp, Ok)?;
            let config = ReferenceAppReplayConfig::new(
                case_database,
                args.reference_app_url,
                args.fixture_control_url,
                args.fixture_control_token,
                args.reset_sequence,
                args.confirm_sequence,
                webhook_timestamp,
            )?;
            let receipt = run_reference_app_replay(&plan, &config).await?;
            serde_json::to_string(&receipt).map_err(CliError::Encode)
        }
        Command::Replay {
            command: ReplayCommand::ReferenceAppEvidence(args),
        } => {
            let plan = replay_plan_from_path(&args.trace)?;
            let admin_password = required_env("TIV_POSTGRES_ADMIN_PASSWORD")?;
            let application_password = required_env("TIV_POSTGRES_APPLICATION_PASSWORD")?;
            let fixture_control_token = required_env("TIV_FIXTURE_CONTROL_TOKEN")?;
            let config = ReferenceAppEvidenceConfig::attest(
                args.postgres_port,
                args.postgres_admin_role,
                admin_password,
                application_password,
                args.reference_app_url,
                args.fixture_control_url,
                fixture_control_token,
            )
            .await?;
            let evidence = run_reference_app_evidence(&plan, &config).await?;
            evidence.to_pretty_json().map_err(CliError::Encode)
        }
        Command::Replay {
            command: ReplayCommand::ReferenceAppCase(args),
        } => {
            let document = read_trace_document(&args.plan)?;
            let plan: PlannedCase =
                serde_json::from_str(&document).map_err(CliError::InvalidPlannedCase)?;
            let configured_sql_probe = configured_sql_probe(&plan, args.config.as_ref())?;
            let admin_password = required_env("TIV_POSTGRES_ADMIN_PASSWORD")?;
            let application_password = required_env("TIV_POSTGRES_APPLICATION_PASSWORD")?;
            let fixture_control_token = required_env("TIV_FIXTURE_CONTROL_TOKEN")?;
            let config = ReferenceAppEvidenceConfig::attest(
                args.postgres_port,
                args.postgres_admin_role,
                admin_password,
                application_password,
                args.reference_app_url,
                args.fixture_control_url,
                fixture_control_token,
            )
            .await?;
            let evidence =
                run_reference_app_planned_case(&plan, &config, args.journal, configured_sql_probe)
                    .await?;
            evidence.to_pretty_json().map_err(CliError::Encode)
        }
        read_only => execute(Cli { command: read_only }),
    }
}

const fn run_verdict_exit_code(verdict: ConfiguredCampaignVerdict) -> u8 {
    match verdict {
        ConfiguredCampaignVerdict::Held => 0,
        ConfiguredCampaignVerdict::Violated => 10,
    }
}

/// Validates an untrusted compiled-trace document and returns its summary.
///
/// # Errors
///
/// Returns [`serde_json::Error`] for malformed JSON, an unsupported schema,
/// unresolved outputs, invalid provider IDs, or an invalid action graph.
pub fn validate_trace_json(document: &str) -> Result<TraceSummary, serde_json::Error> {
    let trace: CompiledTrace = serde_json::from_str(document)?;
    Ok(TraceSummary {
        schema_version: trace.schema_version(),
        action_count: trace.action_count(),
    })
}

fn read_trace_document(path: &PathBuf) -> Result<String, CliError> {
    fs::read_to_string(path).map_err(|source| CliError::Read {
        path: path.clone(),
        source,
    })
}

fn replay_plan_from_path(path: &PathBuf) -> Result<ReplayPlan, CliError> {
    let document = read_trace_document(path)?;
    let trace: CompiledTrace = serde_json::from_str(&document)?;
    ReplayPlan::from_trace(&trace).map_err(CliError::ReplayPlan)
}

fn required_env(name: &'static str) -> Result<String, CliError> {
    std::env::var(name).map_err(|_| CliError::MissingEnvironmentVariable(name))
}

fn configured_sql_probe(
    plan: &PlannedCase,
    config_path: Option<&PathBuf>,
) -> Result<Option<tiv_runtime::postgres::probe::ConfiguredSqlProbe>, CliError> {
    let required = plan.actions().iter().any(|action| {
        matches!(
            action.kind(),
            PlanActionKind::KillApplication {
                cut_point: ProcessCutPoint::SqlProbe
            }
        )
    });
    if !required {
        return Ok(None);
    }
    let path = config_path.ok_or(CliError::MissingSqlProbeConfig)?;
    let config = load_resolved_config(path, &ProcessEnvironment)?;
    load_configured_sql_probe(&config)
        .map(Some)
        .map_err(CliError::ConfiguredSqlProbe)
}

fn current_unix_timestamp() -> Result<i64, CliError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CliError::InvalidSystemTime)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| CliError::InvalidSystemTime)
}

fn parse_shrink_duration(value: &str) -> Result<Duration, String> {
    let (digits, multiplier) = if let Some(digits) = value.strip_suffix("ms") {
        (digits, 1_u64)
    } else if let Some(digits) = value.strip_suffix('s') {
        (digits, 1_000)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, 60_000)
    } else {
        return Err("duration must end in ms, s, or m".to_owned());
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("duration must contain an unsigned integer".to_owned());
    }
    let value = digits
        .parse::<u64>()
        .map_err(|_| "duration is outside the supported range".to_owned())?;
    let milliseconds = value
        .checked_mul(multiplier)
        .ok_or_else(|| "duration is outside the supported range".to_owned())?;
    let duration = Duration::from_millis(milliseconds);
    ConfiguredShrinkOptions::new(1, duration)
        .map(|_| duration)
        .map_err(|error| error.to_string())
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("could not read trace {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("compiled trace is invalid: {0}")]
    InvalidTrace(#[from] serde_json::Error),
    #[error("compiled planned case is invalid: {0}")]
    InvalidPlannedCase(serde_json::Error),
    #[error("compiled trace cannot be replayed by this runtime: {0}")]
    ReplayPlan(#[from] ReplayPlanError),
    #[error("this command requires async execution")]
    AsyncCommand,
    #[error("doctor preflight failed: {0}")]
    Doctor(#[from] DoctorError),
    #[error("customer baseline execution failed: {0}")]
    Baseline(#[from] BaselineError),
    #[error("could not determine the current directory: {0}")]
    CurrentDirectory(std::io::Error),
    #[error("project initialization failed: {0}")]
    Init(#[from] InitError),
    #[error("invalid generated case database")]
    InvalidCaseDatabase,
    #[error("required environment variable {0} is missing or invalid")]
    MissingEnvironmentVariable(&'static str),
    #[error("a SQL-probe planned case requires --config")]
    MissingSqlProbeConfig,
    #[error("project configuration is invalid: {0}")]
    Config(#[from] ConfigError),
    #[error("configured SQL probe is invalid: {0}")]
    ConfiguredSqlProbe(#[source] ConfiguredSqlProbeError),
    #[error("configured campaign options are invalid: {0}")]
    ConfiguredCampaignOptions(#[from] ConfiguredCampaignOptionsError),
    #[error("configured campaign failed: {0}")]
    ConfiguredCampaign(#[from] ConfiguredCampaignError),
    #[error("configured replay options are invalid: {0}")]
    ConfiguredReplayOptions(#[from] ConfiguredReplayOptionsError),
    #[error("configured replay failed: {0}")]
    ConfiguredReplay(#[from] ConfiguredReplayError),
    #[error("configured minimized replay failed: {0}")]
    ConfiguredMinimizedReplay(#[from] ConfiguredMinimizedReplayError),
    #[error("configured shrink options are invalid: {0}")]
    ConfiguredShrinkOptions(#[from] ConfiguredShrinkOptionsError),
    #[error("configured shrink failed: {0}")]
    ConfiguredShrink(#[from] ConfiguredShrinkError),
    #[error("run artifact inspection failed: {0}")]
    Artifact(#[from] ArtifactError),
    #[error("run artifact cleanup failed: {0}")]
    Cleanup(#[from] CleanupError),
    #[error("the system clock could not produce a valid webhook timestamp")]
    InvalidSystemTime,
    #[error("reference app replay configuration is invalid: {0}")]
    ReferenceAppReplayConfig(#[from] ReferenceAppReplayConfigError),
    #[error("reference app replay failed: {0}")]
    ReferenceAppReplay(#[from] ReferenceAppReplayError),
    #[error("reference app evidence configuration is invalid: {0}")]
    ReferenceAppEvidenceConfig(#[from] ReferenceAppEvidenceConfigError),
    #[error("reference app evidence run failed: {0}")]
    ReferenceAppEvidence(#[from] ReferenceAppEvidenceError),
    #[error("could not encode command output: {0}")]
    Encode(serde_json::Error),
}

impl CliError {
    /// Maps outer-boundary failures onto the documented non-overlapping CLI
    /// exit contract.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Doctor(error) if error.is_infrastructure_failure() => 3,
            Self::Baseline(error) if error.is_infrastructure_failure() => 3,
            Self::ConfiguredCampaign(error) => error.exit_code(),
            Self::ConfiguredReplay(error) => error.exit_code(),
            Self::ConfiguredMinimizedReplay(error) => error.exit_code(),
            Self::ConfiguredShrink(error) => error.exit_code(),
            Self::Cleanup(error) => error.exit_code(),
            _ => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use tiv_runtime::{
        cleanup::CleanupError,
        configured_campaign::{ConfiguredCampaignError, ConfiguredCampaignVerdict},
    };

    use super::{CliError, run_verdict_exit_code};

    #[test]
    fn configured_run_verdicts_have_non_overlapping_success_and_violation_codes() {
        assert_eq!(run_verdict_exit_code(ConfiguredCampaignVerdict::Held), 0);
        assert_eq!(
            run_verdict_exit_code(ConfiguredCampaignVerdict::Violated),
            10
        );
    }

    #[test]
    fn configured_run_failures_reach_the_runtime_classification_at_the_cli_boundary() {
        let infrastructure = CliError::ConfiguredCampaign(ConfiguredCampaignError::Process(
            Box::new(std::io::Error::other("docker unavailable")),
        ));
        let inconclusive = CliError::ConfiguredCampaign(ConfiguredCampaignError::CaseTimedOut {
            case_id: "case_0001".to_owned(),
        });
        let interrupted = CliError::ConfiguredCampaign(ConfiguredCampaignError::Interrupted {
            case_id: Some("case_0001".to_owned()),
        });

        assert_eq!(infrastructure.exit_code(), 3);
        assert_eq!(inconclusive.exit_code(), 4);
        assert_eq!(interrupted.exit_code(), 130);
    }

    #[test]
    fn cleanup_keeps_safety_refusals_distinct_from_infrastructure_failures() {
        assert_eq!(CliError::Cleanup(CleanupError::InvalidRunId).exit_code(), 2);
        assert_eq!(CliError::Cleanup(CleanupError::ProjectBusy).exit_code(), 3);
    }
}
