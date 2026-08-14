//! Pure, deterministic `PaymentIntent` case planning.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{
    decision::{DecisionEngine, Seed},
    trace::ActionId,
};

pub const PLAN_SCHEMA_VERSION: u16 = 2;
pub const CAMPAIGN_SCHEMA_VERSION: u16 = 2;
pub const MAX_ACTIONS_PER_CASE: u32 = 40;
pub const MAX_CASES: u32 = 500;
pub const PAYMENT_INTENT_V1_API_VERSION: &str = "2026-02-25.clover";
pub const CASE_SEED_DERIVATION: &str = "blake3/1.8.6/derive_key:dev.txproof.case-seed.v1/le-u64";
pub const SCHEDULER_ALGORITHM: &str =
    "rand/0.10.2/random_range+rand_chacha/0.10.0/ChaCha20Rng-seed_from_u64";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidActionBudget {
    Zero,
    AboveV1Maximum,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ActionBudget(u32);

impl ActionBudget {
    /// Creates a v1 per-case action budget.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidActionBudget`] when the value is zero or exceeds the
    /// fixed v1 ceiling.
    pub const fn new(value: u32) -> Result<Self, InvalidActionBudget> {
        if value == 0 {
            return Err(InvalidActionBudget::Zero);
        }
        if value > MAX_ACTIONS_PER_CASE {
            return Err(InvalidActionBudget::AboveV1Maximum);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ActionBudget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u32::deserialize(deserializer)?)
            .map_err(|error| D::Error::custom(format_args!("invalid action budget: {error:?}")))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderOutcome {
    Normal,
    #[serde(rename = "pre_execute_429")]
    PreExecute429,
    #[serde(rename = "pre_execute_500")]
    PreExecute500,
    #[serde(rename = "post_execute_500")]
    PostExecute500,
    CommitThenClose,
    CommitThenDelay,
}

impl ProviderOutcome {
    const fn commits(self) -> bool {
        !matches!(self, Self::PreExecute429 | Self::PreExecute500)
    }

    const fn response_is_ambiguous(self) -> bool {
        matches!(
            self,
            Self::PostExecute500 | Self::CommitThenClose | Self::CommitThenDelay
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessCutPoint {
    ClientRequestForwarded,
    ClientResponseObserved,
    WebhookRequestForwarded,
    WebhookResponseObserved,
    SqlProbe,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAdapter {
    PaymentIntentV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Checkpoint {
    Final,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanActionKind {
    DriveCheckout { outcome: ProviderOutcome },
    RetrievePaymentIntent,
    ReleaseProviderGate,
    RetryBusinessRequest { outcome: ProviderOutcome },
    ConfirmPaymentIntent { outcome: ProviderOutcome },
    RetryProviderRequest { outcome: ProviderOutcome },
    GenerateProviderEvent,
    DeliverWebhook,
    DuplicateWebhook,
    DelayWebhook { milliseconds: u64 },
    ReorderWebhooks,
    DropWebhook,
    KillApplication { cut_point: ProcessCutPoint },
    RestartAndAwaitHealth,
    WaitForQuiescence,
    CheckCheckpoint { checkpoint: Checkpoint },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookFaultSpec {
    duplicate_max: u8,
    delays_millis: BTreeSet<u64>,
    allow_reorder: bool,
    allow_drop: bool,
}

impl WebhookFaultSpec {
    /// Creates bounded webhook fault capabilities for a v1 plan.
    ///
    /// # Errors
    ///
    /// Returns [`PlanValidationError`] when the multiplicity or any delay
    /// exceeds the fixed v1 limits.
    pub fn new<D>(
        duplicate_max: u8,
        delays_millis: D,
        allow_reorder: bool,
        allow_drop: bool,
    ) -> Result<Self, PlanValidationError>
    where
        D: IntoIterator<Item = u64>,
    {
        let spec = Self {
            duplicate_max,
            delays_millis: delays_millis.into_iter().collect(),
            allow_reorder,
            allow_drop,
        };
        spec.validate()?;
        Ok(spec)
    }

    fn validate(&self) -> Result<(), PlanValidationError> {
        if self.duplicate_max > 3 || self.delays_millis.iter().any(|delay| *delay > 5_000) {
            return Err(PlanValidationError::CapabilityOutsideV1Bounds);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessFaultSpec {
    cut_points: BTreeSet<ProcessCutPoint>,
    max_kills: u8,
}

impl ProcessFaultSpec {
    /// Creates bounded process fault capabilities for a v1 plan.
    ///
    /// # Errors
    ///
    /// Returns [`PlanValidationError`] when more than one kill is configured.
    pub fn new<C>(cut_points: C, max_kills: u8) -> Result<Self, PlanValidationError>
    where
        C: IntoIterator<Item = ProcessCutPoint>,
    {
        let spec = Self {
            cut_points: cut_points.into_iter().collect(),
            max_kills,
        };
        spec.validate()?;
        Ok(spec)
    }

    fn validate(&self) -> Result<(), PlanValidationError> {
        if self.max_kills > 1 {
            return Err(PlanValidationError::CapabilityOutsideV1Bounds);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSpec {
    seed: Seed,
    max_actions: ActionBudget,
    provider_adapter: ProviderAdapter,
    provider_api_version: String,
    provider_outcomes: BTreeSet<ProviderOutcome>,
    webhook_faults: WebhookFaultSpec,
    process_faults: ProcessFaultSpec,
}

impl PlanSpec {
    #[must_use]
    pub fn payment_intent_v1(seed: Seed, max_actions: ActionBudget) -> Self {
        Self {
            seed,
            max_actions,
            provider_adapter: ProviderAdapter::PaymentIntentV1,
            provider_api_version: PAYMENT_INTENT_V1_API_VERSION.to_owned(),
            provider_outcomes: BTreeSet::from([
                ProviderOutcome::Normal,
                ProviderOutcome::PreExecute429,
                ProviderOutcome::PreExecute500,
                ProviderOutcome::PostExecute500,
                ProviderOutcome::CommitThenClose,
                ProviderOutcome::CommitThenDelay,
            ]),
            webhook_faults: WebhookFaultSpec {
                duplicate_max: 3,
                delays_millis: BTreeSet::from([0, 10, 100, 1_000, 5_000]),
                allow_reorder: true,
                allow_drop: true,
            },
            process_faults: ProcessFaultSpec {
                cut_points: BTreeSet::from([
                    ProcessCutPoint::ClientRequestForwarded,
                    ProcessCutPoint::ClientResponseObserved,
                    ProcessCutPoint::WebhookRequestForwarded,
                    ProcessCutPoint::WebhookResponseObserved,
                    ProcessCutPoint::SqlProbe,
                ]),
                max_kills: 1,
            },
        }
    }

    /// Creates a plan specification from validated v1 fault capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`PlanValidationError`] when normal provider recovery is absent
    /// or any configured multiplicity, delay, or kill budget exceeds v1.
    pub fn new_payment_intent_v1<P>(
        seed: Seed,
        max_actions: ActionBudget,
        provider_outcomes: P,
        webhook_faults: WebhookFaultSpec,
        process_faults: ProcessFaultSpec,
    ) -> Result<Self, PlanValidationError>
    where
        P: IntoIterator<Item = ProviderOutcome>,
    {
        let spec = Self {
            seed,
            max_actions,
            provider_adapter: ProviderAdapter::PaymentIntentV1,
            provider_api_version: PAYMENT_INTENT_V1_API_VERSION.to_owned(),
            provider_outcomes: provider_outcomes.into_iter().collect(),
            webhook_faults,
            process_faults,
        };
        spec.validate()?;
        Ok(spec)
    }

    fn validate(&self) -> Result<(), PlanValidationError> {
        if self.provider_adapter != ProviderAdapter::PaymentIntentV1
            || self.provider_api_version != PAYMENT_INTENT_V1_API_VERSION
        {
            return Err(PlanValidationError::UnsupportedProviderVersion);
        }
        if !self.provider_outcomes.contains(&ProviderOutcome::Normal) {
            return Err(PlanValidationError::MissingNormalProviderOutcome);
        }
        self.webhook_faults.validate()?;
        self.process_faults.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedAction {
    id: ActionId,
    logical_sequence: u32,
    dependencies: BTreeSet<ActionId>,
    decision_number: u64,
    eligible_count: usize,
    kind: PlanActionKind,
}

impl PlannedAction {
    #[must_use]
    pub const fn id(&self) -> ActionId {
        self.id
    }

    #[must_use]
    pub const fn logical_sequence(&self) -> u32 {
        self.logical_sequence
    }

    #[must_use]
    pub fn dependencies(&self) -> &BTreeSet<ActionId> {
        &self.dependencies
    }

    #[must_use]
    pub const fn decision_number(&self) -> u64 {
        self.decision_number
    }

    #[must_use]
    pub const fn eligible_count(&self) -> usize {
        self.eligible_count
    }

    #[must_use]
    pub const fn kind(&self) -> &PlanActionKind {
        &self.kind
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlannedCase {
    schema_version: u16,
    scheduler_algorithm: String,
    spec: PlanSpec,
    decision_count: u64,
    actions: Vec<PlannedAction>,
}

impl PlannedCase {
    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    #[must_use]
    pub const fn seed(&self) -> Seed {
        self.spec.seed
    }

    #[must_use]
    pub const fn max_actions(&self) -> ActionBudget {
        self.spec.max_actions
    }

    #[must_use]
    pub const fn provider_adapter(&self) -> ProviderAdapter {
        self.spec.provider_adapter
    }

    #[must_use]
    pub fn provider_api_version(&self) -> &str {
        &self.spec.provider_api_version
    }

    #[must_use]
    pub fn scheduler_algorithm(&self) -> &str {
        &self.scheduler_algorithm
    }

    #[must_use]
    pub const fn decision_count(&self) -> u64 {
        self.decision_count
    }

    #[must_use]
    pub fn actions(&self) -> &[PlannedAction] {
        &self.actions
    }

    /// Replays the pure compiler and compares the complete plan artifact.
    ///
    /// # Errors
    ///
    /// Returns [`PlanValidationError`] when the header, capabilities, action
    /// graph, decision metadata, or selected actions are not exactly what the
    /// recorded seed and budget produce.
    pub fn validate(&self) -> Result<(), PlanValidationError> {
        if self.schema_version != PLAN_SCHEMA_VERSION {
            return Err(PlanValidationError::UnsupportedSchemaVersion);
        }
        if self.scheduler_algorithm != SCHEDULER_ALGORITHM {
            return Err(PlanValidationError::UnsupportedSchedulerAlgorithm);
        }
        self.spec.validate()?;
        let expected = CasePlanCompiler::compile(&self.spec)
            .map_err(|_| PlanValidationError::PlanIsNotFeasible)?;
        if self != &expected {
            return Err(PlanValidationError::PlanDoesNotMatchDecisionStream);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlannedCaseWire {
    schema_version: u16,
    scheduler_algorithm: String,
    spec: PlanSpec,
    decision_count: u64,
    actions: Vec<PlannedAction>,
}

impl<'de> Deserialize<'de> for PlannedCase {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PlannedCaseWire::deserialize(deserializer)?;
        let planned = Self {
            schema_version: wire.schema_version,
            scheduler_algorithm: wire.scheduler_algorithm,
            spec: wire.spec,
            decision_count: wire.decision_count,
            actions: wire.actions,
        };
        planned
            .validate()
            .map_err(|error| D::Error::custom(format_args!("invalid planned case: {error:?}")))?;
        Ok(planned)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanCompileError {
    InvalidSpec(PlanValidationError),
    BudgetCannotReachCheckpoint { max_actions: u32 },
    NoEligibleAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlanValidationError {
    UnsupportedSchemaVersion,
    UnsupportedSchedulerAlgorithm,
    UnsupportedProviderVersion,
    MissingNormalProviderOutcome,
    CapabilityOutsideV1Bounds,
    PlanIsNotFeasible,
    PlanDoesNotMatchDecisionStream,
}

pub struct CasePlanCompiler;

impl CasePlanCompiler {
    /// Compiles one symbolic, state-valid case without performing I/O.
    ///
    /// # Errors
    ///
    /// Returns [`PlanCompileError`] when the specification is outside the v1
    /// contract or its action budget cannot reach the final checkpoint.
    pub fn compile(spec: &PlanSpec) -> Result<PlannedCase, PlanCompileError> {
        spec.validate().map_err(PlanCompileError::InvalidSpec)?;
        let initial = ModelState::default();
        let mut minimum_cache = BTreeMap::new();
        let minimum = minimum_actions_to_complete(initial, spec, &mut minimum_cache)
            .ok_or(PlanCompileError::NoEligibleAction)?;
        if minimum > spec.max_actions.value() as usize {
            return Err(PlanCompileError::BudgetCannotReachCheckpoint {
                max_actions: spec.max_actions.value(),
            });
        }

        let mut state = initial;
        let mut decisions = DecisionEngine::new(spec.seed);
        let mut actions = Vec::new();

        while !state.is_complete() {
            let remaining = spec.max_actions.value() as usize - actions.len();
            let eligible = eligible_actions(state, spec)
                .into_iter()
                .filter(|action| {
                    apply_action(state, *action).is_some_and(|next| {
                        minimum_actions_to_complete(next, spec, &mut minimum_cache)
                            .is_some_and(|distance| distance < remaining)
                    })
                })
                .collect::<Vec<_>>();
            let decision = decisions
                .choose(eligible)
                .map_err(|_| PlanCompileError::NoEligibleAction)?;
            let kind = *decision.selected();
            state = apply_action(state, kind).ok_or(PlanCompileError::NoEligibleAction)?;

            let logical_sequence =
                u32::try_from(actions.len() + 1).map_err(|_| PlanCompileError::NoEligibleAction)?;
            let dependencies = actions
                .last()
                .map_or_else(BTreeSet::new, |previous: &PlannedAction| {
                    BTreeSet::from([previous.id])
                });
            actions.push(PlannedAction {
                id: ActionId::new(logical_sequence),
                logical_sequence,
                dependencies,
                decision_number: decision.decision_number(),
                eligible_count: decision.eligible_count(),
                kind,
            });
        }

        Ok(PlannedCase {
            schema_version: PLAN_SCHEMA_VERSION,
            scheduler_algorithm: SCHEDULER_ALGORITHM.to_owned(),
            spec: spec.clone(),
            decision_count: actions.len() as u64,
            actions,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ModelState {
    phase: Phase,
    provider_objects: u8,
    application_healthy: bool,
    process_kills: u8,
}

impl Default for ModelState {
    fn default() -> Self {
        Self {
            phase: Phase::Start,
            provider_objects: 0,
            application_healthy: true,
            process_kills: 0,
        }
    }
}

impl ModelState {
    const fn is_complete(self) -> bool {
        matches!(self.phase, Phase::Complete)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
enum Phase {
    #[default]
    Start,
    CheckoutRetryable {
        attempts: u8,
        ambiguous: bool,
    },
    CheckoutResponseHeld,
    PaymentIntentKnown,
    ConfirmRetryable {
        attempts: u8,
        ambiguous: bool,
    },
    ConfirmResponseHeld,
    Confirmed,
    Events(EventState),
    Quiesced,
    Complete,
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct EventState {
    generated: u8,
    pending: u8,
    delivered: u8,
    duplicates: u8,
    delay_used: bool,
    reorder_used: bool,
}

fn eligible_actions(state: ModelState, spec: &PlanSpec) -> Vec<PlanActionKind> {
    if !state.application_healthy {
        return vec![PlanActionKind::RestartAndAwaitHealth];
    }

    let mut actions = match state.phase {
        Phase::Start => spec
            .provider_outcomes
            .iter()
            .copied()
            .map(|outcome| PlanActionKind::DriveCheckout { outcome })
            .collect(),
        Phase::CheckoutRetryable {
            attempts,
            ambiguous,
        } => {
            let mut actions = retry_outcomes(spec, attempts)
                .map(|outcome| PlanActionKind::RetryBusinessRequest { outcome })
                .collect::<Vec<_>>();
            if ambiguous {
                actions.push(PlanActionKind::RetrievePaymentIntent);
            }
            actions
        }
        Phase::CheckoutResponseHeld | Phase::ConfirmResponseHeld => {
            vec![PlanActionKind::ReleaseProviderGate]
        }
        Phase::PaymentIntentKnown => spec
            .provider_outcomes
            .iter()
            .copied()
            .map(|outcome| PlanActionKind::ConfirmPaymentIntent { outcome })
            .collect(),
        Phase::ConfirmRetryable {
            attempts,
            ambiguous,
        } => {
            let mut actions = retry_outcomes(spec, attempts)
                .map(|outcome| PlanActionKind::RetryProviderRequest { outcome })
                .collect::<Vec<_>>();
            if ambiguous {
                actions.push(PlanActionKind::RetrievePaymentIntent);
            }
            actions
        }
        Phase::Confirmed => vec![PlanActionKind::GenerateProviderEvent],
        Phase::Events(events) => event_actions(events, state.provider_objects, spec),
        Phase::Quiesced => vec![PlanActionKind::CheckCheckpoint {
            checkpoint: Checkpoint::Final,
        }],
        Phase::Complete => Vec::new(),
    };

    if state.process_kills < spec.process_faults.max_kills {
        actions.extend(
            spec.process_faults
                .cut_points
                .iter()
                .copied()
                .filter(|cut_point| cut_point_is_observable(state.phase, *cut_point))
                .map(|cut_point| PlanActionKind::KillApplication { cut_point }),
        );
    }

    actions
}

fn retry_outcomes(spec: &PlanSpec, attempts: u8) -> impl Iterator<Item = ProviderOutcome> + '_ {
    spec.provider_outcomes
        .iter()
        .copied()
        .filter(move |outcome| attempts < 2 || *outcome == ProviderOutcome::Normal)
}

fn event_actions(events: EventState, provider_objects: u8, spec: &PlanSpec) -> Vec<PlanActionKind> {
    if events.generated < provider_objects {
        return vec![PlanActionKind::GenerateProviderEvent];
    }
    let mut actions = Vec::new();
    if events.pending > 0 {
        actions.push(PlanActionKind::DeliverWebhook);
        if !events.delay_used {
            actions.extend(
                spec.webhook_faults
                    .delays_millis
                    .iter()
                    .copied()
                    .map(|milliseconds| PlanActionKind::DelayWebhook { milliseconds }),
            );
        }
        if spec.webhook_faults.allow_drop {
            actions.push(PlanActionKind::DropWebhook);
        }
    }
    if events.pending >= 2 && spec.webhook_faults.allow_reorder && !events.reorder_used {
        actions.push(PlanActionKind::ReorderWebhooks);
    }
    if events.delivered > 0 && events.duplicates < spec.webhook_faults.duplicate_max {
        actions.push(PlanActionKind::DuplicateWebhook);
    }
    if events.generated > 0 && events.pending == 0 {
        actions.push(PlanActionKind::WaitForQuiescence);
    }
    actions
}

const fn cut_point_is_observable(phase: Phase, cut_point: ProcessCutPoint) -> bool {
    match cut_point {
        ProcessCutPoint::ClientRequestForwarded => matches!(
            phase,
            Phase::CheckoutRetryable { .. }
                | Phase::CheckoutResponseHeld
                | Phase::PaymentIntentKnown
                | Phase::ConfirmRetryable { .. }
                | Phase::ConfirmResponseHeld
                | Phase::Confirmed
        ),
        ProcessCutPoint::ClientResponseObserved => matches!(
            phase,
            Phase::CheckoutRetryable { .. }
                | Phase::PaymentIntentKnown
                | Phase::ConfirmRetryable { .. }
                | Phase::Confirmed
        ),
        ProcessCutPoint::WebhookRequestForwarded | ProcessCutPoint::WebhookResponseObserved => {
            matches!(phase, Phase::Events(events) if events.delivered > 0)
        }
        ProcessCutPoint::SqlProbe => matches!(
            phase,
            Phase::CheckoutResponseHeld
                | Phase::PaymentIntentKnown
                | Phase::ConfirmRetryable { .. }
                | Phase::ConfirmResponseHeld
                | Phase::Confirmed
                | Phase::Events(_)
        ),
    }
}

fn apply_action(mut state: ModelState, action: PlanActionKind) -> Option<ModelState> {
    match action {
        PlanActionKind::KillApplication { .. } if state.application_healthy => {
            state.application_healthy = false;
            state.process_kills += 1;
            return Some(state);
        }
        PlanActionKind::RestartAndAwaitHealth if !state.application_healthy => {
            state.application_healthy = true;
            return Some(state);
        }
        _ if !state.application_healthy => return None,
        _ => {}
    }

    if let Some(phase) = apply_provider_action(state.phase, action) {
        if matches!(
            action,
            PlanActionKind::DriveCheckout { outcome }
                | PlanActionKind::RetryBusinessRequest { outcome }
                if outcome.commits()
        ) {
            state.provider_objects = state.provider_objects.checked_add(1)?;
        }
        state.phase = phase;
        return Some(state);
    }
    if let Some(phase) = apply_event_action(state.phase, action) {
        state.phase = phase;
        return Some(state);
    }
    match (state.phase, action) {
        (Phase::Events(_), PlanActionKind::WaitForQuiescence) => {
            state.phase = Phase::Quiesced;
            Some(state)
        }
        (
            Phase::Quiesced,
            PlanActionKind::CheckCheckpoint {
                checkpoint: Checkpoint::Final,
            },
        ) => {
            state.phase = Phase::Complete;
            Some(state)
        }
        _ => None,
    }
}

fn apply_provider_action(phase: Phase, action: PlanActionKind) -> Option<Phase> {
    match (phase, action) {
        (Phase::Start, PlanActionKind::DriveCheckout { outcome }) => {
            Some(provider_create_phase(outcome, 0))
        }
        (
            Phase::CheckoutRetryable { attempts, .. },
            PlanActionKind::RetryBusinessRequest { outcome },
        ) => Some(provider_create_phase(outcome, attempts + 1)),
        (Phase::CheckoutResponseHeld, PlanActionKind::ReleaseProviderGate) => {
            Some(Phase::PaymentIntentKnown)
        }
        (
            Phase::CheckoutRetryable {
                ambiguous: true, ..
            },
            PlanActionKind::RetrievePaymentIntent,
        ) => Some(Phase::PaymentIntentKnown),
        (Phase::PaymentIntentKnown, PlanActionKind::ConfirmPaymentIntent { outcome }) => {
            Some(provider_confirm_phase(outcome, 0))
        }
        (
            Phase::ConfirmRetryable { attempts, .. },
            PlanActionKind::RetryProviderRequest { outcome },
        ) => Some(provider_confirm_phase(outcome, attempts + 1)),
        (Phase::ConfirmResponseHeld, PlanActionKind::ReleaseProviderGate)
        | (
            Phase::ConfirmRetryable {
                ambiguous: true, ..
            },
            PlanActionKind::RetrievePaymentIntent,
        ) => Some(Phase::Confirmed),
        (Phase::Confirmed, PlanActionKind::GenerateProviderEvent) => {
            Some(Phase::Events(EventState {
                generated: 1,
                pending: 1,
                ..EventState::default()
            }))
        }
        _ => None,
    }
}

fn apply_event_action(phase: Phase, action: PlanActionKind) -> Option<Phase> {
    let Phase::Events(mut events) = phase else {
        return None;
    };
    match action {
        PlanActionKind::GenerateProviderEvent => {
            events.generated += 1;
            events.pending += 1;
        }
        PlanActionKind::DeliverWebhook => {
            events.pending = events.pending.checked_sub(1)?;
            events.delivered += 1;
        }
        PlanActionKind::DuplicateWebhook => events.duplicates += 1,
        PlanActionKind::DelayWebhook { .. } => events.delay_used = true,
        PlanActionKind::ReorderWebhooks => events.reorder_used = true,
        PlanActionKind::DropWebhook => events.pending = events.pending.checked_sub(1)?,
        _ => return None,
    }
    Some(Phase::Events(events))
}

fn provider_create_phase(outcome: ProviderOutcome, attempts: u8) -> Phase {
    match outcome {
        ProviderOutcome::Normal => Phase::PaymentIntentKnown,
        ProviderOutcome::CommitThenDelay => Phase::CheckoutResponseHeld,
        _ => Phase::CheckoutRetryable {
            attempts,
            ambiguous: outcome.commits() && outcome.response_is_ambiguous(),
        },
    }
}

fn provider_confirm_phase(outcome: ProviderOutcome, attempts: u8) -> Phase {
    match outcome {
        ProviderOutcome::Normal => Phase::Confirmed,
        ProviderOutcome::CommitThenDelay => Phase::ConfirmResponseHeld,
        _ => Phase::ConfirmRetryable {
            attempts,
            ambiguous: outcome.commits() && outcome.response_is_ambiguous(),
        },
    }
}

fn minimum_actions_to_complete(
    state: ModelState,
    spec: &PlanSpec,
    cache: &mut BTreeMap<ModelState, Option<usize>>,
) -> Option<usize> {
    if state.is_complete() {
        return Some(0);
    }
    if let Some(cached) = cache.get(&state) {
        return *cached;
    }

    cache.insert(state, None);
    let minimum = eligible_actions(state, spec)
        .into_iter()
        .filter_map(|action| apply_action(state, action))
        .filter_map(|next| minimum_actions_to_complete(next, spec, cache))
        .min()
        .map(|distance| distance + 1);
    cache.insert(state, minimum);
    minimum
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCaseCount {
    Zero,
    AboveV1Maximum,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CaseCount(u32);

impl CaseCount {
    /// Creates a v1 campaign case count.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCaseCount`] when the value is zero or exceeds the v1
    /// maximum.
    pub const fn new(value: u32) -> Result<Self, InvalidCaseCount> {
        if value == 0 {
            return Err(InvalidCaseCount::Zero);
        }
        if value > MAX_CASES {
            return Err(InvalidCaseCount::AboveV1Maximum);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for CaseCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u32::deserialize(deserializer)?)
            .map_err(|error| D::Error::custom(format_args!("invalid case count: {error:?}")))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CaseId(u32);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCaseId {
    Zero,
    AboveV1Maximum,
}

impl CaseId {
    /// Creates a one-based v1 campaign case identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCaseId`] when the identifier is zero or exceeds the
    /// maximum v1 case count.
    pub const fn new(value: u32) -> Result<Self, InvalidCaseId> {
        if value == 0 {
            return Err(InvalidCaseId::Zero);
        }
        if value > MAX_CASES {
            return Err(InvalidCaseId::AboveV1Maximum);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl<'de> Deserialize<'de> for CaseId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u32::deserialize(deserializer)?)
            .map_err(|error| D::Error::custom(format_args!("invalid case ID: {error:?}")))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignSpec {
    campaign_seed: Seed,
    case_count: CaseCount,
    max_actions: ActionBudget,
    provider_adapter: ProviderAdapter,
    provider_api_version: String,
    provider_outcomes: BTreeSet<ProviderOutcome>,
    webhook_faults: WebhookFaultSpec,
    process_faults: ProcessFaultSpec,
}

impl CampaignSpec {
    #[must_use]
    pub fn payment_intent_v1(
        campaign_seed: Seed,
        case_count: CaseCount,
        max_actions: ActionBudget,
    ) -> Self {
        let case = PlanSpec::payment_intent_v1(campaign_seed, max_actions);
        Self::from_case_spec(campaign_seed, case_count, case)
    }

    #[must_use]
    pub const fn campaign_seed(&self) -> Seed {
        self.campaign_seed
    }

    #[must_use]
    pub const fn case_count(&self) -> CaseCount {
        self.case_count
    }

    #[must_use]
    pub const fn max_actions(&self) -> ActionBudget {
        self.max_actions
    }

    /// Creates a campaign from validated v1 fault capabilities.
    ///
    /// # Errors
    ///
    /// Returns [`PlanValidationError`] when the provider or fault contract is
    /// outside v1.
    pub fn new_payment_intent_v1<P>(
        campaign_seed: Seed,
        case_count: CaseCount,
        max_actions: ActionBudget,
        provider_outcomes: P,
        webhook_faults: WebhookFaultSpec,
        process_faults: ProcessFaultSpec,
    ) -> Result<Self, PlanValidationError>
    where
        P: IntoIterator<Item = ProviderOutcome>,
    {
        let case = PlanSpec::new_payment_intent_v1(
            campaign_seed,
            max_actions,
            provider_outcomes,
            webhook_faults,
            process_faults,
        )?;
        Ok(Self::from_case_spec(campaign_seed, case_count, case))
    }

    fn from_case_spec(campaign_seed: Seed, case_count: CaseCount, case: PlanSpec) -> Self {
        Self {
            campaign_seed,
            case_count,
            max_actions: case.max_actions,
            provider_adapter: case.provider_adapter,
            provider_api_version: case.provider_api_version,
            provider_outcomes: case.provider_outcomes,
            webhook_faults: case.webhook_faults,
            process_faults: case.process_faults,
        }
    }

    fn validate(&self) -> Result<(), PlanValidationError> {
        self.case_spec(self.campaign_seed).validate()
    }

    fn case_spec(&self, seed: Seed) -> PlanSpec {
        PlanSpec {
            seed,
            max_actions: self.max_actions,
            provider_adapter: self.provider_adapter,
            provider_api_version: self.provider_api_version.clone(),
            provider_outcomes: self.provider_outcomes.clone(),
            webhook_faults: self.webhook_faults.clone(),
            process_faults: self.process_faults.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CampaignCase {
    case_id: CaseId,
    plan: PlannedCase,
}

impl CampaignCase {
    #[must_use]
    pub const fn id(&self) -> CaseId {
        self.case_id
    }

    #[must_use]
    pub const fn plan(&self) -> &PlannedCase {
        &self.plan
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CampaignPlan {
    schema_version: u16,
    case_seed_derivation: String,
    spec: CampaignSpec,
    cases: Vec<CampaignCase>,
}

impl CampaignPlan {
    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    #[must_use]
    pub fn case_seed_derivation(&self) -> &str {
        &self.case_seed_derivation
    }

    #[must_use]
    pub const fn spec(&self) -> &CampaignSpec {
        &self.spec
    }

    #[must_use]
    pub fn cases(&self) -> &[CampaignCase] {
        &self.cases
    }

    /// Recompiles and compares the complete serial campaign artifact.
    ///
    /// # Errors
    ///
    /// Returns [`CampaignValidationError`] when compatibility metadata, case
    /// identities, derived seeds, or any planned action differs.
    pub fn validate(&self) -> Result<(), CampaignValidationError> {
        if self.schema_version != CAMPAIGN_SCHEMA_VERSION {
            return Err(CampaignValidationError::UnsupportedSchemaVersion);
        }
        if self.case_seed_derivation != CASE_SEED_DERIVATION {
            return Err(CampaignValidationError::UnsupportedCaseSeedDerivation);
        }
        self.spec
            .validate()
            .map_err(|_| CampaignValidationError::InvalidSpec)?;
        let expected = CampaignPlanner::compile(&self.spec)
            .map_err(|_| CampaignValidationError::CampaignIsNotFeasible)?;
        if self != &expected {
            return Err(CampaignValidationError::CampaignDoesNotMatchSpec);
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CampaignPlanWire {
    schema_version: u16,
    case_seed_derivation: String,
    spec: CampaignSpec,
    cases: Vec<CampaignCase>,
}

impl<'de> Deserialize<'de> for CampaignPlan {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CampaignPlanWire::deserialize(deserializer)?;
        let campaign = Self {
            schema_version: wire.schema_version,
            case_seed_derivation: wire.case_seed_derivation,
            spec: wire.spec,
            cases: wire.cases,
        };
        campaign
            .validate()
            .map_err(|error| D::Error::custom(format_args!("invalid campaign plan: {error:?}")))?;
        Ok(campaign)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CampaignCompileError {
    InvalidSpec(PlanValidationError),
    InvalidCase {
        case_id: CaseId,
        source: PlanCompileError,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CampaignValidationError {
    UnsupportedSchemaVersion,
    UnsupportedCaseSeedDerivation,
    InvalidSpec,
    CampaignIsNotFeasible,
    CampaignDoesNotMatchSpec,
}

pub struct CampaignPlanner;

impl CampaignPlanner {
    /// Compiles every case in one deterministic serial order.
    ///
    /// # Errors
    ///
    /// Returns [`CampaignCompileError`] when the campaign specification or any
    /// derived case cannot reach its final checkpoint within budget.
    pub fn compile(spec: &CampaignSpec) -> Result<CampaignPlan, CampaignCompileError> {
        spec.validate().map_err(CampaignCompileError::InvalidSpec)?;
        let mut cases = Vec::with_capacity(spec.case_count.value() as usize);
        for value in 1..=spec.case_count.value() {
            let case_id = CaseId(value);
            let seed = derive_case_seed(spec.campaign_seed, case_id);
            let plan = CasePlanCompiler::compile(&spec.case_spec(seed))
                .map_err(|source| CampaignCompileError::InvalidCase { case_id, source })?;
            cases.push(CampaignCase { case_id, plan });
        }
        Ok(CampaignPlan {
            schema_version: CAMPAIGN_SCHEMA_VERSION,
            case_seed_derivation: CASE_SEED_DERIVATION.to_owned(),
            spec: spec.clone(),
            cases,
        })
    }
}

fn derive_case_seed(campaign_seed: Seed, case_id: CaseId) -> Seed {
    const CONTEXT: &str = "dev.txproof.case-seed.v1";
    let mut material = [0_u8; 12];
    material[..8].copy_from_slice(&campaign_seed.value().to_le_bytes());
    material[8..].copy_from_slice(&case_id.value().to_le_bytes());
    let digest = blake3::derive_key(CONTEXT, &material);
    let mut seed = [0_u8; 8];
    seed.copy_from_slice(&digest[..8]);
    Seed::new(u64::from_le_bytes(seed))
}
