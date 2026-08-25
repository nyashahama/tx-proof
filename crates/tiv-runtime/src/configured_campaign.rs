//! Configured serial campaign preparation and execution.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tiv_core::{
    decision::Seed,
    plan::{CampaignCompileError, CampaignPlan, CampaignPlanner, CaseCount, PlannedCase},
    result::FailureIdentity,
    shrink::ShrinkCandidate,
    trace::{CompiledCaseTrace, CompiledShrinkTrace},
};

use crate::{
    artifacts::{
        ArtifactAuthority, ArtifactError, ArtifactKind, ArtifactResult, ManifestSeed,
        PartialRunClass, RunArtifactStaging,
    },
    baseline::{BaselineError, ConfiguredBaselineSession, ConfiguredCaseResetReport},
    compatibility::{CompatibilityCaptureError, capture_run_compatibility},
    config::{ConfigError, EnvironmentLookup, ResolvedConfig, load_resolved_config},
    configured_database::ConfiguredDatabaseError,
    configured_process::{
        ConfiguredProcessControl, ConfiguredProcessError, attest_configured_service_images,
    },
    doctor::{DoctorError, collect_compose_facts},
    postgres::{
        probe::{ConfiguredSqlProbe, ConfiguredSqlProbeError, load_configured_sql_probe},
        quiescence::{ConfiguredQuiescence, ConfiguredQuiescenceError, load_configured_quiescence},
        safety::DatabaseName,
        snapshot::{
            ConfiguredSnapshot, ConfiguredSnapshotError, InvariantVerdict, load_configured_snapshot,
        },
    },
    reference_case::{
        CaseSqlProbe, ReferenceCaseRunConfig, ReferenceCaseRunConfigError, ReferenceCaseRunError,
        ReferenceCaseRunReceipt, ReferenceShrinkCaseRunReceipt, preflight_reference_planned_case,
        run_configured_planned_case_with_process, run_configured_shrink_candidate_with_process,
    },
    reports::{
        ArtifactReport, PartialReportClass, ReplayCommand, ReportArtifactKind, ReportCheck,
        ReportCheckOutcome, ReportConclusion, ReportFact, ReportFailure, partial_artifact_report,
        validate_replay_command_paths, write_report_bundle,
    },
    repository::capture_repository_provenance,
    run_supervisor::{
        ComposeProjectLock, SupervisedCaseOutcome, complete_supervision, supervise_execution,
    },
};

pub use crate::run_supervisor::RunCancellation;

const MAX_FIXTURE_STATE_BYTES: usize = 16 * 1024;
const MAX_PERSISTED_WITNESS_BYTES_PER_ATTEMPT: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConfiguredCampaignOptions {
    seed: Option<u64>,
    cases: Option<u32>,
    ci: bool,
}

impl ConfiguredCampaignOptions {
    /// Creates typed command-line overrides without expanding v1 case limits.
    ///
    /// # Errors
    ///
    /// Returns [`ConfiguredCampaignOptionsError`] for an invalid case count.
    pub fn new(
        seed: Option<u64>,
        cases: Option<u32>,
        ci: bool,
    ) -> Result<Self, ConfiguredCampaignOptionsError> {
        if cases.is_some_and(|cases| CaseCount::new(cases).is_err()) {
            return Err(ConfiguredCampaignOptionsError);
        }
        Ok(Self { seed, cases, ci })
    }

    #[must_use]
    pub const fn seed(self) -> Option<u64> {
        self.seed
    }

    #[must_use]
    pub const fn cases(self) -> Option<u32> {
        self.cases
    }

    #[must_use]
    pub const fn ci(self) -> bool {
        self.ci
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("configured campaign case override is outside the v1 limit")]
pub struct ConfiguredCampaignOptionsError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfiguredCampaignVerdict {
    Held,
    Violated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfiguredCampaignFailureClass {
    Configuration,
    Infrastructure,
    Inconclusive,
    Interrupted,
}

impl ConfiguredCampaignFailureClass {
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Configuration => 2,
            Self::Infrastructure => 3,
            Self::Inconclusive => 4,
            Self::Interrupted => 130,
        }
    }
}

impl From<ConfiguredCampaignFailureClass> for PartialReportClass {
    fn from(class: ConfiguredCampaignFailureClass) -> Self {
        match class {
            ConfiguredCampaignFailureClass::Configuration => Self::Configuration,
            ConfiguredCampaignFailureClass::Infrastructure => Self::Infrastructure,
            ConfiguredCampaignFailureClass::Inconclusive => Self::Inconclusive,
            ConfiguredCampaignFailureClass::Interrupted => Self::Interrupted,
        }
    }
}

#[derive(Serialize)]
pub struct ConfiguredCampaignOutput {
    schema_version: u16,
    status: &'static str,
    run_id: String,
    verdict: ConfiguredCampaignVerdict,
    completed_cases: usize,
    artifact_path: PathBuf,
}

/// One fully recovered configured execution attempt and its bounded oracle
/// result. Replay reuses this exact path so campaign and replay cannot drift in
/// reset, process supervision, quiescence, or snapshot semantics.
pub(crate) struct ConfiguredAttemptExecution<T> {
    reset: ConfiguredCaseResetReport,
    trace: T,
    journal_record_count: usize,
    journal_last_record_hash: Option<String>,
    provider_object_count: usize,
    invariants: Vec<ConfiguredInvariantOutcome>,
}

pub(crate) type ConfiguredCaseExecution = ConfiguredAttemptExecution<CompiledCaseTrace>;
pub(crate) type ConfiguredShrinkExecution = ConfiguredAttemptExecution<CompiledShrinkTrace>;

impl<T> ConfiguredAttemptExecution<T> {
    pub(crate) const fn reset(&self) -> &ConfiguredCaseResetReport {
        &self.reset
    }

    pub(crate) const fn trace(&self) -> &T {
        &self.trace
    }

    pub(crate) const fn journal_record_count(&self) -> usize {
        self.journal_record_count
    }

    pub(crate) fn journal_last_record_hash(&self) -> Option<&str> {
        self.journal_last_record_hash.as_deref()
    }

    pub(crate) const fn provider_object_count(&self) -> usize {
        self.provider_object_count
    }

    pub(crate) fn invariants(&self) -> &[ConfiguredInvariantOutcome] {
        &self.invariants
    }
}

pub(crate) struct ConfiguredInvariantOutcome {
    identity: FailureIdentity,
    witnesses: Vec<Map<String, Value>>,
}

impl ConfiguredInvariantOutcome {
    pub(crate) const fn identity(&self) -> &FailureIdentity {
        &self.identity
    }

    pub(crate) fn witness_count(&self) -> usize {
        self.witnesses.len()
    }

    pub(crate) fn violated(&self) -> bool {
        !self.witnesses.is_empty()
    }

    pub(crate) fn witnesses(&self) -> &[Map<String, Value>] {
        &self.witnesses
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WitnessProjectionPolicy {
    DigestOnly,
    ReferenceLedgerAllowlist,
}

#[derive(Serialize)]
struct InvariantWitnessArtifact {
    invariant_id: String,
    checkpoint_id: String,
    witness_count: usize,
    witness_digest: String,
    projection: WitnessProjectionPolicy,
    retained_row_count: usize,
    omitted_row_count: usize,
    rows_truncated: bool,
    rows: Vec<Map<String, Value>>,
}

#[derive(Serialize)]
struct InvariantWitnessBundle {
    schema_version: u16,
    invariants: Vec<InvariantWitnessArtifact>,
}

pub(crate) fn witness_projection_policy(config: &ResolvedConfig) -> WitnessProjectionPolicy {
    if config.compose_project() == "tiv-reference-app-spike"
        && config.case_database() == "tiv_case_deadbeef"
        && config.baseline_database() == "tiv_base_deadbeef"
        && config.application_service() == "reference-app"
        && config.postgres_service() == "postgres"
    {
        WitnessProjectionPolicy::ReferenceLedgerAllowlist
    } else {
        WitnessProjectionPolicy::DigestOnly
    }
}

pub(crate) fn write_invariant_witness_artifacts(
    artifacts: &mut RunArtifactStaging,
    relative_directory: impl AsRef<Path>,
    outcomes: &[ConfiguredInvariantOutcome],
    policy: WitnessProjectionPolicy,
) -> Result<(), ArtifactError> {
    let relative_directory = relative_directory.as_ref();
    let mut retained_bytes = 0_usize;
    let mut invariants = Vec::new();
    for outcome in outcomes.iter().filter(|outcome| outcome.violated()) {
        let rows = outcome.witnesses();
        let encoded = serde_json::to_vec(rows).map_err(ArtifactError::Serialize)?;
        let reference_rows_allowed = policy == WitnessProjectionPolicy::ReferenceLedgerAllowlist
            && outcome.identity().invariant().as_str() == "balanced-ledger"
            && rows.iter().all(reference_ledger_witness_row);
        let projection = if reference_rows_allowed {
            WitnessProjectionPolicy::ReferenceLedgerAllowlist
        } else {
            WitnessProjectionPolicy::DigestOnly
        };
        let mut retained_rows = Vec::new();
        if reference_rows_allowed {
            for row in rows {
                let row_bytes = serde_json::to_vec(row)
                    .map_err(ArtifactError::Serialize)?
                    .len();
                if retained_bytes.saturating_add(row_bytes)
                    > MAX_PERSISTED_WITNESS_BYTES_PER_ATTEMPT
                {
                    break;
                }
                retained_bytes = retained_bytes.saturating_add(row_bytes);
                retained_rows.push(row.clone());
            }
        }
        let retained_row_count = retained_rows.len();
        invariants.push(InvariantWitnessArtifact {
            invariant_id: outcome.identity().invariant().as_str().to_owned(),
            checkpoint_id: outcome.identity().checkpoint().as_str().to_owned(),
            witness_count: rows.len(),
            witness_digest: blake3::hash(&encoded).to_hex().to_string(),
            projection,
            retained_row_count,
            omitted_row_count: rows.len().saturating_sub(retained_row_count),
            rows_truncated: retained_row_count != rows.len(),
            rows: retained_rows,
        });
    }
    if !invariants.is_empty() {
        artifacts.write_json(
            relative_directory.join("witnesses.json"),
            &InvariantWitnessBundle {
                schema_version: 1,
                invariants,
            },
        )?;
    }
    Ok(())
}

fn reference_ledger_witness_row(row: &Map<String, Value>) -> bool {
    const EXPECTED_COLUMNS: [&str; 10] = [
        "credit_posting_count",
        "credit_total_minor",
        "currency",
        "debit_posting_count",
        "debit_total_minor",
        "entry_id",
        "imbalance_minor",
        "operation_id",
        "posting_count",
        "provider_event_id",
    ];
    let columns = row.keys().map(String::as_str).collect::<BTreeSet<_>>();
    if columns != BTreeSet::from(EXPECTED_COLUMNS) {
        return false;
    }
    let Some(provider_event_id) = row["provider_event_id"].as_str() else {
        return false;
    };
    let Some(operation_id) = row["operation_id"].as_str() else {
        return false;
    };
    let Some(entry_id) = row["entry_id"].as_str() else {
        return false;
    };
    let Some(currency) = row["currency"].as_str() else {
        return false;
    };
    let Some(posting_count) = row["posting_count"].as_i64() else {
        return false;
    };
    let Some(debit_count) = row["debit_posting_count"].as_i64() else {
        return false;
    };
    let Some(credit_count) = row["credit_posting_count"].as_i64() else {
        return false;
    };
    let Some(debit_total) = row["debit_total_minor"].as_i64() else {
        return false;
    };
    let Some(credit_total) = row["credit_total_minor"].as_i64() else {
        return false;
    };
    let Some(imbalance) = row["imbalance_minor"].as_i64() else {
        return false;
    };
    valid_reference_identifier(provider_event_id, "evt_tiv_")
        && valid_reference_identifier(operation_id, "op_")
        && uuid::Uuid::parse_str(entry_id).is_ok()
        && currency.len() == 3
        && currency.bytes().all(|byte| byte.is_ascii_lowercase())
        && (0..=2).contains(&posting_count)
        && (0..=1).contains(&debit_count)
        && (0..=1).contains(&credit_count)
        && debit_count + credit_count == posting_count
        && debit_total >= 0
        && credit_total >= 0
        && debit_total.checked_sub(credit_total) == Some(imbalance)
}

fn valid_reference_identifier(value: &str, prefix: &str) -> bool {
    let Some(suffix) = value.strip_prefix(prefix) else {
        return false;
    };
    !suffix.is_empty()
        && value.len() <= 255
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

impl ConfiguredCampaignOutput {
    #[must_use]
    pub const fn verdict(&self) -> ConfiguredCampaignVerdict {
        self.verdict
    }

    #[must_use]
    pub fn artifact_path(&self) -> &Path {
        &self.artifact_path
    }

    /// Serializes the secret-free command result.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if the allowlisted report cannot encode.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Executes one fully prepared configured campaign against the attested local
/// disposable stack.
///
/// All plans and repository-owned SQL contracts are loaded and preflighted
/// before the first database reset. A completed artifact directory is exposed
/// only after its manifest is written and the staging directory is atomically
/// renamed.
///
/// # Errors
///
/// Returns [`ConfiguredCampaignError`] for configuration, preparation, safety,
/// infrastructure, case execution, oracle, or artifact-finalization failures.
pub async fn run_configured_campaign(
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    options: ConfiguredCampaignOptions,
) -> Result<ConfiguredCampaignOutput, ConfiguredCampaignError> {
    Box::pin(run_configured_campaign_with_cancellation(
        config_path,
        environment,
        options,
        &RunCancellation::new(),
    ))
    .await
}

/// Executes a configured campaign that can be cancelled by the command root.
///
/// # Errors
///
/// Returns [`ConfiguredCampaignError`] after recovery and partial-evidence
/// finalization when execution has already entered the mutable run boundary.
#[allow(clippy::too_many_lines)]
pub async fn run_configured_campaign_with_cancellation(
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    options: ConfiguredCampaignOptions,
    cancellation: &RunCancellation,
) -> Result<ConfiguredCampaignOutput, ConfiguredCampaignError> {
    let config = load_resolved_config(config_path, environment)?;
    let case_override = options
        .cases
        .map(CaseCount::new)
        .transpose()
        .map_err(|_| ConfiguredCampaignError::Options(ConfiguredCampaignOptionsError))?;
    let effective_spec = config
        .campaign_spec()
        .with_run_overrides(options.seed.map(Seed::new), case_override);
    let campaign = CampaignPlanner::compile(&effective_spec)
        .map_err(ConfiguredCampaignError::CampaignCompile)?;
    let configured_probe = load_configured_sql_probe(&config)?;
    let configured_quiescence = load_configured_quiescence(&config)?;
    let configured_snapshot = load_configured_snapshot(&config)?;
    for case in campaign.cases() {
        preflight_reference_planned_case(case.plan(), true, true)?;
    }
    let run_id = format!("run_{}", uuid::Uuid::new_v4().simple());
    validate_replay_command_paths(&config.artifact_dir().join(&run_id), config.source_path())
        .map_err(ConfiguredCampaignError::ReportPath)?;
    if cancellation.is_cancelled() {
        return Err(ConfiguredCampaignError::Interrupted { case_id: None });
    }
    let _project_lock = ComposeProjectLock::try_acquire(config.root(), config.compose_project())
        .map_err(|error| ConfiguredCampaignError::ProjectLock(Box::new(error)))?;
    let repository = capture_repository_provenance(config.root())
        .await
        .map_err(|error| ConfiguredCampaignError::Repository(Box::new(error)))?;

    let mut artifacts = RunArtifactStaging::create_v2(
        config.root(),
        config.artifact_dir(),
        &run_id,
        ManifestSeed::new(ArtifactKind::Campaign, repository, Vec::new()),
    )?;
    artifacts.write_json("config.redacted.json", config.redacted())?;
    artifacts.write_json("campaign-plan.json", &campaign)?;
    let mut case_results = Vec::with_capacity(campaign.cases().len());

    let execution = Box::pin(execute_staged_campaign(
        &config,
        &campaign,
        &configured_probe,
        &configured_quiescence,
        &configured_snapshot,
        &run_id,
        &mut artifacts,
        &mut case_results,
        cancellation,
    ))
    .await;
    let campaign_verdict = match execution {
        Ok(verdict) => verdict,
        Err(cause) => {
            let failure_class = cause.failure_class();
            let failure_code = cause.failure_code();
            let summary = PartialCampaignSummary {
                schema_version: 1,
                status: partial_status(failure_class),
                run_id: &run_id,
                failure_class,
                failure_code,
                configured_cases: campaign.cases().len(),
                completed_cases: case_results.len(),
                cases: &case_results,
            };
            if let Err(artifact_error) = artifacts.write_json("summary.json", &summary) {
                return Err(ConfiguredCampaignError::PartialFinalization {
                    cause: Box::new(cause),
                    artifact: Box::new(artifact_error),
                });
            }
            let mut report = partial_artifact_report(
                &run_id,
                ReportArtifactKind::Campaign,
                failure_class.into(),
                failure_code,
            );
            report.add_fact(ReportFact::new(
                "Configured cases",
                campaign.cases().len().to_string(),
            ));
            report.add_fact(ReportFact::new(
                "Completed cases",
                case_results.len().to_string(),
            ));
            if let Err(artifact_error) = write_report_bundle(&mut artifacts, &report) {
                return Err(ConfiguredCampaignError::PartialFinalization {
                    cause: Box::new(cause),
                    artifact: Box::new(artifact_error),
                });
            }
            let authorities = match campaign_authorities(&case_results) {
                Ok(authorities) => authorities,
                Err(artifact_error) => {
                    return Err(ConfiguredCampaignError::PartialFinalization {
                        cause: Box::new(cause),
                        artifact: Box::new(artifact_error),
                    });
                }
            };
            let artifact_path = match artifacts.finalize_partial_v2(
                partial_artifact_class(failure_class),
                failure_code,
                authorities,
            ) {
                Ok(path) => path,
                Err(artifact_error) => {
                    return Err(ConfiguredCampaignError::PartialFinalization {
                        cause: Box::new(cause),
                        artifact: Box::new(artifact_error),
                    });
                }
            };
            return Err(ConfiguredCampaignError::RunFailed {
                cause: Box::new(cause),
                artifact_path,
            });
        }
    };

    let completed_cases = case_results.len();
    let summary = CampaignSummary {
        schema_version: 1,
        run_id: run_id.clone(),
        verdict: campaign_verdict,
        configured_cases: campaign.cases().len(),
        completed_cases,
        cases: case_results,
    };
    artifacts.write_json("summary.json", &summary)?;
    let report = completed_campaign_report(
        &summary,
        artifacts.planned_final_path(),
        config.source_path(),
    )?;
    write_report_bundle(&mut artifacts, &report)?;
    let authorities = campaign_authorities(&summary.cases)?;
    let result = match campaign_verdict {
        ConfiguredCampaignVerdict::Held => ArtifactResult::Held,
        ConfiguredCampaignVerdict::Violated => ArtifactResult::Counterexample,
    };
    let artifact_path = artifacts.finalize_complete(result, authorities)?;
    Ok(ConfiguredCampaignOutput {
        schema_version: 1,
        status: "campaign_complete",
        run_id,
        verdict: campaign_verdict,
        completed_cases,
        artifact_path,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_configured_case_attempt(
    config: &ResolvedConfig,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    baseline: ConfiguredBaselineSession,
    planned_case: &PlannedCase,
    run_id: &str,
    case_id: &str,
    journal_path: PathBuf,
    cancellation: &RunCancellation,
) -> Result<(ConfiguredBaselineSession, ConfiguredCaseExecution), ConfiguredCampaignError> {
    Box::pin(execute_configured_case_attempt_with_timeout(
        config,
        configured_probe,
        configured_quiescence,
        configured_snapshot,
        baseline,
        planned_case,
        run_id,
        case_id,
        journal_path,
        config.case_timeout(),
        cancellation,
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_configured_case_attempt_with_timeout(
    config: &ResolvedConfig,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    baseline: ConfiguredBaselineSession,
    planned_case: &PlannedCase,
    run_id: &str,
    case_id: &str,
    journal_path: PathBuf,
    case_timeout: Duration,
    cancellation: &RunCancellation,
) -> Result<(ConfiguredBaselineSession, ConfiguredCaseExecution), ConfiguredCampaignError> {
    let (baseline, execution) = Box::pin(execute_configured_attempt(
        config,
        configured_probe,
        configured_quiescence,
        configured_snapshot,
        baseline,
        ConfiguredAttemptAuthority::Planned(planned_case),
        run_id,
        case_id,
        journal_path,
        case_timeout,
        cancellation,
    ))
    .await?;
    Ok((baseline, execution.into_planned()?))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_configured_shrink_attempt_with_timeout(
    config: &ResolvedConfig,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    baseline: ConfiguredBaselineSession,
    candidate: &ShrinkCandidate,
    run_id: &str,
    case_id: &str,
    journal_path: PathBuf,
    case_timeout: Duration,
    cancellation: &RunCancellation,
) -> Result<(ConfiguredBaselineSession, ConfiguredShrinkExecution), ConfiguredCampaignError> {
    let (baseline, execution) = Box::pin(execute_configured_attempt(
        config,
        configured_probe,
        configured_quiescence,
        configured_snapshot,
        baseline,
        ConfiguredAttemptAuthority::Shrink(candidate),
        run_id,
        case_id,
        journal_path,
        case_timeout,
        cancellation,
    ))
    .await?;
    Ok((baseline, execution.into_shrink()?))
}

enum ConfiguredAttemptAuthority<'a> {
    Planned(&'a PlannedCase),
    Shrink(&'a ShrinkCandidate),
}

enum ConfiguredAttemptReceipt {
    Planned(ReferenceCaseRunReceipt),
    Shrink(ReferenceShrinkCaseRunReceipt),
}

enum ConfiguredAttemptTrace {
    Planned(CompiledCaseTrace),
    Shrink(CompiledShrinkTrace),
}

struct ConfiguredAttemptParts {
    reset: ConfiguredCaseResetReport,
    trace: ConfiguredAttemptTrace,
    journal_record_count: usize,
    journal_last_record_hash: Option<String>,
    provider_object_count: usize,
    invariants: Vec<ConfiguredInvariantOutcome>,
}

impl ConfiguredAttemptParts {
    fn into_planned(self) -> Result<ConfiguredCaseExecution, ConfiguredCampaignError> {
        let ConfiguredAttemptTrace::Planned(trace) = self.trace else {
            return Err(ConfiguredCampaignError::AttemptTraceMismatch);
        };
        Ok(ConfiguredAttemptExecution {
            reset: self.reset,
            trace,
            journal_record_count: self.journal_record_count,
            journal_last_record_hash: self.journal_last_record_hash,
            provider_object_count: self.provider_object_count,
            invariants: self.invariants,
        })
    }

    fn into_shrink(self) -> Result<ConfiguredShrinkExecution, ConfiguredCampaignError> {
        let ConfiguredAttemptTrace::Shrink(trace) = self.trace else {
            return Err(ConfiguredCampaignError::AttemptTraceMismatch);
        };
        Ok(ConfiguredAttemptExecution {
            reset: self.reset,
            trace,
            journal_record_count: self.journal_record_count,
            journal_last_record_hash: self.journal_last_record_hash,
            provider_object_count: self.provider_object_count,
            invariants: self.invariants,
        })
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn execute_configured_attempt(
    config: &ResolvedConfig,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    baseline: ConfiguredBaselineSession,
    authority: ConfiguredAttemptAuthority<'_>,
    run_id: &str,
    case_id: &str,
    journal_path: PathBuf,
    case_timeout: Duration,
    cancellation: &RunCancellation,
) -> Result<(ConfiguredBaselineSession, ConfiguredAttemptParts), ConfiguredCampaignError> {
    if cancellation.is_cancelled() {
        return Err(ConfiguredCampaignError::Interrupted {
            case_id: Some(case_id.to_owned()),
        });
    }
    let (baseline, reset) = baseline.reset_case().await?;
    let database = baseline.attest_case_database().await?;
    let mut process = ConfiguredProcessControl::attest(config).await?;
    let reset_sequence = fixture_control_sequence(config)
        .await?
        .checked_add(1)
        .ok_or(ConfiguredCampaignError::FixtureSequenceExhausted)?;
    let case_database_name = DatabaseName::parse(config.case_database())
        .map_err(|_| ConfiguredCampaignError::CaseDatabaseName)?;
    let provider_proxy_url = driver_origin(config.driver_url())?;
    let run_config = ReferenceCaseRunConfig::from_http_contract(
        &case_database_name,
        config.driver_url().as_str(),
        config.driver_body().clone(),
        provider_proxy_url.as_str(),
        config.fixture_control_url().as_str(),
        config.fixture_control_token().to_owned(),
        reset_sequence,
        current_unix_timestamp()?,
        config.driver_timeout(),
        config.fixture_poll_interval(),
    )?;
    let mut sql_probe = database
        .open_sql_probe(
            configured_probe.clone(),
            config.case_timeout(),
            config.fixture_poll_interval(),
        )
        .await?;
    let mut quiescence = database
        .open_quiescence(
            configured_quiescence.clone(),
            config.fixture_poll_interval(),
        )
        .await?;
    let execution = async {
        match authority {
            ConfiguredAttemptAuthority::Planned(planned_case) => {
                run_configured_planned_case_with_process(
                    run_id.to_owned(),
                    case_id.to_owned(),
                    planned_case,
                    journal_path,
                    run_config,
                    &mut process,
                    Some(&mut sql_probe as &mut dyn CaseSqlProbe),
                    &mut quiescence,
                )
                .await
                .map(ConfiguredAttemptReceipt::Planned)
            }
            ConfiguredAttemptAuthority::Shrink(candidate) => {
                run_configured_shrink_candidate_with_process(
                    run_id.to_owned(),
                    case_id.to_owned(),
                    candidate,
                    journal_path,
                    run_config,
                    &mut process,
                    Some(&mut sql_probe as &mut dyn CaseSqlProbe),
                    &mut quiescence,
                )
                .await
                .map(ConfiguredAttemptReceipt::Shrink)
            }
        }
    };
    let outcome = supervise_execution(case_timeout, cancellation, execution).await;
    let supervised = complete_supervision(&mut process, outcome).await;
    let probe_close = sql_probe.close().await;
    let quiescence_close = quiescence.close().await;
    let (outcome, process_recovery) = supervised.into_parts();
    let primary = match outcome {
        SupervisedCaseOutcome::Completed(result) => {
            result.map_err(ConfiguredCampaignError::CaseRun)
        }
        SupervisedCaseOutcome::TimedOut => Err(ConfiguredCampaignError::CaseTimedOut {
            case_id: case_id.to_owned(),
        }),
        SupervisedCaseOutcome::Cancelled => Err(ConfiguredCampaignError::Interrupted {
            case_id: Some(case_id.to_owned()),
        }),
    };
    let cleanup = configured_case_cleanup_error(process_recovery, probe_close, quiescence_close);
    let receipt = match (primary, cleanup) {
        (Ok(receipt), None) => receipt,
        (Err(cause), None) => return Err(cause),
        (Ok(_), Some(recovery)) => {
            return Err(ConfiguredCampaignError::RecoveryFailed {
                cause: None,
                recovery,
            });
        }
        (Err(cause), Some(recovery)) => {
            return Err(ConfiguredCampaignError::RecoveryFailed {
                cause: Some(Box::new(cause)),
                recovery,
            });
        }
    };
    if cancellation.is_cancelled() {
        return Err(ConfiguredCampaignError::Interrupted {
            case_id: Some(case_id.to_owned()),
        });
    }
    let (trace, journal_record_count, journal_last_record_hash, checkpoint) = match receipt {
        ConfiguredAttemptReceipt::Planned(receipt) => {
            let (executed, checkpoint) = receipt.into_parts();
            (
                ConfiguredAttemptTrace::Planned(executed.trace().clone()),
                executed.journal_summary().record_count(),
                executed
                    .journal_summary()
                    .last_record_hash()
                    .map(str::to_owned),
                checkpoint,
            )
        }
        ConfiguredAttemptReceipt::Shrink(receipt) => {
            let (executed, checkpoint) = receipt.into_parts();
            (
                ConfiguredAttemptTrace::Shrink(executed.trace().clone()),
                executed.journal_summary().record_count(),
                executed
                    .journal_summary()
                    .last_record_hash()
                    .map(str::to_owned),
                checkpoint,
            )
        }
    };
    let (provider_objects, quiescence_permit) = checkpoint.into_oracle_parts();
    let provider_object_count = provider_objects.len();
    let snapshot = database
        .open_snapshot(configured_snapshot.clone())
        .await?
        .run(&provider_objects, quiescence_permit)
        .await?;
    let invariants = snapshot
        .outcomes()
        .iter()
        .map(|outcome| ConfiguredInvariantOutcome {
            identity: outcome.identity().clone(),
            witnesses: match outcome.verdict() {
                InvariantVerdict::Held => Vec::new(),
                InvariantVerdict::Violated(witnesses) => witnesses
                    .iter()
                    .map(|witness| witness.columns().clone())
                    .collect(),
            },
        })
        .collect();
    Ok((
        baseline,
        ConfiguredAttemptParts {
            reset,
            trace,
            journal_record_count,
            journal_last_record_hash,
            provider_object_count,
            invariants,
        },
    ))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn execute_staged_campaign(
    config: &ResolvedConfig,
    campaign: &CampaignPlan,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    run_id: &str,
    artifacts: &mut RunArtifactStaging,
    case_results: &mut Vec<CaseArtifact>,
    cancellation: &RunCancellation,
) -> Result<ConfiguredCampaignVerdict, ConfiguredCampaignError> {
    if cancellation.is_cancelled() {
        return Err(ConfiguredCampaignError::Interrupted { case_id: None });
    }

    let compose = collect_compose_facts(config).await?;
    let mut baseline = ConfiguredBaselineSession::attest(config).await?;
    ConfiguredProcessControl::attest(config).await?;
    let service_images = attest_configured_service_images(config, &compose).await?;
    let compatibility = capture_run_compatibility(
        config,
        &compose,
        baseline.baseline_identity(),
        &service_images,
        configured_probe,
        configured_quiescence,
        configured_snapshot,
    )?;
    artifacts.write_json("compatibility.json", &compatibility)?;
    let mut campaign_verdict = ConfiguredCampaignVerdict::Held;

    for case in campaign.cases() {
        let case_id = format!("case_{:04}", case.id().value());
        if cancellation.is_cancelled() {
            return Err(ConfiguredCampaignError::Interrupted {
                case_id: Some(case_id),
            });
        }
        let journal_path =
            artifacts.prepare_path(format!("cases/{case_id}/observations.ndjson"))?;
        let (fresh_baseline, execution) = Box::pin(execute_configured_case_attempt(
            config,
            configured_probe,
            configured_quiescence,
            configured_snapshot,
            baseline,
            case.plan(),
            run_id,
            &case_id,
            journal_path,
            cancellation,
        ))
        .await?;
        baseline = fresh_baseline;
        artifacts.write_json(format!("cases/{case_id}/trace.json"), execution.trace())?;
        write_invariant_witness_artifacts(
            artifacts,
            format!("cases/{case_id}/invariants"),
            execution.invariants(),
            witness_projection_policy(config),
        )?;
        let invariants = execution
            .invariants()
            .iter()
            .map(|outcome| {
                let verdict = if outcome.violated() {
                    campaign_verdict = ConfiguredCampaignVerdict::Violated;
                    ConfiguredCampaignVerdict::Violated
                } else {
                    ConfiguredCampaignVerdict::Held
                };
                InvariantArtifact {
                    invariant_id: outcome.identity().invariant().as_str().to_owned(),
                    checkpoint_id: outcome.identity().checkpoint().as_str().to_owned(),
                    verdict,
                    witness_count: outcome.witness_count(),
                }
            })
            .collect::<Vec<_>>();
        let result = CaseArtifact {
            schema_version: 1,
            case_id: case_id.clone(),
            seed: case.plan().seed().value(),
            planned_action_count: case.plan().actions().len(),
            executed_action_count: execution.trace().action_count(),
            journal_record_count: execution.journal_record_count(),
            journal_last_record_hash: execution.journal_last_record_hash().map(str::to_owned),
            before_database_oid: execution.reset().before_database_oid(),
            after_database_oid: execution.reset().after_database_oid(),
            before_marker_uuid: execution.reset().before_marker_uuid().to_owned(),
            after_marker_uuid: execution.reset().after_marker_uuid().to_owned(),
            provider_object_count: execution.provider_object_count(),
            invariants,
        };
        artifacts.write_json(format!("cases/{case_id}/result.json"), &result)?;
        case_results.push(result);
    }

    Ok(campaign_verdict)
}

fn driver_origin(driver_url: &reqwest::Url) -> Result<reqwest::Url, ConfiguredCampaignError> {
    let mut origin = driver_url.clone();
    origin.set_path("/");
    origin.set_query(None);
    origin.set_fragment(None);
    if origin.username().is_empty() && origin.password().is_none() {
        Ok(origin)
    } else {
        Err(ConfiguredCampaignError::DriverOrigin)
    }
}

fn current_unix_timestamp() -> Result<i64, ConfiguredCampaignError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ConfiguredCampaignError::Clock)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| ConfiguredCampaignError::Clock)
}

async fn fixture_control_sequence(
    config: &crate::config::ResolvedConfig,
) -> Result<u64, ConfiguredCampaignError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(config.driver_timeout())
        .timeout(config.driver_timeout())
        .build()
        .map_err(ConfiguredCampaignError::FixtureRequest)?;
    let response = client
        .get(
            config
                .fixture_control_url()
                .join("v1/control/state")
                .map_err(|_| ConfiguredCampaignError::FixtureState)?,
        )
        .header("X-Tiv-Control-Token", config.fixture_control_token())
        .send()
        .await
        .map_err(ConfiguredCampaignError::FixtureRequest)?;
    if response.status() != StatusCode::OK {
        return Err(ConfiguredCampaignError::FixtureState);
    }
    let body = response
        .bytes()
        .await
        .map_err(ConfiguredCampaignError::FixtureRequest)?;
    if body.len() > MAX_FIXTURE_STATE_BYTES {
        return Err(ConfiguredCampaignError::FixtureState);
    }
    let state: FixtureState =
        serde_json::from_slice(&body).map_err(|_| ConfiguredCampaignError::FixtureState)?;
    if !state.held_gates.is_empty() {
        return Err(ConfiguredCampaignError::FixtureState);
    }
    Ok(state.command_sequence)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureState {
    command_sequence: u64,
    #[serde(rename = "remaining_outcomes")]
    _remaining_outcomes: usize,
    held_gates: Vec<serde_json::Value>,
    #[serde(rename = "payment_intents")]
    _payment_intents: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct CampaignSummary {
    schema_version: u16,
    run_id: String,
    verdict: ConfiguredCampaignVerdict,
    configured_cases: usize,
    completed_cases: usize,
    cases: Vec<CaseArtifact>,
}

#[derive(Serialize)]
struct PartialCampaignSummary<'a> {
    schema_version: u16,
    status: &'static str,
    run_id: &'a str,
    failure_class: ConfiguredCampaignFailureClass,
    failure_code: &'static str,
    configured_cases: usize,
    completed_cases: usize,
    cases: &'a [CaseArtifact],
}

fn campaign_authorities(cases: &[CaseArtifact]) -> Result<Vec<ArtifactAuthority>, ArtifactError> {
    let mut authorities = Vec::with_capacity(cases.len() + 1);
    authorities.push(ArtifactAuthority::campaign_plan());
    for case in cases {
        authorities.push(ArtifactAuthority::campaign_case_trace(&case.case_id)?);
    }
    Ok(authorities)
}

fn completed_campaign_report(
    summary: &CampaignSummary,
    artifact_path: &Path,
    config_path: &Path,
) -> Result<ArtifactReport, ArtifactError> {
    let conclusion = match summary.verdict {
        ConfiguredCampaignVerdict::Held => ReportConclusion::Held,
        ConfiguredCampaignVerdict::Violated => ReportConclusion::Counterexample,
    };
    let replay_stability = match summary.verdict {
        ConfiguredCampaignVerdict::Held => {
            "Not applicable: no violating case was observed.".to_owned()
        }
        ConfiguredCampaignVerdict::Violated => {
            "Not yet classified; use the emitted three-attempt replay command for each violating case."
                .to_owned()
        }
    };
    let mut report = ArtifactReport::new(
        &summary.run_id,
        ReportArtifactKind::Campaign,
        conclusion,
        format!(
            "{} configured serial cases; {} completed.",
            summary.configured_cases, summary.completed_cases
        ),
        replay_stability,
    );
    report.add_fact(ReportFact::new(
        "Configured cases",
        summary.configured_cases.to_string(),
    ));
    report.add_fact(ReportFact::new(
        "Completed cases",
        summary.completed_cases.to_string(),
    ));

    let mut violating_cases = 0_usize;
    for case in &summary.cases {
        let mut case_violated = false;
        for invariant in &case.invariants {
            let name = format!(
                "{} / {} at {}",
                case.case_id, invariant.invariant_id, invariant.checkpoint_id
            );
            match invariant.verdict {
                ConfiguredCampaignVerdict::Held => report.add_check(ReportCheck::new(
                    name,
                    ReportCheckOutcome::Passed,
                    "invariant held for this bounded case",
                )),
                ConfiguredCampaignVerdict::Violated => {
                    case_violated = true;
                    report.add_check(ReportCheck::new(
                        name,
                        ReportCheckOutcome::Failed,
                        "counterexample observed; fresh-baseline replay classification is required",
                    ));
                    report.add_failure(ReportFailure::new(
                        &invariant.invariant_id,
                        &invariant.checkpoint_id,
                        Some(invariant.witness_count),
                    ));
                }
            }
        }
        if case_violated {
            violating_cases += 1;
            let case_number = case
                .case_id
                .strip_prefix("case_")
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or(ArtifactError::InvalidReport)?;
            report.add_replay_command(ReplayCommand::configured(
                artifact_path,
                config_path,
                case_number,
            ));
        }
    }
    report.add_fact(ReportFact::new(
        "Violating cases",
        violating_cases.to_string(),
    ));
    Ok(report)
}

const fn partial_status(class: ConfiguredCampaignFailureClass) -> &'static str {
    match class {
        ConfiguredCampaignFailureClass::Configuration
        | ConfiguredCampaignFailureClass::Infrastructure => "failed",
        ConfiguredCampaignFailureClass::Inconclusive => "inconclusive",
        ConfiguredCampaignFailureClass::Interrupted => "interrupted",
    }
}

const fn partial_artifact_class(class: ConfiguredCampaignFailureClass) -> PartialRunClass {
    match class {
        ConfiguredCampaignFailureClass::Configuration => PartialRunClass::Configuration,
        ConfiguredCampaignFailureClass::Infrastructure => PartialRunClass::Infrastructure,
        ConfiguredCampaignFailureClass::Inconclusive => PartialRunClass::Inconclusive,
        ConfiguredCampaignFailureClass::Interrupted => PartialRunClass::Interrupted,
    }
}

fn configured_case_cleanup_error(
    process: Result<(), ConfiguredProcessError>,
    probe: Result<(), BaselineError>,
    quiescence: Result<(), BaselineError>,
) -> Option<Box<dyn std::error::Error + Send + Sync>> {
    if process.is_ok() && probe.is_ok() && quiescence.is_ok() {
        None
    } else {
        Some(Box::new(ConfiguredCaseCleanupError {
            process: process.err(),
            probe: probe.err(),
            quiescence: quiescence.err(),
        }))
    }
}

#[derive(Debug)]
struct ConfiguredCaseCleanupError {
    process: Option<ConfiguredProcessError>,
    probe: Option<BaselineError>,
    quiescence: Option<BaselineError>,
}

impl std::fmt::Display for ConfiguredCaseCleanupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "process recovery: {}; SQL-probe close: {}; quiescence close: {}",
            self.process
                .as_ref()
                .map_or_else(|| "ok".to_owned(), ToString::to_string),
            self.probe
                .as_ref()
                .map_or_else(|| "ok".to_owned(), ToString::to_string),
            self.quiescence
                .as_ref()
                .map_or_else(|| "ok".to_owned(), ToString::to_string),
        )
    }
}

impl std::error::Error for ConfiguredCaseCleanupError {}

#[derive(Serialize)]
struct CaseArtifact {
    schema_version: u16,
    case_id: String,
    seed: u64,
    planned_action_count: usize,
    executed_action_count: usize,
    journal_record_count: usize,
    journal_last_record_hash: Option<String>,
    before_database_oid: u32,
    after_database_oid: u32,
    before_marker_uuid: String,
    after_marker_uuid: String,
    provider_object_count: usize,
    invariants: Vec<InvariantArtifact>,
}

#[derive(Serialize)]
struct InvariantArtifact {
    invariant_id: String,
    checkpoint_id: String,
    verdict: ConfiguredCampaignVerdict,
    witness_count: usize,
}

#[derive(Debug, Error)]
pub enum ConfiguredCampaignError {
    #[error("configured campaign options are invalid: {0}")]
    Options(#[from] ConfiguredCampaignOptionsError),
    #[error("configured campaign configuration failed: {0}")]
    Config(#[from] ConfigError),
    #[error("configured campaign repository provenance failed: {0}")]
    Repository(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured campaign could not be compiled")]
    CampaignCompile(CampaignCompileError),
    #[error("configured SQL probe could not be prepared: {0}")]
    SqlProbe(#[from] ConfiguredSqlProbeError),
    #[error("configured quiescence could not be prepared: {0}")]
    Quiescence(#[from] ConfiguredQuiescenceError),
    #[error("configured invariant suite could not be prepared: {0}")]
    Snapshot(#[from] ConfiguredSnapshotError),
    #[error("configured Compose compatibility preflight failed: {0}")]
    Compose(#[from] DoctorError),
    #[error("configured replay compatibility capture failed: {0}")]
    Compatibility(#[from] CompatibilityCaptureError),
    #[error("configured case preflight failed: {0}")]
    CasePreflight(#[from] ReferenceCaseRunError),
    #[error("configured report path preflight failed: {0}")]
    ReportPath(#[source] ArtifactError),
    #[error("configured run artifact failed: {0}")]
    Artifact(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured baseline or case database failed: {0}")]
    Baseline(#[from] BaselineError),
    #[error("configured application process attestation failed: {0}")]
    Process(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured Compose project exclusion failed: {0}")]
    ProjectLock(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured case database name is invalid")]
    CaseDatabaseName,
    #[error("configured driver origin is invalid")]
    DriverOrigin,
    #[error("configured fixture sequence is exhausted")]
    FixtureSequenceExhausted,
    #[error("configured fixture request failed: {0}")]
    FixtureRequest(#[source] reqwest::Error),
    #[error("configured fixture state was incoherent")]
    FixtureState,
    #[error("configured case HTTP contract is invalid: {0}")]
    CaseConfig(#[from] ReferenceCaseRunConfigError),
    #[error("configured case execution failed: {0}")]
    CaseRun(ReferenceCaseRunError),
    #[error("configured attempt returned the wrong validated trace authority")]
    AttemptTraceMismatch,
    #[error("configured case {case_id} exceeded its total timeout")]
    CaseTimedOut { case_id: String },
    #[error("configured campaign was interrupted")]
    Interrupted { case_id: Option<String> },
    #[error("configured campaign cleanup failed: {recovery}")]
    RecoveryFailed {
        #[source]
        cause: Option<Box<ConfiguredCampaignError>>,
        recovery: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("configured campaign failed; partial evidence retained at {artifact_path}: {cause}")]
    RunFailed {
        #[source]
        cause: Box<ConfiguredCampaignError>,
        artifact_path: PathBuf,
    },
    #[error("configured campaign partial-evidence finalization failed: {artifact}")]
    PartialFinalization {
        #[source]
        cause: Box<ConfiguredCampaignError>,
        artifact: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("configured invariant execution failed: {0}")]
    Database(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("system clock cannot represent a fixture timestamp")]
    Clock,
}

impl ConfiguredCampaignError {
    #[must_use]
    pub fn failure_class(&self) -> ConfiguredCampaignFailureClass {
        match self {
            Self::Options(_)
            | Self::Config(_)
            | Self::Repository(_)
            | Self::CampaignCompile(_)
            | Self::SqlProbe(_)
            | Self::Quiescence(_)
            | Self::Snapshot(_)
            | Self::CasePreflight(_)
            | Self::ReportPath(_)
            | Self::CaseDatabaseName
            | Self::DriverOrigin
            | Self::CaseConfig(_) => ConfiguredCampaignFailureClass::Configuration,
            Self::Compose(error) if !error.is_infrastructure_failure() => {
                ConfiguredCampaignFailureClass::Configuration
            }
            Self::Compatibility(error) if !error.is_infrastructure_failure() => {
                ConfiguredCampaignFailureClass::Configuration
            }
            Self::Baseline(error) if !error.is_infrastructure_failure() => {
                ConfiguredCampaignFailureClass::Configuration
            }
            Self::CaseRun(error) if error.is_inconclusive() => {
                ConfiguredCampaignFailureClass::Inconclusive
            }
            Self::CaseTimedOut { .. } => ConfiguredCampaignFailureClass::Inconclusive,
            Self::Interrupted { .. } => ConfiguredCampaignFailureClass::Interrupted,
            Self::RecoveryFailed {
                cause: Some(cause), ..
            }
            | Self::RunFailed { cause, .. }
            | Self::PartialFinalization { cause, .. } => cause.failure_class(),
            Self::Artifact(_)
            | Self::Compose(_)
            | Self::Compatibility(_)
            | Self::Baseline(_)
            | Self::Process(_)
            | Self::ProjectLock(_)
            | Self::FixtureSequenceExhausted
            | Self::FixtureRequest(_)
            | Self::FixtureState
            | Self::CaseRun(_)
            | Self::AttemptTraceMismatch
            | Self::RecoveryFailed { cause: None, .. }
            | Self::Database(_)
            | Self::Clock => ConfiguredCampaignFailureClass::Infrastructure,
        }
    }

    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.failure_class().exit_code()
    }

    #[must_use]
    pub fn failure_code(&self) -> &'static str {
        match self {
            Self::Options(_) => "invalid_options",
            Self::Config(_) => "invalid_config",
            Self::Repository(_) => "repository_provenance",
            Self::CampaignCompile(_) => "campaign_compile",
            Self::SqlProbe(_) => "invalid_sql_probe",
            Self::Quiescence(_) => "invalid_quiescence",
            Self::Snapshot(_) => "invalid_snapshot",
            Self::Compose(_) => "compose_preflight",
            Self::Compatibility(_) => "compatibility_capture",
            Self::CasePreflight(_) => "case_preflight",
            Self::ReportPath(_) => "invalid_report_path",
            Self::Artifact(_) => "artifact_failure",
            Self::Baseline(_) => "baseline_failure",
            Self::Process(_) => "process_failure",
            Self::ProjectLock(_) => "project_locked",
            Self::CaseDatabaseName => "invalid_case_database",
            Self::DriverOrigin => "invalid_driver_origin",
            Self::FixtureSequenceExhausted => "fixture_sequence_exhausted",
            Self::FixtureRequest(_) => "fixture_request",
            Self::FixtureState => "fixture_state",
            Self::CaseConfig(_) => "invalid_case_http_contract",
            Self::CaseRun(_) => "case_execution",
            Self::AttemptTraceMismatch => "attempt_trace_mismatch",
            Self::CaseTimedOut { .. } => "case_timeout",
            Self::Interrupted { .. } => "interrupted",
            Self::RecoveryFailed { .. } => "recovery_failed",
            Self::RunFailed { cause, .. } => cause.failure_code(),
            Self::PartialFinalization { .. } => "partial_finalization_failed",
            Self::Database(_) => "invariant_execution",
            Self::Clock => "system_clock",
        }
    }

    #[must_use]
    pub fn artifact_path(&self) -> Option<&Path> {
        match self {
            Self::RunFailed { artifact_path, .. } => Some(artifact_path),
            _ => None,
        }
    }
}

impl From<ArtifactError> for ConfiguredCampaignError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(Box::new(error))
    }
}

impl From<ConfiguredProcessError> for ConfiguredCampaignError {
    fn from(error: ConfiguredProcessError) -> Self {
        Self::Process(Box::new(error))
    }
}

impl From<ConfiguredDatabaseError> for ConfiguredCampaignError {
    fn from(error: ConfiguredDatabaseError) -> Self {
        Self::Database(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tiv_core::{
        decision::Seed,
        plan::{
            ActionBudget, CampaignPlanner, CampaignSpec, CaseCount, ProcessCutPoint,
            ProcessFaultSpec, ProviderOutcome, WebhookFaultSpec,
        },
    };

    use super::{
        CampaignSummary, CaseArtifact, ConfiguredCampaignError, ConfiguredCampaignFailureClass,
        ConfiguredCampaignOptions, ConfiguredCampaignOptionsError, ConfiguredCampaignVerdict,
        InvariantArtifact, MAX_PERSISTED_WITNESS_BYTES_PER_ATTEMPT, completed_campaign_report,
        reference_ledger_witness_row,
    };

    #[test]
    fn reference_ledger_witness_allowlist_is_exact_and_budgeted() {
        let valid = serde_json::json!({
            "provider_event_id": "evt_tiv_contract",
            "operation_id": "op_deadbeef",
            "entry_id": "10000000-0000-4000-8000-000000000001",
            "currency": "usd",
            "posting_count": 1,
            "debit_posting_count": 1,
            "credit_posting_count": 0,
            "debit_total_minor": 2500,
            "credit_total_minor": 0,
            "imbalance_minor": 2500
        });
        let valid = valid.as_object().unwrap();
        assert!(reference_ledger_witness_row(valid));

        let mut unexpected = valid.clone();
        unexpected.insert(
            "secret".to_owned(),
            serde_json::Value::String("must-not-persist".to_owned()),
        );
        assert!(!reference_ledger_witness_row(&unexpected));

        let mut incoherent = valid.clone();
        incoherent.insert("imbalance_minor".to_owned(), serde_json::json!(1));
        assert!(!reference_ledger_witness_row(&incoherent));

        const {
            assert!(MAX_PERSISTED_WITNESS_BYTES_PER_ATTEMPT * 500 < 5 * 1024 * 1024);
            assert!(MAX_PERSISTED_WITNESS_BYTES_PER_ATTEMPT * 183 < 2 * 1024 * 1024);
        }
    }
    use crate::{
        compatibility::{CompatibilityCaptureError, CompatibilityError},
        doctor::DoctorError,
        postgres::quiescence::QuiescenceError,
        reference_case::{
            ReferenceCaseError, ReferenceCaseRunError, preflight_reference_planned_case,
        },
        reports::{ReportCheckOutcome, ReportConclusion},
    };

    #[test]
    fn held_campaign_report_maps_the_real_producer_outcome() {
        let summary = CampaignSummary {
            schema_version: 1,
            run_id: "run_held".to_owned(),
            verdict: ConfiguredCampaignVerdict::Held,
            configured_cases: 1,
            completed_cases: 1,
            cases: vec![CaseArtifact {
                schema_version: 1,
                case_id: "case_0001".to_owned(),
                seed: 1,
                planned_action_count: 1,
                executed_action_count: 1,
                journal_record_count: 1,
                journal_last_record_hash: None,
                before_database_oid: 1,
                after_database_oid: 2,
                before_marker_uuid: "before".to_owned(),
                after_marker_uuid: "after".to_owned(),
                provider_object_count: 1,
                invariants: vec![InvariantArtifact {
                    invariant_id: "provider_object_uniqueness".to_owned(),
                    checkpoint_id: "checkout_complete".to_owned(),
                    verdict: ConfiguredCampaignVerdict::Held,
                    witness_count: 0,
                }],
            }],
        };

        let report = completed_campaign_report(
            &summary,
            Path::new("/tmp/run_held"),
            Path::new("/tmp/tiv.toml"),
        )
        .unwrap();

        assert_eq!(report.conclusion(), ReportConclusion::Held);
        assert_eq!(report.check_outcomes(), vec![ReportCheckOutcome::Passed]);
        assert_eq!(report.replay_command_count(), 0);
    }

    #[test]
    fn configured_campaign_options_are_explicit_and_bounded_by_typed_core_values() {
        let options = ConfiguredCampaignOptions::new(Some(99), Some(2), true).unwrap();

        assert_eq!(options.seed(), Some(99));
        assert_eq!(options.cases(), Some(2));
        assert!(options.ci());
        assert!(ConfiguredCampaignOptions::new(None, Some(0), false).is_err());
        assert!(ConfiguredCampaignOptions::new(None, Some(501), false).is_err());
    }

    #[test]
    fn configured_failures_keep_the_non_overlapping_cli_classification() {
        let configuration = ConfiguredCampaignError::Options(ConfiguredCampaignOptionsError);
        let infrastructure =
            ConfiguredCampaignError::Process(Box::new(std::io::Error::other("docker unavailable")));
        let inconclusive = ConfiguredCampaignError::CaseTimedOut {
            case_id: "case_0001".to_owned(),
        };
        let interrupted = ConfiguredCampaignError::Interrupted {
            case_id: Some("case_0001".to_owned()),
        };
        let incompatible = ConfiguredCampaignError::Compatibility(
            CompatibilityCaptureError::Contract(CompatibilityError::InvalidDocument),
        );
        let compose_unavailable = ConfiguredCampaignError::Compose(DoctorError::DockerUnavailable);

        assert_eq!(
            configuration.failure_class(),
            ConfiguredCampaignFailureClass::Configuration
        );
        assert_eq!(infrastructure.exit_code(), 3);
        assert_eq!(inconclusive.exit_code(), 4);
        assert_eq!(interrupted.exit_code(), 130);
        assert_eq!(inconclusive.failure_code(), "case_timeout");
        assert_eq!(
            incompatible.failure_class(),
            ConfiguredCampaignFailureClass::Configuration
        );
        assert_eq!(compose_unavailable.exit_code(), 3);
    }

    #[test]
    fn finalized_partial_evidence_does_not_erase_the_original_failure_class() {
        let error = ConfiguredCampaignError::RunFailed {
            cause: Box::new(ConfiguredCampaignError::CaseTimedOut {
                case_id: "case_0001".to_owned(),
            }),
            artifact_path: PathBuf::from("/tmp/run_partial"),
        };

        assert_eq!(error.exit_code(), 4);
        assert_eq!(error.failure_code(), "case_timeout");
        assert_eq!(error.artifact_path(), Some(Path::new("/tmp/run_partial")));
    }

    #[test]
    fn quiescence_timeout_is_inconclusive_and_never_an_invariant_violation() {
        let error = ConfiguredCampaignError::CaseRun(ReferenceCaseRunError::Lifecycle(
            ReferenceCaseError::DatabaseQuiescence(QuiescenceError::Timeout),
        ));

        assert_eq!(
            error.failure_class(),
            ConfiguredCampaignFailureClass::Inconclusive
        );
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn configured_fault_model_has_a_one_case_seed_supported_by_current_adapters() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let seed = (0..1_024)
            .find(|seed| {
                let spec = CampaignSpec::new_payment_intent_v1(
                    Seed::new(*seed),
                    CaseCount::new(1).unwrap(),
                    ActionBudget::new(40).unwrap(),
                    [
                        ProviderOutcome::Normal,
                        ProviderOutcome::PreExecute429,
                        ProviderOutcome::PreExecute500,
                        ProviderOutcome::PostExecute500,
                        ProviderOutcome::CommitThenClose,
                        ProviderOutcome::CommitThenDelay,
                    ],
                    WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
                    process_faults.clone(),
                )
                .unwrap();
                CampaignPlanner::compile(&spec).is_ok_and(|campaign| {
                    preflight_reference_planned_case(campaign.cases()[0].plan(), true, true).is_ok()
                })
            })
            .expect("the bounded corpus contains an adapter-supported one-case campaign");

        assert_eq!(seed, 4);
    }

    #[test]
    fn row_three_campaign_seed_has_one_supported_commit_close_checkout() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let spec = CampaignSpec::new_payment_intent_v1(
            Seed::new(69),
            CaseCount::new(1).unwrap(),
            ActionBudget::new(40).unwrap(),
            [
                ProviderOutcome::Normal,
                ProviderOutcome::PreExecute429,
                ProviderOutcome::PreExecute500,
                ProviderOutcome::PostExecute500,
                ProviderOutcome::CommitThenClose,
                ProviderOutcome::CommitThenDelay,
            ],
            WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
            process_faults,
        )
        .unwrap();
        let campaign = CampaignPlanner::compile(&spec).expect("the row-three campaign compiles");
        let plan = campaign.cases()[0].plan();
        let business_scripts = plan
            .actions()
            .iter()
            .filter_map(|action| match action.kind() {
                tiv_core::plan::PlanActionKind::DriveCheckout { provider_script }
                | tiv_core::plan::PlanActionKind::RetryBusinessRequest { provider_script } => {
                    Some(provider_script.outcomes().collect::<Vec<ProviderOutcome>>())
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            business_scripts,
            vec![vec![
                ProviderOutcome::CommitThenClose,
                ProviderOutcome::Normal,
            ]]
        );
        preflight_reference_planned_case(plan, true, true)
            .expect("the row-three campaign uses supported process boundaries");
    }

    #[test]
    fn row_one_campaign_seed_isolates_one_duplicated_immutable_event() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let spec = CampaignSpec::new_payment_intent_v1(
            Seed::new(1_792),
            CaseCount::new(1).unwrap(),
            ActionBudget::new(40).unwrap(),
            [
                ProviderOutcome::Normal,
                ProviderOutcome::PreExecute429,
                ProviderOutcome::PreExecute500,
                ProviderOutcome::PostExecute500,
                ProviderOutcome::CommitThenClose,
                ProviderOutcome::CommitThenDelay,
            ],
            WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
            process_faults,
        )
        .unwrap();
        let campaign = CampaignPlanner::compile(&spec).expect("the row-one campaign compiles");
        let plan = campaign.cases()[0].plan();
        preflight_reference_planned_case(plan, true, true)
            .expect("the row-one campaign uses supported boundaries");
        let actions = plan.actions();
        let committed = actions
            .iter()
            .filter_map(|action| match action.kind() {
                tiv_core::plan::PlanActionKind::DriveCheckout { provider_script }
                | tiv_core::plan::PlanActionKind::RetryBusinessRequest { provider_script } => {
                    Some(usize::from(provider_script.committed_count()))
                }
                _ => None,
            })
            .sum::<usize>();
        let generated = actions
            .iter()
            .filter(|action| {
                matches!(
                    action.kind(),
                    tiv_core::plan::PlanActionKind::GenerateProviderEvent
                )
            })
            .count();
        let duplicate_count = actions
            .iter()
            .filter(|action| {
                matches!(
                    action.kind(),
                    tiv_core::plan::PlanActionKind::DuplicateWebhook
                )
            })
            .count();

        assert_eq!(committed, 1);
        assert_eq!(generated, 1);
        assert_eq!(duplicate_count, 1);
        assert!(actions.iter().all(|action| !matches!(
            action.kind(),
            tiv_core::plan::PlanActionKind::KillApplication { .. }
        )));
    }

    #[test]
    fn full_configured_fault_model_reaches_two_persisted_provider_objects() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let seed = (0..4_096)
            .find(|seed| {
                let spec = CampaignSpec::new_payment_intent_v1(
                    Seed::new(*seed),
                    CaseCount::new(1).unwrap(),
                    ActionBudget::new(40).unwrap(),
                    [
                        ProviderOutcome::Normal,
                        ProviderOutcome::PreExecute429,
                        ProviderOutcome::PreExecute500,
                        ProviderOutcome::PostExecute500,
                        ProviderOutcome::CommitThenClose,
                        ProviderOutcome::CommitThenDelay,
                    ],
                    WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
                    process_faults.clone(),
                )
                .unwrap();
                CampaignPlanner::compile(&spec).is_ok_and(|campaign| {
                    let plan = campaign.cases()[0].plan();
                    if preflight_reference_planned_case(plan, true, true).is_err() {
                        return false;
                    }
                    let actions = plan.actions();
                    let committed = actions
                        .iter()
                        .filter_map(|action| match action.kind() {
                            tiv_core::plan::PlanActionKind::DriveCheckout { provider_script }
                            | tiv_core::plan::PlanActionKind::RetryBusinessRequest {
                                provider_script,
                            } => Some(usize::from(provider_script.committed_count())),
                            _ => None,
                        })
                        .sum::<usize>();
                    let persisted_deliveries = actions
                        .iter()
                        .enumerate()
                        .filter(|(index, action)| {
                            matches!(
                                action.kind(),
                                tiv_core::plan::PlanActionKind::DeliverWebhook
                                    | tiv_core::plan::PlanActionKind::DuplicateWebhook
                            ) && !actions.get(index + 1).is_some_and(|next| {
                                matches!(
                                    next.kind(),
                                    tiv_core::plan::PlanActionKind::KillApplication {
                                        cut_point: ProcessCutPoint::WebhookRequestForwarded
                                    }
                                )
                            })
                        })
                        .count();
                    committed >= 2 && persisted_deliveries >= 2
                })
            })
            .expect("the bounded corpus reaches two persisted provider objects");

        assert_eq!(seed, 8);
    }

    #[test]
    fn configured_fault_model_reaches_the_webhook_request_forwarded_handshake() {
        let process_faults =
            ProcessFaultSpec::new([ProcessCutPoint::WebhookRequestForwarded], 1).unwrap();
        let seed = (0..4_096)
            .find(|seed| {
                let spec = CampaignSpec::new_payment_intent_v1(
                    Seed::new(*seed),
                    CaseCount::new(1).unwrap(),
                    ActionBudget::new(40).unwrap(),
                    [ProviderOutcome::Normal],
                    WebhookFaultSpec::new(1, [], false, false).unwrap(),
                    process_faults.clone(),
                )
                .unwrap();
                CampaignPlanner::compile(&spec).is_ok_and(|campaign| {
                    campaign.cases()[0]
                        .plan()
                        .actions()
                        .windows(2)
                        .any(|actions| {
                            matches!(
                                actions[0].kind(),
                                tiv_core::plan::PlanActionKind::DeliverWebhook
                                    | tiv_core::plan::PlanActionKind::DuplicateWebhook
                            ) && matches!(
                                actions[1].kind(),
                                tiv_core::plan::PlanActionKind::KillApplication {
                                    cut_point: ProcessCutPoint::WebhookRequestForwarded
                                }
                            )
                        })
                })
            })
            .expect("the bounded corpus reaches the instrumented ingress handshake");

        assert_eq!(seed, 2);
    }

    #[test]
    fn full_configured_fault_model_has_a_deterministic_webhook_request_cut_seed() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let seed = (0..4_096)
            .find(|seed| {
                let spec = CampaignSpec::new_payment_intent_v1(
                    Seed::new(*seed),
                    CaseCount::new(1).unwrap(),
                    ActionBudget::new(40).unwrap(),
                    [
                        ProviderOutcome::Normal,
                        ProviderOutcome::PreExecute429,
                        ProviderOutcome::PreExecute500,
                        ProviderOutcome::PostExecute500,
                        ProviderOutcome::CommitThenClose,
                        ProviderOutcome::CommitThenDelay,
                    ],
                    WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
                    process_faults.clone(),
                )
                .unwrap();
                CampaignPlanner::compile(&spec).is_ok_and(|campaign| {
                    campaign.cases()[0]
                        .plan()
                        .actions()
                        .windows(2)
                        .any(|actions| {
                            matches!(
                                actions[0].kind(),
                                tiv_core::plan::PlanActionKind::DeliverWebhook
                                    | tiv_core::plan::PlanActionKind::DuplicateWebhook
                            ) && matches!(
                                actions[1].kind(),
                                tiv_core::plan::PlanActionKind::KillApplication {
                                    cut_point: ProcessCutPoint::WebhookRequestForwarded
                                }
                            )
                        })
                })
            })
            .expect("the bounded full-config corpus reaches the instrumented ingress handshake");

        assert_eq!(seed, 67);
    }

    #[test]
    fn full_configured_fault_model_has_a_supported_two_case_seed() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let seed = (0..4_096)
            .find(|seed| {
                let spec = CampaignSpec::new_payment_intent_v1(
                    Seed::new(*seed),
                    CaseCount::new(2).unwrap(),
                    ActionBudget::new(40).unwrap(),
                    [
                        ProviderOutcome::Normal,
                        ProviderOutcome::PreExecute429,
                        ProviderOutcome::PreExecute500,
                        ProviderOutcome::PostExecute500,
                        ProviderOutcome::CommitThenClose,
                        ProviderOutcome::CommitThenDelay,
                    ],
                    WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
                    process_faults.clone(),
                )
                .unwrap();
                CampaignPlanner::compile(&spec).is_ok_and(|campaign| {
                    campaign.cases().iter().all(|case| {
                        preflight_reference_planned_case(case.plan(), true, true).is_ok()
                            && case.plan().actions().windows(2).all(|actions| {
                                if !matches!(
                                    actions[1].kind(),
                                    tiv_core::plan::PlanActionKind::KillApplication {
                                        cut_point: ProcessCutPoint::SqlProbe
                                    }
                                ) {
                                    return true;
                                }
                                matches!(
                                    actions[0].kind(),
                                    tiv_core::plan::PlanActionKind::DriveCheckout {
                                        provider_script
                                    } if provider_script.terminal_outcome() == ProviderOutcome::Normal
                                )
                            })
                    })
                })
            })
            .expect("the bounded full-config corpus contains two supported serial cases");

        assert_eq!(seed, 48);
    }

    #[test]
    fn row_four_campaign_seed_isolates_unacknowledged_checkout_and_caller_retry() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let campaign_seed = 422;
        let spec = CampaignSpec::new_payment_intent_v1(
            Seed::new(campaign_seed),
            CaseCount::new(1).unwrap(),
            ActionBudget::new(40).unwrap(),
            [
                ProviderOutcome::Normal,
                ProviderOutcome::PreExecute429,
                ProviderOutcome::PreExecute500,
                ProviderOutcome::PostExecute500,
                ProviderOutcome::CommitThenClose,
                ProviderOutcome::CommitThenDelay,
            ],
            WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
            process_faults,
        )
        .unwrap();
        let campaign = CampaignPlanner::compile(&spec).unwrap();
        let actions = campaign.cases()[0].plan().actions();
        assert!(matches!(
            actions.first().map(tiv_core::plan::PlannedAction::kind),
            Some(tiv_core::plan::PlanActionKind::DriveCheckout {
                provider_script
            }) if provider_script.terminal_outcome() == ProviderOutcome::Normal
        ));
        assert!(matches!(
            actions.get(1).map(tiv_core::plan::PlannedAction::kind),
            Some(tiv_core::plan::PlanActionKind::KillApplication {
                cut_point: ProcessCutPoint::ClientResponseObserved
            })
        ));
        assert!(matches!(
            actions.get(2).map(tiv_core::plan::PlannedAction::kind),
            Some(tiv_core::plan::PlanActionKind::RestartAndAwaitHealth)
        ));
        assert!(matches!(
            actions.get(3).map(tiv_core::plan::PlannedAction::kind),
            Some(tiv_core::plan::PlanActionKind::RetryBusinessRequest {
                provider_script
            }) if provider_script.terminal_outcome() == ProviderOutcome::Normal
        ));
        preflight_reference_planned_case(campaign.cases()[0].plan(), true, true).unwrap();
        assert_eq!(campaign_seed, 422);
    }

    #[test]
    fn row_five_campaign_seed_isolates_one_dropped_success_event() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let campaign_seed = 359;
        let spec = CampaignSpec::new_payment_intent_v1(
            Seed::new(campaign_seed),
            CaseCount::new(1).unwrap(),
            ActionBudget::new(40).unwrap(),
            [
                ProviderOutcome::Normal,
                ProviderOutcome::PreExecute429,
                ProviderOutcome::PreExecute500,
                ProviderOutcome::PostExecute500,
                ProviderOutcome::CommitThenClose,
                ProviderOutcome::CommitThenDelay,
            ],
            WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
            process_faults,
        )
        .unwrap();
        let campaign = CampaignPlanner::compile(&spec).unwrap();
        let plan = campaign.cases()[0].plan();
        preflight_reference_planned_case(plan, true, true).unwrap();
        let actions = plan.actions();
        let committed = actions
            .iter()
            .filter_map(|action| match action.kind() {
                tiv_core::plan::PlanActionKind::DriveCheckout { provider_script }
                | tiv_core::plan::PlanActionKind::RetryBusinessRequest { provider_script } => {
                    Some(usize::from(provider_script.committed_count()))
                }
                _ => None,
            })
            .sum::<usize>();

        assert_eq!(committed, 1);
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(
                    action.kind(),
                    tiv_core::plan::PlanActionKind::GenerateProviderEvent
                ))
                .count(),
            1
        );
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(
                    action.kind(),
                    tiv_core::plan::PlanActionKind::DropWebhook
                ))
                .count(),
            1
        );
        assert!(actions.iter().all(|action| !matches!(
            action.kind(),
            tiv_core::plan::PlanActionKind::DeliverWebhook
                | tiv_core::plan::PlanActionKind::DuplicateWebhook
                | tiv_core::plan::PlanActionKind::KillApplication { .. }
        )));
    }
}
