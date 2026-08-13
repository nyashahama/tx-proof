use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

use crate::{
    decision::Seed,
    ids::{EventId, InvalidProviderId, PaymentIntentId},
};

pub const TRACE_SCHEMA_VERSION: u16 = 1;

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
