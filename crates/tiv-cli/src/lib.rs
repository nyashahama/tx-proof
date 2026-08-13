//! Command-line composition root for `TxProof`.

use std::{fs, path::PathBuf};

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use thiserror::Error;
use tiv_core::trace::CompiledTrace;
use tiv_runtime::{
    postgres::safety::DatabaseName,
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
    /// Inspect replay readiness without executing customer code.
    Replay {
        #[command(subcommand)]
        command: ReplayCommand,
    },
    /// Inspect and validate compiled replay traces.
    Trace {
        #[command(subcommand)]
        command: TraceCommand,
    },
}

#[derive(Debug, PartialEq, Subcommand)]
pub enum ReplayCommand {
    /// Compile a trace into the runtime replay plan without executing it.
    Inspect { path: PathBuf },
    /// Execute the committed reference-app checkout path against loopback services.
    ReferenceApp(ReferenceAppReplayArgs),
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
    #[arg(long, default_value_t = 1_700_000_000)]
    pub webhook_timestamp: i64,
}

#[derive(Debug, PartialEq, Subcommand)]
pub enum TraceCommand {
    /// Validate schema, graph, captured values, and replay bindings.
    Validate { path: PathBuf },
}

/// Executes one read-only CLI command.
///
/// # Errors
///
/// Returns [`CliError`] when the input cannot be read, validated, or encoded.
pub fn execute(cli: Cli) -> Result<String, CliError> {
    match cli.command {
        Command::Replay {
            command: ReplayCommand::Inspect { path },
        } => {
            let plan = replay_plan_from_path(&path)?;
            serde_json::to_string(&plan).map_err(CliError::Encode)
        }
        Command::Replay {
            command: ReplayCommand::ReferenceApp(_),
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
pub async fn execute_async(cli: Cli) -> Result<String, CliError> {
    match cli.command {
        Command::Replay {
            command: ReplayCommand::ReferenceApp(args),
        } => {
            let plan = replay_plan_from_path(&args.trace)?;
            let case_database = DatabaseName::parse(args.case_database)
                .map_err(|_| CliError::InvalidCaseDatabase)?;
            let config = ReferenceAppReplayConfig::new(
                case_database,
                args.reference_app_url,
                args.fixture_control_url,
                args.fixture_control_token,
                args.reset_sequence,
                args.confirm_sequence,
                args.webhook_timestamp,
            )?;
            let receipt = run_reference_app_replay(&plan, &config).await?;
            serde_json::to_string(&receipt).map_err(CliError::Encode)
        }
        read_only => execute(Cli { command: read_only }),
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

#[derive(Debug, Error)]
pub enum CliError {
    #[error("could not read trace {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("compiled trace is invalid: {0}")]
    InvalidTrace(#[from] serde_json::Error),
    #[error("compiled trace cannot be replayed by this runtime: {0}")]
    ReplayPlan(#[from] ReplayPlanError),
    #[error("reference app replay requires async execution")]
    AsyncCommand,
    #[error("invalid generated case database")]
    InvalidCaseDatabase,
    #[error("reference app replay configuration is invalid: {0}")]
    ReferenceAppReplayConfig(#[from] ReferenceAppReplayConfigError),
    #[error("reference app replay failed: {0}")]
    ReferenceAppReplay(#[from] ReferenceAppReplayError),
    #[error("could not encode trace summary: {0}")]
    Encode(serde_json::Error),
}
