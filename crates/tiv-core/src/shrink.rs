//! Deterministic, dependency-aware replay candidate generation.

use std::{
    collections::BTreeSet,
    fmt::{self, Write as _},
};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{
    plan::{
        Checkpoint, PlanActionKind, PlanValidationError, PlannedAction, PlannedCase,
        ProcessCutPoint, ProviderOutcome, ProviderOutcomeScript, SCHEDULER_ALGORITHM,
        normalize_replay_action_kinds, validate_replay_action_kinds,
    },
    trace::ActionId,
};

pub const SHRINK_CANDIDATE_SCHEMA_VERSION: u16 = 1;
pub const DEPENDENCY_AWARE_SHRINK_ALGORITHM: &str =
    "dev.txproof/dependency-aware-hierarchical-ddmin/v1";
pub const MAX_SHRINK_CANDIDATES: u8 = 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCandidateLimit {
    Zero,
    AboveV1Maximum,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateLimit(u8);

impl CandidateLimit {
    /// Creates a bounded v1 candidate limit.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCandidateLimit`] for zero or a value above the fixed
    /// 60-candidate v1 ceiling.
    pub const fn new(value: u8) -> Result<Self, InvalidCandidateLimit> {
        if value == 0 {
            return Err(InvalidCandidateLimit::Zero);
        }
        if value > MAX_SHRINK_CANDIDATES {
            return Err(InvalidCandidateLimit::AboveV1Maximum);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn value(self) -> u8 {
        self.0
    }
}

impl Default for CandidateLimit {
    fn default() -> Self {
        Self(MAX_SHRINK_CANDIDATES)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CandidateIdentity([u8; 32]);

impl CandidateIdentity {
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn to_hex(self) -> String {
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
        }
        encoded
    }
}

impl fmt::Display for CandidateIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ShrinkComplexity {
    action_count: u32,
    fault_action_count: u32,
    provider_fault_score: u32,
    provider_call_count: u32,
    delay_milliseconds: u64,
}

impl ShrinkComplexity {
    #[must_use]
    pub const fn action_count(self) -> u32 {
        self.action_count
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
struct ShrinkAction {
    source_action_id: ActionId,
    kind: PlanActionKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ShrinkCandidate {
    schema_version: u16,
    algorithm: String,
    source: PlannedCase,
    schedule: Vec<ShrinkAction>,
    #[serde(skip)]
    actions: Vec<PlannedAction>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateBatch {
    candidates: Vec<ShrinkCandidate>,
    truncated: bool,
}

impl CandidateBatch {
    #[must_use]
    pub fn candidates(&self) -> &[ShrinkCandidate] {
        &self.candidates
    }

    #[must_use]
    pub fn into_candidates(self) -> Vec<ShrinkCandidate> {
        self.candidates
    }

    #[must_use]
    pub const fn was_truncated(&self) -> bool {
        self.truncated
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShrinkCandidateWire {
    schema_version: u16,
    algorithm: String,
    source: PlannedCase,
    schedule: Vec<ShrinkAction>,
}

impl<'de> Deserialize<'de> for ShrinkCandidate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ShrinkCandidateWire::deserialize(deserializer)?;
        Self::from_schedule(
            wire.schema_version,
            wire.algorithm,
            wire.source,
            wire.schedule,
        )
        .map_err(|error| D::Error::custom(format_args!("invalid shrink candidate: {error:?}")))
    }
}

impl ShrinkCandidate {
    fn new(source: PlannedCase, schedule: Vec<ShrinkAction>) -> Result<Self, ShrinkError> {
        Self::materialize(
            SHRINK_CANDIDATE_SCHEMA_VERSION,
            DEPENDENCY_AWARE_SHRINK_ALGORITHM.to_owned(),
            source,
            schedule,
            false,
        )
    }

    fn from_schedule(
        schema_version: u16,
        algorithm: String,
        source: PlannedCase,
        schedule: Vec<ShrinkAction>,
    ) -> Result<Self, ShrinkError> {
        Self::materialize(schema_version, algorithm, source, schedule, true)
    }

    fn materialize(
        schema_version: u16,
        algorithm: String,
        source: PlannedCase,
        schedule: Vec<ShrinkAction>,
        validate_source: bool,
    ) -> Result<Self, ShrinkError> {
        if schema_version != SHRINK_CANDIDATE_SCHEMA_VERSION {
            return Err(ShrinkError::UnsupportedSchemaVersion);
        }
        if algorithm != DEPENDENCY_AWARE_SHRINK_ALGORITHM {
            return Err(ShrinkError::UnsupportedAlgorithm);
        }
        if validate_source {
            validate_seeded_source(&source)?;
        }
        validate_lineage(&source, &schedule)?;
        let kinds = schedule.iter().map(|entry| entry.kind).collect::<Vec<_>>();
        let eligible_counts = validate_replay_action_kinds(source.spec(), &kinds)
            .map_err(ShrinkError::InvalidSchedule)?;
        if !reference_cut_points_are_supported(&kinds) {
            return Err(ShrinkError::InvalidReferenceCutPoint);
        }
        if complexity(&kinds) >= complexity_for_source(&source) {
            return Err(ShrinkError::DoesNotSimplifySource);
        }
        let actions = materialize_actions(&kinds, &eligible_counts)?;
        Ok(Self {
            schema_version,
            algorithm,
            source,
            schedule,
            actions,
        })
    }

    /// Revalidates source provenance, lineage, state transitions, reference
    /// cut-point support, and strict complexity progress.
    ///
    /// # Errors
    ///
    /// Returns [`ShrinkError`] when any candidate authority is inconsistent.
    pub fn validate(&self) -> Result<(), ShrinkError> {
        let expected = Self::from_schedule(
            self.schema_version,
            self.algorithm.clone(),
            self.source.clone(),
            self.schedule.clone(),
        )?;
        if &expected != self {
            return Err(ShrinkError::InvalidMaterialization);
        }
        Ok(())
    }

    #[must_use]
    pub const fn source(&self) -> &PlannedCase {
        &self.source
    }

    #[must_use]
    pub fn actions(&self) -> &[PlannedAction] {
        &self.actions
    }

    #[must_use]
    pub fn complexity(&self) -> ShrinkComplexity {
        complexity(
            &self
                .schedule
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
        )
    }

    #[must_use]
    /// Returns the stable BLAKE3 identity used for candidate result caching.
    ///
    /// # Panics
    ///
    /// Panics only if serialization of the fixed, already validated in-memory
    /// candidate types fails while writing to an infallible byte buffer.
    pub fn canonical_identity(&self) -> CandidateIdentity {
        let bytes = serde_json::to_vec(&(
            "dev.txproof.shrink-candidate-identity.v1",
            &self.source,
            &self.schedule,
        ))
        .expect("validated shrink candidate fields always serialize");
        CandidateIdentity(*blake3::hash(&bytes).as_bytes())
    }
}

#[derive(Clone, Debug)]
pub struct CandidateGenerator {
    source: PlannedCase,
    source_complexity: ShrinkComplexity,
}

impl CandidateGenerator {
    /// Creates a generator rooted in one exact seeded compiler artifact.
    ///
    /// # Errors
    ///
    /// Returns [`ShrinkError`] when the source is not an exact seeded plan.
    pub fn new(source: &PlannedCase) -> Result<Self, ShrinkError> {
        validate_seeded_source(source)?;
        Ok(Self {
            source: source.clone(),
            source_complexity: complexity_for_source(source),
        })
    }

    #[must_use]
    pub const fn source_complexity(&self) -> ShrinkComplexity {
        self.source_complexity
    }

    /// Returns the next deterministic, deduplicated candidate frontier.
    ///
    /// The optional current candidate must descend from this generator's
    /// exact source. Every emitted candidate is strictly simpler than the
    /// current best and never exceeds `limit`.
    ///
    /// # Errors
    ///
    /// Returns [`ShrinkError`] for an invalid or foreign current candidate.
    #[allow(clippy::too_many_lines)]
    pub fn candidates(
        &self,
        current: Option<&ShrinkCandidate>,
        limit: CandidateLimit,
    ) -> Result<Vec<ShrinkCandidate>, ShrinkError> {
        Ok(self.candidate_batch(current, limit)?.into_candidates())
    }

    /// Returns the next deterministic, deduplicated candidate frontier plus a
    /// true truncation bit.
    ///
    /// `was_truncated` is set only when at least one additional unique
    /// candidate existed beyond `limit`; an exactly full frontier is reported
    /// as exhausted.
    ///
    /// # Errors
    ///
    /// Returns [`ShrinkError`] for an invalid or foreign current candidate.
    #[allow(clippy::too_many_lines)]
    pub fn candidate_batch(
        &self,
        current: Option<&ShrinkCandidate>,
        limit: CandidateLimit,
    ) -> Result<CandidateBatch, ShrinkError> {
        let schedule = match current {
            Some(candidate) => {
                candidate.validate()?;
                if candidate.source != self.source {
                    return Err(ShrinkError::SourceMismatch);
                }
                candidate.schedule.clone()
            }
            None => self
                .source
                .actions()
                .iter()
                .map(|action| ShrinkAction {
                    source_action_id: action.id(),
                    kind: *action.kind(),
                })
                .collect(),
        };
        let current_complexity =
            current.map_or(self.source_complexity, ShrinkCandidate::complexity);
        let mut candidates = Vec::new();
        let mut identities = BTreeSet::new();
        let limit_len = usize::from(limit.value());
        let detection_len = limit_len.saturating_add(1);

        for chunk_size in hierarchical_chunk_sizes(schedule.len()) {
            for start in (0..schedule.len()).step_by(chunk_size) {
                let end = start.saturating_add(chunk_size).min(schedule.len());
                let mut proposal = schedule.clone();
                proposal.drain(start..end);
                self.push_candidate(
                    &proposal,
                    current_complexity,
                    detection_len,
                    &mut identities,
                    &mut candidates,
                );
                if candidates.len() == detection_len {
                    candidates.truncate(limit_len);
                    return Ok(CandidateBatch {
                        candidates,
                        truncated: true,
                    });
                }
            }
        }

        for (index, entry) in schedule.iter().enumerate() {
            if matches!(
                entry.kind,
                PlanActionKind::RetryBusinessRequest { .. }
                    | PlanActionKind::RetryProviderRequest { .. }
                    | PlanActionKind::RetrievePaymentIntent
                    | PlanActionKind::ReleaseProviderGate
            ) {
                let mut proposal = schedule.clone();
                proposal.remove(index);
                self.push_candidate(
                    &proposal,
                    current_complexity,
                    detection_len,
                    &mut identities,
                    &mut candidates,
                );
            }
        }

        for (index, entry) in schedule.iter().enumerate() {
            match entry.kind {
                PlanActionKind::KillApplication { .. } => {
                    let mut proposal = schedule.clone();
                    proposal.remove(index);
                    if proposal.get(index).is_some_and(|next| {
                        matches!(next.kind, PlanActionKind::RestartAndAwaitHealth)
                    }) {
                        proposal.remove(index);
                    }
                    self.push_candidate(
                        &proposal,
                        current_complexity,
                        detection_len,
                        &mut identities,
                        &mut candidates,
                    );
                }
                PlanActionKind::DuplicateWebhook | PlanActionKind::ReorderWebhooks => {
                    let mut proposal = schedule.clone();
                    proposal.remove(index);
                    self.push_candidate(
                        &proposal,
                        current_complexity,
                        detection_len,
                        &mut identities,
                        &mut candidates,
                    );
                }
                PlanActionKind::DelayWebhook { milliseconds } => {
                    if milliseconds > 0 {
                        let mut proposal = schedule.clone();
                        proposal[index].kind = PlanActionKind::DelayWebhook { milliseconds: 0 };
                        self.push_candidate(
                            &proposal,
                            current_complexity,
                            detection_len,
                            &mut identities,
                            &mut candidates,
                        );
                    }
                    let mut proposal = schedule.clone();
                    proposal.remove(index);
                    self.push_candidate(
                        &proposal,
                        current_complexity,
                        detection_len,
                        &mut identities,
                        &mut candidates,
                    );
                }
                PlanActionKind::DropWebhook => {
                    let mut proposal = schedule.clone();
                    proposal[index].kind = PlanActionKind::DeliverWebhook;
                    self.push_candidate(
                        &proposal,
                        current_complexity,
                        detection_len,
                        &mut identities,
                        &mut candidates,
                    );
                }
                _ => {
                    for simplified in simplified_provider_actions(entry.kind) {
                        let mut proposal = schedule.clone();
                        proposal[index].kind = simplified;
                        self.push_candidate(
                            &proposal,
                            current_complexity,
                            detection_len,
                            &mut identities,
                            &mut candidates,
                        );
                    }
                }
            }
            if candidates.len() == detection_len {
                break;
            }
        }

        let truncated = candidates.len() > limit_len;
        candidates.truncate(limit_len);
        Ok(CandidateBatch {
            candidates,
            truncated,
        })
    }

    fn push_candidate(
        &self,
        proposal: &[ShrinkAction],
        current_complexity: ShrinkComplexity,
        target_len: usize,
        identities: &mut BTreeSet<CandidateIdentity>,
        candidates: &mut Vec<ShrinkCandidate>,
    ) {
        if candidates.len() == target_len {
            return;
        }
        let kinds = proposal.iter().map(|entry| entry.kind).collect::<Vec<_>>();
        let Some(retained) = normalize_replay_action_kinds(self.source.spec(), &kinds) else {
            return;
        };
        let normalized = retained
            .into_iter()
            .map(|index| proposal[index])
            .collect::<Vec<_>>();
        let Ok(candidate) = ShrinkCandidate::new(self.source.clone(), normalized) else {
            return;
        };
        if candidate.complexity() >= current_complexity {
            return;
        }
        let identity = candidate.canonical_identity();
        if identities.insert(identity) {
            candidates.push(candidate);
        }
    }
}

fn validate_seeded_source(source: &PlannedCase) -> Result<(), ShrinkError> {
    source.validate().map_err(ShrinkError::InvalidSource)?;
    if source.scheduler_algorithm() != SCHEDULER_ALGORITHM {
        return Err(ShrinkError::SourceIsNotSeeded);
    }
    Ok(())
}

fn validate_lineage(source: &PlannedCase, schedule: &[ShrinkAction]) -> Result<(), ShrinkError> {
    let mut previous = None;
    for entry in schedule {
        if previous.is_some_and(|id| entry.source_action_id <= id) {
            return Err(ShrinkError::InvalidLineage);
        }
        let original = source
            .actions()
            .iter()
            .find(|action| action.id() == entry.source_action_id)
            .ok_or(ShrinkError::InvalidLineage)?;
        if !kind_is_simplification(*original.kind(), entry.kind) {
            return Err(ShrinkError::InvalidLineage);
        }
        previous = Some(entry.source_action_id);
    }
    Ok(())
}

fn materialize_actions(
    kinds: &[PlanActionKind],
    eligible_counts: &[usize],
) -> Result<Vec<PlannedAction>, ShrinkError> {
    kinds
        .iter()
        .zip(eligible_counts)
        .enumerate()
        .map(|(index, (kind, eligible_count))| {
            let sequence = u32::try_from(index + 1).map_err(|_| ShrinkError::ActionLimit)?;
            Ok(PlannedAction::replay(
                sequence,
                index as u64,
                *eligible_count,
                *kind,
            ))
        })
        .collect()
}

fn hierarchical_chunk_sizes(action_count: usize) -> Vec<usize> {
    if action_count < 2 {
        return vec![1];
    }
    let mut result = Vec::new();
    let mut partitions = 2_usize;
    loop {
        let chunk_size = action_count.div_ceil(partitions).max(1);
        if result.last() != Some(&chunk_size) {
            result.push(chunk_size);
        }
        if chunk_size == 1 {
            break;
        }
        partitions = partitions.saturating_mul(2);
    }
    result
}

fn kind_is_simplification(source: PlanActionKind, candidate: PlanActionKind) -> bool {
    if source == candidate {
        return true;
    }
    match (source, candidate) {
        (
            PlanActionKind::DelayWebhook {
                milliseconds: source,
            },
            PlanActionKind::DelayWebhook {
                milliseconds: candidate,
            },
        ) => candidate <= source,
        (PlanActionKind::DropWebhook, PlanActionKind::DeliverWebhook) => true,
        (
            PlanActionKind::DriveCheckout {
                provider_script: source,
            },
            PlanActionKind::DriveCheckout {
                provider_script: candidate,
            },
        )
        | (
            PlanActionKind::RetryBusinessRequest {
                provider_script: source,
            },
            PlanActionKind::RetryBusinessRequest {
                provider_script: candidate,
            },
        )
        | (
            PlanActionKind::ConfirmPaymentIntent {
                provider_script: source,
            },
            PlanActionKind::ConfirmPaymentIntent {
                provider_script: candidate,
            },
        )
        | (
            PlanActionKind::RetryProviderRequest {
                provider_script: source,
            },
            PlanActionKind::RetryProviderRequest {
                provider_script: candidate,
            },
        ) => provider_script_is_simplification(source, candidate),
        _ => false,
    }
}

fn provider_script_is_simplification(
    source: ProviderOutcomeScript,
    candidate: ProviderOutcomeScript,
) -> bool {
    if source == candidate {
        return true;
    }
    if candidate == ProviderOutcomeScript::single(ProviderOutcome::Normal) {
        return true;
    }
    let source_outcomes = source.outcomes().collect::<Vec<_>>();
    let candidate_outcomes = candidate.outcomes().collect::<Vec<_>>();
    if source_outcomes.len() != 2 {
        return false;
    }
    candidate_outcomes == source_outcomes[..1]
        || (candidate_outcomes.len() == 2
            && candidate_outcomes[0] == source_outcomes[0]
            && candidate_outcomes[1] == ProviderOutcome::Normal)
}

fn simplified_provider_actions(kind: PlanActionKind) -> Vec<PlanActionKind> {
    let (script, build): (
        ProviderOutcomeScript,
        fn(ProviderOutcomeScript) -> PlanActionKind,
    ) = match kind {
        PlanActionKind::DriveCheckout { provider_script } => (provider_script, |provider_script| {
            PlanActionKind::DriveCheckout { provider_script }
        }),
        PlanActionKind::RetryBusinessRequest { provider_script } => {
            (provider_script, |provider_script| {
                PlanActionKind::RetryBusinessRequest { provider_script }
            })
        }
        PlanActionKind::ConfirmPaymentIntent { provider_script } => {
            (provider_script, |provider_script| {
                PlanActionKind::ConfirmPaymentIntent { provider_script }
            })
        }
        PlanActionKind::RetryProviderRequest { provider_script } => {
            (provider_script, |provider_script| {
                PlanActionKind::RetryProviderRequest { provider_script }
            })
        }
        _ => return Vec::new(),
    };
    let outcomes = script.outcomes().collect::<Vec<_>>();
    let mut simplified = Vec::new();
    if outcomes.len() == 2 {
        simplified.push(build(ProviderOutcomeScript::single(outcomes[0])));
        if outcomes[1] != ProviderOutcome::Normal {
            simplified.push(build(
                ProviderOutcomeScript::with_transport_retry(
                    ProviderOutcome::CommitThenClose,
                    ProviderOutcome::Normal,
                )
                .expect("two-call scripts always start with commit_then_close"),
            ));
        }
    }
    if script != ProviderOutcomeScript::single(ProviderOutcome::Normal) {
        simplified.push(build(ProviderOutcomeScript::single(
            ProviderOutcome::Normal,
        )));
    }
    simplified
}

fn reference_cut_points_are_supported(actions: &[PlanActionKind]) -> bool {
    actions.iter().enumerate().all(|(index, action)| {
        let PlanActionKind::KillApplication { cut_point } = action else {
            return true;
        };
        let Some(previous) = index.checked_sub(1).and_then(|index| actions.get(index)) else {
            return false;
        };
        match cut_point {
            ProcessCutPoint::ClientRequestForwarded => matches!(
                previous,
                PlanActionKind::DriveCheckout { provider_script }
                    | PlanActionKind::RetryBusinessRequest { provider_script }
                    if provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay
            ),
            ProcessCutPoint::ClientResponseObserved => matches!(
                previous,
                PlanActionKind::DriveCheckout { provider_script }
                    | PlanActionKind::RetryBusinessRequest { provider_script }
                    if provider_script.terminal_outcome() != ProviderOutcome::CommitThenDelay
            ),
            ProcessCutPoint::WebhookRequestForwarded | ProcessCutPoint::WebhookResponseObserved => {
                matches!(
                    previous,
                    PlanActionKind::DeliverWebhook | PlanActionKind::DuplicateWebhook
                )
            }
            ProcessCutPoint::SqlProbe => {
                matches!(previous, PlanActionKind::DriveCheckout { .. })
            }
        }
    })
}

fn complexity_for_source(source: &PlannedCase) -> ShrinkComplexity {
    complexity(
        &source
            .actions()
            .iter()
            .map(|action| *action.kind())
            .collect::<Vec<_>>(),
    )
}

fn complexity(actions: &[PlanActionKind]) -> ShrinkComplexity {
    let mut result = ShrinkComplexity {
        action_count: u32::try_from(actions.len()).unwrap_or(u32::MAX),
        fault_action_count: 0,
        provider_fault_score: 0,
        provider_call_count: 0,
        delay_milliseconds: 0,
    };
    for action in actions {
        match action {
            PlanActionKind::DriveCheckout { provider_script }
            | PlanActionKind::RetryBusinessRequest { provider_script }
            | PlanActionKind::ConfirmPaymentIntent { provider_script }
            | PlanActionKind::RetryProviderRequest { provider_script } => {
                let outcomes = provider_script.outcomes().collect::<Vec<_>>();
                result.provider_call_count = result
                    .provider_call_count
                    .saturating_add(u32::try_from(outcomes.len()).unwrap_or(u32::MAX));
                let score = outcomes.iter().copied().map(provider_fault_score).sum();
                if score > 0 {
                    result.fault_action_count = result.fault_action_count.saturating_add(1);
                    result.provider_fault_score = result.provider_fault_score.saturating_add(score);
                }
            }
            PlanActionKind::DuplicateWebhook
            | PlanActionKind::ReorderWebhooks
            | PlanActionKind::DropWebhook
            | PlanActionKind::KillApplication { .. } => {
                result.fault_action_count = result.fault_action_count.saturating_add(1);
            }
            PlanActionKind::DelayWebhook { milliseconds } => {
                result.fault_action_count = result.fault_action_count.saturating_add(1);
                result.delay_milliseconds = result.delay_milliseconds.saturating_add(*milliseconds);
            }
            PlanActionKind::RetrievePaymentIntent
            | PlanActionKind::ReleaseProviderGate
            | PlanActionKind::RestartAndAwaitHealth
            | PlanActionKind::GenerateProviderEvent
            | PlanActionKind::DeliverWebhook
            | PlanActionKind::WaitForQuiescence
            | PlanActionKind::CheckCheckpoint {
                checkpoint: Checkpoint::Final,
            } => {}
        }
    }
    result
}

const fn provider_fault_score(outcome: ProviderOutcome) -> u32 {
    match outcome {
        ProviderOutcome::Normal => 0,
        ProviderOutcome::PreExecute429 | ProviderOutcome::PreExecute500 => 1,
        ProviderOutcome::PostExecute500 => 2,
        ProviderOutcome::CommitThenClose | ProviderOutcome::CommitThenDelay => 3,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShrinkError {
    UnsupportedSchemaVersion,
    UnsupportedAlgorithm,
    InvalidSource(PlanValidationError),
    SourceIsNotSeeded,
    SourceMismatch,
    InvalidLineage,
    InvalidSchedule(PlanValidationError),
    InvalidReferenceCutPoint,
    DoesNotSimplifySource,
    InvalidMaterialization,
    ActionLimit,
}
