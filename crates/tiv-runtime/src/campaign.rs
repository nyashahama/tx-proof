//! Serial, journal-first execution of one validated case plan.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::Path,
    pin::Pin,
    time::Instant,
};

use tiv_core::{
    plan::{PlannedAction, PlannedCase},
    trace::{
        ActionId, CaseCapturedValue, CaseInputBinding, CaseInputSlot, CaseOutputRef,
        CaseOutputSlot, CaseTraceMaterializationError, CaseTraceMaterializer, CompiledCaseTrace,
    },
};

use crate::journal::{
    InvalidJournalId, JournalContext, JournalError, JournalLimits, JournalSummary, Observation,
    ObservationEvent, ObservationJournal, ObservationProducer,
};

pub struct CaseEffectRequest<'a> {
    action: &'a PlannedAction,
    inputs: &'a [(CaseInputSlot, CaseCapturedValue)],
    expected_outputs: &'a [CaseOutputRef],
    context: &'a JournalContext,
    journal: &'a ObservationJournal,
    start: Instant,
}

impl CaseEffectRequest<'_> {
    #[must_use]
    pub const fn action(&self) -> &PlannedAction {
        self.action
    }

    #[must_use]
    pub fn input(&self, slot: CaseInputSlot) -> Option<&CaseCapturedValue> {
        self.inputs
            .iter()
            .find_map(|(candidate, value)| (*candidate == slot).then_some(value))
    }

    #[must_use]
    pub const fn expected_outputs(&self) -> &[CaseOutputRef] {
        self.expected_outputs
    }

    /// Durably records an observation produced while this action is active.
    ///
    /// The producer owns its strictly increasing sequence. Completion means
    /// the record has been flushed and synchronized to the case journal.
    ///
    /// # Errors
    ///
    /// Returns a typed journal error when the record is out of sequence,
    /// outside the fixed case bound, or cannot be durably written.
    pub async fn record_observation(
        &self,
        producer: ObservationProducer,
        producer_sequence: u64,
        event: ObservationEvent,
    ) -> Result<(), JournalError> {
        self.journal
            .append(Observation::new(
                self.context.clone(),
                producer,
                producer_sequence,
                elapsed_micros(self.start),
                event,
            ))
            .await
            .map(|_| ())
    }
}

pub type CaseEffectOutput = Vec<(CaseOutputRef, CaseCapturedValue)>;
pub type CaseEffectFuture<'a, E> =
    Pin<Box<dyn Future<Output = Result<CaseEffectOutput, E>> + Send + 'a>>;

/// Stateful adapter for one case's ordered observable action boundaries.
///
/// Completion means that the planned boundary was reached, not necessarily
/// that every child I/O task ended. For example, `commit_then_delay` returns
/// after the held response and gate are observable; the adapter retains that
/// suspended request until the later release action resumes and joins it.
/// Implementations must not consume campaign randomness or reorder calls.
pub trait CaseEffectAdapter {
    type Error;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error>;
}

#[derive(Debug)]
pub struct ExecutedCase {
    trace: CompiledCaseTrace,
    journal_summary: JournalSummary,
}

impl ExecutedCase {
    #[must_use]
    pub const fn trace(&self) -> &CompiledCaseTrace {
        &self.trace
    }

    #[must_use]
    pub const fn journal_summary(&self) -> &JournalSummary {
        &self.journal_summary
    }
}

/// Executes one plan in exact action order and finalizes its journal.
///
/// Every action intent is flushed and synchronized before the adapter future
/// is created. Successful captures are validated before the corresponding
/// action outcome is written.
///
/// # Errors
///
/// Returns [`CaseExecutionError`] before journal creation for an invalid plan
/// or context, on a create collision, or after finalizing the journal when an
/// action, capture, materialization, append, or final sync fails.
pub async fn execute_planned_case<A>(
    run_id: impl Into<String>,
    case_id: impl Into<String>,
    planned_case: &PlannedCase,
    journal_path: impl AsRef<Path>,
    adapter: &mut A,
) -> Result<ExecutedCase, CaseExecutionError<A::Error>>
where
    A: CaseEffectAdapter,
{
    let required_outputs = CaseTraceMaterializer::required_outputs(planned_case)
        .map_err(CaseExecutionError::InvalidPlan)?;
    let required_inputs = CaseTraceMaterializer::required_inputs(planned_case)
        .map_err(CaseExecutionError::InvalidPlan)?;
    let run_id = run_id.into();
    let case_id = case_id.into();
    let contexts = planned_case
        .actions()
        .iter()
        .map(|action| JournalContext::new(&run_id, &case_id, action.id()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(CaseExecutionError::InvalidContext)?;
    let max_records = planned_case
        .actions()
        .len()
        .checked_mul(4)
        .ok_or(CaseExecutionError::InvalidJournalBounds)?;
    let limits =
        JournalLimits::new(1, max_records).map_err(|_| CaseExecutionError::InvalidJournalBounds)?;
    let journal = ObservationJournal::create(journal_path, limits)
        .await
        .map_err(CaseExecutionError::JournalCreate)?;

    let execution = run_actions(
        planned_case,
        &contexts,
        &required_inputs,
        &required_outputs,
        &journal,
        adapter,
    )
    .await;
    let journal_result = journal.finish().await;

    match execution {
        Ok(trace) => match journal_result {
            Ok(journal_summary) => Ok(ExecutedCase {
                trace,
                journal_summary,
            }),
            Err(error) => Err(CaseExecutionError::Failed(CaseExecutionFailure {
                cause: CaseExecutionCause::Finalization,
                journal_result: Err(error),
            })),
        },
        Err(cause) => Err(CaseExecutionError::Failed(CaseExecutionFailure {
            cause,
            journal_result,
        })),
    }
}

async fn run_actions<A>(
    planned_case: &PlannedCase,
    contexts: &[JournalContext],
    required_inputs: &[CaseInputBinding],
    required_outputs: &[CaseOutputRef],
    journal: &ObservationJournal,
    adapter: &mut A,
) -> Result<CompiledCaseTrace, CaseExecutionCause<A::Error>>
where
    A: CaseEffectAdapter,
{
    let start = Instant::now();
    let mut captured = Vec::new();

    for (index, (action, context)) in planned_case.actions().iter().zip(contexts).enumerate() {
        let inputs = resolve_inputs(action.id(), required_inputs, &captured)?;
        let intent_sequence = observation_sequence(index, 1)?;
        journal
            .append(Observation::new(
                context.clone(),
                ObservationProducer::Orchestrator,
                intent_sequence,
                elapsed_micros(start),
                ObservationEvent::ActionIntent,
            ))
            .await
            .map_err(CaseExecutionCause::JournalAppend)?;

        let expected_outputs = required_outputs
            .iter()
            .copied()
            .filter(|output| output.action_id() == action.id())
            .collect::<Vec<_>>();
        let observed = adapter
            .execute(CaseEffectRequest {
                action,
                inputs: &inputs,
                expected_outputs: &expected_outputs,
                context,
                journal,
                start,
            })
            .await
            .map_err(CaseExecutionCause::Effect)?;
        let observed =
            validate_captures(&expected_outputs, observed).map_err(CaseExecutionCause::Capture)?;
        captured.extend(observed);

        let outcome_sequence = observation_sequence(index, 2)?;
        journal
            .append(Observation::new(
                context.clone(),
                ObservationProducer::Orchestrator,
                outcome_sequence,
                elapsed_micros(start),
                ObservationEvent::ActionOutcome,
            ))
            .await
            .map_err(CaseExecutionCause::JournalAppend)?;
    }

    CaseTraceMaterializer::materialize(planned_case, captured)
        .map_err(CaseExecutionCause::Materialization)
}

fn resolve_inputs<E>(
    action_id: ActionId,
    required_inputs: &[CaseInputBinding],
    captured: &[(CaseOutputRef, CaseCapturedValue)],
) -> Result<Vec<(CaseInputSlot, CaseCapturedValue)>, CaseExecutionCause<E>> {
    required_inputs
        .iter()
        .copied()
        .filter(|binding| binding.action_id() == action_id)
        .map(|binding| {
            let value = captured
                .iter()
                .find_map(|(output_ref, value)| {
                    (*output_ref == binding.source()).then_some(value.clone())
                })
                .ok_or(CaseExecutionCause::MissingResolvedInput {
                    action_id,
                    input: binding.slot(),
                })?;
            Ok((binding.slot(), value))
        })
        .collect()
}

fn validate_captures(
    expected: &[CaseOutputRef],
    observed: Vec<(CaseOutputRef, CaseCapturedValue)>,
) -> Result<Vec<(CaseOutputRef, CaseCapturedValue)>, CaseCaptureError> {
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    let mut observed_by_ref = BTreeMap::new();
    for (output_ref, value) in observed {
        if observed_by_ref.insert(output_ref, value).is_some() {
            return Err(CaseCaptureError::DuplicateOutput(output_ref));
        }
    }
    if let Some(&unexpected) = observed_by_ref
        .keys()
        .find(|output_ref| !expected.contains(output_ref))
    {
        return Err(CaseCaptureError::UnexpectedOutput(unexpected));
    }
    for output_ref in expected {
        let value = observed_by_ref
            .get(&output_ref)
            .ok_or(CaseCaptureError::MissingOutput(output_ref))?;
        if value.slot() != output_ref.slot() {
            return Err(CaseCaptureError::OutputTypeMismatch {
                output_ref,
                actual: value.slot(),
            });
        }
    }
    Ok(observed_by_ref.into_iter().collect())
}

fn observation_sequence<E>(
    action_index: usize,
    offset: usize,
) -> Result<u64, CaseExecutionCause<E>> {
    action_index
        .checked_mul(2)
        .and_then(|value| value.checked_add(offset))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(CaseExecutionCause::<E>::SequenceOverflow)
}

fn elapsed_micros(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaseCaptureError {
    DuplicateOutput(CaseOutputRef),
    MissingOutput(CaseOutputRef),
    UnexpectedOutput(CaseOutputRef),
    OutputTypeMismatch {
        output_ref: CaseOutputRef,
        actual: CaseOutputSlot,
    },
}

#[derive(Debug)]
pub enum CaseExecutionCause<E> {
    JournalAppend(JournalError),
    Effect(E),
    Capture(CaseCaptureError),
    Materialization(CaseTraceMaterializationError),
    MissingResolvedInput {
        action_id: ActionId,
        input: CaseInputSlot,
    },
    SequenceOverflow,
    Finalization,
}

#[derive(Debug)]
pub struct CaseExecutionFailure<E> {
    cause: CaseExecutionCause<E>,
    journal_result: Result<JournalSummary, JournalError>,
}

impl<E> CaseExecutionFailure<E> {
    #[must_use]
    pub const fn cause(&self) -> &CaseExecutionCause<E> {
        &self.cause
    }

    pub const fn journal_result(&self) -> &Result<JournalSummary, JournalError> {
        &self.journal_result
    }
}

#[derive(Debug)]
pub enum CaseExecutionError<E> {
    InvalidPlan(CaseTraceMaterializationError),
    InvalidContext(InvalidJournalId),
    InvalidJournalBounds,
    JournalCreate(JournalError),
    Failed(CaseExecutionFailure<E>),
}
