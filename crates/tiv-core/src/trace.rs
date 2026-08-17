use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{
    decision::Seed,
    ids::{EventId, InvalidProviderId, PaymentIntentId},
    plan::{PlanActionKind, PlannedCase, ProviderOutcome},
};

pub const TRACE_SCHEMA_VERSION: u16 = 1;
pub const CASE_TRACE_SCHEMA_VERSION: u16 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
struct TraceSchemaVersion(u16);

impl TraceSchemaVersion {
    const CURRENT: Self = Self(TRACE_SCHEMA_VERSION);
}

impl<'de> Deserialize<'de> for TraceSchemaVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u16::deserialize(deserializer)?;
        if value != TRACE_SCHEMA_VERSION {
            return Err(D::Error::custom(format_args!(
                "unsupported trace schema version {value}; expected {TRACE_SCHEMA_VERSION}"
            )));
        }
        Ok(Self::CURRENT)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ActionId(u32);

impl ActionId {
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProviderGateId(u64);

impl ProviderGateId {
    /// Creates a fixture gate identifier captured from the control plane.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProviderGateId`] when the identifier is zero.
    pub const fn new(value: u64) -> Result<Self, InvalidProviderGateId> {
        if value == 0 {
            return Err(InvalidProviderGateId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for ProviderGateId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?)
            .map_err(|_| D::Error::custom("provider gate ID must be non-zero"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidProviderGateId;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum OutputSlot {
    PaymentIntentId,
    EventId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum InputSlot {
    PaymentIntentId,
}

impl InputSlot {
    const fn output_slot(self) -> OutputSlot {
        match self {
            Self::PaymentIntentId => OutputSlot::PaymentIntentId,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputRef {
    action_id: ActionId,
    slot: OutputSlot,
}

impl OutputRef {
    #[must_use]
    pub const fn new(action_id: ActionId, slot: OutputSlot) -> Self {
        Self { action_id, slot }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ActionKind {
    DriveCheckout,
    ConfirmPaymentIntent,
}

impl ActionKind {
    const fn required_inputs(self) -> &'static [InputSlot] {
        match self {
            Self::DriveCheckout => &[],
            Self::ConfirmPaymentIntent => &[InputSlot::PaymentIntentId],
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ActionInput {
    slot: InputSlot,
    source: OutputRef,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedAction {
    id: ActionId,
    kind: ActionKind,
    dependencies: BTreeSet<ActionId>,
    inputs: Vec<ActionInput>,
    declared_outputs: BTreeSet<OutputSlot>,
}

impl PlannedAction {
    #[must_use]
    pub const fn new(id: ActionId, kind: ActionKind) -> Self {
        Self {
            id,
            kind,
            dependencies: BTreeSet::new(),
            inputs: Vec::new(),
            declared_outputs: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn depends_on(mut self, action_id: ActionId) -> Self {
        self.dependencies.insert(action_id);
        self
    }

    #[must_use]
    pub fn declares_output(mut self, slot: OutputSlot) -> Self {
        self.declared_outputs.insert(slot);
        self
    }

    #[must_use]
    pub fn binds_input(mut self, slot: InputSlot, source: OutputRef) -> Self {
        self.inputs.push(ActionInput { slot, source });
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CapturedValue {
    PaymentIntentId(PaymentIntentId),
    EventId(EventId),
}

impl CapturedValue {
    /// Creates a captured `PaymentIntent` identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProviderId`] when the identifier is malformed.
    pub fn payment_intent_id(value: impl Into<String>) -> Result<Self, InvalidProviderId> {
        PaymentIntentId::new(value).map(Self::PaymentIntentId)
    }

    /// Creates a captured event identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProviderId`] when the identifier is malformed.
    pub fn event_id(value: impl Into<String>) -> Result<Self, InvalidProviderId> {
        EventId::new(value).map(Self::EventId)
    }

    const fn kind(&self) -> OutputSlot {
        match self {
            Self::PaymentIntentId(_) => OutputSlot::PaymentIntentId,
            Self::EventId(_) => OutputSlot::EventId,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum CaseOutputSlot {
    PaymentIntentId,
    EventId,
    ProviderGateId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum CaseInputSlot {
    PaymentIntentId,
    ProviderGateId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CaseInputBinding {
    action_id: ActionId,
    slot: CaseInputSlot,
    source: CaseOutputRef,
}

impl CaseInputBinding {
    #[must_use]
    pub const fn action_id(self) -> ActionId {
        self.action_id
    }

    #[must_use]
    pub const fn slot(self) -> CaseInputSlot {
        self.slot
    }

    #[must_use]
    pub const fn source(self) -> CaseOutputRef {
        self.source
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseOutputRef {
    action_id: ActionId,
    slot: CaseOutputSlot,
    occurrence: u8,
}

impl CaseOutputRef {
    #[must_use]
    pub const fn new(action_id: ActionId, slot: CaseOutputSlot) -> Self {
        Self {
            action_id,
            slot,
            occurrence: 0,
        }
    }

    #[must_use]
    pub const fn for_occurrence(action_id: ActionId, slot: CaseOutputSlot, occurrence: u8) -> Self {
        Self {
            action_id,
            slot,
            occurrence,
        }
    }

    #[must_use]
    pub const fn action_id(self) -> ActionId {
        self.action_id
    }

    #[must_use]
    pub const fn slot(self) -> CaseOutputSlot {
        self.slot
    }

    #[must_use]
    pub const fn occurrence(self) -> u8 {
        self.occurrence
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CaseCapturedValue {
    PaymentIntentId(PaymentIntentId),
    EventId(EventId),
    ProviderGateId(ProviderGateId),
}

impl CaseCapturedValue {
    /// Creates a captured `PaymentIntent` identifier for a full case trace.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProviderId`] when the identifier is malformed.
    pub fn payment_intent_id(value: impl Into<String>) -> Result<Self, InvalidProviderId> {
        PaymentIntentId::new(value).map(Self::PaymentIntentId)
    }

    /// Creates a captured event identifier for a full case trace.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProviderId`] when the identifier is malformed.
    pub fn event_id(value: impl Into<String>) -> Result<Self, InvalidProviderId> {
        EventId::new(value).map(Self::EventId)
    }

    /// Creates a captured provider control gate identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProviderGateId`] when the identifier is zero.
    pub const fn provider_gate_id(value: u64) -> Result<Self, InvalidProviderGateId> {
        match ProviderGateId::new(value) {
            Ok(value) => Ok(Self::ProviderGateId(value)),
            Err(error) => Err(error),
        }
    }

    #[must_use]
    pub const fn slot(&self) -> CaseOutputSlot {
        self.kind()
    }

    const fn kind(&self) -> CaseOutputSlot {
        match self {
            Self::PaymentIntentId(_) => CaseOutputSlot::PaymentIntentId,
            Self::EventId(_) => CaseOutputSlot::EventId,
            Self::ProviderGateId(_) => CaseOutputSlot::ProviderGateId,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CaseActionInput {
    slot: CaseInputSlot,
    source: CaseOutputRef,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CaseCapturedOutput {
    output_ref: CaseOutputRef,
    value: CaseCapturedValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedTrace {
    seed: Seed,
    actions: Vec<PlannedAction>,
}

impl PlannedTrace {
    #[must_use]
    pub fn new<I>(seed: Seed, actions: I) -> Self
    where
        I: IntoIterator<Item = PlannedAction>,
    {
        Self {
            seed,
            actions: actions.into_iter().collect(),
        }
    }

    /// Binds every declared dynamic output and validates the replay graph.
    ///
    /// # Errors
    ///
    /// Returns [`CompileError`] when action identities or dependencies are
    /// invalid, or when captured outputs are missing, duplicated, unexpected,
    /// or bound to the wrong output kind.
    pub fn compile<I>(self, captured: I) -> Result<CompiledTrace, CompileError>
    where
        I: IntoIterator<Item = (OutputRef, CapturedValue)>,
    {
        let mut captured_outputs = BTreeMap::new();
        for (output_ref, value) in captured {
            if captured_outputs.insert(output_ref, value).is_some() {
                return Err(CompileError::DuplicateOutput(output_ref));
            }
        }
        let captured = captured_outputs;
        let mut action_ids = BTreeSet::new();
        for action in &self.actions {
            if !action_ids.insert(action.id) {
                return Err(CompileError::DuplicateActionId(action.id));
            }
        }

        let declared_outputs = self
            .actions
            .iter()
            .flat_map(|action| {
                action
                    .declared_outputs
                    .iter()
                    .map(|&slot| OutputRef::new(action.id, slot))
            })
            .collect::<BTreeSet<_>>();
        let declared_outputs_by_action = self
            .actions
            .iter()
            .map(|action| (action.id, &action.declared_outputs))
            .collect::<BTreeMap<_, _>>();
        if let Some(&unexpected) = captured
            .keys()
            .find(|output_ref| !declared_outputs.contains(output_ref))
        {
            return Err(CompileError::UnexpectedOutput(unexpected));
        }

        let mut earlier_action_ids = BTreeSet::new();

        for action in &self.actions {
            for &dependency in &action.dependencies {
                if !action_ids.contains(&dependency) {
                    return Err(CompileError::UnknownDependency {
                        action_id: action.id,
                        dependency,
                    });
                }
                if !earlier_action_ids.contains(&dependency) {
                    return Err(CompileError::DependencyNotEarlier {
                        action_id: action.id,
                        dependency,
                    });
                }
            }

            validate_action_inputs(action, &declared_outputs_by_action)?;

            for &slot in &action.declared_outputs {
                let output_ref = OutputRef::new(action.id, slot);
                match captured.get(&output_ref) {
                    None => return Err(CompileError::MissingOutput(output_ref)),
                    Some(value) if value.kind() != slot => {
                        return Err(CompileError::OutputTypeMismatch {
                            output_ref,
                            actual: value.kind(),
                        });
                    }
                    Some(_) => {}
                }
            }

            earlier_action_ids.insert(action.id);
        }

        Ok(CompiledTrace {
            schema_version: TraceSchemaVersion::CURRENT,
            seed: self.seed,
            actions: self.actions,
            captured: captured
                .into_iter()
                .map(|(output_ref, value)| CapturedOutput { output_ref, value })
                .collect(),
        })
    }
}

fn validate_action_inputs(
    action: &PlannedAction,
    declared_outputs_by_action: &BTreeMap<ActionId, &BTreeSet<OutputSlot>>,
) -> Result<(), CompileError> {
    let required_inputs = action.kind.required_inputs();
    let mut bound_inputs = BTreeSet::new();
    for input in &action.inputs {
        if !bound_inputs.insert(input.slot) {
            return Err(CompileError::DuplicateInput {
                action_id: action.id,
                input: input.slot,
            });
        }
        if !required_inputs.contains(&input.slot) {
            return Err(CompileError::UnexpectedInput {
                action_id: action.id,
                input: input.slot,
            });
        }
        let expected = input.slot.output_slot();
        if input.source.slot != expected {
            return Err(CompileError::InputTypeMismatch {
                action_id: action.id,
                input: input.slot,
                expected,
                actual: input.source.slot,
            });
        }
        if !action.dependencies.contains(&input.source.action_id) {
            return Err(CompileError::InputSourceNotDependency {
                action_id: action.id,
                input: input.slot,
                source_action_id: input.source.action_id,
            });
        }
        if !declared_outputs_by_action
            .get(&input.source.action_id)
            .is_some_and(|outputs| outputs.contains(&input.source.slot))
        {
            return Err(CompileError::InputSourceNotDeclared {
                action_id: action.id,
                input: input.slot,
                source: input.source,
            });
        }
    }
    for &required in required_inputs {
        if !bound_inputs.contains(&required) {
            return Err(CompileError::MissingInput {
                action_id: action.id,
                input: required,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CapturedOutput {
    output_ref: OutputRef,
    value: CapturedValue,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompiledTrace {
    schema_version: TraceSchemaVersion,
    seed: Seed,
    actions: Vec<PlannedAction>,
    captured: Vec<CapturedOutput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompiledTraceWire {
    schema_version: TraceSchemaVersion,
    seed: Seed,
    actions: Vec<PlannedAction>,
    captured: Vec<CapturedOutput>,
}

impl<'de> Deserialize<'de> for CompiledTrace {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let CompiledTraceWire {
            schema_version,
            seed,
            actions,
            captured,
        } = CompiledTraceWire::deserialize(deserializer)?;
        let _ = schema_version;

        PlannedTrace::new(seed, actions)
            .compile(
                captured
                    .into_iter()
                    .map(|captured| (captured.output_ref, captured.value)),
            )
            .map_err(|error| D::Error::custom(format_args!("invalid compiled trace: {error:?}")))
    }
}

impl CompiledTrace {
    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version.0
    }

    #[must_use]
    pub const fn seed(&self) -> Seed {
        self.seed
    }

    #[must_use]
    pub const fn action_count(&self) -> usize {
        self.actions.len()
    }

    #[must_use]
    pub fn replay_action(&self, action_id: ActionId) -> Option<ReplayAction<'_>> {
        self.actions
            .iter()
            .find(|action| action.id == action_id)
            .map(|action| ReplayAction {
                action,
                captured: &self.captured,
            })
    }

    pub fn replay_actions(&self) -> impl Iterator<Item = ReplayAction<'_>> {
        self.actions.iter().map(|action| ReplayAction {
            action,
            captured: &self.captured,
        })
    }

    #[must_use]
    pub fn resolve(&self, output_ref: OutputRef) -> Option<&CapturedValue> {
        self.captured
            .iter()
            .find(|captured| captured.output_ref == output_ref)
            .map(|captured| &captured.value)
    }
}

/// A compiled action view whose declared inputs resolve through the trace.
#[derive(Clone, Copy)]
pub struct ReplayAction<'a> {
    action: &'a PlannedAction,
    captured: &'a [CapturedOutput],
}

impl ReplayAction<'_> {
    #[must_use]
    pub const fn id(&self) -> ActionId {
        self.action.id
    }

    #[must_use]
    pub const fn kind(&self) -> ActionKind {
        self.action.kind
    }

    #[must_use]
    pub fn input(&self, slot: InputSlot) -> Option<&CapturedValue> {
        let source = self
            .action
            .inputs
            .iter()
            .find(|input| input.slot == slot)?
            .source;
        self.captured
            .iter()
            .find(|captured| captured.output_ref == source)
            .map(|captured| &captured.value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CompiledCaseAction {
    id: ActionId,
    logical_sequence: u32,
    dependencies: BTreeSet<ActionId>,
    decision_number: u64,
    eligible_count: usize,
    kind: PlanActionKind,
    inputs: Vec<CaseActionInput>,
    declared_outputs: BTreeSet<CaseOutputRef>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CompiledCaseTrace {
    schema_version: u16,
    planned_case: PlannedCase,
    actions: Vec<CompiledCaseAction>,
    captured: Vec<CaseCapturedOutput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompiledCaseTraceWire {
    schema_version: u16,
    planned_case: PlannedCase,
    actions: Vec<CompiledCaseAction>,
    captured: Vec<CaseCapturedOutput>,
}

impl<'de> Deserialize<'de> for CompiledCaseTrace {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CompiledCaseTraceWire::deserialize(deserializer)?;
        if wire.schema_version != CASE_TRACE_SCHEMA_VERSION {
            return Err(D::Error::custom(format_args!(
                "unsupported case trace schema version {}; expected {CASE_TRACE_SCHEMA_VERSION}",
                wire.schema_version
            )));
        }
        let expected = CaseTraceMaterializer::materialize(
            &wire.planned_case,
            wire.captured
                .iter()
                .map(|captured| (captured.output_ref, captured.value.clone())),
        )
        .map_err(|error| {
            D::Error::custom(format_args!("invalid materialized case trace: {error:?}"))
        })?;
        let received = Self {
            schema_version: wire.schema_version,
            planned_case: wire.planned_case,
            actions: wire.actions,
            captured: wire.captured,
        };
        if received != expected {
            return Err(D::Error::custom(
                "materialized case actions do not match the validated plan",
            ));
        }
        Ok(received)
    }
}

impl CompiledCaseTrace {
    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    #[must_use]
    pub const fn planned_case(&self) -> &PlannedCase {
        &self.planned_case
    }

    #[must_use]
    pub const fn action_count(&self) -> usize {
        self.actions.len()
    }

    #[must_use]
    pub fn replay_action(&self, action_id: ActionId) -> Option<ReplayCaseAction<'_>> {
        self.actions
            .iter()
            .find(|action| action.id == action_id)
            .map(|action| ReplayCaseAction {
                action,
                captured: &self.captured,
            })
    }

    pub fn replay_actions(&self) -> impl Iterator<Item = ReplayCaseAction<'_>> {
        self.actions.iter().map(|action| ReplayCaseAction {
            action,
            captured: &self.captured,
        })
    }

    #[must_use]
    pub fn resolve(&self, output_ref: CaseOutputRef) -> Option<&CaseCapturedValue> {
        self.captured
            .iter()
            .find(|captured| captured.output_ref == output_ref)
            .map(|captured| &captured.value)
    }
}

#[derive(Clone, Copy)]
pub struct ReplayCaseAction<'a> {
    action: &'a CompiledCaseAction,
    captured: &'a [CaseCapturedOutput],
}

impl ReplayCaseAction<'_> {
    #[must_use]
    pub const fn id(&self) -> ActionId {
        self.action.id
    }

    #[must_use]
    pub const fn logical_sequence(&self) -> u32 {
        self.action.logical_sequence
    }

    #[must_use]
    pub fn dependencies(&self) -> &BTreeSet<ActionId> {
        &self.action.dependencies
    }

    #[must_use]
    pub const fn decision_number(&self) -> u64 {
        self.action.decision_number
    }

    #[must_use]
    pub const fn eligible_count(&self) -> usize {
        self.action.eligible_count
    }

    #[must_use]
    pub const fn kind(&self) -> &PlanActionKind {
        &self.action.kind
    }

    #[must_use]
    pub fn input(&self, slot: CaseInputSlot) -> Option<&CaseCapturedValue> {
        let source = self
            .action
            .inputs
            .iter()
            .find(|input| input.slot == slot)?
            .source;
        self.captured
            .iter()
            .find(|captured| captured.output_ref == source)
            .map(|captured| &captured.value)
    }
}

pub struct CaseTraceMaterializer;

impl CaseTraceMaterializer {
    /// Returns the exact dynamic values the executor must capture.
    ///
    /// # Errors
    ///
    /// Returns [`CaseTraceMaterializationError`] when the planned case is not
    /// the valid deterministic result of its embedded specification.
    pub fn required_outputs(
        planned_case: &PlannedCase,
    ) -> Result<Vec<CaseOutputRef>, CaseTraceMaterializationError> {
        planned_case
            .validate()
            .map_err(|_| CaseTraceMaterializationError::InvalidPlannedCase)?;
        Ok(materialize_case_actions(planned_case)?
            .iter()
            .flat_map(|action| action.declared_outputs.iter().copied())
            .collect())
    }

    /// Returns the exact earlier outputs each action consumes at execution.
    ///
    /// # Errors
    ///
    /// Returns [`CaseTraceMaterializationError`] when the planned case is not
    /// the valid deterministic result of its embedded specification.
    pub fn required_inputs(
        planned_case: &PlannedCase,
    ) -> Result<Vec<CaseInputBinding>, CaseTraceMaterializationError> {
        planned_case
            .validate()
            .map_err(|_| CaseTraceMaterializationError::InvalidPlannedCase)?;
        Ok(materialize_case_actions(planned_case)?
            .iter()
            .flat_map(|action| {
                action.inputs.iter().map(|input| CaseInputBinding {
                    action_id: action.id,
                    slot: input.slot,
                    source: input.source,
                })
            })
            .collect())
    }

    /// Binds every reserved external value to a complete, validated case plan.
    ///
    /// # Errors
    ///
    /// Returns [`CaseTraceMaterializationError`] when the plan is invalid or a
    /// captured output is missing, duplicated, unexpected, or has the wrong
    /// type.
    pub fn materialize<I>(
        planned_case: &PlannedCase,
        captured: I,
    ) -> Result<CompiledCaseTrace, CaseTraceMaterializationError>
    where
        I: IntoIterator<Item = (CaseOutputRef, CaseCapturedValue)>,
    {
        planned_case
            .validate()
            .map_err(|_| CaseTraceMaterializationError::InvalidPlannedCase)?;
        let actions = materialize_case_actions(planned_case)?;
        let declared_outputs = actions
            .iter()
            .flat_map(|action| action.declared_outputs.iter().copied())
            .collect::<BTreeSet<_>>();
        let mut captured_outputs = BTreeMap::new();
        for (output_ref, value) in captured {
            if captured_outputs.insert(output_ref, value).is_some() {
                return Err(CaseTraceMaterializationError::DuplicateOutput(output_ref));
            }
        }
        if let Some(&unexpected) = captured_outputs
            .keys()
            .find(|output_ref| !declared_outputs.contains(output_ref))
        {
            return Err(CaseTraceMaterializationError::UnexpectedOutput(unexpected));
        }
        for output_ref in declared_outputs {
            let value = captured_outputs
                .get(&output_ref)
                .ok_or(CaseTraceMaterializationError::MissingOutput(output_ref))?;
            if value.kind() != output_ref.slot {
                return Err(CaseTraceMaterializationError::OutputTypeMismatch {
                    output_ref,
                    actual: value.kind(),
                });
            }
        }
        Ok(CompiledCaseTrace {
            schema_version: CASE_TRACE_SCHEMA_VERSION,
            planned_case: planned_case.clone(),
            actions,
            captured: captured_outputs
                .into_iter()
                .map(|(output_ref, value)| CaseCapturedOutput { output_ref, value })
                .collect(),
        })
    }
}

fn materialize_case_actions(
    planned_case: &PlannedCase,
) -> Result<Vec<CompiledCaseAction>, CaseTraceMaterializationError> {
    let mut active_payment_intent = None;
    let mut provider_objects = Vec::new();
    let mut generated_provider_events = 0_usize;
    let mut held_provider_gate = None;
    let mut actions = Vec::with_capacity(planned_case.actions().len());

    for planned in planned_case.actions() {
        let mut inputs = Vec::new();
        let mut declared_outputs = BTreeSet::new();
        match *planned.kind() {
            PlanActionKind::DriveCheckout { provider_script }
            | PlanActionKind::RetryBusinessRequest { provider_script } => {
                reserve_business_provider_outputs(
                    planned.id(),
                    provider_script,
                    &mut declared_outputs,
                    &mut active_payment_intent,
                    &mut provider_objects,
                    &mut held_provider_gate,
                );
            }
            PlanActionKind::RetrievePaymentIntent
            | PlanActionKind::ConfirmPaymentIntent { .. }
            | PlanActionKind::RetryProviderRequest { .. } => {
                inputs.push(CaseActionInput {
                    slot: CaseInputSlot::PaymentIntentId,
                    source: active_payment_intent.ok_or(
                        CaseTraceMaterializationError::MissingDynamicSource {
                            action_id: planned.id(),
                            input: CaseInputSlot::PaymentIntentId,
                        },
                    )?,
                });
                let held = match *planned.kind() {
                    PlanActionKind::ConfirmPaymentIntent { provider_script }
                    | PlanActionKind::RetryProviderRequest { provider_script } => {
                        provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay
                    }
                    _ => false,
                };
                if held {
                    let output = CaseOutputRef::new(planned.id(), CaseOutputSlot::ProviderGateId);
                    declared_outputs.insert(output);
                    held_provider_gate = Some(output);
                }
            }
            PlanActionKind::GenerateProviderEvent => {
                let source = provider_objects
                    .get(generated_provider_events)
                    .copied()
                    .ok_or(CaseTraceMaterializationError::MissingDynamicSource {
                        action_id: planned.id(),
                        input: CaseInputSlot::PaymentIntentId,
                    })?;
                generated_provider_events += 1;
                inputs.push(CaseActionInput {
                    slot: CaseInputSlot::PaymentIntentId,
                    source,
                });
                declared_outputs.insert(CaseOutputRef::new(planned.id(), CaseOutputSlot::EventId));
            }
            PlanActionKind::ReleaseProviderGate => {
                inputs.push(CaseActionInput {
                    slot: CaseInputSlot::ProviderGateId,
                    source: held_provider_gate.take().ok_or(
                        CaseTraceMaterializationError::MissingDynamicSource {
                            action_id: planned.id(),
                            input: CaseInputSlot::ProviderGateId,
                        },
                    )?,
                });
            }
            PlanActionKind::DeliverWebhook
            | PlanActionKind::DuplicateWebhook
            | PlanActionKind::DelayWebhook { .. }
            | PlanActionKind::ReorderWebhooks
            | PlanActionKind::DropWebhook
            | PlanActionKind::KillApplication { .. }
            | PlanActionKind::RestartAndAwaitHealth
            | PlanActionKind::WaitForQuiescence
            | PlanActionKind::CheckCheckpoint { .. } => {}
        }
        actions.push(CompiledCaseAction {
            id: planned.id(),
            logical_sequence: planned.logical_sequence(),
            dependencies: planned.dependencies().clone(),
            decision_number: planned.decision_number(),
            eligible_count: planned.eligible_count(),
            kind: *planned.kind(),
            inputs,
            declared_outputs,
        });
    }
    Ok(actions)
}

fn reserve_business_provider_outputs(
    action_id: ActionId,
    provider_script: crate::plan::ProviderOutcomeScript,
    declared_outputs: &mut BTreeSet<CaseOutputRef>,
    active_payment_intent: &mut Option<CaseOutputRef>,
    provider_objects: &mut Vec<CaseOutputRef>,
    held_provider_gate: &mut Option<CaseOutputRef>,
) {
    for (occurrence, outcome) in provider_script.outcomes().enumerate() {
        if !provider_create_commits(outcome) {
            continue;
        }
        let output = CaseOutputRef::for_occurrence(
            action_id,
            CaseOutputSlot::PaymentIntentId,
            u8::try_from(occurrence).expect("v1 provider scripts have at most two calls"),
        );
        declared_outputs.insert(output);
        *active_payment_intent = Some(output);
        provider_objects.push(output);
    }
    if provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay {
        let output = CaseOutputRef::new(action_id, CaseOutputSlot::ProviderGateId);
        declared_outputs.insert(output);
        *held_provider_gate = Some(output);
    }
}

const fn provider_create_commits(outcome: ProviderOutcome) -> bool {
    !matches!(
        outcome,
        ProviderOutcome::PreExecute429 | ProviderOutcome::PreExecute500
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaseTraceMaterializationError {
    InvalidPlannedCase,
    DuplicateOutput(CaseOutputRef),
    MissingOutput(CaseOutputRef),
    UnexpectedOutput(CaseOutputRef),
    OutputTypeMismatch {
        output_ref: CaseOutputRef,
        actual: CaseOutputSlot,
    },
    MissingDynamicSource {
        action_id: ActionId,
        input: CaseInputSlot,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompileError {
    DuplicateActionId(ActionId),
    DuplicateInput {
        action_id: ActionId,
        input: InputSlot,
    },
    DuplicateOutput(OutputRef),
    MissingInput {
        action_id: ActionId,
        input: InputSlot,
    },
    MissingOutput(OutputRef),
    UnexpectedInput {
        action_id: ActionId,
        input: InputSlot,
    },
    UnexpectedOutput(OutputRef),
    UnknownDependency {
        action_id: ActionId,
        dependency: ActionId,
    },
    DependencyNotEarlier {
        action_id: ActionId,
        dependency: ActionId,
    },
    OutputTypeMismatch {
        output_ref: OutputRef,
        actual: OutputSlot,
    },
    InputTypeMismatch {
        action_id: ActionId,
        input: InputSlot,
        expected: OutputSlot,
        actual: OutputSlot,
    },
    InputSourceNotDependency {
        action_id: ActionId,
        input: InputSlot,
        source_action_id: ActionId,
    },
    InputSourceNotDeclared {
        action_id: ActionId,
        input: InputSlot,
        source: OutputRef,
    },
}
