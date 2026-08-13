//! Read-only replay preparation for compiled traces.

use serde::Serialize;
use thiserror::Error;
use tiv_core::{
    decision::Seed,
    trace::{ActionId, ActionKind, CapturedValue, CompiledTrace, InputSlot, OutputRef, OutputSlot},
};

/// A read-only runtime replay plan compiled from a fully bound trace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReplayPlan {
    schema_version: u16,
    seed: Seed,
    action_count: usize,
    steps: Vec<ReplayStep>,
}

impl ReplayPlan {
    /// Converts a validated compiled trace into runtime-executable step
    /// descriptors without opening sockets, touching databases, or releasing
    /// customer-code effects.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayPlanError`] when a trace is structurally valid but not
    /// executable by the current runtime slice.
    pub fn from_trace(trace: &CompiledTrace) -> Result<Self, ReplayPlanError> {
        let steps = trace
            .replay_actions()
            .map(|action| {
                let operation = match action.kind() {
                    ActionKind::DriveCheckout => {
                        let output_ref = OutputRef::new(action.id(), OutputSlot::PaymentIntentId);
                        let captured_payment_intent_id =
                            payment_intent_id(trace.resolve(output_ref))
                                .ok_or(ReplayPlanError::MissingPaymentIntentOutput(action.id()))?;
                        ReplayOperation::DriveCheckout {
                            captured_payment_intent_id,
                        }
                    }
                    ActionKind::ConfirmPaymentIntent => {
                        let payment_intent_id =
                            payment_intent_id(action.input(InputSlot::PaymentIntentId))
                                .ok_or(ReplayPlanError::MissingPaymentIntentInput(action.id()))?;
                        ReplayOperation::ConfirmPaymentIntent { payment_intent_id }
                    }
                };
                Ok(ReplayStep {
                    action_id: action.id(),
                    operation,
                })
            })
            .collect::<Result<Vec<_>, ReplayPlanError>>()?;
        Ok(Self {
            schema_version: trace.schema_version(),
            seed: trace.seed(),
            action_count: trace.action_count(),
            steps,
        })
    }

    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    #[must_use]
    pub const fn seed(&self) -> Seed {
        self.seed
    }

    #[must_use]
    pub const fn action_count(&self) -> usize {
        self.action_count
    }

    #[must_use]
    pub fn steps(&self) -> &[ReplayStep] {
        &self.steps
    }

    #[must_use]
    pub fn step(&self, action_id: ActionId) -> Option<&ReplayStep> {
        self.steps.iter().find(|step| step.action_id == action_id)
    }
}

/// One ordered runtime replay step.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReplayStep {
    action_id: ActionId,
    #[serde(flatten)]
    operation: ReplayOperation,
}

impl ReplayStep {
    #[must_use]
    pub const fn action_id(&self) -> ActionId {
        self.action_id
    }

    #[must_use]
    pub const fn operation(&self) -> &ReplayOperation {
        &self.operation
    }
}

/// The current runtime's supported replay operations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ReplayOperation {
    DriveCheckout { captured_payment_intent_id: String },
    ConfirmPaymentIntent { payment_intent_id: String },
}

/// The executable script shape currently supported by the reference app spike.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceReplayScript {
    fixture_seed: Seed,
    expected_payment_intent_id: String,
}

impl ReferenceReplayScript {
    /// Derives the reference app replay script from a runtime plan without
    /// hard-coding fixture identities in the executor.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceReplayScriptError`] when the plan is not the current
    /// commit-then-close checkout shape supported by the reference app replay.
    pub fn from_plan(plan: &ReplayPlan) -> Result<Self, ReferenceReplayScriptError> {
        let steps = plan.steps();
        if steps.len() != 2 {
            return Err(ReferenceReplayScriptError::UnexpectedStepCount {
                actual: steps.len(),
            });
        }
        let ReplayOperation::DriveCheckout {
            captured_payment_intent_id,
        } = steps[0].operation()
        else {
            return Err(ReferenceReplayScriptError::ExpectedDriveCheckout);
        };
        let ReplayOperation::ConfirmPaymentIntent { payment_intent_id } = steps[1].operation()
        else {
            return Err(ReferenceReplayScriptError::ExpectedConfirmPaymentIntent);
        };
        if captured_payment_intent_id != payment_intent_id {
            return Err(ReferenceReplayScriptError::PaymentIntentMismatch);
        }
        Ok(Self {
            fixture_seed: plan.seed(),
            expected_payment_intent_id: captured_payment_intent_id.clone(),
        })
    }

    #[must_use]
    pub const fn fixture_seed(&self) -> Seed {
        self.fixture_seed
    }

    #[must_use]
    pub fn expected_payment_intent_id(&self) -> &str {
        &self.expected_payment_intent_id
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ReferenceReplayScriptError {
    #[error("reference replay requires exactly two trace steps, got {actual}")]
    UnexpectedStepCount { actual: usize },
    #[error("first replay step must drive checkout")]
    ExpectedDriveCheckout,
    #[error("second replay step must confirm the PaymentIntent")]
    ExpectedConfirmPaymentIntent,
    #[error("confirm step targets a different PaymentIntent than checkout produced")]
    PaymentIntentMismatch,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ReplayPlanError {
    #[error("drive checkout action {0:?} did not capture a PaymentIntent ID")]
    MissingPaymentIntentOutput(ActionId),
    #[error("confirm action {0:?} did not resolve a PaymentIntent ID")]
    MissingPaymentIntentInput(ActionId),
}

fn payment_intent_id(value: Option<&CapturedValue>) -> Option<String> {
    match value {
        Some(CapturedValue::PaymentIntentId(id)) => Some(id.as_str().to_owned()),
        Some(CapturedValue::EventId(_)) | None => None,
    }
}
