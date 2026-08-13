//! Command-line composition root for `TxProof`.

use std::{fs, path::PathBuf};

use clap::{Parser, Subcommand};
use serde::Serialize;
use thiserror::Error;
use tiv_core::trace::CompiledTrace;
use tiv_runtime::replay::{ReplayPlan, ReplayPlanError};

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
            let document = read_trace_document(&path)?;
            let trace: CompiledTrace = serde_json::from_str(&document)?;
            let plan = ReplayPlan::from_trace(&trace)?;
            serde_json::to_string(&plan).map_err(CliError::Encode)
        }
        Command::Trace {
            command: TraceCommand::Validate { path },
        } => {
            let document = read_trace_document(&path)?;
            let summary = validate_trace_json(&document)?;
            serde_json::to_string(&summary).map_err(CliError::Encode)
        }
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
    #[error("could not encode trace summary: {0}")]
    Encode(serde_json::Error),
}
