//! Real loopback HTTP execution for a checkout response held by the fixture.

use std::{collections::BTreeSet, time::Duration};

use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use thiserror::Error;
use tiv_core::{
    plan::{PlanActionKind, ProviderOutcome},
    trace::{CaseCapturedValue, CaseOutputRef, CaseOutputSlot},
};
use tokio::{task::JoinHandle, time::timeout};

use crate::{
    campaign::{CaseEffectAdapter, CaseEffectFuture, CaseEffectOutput, CaseEffectRequest},
    journal::{JournalError, ObservationEvent, ObservationProducer},
};

const MAX_DRIVER_BODY_BYTES: usize = 16 * 1024;
const MAX_CONTROL_TOKEN_BYTES: usize = 1_024;

/// Secret-bearing, loopback-only configuration for one held checkout path.
///
/// This type intentionally implements neither `Debug` nor `Serialize`.
pub struct HeldCheckoutHttpConfig {
    driver_url: Url,
    driver_body: serde_json::Value,
    expected_operation_id: String,
    expected_amount_minor: i64,
    expected_currency: String,
    fixture_control_url: Url,
    fixture_control_token: String,
    control_sequence: u64,
    timeout: Duration,
    poll_interval: Duration,
}

impl HeldCheckoutHttpConfig {
    /// Validates the bounded driver and fixture-control contract.
    ///
    /// # Errors
    ///
    /// Returns [`HeldCheckoutHttpConfigError`] unless both targets are
    /// explicit loopback HTTP endpoints, the driver body is a small JSON
    /// object with an operation ID, the control token is usable as a header,
    /// and all sequence and time bounds are positive.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        driver_url: impl AsRef<str>,
        driver_body: serde_json::Value,
        fixture_control_url: impl AsRef<str>,
        fixture_control_token: impl Into<String>,
        control_sequence: u64,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self, HeldCheckoutHttpConfigError> {
        let driver_url = normalize_loopback_url(driver_url.as_ref(), true)?;
        let fixture_control_url = normalize_loopback_url(fixture_control_url.as_ref(), false)?;
        let encoded_body = serde_json::to_vec(&driver_body)
            .map_err(|_| HeldCheckoutHttpConfigError::InvalidDriverBody)?;
        let body = driver_body
            .as_object()
            .ok_or(HeldCheckoutHttpConfigError::InvalidDriverBody)?;
        let expected_operation_id = body
            .get("operation_id")
            .and_then(serde_json::Value::as_str)
            .filter(|operation_id| valid_operation_id(operation_id))
            .ok_or(HeldCheckoutHttpConfigError::InvalidDriverBody)?
            .to_owned();
        let expected_amount_minor = body
            .get("amount_minor")
            .and_then(serde_json::Value::as_i64)
            .filter(|amount| *amount > 0)
            .ok_or(HeldCheckoutHttpConfigError::InvalidDriverBody)?;
        let expected_currency = body
            .get("currency")
            .and_then(serde_json::Value::as_str)
            .filter(|currency| {
                currency.len() == 3 && currency.bytes().all(|byte| byte.is_ascii_lowercase())
            })
            .ok_or(HeldCheckoutHttpConfigError::InvalidDriverBody)?
            .to_owned();
        if encoded_body.len() > MAX_DRIVER_BODY_BYTES {
            return Err(HeldCheckoutHttpConfigError::InvalidDriverBody);
        }
        let fixture_control_token = fixture_control_token.into();
        if fixture_control_token.trim().is_empty()
            || fixture_control_token.len() > MAX_CONTROL_TOKEN_BYTES
            || reqwest::header::HeaderValue::from_str(&fixture_control_token).is_err()
        {
            return Err(HeldCheckoutHttpConfigError::InvalidControlToken);
        }
        if control_sequence == 0
            || timeout.is_zero()
            || poll_interval.is_zero()
            || poll_interval > timeout
        {
            return Err(HeldCheckoutHttpConfigError::InvalidBounds);
        }
        Ok(Self {
            driver_url,
            driver_body,
            expected_operation_id,
            expected_amount_minor,
            expected_currency,
            fixture_control_url,
            fixture_control_token,
            control_sequence,
            timeout,
            poll_interval,
        })
    }
}

fn valid_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn normalize_loopback_url(
    value: &str,
    allow_path: bool,
) -> Result<Url, HeldCheckoutHttpConfigError> {
    if value.chars().any(char::is_whitespace) {
        return Err(HeldCheckoutHttpConfigError::NonLoopbackUrl);
    }
    let url = Url::parse(value).map_err(|_| HeldCheckoutHttpConfigError::NonLoopbackUrl)?;
    let valid_path = allow_path || matches!(url.path(), "" | "/");
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !valid_path
    {
        return Err(HeldCheckoutHttpConfigError::NonLoopbackUrl);
    }
    Ok(url)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum HeldCheckoutHttpConfigError {
    #[error("held checkout URLs must be explicit loopback HTTP endpoints")]
    NonLoopbackUrl,
    #[error("held checkout driver body is outside the supported contract")]
    InvalidDriverBody,
    #[error("fixture control token is outside the supported header contract")]
    InvalidControlToken,
    #[error("held checkout sequence and timeout bounds must be positive")]
    InvalidBounds,
}

/// Stateful adapter for the real commit-then-delay checkout/release boundary.
pub struct HeldCheckoutHttpAdapter {
    config: HeldCheckoutHttpConfig,
    client: Client,
    pending: Option<PendingDriver>,
    known_payment_intents: BTreeSet<String>,
    fixture_producer_sequence: u64,
}

impl HeldCheckoutHttpAdapter {
    /// Creates a redirect-free HTTP client for the validated loopback targets.
    ///
    /// # Errors
    ///
    /// Returns [`HeldCheckoutHttpError::ClientBuild`] when the client cannot
    /// be constructed.
    pub fn new(config: HeldCheckoutHttpConfig) -> Result<Self, HeldCheckoutHttpError> {
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(config.timeout)
            .timeout(config.timeout)
            .build()
            .map_err(HeldCheckoutHttpError::ClientBuild)?;
        Ok(Self {
            config,
            client,
            pending: None,
            known_payment_intents: BTreeSet::new(),
            fixture_producer_sequence: 0,
        })
    }

    async fn start_held_checkout(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<CaseEffectOutput, HeldCheckoutHttpError> {
        if self.pending.is_some() {
            return Err(HeldCheckoutHttpError::DriverAlreadyPending);
        }
        require_output_contract(
            request.expected_outputs(),
            &[
                CaseOutputSlot::PaymentIntentId,
                CaseOutputSlot::ProviderGateId,
            ],
        )?;
        let baseline = self.fixture_state().await?;
        self.validate_control_sequence(&baseline)?;
        if !baseline.held_gates.is_empty() {
            return Err(HeldCheckoutHttpError::UnexpectedFixtureState);
        }
        self.known_payment_intents = baseline
            .payment_intents
            .iter()
            .map(|payment_intent| payment_intent.id.clone())
            .collect();

        let client = self.client.clone();
        let driver_url = self.config.driver_url.clone();
        let driver_body = self.config.driver_body.clone();
        let driver_timeout = self.config.timeout;
        let task = tokio::spawn(async move {
            timeout(driver_timeout, async move {
                let response = client
                    .post(driver_url)
                    .json(&driver_body)
                    .send()
                    .await
                    .map_err(HeldCheckoutHttpError::DriverRequest)?;
                if !response.status().is_success() {
                    return Err(HeldCheckoutHttpError::UnexpectedStatus {
                        step: "driver checkout",
                        status: response.status(),
                    });
                }
                response
                    .json::<DriverCheckoutResponse>()
                    .await
                    .map_err(HeldCheckoutHttpError::DriverRequest)
            })
            .await
            .map_err(|_| HeldCheckoutHttpError::Timeout("driver checkout"))?
        });

        let held = match self.wait_for_held_response().await {
            Ok(held) if !task.is_finished() => held,
            Ok(_) => {
                abort_task(task).await;
                return Err(HeldCheckoutHttpError::DriverCompletedBeforeRelease);
            }
            Err(error) => {
                abort_task(task).await;
                return Err(error);
            }
        };
        let next_sequence = match self.next_fixture_sequence() {
            Ok(sequence) => sequence,
            Err(error) => {
                abort_task(task).await;
                return Err(error);
            }
        };
        let captured = match held_captures(request.expected_outputs(), &held) {
            Ok(captured) => captured,
            Err(error) => {
                abort_task(task).await;
                return Err(error);
            }
        };
        if let Err(error) = request
            .record_observation(
                ObservationProducer::Fixture,
                next_sequence,
                ObservationEvent::ProviderResponseHeld {
                    gate_id: held.gate_id,
                },
            )
            .await
        {
            abort_task(task).await;
            return Err(HeldCheckoutHttpError::Journal(error));
        }
        self.fixture_producer_sequence = next_sequence;
        self.known_payment_intents
            .insert(held.payment_intent_id.clone());
        self.pending = Some(PendingDriver {
            task,
            gate_id: held.gate_id,
            payment_intent_id: held.payment_intent_id,
        });
        Ok(captured)
    }

    async fn release_held_checkout(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<CaseEffectOutput, HeldCheckoutHttpError> {
        require_output_contract(request.expected_outputs(), &[])?;
        let pending = self
            .pending
            .take()
            .ok_or(HeldCheckoutHttpError::NoPendingDriver)?;
        let next_control_sequence = self
            .config
            .control_sequence
            .checked_add(1)
            .ok_or(HeldCheckoutHttpError::SequenceExhausted)?;
        let release_result = self
            .release_gate(next_control_sequence, pending.gate_id)
            .await;
        let state = match release_result {
            Ok(state) => state,
            Err(error) => {
                abort_task(pending.task).await;
                return Err(error);
            }
        };
        if state.command_sequence != next_control_sequence
            || state
                .held_gates
                .iter()
                .any(|gate| gate.gate_id == pending.gate_id)
            || !state
                .payment_intents
                .iter()
                .any(|payment_intent| payment_intent.id == pending.payment_intent_id)
        {
            abort_task(pending.task).await;
            return Err(HeldCheckoutHttpError::UnexpectedFixtureState);
        }
        self.config.control_sequence = next_control_sequence;
        let next_fixture_sequence = match self.next_fixture_sequence() {
            Ok(sequence) => sequence,
            Err(error) => {
                abort_task(pending.task).await;
                return Err(error);
            }
        };
        if let Err(error) = request
            .record_observation(
                ObservationProducer::Fixture,
                next_fixture_sequence,
                ObservationEvent::ProviderResponseReleased {
                    gate_id: pending.gate_id,
                },
            )
            .await
        {
            abort_task(pending.task).await;
            return Err(HeldCheckoutHttpError::Journal(error));
        }
        self.fixture_producer_sequence = next_fixture_sequence;

        let driver = pending
            .task
            .await
            .map_err(HeldCheckoutHttpError::DriverJoin)??;
        if driver.payment_intent_id != pending.payment_intent_id
            || driver.operation_id != self.config.expected_operation_id
        {
            return Err(HeldCheckoutHttpError::DriverResponseMismatch);
        }
        Ok(Vec::new())
    }

    async fn wait_for_held_response(&self) -> Result<HeldResponse, HeldCheckoutHttpError> {
        timeout(self.config.timeout, async {
            loop {
                let state = self.fixture_state().await?;
                self.validate_control_sequence(&state)?;
                match state.held_gates.as_slice() {
                    [] => {
                        if state.payment_intents.iter().any(|payment_intent| {
                            !self.known_payment_intents.contains(&payment_intent.id)
                        }) {
                            return Err(HeldCheckoutHttpError::UnexpectedFixtureState);
                        }
                    }
                    [gate] => {
                        let mut new_payment_intents =
                            state.payment_intents.iter().filter(|payment_intent| {
                                !self.known_payment_intents.contains(&payment_intent.id)
                            });
                        let payment_intent = new_payment_intents
                            .next()
                            .ok_or(HeldCheckoutHttpError::UnexpectedFixtureState)?;
                        if new_payment_intents.next().is_some()
                            || payment_intent.amount_minor != self.config.expected_amount_minor
                            || payment_intent.currency != self.config.expected_currency
                            || payment_intent.status != "requires_confirmation"
                            || payment_intent.operation_id.as_deref()
                                != Some(self.config.expected_operation_id.as_str())
                        {
                            return Err(HeldCheckoutHttpError::UnexpectedFixtureState);
                        }
                        return Ok(HeldResponse {
                            gate_id: gate.gate_id,
                            payment_intent_id: payment_intent.id.clone(),
                        });
                    }
                    _ => return Err(HeldCheckoutHttpError::UnexpectedFixtureState),
                }
                tokio::time::sleep(self.config.poll_interval).await;
            }
        })
        .await
        .map_err(|_| HeldCheckoutHttpError::Timeout("fixture held response"))?
    }

    async fn fixture_state(&self) -> Result<FixtureState, HeldCheckoutHttpError> {
        let response = self
            .client
            .get(
                self.config
                    .fixture_control_url
                    .join("v1/control/state")
                    .map_err(|_| HeldCheckoutHttpError::UnexpectedFixtureState)?,
            )
            .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
            .send()
            .await
            .map_err(HeldCheckoutHttpError::ControlRequest)?;
        require_status(response.status(), "fixture state")?;
        response
            .json()
            .await
            .map_err(HeldCheckoutHttpError::ControlRequest)
    }

    async fn release_gate(
        &self,
        command_sequence: u64,
        gate_id: u64,
    ) -> Result<FixtureState, HeldCheckoutHttpError> {
        let response = self
            .client
            .post(
                self.config
                    .fixture_control_url
                    .join("v1/control/release-gate")
                    .map_err(|_| HeldCheckoutHttpError::UnexpectedFixtureState)?,
            )
            .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
            .json(&serde_json::json!({
                "command_sequence": command_sequence,
                "gate_id": gate_id,
            }))
            .send()
            .await
            .map_err(HeldCheckoutHttpError::ControlRequest)?;
        require_status(response.status(), "fixture gate release")?;
        response
            .json()
            .await
            .map_err(HeldCheckoutHttpError::ControlRequest)
    }

    fn validate_control_sequence(&self, state: &FixtureState) -> Result<(), HeldCheckoutHttpError> {
        if state.command_sequence != self.config.control_sequence {
            return Err(HeldCheckoutHttpError::UnexpectedFixtureState);
        }
        Ok(())
    }

    fn next_fixture_sequence(&self) -> Result<u64, HeldCheckoutHttpError> {
        self.fixture_producer_sequence
            .checked_add(1)
            .ok_or(HeldCheckoutHttpError::SequenceExhausted)
    }
}

impl CaseEffectAdapter for HeldCheckoutHttpAdapter {
    type Error = HeldCheckoutHttpError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        Box::pin(async move {
            match request.action().kind() {
                PlanActionKind::DriveCheckout {
                    outcome: ProviderOutcome::CommitThenDelay,
                }
                | PlanActionKind::RetryBusinessRequest {
                    outcome: ProviderOutcome::CommitThenDelay,
                } => self.start_held_checkout(&request).await,
                PlanActionKind::ReleaseProviderGate => self.release_held_checkout(&request).await,
                _ => Err(HeldCheckoutHttpError::UnsupportedAction),
            }
        })
    }
}

impl Drop for HeldCheckoutHttpAdapter {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.task.abort();
        }
    }
}

fn require_output_contract(
    outputs: &[CaseOutputRef],
    expected: &[CaseOutputSlot],
) -> Result<(), HeldCheckoutHttpError> {
    let actual = outputs
        .iter()
        .map(|output| output.slot())
        .collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected || outputs.len() != expected.len() {
        return Err(HeldCheckoutHttpError::UnexpectedOutputContract);
    }
    Ok(())
}

fn held_captures(
    outputs: &[CaseOutputRef],
    held: &HeldResponse,
) -> Result<CaseEffectOutput, HeldCheckoutHttpError> {
    outputs
        .iter()
        .copied()
        .map(|output| {
            let value = match output.slot() {
                CaseOutputSlot::PaymentIntentId => {
                    CaseCapturedValue::payment_intent_id(held.payment_intent_id.clone())
                        .map_err(|_| HeldCheckoutHttpError::UnexpectedFixtureState)?
                }
                CaseOutputSlot::ProviderGateId => CaseCapturedValue::provider_gate_id(held.gate_id)
                    .map_err(|_| HeldCheckoutHttpError::UnexpectedFixtureState)?,
                CaseOutputSlot::EventId => {
                    return Err(HeldCheckoutHttpError::UnexpectedOutputContract);
                }
            };
            Ok((output, value))
        })
        .collect()
}

fn require_status(status: StatusCode, step: &'static str) -> Result<(), HeldCheckoutHttpError> {
    if status.is_success() {
        Ok(())
    } else {
        Err(HeldCheckoutHttpError::UnexpectedStatus { step, status })
    }
}

async fn abort_task(task: JoinHandle<Result<DriverCheckoutResponse, HeldCheckoutHttpError>>) {
    task.abort();
    let _ = task.await;
}

struct PendingDriver {
    task: JoinHandle<Result<DriverCheckoutResponse, HeldCheckoutHttpError>>,
    gate_id: u64,
    payment_intent_id: String,
}

struct HeldResponse {
    gate_id: u64,
    payment_intent_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DriverCheckoutResponse {
    payment_intent_id: String,
    operation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureState {
    command_sequence: u64,
    #[serde(rename = "remaining_outcomes")]
    _remaining_outcomes: usize,
    held_gates: Vec<HeldGate>,
    payment_intents: Vec<FixturePaymentIntent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeldGate {
    gate_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixturePaymentIntent {
    id: String,
    amount_minor: i64,
    currency: String,
    status: String,
    operation_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum HeldCheckoutHttpError {
    #[error("could not build the held checkout HTTP client: {0}")]
    ClientBuild(reqwest::Error),
    #[error("held checkout driver request failed: {0}")]
    DriverRequest(reqwest::Error),
    #[error("fixture control request failed: {0}")]
    ControlRequest(reqwest::Error),
    #[error("held checkout step {step} returned HTTP {status}")]
    UnexpectedStatus {
        step: &'static str,
        status: StatusCode,
    },
    #[error("held checkout step timed out: {0}")]
    Timeout(&'static str),
    #[error("held checkout fixture state did not match the exact gate contract")]
    UnexpectedFixtureState,
    #[error("held checkout action declared an unexpected output contract")]
    UnexpectedOutputContract,
    #[error("a held checkout driver is already pending")]
    DriverAlreadyPending,
    #[error("no held checkout driver is pending")]
    NoPendingDriver,
    #[error("held checkout driver completed before fixture release")]
    DriverCompletedBeforeRelease,
    #[error("held checkout driver response did not match the captured provider object")]
    DriverResponseMismatch,
    #[error("held checkout sequence exhausted")]
    SequenceExhausted,
    #[error("held checkout driver task failed: {0}")]
    DriverJoin(tokio::task::JoinError),
    #[error("held checkout observation journal failed: {0}")]
    Journal(#[from] JournalError),
    #[error("action is outside the held checkout HTTP adapter boundary")]
    UnsupportedAction,
}
