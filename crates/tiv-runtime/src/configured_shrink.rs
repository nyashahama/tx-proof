//! Bounded, compatibility-gated shrinking of a configured replay.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::Serialize;
use thiserror::Error;
use tiv_core::{
    result::{AttemptResult, FailureIdentity},
    shrink::{
        CandidateGenerator, CandidateIdentity, CandidateLimit, InvalidCandidateLimit,
        ShrinkCandidate, ShrinkComplexity, ShrinkError,
    },
    trace::{CompiledCaseTrace, CompiledShrinkTrace},
};

use crate::{
    artifacts::{
        ArtifactAuthority, ArtifactError, ArtifactKind, ArtifactResult, ManifestSeed,
        PartialRunClass, RunArtifactStaging, verify_complete_run_artifact,
    },
    baseline::ConfiguredBaselineSession,
    compatibility::{CompatibilityError, RunCompatibilityV1},
    config::{ConfigError, EnvironmentLookup, ResolvedConfig, load_resolved_config},
    configured_campaign::{
        ConfiguredCampaignError, ConfiguredCampaignFailureClass, ConfiguredInvariantOutcome,
        RunCancellation, execute_configured_case_attempt_with_timeout,
        execute_configured_shrink_attempt_with_timeout,
    },
    configured_replay::{
        ATTEMPT_COUNT, ConfiguredReplayArtifactError, ConfiguredReplayError,
        VerifiedConfiguredReplaySource, attempt_result, attest_replay_boundary,
        load_verified_configured_replay_source,
    },
    postgres::{
        probe::{ConfiguredSqlProbe, ConfiguredSqlProbeError, load_configured_sql_probe},
        quiescence::{ConfiguredQuiescence, ConfiguredQuiescenceError, load_configured_quiescence},
        snapshot::{ConfiguredSnapshot, ConfiguredSnapshotError, load_configured_snapshot},
    },
    reference_case::{ReferenceCaseRunError, preflight_reference_planned_case},
    repository::capture_repository_provenance,
    run_supervisor::ComposeProjectLock,
};

pub const MIN_SHRINK_TIME: Duration = Duration::from_millis(1);
pub const MAX_SHRINK_TIME: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfiguredShrinkOptions {
    candidate_limit: CandidateLimit,
    max_time: Duration,
}

impl ConfiguredShrinkOptions {
    /// Creates bounded shrink-search options without expanding v1 limits.
    ///
    /// # Errors
    ///
    /// Returns [`ConfiguredShrinkOptionsError`] for zero or out-of-range
    /// candidate/time values.
    pub fn new(
        max_candidates: u8,
        max_time: Duration,
    ) -> Result<Self, ConfiguredShrinkOptionsError> {
        let candidate_limit = CandidateLimit::new(max_candidates)
            .map_err(ConfiguredShrinkOptionsError::CandidateLimit)?;
        if max_time < MIN_SHRINK_TIME || max_time > MAX_SHRINK_TIME {
            return Err(ConfiguredShrinkOptionsError::MaxTime);
        }
        Ok(Self {
            candidate_limit,
            max_time,
        })
    }

    #[must_use]
    pub const fn candidate_limit(self) -> CandidateLimit {
        self.candidate_limit
    }

    #[must_use]
    pub const fn max_time(self) -> Duration {
        self.max_time
    }
}

impl Default for ConfiguredShrinkOptions {
    fn default() -> Self {
        Self {
            candidate_limit: CandidateLimit::default(),
            max_time: MAX_SHRINK_TIME,
        }
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ConfiguredShrinkOptionsError {
    #[error("configured shrink candidate limit is invalid: {0:?}")]
    CandidateLimit(InvalidCandidateLimit),
    #[error("configured shrink time limit must be between 1ms and 10m")]
    MaxTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfiguredShrinkCompletion {
    Complete,
    BudgetExhausted,
    SourceInconclusive,
}

impl ConfiguredShrinkCompletion {
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Complete => 10,
            Self::BudgetExhausted => 11,
            Self::SourceInconclusive => 4,
        }
    }
}

#[derive(Serialize)]
pub struct ConfiguredShrinkOutput {
    schema_version: u16,
    status: &'static str,
    shrink_id: String,
    source_replay_id: String,
    case_id: String,
    completion: ConfiguredShrinkCompletion,
    evaluated_candidates: usize,
    accepted_candidates: usize,
    original_action_count: usize,
    best_action_count: usize,
    artifact_path: PathBuf,
}

impl ConfiguredShrinkOutput {
    #[must_use]
    pub const fn completion(&self) -> ConfiguredShrinkCompletion {
        self.completion
    }

    #[must_use]
    pub fn artifact_path(&self) -> &Path {
        &self.artifact_path
    }

    /// Encodes the allowlisted configured-shrink receipt.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if JSON encoding fails.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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

#[derive(Serialize)]
struct ShrinkSourceArtifact<'a> {
    schema_version: u16,
    source_replay_id: &'a str,
    source_run_id: &'a str,
    case_id: &'a str,
    expected_failure: FailureIdentityArtifact,
    original_trace_retained: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AttemptVerdict {
    Held,
    ExpectedViolation,
    OtherViolation,
}

#[derive(Serialize)]
struct AttemptArtifact {
    schema_version: u16,
    attempt: usize,
    trace_matches_authority: bool,
    before_database_oid: u32,
    after_database_oid: u32,
    before_marker_uuid: String,
    after_marker_uuid: String,
    executed_action_count: usize,
    journal_record_count: usize,
    journal_last_record_hash: Option<String>,
    provider_object_count: usize,
    verdict: AttemptVerdict,
    failure_identity: Option<FailureIdentityArtifact>,
    invariants: Vec<InvariantArtifact>,
}

#[derive(Serialize)]
struct InvariantArtifact {
    invariant_id: String,
    checkpoint_id: String,
    violated: bool,
    witness_count: usize,
}

#[derive(Serialize)]
struct CandidateEvaluationArtifact {
    schema_version: u16,
    candidate_id: String,
    action_count: usize,
    complexity: ShrinkComplexity,
    matching_failure_count: usize,
    accepted: bool,
    attempts: Vec<AttemptArtifact>,
}

struct CandidateEvaluation {
    artifact: CandidateEvaluationArtifact,
    authority: CompiledShrinkTrace,
}

#[derive(Serialize)]
struct ShrinkSummary<'a> {
    schema_version: u16,
    status: &'static str,
    shrink_id: &'a str,
    source_replay_id: &'a str,
    source_run_id: &'a str,
    case_id: &'a str,
    expected_failure: FailureIdentityArtifact,
    completion: ConfiguredShrinkCompletion,
    candidate_limit: u8,
    max_time_milliseconds: u64,
    original_attempt_count: usize,
    original_matching_failure_count: usize,
    evaluated_candidates: usize,
    cache_hits: usize,
    accepted_candidates: usize,
    original_action_count: usize,
    best_action_count: usize,
    minimized_trace_written: bool,
    original_attempts: &'a [AttemptArtifact],
    candidates: &'a [CandidateEvaluationArtifact],
}

#[derive(Serialize)]
struct PartialShrinkSummary<'a> {
    schema_version: u16,
    status: &'static str,
    shrink_id: &'a str,
    source_replay_id: &'a str,
    case_id: &'a str,
    failure_class: ConfiguredCampaignFailureClass,
    failure_code: &'static str,
    completed_original_attempts: usize,
    completed_candidate_evaluations: usize,
}

/// Shrinks one verified configured replay under fixed candidate and time
/// budgets, accepting only the same invariant/checkpoint in at least 2/3
/// fresh-baseline attempts.
///
/// # Errors
///
/// Returns [`ConfiguredShrinkError`] for invalid source evidence, preparation,
/// compatibility, reset, execution, recovery, or evidence failures.
pub async fn run_configured_shrink(
    artifact_path: &Path,
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    options: ConfiguredShrinkOptions,
) -> Result<ConfiguredShrinkOutput, ConfiguredShrinkError> {
    Box::pin(run_configured_shrink_with_cancellation(
        artifact_path,
        config_path,
        environment,
        options,
        &RunCancellation::new(),
    ))
    .await
}

/// Executes configured shrink with a root cancellation capability.
///
/// # Errors
///
/// Returns [`ConfiguredShrinkError`] after recovery and partial-evidence
/// finalization when cancellation interrupts an entered shrink boundary.
#[allow(clippy::too_many_lines)]
pub async fn run_configured_shrink_with_cancellation(
    artifact_path: &Path,
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    options: ConfiguredShrinkOptions,
    cancellation: &RunCancellation,
) -> Result<ConfiguredShrinkOutput, ConfiguredShrinkError> {
    let artifact = verify_complete_run_artifact(artifact_path)
        .map_err(ConfiguredReplayArtifactError::Artifact)?;
    let source = load_verified_configured_replay_source(&artifact)?;
    let generator = CandidateGenerator::new(source.original_trace().planned_case())?;
    let config = load_resolved_config(config_path, environment)?;
    let configured_probe = load_configured_sql_probe(&config)?;
    let configured_quiescence = load_configured_quiescence(&config)?;
    let configured_snapshot = load_configured_snapshot(&config)?;
    preflight_reference_planned_case(source.original_trace().planned_case(), true, true)?;
    if cancellation.is_cancelled() {
        return Err(ConfiguredShrinkError::Interrupted);
    }
    let _project_lock = ComposeProjectLock::try_acquire(config.root(), config.compose_project())
        .map_err(|error| ConfiguredShrinkError::ProjectLock(Box::new(error)))?;
    let (baseline, current_compatibility) = attest_replay_boundary(
        &config,
        &configured_probe,
        &configured_quiescence,
        &configured_snapshot,
    )
    .await
    .map_err(ConfiguredShrinkError::Boundary)?;
    artifact
        .compatibility()
        .require_exact_match(&current_compatibility)?;
    let repository = capture_repository_provenance(config.root())
        .await
        .map_err(|error| ConfiguredShrinkError::Repository(Box::new(error)))?;

    let shrink_id = format!("run_{}", uuid::Uuid::new_v4().simple());
    let mut artifacts = RunArtifactStaging::create_v2(
        config.root(),
        config.artifact_dir(),
        &shrink_id,
        ManifestSeed::new(
            ArtifactKind::Shrink,
            repository,
            vec![artifact.source_identity()],
        ),
    )?;
    artifacts.write_json("config.redacted.json", config.redacted())?;
    artifacts.write_json("compatibility.json", &current_compatibility)?;
    artifacts.write_bytes("trace.original.json", source.original_trace_bytes())?;
    artifacts.write_json(
        "source.json",
        &ShrinkSourceArtifact {
            schema_version: 1,
            source_replay_id: source.replay_id(),
            source_run_id: source.source_run_id(),
            case_id: source.case_id(),
            expected_failure: source.expected_failure().into(),
            original_trace_retained: true,
        },
    )?;

    let deadline = ShrinkDeadline::start(options.max_time());
    let mut original_attempts = Vec::with_capacity(ATTEMPT_COUNT);
    let mut original_results = Vec::with_capacity(ATTEMPT_COUNT);
    let original_evaluation = Box::pin(evaluate_original(
        &config,
        &configured_probe,
        &configured_quiescence,
        &configured_snapshot,
        baseline,
        artifact.compatibility(),
        &source,
        &shrink_id,
        &mut artifacts,
        &mut original_attempts,
        &mut original_results,
        &deadline,
        cancellation,
    ))
    .await;
    match original_evaluation {
        Ok(()) => {}
        Err(ConfiguredShrinkError::BudgetExpired) => {
            let original_matching_failure_count =
                matching_count(&original_results, source.expected_failure());
            return finalize_completed_shrink(
                artifacts,
                &shrink_id,
                &source,
                options,
                ConfiguredShrinkCompletion::SourceInconclusive,
                &original_attempts,
                original_matching_failure_count,
                &[],
                0,
                0,
                None,
            );
        }
        Err(cause) => {
            return finalize_failed_shrink(
                artifacts,
                cause,
                &shrink_id,
                &source,
                original_attempts.len(),
                0,
            );
        }
    }
    let original_matching_failure_count =
        matching_count(&original_results, source.expected_failure());
    if original_matching_failure_count < 2 {
        return finalize_completed_shrink(
            artifacts,
            &shrink_id,
            &source,
            options,
            ConfiguredShrinkCompletion::SourceInconclusive,
            &original_attempts,
            original_matching_failure_count,
            &[],
            0,
            0,
            None,
        );
    }

    let mut best = None;
    let mut best_trace = None;
    let mut evaluations = Vec::new();
    let mut cache = BTreeMap::<CandidateIdentity, bool>::new();
    let mut cache_hits = 0_usize;
    let mut accepted_candidates = 0_usize;
    let mut completion = ConfiguredShrinkCompletion::Complete;

    'search: loop {
        let candidate_batch =
            generator.candidate_batch(best.as_ref(), CandidateLimit::default())?;
        let frontier_was_capped = candidate_batch.was_truncated();
        let candidates = candidate_batch.into_candidates();
        if candidates.is_empty() {
            break;
        }
        let mut accepted_this_frontier = false;
        for candidate in candidates {
            let identity = candidate.canonical_identity();
            if cache.contains_key(&identity) {
                cache_hits = cache_hits.saturating_add(1);
                continue;
            }
            if evaluations.len() == usize::from(options.candidate_limit().value())
                || deadline.is_exhausted()
            {
                completion = ConfiguredShrinkCompletion::BudgetExhausted;
                break 'search;
            }
            let evaluation = Box::pin(evaluate_candidate(
                &config,
                &configured_probe,
                &configured_quiescence,
                &configured_snapshot,
                artifact.compatibility(),
                &source,
                &candidate,
                &shrink_id,
                &mut artifacts,
                cancellation,
                &deadline,
            ))
            .await;
            let evaluation = match evaluation {
                Ok(evaluation) => evaluation,
                Err(ConfiguredShrinkError::BudgetExpired) => {
                    completion = ConfiguredShrinkCompletion::BudgetExhausted;
                    break 'search;
                }
                Err(cause) => {
                    return finalize_failed_shrink(
                        artifacts,
                        cause,
                        &shrink_id,
                        &source,
                        original_attempts.len(),
                        evaluations.len(),
                    );
                }
            };
            let accepted = evaluation.artifact.accepted;
            cache.insert(identity, accepted);
            if accepted {
                best_trace = Some(evaluation.authority.clone());
                best = Some(candidate);
                accepted_candidates = accepted_candidates.saturating_add(1);
                accepted_this_frontier = true;
            }
            evaluations.push(evaluation.artifact);
            if accepted_this_frontier {
                continue 'search;
            }
        }
        if !accepted_this_frontier {
            if frontier_was_capped {
                completion = ConfiguredShrinkCompletion::BudgetExhausted;
            }
            break;
        }
    }

    finalize_completed_shrink(
        artifacts,
        &shrink_id,
        &source,
        options,
        completion,
        &original_attempts,
        original_matching_failure_count,
        &evaluations,
        cache_hits,
        accepted_candidates,
        best_trace.as_ref(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_original(
    config: &ResolvedConfig,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    baseline: ConfiguredBaselineSession,
    expected_compatibility: &RunCompatibilityV1,
    source: &VerifiedConfiguredReplaySource,
    shrink_id: &str,
    artifacts: &mut RunArtifactStaging,
    attempt_artifacts: &mut Vec<AttemptArtifact>,
    attempt_results: &mut Vec<AttemptResult>,
    deadline: &ShrinkDeadline,
    cancellation: &RunCancellation,
) -> Result<(), ConfiguredShrinkError> {
    let mut first_baseline = Some(baseline);
    for attempt in 1..=ATTEMPT_COUNT {
        if cancellation.is_cancelled() {
            return Err(ConfiguredShrinkError::Interrupted);
        }
        let case_timeout = deadline
            .attempt_timeout(config.case_timeout())
            .ok_or(ConfiguredShrinkError::BudgetExpired)?;
        let current_baseline = if attempt == 1 {
            first_baseline
                .take()
                .ok_or(ConfiguredShrinkError::AttemptAccounting)?
        } else {
            fresh_compatible_baseline(
                config,
                configured_probe,
                configured_quiescence,
                configured_snapshot,
                expected_compatibility,
            )
            .await?
        };
        let attempt_id = format!("attempt_{attempt:04}");
        let journal_path = artifacts.prepare_path(format!(
            "original/attempts/{attempt_id}/observations.ndjson"
        ))?;
        let attempt_execution = Box::pin(execute_configured_case_attempt_with_timeout(
            config,
            configured_probe,
            configured_quiescence,
            configured_snapshot,
            current_baseline,
            source.original_trace().planned_case(),
            shrink_id,
            &format!("{}_original_{attempt_id}", source.case_id()),
            journal_path,
            case_timeout,
            cancellation,
        ))
        .await;
        let (_, execution) = match attempt_execution {
            Ok(execution) => execution,
            Err(ConfiguredCampaignError::CaseTimedOut { .. }) if deadline.is_exhausted() => {
                return Err(ConfiguredShrinkError::BudgetExpired);
            }
            Err(error) => return Err(error.into()),
        };
        let trace_matches = source
            .original_trace()
            .matches_replay_authority(execution.trace());
        let result = configured_attempt_result(source.expected_failure(), execution.invariants());
        let evidence = attempt_artifact(
            attempt,
            &execution,
            &result,
            source.expected_failure(),
            trace_matches,
        );
        artifacts.write_json(
            format!("original/attempts/{attempt_id}/trace.json"),
            execution.trace(),
        )?;
        artifacts.write_json(
            format!("original/attempts/{attempt_id}/result.json"),
            &evidence,
        )?;
        attempt_artifacts.push(evidence);
        if !trace_matches {
            return Err(ConfiguredShrinkError::OriginalTraceDiverged { attempt });
        }
        attempt_results.push(result);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn evaluate_candidate(
    config: &ResolvedConfig,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    expected_compatibility: &RunCompatibilityV1,
    source: &VerifiedConfiguredReplaySource,
    candidate: &ShrinkCandidate,
    shrink_id: &str,
    artifacts: &mut RunArtifactStaging,
    cancellation: &RunCancellation,
    deadline: &ShrinkDeadline,
) -> Result<CandidateEvaluation, ConfiguredShrinkError> {
    let identity = candidate.canonical_identity();
    let candidate_id = identity.to_hex();
    artifacts.write_json(
        format!("candidates/{candidate_id}/candidate.json"),
        candidate,
    )?;
    let mut attempts = Vec::with_capacity(ATTEMPT_COUNT);
    let mut results = Vec::with_capacity(ATTEMPT_COUNT);
    let mut authority = None;
    for attempt in 1..=ATTEMPT_COUNT {
        if cancellation.is_cancelled() {
            return Err(ConfiguredShrinkError::Interrupted);
        }
        let case_timeout = deadline
            .attempt_timeout(config.case_timeout())
            .ok_or(ConfiguredShrinkError::BudgetExpired)?;
        let baseline = fresh_compatible_baseline(
            config,
            configured_probe,
            configured_quiescence,
            configured_snapshot,
            expected_compatibility,
        )
        .await?;
        let attempt_id = format!("attempt_{attempt:04}");
        let journal_path = artifacts.prepare_path(format!(
            "candidates/{candidate_id}/attempts/{attempt_id}/observations.ndjson"
        ))?;
        let attempt_execution = Box::pin(execute_configured_shrink_attempt_with_timeout(
            config,
            configured_probe,
            configured_quiescence,
            configured_snapshot,
            baseline,
            candidate,
            shrink_id,
            &format!("{}_candidate_{candidate_id}_{attempt_id}", source.case_id()),
            journal_path,
            case_timeout,
            cancellation,
        ))
        .await;
        let (_, execution) = match attempt_execution {
            Ok(execution) => execution,
            Err(ConfiguredCampaignError::CaseTimedOut { .. }) if deadline.is_exhausted() => {
                return Err(ConfiguredShrinkError::BudgetExpired);
            }
            Err(error) => return Err(error.into()),
        };
        let trace_matches = authority
            .as_ref()
            .is_none_or(|authority: &CompiledShrinkTrace| {
                authority.matches_replay_authority(execution.trace())
            });
        if authority.is_none() {
            authority = Some(execution.trace().clone());
        }
        let result = configured_attempt_result(source.expected_failure(), execution.invariants());
        let evidence = attempt_artifact(
            attempt,
            &execution,
            &result,
            source.expected_failure(),
            trace_matches,
        );
        artifacts.write_json(
            format!("candidates/{candidate_id}/attempts/{attempt_id}/trace.json"),
            execution.trace(),
        )?;
        artifacts.write_json(
            format!("candidates/{candidate_id}/attempts/{attempt_id}/result.json"),
            &evidence,
        )?;
        attempts.push(evidence);
        if !trace_matches {
            return Err(ConfiguredShrinkError::CandidateTraceDiverged {
                candidate_id,
                attempt,
            });
        }
        results.push(result);
    }
    let matching_failure_count = matching_count(&results, source.expected_failure());
    let accepted = matching_failure_count >= 2;
    let artifact = CandidateEvaluationArtifact {
        schema_version: 1,
        candidate_id: candidate_id.clone(),
        action_count: candidate.actions().len(),
        complexity: candidate.complexity(),
        matching_failure_count,
        accepted,
        attempts,
    };
    artifacts.write_json(
        format!("candidates/{candidate_id}/evaluation.json"),
        &artifact,
    )?;
    Ok(CandidateEvaluation {
        artifact,
        authority: authority.ok_or(ConfiguredShrinkError::AttemptAccounting)?,
    })
}

async fn fresh_compatible_baseline(
    config: &ResolvedConfig,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    expected_compatibility: &RunCompatibilityV1,
) -> Result<ConfiguredBaselineSession, ConfiguredShrinkError> {
    let (baseline, compatibility) = attest_replay_boundary(
        config,
        configured_probe,
        configured_quiescence,
        configured_snapshot,
    )
    .await
    .map_err(ConfiguredShrinkError::Boundary)?;
    expected_compatibility.require_exact_match(&compatibility)?;
    Ok(baseline)
}

fn configured_attempt_result(
    expected: &FailureIdentity,
    invariants: &[ConfiguredInvariantOutcome],
) -> AttemptResult {
    attempt_result(
        expected,
        invariants
            .iter()
            .map(|outcome| (outcome.identity(), outcome.violated())),
    )
}

fn matching_count(attempts: &[AttemptResult], expected: &FailureIdentity) -> usize {
    attempts
        .iter()
        .filter(
            |attempt| matches!(attempt, AttemptResult::Violation(identity) if identity == expected),
        )
        .count()
}

fn capped_attempt_timeout(
    max_time: Duration,
    elapsed: Duration,
    configured_case_timeout: Duration,
) -> Option<Duration> {
    let remaining = max_time.checked_sub(elapsed)?;
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.min(configured_case_timeout))
}

#[derive(Clone, Copy, Debug)]
struct ShrinkDeadline {
    started: Instant,
    max_time: Duration,
}

impl ShrinkDeadline {
    fn start(max_time: Duration) -> Self {
        Self {
            started: Instant::now(),
            max_time,
        }
    }

    fn attempt_timeout(&self, configured_case_timeout: Duration) -> Option<Duration> {
        capped_attempt_timeout(
            self.max_time,
            self.started.elapsed(),
            configured_case_timeout,
        )
    }

    fn is_exhausted(&self) -> bool {
        self.started.elapsed() >= self.max_time
    }
}

fn attempt_artifact<T>(
    attempt: usize,
    execution: &crate::configured_campaign::ConfiguredAttemptExecution<T>,
    result: &AttemptResult,
    expected: &FailureIdentity,
    trace_matches_authority: bool,
) -> AttemptArtifact
where
    T: TraceActionCount,
{
    let (verdict, failure_identity) = match result {
        AttemptResult::Held => (AttemptVerdict::Held, None),
        AttemptResult::Violation(identity) if identity == expected => {
            (AttemptVerdict::ExpectedViolation, Some(identity.into()))
        }
        AttemptResult::Violation(identity) => {
            (AttemptVerdict::OtherViolation, Some(identity.into()))
        }
        AttemptResult::Inconclusive => unreachable!("only completed oracle attempts are retained"),
    };
    AttemptArtifact {
        schema_version: 1,
        attempt,
        trace_matches_authority,
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
            .map(|outcome| InvariantArtifact {
                invariant_id: outcome.identity().invariant().as_str().to_owned(),
                checkpoint_id: outcome.identity().checkpoint().as_str().to_owned(),
                violated: outcome.violated(),
                witness_count: outcome.witness_count(),
            })
            .collect(),
    }
}

trait TraceActionCount {
    fn action_count(&self) -> usize;
}

impl TraceActionCount for CompiledCaseTrace {
    fn action_count(&self) -> usize {
        self.action_count()
    }
}

impl TraceActionCount for CompiledShrinkTrace {
    fn action_count(&self) -> usize {
        self.action_count()
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize_completed_shrink(
    mut artifacts: RunArtifactStaging,
    shrink_id: &str,
    source: &VerifiedConfiguredReplaySource,
    options: ConfiguredShrinkOptions,
    completion: ConfiguredShrinkCompletion,
    original_attempts: &[AttemptArtifact],
    original_matching_failure_count: usize,
    evaluations: &[CandidateEvaluationArtifact],
    cache_hits: usize,
    accepted_candidates: usize,
    best_trace: Option<&CompiledShrinkTrace>,
) -> Result<ConfiguredShrinkOutput, ConfiguredShrinkError> {
    if let Some(trace) = best_trace {
        artifacts.write_json("trace.minimized.json", trace)?;
    }
    let original_action_count = source.original_trace().action_count();
    let best_action_count =
        best_trace.map_or(original_action_count, CompiledShrinkTrace::action_count);
    artifacts.write_json(
        "summary.json",
        &ShrinkSummary {
            schema_version: 1,
            status: "configured_shrink_complete",
            shrink_id,
            source_replay_id: source.replay_id(),
            source_run_id: source.source_run_id(),
            case_id: source.case_id(),
            expected_failure: source.expected_failure().into(),
            completion,
            candidate_limit: options.candidate_limit().value(),
            max_time_milliseconds: u64::try_from(options.max_time().as_millis())
                .unwrap_or(u64::MAX),
            original_attempt_count: original_attempts.len(),
            original_matching_failure_count,
            evaluated_candidates: evaluations.len(),
            cache_hits,
            accepted_candidates,
            original_action_count,
            best_action_count,
            minimized_trace_written: best_trace.is_some(),
            original_attempts,
            candidates: evaluations,
        },
    )?;
    let evaluated_candidates = evaluations.len();
    let mut authorities = vec![
        ArtifactAuthority::shrink_source(),
        ArtifactAuthority::original_trace(),
    ];
    if best_trace.is_some() {
        authorities.push(ArtifactAuthority::minimized_trace());
    }
    let result = match completion {
        ConfiguredShrinkCompletion::Complete => ArtifactResult::Counterexample,
        ConfiguredShrinkCompletion::BudgetExhausted => ArtifactResult::BudgetExhausted,
        ConfiguredShrinkCompletion::SourceInconclusive => ArtifactResult::Inconclusive,
    };
    let artifact_path = artifacts.finalize_complete(result, authorities)?;
    Ok(ConfiguredShrinkOutput {
        schema_version: 1,
        status: "configured_shrink_complete",
        shrink_id: shrink_id.to_owned(),
        source_replay_id: source.replay_id().to_owned(),
        case_id: source.case_id().to_owned(),
        completion,
        evaluated_candidates,
        accepted_candidates,
        original_action_count,
        best_action_count,
        artifact_path,
    })
}

fn finalize_failed_shrink(
    mut artifacts: RunArtifactStaging,
    cause: ConfiguredShrinkError,
    shrink_id: &str,
    source: &VerifiedConfiguredReplaySource,
    completed_original_attempts: usize,
    completed_candidate_evaluations: usize,
) -> Result<ConfiguredShrinkOutput, ConfiguredShrinkError> {
    let failure_class = cause.failure_class();
    let failure_code = cause.failure_code();
    if let Err(artifact) = artifacts.write_json(
        "summary.json",
        &PartialShrinkSummary {
            schema_version: 1,
            status: partial_status(failure_class),
            shrink_id,
            source_replay_id: source.replay_id(),
            case_id: source.case_id(),
            failure_class,
            failure_code,
            completed_original_attempts,
            completed_candidate_evaluations,
        },
    ) {
        return Err(ConfiguredShrinkError::PartialFinalization {
            cause: Box::new(cause),
            artifact,
        });
    }
    let artifact_path = match artifacts.finalize_partial_v2(
        partial_artifact_class(failure_class),
        failure_code,
        vec![
            ArtifactAuthority::shrink_source(),
            ArtifactAuthority::original_trace(),
        ],
    ) {
        Ok(path) => path,
        Err(artifact) => {
            return Err(ConfiguredShrinkError::PartialFinalization {
                cause: Box::new(cause),
                artifact,
            });
        }
    };
    Err(ConfiguredShrinkError::RunFailed {
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

#[derive(Debug, Error)]
pub enum ConfiguredShrinkError {
    #[error("configured shrink options are invalid: {0}")]
    Options(#[from] ConfiguredShrinkOptionsError),
    #[error("configured shrink source replay is invalid: {0}")]
    SourceArtifact(#[from] ConfiguredReplayArtifactError),
    #[error("configured shrink candidate generation failed: {0:?}")]
    Candidate(ShrinkError),
    #[error("configured shrink configuration failed: {0}")]
    Config(#[from] ConfigError),
    #[error("configured shrink repository provenance failed: {0}")]
    Repository(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured shrink SQL probe could not be prepared: {0}")]
    SqlProbe(#[from] ConfiguredSqlProbeError),
    #[error("configured shrink quiescence query could not be prepared: {0}")]
    Quiescence(#[from] ConfiguredQuiescenceError),
    #[error("configured shrink invariant suite could not be prepared: {0}")]
    Snapshot(#[from] ConfiguredSnapshotError),
    #[error("configured shrink case preflight failed: {0}")]
    CasePreflight(#[from] ReferenceCaseRunError),
    #[error("configured shrink compatibility boundary failed: {0}")]
    Boundary(#[source] ConfiguredReplayError),
    #[error("configured shrink compatibility gate failed: {0}")]
    Compatibility(#[from] CompatibilityError),
    #[error("configured shrink Compose project exclusion failed: {0}")]
    ProjectLock(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured shrink evidence failed: {0}")]
    Evidence(#[from] ArtifactError),
    #[error("configured shrink case attempt failed: {0}")]
    Case(#[from] ConfiguredCampaignError),
    #[error("configured shrink original attempt {attempt} diverged from its trace authority")]
    OriginalTraceDiverged { attempt: usize },
    #[error("configured shrink candidate {candidate_id} attempt {attempt} diverged")]
    CandidateTraceDiverged {
        candidate_id: String,
        attempt: usize,
    },
    #[error("configured shrink completed an invalid number of attempts")]
    AttemptAccounting,
    #[error("configured shrink budget expired")]
    BudgetExpired,
    #[error("configured shrink was interrupted")]
    Interrupted,
    #[error("configured shrink failed; partial evidence retained at {artifact_path}: {cause}")]
    RunFailed {
        #[source]
        cause: Box<ConfiguredShrinkError>,
        artifact_path: PathBuf,
    },
    #[error("configured shrink partial-evidence finalization failed: {artifact}")]
    PartialFinalization {
        #[source]
        cause: Box<ConfiguredShrinkError>,
        artifact: ArtifactError,
    },
}

impl From<ShrinkError> for ConfiguredShrinkError {
    fn from(error: ShrinkError) -> Self {
        Self::Candidate(error)
    }
}

impl ConfiguredShrinkError {
    #[must_use]
    pub fn failure_class(&self) -> ConfiguredCampaignFailureClass {
        match self {
            Self::Options(_)
            | Self::SourceArtifact(_)
            | Self::Candidate(_)
            | Self::Config(_)
            | Self::Repository(_)
            | Self::SqlProbe(_)
            | Self::Quiescence(_)
            | Self::Snapshot(_)
            | Self::CasePreflight(_)
            | Self::Compatibility(_) => ConfiguredCampaignFailureClass::Configuration,
            Self::Boundary(error) => error.failure_class(),
            Self::Case(error) => error.failure_class(),
            Self::OriginalTraceDiverged { .. }
            | Self::CandidateTraceDiverged { .. }
            | Self::AttemptAccounting
            | Self::BudgetExpired => ConfiguredCampaignFailureClass::Inconclusive,
            Self::Interrupted => ConfiguredCampaignFailureClass::Interrupted,
            Self::RunFailed { cause, .. } | Self::PartialFinalization { cause, .. } => {
                cause.failure_class()
            }
            Self::ProjectLock(_) | Self::Evidence(_) => {
                ConfiguredCampaignFailureClass::Infrastructure
            }
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
            Self::Candidate(_) => "invalid_candidate",
            Self::Config(_) => "invalid_config",
            Self::Repository(_) => "repository_provenance",
            Self::SqlProbe(_) => "invalid_sql_probe",
            Self::Quiescence(_) => "invalid_quiescence",
            Self::Snapshot(_) => "invalid_snapshot",
            Self::CasePreflight(_) => "case_preflight",
            Self::Boundary(error) => error.failure_code(),
            Self::Compatibility(_) => "compatibility_mismatch",
            Self::ProjectLock(_) => "project_locked",
            Self::Evidence(_) => "artifact_failure",
            Self::Case(error) => error.failure_code(),
            Self::OriginalTraceDiverged { .. } => "original_trace_diverged",
            Self::CandidateTraceDiverged { .. } => "candidate_trace_diverged",
            Self::AttemptAccounting => "attempt_accounting",
            Self::BudgetExpired => "shrink_budget_expired",
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

#[cfg(test)]
mod tests {
    use super::{
        ConfiguredShrinkCompletion, ConfiguredShrinkOptions, ConfiguredShrinkOptionsError,
        MAX_SHRINK_TIME, capped_attempt_timeout,
    };
    use std::time::Duration;

    #[test]
    fn configured_shrink_options_enforce_both_v1_budgets() {
        let defaults = ConfiguredShrinkOptions::default();
        assert_eq!(defaults.candidate_limit().value(), 60);
        assert_eq!(defaults.max_time(), Duration::from_secs(600));
        assert!(ConfiguredShrinkOptions::new(0, Duration::from_secs(1)).is_err());
        assert!(ConfiguredShrinkOptions::new(61, Duration::from_secs(1)).is_err());
        assert_eq!(
            ConfiguredShrinkOptions::new(1, Duration::ZERO),
            Err(ConfiguredShrinkOptionsError::MaxTime)
        );
        assert_eq!(
            ConfiguredShrinkOptions::new(1, Duration::from_micros(999)),
            Err(ConfiguredShrinkOptionsError::MaxTime)
        );
        assert!(ConfiguredShrinkOptions::new(1, Duration::from_millis(1)).is_ok());
        assert_eq!(
            ConfiguredShrinkOptions::new(1, MAX_SHRINK_TIME + Duration::from_nanos(1)),
            Err(ConfiguredShrinkOptionsError::MaxTime)
        );
    }

    #[test]
    fn configured_shrink_completion_has_non_overlapping_exit_codes() {
        assert_eq!(ConfiguredShrinkCompletion::Complete.exit_code(), 10);
        assert_eq!(ConfiguredShrinkCompletion::BudgetExhausted.exit_code(), 11);
        assert_eq!(
            ConfiguredShrinkCompletion::SourceInconclusive.exit_code(),
            4
        );
    }

    #[test]
    fn attempt_timeout_never_exceeds_remaining_shrink_budget() {
        assert_eq!(
            capped_attempt_timeout(
                Duration::from_secs(10),
                Duration::from_secs(2),
                Duration::from_secs(90)
            ),
            Some(Duration::from_secs(8))
        );
        assert_eq!(
            capped_attempt_timeout(
                Duration::from_secs(10),
                Duration::from_secs(2),
                Duration::from_secs(3)
            ),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            capped_attempt_timeout(
                Duration::from_secs(10),
                Duration::from_secs(10),
                Duration::from_secs(90)
            ),
            None
        );
        assert_eq!(
            capped_attempt_timeout(
                Duration::from_secs(10),
                Duration::from_secs(11),
                Duration::from_secs(90)
            ),
            None
        );
    }
}
