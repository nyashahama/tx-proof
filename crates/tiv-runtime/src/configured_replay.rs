//! Compatibility-gated replay of one configured campaign case.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiv_core::{
    plan::{CampaignPlan, CaseId, PlannedCase},
    result::{
        AttemptResult, CheckpointId, FailureIdentity, InvariantId, ReproductionClass,
        classify_reproduction,
    },
    trace::CompiledCaseTrace,
};

use crate::{
    artifacts::{
        ArtifactAuthority, ArtifactError, ArtifactKind, ArtifactResult, ManifestSeed,
        PartialRunClass, RunArtifactStaging, VerifiedRunArtifact, verify_complete_run_artifact,
    },
    baseline::{BaselineError, ConfiguredBaselineSession},
    compatibility::{
        CompatibilityCaptureError, CompatibilityError, RunCompatibilityV1,
        capture_run_compatibility,
    },
    config::{ConfigError, EnvironmentLookup, ResolvedConfig, load_resolved_config},
    configured_campaign::{
        ConfiguredCampaignError, ConfiguredCampaignFailureClass, ConfiguredCaseExecution,
        ConfiguredInvariantOutcome, RunCancellation, execute_configured_case_attempt,
    },
    configured_process::{
        ConfiguredProcessControl, ConfiguredProcessError, attest_configured_service_images,
    },
    doctor::{DoctorError, collect_compose_facts},
    postgres::{
        probe::{ConfiguredSqlProbeError, load_configured_sql_probe},
        quiescence::{ConfiguredQuiescenceError, load_configured_quiescence},
        snapshot::{ConfiguredSnapshotError, load_configured_snapshot},
    },
    reference_case::{ReferenceCaseRunError, preflight_reference_planned_case},
    reports::{
        ArtifactReport, ReplayCommand, ReportArtifactKind, ReportCheck, ReportCheckOutcome,
        ReportConclusion, ReportFact, ReportFailure, partial_artifact_report,
        validate_replay_command_paths, write_report_bundle,
    },
    repository::capture_repository_provenance,
    run_supervisor::ComposeProjectLock,
};

pub(crate) const ATTEMPT_COUNT: usize = 3;

/// Fixed version-one configured replay selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfiguredReplayOptions {
    case_id: CaseId,
}

impl ConfiguredReplayOptions {
    /// Selects one recorded campaign case for exactly three fresh attempts.
    ///
    /// # Errors
    ///
    /// Returns [`ConfiguredReplayOptionsError`] for zero or a case above the
    /// version-one campaign maximum.
    pub fn new(case_number: u32) -> Result<Self, ConfiguredReplayOptionsError> {
        let case_id =
            CaseId::new(case_number).map_err(|_| ConfiguredReplayOptionsError::InvalidCase)?;
        Ok(Self { case_id })
    }

    #[must_use]
    pub const fn case_number(self) -> u32 {
        self.case_id.value()
    }

    #[must_use]
    pub const fn attempt_count(self) -> usize {
        ATTEMPT_COUNT
    }

    const fn case_id(self) -> CaseId {
        self.case_id
    }
}

/// Invalid configured replay selection.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ConfiguredReplayOptionsError {
    #[error("configured replay case is outside the v1 campaign limit")]
    InvalidCase,
}

/// Three-attempt same-identity reproduction classification.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfiguredReplayClassification {
    Stable,
    Reproducible,
    Inconclusive,
}

impl ConfiguredReplayClassification {
    /// Maps the completed replay conclusion to the non-overlapping CLI exit
    /// contract.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Stable | Self::Reproducible => 10,
            Self::Inconclusive => 4,
        }
    }
}

impl From<ReproductionClass> for ConfiguredReplayClassification {
    fn from(value: ReproductionClass) -> Self {
        match value {
            ReproductionClass::Stable => Self::Stable,
            ReproductionClass::Reproducible => Self::Reproducible,
            ReproductionClass::Inconclusive => Self::Inconclusive,
        }
    }
}

/// Secret-free receipt for one completed configured replay.
#[derive(Serialize)]
pub struct ConfiguredReplayOutput {
    schema_version: u16,
    status: &'static str,
    replay_id: String,
    source_run_id: String,
    case_id: String,
    attempt_count: usize,
    matching_failure_count: usize,
    classification: ConfiguredReplayClassification,
    artifact_path: PathBuf,
}

impl ConfiguredReplayOutput {
    #[must_use]
    pub const fn classification(&self) -> ConfiguredReplayClassification {
        self.classification
    }

    #[must_use]
    pub fn artifact_path(&self) -> &Path {
        &self.artifact_path
    }

    /// Encodes the allowlisted configured replay receipt.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if JSON encoding fails.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RecordedVerdict {
    Held,
    Violated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RecordedInvariant {
    invariant_id: String,
    checkpoint_id: String,
    verdict: RecordedVerdict,
    witness_count: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct RecordedCaseResult {
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
    invariants: Vec<RecordedInvariant>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedCampaignSummary {
    schema_version: u16,
    run_id: String,
    verdict: RecordedVerdict,
    configured_cases: usize,
    completed_cases: usize,
    cases: Vec<RecordedCaseResult>,
}

struct RecordedConfiguredCase {
    case_id: String,
    trace: CompiledCaseTrace,
    expected_failure: FailureIdentity,
}

impl RecordedConfiguredCase {
    fn case_id(&self) -> &str {
        &self.case_id
    }

    const fn trace(&self) -> &CompiledCaseTrace {
        &self.trace
    }

    const fn expected_failure(&self) -> &FailureIdentity {
        &self.expected_failure
    }
}

#[derive(Serialize)]
struct ReplaySourceArtifact<'a> {
    schema_version: u16,
    source_run_id: &'a str,
    case_id: &'a str,
    expected_failure: FailureIdentityArtifact,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FailureIdentityArtifact {
    invariant_id: String,
    checkpoint_id: String,
}

impl From<&FailureIdentity> for FailureIdentityArtifact {
    fn from(identity: &FailureIdentity) -> Self {
        Self {
            invariant_id: identity.invariant().as_str().to_owned(),
            checkpoint_id: identity.checkpoint().as_str().to_owned(),
        }
    }
}

impl FailureIdentityArtifact {
    fn to_identity(&self) -> Result<FailureIdentity, ConfiguredReplayArtifactError> {
        Ok(FailureIdentity::new(
            InvariantId::new(&self.invariant_id)
                .map_err(|_| ConfiguredReplayArtifactError::InvalidReplaySummary)?,
            CheckpointId::new(&self.checkpoint_id)
                .map_err(|_| ConfiguredReplayArtifactError::InvalidReplaySummary)?,
        ))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReplayAttemptVerdict {
    Held,
    ExpectedViolation,
    OtherViolation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReplayAttemptArtifact {
    schema_version: u16,
    attempt: usize,
    case_id: String,
    trace_matches_source: bool,
    before_database_oid: u32,
    after_database_oid: u32,
    before_marker_uuid: String,
    after_marker_uuid: String,
    executed_action_count: usize,
    journal_record_count: usize,
    journal_last_record_hash: Option<String>,
    provider_object_count: usize,
    verdict: ReplayAttemptVerdict,
    failure_identity: Option<FailureIdentityArtifact>,
    invariants: Vec<ReplayInvariantArtifact>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ReplayInvariantArtifact {
    invariant_id: String,
    checkpoint_id: String,
    verdict: RecordedVerdict,
    witness_count: usize,
}

#[derive(Serialize)]
struct ReplaySummary<'a> {
    schema_version: u16,
    status: &'static str,
    replay_id: &'a str,
    source_run_id: &'a str,
    case_id: &'a str,
    expected_failure: FailureIdentityArtifact,
    attempt_count: usize,
    matching_failure_count: usize,
    classification: ConfiguredReplayClassification,
    attempts: &'a [ReplayAttemptArtifact],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaySourceDocument {
    schema_version: u16,
    source_run_id: String,
    case_id: String,
    expected_failure: FailureIdentityArtifact,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaySummaryDocument {
    schema_version: u16,
    status: String,
    replay_id: String,
    source_run_id: String,
    case_id: String,
    expected_failure: FailureIdentityArtifact,
    attempt_count: usize,
    matching_failure_count: usize,
    classification: ConfiguredReplayClassification,
    attempts: Vec<ReplayAttemptArtifact>,
}

pub(crate) struct VerifiedConfiguredReplaySource {
    replay_id: String,
    source_run_id: String,
    case_id: String,
    expected_failure: FailureIdentity,
    original_trace: CompiledCaseTrace,
    original_trace_bytes: Vec<u8>,
}

impl VerifiedConfiguredReplaySource {
    pub(crate) fn replay_id(&self) -> &str {
        &self.replay_id
    }

    pub(crate) fn source_run_id(&self) -> &str {
        &self.source_run_id
    }

    pub(crate) fn case_id(&self) -> &str {
        &self.case_id
    }

    pub(crate) const fn expected_failure(&self) -> &FailureIdentity {
        &self.expected_failure
    }

    pub(crate) const fn original_trace(&self) -> &CompiledCaseTrace {
        &self.original_trace
    }

    pub(crate) fn original_trace_bytes(&self) -> &[u8] {
        &self.original_trace_bytes
    }
}

#[derive(Serialize)]
struct PartialReplaySummary<'a> {
    schema_version: u16,
    status: &'static str,
    replay_id: &'a str,
    source_run_id: &'a str,
    case_id: &'a str,
    failure_class: ConfiguredCampaignFailureClass,
    failure_code: &'static str,
    completed_attempts: usize,
    attempts: &'a [ReplayAttemptArtifact],
}

/// Replays one verified violating configured case exactly three times from
/// freshly reset baselines.
///
/// The source artifact is verified before stack access. Current execution
/// compatibility is captured and compared exactly before the first customer
/// case reset.
///
/// # Errors
///
/// Returns [`ConfiguredReplayError`] for invalid source evidence, preparation,
/// compatibility, reset, execution, oracle, recovery, or evidence failures.
pub async fn run_configured_replay(
    artifact_path: &Path,
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    options: ConfiguredReplayOptions,
) -> Result<ConfiguredReplayOutput, ConfiguredReplayError> {
    Box::pin(run_configured_replay_with_cancellation(
        artifact_path,
        config_path,
        environment,
        options,
        &RunCancellation::new(),
    ))
    .await
}

/// Executes configured replay with a root cancellation capability.
///
/// # Errors
///
/// Returns [`ConfiguredReplayError`] after recovery and partial-evidence
/// finalization when cancellation interrupts an entered replay boundary.
#[allow(clippy::too_many_lines)]
pub async fn run_configured_replay_with_cancellation(
    artifact_path: &Path,
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    options: ConfiguredReplayOptions,
    cancellation: &RunCancellation,
) -> Result<ConfiguredReplayOutput, ConfiguredReplayError> {
    let source = verify_complete_run_artifact(artifact_path)
        .map_err(ConfiguredReplayArtifactError::Artifact)?;
    let recorded = load_recorded_case(&source, options)?;
    let config = load_resolved_config(config_path, environment)?;
    let configured_probe = load_configured_sql_probe(&config)?;
    let configured_quiescence = load_configured_quiescence(&config)?;
    let configured_snapshot = load_configured_snapshot(&config)?;
    preflight_reference_planned_case(recorded.trace().planned_case(), true, true)?;
    validate_replay_command_paths(source.root(), config.source_path())
        .map_err(ConfiguredReplayError::ReportPath)?;
    if cancellation.is_cancelled() {
        return Err(ConfiguredReplayError::Interrupted);
    }
    let _project_lock = ComposeProjectLock::try_acquire(config.root(), config.compose_project())
        .map_err(|error| ConfiguredReplayError::ProjectLock(Box::new(error)))?;
    let (baseline, current_compatibility) = attest_replay_boundary(
        &config,
        &configured_probe,
        &configured_quiescence,
        &configured_snapshot,
    )
    .await?;
    source
        .compatibility()
        .require_exact_match(&current_compatibility)?;
    if cancellation.is_cancelled() {
        return Err(ConfiguredReplayError::Interrupted);
    }
    let repository = capture_repository_provenance(config.root())
        .await
        .map_err(|error| ConfiguredReplayError::Repository(Box::new(error)))?;

    let replay_id = format!("run_{}", uuid::Uuid::new_v4().simple());
    let mut artifacts = RunArtifactStaging::create_v2(
        config.root(),
        config.artifact_dir(),
        &replay_id,
        ManifestSeed::new(
            ArtifactKind::Replay,
            repository,
            vec![source.source_identity()],
        ),
    )
    .map_err(ConfiguredReplayError::EvidenceArtifact)?;
    artifacts
        .write_json("config.redacted.json", config.redacted())
        .map_err(ConfiguredReplayError::EvidenceArtifact)?;
    artifacts
        .write_json("compatibility.json", &current_compatibility)
        .map_err(ConfiguredReplayError::EvidenceArtifact)?;
    artifacts
        .write_json("trace.original.json", recorded.trace())
        .map_err(ConfiguredReplayError::EvidenceArtifact)?;
    artifacts
        .write_json(
            "source.json",
            &ReplaySourceArtifact {
                schema_version: 1,
                source_run_id: source.run_id(),
                case_id: recorded.case_id(),
                expected_failure: recorded.expected_failure().into(),
            },
        )
        .map_err(ConfiguredReplayError::EvidenceArtifact)?;

    let mut attempt_artifacts = Vec::with_capacity(ATTEMPT_COUNT);
    let mut attempt_results = Vec::with_capacity(ATTEMPT_COUNT);
    let execution = Box::pin(execute_replay_attempts(
        &config,
        &configured_probe,
        &configured_quiescence,
        &configured_snapshot,
        baseline,
        source.compatibility(),
        &recorded,
        &replay_id,
        &mut artifacts,
        &mut attempt_artifacts,
        &mut attempt_results,
        cancellation,
    ))
    .await;
    if let Err(cause) = execution {
        return finalize_failed_replay(
            artifacts,
            cause,
            &replay_id,
            source.run_id(),
            recorded.case_id(),
            &attempt_artifacts,
        );
    }

    let attempts: [AttemptResult; ATTEMPT_COUNT] = attempt_results
        .try_into()
        .map_err(|_| ConfiguredReplayError::AttemptAccounting)?;
    let reproduction = classify_reproduction(recorded.expected_failure(), &attempts);
    let classification = reproduction.into();
    let matching_failure_count = attempts
        .iter()
        .filter(|attempt| {
            matches!(attempt, AttemptResult::Violation(identity) if identity == recorded.expected_failure())
        })
        .count();
    let summary = ReplaySummary {
        schema_version: 1,
        status: "configured_replay_complete",
        replay_id: &replay_id,
        source_run_id: source.run_id(),
        case_id: recorded.case_id(),
        expected_failure: recorded.expected_failure().into(),
        attempt_count: ATTEMPT_COUNT,
        matching_failure_count,
        classification,
        attempts: &attempt_artifacts,
    };
    artifacts
        .write_json("summary.json", &summary)
        .map_err(ConfiguredReplayError::EvidenceArtifact)?;
    let report = completed_replay_report(
        &summary,
        source.root(),
        config.source_path(),
        options.case_number(),
    );
    write_report_bundle(&mut artifacts, &report)
        .map_err(ConfiguredReplayError::EvidenceArtifact)?;
    let result = match classification {
        ConfiguredReplayClassification::Stable | ConfiguredReplayClassification::Reproducible => {
            ArtifactResult::Counterexample
        }
        ConfiguredReplayClassification::Inconclusive => ArtifactResult::Inconclusive,
    };
    let artifact_path = artifacts
        .finalize_complete(
            result,
            vec![
                ArtifactAuthority::replay_source(),
                ArtifactAuthority::original_trace(),
            ],
        )
        .map_err(ConfiguredReplayError::EvidenceArtifact)?;
    Ok(ConfiguredReplayOutput {
        schema_version: 1,
        status: "configured_replay_complete",
        replay_id,
        source_run_id: source.run_id().to_owned(),
        case_id: recorded.case_id().to_owned(),
        attempt_count: ATTEMPT_COUNT,
        matching_failure_count,
        classification,
        artifact_path,
    })
}

#[allow(clippy::too_many_arguments)]
async fn execute_replay_attempts(
    config: &ResolvedConfig,
    configured_probe: &crate::postgres::probe::ConfiguredSqlProbe,
    configured_quiescence: &crate::postgres::quiescence::ConfiguredQuiescence,
    configured_snapshot: &crate::postgres::snapshot::ConfiguredSnapshot,
    baseline: ConfiguredBaselineSession,
    expected_compatibility: &RunCompatibilityV1,
    recorded: &RecordedConfiguredCase,
    replay_id: &str,
    artifacts: &mut RunArtifactStaging,
    attempt_artifacts: &mut Vec<ReplayAttemptArtifact>,
    attempt_results: &mut Vec<AttemptResult>,
    cancellation: &RunCancellation,
) -> Result<(), ConfiguredReplayError> {
    let mut initial_baseline = Some(baseline);
    for attempt in 1..=ATTEMPT_COUNT {
        if cancellation.is_cancelled() {
            return Err(ConfiguredReplayError::Interrupted);
        }
        let attempt_id = format!("attempt_{attempt:04}");
        let journal_path = artifacts
            .prepare_path(format!("attempts/{attempt_id}/observations.ndjson"))
            .map_err(ConfiguredReplayError::EvidenceArtifact)?;
        let current_baseline = if attempt == 1 {
            initial_baseline
                .take()
                .ok_or(ConfiguredReplayError::AttemptAccounting)?
        } else {
            let (fresh_baseline, current_compatibility) = attest_replay_boundary(
                config,
                configured_probe,
                configured_quiescence,
                configured_snapshot,
            )
            .await?;
            expected_compatibility.require_exact_match(&current_compatibility)?;
            fresh_baseline
        };
        let (_fresh_baseline, execution) = Box::pin(execute_configured_case_attempt(
            config,
            configured_probe,
            configured_quiescence,
            configured_snapshot,
            current_baseline,
            recorded.trace().planned_case(),
            replay_id,
            &format!("{}_{}", recorded.case_id(), attempt_id),
            journal_path,
            cancellation,
        ))
        .await
        .map_err(ConfiguredReplayError::Case)?;
        let trace_matches_source = recorded.trace().matches_replay_authority(execution.trace());
        let result = attempt_result(
            recorded.expected_failure(),
            execution
                .invariants()
                .iter()
                .map(|outcome| (outcome.identity(), outcome.violated())),
        );
        let attempt_artifact = replay_attempt_artifact(
            attempt,
            recorded.case_id(),
            &execution,
            &result,
            recorded.expected_failure(),
            trace_matches_source,
        );
        artifacts
            .write_json(
                format!("attempts/{attempt_id}/trace.json"),
                execution.trace(),
            )
            .map_err(ConfiguredReplayError::EvidenceArtifact)?;
        artifacts
            .write_json(
                format!("attempts/{attempt_id}/result.json"),
                &attempt_artifact,
            )
            .map_err(ConfiguredReplayError::EvidenceArtifact)?;
        attempt_artifacts.push(attempt_artifact);
        if !trace_matches_source {
            return Err(ConfiguredReplayError::TraceDiverged { attempt });
        }
        attempt_results.push(result);
    }
    Ok(())
}

pub(crate) async fn attest_replay_boundary(
    config: &ResolvedConfig,
    configured_probe: &crate::postgres::probe::ConfiguredSqlProbe,
    configured_quiescence: &crate::postgres::quiescence::ConfiguredQuiescence,
    configured_snapshot: &crate::postgres::snapshot::ConfiguredSnapshot,
) -> Result<(ConfiguredBaselineSession, RunCompatibilityV1), ConfiguredReplayError> {
    let compose = collect_compose_facts(config).await?;
    let baseline = ConfiguredBaselineSession::attest(config).await?;
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
    Ok((baseline, compatibility))
}

fn replay_attempt_artifact(
    attempt: usize,
    case_id: &str,
    execution: &ConfiguredCaseExecution,
    result: &AttemptResult,
    expected: &FailureIdentity,
    trace_matches_source: bool,
) -> ReplayAttemptArtifact {
    let (verdict, failure_identity) = match result {
        AttemptResult::Held => (ReplayAttemptVerdict::Held, None),
        AttemptResult::Violation(identity) if identity == expected => (
            ReplayAttemptVerdict::ExpectedViolation,
            Some(identity.into()),
        ),
        AttemptResult::Violation(identity) => {
            (ReplayAttemptVerdict::OtherViolation, Some(identity.into()))
        }
        AttemptResult::Inconclusive => unreachable!("only completed oracle attempts are retained"),
    };
    ReplayAttemptArtifact {
        schema_version: 1,
        attempt,
        case_id: case_id.to_owned(),
        trace_matches_source,
        before_database_oid: execution.reset().before_database_oid(),
        after_database_oid: execution.reset().after_database_oid(),
        before_marker_uuid: execution.reset().before_marker_uuid().to_owned(),
        after_marker_uuid: execution.reset().after_marker_uuid().to_owned(),
        executed_action_count: execution.trace().action_count(),
        journal_record_count: execution.journal_record_count(),
        journal_last_record_hash: execution.journal_last_record_hash().map(str::to_owned),
        provider_object_count: execution.provider_object_count(),
        verdict,
        failure_identity,
        invariants: execution
            .invariants()
            .iter()
            .map(replay_invariant_artifact)
            .collect(),
    }
}

fn replay_invariant_artifact(outcome: &ConfiguredInvariantOutcome) -> ReplayInvariantArtifact {
    ReplayInvariantArtifact {
        invariant_id: outcome.identity().invariant().as_str().to_owned(),
        checkpoint_id: outcome.identity().checkpoint().as_str().to_owned(),
        verdict: if outcome.violated() {
            RecordedVerdict::Violated
        } else {
            RecordedVerdict::Held
        },
        witness_count: outcome.witness_count(),
    }
}

fn completed_replay_report(
    summary: &ReplaySummary<'_>,
    source_artifact_path: &Path,
    config_path: &Path,
    case_number: u32,
) -> ArtifactReport {
    let (conclusion, outcome, stability, message) = match summary.classification {
        ConfiguredReplayClassification::Stable => (
            ReportConclusion::Counterexample,
            ReportCheckOutcome::Failed,
            format!(
                "Stable: the same failure identity reproduced {}/{} times.",
                summary.matching_failure_count, summary.attempt_count
            ),
            "stable counterexample reproduced on all three fresh baselines",
        ),
        ConfiguredReplayClassification::Reproducible => (
            ReportConclusion::Counterexample,
            ReportCheckOutcome::Failed,
            format!(
                "Reproducible: the same failure identity reproduced {}/{} times.",
                summary.matching_failure_count, summary.attempt_count
            ),
            "reproducible counterexample matched on two of three fresh baselines",
        ),
        ConfiguredReplayClassification::Inconclusive => (
            ReportConclusion::Inconclusive,
            ReportCheckOutcome::Skipped,
            format!(
                "Inconclusive: the same failure identity reproduced {}/{} times.",
                summary.matching_failure_count, summary.attempt_count
            ),
            "fewer than two fresh-baseline attempts matched the expected failure",
        ),
    };
    let mut report = ArtifactReport::new(
        summary.replay_id,
        ReportArtifactKind::Replay,
        conclusion,
        format!(
            "Exactly {} fresh-baseline attempts; no retry-count override.",
            summary.attempt_count
        ),
        stability,
    );
    report.add_failure(ReportFailure::new(
        &summary.expected_failure.invariant_id,
        &summary.expected_failure.checkpoint_id,
        None,
    ));
    report.add_check(ReportCheck::new(
        format!(
            "{} at {}",
            summary.expected_failure.invariant_id, summary.expected_failure.checkpoint_id
        ),
        outcome,
        message,
    ));
    report.add_fact(ReportFact::new("Source run", summary.source_run_id));
    report.add_fact(ReportFact::new("Case", summary.case_id));
    report.add_fact(ReportFact::new(
        "Matching attempts",
        format!(
            "{}/{}",
            summary.matching_failure_count, summary.attempt_count
        ),
    ));
    report.add_replay_command(ReplayCommand::configured(
        source_artifact_path,
        config_path,
        case_number,
    ));
    report
}

fn finalize_failed_replay(
    mut artifacts: RunArtifactStaging,
    cause: ConfiguredReplayError,
    replay_id: &str,
    source_run_id: &str,
    case_id: &str,
    attempts: &[ReplayAttemptArtifact],
) -> Result<ConfiguredReplayOutput, ConfiguredReplayError> {
    let failure_class = cause.failure_class();
    let failure_code = cause.failure_code();
    if let Err(artifact) = artifacts.write_json(
        "summary.json",
        &PartialReplaySummary {
            schema_version: 1,
            status: partial_status(failure_class),
            replay_id,
            source_run_id,
            case_id,
            failure_class,
            failure_code,
            completed_attempts: attempts.len(),
            attempts,
        },
    ) {
        return Err(ConfiguredReplayError::PartialFinalization {
            cause: Box::new(cause),
            artifact,
        });
    }
    let mut report = partial_artifact_report(
        replay_id,
        ReportArtifactKind::Replay,
        failure_class.into(),
        failure_code,
    );
    report.add_fact(ReportFact::new("Source run", source_run_id));
    report.add_fact(ReportFact::new("Case", case_id));
    report.add_fact(ReportFact::new(
        "Completed attempts",
        attempts.len().to_string(),
    ));
    if let Err(artifact) = write_report_bundle(&mut artifacts, &report) {
        return Err(ConfiguredReplayError::PartialFinalization {
            cause: Box::new(cause),
            artifact,
        });
    }
    let artifact_path = match artifacts.finalize_partial_v2(
        partial_artifact_class(failure_class),
        failure_code,
        vec![
            ArtifactAuthority::replay_source(),
            ArtifactAuthority::original_trace(),
        ],
    ) {
        Ok(path) => path,
        Err(artifact) => {
            return Err(ConfiguredReplayError::PartialFinalization {
                cause: Box::new(cause),
                artifact,
            });
        }
    };
    Err(ConfiguredReplayError::RunFailed {
        cause: Box::new(cause),
        artifact_path,
    })
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

fn load_recorded_case(
    artifact: &VerifiedRunArtifact,
    options: ConfiguredReplayOptions,
) -> Result<RecordedConfiguredCase, ConfiguredReplayArtifactError> {
    if artifact
        .artifact_kind()
        .is_some_and(|kind| kind != ArtifactKind::Campaign)
        || artifact
            .artifact_result()
            .is_some_and(|result| result != ArtifactResult::Counterexample)
    {
        return Err(ConfiguredReplayArtifactError::InvalidCampaignSummary);
    }
    let campaign_bytes = artifact.read_indexed_bytes(std::path::Path::new("campaign-plan.json"))?;
    let campaign: CampaignPlan = serde_json::from_slice(&campaign_bytes).map_err(|source| {
        ConfiguredReplayArtifactError::Decode {
            path: PathBuf::from("campaign-plan.json"),
            source,
        }
    })?;
    let selected = campaign
        .cases()
        .iter()
        .find(|case| case.id() == options.case_id())
        .ok_or(ConfiguredReplayArtifactError::CaseNotFound {
            case_number: options.case_number(),
        })?;
    let case_id = format!("case_{:04}", selected.id().value());
    let trace_path = PathBuf::from(format!("cases/{case_id}/trace.json"));
    let result_path = PathBuf::from(format!("cases/{case_id}/result.json"));
    let trace: CompiledCaseTrace =
        serde_json::from_slice(&artifact.read_indexed_bytes(&trace_path)?).map_err(|source| {
            ConfiguredReplayArtifactError::Decode {
                path: trace_path,
                source,
            }
        })?;
    let result: RecordedCaseResult =
        serde_json::from_slice(&artifact.read_indexed_bytes(&result_path)?).map_err(|source| {
            ConfiguredReplayArtifactError::Decode {
                path: result_path,
                source,
            }
        })?;
    let summary_path = PathBuf::from("summary.json");
    let summary: RecordedCampaignSummary =
        serde_json::from_slice(&artifact.read_indexed_bytes(&summary_path)?).map_err(|source| {
            ConfiguredReplayArtifactError::Decode {
                path: summary_path,
                source,
            }
        })?;

    validate_summary(artifact, &campaign, &summary)?;
    validate_selected_case(&case_id, selected.plan(), &trace, &result)?;
    let summary_result = summary
        .cases
        .iter()
        .find(|candidate| candidate.case_id == case_id)
        .ok_or(ConfiguredReplayArtifactError::InvalidCampaignSummary)?;
    if summary_result != &result {
        return Err(ConfiguredReplayArtifactError::InvalidCampaignSummary);
    }
    let expected_failure = select_expected_failure(&result.invariants)?;
    Ok(RecordedConfiguredCase {
        case_id,
        trace,
        expected_failure,
    })
}

pub(crate) fn load_verified_configured_replay_source(
    artifact: &VerifiedRunArtifact,
) -> Result<VerifiedConfiguredReplaySource, ConfiguredReplayArtifactError> {
    if artifact
        .artifact_kind()
        .is_some_and(|kind| kind != ArtifactKind::Replay)
        || artifact
            .artifact_result()
            .is_some_and(|result| result != ArtifactResult::Counterexample)
    {
        return Err(ConfiguredReplayArtifactError::InvalidReplaySummary);
    }
    let source_path = PathBuf::from("source.json");
    let source: ReplaySourceDocument = decode_indexed(artifact, &source_path)?;
    let summary_path = PathBuf::from("summary.json");
    let summary: ReplaySummaryDocument = decode_indexed(artifact, &summary_path)?;
    let original_path = PathBuf::from("trace.original.json");
    let original_trace_bytes = artifact.read_indexed_bytes(&original_path)?;
    let original_trace: CompiledCaseTrace =
        serde_json::from_slice(&original_trace_bytes).map_err(|source| {
            ConfiguredReplayArtifactError::Decode {
                path: original_path,
                source,
            }
        })?;
    let expected_failure = source.expected_failure.to_identity()?;

    if source.schema_version != 1
        || !valid_run_label(&source.source_run_id)
        || !valid_case_label(&source.case_id)
        || summary.schema_version != 1
        || summary.status != "configured_replay_complete"
        || summary.replay_id != artifact.run_id()
        || summary.source_run_id != source.source_run_id
        || summary.case_id != source.case_id
        || summary.expected_failure != source.expected_failure
        || summary.attempt_count != ATTEMPT_COUNT
        || summary.attempts.len() != ATTEMPT_COUNT
    {
        return Err(ConfiguredReplayArtifactError::InvalidReplaySummary);
    }

    let mut results = Vec::with_capacity(ATTEMPT_COUNT);
    for (index, attempt) in summary.attempts.iter().enumerate() {
        let attempt_number = index + 1;
        let attempt_id = format!("attempt_{attempt_number:04}");
        let result_path = PathBuf::from(format!("attempts/{attempt_id}/result.json"));
        let indexed_result: ReplayAttemptArtifact = decode_indexed(artifact, &result_path)?;
        if &indexed_result != attempt {
            return Err(ConfiguredReplayArtifactError::InvalidReplayAttempt {
                attempt: attempt_number,
            });
        }
        let trace_path = PathBuf::from(format!("attempts/{attempt_id}/trace.json"));
        let replay_trace: CompiledCaseTrace = decode_indexed(artifact, &trace_path)?;
        if !original_trace.matches_replay_authority(&replay_trace) {
            return Err(ConfiguredReplayArtifactError::InvalidReplayAttempt {
                attempt: attempt_number,
            });
        }
        results.push(validate_replay_attempt(
            attempt,
            attempt_number,
            &source.case_id,
            &expected_failure,
            original_trace.action_count(),
        )?);
    }
    let attempts: [AttemptResult; ATTEMPT_COUNT] = results
        .try_into()
        .map_err(|_| ConfiguredReplayArtifactError::InvalidReplaySummary)?;
    let classification: ConfiguredReplayClassification =
        classify_reproduction(&expected_failure, &attempts).into();
    let matching_failure_count = attempts
        .iter()
        .filter(|attempt| {
            matches!(attempt, AttemptResult::Violation(identity) if identity == &expected_failure)
        })
        .count();
    if summary.classification != classification
        || summary.matching_failure_count != matching_failure_count
        || matching_failure_count < 2
    {
        return Err(ConfiguredReplayArtifactError::ReplayIsNotReproducible);
    }

    Ok(VerifiedConfiguredReplaySource {
        replay_id: artifact.run_id().to_owned(),
        source_run_id: source.source_run_id,
        case_id: source.case_id,
        expected_failure,
        original_trace,
        original_trace_bytes,
    })
}

fn decode_indexed<T: for<'de> Deserialize<'de>>(
    artifact: &VerifiedRunArtifact,
    path: &Path,
) -> Result<T, ConfiguredReplayArtifactError> {
    serde_json::from_slice(&artifact.read_indexed_bytes(path)?).map_err(|source| {
        ConfiguredReplayArtifactError::Decode {
            path: path.to_owned(),
            source,
        }
    })
}

fn validate_replay_attempt(
    attempt: &ReplayAttemptArtifact,
    attempt_number: usize,
    case_id: &str,
    expected_failure: &FailureIdentity,
    action_count: usize,
) -> Result<AttemptResult, ConfiguredReplayArtifactError> {
    if attempt.schema_version != 1
        || attempt.attempt != attempt_number
        || attempt.case_id != case_id
        || !attempt.trace_matches_source
        || attempt.before_database_oid == 0
        || attempt.after_database_oid == 0
        || attempt.before_database_oid == attempt.after_database_oid
        || attempt.before_marker_uuid == attempt.after_marker_uuid
        || uuid::Uuid::parse_str(&attempt.before_marker_uuid).is_err()
        || uuid::Uuid::parse_str(&attempt.after_marker_uuid).is_err()
        || attempt.executed_action_count != action_count
        || attempt.journal_record_count == 0
        || !attempt
            .journal_last_record_hash
            .as_deref()
            .is_some_and(valid_digest)
    {
        return Err(ConfiguredReplayArtifactError::InvalidReplayAttempt {
            attempt: attempt_number,
        });
    }
    validate_replay_invariants(&attempt.invariants, attempt_number)?;
    let outcomes = attempt
        .invariants
        .iter()
        .map(|invariant| {
            (
                FailureIdentity::new(
                    InvariantId::new(&invariant.invariant_id)
                        .expect("replay invariants were validated"),
                    CheckpointId::new(&invariant.checkpoint_id)
                        .expect("replay invariants were validated"),
                ),
                invariant.verdict == RecordedVerdict::Violated,
            )
        })
        .collect::<Vec<_>>();
    let result = attempt_result(
        expected_failure,
        outcomes
            .iter()
            .map(|(identity, violated)| (identity, *violated)),
    );
    let declared_identity = attempt
        .failure_identity
        .as_ref()
        .map(FailureIdentityArtifact::to_identity)
        .transpose()?;
    let coherent = match (&attempt.verdict, &declared_identity, &result) {
        (ReplayAttemptVerdict::Held, None, AttemptResult::Held) => true,
        (
            ReplayAttemptVerdict::ExpectedViolation,
            Some(declared),
            AttemptResult::Violation(actual),
        ) => declared == expected_failure && actual == expected_failure,
        (
            ReplayAttemptVerdict::OtherViolation,
            Some(declared),
            AttemptResult::Violation(actual),
        ) => declared == actual && actual != expected_failure,
        _ => false,
    };
    if !coherent {
        return Err(ConfiguredReplayArtifactError::InvalidReplayAttempt {
            attempt: attempt_number,
        });
    }
    Ok(result)
}

fn validate_replay_invariants(
    invariants: &[ReplayInvariantArtifact],
    attempt: usize,
) -> Result<(), ConfiguredReplayArtifactError> {
    if invariants.len() != 5 {
        return Err(ConfiguredReplayArtifactError::InvalidReplayAttempt { attempt });
    }
    let mut identities = BTreeSet::new();
    for invariant in invariants {
        if InvariantId::new(&invariant.invariant_id).is_err()
            || CheckpointId::new(&invariant.checkpoint_id).is_err()
            || !identities.insert((
                invariant.invariant_id.as_str(),
                invariant.checkpoint_id.as_str(),
            ))
            || matches!(invariant.verdict, RecordedVerdict::Held) != (invariant.witness_count == 0)
        {
            return Err(ConfiguredReplayArtifactError::InvalidReplayAttempt { attempt });
        }
    }
    Ok(())
}

fn valid_run_label(value: &str) -> bool {
    value.starts_with("run_")
        && (5..=80).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_case_label(value: &str) -> bool {
    value.len() == 9
        && value.starts_with("case_")
        && value.as_bytes()[5..].iter().all(u8::is_ascii_digit)
}

fn validate_summary(
    artifact: &VerifiedRunArtifact,
    campaign: &CampaignPlan,
    summary: &RecordedCampaignSummary,
) -> Result<(), ConfiguredReplayArtifactError> {
    let case_count = campaign.cases().len();
    if summary.schema_version != 1
        || summary.run_id != artifact.run_id()
        || summary.configured_cases != case_count
        || summary.completed_cases != case_count
        || summary.cases.len() != case_count
    {
        return Err(ConfiguredReplayArtifactError::InvalidCampaignSummary);
    }
    let mut any_violation = false;
    for (planned, result) in campaign.cases().iter().zip(&summary.cases) {
        let expected_case_id = format!("case_{:04}", planned.id().value());
        validate_result_header(&expected_case_id, planned.plan(), result)?;
        validate_recorded_invariants(&result.invariants)?;
        any_violation |= result
            .invariants
            .iter()
            .any(|invariant| invariant.verdict == RecordedVerdict::Violated);
    }
    let expected_verdict = if any_violation {
        RecordedVerdict::Violated
    } else {
        RecordedVerdict::Held
    };
    if summary.verdict != expected_verdict {
        return Err(ConfiguredReplayArtifactError::InvalidCampaignSummary);
    }
    Ok(())
}

fn validate_selected_case(
    case_id: &str,
    planned_case: &PlannedCase,
    trace: &CompiledCaseTrace,
    result: &RecordedCaseResult,
) -> Result<(), ConfiguredReplayArtifactError> {
    validate_result_header(case_id, planned_case, result)?;
    validate_recorded_invariants(&result.invariants)?;
    if trace.planned_case() != planned_case || trace.action_count() != result.executed_action_count
    {
        return Err(ConfiguredReplayArtifactError::InvalidCaseResult);
    }
    Ok(())
}

fn validate_result_header(
    case_id: &str,
    planned_case: &PlannedCase,
    result: &RecordedCaseResult,
) -> Result<(), ConfiguredReplayArtifactError> {
    let valid_hash = result
        .journal_last_record_hash
        .as_deref()
        .is_some_and(valid_digest);
    if result.schema_version != 1
        || result.case_id != case_id
        || result.seed != planned_case.seed().value()
        || result.planned_action_count != planned_case.actions().len()
        || result.executed_action_count != result.planned_action_count
        || result.journal_record_count == 0
        || !valid_hash
        || result.before_database_oid == 0
        || result.after_database_oid == 0
        || result.before_database_oid == result.after_database_oid
        || result.before_marker_uuid == result.after_marker_uuid
        || uuid::Uuid::parse_str(&result.before_marker_uuid).is_err()
        || uuid::Uuid::parse_str(&result.after_marker_uuid).is_err()
    {
        return Err(ConfiguredReplayArtifactError::InvalidCaseResult);
    }
    Ok(())
}

fn validate_recorded_invariants(
    invariants: &[RecordedInvariant],
) -> Result<(), ConfiguredReplayArtifactError> {
    if invariants.len() != 5 {
        return Err(ConfiguredReplayArtifactError::InvalidCaseResult);
    }
    let mut identities = BTreeSet::new();
    for invariant in invariants {
        InvariantId::new(&invariant.invariant_id)
            .map_err(|_| ConfiguredReplayArtifactError::InvalidCaseResult)?;
        CheckpointId::new(&invariant.checkpoint_id)
            .map_err(|_| ConfiguredReplayArtifactError::InvalidCaseResult)?;
        if !identities.insert((
            invariant.invariant_id.as_str(),
            invariant.checkpoint_id.as_str(),
        )) || matches!(invariant.verdict, RecordedVerdict::Held)
            != (invariant.witness_count == 0)
        {
            return Err(ConfiguredReplayArtifactError::InvalidCaseResult);
        }
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn select_expected_failure(
    invariants: &[RecordedInvariant],
) -> Result<FailureIdentity, ConfiguredReplayArtifactError> {
    validate_recorded_invariants(invariants)?;
    let mut violations = Vec::new();
    for invariant in invariants {
        let identity = FailureIdentity::new(
            InvariantId::new(&invariant.invariant_id)
                .map_err(|_| ConfiguredReplayArtifactError::InvalidCaseResult)?,
            CheckpointId::new(&invariant.checkpoint_id)
                .map_err(|_| ConfiguredReplayArtifactError::InvalidCaseResult)?,
        );
        let key = (
            invariant.invariant_id.as_str(),
            invariant.checkpoint_id.as_str(),
        );
        if invariant.verdict == RecordedVerdict::Violated {
            violations.push((key.0, key.1, identity));
        }
    }
    violations
        .into_iter()
        .min_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)))
        .map(|(_, _, identity)| identity)
        .ok_or(ConfiguredReplayArtifactError::CaseDidNotViolate)
}

pub(crate) fn attempt_result<'a>(
    expected: &FailureIdentity,
    outcomes: impl IntoIterator<Item = (&'a FailureIdentity, bool)>,
) -> AttemptResult {
    let mut violations = outcomes
        .into_iter()
        .filter_map(|(identity, violated)| violated.then_some(identity))
        .collect::<Vec<_>>();
    if violations.contains(&expected) {
        return AttemptResult::Violation(expected.clone());
    }
    violations.sort_by(|left, right| {
        (left.invariant().as_str(), left.checkpoint().as_str())
            .cmp(&(right.invariant().as_str(), right.checkpoint().as_str()))
    });
    violations.first().map_or(AttemptResult::Held, |identity| {
        AttemptResult::Violation((*identity).clone())
    })
}

/// Invalid or incoherent source evidence selected for configured replay.
#[derive(Debug, Error)]
pub enum ConfiguredReplayArtifactError {
    #[error("configured replay artifact verification failed: {0}")]
    Artifact(#[from] ArtifactError),
    #[error("configured replay artifact document {path} is invalid: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("configured replay case {case_number} is absent from the recorded campaign")]
    CaseNotFound { case_number: u32 },
    #[error("configured replay campaign summary is incoherent")]
    InvalidCampaignSummary,
    #[error("configured replay case result is invalid")]
    InvalidCaseResult,
    #[error("configured replay requires a recorded invariant violation")]
    CaseDidNotViolate,
    #[error("configured replay summary is invalid or incoherent")]
    InvalidReplaySummary,
    #[error("configured replay attempt {attempt} is invalid or incoherent")]
    InvalidReplayAttempt { attempt: usize },
    #[error("configured replay source is not reproducible at the expected identity")]
    ReplayIsNotReproducible,
}

/// Failure to prepare, execute, recover, or persist configured replay.
#[derive(Debug, Error)]
pub enum ConfiguredReplayError {
    #[error("configured replay options are invalid: {0}")]
    Options(#[from] ConfiguredReplayOptionsError),
    #[error("configured replay source artifact is invalid: {0}")]
    SourceArtifact(#[from] ConfiguredReplayArtifactError),
    #[error("configured replay configuration failed: {0}")]
    Config(#[from] ConfigError),
    #[error("configured replay repository provenance failed: {0}")]
    Repository(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured replay SQL probe could not be prepared: {0}")]
    SqlProbe(#[from] ConfiguredSqlProbeError),
    #[error("configured replay quiescence query could not be prepared: {0}")]
    Quiescence(#[from] ConfiguredQuiescenceError),
    #[error("configured replay invariant suite could not be prepared: {0}")]
    Snapshot(#[from] ConfiguredSnapshotError),
    #[error("configured replay case preflight failed: {0}")]
    CasePreflight(#[from] ReferenceCaseRunError),
    #[error("configured replay report path preflight failed: {0}")]
    ReportPath(#[source] ArtifactError),
    #[error("configured replay Compose preflight failed: {0}")]
    Compose(#[from] DoctorError),
    #[error("configured replay baseline attestation failed: {0}")]
    Baseline(#[from] BaselineError),
    #[error("configured replay current compatibility capture failed: {0}")]
    CompatibilityCapture(#[from] CompatibilityCaptureError),
    #[error("configured replay compatibility gate failed: {0}")]
    Compatibility(#[from] CompatibilityError),
    #[error("configured replay application process attestation failed: {0}")]
    Process(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured replay Compose project exclusion failed: {0}")]
    ProjectLock(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured replay evidence failed: {0}")]
    EvidenceArtifact(#[source] ArtifactError),
    #[error("configured replay case execution failed: {0}")]
    Case(#[source] ConfiguredCampaignError),
    #[error("configured replay attempt {attempt} diverged from the recorded trace authority")]
    TraceDiverged { attempt: usize },
    #[error("configured replay completed an invalid number of attempts")]
    AttemptAccounting,
    #[error("configured replay was interrupted")]
    Interrupted,
    #[error("configured replay failed; partial evidence retained at {artifact_path}: {cause}")]
    RunFailed {
        #[source]
        cause: Box<ConfiguredReplayError>,
        artifact_path: PathBuf,
    },
    #[error("configured replay partial-evidence finalization failed: {artifact}")]
    PartialFinalization {
        #[source]
        cause: Box<ConfiguredReplayError>,
        artifact: ArtifactError,
    },
}

impl ConfiguredReplayError {
    #[must_use]
    pub fn failure_class(&self) -> ConfiguredCampaignFailureClass {
        match self {
            Self::Options(_)
            | Self::SourceArtifact(_)
            | Self::Config(_)
            | Self::Repository(_)
            | Self::SqlProbe(_)
            | Self::Quiescence(_)
            | Self::Snapshot(_)
            | Self::CasePreflight(_)
            | Self::ReportPath(_)
            | Self::Compatibility(_) => ConfiguredCampaignFailureClass::Configuration,
            Self::Compose(error) if !error.is_infrastructure_failure() => {
                ConfiguredCampaignFailureClass::Configuration
            }
            Self::CompatibilityCapture(error) if !error.is_infrastructure_failure() => {
                ConfiguredCampaignFailureClass::Configuration
            }
            Self::Baseline(error) if !error.is_infrastructure_failure() => {
                ConfiguredCampaignFailureClass::Configuration
            }
            Self::Case(error) => error.failure_class(),
            Self::TraceDiverged { .. } | Self::AttemptAccounting => {
                ConfiguredCampaignFailureClass::Inconclusive
            }
            Self::Interrupted => ConfiguredCampaignFailureClass::Interrupted,
            Self::RunFailed { cause, .. } | Self::PartialFinalization { cause, .. } => {
                cause.failure_class()
            }
            Self::Compose(_)
            | Self::Baseline(_)
            | Self::CompatibilityCapture(_)
            | Self::Process(_)
            | Self::ProjectLock(_)
            | Self::EvidenceArtifact(_) => ConfiguredCampaignFailureClass::Infrastructure,
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
            Self::SourceArtifact(_) => "invalid_source_artifact",
            Self::Config(_) => "invalid_config",
            Self::Repository(_) => "repository_provenance",
            Self::SqlProbe(_) => "invalid_sql_probe",
            Self::Quiescence(_) => "invalid_quiescence",
            Self::Snapshot(_) => "invalid_snapshot",
            Self::CasePreflight(_) => "case_preflight",
            Self::ReportPath(_) => "invalid_report_path",
            Self::Compose(_) => "compose_preflight",
            Self::Baseline(_) => "baseline_failure",
            Self::CompatibilityCapture(_) => "compatibility_capture",
            Self::Compatibility(_) => "compatibility_mismatch",
            Self::Process(_) => "process_failure",
            Self::ProjectLock(_) => "project_locked",
            Self::EvidenceArtifact(_) => "artifact_failure",
            Self::Case(error) => error.failure_code(),
            Self::TraceDiverged { .. } => "trace_diverged",
            Self::AttemptAccounting => "attempt_accounting",
            Self::Interrupted => "interrupted",
            Self::RunFailed { cause, .. } => cause.failure_code(),
            Self::PartialFinalization { .. } => "partial_finalization_failed",
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

impl From<ConfiguredProcessError> for ConfiguredReplayError {
    fn from(error: ConfiguredProcessError) -> Self {
        Self::Process(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tiv_core::result::{
        AttemptResult, CheckpointId, FailureIdentity, InvariantId, ReproductionClass,
    };
    use tiv_core::{
        decision::Seed,
        plan::{ActionBudget, CampaignPlanner, CampaignSpec, CaseCount},
        trace::{CaseCapturedValue, CaseOutputSlot, CaseTraceMaterializer},
    };
    use uuid::Uuid;

    use super::{
        ConfiguredReplayArtifactError, ConfiguredReplayClassification, ConfiguredReplayOptions,
        FailureIdentityArtifact, RecordedInvariant, RecordedVerdict, ReplayAttemptArtifact,
        ReplayAttemptVerdict, ReplayInvariantArtifact, ReplaySourceArtifact, ReplaySummary,
        attempt_result, completed_replay_report, load_recorded_case,
        load_verified_configured_replay_source, select_expected_failure,
    };
    use crate::{
        artifacts::{RunArtifactStaging, verify_complete_run_artifact},
        compatibility::RunCompatibilityV1,
        reports::{ReportCheckOutcome, ReportConclusion},
    };

    fn identity(invariant: &str) -> FailureIdentity {
        FailureIdentity::new(
            InvariantId::new(invariant).unwrap(),
            CheckpointId::new("checkout-quiescent").unwrap(),
        )
    }

    fn recorded(
        invariant_id: &str,
        verdict: RecordedVerdict,
        witness_count: usize,
    ) -> RecordedInvariant {
        RecordedInvariant {
            invariant_id: invariant_id.to_owned(),
            checkpoint_id: "checkout-quiescent".to_owned(),
            verdict,
            witness_count,
        }
    }

    #[test]
    fn replay_report_maps_every_real_producer_classification() {
        let fixtures = [
            (
                ConfiguredReplayClassification::Stable,
                3,
                ReportConclusion::Counterexample,
                ReportCheckOutcome::Failed,
            ),
            (
                ConfiguredReplayClassification::Reproducible,
                2,
                ReportConclusion::Counterexample,
                ReportCheckOutcome::Failed,
            ),
            (
                ConfiguredReplayClassification::Inconclusive,
                1,
                ReportConclusion::Inconclusive,
                ReportCheckOutcome::Skipped,
            ),
        ];

        for (classification, matching_failure_count, conclusion, outcome) in fixtures {
            let summary = ReplaySummary {
                schema_version: 1,
                status: "configured_replay_complete",
                replay_id: "run_replay_report",
                source_run_id: "run_source",
                case_id: "case_0001",
                expected_failure: FailureIdentityArtifact {
                    invariant_id: "provider_object_uniqueness".to_owned(),
                    checkpoint_id: "checkout_complete".to_owned(),
                },
                attempt_count: 3,
                matching_failure_count,
                classification,
                attempts: &[],
            };

            let report = completed_replay_report(
                &summary,
                Path::new("/tmp/source"),
                Path::new("/tmp/tiv.toml"),
                1,
            );

            assert_eq!(report.conclusion(), conclusion);
            assert_eq!(report.check_outcomes(), vec![outcome]);
            assert_eq!(report.replay_command_count(), 1);
        }
    }

    #[test]
    fn configured_replay_is_fixed_at_three_attempts_and_one_bounded_case() {
        let options = ConfiguredReplayOptions::new(7).unwrap();

        assert_eq!(options.case_number(), 7);
        assert_eq!(options.attempt_count(), 3);
        assert!(ConfiguredReplayOptions::new(0).is_err());
        assert!(ConfiguredReplayOptions::new(501).is_err());
    }

    #[test]
    fn source_failure_selection_is_deterministic_and_rejects_held_cases() {
        let invariants = vec![
            recorded("z-last", RecordedVerdict::Violated, 2),
            recorded("a-first", RecordedVerdict::Violated, 1),
            recorded("held-1", RecordedVerdict::Held, 0),
            recorded("held-2", RecordedVerdict::Held, 0),
            recorded("held-3", RecordedVerdict::Held, 0),
        ];

        assert_eq!(
            select_expected_failure(&invariants).unwrap(),
            identity("a-first")
        );
        let held = [
            recorded("held-1", RecordedVerdict::Held, 0),
            recorded("held-2", RecordedVerdict::Held, 0),
            recorded("held-3", RecordedVerdict::Held, 0),
            recorded("held-4", RecordedVerdict::Held, 0),
            recorded("held-5", RecordedVerdict::Held, 0),
        ];
        assert!(select_expected_failure(&held).is_err());
        let mut invalid = held;
        invalid[0].witness_count = 1;
        assert!(select_expected_failure(&invalid).is_err());
    }

    #[test]
    fn completed_attempt_prefers_the_expected_identity_then_other_violations() {
        let expected = identity("expected");
        let other = identity("other");

        assert_eq!(
            attempt_result(&expected, [(&other, true), (&expected, true)]),
            AttemptResult::Violation(expected.clone())
        );
        assert_eq!(
            attempt_result(&expected, [(&other, true), (&expected, false)]),
            AttemptResult::Violation(other)
        );
        assert_eq!(
            attempt_result(&expected, [(&expected, false)]),
            AttemptResult::Held
        );
        let attempts = [
            AttemptResult::Violation(expected.clone()),
            AttemptResult::Violation(expected.clone()),
            AttemptResult::Held,
        ];
        assert_eq!(
            tiv_core::result::classify_reproduction(&expected, &attempts),
            ReproductionClass::Reproducible
        );
    }

    #[test]
    fn verified_run_loads_only_the_selected_coherent_violating_case() {
        let root = std::env::temp_dir().join(format!("tiv-replay-source-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let campaign = CampaignPlanner::compile(&CampaignSpec::payment_intent_v1(
            Seed::new(4),
            CaseCount::new(1).unwrap(),
            ActionBudget::new(40).unwrap(),
        ))
        .unwrap();
        let plan = campaign.cases()[0].plan();
        let mut payment_intent = 0_u32;
        let mut event = 0_u32;
        let mut gate = 0_u64;
        let captured = CaseTraceMaterializer::required_outputs(plan)
            .unwrap()
            .into_iter()
            .map(|output| {
                let value = match output.slot() {
                    CaseOutputSlot::PaymentIntentId => {
                        payment_intent += 1;
                        CaseCapturedValue::payment_intent_id(format!("pi_test_{payment_intent}"))
                            .unwrap()
                    }
                    CaseOutputSlot::EventId => {
                        event += 1;
                        CaseCapturedValue::event_id(format!("evt_test_{event}")).unwrap()
                    }
                    CaseOutputSlot::ProviderGateId => {
                        gate += 1;
                        CaseCapturedValue::provider_gate_id(gate).unwrap()
                    }
                };
                (output, value)
            });
        let trace = CaseTraceMaterializer::materialize(plan, captured).unwrap();
        let invariants = serde_json::json!([
            {"invariant_id": "provider-object-unique", "checkpoint_id": "checkout-quiescent", "verdict": "violated", "witness_count": 1},
            {"invariant_id": "event-process-at-most-once", "checkpoint_id": "checkout-quiescent", "verdict": "held", "witness_count": 0},
            {"invariant_id": "paid-order-amount-conservation", "checkpoint_id": "checkout-quiescent", "verdict": "held", "witness_count": 0},
            {"invariant_id": "terminal-success-monotonic", "checkpoint_id": "checkout-quiescent", "verdict": "held", "witness_count": 0},
            {"invariant_id": "balanced-ledger", "checkpoint_id": "checkout-quiescent", "verdict": "held", "witness_count": 0}
        ]);
        let result = serde_json::json!({
            "schema_version": 1,
            "case_id": "case_0001",
            "seed": plan.seed().value(),
            "planned_action_count": plan.actions().len(),
            "executed_action_count": trace.action_count(),
            "journal_record_count": trace.action_count() * 2,
            "journal_last_record_hash": "a".repeat(64),
            "before_database_oid": 100,
            "after_database_oid": 101,
            "before_marker_uuid": "00000000-0000-4000-8000-000000000001",
            "after_marker_uuid": "00000000-0000-4000-8000-000000000002",
            "provider_object_count": 2,
            "invariants": invariants
        });
        let mut staging =
            RunArtifactStaging::create(&root, &root.join(".tiv/runs"), "run_source").unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        staging.write_json("campaign-plan.json", &campaign).unwrap();
        staging
            .write_json("cases/case_0001/trace.json", &trace)
            .unwrap();
        staging
            .write_json("cases/case_0001/result.json", &result)
            .unwrap();
        staging
            .write_bytes("cases/case_0001/observations.ndjson", b"{}\n")
            .unwrap();
        staging
            .write_json(
                "summary.json",
                &serde_json::json!({
                    "schema_version": 1,
                    "run_id": "run_source",
                    "verdict": "violated",
                    "configured_cases": 1,
                    "completed_cases": 1,
                    "cases": [result]
                }),
            )
            .unwrap();
        let final_path = staging.finalize().unwrap();
        let verified = verify_complete_run_artifact(&final_path).unwrap();

        let selected =
            load_recorded_case(&verified, ConfiguredReplayOptions::new(1).unwrap()).unwrap();
        assert_eq!(selected.case_id(), "case_0001");
        assert_eq!(selected.trace(), &trace);
        assert_eq!(
            selected.expected_failure(),
            &identity("provider-object-unique")
        );
        assert!(matches!(
            load_recorded_case(&verified, ConfiguredReplayOptions::new(2).unwrap()),
            Err(ConfiguredReplayArtifactError::CaseNotFound { case_number: 2 })
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shrink_source_loader_requires_a_coherent_reproducible_replay() {
        let (root, final_path, trace) = replay_artifact(2);
        let verified = verify_complete_run_artifact(&final_path).unwrap();

        let source = load_verified_configured_replay_source(&verified).unwrap();

        assert_eq!(source.replay_id(), "run_replay_source");
        assert_eq!(source.source_run_id(), "run_campaign_source");
        assert_eq!(source.case_id(), "case_0001");
        assert_eq!(source.original_trace(), &trace);
        assert_eq!(
            source.expected_failure(),
            &identity("provider-object-unique")
        );
        assert_eq!(
            source.original_trace_bytes(),
            fs::read(final_path.join("trace.original.json")).unwrap()
        );
        fs::remove_dir_all(root).unwrap();

        let (root, final_path, _) = replay_artifact(1);
        let verified = verify_complete_run_artifact(&final_path).unwrap();
        assert!(matches!(
            load_verified_configured_replay_source(&verified),
            Err(ConfiguredReplayArtifactError::ReplayIsNotReproducible)
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[allow(clippy::too_many_lines)]
    fn replay_artifact(
        matching_failure_count: usize,
    ) -> (
        std::path::PathBuf,
        std::path::PathBuf,
        tiv_core::trace::CompiledCaseTrace,
    ) {
        let root = std::env::temp_dir().join(format!("tiv-shrink-source-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let campaign = CampaignPlanner::compile(&CampaignSpec::payment_intent_v1(
            Seed::new(4),
            CaseCount::new(1).unwrap(),
            ActionBudget::new(40).unwrap(),
        ))
        .unwrap();
        let plan = campaign.cases()[0].plan();
        let mut ordinal = 0_u64;
        let captures = CaseTraceMaterializer::required_outputs(plan)
            .unwrap()
            .into_iter()
            .map(|output| {
                ordinal += 1;
                let value = match output.slot() {
                    CaseOutputSlot::PaymentIntentId => {
                        CaseCapturedValue::payment_intent_id(format!("pi_replay_{ordinal}"))
                            .unwrap()
                    }
                    CaseOutputSlot::EventId => {
                        CaseCapturedValue::event_id(format!("evt_replay_{ordinal}")).unwrap()
                    }
                    CaseOutputSlot::ProviderGateId => {
                        CaseCapturedValue::provider_gate_id(ordinal).unwrap()
                    }
                };
                (output, value)
            });
        let trace = CaseTraceMaterializer::materialize(plan, captures).unwrap();
        let expected = identity("provider-object-unique");
        let mut attempts = Vec::new();
        for attempt in 1..=3 {
            let matches = attempt <= matching_failure_count;
            let invariants = vec![
                ReplayInvariantArtifact {
                    invariant_id: "provider-object-unique".to_owned(),
                    checkpoint_id: "checkout-quiescent".to_owned(),
                    verdict: if matches {
                        RecordedVerdict::Violated
                    } else {
                        RecordedVerdict::Held
                    },
                    witness_count: usize::from(matches),
                },
                ReplayInvariantArtifact {
                    invariant_id: "event-process-at-most-once".to_owned(),
                    checkpoint_id: "checkout-quiescent".to_owned(),
                    verdict: RecordedVerdict::Held,
                    witness_count: 0,
                },
                ReplayInvariantArtifact {
                    invariant_id: "paid-order-amount-conservation".to_owned(),
                    checkpoint_id: "checkout-quiescent".to_owned(),
                    verdict: RecordedVerdict::Held,
                    witness_count: 0,
                },
                ReplayInvariantArtifact {
                    invariant_id: "terminal-success-monotonic".to_owned(),
                    checkpoint_id: "checkout-quiescent".to_owned(),
                    verdict: RecordedVerdict::Held,
                    witness_count: 0,
                },
                ReplayInvariantArtifact {
                    invariant_id: "balanced-ledger".to_owned(),
                    checkpoint_id: "checkout-quiescent".to_owned(),
                    verdict: RecordedVerdict::Held,
                    witness_count: 0,
                },
            ];
            attempts.push(ReplayAttemptArtifact {
                schema_version: 1,
                attempt,
                case_id: "case_0001".to_owned(),
                trace_matches_source: true,
                before_database_oid: u32::try_from(100 + attempt * 2).unwrap(),
                after_database_oid: u32::try_from(101 + attempt * 2).unwrap(),
                before_marker_uuid: format!("00000000-0000-4000-8000-{attempt:012}"),
                after_marker_uuid: format!("10000000-0000-4000-8000-{attempt:012}"),
                executed_action_count: trace.action_count(),
                journal_record_count: trace.action_count() * 2,
                journal_last_record_hash: Some("a".repeat(64)),
                provider_object_count: 1,
                verdict: if matches {
                    ReplayAttemptVerdict::ExpectedViolation
                } else {
                    ReplayAttemptVerdict::Held
                },
                failure_identity: matches.then(|| (&expected).into()),
                invariants,
            });
        }
        let classification = match matching_failure_count {
            3 => ConfiguredReplayClassification::Stable,
            2 => ConfiguredReplayClassification::Reproducible,
            _ => ConfiguredReplayClassification::Inconclusive,
        };
        let mut staging =
            RunArtifactStaging::create(&root, &root.join(".tiv/runs"), "run_replay_source")
                .unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        staging.write_json("trace.original.json", &trace).unwrap();
        staging
            .write_json(
                "source.json",
                &ReplaySourceArtifact {
                    schema_version: 1,
                    source_run_id: "run_campaign_source",
                    case_id: "case_0001",
                    expected_failure: (&expected).into(),
                },
            )
            .unwrap();
        for (index, attempt) in attempts.iter().enumerate() {
            let attempt_id = format!("attempt_{:04}", index + 1);
            staging
                .write_json(format!("attempts/{attempt_id}/trace.json"), &trace)
                .unwrap();
            staging
                .write_json(format!("attempts/{attempt_id}/result.json"), attempt)
                .unwrap();
            staging
                .write_bytes(
                    format!("attempts/{attempt_id}/observations.ndjson"),
                    b"{}\n",
                )
                .unwrap();
        }
        staging
            .write_json(
                "summary.json",
                &ReplaySummary {
                    schema_version: 1,
                    status: "configured_replay_complete",
                    replay_id: "run_replay_source",
                    source_run_id: "run_campaign_source",
                    case_id: "case_0001",
                    expected_failure: FailureIdentityArtifact::from(&expected),
                    attempt_count: 3,
                    matching_failure_count,
                    classification,
                    attempts: &attempts,
                },
            )
            .unwrap();
        let final_path = staging.finalize().unwrap();
        (root, final_path, trace)
    }

    fn compatibility_fixture() -> RunCompatibilityV1 {
        RunCompatibilityV1::from_json(
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "tool": {
                    "package_version": "0.0.0",
                    "executable_digest": "a".repeat(64),
                    "trace_schema": tiv_core::trace::TRACE_SCHEMA_VERSION,
                    "case_trace_schema": tiv_core::trace::CASE_TRACE_SCHEMA_VERSION,
                    "fixture_control_protocol": 1
                },
                "platform_os": "linux",
                "platform_arch": "x86_64",
                "config_digest": "a".repeat(64),
                "compose": {
                    "version": "5.4.0",
                    "services": ["postgres", "reference-app", "stripe-fixture"],
                    "resolved_redacted_hash": "a".repeat(64)
                },
                "sources": [{
                    "kind": "invariant",
                    "id": "provider-object-unique",
                    "digest": "a".repeat(64)
                }],
                "baseline": {
                    "server_fingerprint": "postgres-system-id:123456789",
                    "endpoint_port": 15432,
                    "database_name": "tiv_base_deadbeef",
                    "database_oid": 16384,
                    "owner_oid": 10,
                    "marker_uuid": "00000000-0000-4000-8000-000000000001",
                    "compose_project": "tiv-reference-app-spike",
                    "application_role": "tiv_app"
                },
                "services": [{
                    "service": "reference-app",
                    "compose_config_hash": "a".repeat(64),
                    "image_id": format!("sha256:{}", "a".repeat(64))
                }]
            }))
            .unwrap(),
        )
        .unwrap()
    }
}
