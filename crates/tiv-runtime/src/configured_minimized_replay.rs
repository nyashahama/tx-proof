//! Compatibility-gated replay of one authority-bound minimized trace.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiv_core::{
    result::{AttemptResult, CheckpointId, FailureIdentity, InvariantId, classify_reproduction},
    shrink::ShrinkCandidate,
    trace::{CompiledCaseTrace, CompiledShrinkTrace},
};

use crate::{
    artifacts::{
        ArtifactAuthority, ArtifactError, ArtifactKind, ArtifactResult, AuthorityRole,
        ManifestSeed, PartialRunClass, RunArtifactStaging, VerifiedRunArtifact,
        verify_complete_run_artifact,
    },
    baseline::ConfiguredBaselineSession,
    compatibility::{CompatibilityError, RunCompatibilityV1},
    config::{ConfigError, EnvironmentLookup, ResolvedConfig, load_resolved_config},
    configured_campaign::{
        ConfiguredCampaignError, ConfiguredCampaignFailureClass, ConfiguredInvariantOutcome,
        ConfiguredShrinkExecution, RunCancellation, execute_configured_shrink_attempt_with_timeout,
        witness_projection_policy, write_invariant_witness_artifacts,
    },
    configured_replay::{
        ATTEMPT_COUNT, ConfiguredReplayClassification, ConfiguredReplayError, attempt_result,
        attest_replay_boundary,
    },
    postgres::{
        probe::{ConfiguredSqlProbe, ConfiguredSqlProbeError, load_configured_sql_probe},
        quiescence::{ConfiguredQuiescence, ConfiguredQuiescenceError, load_configured_quiescence},
        snapshot::{ConfiguredSnapshot, ConfiguredSnapshotError, load_configured_snapshot},
    },
    reference_case::{ReferenceCaseRunError, preflight_reference_shrink_candidate},
    reports::{
        ArtifactReport, ReplayCommand, ReportArtifactKind, ReportCheck, ReportCheckOutcome,
        ReportConclusion, ReportFact, ReportFailure, partial_artifact_report,
        validate_replay_command_paths, write_report_bundle,
    },
    repository::capture_repository_provenance,
    run_supervisor::ComposeProjectLock,
};

const SOURCE_FILE: &str = "source.json";
const SUMMARY_FILE: &str = "summary.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FailureIdentityDocument {
    invariant_id: String,
    checkpoint_id: String,
}

impl FailureIdentityDocument {
    fn to_identity(&self) -> Result<FailureIdentity, ConfiguredMinimizedReplayArtifactError> {
        Ok(FailureIdentity::new(
            InvariantId::new(&self.invariant_id)
                .map_err(|_| ConfiguredMinimizedReplayArtifactError::InvalidSource)?,
            CheckpointId::new(&self.checkpoint_id)
                .map_err(|_| ConfiguredMinimizedReplayArtifactError::InvalidSource)?,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShrinkSourceDocument {
    schema_version: u16,
    source_replay_id: String,
    source_run_id: String,
    case_id: String,
    expected_failure: FailureIdentityDocument,
    original_trace_retained: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ShrinkCompletionDocument {
    Complete,
    BudgetExhausted,
    SourceInconclusive,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ShrinkInvariantDocument {
    invariant_id: String,
    checkpoint_id: String,
    violated: bool,
    witness_count: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ShrinkAttemptVerdictDocument {
    Held,
    ExpectedViolation,
    OtherViolation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ShrinkAttemptDocument {
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
    verdict: ShrinkAttemptVerdictDocument,
    failure_identity: Option<FailureIdentityDocument>,
    invariants: Vec<ShrinkInvariantDocument>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct CandidateEvaluationDocument {
    schema_version: u16,
    candidate_id: String,
    action_count: usize,
    complexity: serde_json::Value,
    matching_failure_count: usize,
    accepted: bool,
    attempts: Vec<ShrinkAttemptDocument>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShrinkSummaryDocument {
    schema_version: u16,
    status: String,
    shrink_id: String,
    source_replay_id: String,
    source_run_id: String,
    case_id: String,
    expected_failure: FailureIdentityDocument,
    completion: ShrinkCompletionDocument,
    candidate_limit: u8,
    max_time_milliseconds: u64,
    original_attempt_count: usize,
    original_matching_failure_count: usize,
    evaluated_candidates: usize,
    #[allow(dead_code)]
    cache_hits: usize,
    accepted_candidates: usize,
    original_action_count: usize,
    best_action_count: usize,
    minimized_trace_written: bool,
    original_attempts: Vec<ShrinkAttemptDocument>,
    candidates: Vec<CandidateEvaluationDocument>,
}

pub(crate) struct VerifiedMinimizedShrinkSource {
    shrink_id: String,
    source_replay_id: String,
    source_run_id: String,
    case_id: String,
    expected_failure: FailureIdentity,
    trace: CompiledShrinkTrace,
    trace_bytes: Vec<u8>,
}

impl VerifiedMinimizedShrinkSource {
    pub(crate) fn shrink_id(&self) -> &str {
        &self.shrink_id
    }

    pub(crate) fn source_replay_id(&self) -> &str {
        &self.source_replay_id
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

    pub(crate) const fn trace(&self) -> &CompiledShrinkTrace {
        &self.trace
    }

    pub(crate) fn trace_bytes(&self) -> &[u8] {
        &self.trace_bytes
    }
}

impl From<&FailureIdentity> for FailureIdentityDocument {
    fn from(identity: &FailureIdentity) -> Self {
        Self {
            invariant_id: identity.invariant().as_str().to_owned(),
            checkpoint_id: identity.checkpoint().as_str().to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum MinimizedReplayAttemptVerdict {
    Held,
    ExpectedViolation,
    OtherViolation,
}

#[derive(Serialize)]
struct MinimizedReplayInvariantArtifact {
    invariant_id: String,
    checkpoint_id: String,
    violated: bool,
    witness_count: usize,
}

#[derive(Serialize)]
struct MinimizedReplayAttemptArtifact {
    schema_version: u16,
    attempt: usize,
    trace_matches_minimized_authority: bool,
    before_database_oid: u32,
    after_database_oid: u32,
    before_marker_uuid: String,
    after_marker_uuid: String,
    executed_action_count: usize,
    journal_record_count: usize,
    journal_last_record_hash: Option<String>,
    #[allow(dead_code)]
    provider_object_count: usize,
    verdict: MinimizedReplayAttemptVerdict,
    failure_identity: Option<FailureIdentityDocument>,
    invariants: Vec<MinimizedReplayInvariantArtifact>,
}

#[derive(Serialize)]
struct MinimizedReplaySourceArtifact<'a> {
    schema_version: u16,
    source_shrink_id: &'a str,
    source_replay_id: &'a str,
    source_run_id: &'a str,
    case_id: &'a str,
    expected_failure: FailureIdentityDocument,
}

#[derive(Serialize)]
struct MinimizedReplaySummary<'a> {
    schema_version: u16,
    status: &'static str,
    minimized_replay_id: &'a str,
    source_shrink_id: &'a str,
    source_replay_id: &'a str,
    source_run_id: &'a str,
    case_id: &'a str,
    expected_failure: FailureIdentityDocument,
    attempt_count: usize,
    matching_failure_count: usize,
    classification: ConfiguredReplayClassification,
    attempts: &'a [MinimizedReplayAttemptArtifact],
}

#[derive(Serialize)]
struct PartialMinimizedReplaySummary<'a> {
    schema_version: u16,
    status: &'static str,
    minimized_replay_id: &'a str,
    source_shrink_id: &'a str,
    source_replay_id: &'a str,
    case_id: &'a str,
    failure_class: ConfiguredCampaignFailureClass,
    failure_code: &'static str,
    completed_attempts: usize,
    attempts: &'a [MinimizedReplayAttemptArtifact],
}

/// Secret-free receipt for one completed authority-bound minimized replay.
#[derive(Serialize)]
pub struct ConfiguredMinimizedReplayOutput {
    schema_version: u16,
    status: &'static str,
    minimized_replay_id: String,
    source_shrink_id: String,
    source_replay_id: String,
    case_id: String,
    attempt_count: usize,
    matching_failure_count: usize,
    classification: ConfiguredReplayClassification,
    artifact_path: PathBuf,
}

impl ConfiguredMinimizedReplayOutput {
    #[must_use]
    pub const fn classification(&self) -> ConfiguredReplayClassification {
        self.classification
    }

    #[must_use]
    pub fn artifact_path(&self) -> &Path {
        &self.artifact_path
    }

    /// Encodes the allowlisted minimized-replay receipt.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if JSON encoding fails.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

fn load_verified_minimized_shrink_source(
    artifact: &VerifiedRunArtifact,
) -> Result<VerifiedMinimizedShrinkSource, ConfiguredMinimizedReplayArtifactError> {
    if artifact.manifest_schema_version() != 2 {
        return Err(ConfiguredMinimizedReplayArtifactError::UnsupportedManifest);
    }
    if artifact.artifact_kind() != Some(ArtifactKind::Shrink) {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidArtifactKind);
    }
    let result = artifact
        .artifact_result()
        .ok_or(ConfiguredMinimizedReplayArtifactError::InvalidArtifactResult)?;
    if !matches!(
        result,
        ArtifactResult::Counterexample | ArtifactResult::BudgetExhausted
    ) {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidArtifactResult);
    }
    let trace_bytes = artifact
        .read_unique_authority_bytes(AuthorityRole::MinimizedTrace)?
        .ok_or(ConfiguredMinimizedReplayArtifactError::MissingMinimizedAuthority)?;
    let trace: CompiledShrinkTrace = serde_json::from_slice(&trace_bytes).map_err(|source| {
        ConfiguredMinimizedReplayArtifactError::Decode {
            path: PathBuf::from("trace.minimized.json"),
            source,
        }
    })?;
    let original_trace_bytes = artifact
        .read_unique_authority_bytes(AuthorityRole::OriginalTrace)?
        .ok_or(ConfiguredMinimizedReplayArtifactError::InvalidSummary)?;
    let original_trace: CompiledCaseTrace =
        serde_json::from_slice(&original_trace_bytes).map_err(|source| {
            ConfiguredMinimizedReplayArtifactError::Decode {
                path: PathBuf::from("trace.original.json"),
                source,
            }
        })?;
    let source: ShrinkSourceDocument = decode_indexed(artifact, Path::new(SOURCE_FILE))?;
    let summary: ShrinkSummaryDocument = decode_indexed(artifact, Path::new(SUMMARY_FILE))?;
    validate_source_and_summary(artifact, result, &source, &summary, &original_trace, &trace)?;
    let expected_failure = source.expected_failure.to_identity()?;
    Ok(VerifiedMinimizedShrinkSource {
        shrink_id: artifact.run_id().to_owned(),
        source_replay_id: source.source_replay_id,
        source_run_id: source.source_run_id,
        case_id: source.case_id,
        expected_failure,
        trace,
        trace_bytes,
    })
}

fn decode_indexed<T: for<'de> Deserialize<'de>>(
    artifact: &VerifiedRunArtifact,
    path: &Path,
) -> Result<T, ConfiguredMinimizedReplayArtifactError> {
    serde_json::from_slice(&artifact.read_indexed_bytes(path)?).map_err(|source| {
        ConfiguredMinimizedReplayArtifactError::Decode {
            path: path.to_owned(),
            source,
        }
    })
}

fn validate_source_and_summary(
    artifact: &VerifiedRunArtifact,
    result: ArtifactResult,
    source: &ShrinkSourceDocument,
    summary: &ShrinkSummaryDocument,
    original_trace: &CompiledCaseTrace,
    trace: &CompiledShrinkTrace,
) -> Result<(), ConfiguredMinimizedReplayArtifactError> {
    let expected_failure = source.expected_failure.to_identity()?;
    let completion_matches = matches!(
        (summary.completion, result),
        (
            ShrinkCompletionDocument::Complete,
            ArtifactResult::Counterexample
        ) | (
            ShrinkCompletionDocument::BudgetExhausted,
            ArtifactResult::BudgetExhausted
        )
    );
    let accepted_count = summary
        .candidates
        .iter()
        .filter(|candidate| candidate.accepted)
        .count();
    if source.schema_version != 1
        || !source.original_trace_retained
        || !valid_run_id(&source.source_replay_id)
        || !valid_run_id(&source.source_run_id)
        || !valid_case_id(&source.case_id)
        || summary.schema_version != 1
        || summary.status != "configured_shrink_complete"
        || summary.shrink_id != artifact.run_id()
        || summary.source_replay_id != source.source_replay_id
        || summary.source_run_id != source.source_run_id
        || summary.case_id != source.case_id
        || summary.expected_failure != source.expected_failure
        || !completion_matches
        || !(1..=60).contains(&summary.candidate_limit)
        || !(1..=600_000).contains(&summary.max_time_milliseconds)
        || summary.original_attempt_count != 3
        || summary.original_attempts.len() != 3
        || !(2..=3).contains(&summary.original_matching_failure_count)
        || summary.evaluated_candidates != summary.candidates.len()
        || summary.evaluated_candidates > usize::from(summary.candidate_limit)
        || summary.accepted_candidates != accepted_count
        || summary.accepted_candidates == 0
        || summary.original_action_count != original_trace.action_count()
        || summary.best_action_count != trace.action_count()
        || summary.best_action_count > summary.original_action_count
        || !summary.minimized_trace_written
        || trace.candidate().source() != original_trace.planned_case()
    {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
    }

    let original_results = validate_original_attempts(
        artifact,
        &summary.original_attempts,
        &expected_failure,
        original_trace,
    )?;
    if matching_failure_count(&original_results, &expected_failure)
        != summary.original_matching_failure_count
    {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
    }

    let selected_candidate_id = trace.candidate().canonical_identity().to_hex();
    let mut candidate_ids = BTreeSet::new();
    let mut selected_count = 0_usize;
    for candidate in &summary.candidates {
        if !valid_digest(&candidate.candidate_id)
            || !candidate_ids.insert(candidate.candidate_id.as_str())
        {
            return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
        }
        let indexed_candidate = validate_candidate_evidence(
            artifact,
            candidate,
            &expected_failure,
            (candidate.candidate_id == selected_candidate_id).then_some(trace),
        )?;
        if candidate.candidate_id == selected_candidate_id {
            selected_count = selected_count.saturating_add(1);
            if !candidate.accepted || &indexed_candidate != trace.candidate() {
                return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
            }
        }
    }
    if selected_count != 1 {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
    }
    Ok(())
}

fn validate_original_attempts(
    artifact: &VerifiedRunArtifact,
    attempts: &[ShrinkAttemptDocument],
    expected: &FailureIdentity,
    authority: &CompiledCaseTrace,
) -> Result<Vec<AttemptResult>, ConfiguredMinimizedReplayArtifactError> {
    if !has_distinct_fresh_baselines(attempts) {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
    }
    let mut results = Vec::with_capacity(ATTEMPT_COUNT);
    for (index, attempt) in attempts.iter().enumerate() {
        let attempt_number = index + 1;
        let attempt_id = format!("attempt_{attempt_number:04}");
        let result_path = PathBuf::from(format!("original/attempts/{attempt_id}/result.json"));
        let indexed_result: ShrinkAttemptDocument = decode_indexed(artifact, &result_path)?;
        if &indexed_result != attempt {
            return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
        }
        let trace_path = PathBuf::from(format!("original/attempts/{attempt_id}/trace.json"));
        let attempt_trace: CompiledCaseTrace = decode_indexed(artifact, &trace_path)?;
        if !authority.matches_replay_authority(&attempt_trace) {
            return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
        }
        results.push(validate_attempt_document(
            attempt,
            attempt_number,
            authority.action_count(),
            expected,
        )?);
    }
    Ok(results)
}

fn validate_candidate_evidence(
    artifact: &VerifiedRunArtifact,
    evaluation: &CandidateEvaluationDocument,
    expected: &FailureIdentity,
    selected_authority: Option<&CompiledShrinkTrace>,
) -> Result<ShrinkCandidate, ConfiguredMinimizedReplayArtifactError> {
    let candidate_dir = format!("candidates/{}", evaluation.candidate_id);
    let candidate_path = PathBuf::from(format!("{candidate_dir}/candidate.json"));
    let candidate: ShrinkCandidate = decode_indexed(artifact, &candidate_path)?;
    let expected_complexity = serde_json::to_value(candidate.complexity())
        .map_err(|_| ConfiguredMinimizedReplayArtifactError::InvalidSummary)?;
    if evaluation.schema_version != 1
        || evaluation.candidate_id != candidate.canonical_identity().to_hex()
        || evaluation.action_count != candidate.actions().len()
        || evaluation.complexity != expected_complexity
        || evaluation.attempts.len() != ATTEMPT_COUNT
        || !has_distinct_fresh_baselines(&evaluation.attempts)
    {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
    }
    let evaluation_path = PathBuf::from(format!("{candidate_dir}/evaluation.json"));
    let indexed_evaluation: CandidateEvaluationDocument =
        decode_indexed(artifact, &evaluation_path)?;
    if &indexed_evaluation != evaluation {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
    }

    let mut results = Vec::with_capacity(ATTEMPT_COUNT);
    let mut candidate_authority = None;
    for (index, attempt) in evaluation.attempts.iter().enumerate() {
        let attempt_number = index + 1;
        let attempt_id = format!("attempt_{attempt_number:04}");
        let result_path =
            PathBuf::from(format!("{candidate_dir}/attempts/{attempt_id}/result.json"));
        let indexed_result: ShrinkAttemptDocument = decode_indexed(artifact, &result_path)?;
        if &indexed_result != attempt {
            return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
        }
        let trace_path = PathBuf::from(format!("{candidate_dir}/attempts/{attempt_id}/trace.json"));
        let attempt_trace: CompiledShrinkTrace = decode_indexed(artifact, &trace_path)?;
        if attempt_trace.candidate() != &candidate
            || candidate_authority
                .as_ref()
                .is_some_and(|authority: &CompiledShrinkTrace| {
                    !authority.matches_replay_authority(&attempt_trace)
                })
            || selected_authority
                .is_some_and(|authority| !authority.matches_replay_authority(&attempt_trace))
        {
            return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
        }
        candidate_authority.get_or_insert(attempt_trace);
        results.push(validate_attempt_document(
            attempt,
            attempt_number,
            candidate.actions().len(),
            expected,
        )?);
    }
    let observed_matching = matching_failure_count(&results, expected);
    if evaluation.matching_failure_count != observed_matching
        || evaluation.accepted != (observed_matching >= 2)
    {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
    }
    Ok(candidate)
}

fn validate_attempt_document(
    attempt: &ShrinkAttemptDocument,
    attempt_number: usize,
    action_count: usize,
    expected: &FailureIdentity,
) -> Result<AttemptResult, ConfiguredMinimizedReplayArtifactError> {
    if attempt.schema_version != 1
        || attempt.attempt != attempt_number
        || !attempt.trace_matches_authority
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
        || attempt.invariants.len() != 5
    {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
    }
    let mut identities = BTreeSet::new();
    let mut outcomes = Vec::with_capacity(attempt.invariants.len());
    for invariant in &attempt.invariants {
        let identity = FailureIdentity::new(
            InvariantId::new(&invariant.invariant_id)
                .map_err(|_| ConfiguredMinimizedReplayArtifactError::InvalidSummary)?,
            CheckpointId::new(&invariant.checkpoint_id)
                .map_err(|_| ConfiguredMinimizedReplayArtifactError::InvalidSummary)?,
        );
        if !identities.insert((
            invariant.invariant_id.as_str(),
            invariant.checkpoint_id.as_str(),
        )) || invariant.violated != (invariant.witness_count > 0)
        {
            return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
        }
        outcomes.push((identity, invariant.violated));
    }
    let result = attempt_result(
        expected,
        outcomes
            .iter()
            .map(|(identity, violated)| (identity, *violated)),
    );
    let declared = attempt
        .failure_identity
        .as_ref()
        .map(FailureIdentityDocument::to_identity)
        .transpose()?;
    let coherent = match (&attempt.verdict, &declared, &result) {
        (ShrinkAttemptVerdictDocument::Held, None, AttemptResult::Held) => true,
        (
            ShrinkAttemptVerdictDocument::ExpectedViolation,
            Some(declared),
            AttemptResult::Violation(actual),
        ) => declared == expected && actual == expected,
        (
            ShrinkAttemptVerdictDocument::OtherViolation,
            Some(declared),
            AttemptResult::Violation(actual),
        ) => declared == actual && actual != expected,
        _ => false,
    };
    if !coherent {
        return Err(ConfiguredMinimizedReplayArtifactError::InvalidSummary);
    }
    Ok(result)
}

fn matching_failure_count(attempts: &[AttemptResult], expected: &FailureIdentity) -> usize {
    attempts
        .iter()
        .filter(
            |attempt| matches!(attempt, AttemptResult::Violation(identity) if identity == expected),
        )
        .count()
}

fn has_distinct_fresh_baselines(attempts: &[ShrinkAttemptDocument]) -> bool {
    attempts.len() == ATTEMPT_COUNT
        && attempts
            .iter()
            .map(|attempt| attempt.after_database_oid)
            .collect::<BTreeSet<_>>()
            .len()
            == ATTEMPT_COUNT
        && attempts
            .iter()
            .map(|attempt| attempt.after_marker_uuid.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            == ATTEMPT_COUNT
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_run_id(value: &str) -> bool {
    value.starts_with("run_")
        && (5..=80).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_case_id(value: &str) -> bool {
    value.len() == 9
        && value.starts_with("case_")
        && value.as_bytes()[5..].iter().all(u8::is_ascii_digit)
}

fn output_base_is_disjoint(
    source_artifact: &Path,
    output_base: &Path,
) -> Result<bool, std::io::Error> {
    let source_artifact = source_artifact.canonicalize()?;
    let output_base = resolve_without_creating(output_base)?;
    Ok(!output_base.starts_with(source_artifact))
}

fn resolve_without_creating(path: &Path) -> Result<PathBuf, std::io::Error> {
    let mut unresolved = Vec::new();
    let mut existing = path;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "output path has no existing ancestor",
                    )
                })?;
                unresolved.push(name.to_owned());
                existing = existing.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "output path has no existing ancestor",
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
    let mut resolved = existing.canonicalize()?;
    for component in unresolved.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

/// Replays the authority-bound minimized trace from one verified shrink
/// artifact exactly three times from fresh compatible baselines.
///
/// The complete source artifact, its canonical minimized-trace authority, and
/// all source/summary coherence are verified before configuration or stack
/// access.
///
/// # Errors
///
/// Returns [`ConfiguredMinimizedReplayError`] for invalid source evidence,
/// preparation, compatibility, reset, execution, recovery, or evidence
/// failures.
pub async fn run_configured_minimized_replay(
    artifact_path: &Path,
    config_path: &Path,
    environment: &impl EnvironmentLookup,
) -> Result<ConfiguredMinimizedReplayOutput, ConfiguredMinimizedReplayError> {
    Box::pin(run_configured_minimized_replay_with_cancellation(
        artifact_path,
        config_path,
        environment,
        &RunCancellation::new(),
    ))
    .await
}

/// Executes minimized replay with a root cancellation capability.
///
/// # Errors
///
/// Returns [`ConfiguredMinimizedReplayError`] after recovery and
/// partial-evidence finalization when cancellation interrupts an entered
/// replay boundary.
#[allow(clippy::too_many_lines)]
pub async fn run_configured_minimized_replay_with_cancellation(
    artifact_path: &Path,
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    cancellation: &RunCancellation,
) -> Result<ConfiguredMinimizedReplayOutput, ConfiguredMinimizedReplayError> {
    let artifact = verify_complete_run_artifact(artifact_path)
        .map_err(ConfiguredMinimizedReplayArtifactError::Artifact)?;
    let source = load_verified_minimized_shrink_source(&artifact)?;
    preflight_reference_shrink_candidate(source.trace().candidate(), true, true)?;

    let config = load_resolved_config(config_path, environment)?;
    if !output_base_is_disjoint(artifact_path, config.artifact_dir())
        .map_err(ConfiguredMinimizedReplayError::OutputBoundary)?
    {
        return Err(ConfiguredMinimizedReplayError::OutputOverlapsSource);
    }
    let configured_probe = load_configured_sql_probe(&config)?;
    let configured_quiescence = load_configured_quiescence(&config)?;
    let configured_snapshot = load_configured_snapshot(&config)?;
    validate_replay_command_paths(artifact.root(), config.source_path())
        .map_err(ConfiguredMinimizedReplayError::ReportPath)?;
    if cancellation.is_cancelled() {
        return Err(ConfiguredMinimizedReplayError::Interrupted);
    }
    let _project_lock = ComposeProjectLock::try_acquire(config.root(), config.compose_project())
        .map_err(|error| ConfiguredMinimizedReplayError::ProjectLock(Box::new(error)))?;
    let (baseline, current_compatibility) = attest_replay_boundary(
        &config,
        &configured_probe,
        &configured_quiescence,
        &configured_snapshot,
    )
    .await
    .map_err(ConfiguredMinimizedReplayError::Boundary)?;
    artifact
        .compatibility()
        .require_exact_match(&current_compatibility)?;
    if cancellation.is_cancelled() {
        return Err(ConfiguredMinimizedReplayError::Interrupted);
    }
    let repository = capture_repository_provenance(config.root())
        .await
        .map_err(|error| ConfiguredMinimizedReplayError::Repository(Box::new(error)))?;

    let replay_id = format!("run_{}", uuid::Uuid::new_v4().simple());
    let mut artifacts = RunArtifactStaging::create_v2(
        config.root(),
        config.artifact_dir(),
        &replay_id,
        ManifestSeed::new(
            ArtifactKind::MinimizedReplay,
            repository,
            vec![artifact.source_identity()],
        ),
    )?;
    artifacts.write_json("config.redacted.json", config.redacted())?;
    artifacts.write_json("compatibility.json", &current_compatibility)?;
    artifacts.write_bytes("trace.minimized.json", source.trace_bytes())?;
    artifacts.write_json(
        "source.json",
        &MinimizedReplaySourceArtifact {
            schema_version: 1,
            source_shrink_id: source.shrink_id(),
            source_replay_id: source.source_replay_id(),
            source_run_id: source.source_run_id(),
            case_id: source.case_id(),
            expected_failure: source.expected_failure().into(),
        },
    )?;

    let mut attempt_artifacts = Vec::with_capacity(ATTEMPT_COUNT);
    let mut attempt_results = Vec::with_capacity(ATTEMPT_COUNT);
    let execution = Box::pin(execute_minimized_replay_attempts(
        &config,
        &configured_probe,
        &configured_quiescence,
        &configured_snapshot,
        baseline,
        artifact.compatibility(),
        &source,
        &replay_id,
        &mut artifacts,
        &mut attempt_artifacts,
        &mut attempt_results,
        cancellation,
    ))
    .await;
    if let Err(cause) = execution {
        return finalize_failed_minimized_replay(
            artifacts,
            cause,
            &replay_id,
            &source,
            &attempt_artifacts,
        );
    }

    let attempts: [AttemptResult; ATTEMPT_COUNT] = attempt_results
        .try_into()
        .map_err(|_| ConfiguredMinimizedReplayError::AttemptAccounting)?;
    let classification: ConfiguredReplayClassification =
        classify_reproduction(source.expected_failure(), &attempts).into();
    let matching_failure_count = attempts
        .iter()
        .filter(|attempt| {
            matches!(attempt, AttemptResult::Violation(identity) if identity == source.expected_failure())
        })
        .count();
    let summary = MinimizedReplaySummary {
        schema_version: 1,
        status: "configured_minimized_replay_complete",
        minimized_replay_id: &replay_id,
        source_shrink_id: source.shrink_id(),
        source_replay_id: source.source_replay_id(),
        source_run_id: source.source_run_id(),
        case_id: source.case_id(),
        expected_failure: source.expected_failure().into(),
        attempt_count: ATTEMPT_COUNT,
        matching_failure_count,
        classification,
        attempts: &attempt_artifacts,
    };
    artifacts.write_json(SUMMARY_FILE, &summary)?;
    let report = completed_minimized_replay_report(&summary, artifact.root(), config.source_path());
    write_report_bundle(&mut artifacts, &report)?;
    let result = match classification {
        ConfiguredReplayClassification::Stable | ConfiguredReplayClassification::Reproducible => {
            ArtifactResult::Counterexample
        }
        ConfiguredReplayClassification::Inconclusive => ArtifactResult::Inconclusive,
    };
    let artifact_path = artifacts.finalize_complete(
        result,
        vec![
            ArtifactAuthority::minimized_replay_source(),
            ArtifactAuthority::minimized_trace(),
        ],
    )?;
    Ok(ConfiguredMinimizedReplayOutput {
        schema_version: 1,
        status: "configured_minimized_replay_complete",
        minimized_replay_id: replay_id,
        source_shrink_id: source.shrink_id().to_owned(),
        source_replay_id: source.source_replay_id().to_owned(),
        case_id: source.case_id().to_owned(),
        attempt_count: ATTEMPT_COUNT,
        matching_failure_count,
        classification,
        artifact_path,
    })
}

#[allow(clippy::too_many_arguments)]
async fn execute_minimized_replay_attempts(
    config: &ResolvedConfig,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    baseline: ConfiguredBaselineSession,
    expected_compatibility: &RunCompatibilityV1,
    source: &VerifiedMinimizedShrinkSource,
    replay_id: &str,
    artifacts: &mut RunArtifactStaging,
    attempt_artifacts: &mut Vec<MinimizedReplayAttemptArtifact>,
    attempt_results: &mut Vec<AttemptResult>,
    cancellation: &RunCancellation,
) -> Result<(), ConfiguredMinimizedReplayError> {
    let mut initial_baseline = Some(baseline);
    let mut fresh_database_oids = BTreeSet::new();
    let mut fresh_marker_uuids = BTreeSet::new();
    for attempt in 1..=ATTEMPT_COUNT {
        if cancellation.is_cancelled() {
            return Err(ConfiguredMinimizedReplayError::Interrupted);
        }
        let current_baseline = if attempt == 1 {
            initial_baseline
                .take()
                .ok_or(ConfiguredMinimizedReplayError::AttemptAccounting)?
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
        let journal_path =
            artifacts.prepare_path(format!("attempts/{attempt_id}/observations.ndjson"))?;
        let (_, execution) = Box::pin(execute_configured_shrink_attempt_with_timeout(
            config,
            configured_probe,
            configured_quiescence,
            configured_snapshot,
            current_baseline,
            source.trace().candidate(),
            replay_id,
            &format!("{}_minimized_{attempt_id}", source.case_id()),
            journal_path,
            config.case_timeout(),
            cancellation,
        ))
        .await?;
        let trace_matches_minimized_authority =
            source.trace().matches_replay_authority(execution.trace());
        let result = configured_attempt_result(source.expected_failure(), execution.invariants());
        let evidence = minimized_replay_attempt_artifact(
            attempt,
            &execution,
            &result,
            source.expected_failure(),
            trace_matches_minimized_authority,
        );
        artifacts.write_json(
            format!("attempts/{attempt_id}/trace.json"),
            execution.trace(),
        )?;
        write_invariant_witness_artifacts(
            artifacts,
            format!("attempts/{attempt_id}/invariants"),
            execution.invariants(),
            witness_projection_policy(config),
        )?;
        artifacts.write_json(format!("attempts/{attempt_id}/result.json"), &evidence)?;
        attempt_artifacts.push(evidence);
        if !trace_matches_minimized_authority {
            return Err(ConfiguredMinimizedReplayError::TraceDiverged { attempt });
        }
        if !fresh_database_oids.insert(execution.reset().after_database_oid())
            || !fresh_marker_uuids.insert(execution.reset().after_marker_uuid().to_owned())
        {
            return Err(ConfiguredMinimizedReplayError::FreshBaselineReused { attempt });
        }
        attempt_results.push(result);
    }
    Ok(())
}

async fn fresh_compatible_baseline(
    config: &ResolvedConfig,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    expected_compatibility: &RunCompatibilityV1,
) -> Result<ConfiguredBaselineSession, ConfiguredMinimizedReplayError> {
    let (baseline, compatibility) = attest_replay_boundary(
        config,
        configured_probe,
        configured_quiescence,
        configured_snapshot,
    )
    .await
    .map_err(ConfiguredMinimizedReplayError::Boundary)?;
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

fn minimized_replay_attempt_artifact(
    attempt: usize,
    execution: &ConfiguredShrinkExecution,
    result: &AttemptResult,
    expected: &FailureIdentity,
    trace_matches_minimized_authority: bool,
) -> MinimizedReplayAttemptArtifact {
    let (verdict, failure_identity) = match result {
        AttemptResult::Held => (MinimizedReplayAttemptVerdict::Held, None),
        AttemptResult::Violation(identity) if identity == expected => (
            MinimizedReplayAttemptVerdict::ExpectedViolation,
            Some(identity.into()),
        ),
        AttemptResult::Violation(identity) => (
            MinimizedReplayAttemptVerdict::OtherViolation,
            Some(identity.into()),
        ),
        AttemptResult::Inconclusive => unreachable!("only completed oracle attempts are retained"),
    };
    MinimizedReplayAttemptArtifact {
        schema_version: 1,
        attempt,
        trace_matches_minimized_authority,
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
            .map(|outcome| MinimizedReplayInvariantArtifact {
                invariant_id: outcome.identity().invariant().as_str().to_owned(),
                checkpoint_id: outcome.identity().checkpoint().as_str().to_owned(),
                violated: outcome.violated(),
                witness_count: outcome.witness_count(),
            })
            .collect(),
    }
}

fn completed_minimized_replay_report(
    summary: &MinimizedReplaySummary<'_>,
    source_shrink_artifact_path: &Path,
    config_path: &Path,
) -> ArtifactReport {
    let (conclusion, outcome, stability, message) = match summary.classification {
        ConfiguredReplayClassification::Stable => (
            ReportConclusion::Counterexample,
            ReportCheckOutcome::Failed,
            format!(
                "Stable: the minimized failure identity reproduced {}/{} times.",
                summary.matching_failure_count, summary.attempt_count
            ),
            "authority-bound minimized counterexample reproduced on all fresh baselines",
        ),
        ConfiguredReplayClassification::Reproducible => (
            ReportConclusion::Counterexample,
            ReportCheckOutcome::Failed,
            format!(
                "Reproducible: the minimized failure identity reproduced {}/{} times.",
                summary.matching_failure_count, summary.attempt_count
            ),
            "authority-bound minimized counterexample matched on two fresh baselines",
        ),
        ConfiguredReplayClassification::Inconclusive => (
            ReportConclusion::Inconclusive,
            ReportCheckOutcome::Skipped,
            format!(
                "Inconclusive: the minimized failure identity reproduced {}/{} times.",
                summary.matching_failure_count, summary.attempt_count
            ),
            "fewer than two minimized replay attempts matched the expected failure",
        ),
    };
    let mut report = ArtifactReport::new(
        summary.minimized_replay_id,
        ReportArtifactKind::MinimizedReplay,
        conclusion,
        format!(
            "Exactly {} fresh-baseline attempts; compatibility is recaptured before attempts two and three.",
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
    report.add_fact(ReportFact::new("Source shrink", summary.source_shrink_id));
    report.add_fact(ReportFact::new("Source replay", summary.source_replay_id));
    report.add_fact(ReportFact::new("Source run", summary.source_run_id));
    report.add_fact(ReportFact::new("Case", summary.case_id));
    report.add_fact(ReportFact::new(
        "Matching attempts",
        format!(
            "{}/{}",
            summary.matching_failure_count, summary.attempt_count
        ),
    ));
    report.add_replay_command(ReplayCommand::minimized(
        source_shrink_artifact_path,
        config_path,
    ));
    report
}

fn finalize_failed_minimized_replay(
    mut artifacts: RunArtifactStaging,
    cause: ConfiguredMinimizedReplayError,
    replay_id: &str,
    source: &VerifiedMinimizedShrinkSource,
    attempts: &[MinimizedReplayAttemptArtifact],
) -> Result<ConfiguredMinimizedReplayOutput, ConfiguredMinimizedReplayError> {
    let failure_class = cause.failure_class();
    let failure_code = cause.failure_code();
    if let Err(artifact) = artifacts.write_json(
        SUMMARY_FILE,
        &PartialMinimizedReplaySummary {
            schema_version: 1,
            status: partial_status(failure_class),
            minimized_replay_id: replay_id,
            source_shrink_id: source.shrink_id(),
            source_replay_id: source.source_replay_id(),
            case_id: source.case_id(),
            failure_class,
            failure_code,
            completed_attempts: attempts.len(),
            attempts,
        },
    ) {
        return Err(ConfiguredMinimizedReplayError::PartialFinalization {
            cause: Box::new(cause),
            artifact,
        });
    }
    let report = partial_minimized_replay_report(
        replay_id,
        failure_class,
        failure_code,
        source.shrink_id(),
        source.source_replay_id(),
        source.case_id(),
        attempts.len(),
    );
    if let Err(artifact) = write_report_bundle(&mut artifacts, &report) {
        return Err(ConfiguredMinimizedReplayError::PartialFinalization {
            cause: Box::new(cause),
            artifact,
        });
    }
    let artifact_path = match artifacts.finalize_partial_v2(
        partial_artifact_class(failure_class),
        failure_code,
        vec![
            ArtifactAuthority::minimized_replay_source(),
            ArtifactAuthority::minimized_trace(),
        ],
    ) {
        Ok(path) => path,
        Err(artifact) => {
            return Err(ConfiguredMinimizedReplayError::PartialFinalization {
                cause: Box::new(cause),
                artifact,
            });
        }
    };
    Err(ConfiguredMinimizedReplayError::RunFailed {
        cause: Box::new(cause),
        artifact_path,
    })
}

fn partial_minimized_replay_report(
    replay_id: &str,
    failure_class: ConfiguredCampaignFailureClass,
    failure_code: &'static str,
    source_shrink_id: &str,
    source_replay_id: &str,
    case_id: &str,
    completed_attempts: usize,
) -> ArtifactReport {
    let mut report = partial_artifact_report(
        replay_id,
        ReportArtifactKind::MinimizedReplay,
        failure_class.into(),
        failure_code,
    );
    report.add_fact(ReportFact::new("Source shrink", source_shrink_id));
    report.add_fact(ReportFact::new("Source replay", source_replay_id));
    report.add_fact(ReportFact::new("Case", case_id));
    report.add_fact(ReportFact::new(
        "Completed attempts",
        completed_attempts.to_string(),
    ));
    report
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
pub enum ConfiguredMinimizedReplayArtifactError {
    #[error("minimized replay source artifact is invalid: {0}")]
    Artifact(#[from] ArtifactError),
    #[error("minimized replay requires manifest schema 2")]
    UnsupportedManifest,
    #[error("minimized replay requires a configured-shrink artifact")]
    InvalidArtifactKind,
    #[error("minimized replay source result cannot contain replayable minimized evidence")]
    InvalidArtifactResult,
    #[error("configured-shrink artifact has no authority-bound minimized trace")]
    MissingMinimizedAuthority,
    #[error("minimized replay artifact document {path} is invalid: {source}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("configured-shrink source document is invalid or incoherent")]
    InvalidSource,
    #[error("configured-shrink summary is invalid or incoherent")]
    InvalidSummary,
}

/// Failure to prepare, execute, recover, or persist minimized replay.
#[derive(Debug, Error)]
pub enum ConfiguredMinimizedReplayError {
    #[error("configured minimized replay source artifact is invalid: {0}")]
    SourceArtifact(#[from] ConfiguredMinimizedReplayArtifactError),
    #[error("configured minimized replay configuration failed: {0}")]
    Config(#[from] ConfigError),
    #[error("configured minimized replay could not resolve its output boundary: {0}")]
    OutputBoundary(#[source] std::io::Error),
    #[error("configured minimized replay output must be disjoint from its source artifact")]
    OutputOverlapsSource,
    #[error("configured minimized replay repository provenance failed: {0}")]
    Repository(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured minimized replay SQL probe could not be prepared: {0}")]
    SqlProbe(#[from] ConfiguredSqlProbeError),
    #[error("configured minimized replay quiescence query could not be prepared: {0}")]
    Quiescence(#[from] ConfiguredQuiescenceError),
    #[error("configured minimized replay invariant suite could not be prepared: {0}")]
    Snapshot(#[from] ConfiguredSnapshotError),
    #[error("configured minimized replay candidate preflight failed: {0}")]
    CandidatePreflight(#[from] ReferenceCaseRunError),
    #[error("configured minimized replay report path preflight failed: {0}")]
    ReportPath(#[source] ArtifactError),
    #[error("configured minimized replay compatibility boundary failed: {0}")]
    Boundary(#[source] ConfiguredReplayError),
    #[error("configured minimized replay compatibility gate failed: {0}")]
    Compatibility(#[from] CompatibilityError),
    #[error("configured minimized replay Compose project exclusion failed: {0}")]
    ProjectLock(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured minimized replay evidence failed: {0}")]
    Evidence(#[from] ArtifactError),
    #[error("configured minimized replay attempt failed: {0}")]
    Case(#[from] ConfiguredCampaignError),
    #[error("configured minimized replay attempt {attempt} diverged from trace authority")]
    TraceDiverged { attempt: usize },
    #[error("configured minimized replay attempt {attempt} reused a prior baseline identity")]
    FreshBaselineReused { attempt: usize },
    #[error("configured minimized replay completed an invalid number of attempts")]
    AttemptAccounting,
    #[error("configured minimized replay was interrupted")]
    Interrupted,
    #[error(
        "configured minimized replay failed; partial evidence retained at {artifact_path}: {cause}"
    )]
    RunFailed {
        #[source]
        cause: Box<ConfiguredMinimizedReplayError>,
        artifact_path: PathBuf,
    },
    #[error("configured minimized replay partial-evidence finalization failed: {artifact}")]
    PartialFinalization {
        #[source]
        cause: Box<ConfiguredMinimizedReplayError>,
        artifact: ArtifactError,
    },
}

impl ConfiguredMinimizedReplayError {
    #[must_use]
    pub fn failure_class(&self) -> ConfiguredCampaignFailureClass {
        match self {
            Self::SourceArtifact(_)
            | Self::Config(_)
            | Self::OutputBoundary(_)
            | Self::OutputOverlapsSource
            | Self::Repository(_)
            | Self::SqlProbe(_)
            | Self::Quiescence(_)
            | Self::Snapshot(_)
            | Self::CandidatePreflight(_)
            | Self::ReportPath(_)
            | Self::Compatibility(_) => ConfiguredCampaignFailureClass::Configuration,
            Self::Boundary(error) => error.failure_class(),
            Self::Case(error) => error.failure_class(),
            Self::TraceDiverged { .. }
            | Self::FreshBaselineReused { .. }
            | Self::AttemptAccounting => ConfiguredCampaignFailureClass::Inconclusive,
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
            Self::SourceArtifact(_) => "invalid_source_artifact",
            Self::Config(_) => "invalid_config",
            Self::OutputBoundary(_) => "output_boundary",
            Self::OutputOverlapsSource => "output_overlaps_source",
            Self::Repository(_) => "repository_provenance",
            Self::SqlProbe(_) => "invalid_sql_probe",
            Self::Quiescence(_) => "invalid_quiescence",
            Self::Snapshot(_) => "invalid_snapshot",
            Self::CandidatePreflight(_) => "candidate_preflight",
            Self::ReportPath(_) => "invalid_report_path",
            Self::Boundary(error) => error.failure_code(),
            Self::Compatibility(_) => "compatibility_mismatch",
            Self::ProjectLock(_) => "project_locked",
            Self::Evidence(_) => "artifact_failure",
            Self::Case(error) => error.failure_code(),
            Self::TraceDiverged { .. } => "trace_diverged",
            Self::FreshBaselineReused { .. } => "fresh_baseline_reused",
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

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::symlink, path::Path};

    use tiv_core::{
        decision::Seed,
        plan::{ActionBudget, CasePlanCompiler, PlanSpec},
        shrink::{CandidateGenerator, CandidateLimit},
        trace::{
            CaseCapturedValue, CaseOutputRef, CaseOutputSlot, CaseTraceMaterializer,
            CompiledCaseTrace, CompiledShrinkTrace, ShrinkTraceMaterializer,
        },
    };
    use uuid::Uuid;

    use super::{
        ConfiguredMinimizedReplayArtifactError, FailureIdentityDocument, MinimizedReplaySummary,
        ShrinkAttemptDocument, completed_minimized_replay_report, has_distinct_fresh_baselines,
        load_verified_minimized_shrink_source, output_base_is_disjoint,
        partial_minimized_replay_report,
    };
    use crate::{
        artifacts::{
            ArtifactAuthority, ArtifactKind, ArtifactResult, ManifestSeed, RepositoryProvenance,
            RunArtifactStaging, WorktreeState, verify_complete_run_artifact,
        },
        configured_campaign::ConfiguredCampaignFailureClass,
        configured_replay::ConfiguredReplayClassification,
        reports::{ReportCheckOutcome, ReportConclusion},
    };

    #[test]
    fn minimized_replay_report_maps_every_real_producer_classification() {
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
            let summary = MinimizedReplaySummary {
                schema_version: 1,
                status: "configured_minimized_replay_complete",
                minimized_replay_id: "run_minimized_report",
                source_shrink_id: "run_shrink",
                source_replay_id: "run_replay",
                source_run_id: "run_source",
                case_id: "case_0001",
                expected_failure: FailureIdentityDocument {
                    invariant_id: "provider_object_uniqueness".to_owned(),
                    checkpoint_id: "checkout_complete".to_owned(),
                },
                attempt_count: 3,
                matching_failure_count,
                classification,
                attempts: &[],
            };

            let report = completed_minimized_replay_report(
                &summary,
                Path::new("/tmp/shrink"),
                Path::new("/tmp/tiv.toml"),
            );

            assert_eq!(report.conclusion(), conclusion);
            assert_eq!(report.check_outcomes(), vec![outcome]);
            assert_eq!(report.replay_command_count(), 1);
        }
    }

    #[test]
    fn minimized_replay_partial_report_maps_failure_class_without_a_replay_command() {
        let report = partial_minimized_replay_report(
            "run_partial_minimized",
            ConfiguredCampaignFailureClass::Infrastructure,
            "process_failure",
            "run_shrink",
            "run_replay",
            "case_0001",
            1,
        );

        assert_eq!(report.conclusion(), ReportConclusion::InfrastructureFailure);
        assert_eq!(report.check_outcomes(), vec![ReportCheckOutcome::Error]);
        assert_eq!(report.replay_command_count(), 0);
    }

    #[test]
    fn source_attempts_must_bind_three_distinct_fresh_baselines() {
        let mut attempts = (1..=3)
            .map(|attempt| expected_violation_attempt(attempt, 4))
            .collect::<Vec<_>>();
        attempts[1]["after_database_oid"] = attempts[0]["after_database_oid"].clone();
        attempts[1]["after_marker_uuid"] = attempts[0]["after_marker_uuid"].clone();
        let attempts = attempts
            .into_iter()
            .map(serde_json::from_value::<ShrinkAttemptDocument>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(!has_distinct_fresh_baselines(&attempts));
    }

    #[test]
    fn output_base_must_not_resolve_inside_the_source_artifact() {
        let root = std::env::temp_dir().join(format!("tiv-minimized-replay-{}", Uuid::new_v4()));
        let source = root.join("source");
        let safe = root.join("runs");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&safe).unwrap();
        symlink(&source, root.join("source-alias")).unwrap();

        assert!(output_base_is_disjoint(&source, &safe).unwrap());
        assert!(!output_base_is_disjoint(&source, &source).unwrap());
        assert!(!output_base_is_disjoint(&source, &source.join("runs")).unwrap());
        assert!(!output_base_is_disjoint(&source, &root.join("source-alias/runs")).unwrap());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loader_rejects_legacy_manifests_before_searching_for_trace_files() {
        let root = std::env::temp_dir().join(format!("tiv-minimized-replay-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut legacy = RunArtifactStaging::create(&root, &base, "run_legacy_shrink").unwrap();
        legacy
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        legacy
            .write_json("summary.json", &serde_json::json!({"status": "fixture"}))
            .unwrap();
        let verified = verify_complete_run_artifact(&legacy.finalize().unwrap()).unwrap();

        assert!(matches!(
            load_verified_minimized_shrink_source(&verified),
            Err(ConfiguredMinimizedReplayArtifactError::UnsupportedManifest)
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loader_rejects_an_indexed_but_unbound_minimized_trace() {
        let root = std::env::temp_dir().join(format!("tiv-minimized-replay-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");

        let mut legacy =
            RunArtifactStaging::create(&root, &base, "run_legacy_replay_source").unwrap();
        legacy
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        legacy
            .write_json("summary.json", &serde_json::json!({"status": "fixture"}))
            .unwrap();
        let legacy = verify_complete_run_artifact(&legacy.finalize().unwrap()).unwrap();

        let repository =
            RepositoryProvenance::captured(&"a".repeat(40), WorktreeState::Clean, &"b".repeat(64))
                .unwrap();
        let mut shrink = RunArtifactStaging::create_v2(
            &root,
            &base,
            "run_shrink_source",
            ManifestSeed::new(
                ArtifactKind::Shrink,
                repository,
                vec![legacy.source_identity()],
            ),
        )
        .unwrap();
        shrink
            .write_json(
                "config.redacted.json",
                &serde_json::json!({"schema_version": 1}),
            )
            .unwrap();
        shrink
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        for path in [
            "source.json",
            "trace.original.json",
            "trace.minimized.json",
            "summary.json",
        ] {
            shrink
                .write_json(path, &serde_json::json!({"schema_version": 1}))
                .unwrap();
        }
        let shrink = shrink
            .finalize_complete(
                ArtifactResult::Counterexample,
                vec![
                    ArtifactAuthority::shrink_source(),
                    ArtifactAuthority::original_trace(),
                ],
            )
            .unwrap();
        let verified = verify_complete_run_artifact(&shrink).unwrap();

        assert!(matches!(
            load_verified_minimized_shrink_source(&verified),
            Err(ConfiguredMinimizedReplayArtifactError::MissingMinimizedAuthority)
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn loader_accepts_one_coherent_authority_bound_minimized_trace() {
        let root = std::env::temp_dir().join(format!("tiv-minimized-replay-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");

        let mut replay = RunArtifactStaging::create(&root, &base, "run_replay_source").unwrap();
        replay
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        replay
            .write_json("summary.json", &serde_json::json!({"status": "fixture"}))
            .unwrap();
        let replay = verify_complete_run_artifact(&replay.finalize().unwrap()).unwrap();

        let trace = minimized_trace();
        let original_trace = original_trace(trace.candidate().source());
        let candidate_id = trace.candidate().canonical_identity().to_hex();
        let failure = serde_json::json!({
            "invariant_id": "provider-object-unique",
            "checkpoint_id": "checkout-quiescent"
        });
        let repository =
            RepositoryProvenance::captured(&"a".repeat(40), WorktreeState::Clean, &"b".repeat(64))
                .unwrap();
        let mut shrink = RunArtifactStaging::create_v2(
            &root,
            &base,
            "run_shrink_source",
            ManifestSeed::new(
                ArtifactKind::Shrink,
                repository,
                vec![replay.source_identity()],
            ),
        )
        .unwrap();
        shrink
            .write_json(
                "config.redacted.json",
                &serde_json::json!({"schema_version": 1}),
            )
            .unwrap();
        shrink
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        shrink
            .write_json(
                "source.json",
                &serde_json::json!({
                    "schema_version": 1,
                    "source_replay_id": "run_replay_source",
                    "source_run_id": "run_campaign_source",
                    "case_id": "case_0001",
                    "expected_failure": failure,
                    "original_trace_retained": true
                }),
            )
            .unwrap();
        shrink
            .write_json("trace.original.json", &original_trace)
            .unwrap();
        shrink.write_json("trace.minimized.json", &trace).unwrap();
        let original_attempts = (1..=3)
            .map(|attempt| expected_violation_attempt(attempt, original_trace.action_count()))
            .collect::<Vec<_>>();
        let candidate_attempts = (1..=3)
            .map(|attempt| expected_violation_attempt(attempt, trace.action_count()))
            .collect::<Vec<_>>();
        for (index, attempt) in original_attempts.iter().enumerate() {
            let attempt_id = format!("attempt_{:04}", index + 1);
            shrink
                .write_json(
                    format!("original/attempts/{attempt_id}/result.json"),
                    attempt,
                )
                .unwrap();
            shrink
                .write_json(
                    format!("original/attempts/{attempt_id}/trace.json"),
                    &original_trace,
                )
                .unwrap();
        }
        let candidate_evaluation = serde_json::json!({
            "schema_version": 1,
            "candidate_id": candidate_id,
            "action_count": trace.action_count(),
            "complexity": trace.candidate().complexity(),
            "matching_failure_count": 3,
            "accepted": true,
            "attempts": candidate_attempts
        });
        shrink
            .write_json(
                format!("candidates/{candidate_id}/candidate.json"),
                trace.candidate(),
            )
            .unwrap();
        shrink
            .write_json(
                format!("candidates/{candidate_id}/evaluation.json"),
                &candidate_evaluation,
            )
            .unwrap();
        for (index, attempt) in candidate_attempts.iter().enumerate() {
            let attempt_id = format!("attempt_{:04}", index + 1);
            shrink
                .write_json(
                    format!("candidates/{candidate_id}/attempts/{attempt_id}/result.json"),
                    attempt,
                )
                .unwrap();
            shrink
                .write_json(
                    format!("candidates/{candidate_id}/attempts/{attempt_id}/trace.json"),
                    &trace,
                )
                .unwrap();
        }
        shrink
            .write_json(
                "summary.json",
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "configured_shrink_complete",
                    "shrink_id": "run_shrink_source",
                    "source_replay_id": "run_replay_source",
                    "source_run_id": "run_campaign_source",
                    "case_id": "case_0001",
                    "expected_failure": failure,
                    "completion": "complete",
                    "candidate_limit": 60,
                    "max_time_milliseconds": 600_000,
                    "original_attempt_count": 3,
                    "original_matching_failure_count": 3,
                    "evaluated_candidates": 1,
                    "cache_hits": 0,
                    "accepted_candidates": 1,
                    "original_action_count": trace.candidate().source().actions().len(),
                    "best_action_count": trace.action_count(),
                    "minimized_trace_written": true,
                    "original_attempts": original_attempts,
                    "candidates": [candidate_evaluation]
                }),
            )
            .unwrap();
        let shrink = shrink
            .finalize_complete(
                ArtifactResult::Counterexample,
                vec![
                    ArtifactAuthority::shrink_source(),
                    ArtifactAuthority::original_trace(),
                    ArtifactAuthority::minimized_trace(),
                ],
            )
            .unwrap();
        let verified = verify_complete_run_artifact(&shrink).unwrap();

        let source = load_verified_minimized_shrink_source(&verified).unwrap();
        assert_eq!(source.shrink_id(), "run_shrink_source");
        assert_eq!(source.source_replay_id(), "run_replay_source");
        assert_eq!(source.source_run_id(), "run_campaign_source");
        assert_eq!(source.case_id(), "case_0001");
        assert_eq!(source.trace(), &trace);
        let mut expected_trace_bytes = serde_json::to_vec_pretty(&trace).unwrap();
        expected_trace_bytes.push(b'\n');
        assert_eq!(source.trace_bytes(), expected_trace_bytes);

        fs::remove_dir_all(root).unwrap();
    }

    fn minimized_trace() -> CompiledShrinkTrace {
        let source = CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
            Seed::new(42),
            ActionBudget::new(40).unwrap(),
        ))
        .unwrap();
        let candidate = CandidateGenerator::new(&source)
            .unwrap()
            .candidates(None, CandidateLimit::default())
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        let captures = captured_values(
            ShrinkTraceMaterializer::required_outputs(&candidate).unwrap(),
            "minimized",
        );
        ShrinkTraceMaterializer::materialize(&candidate, captures).unwrap()
    }

    fn original_trace(source: &tiv_core::plan::PlannedCase) -> CompiledCaseTrace {
        let captures = captured_values(
            CaseTraceMaterializer::required_outputs(source).unwrap(),
            "original",
        );
        CaseTraceMaterializer::materialize(source, captures).unwrap()
    }

    fn captured_values(
        outputs: Vec<CaseOutputRef>,
        label: &str,
    ) -> Vec<(CaseOutputRef, CaseCapturedValue)> {
        outputs
            .into_iter()
            .enumerate()
            .map(|(index, output)| {
                let ordinal = index + 1;
                let value = match output.slot() {
                    CaseOutputSlot::PaymentIntentId => {
                        CaseCapturedValue::payment_intent_id(format!("pi_{label}_{ordinal}"))
                            .unwrap()
                    }
                    CaseOutputSlot::EventId => {
                        CaseCapturedValue::event_id(format!("evt_{label}_{ordinal}")).unwrap()
                    }
                    CaseOutputSlot::ProviderGateId => {
                        CaseCapturedValue::provider_gate_id(u64::try_from(ordinal).unwrap())
                            .unwrap()
                    }
                };
                (output, value)
            })
            .collect()
    }

    fn expected_violation_attempt(attempt: usize, action_count: usize) -> serde_json::Value {
        let before = Uuid::from_u128(u128::try_from(attempt * 2 - 1).unwrap()).to_string();
        let after = Uuid::from_u128(u128::try_from(attempt * 2).unwrap()).to_string();
        serde_json::json!({
            "schema_version": 1,
            "attempt": attempt,
            "trace_matches_authority": true,
            "before_database_oid": 16_384 + attempt,
            "after_database_oid": 17_384 + attempt,
            "before_marker_uuid": before,
            "after_marker_uuid": after,
            "executed_action_count": action_count,
            "journal_record_count": action_count * 2,
            "journal_last_record_hash": "c".repeat(64),
            "provider_object_count": 1,
            "verdict": "expected_violation",
            "failure_identity": {
                "invariant_id": "provider-object-unique",
                "checkpoint_id": "checkout-quiescent"
            },
            "invariants": [
                {
                    "invariant_id": "provider-object-unique",
                    "checkpoint_id": "checkout-quiescent",
                    "violated": true,
                    "witness_count": 1
                },
                {
                    "invariant_id": "webhook-effect-at-most-once",
                    "checkpoint_id": "checkout-quiescent",
                    "violated": false,
                    "witness_count": 0
                },
                {
                    "invariant_id": "paid-order-amount-conservation",
                    "checkpoint_id": "checkout-quiescent",
                    "violated": false,
                    "witness_count": 0
                },
                {
                    "invariant_id": "terminal-success-monotonic",
                    "checkpoint_id": "checkout-quiescent",
                    "violated": false,
                    "witness_count": 0
                },
                {
                    "invariant_id": "balanced-ledger",
                    "checkpoint_id": "checkout-quiescent",
                    "violated": false,
                    "witness_count": 0
                }
            ]
        })
    }

    fn compatibility_fixture() -> serde_json::Value {
        serde_json::json!({
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
                "version": "5.5.0",
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
        })
    }
}
