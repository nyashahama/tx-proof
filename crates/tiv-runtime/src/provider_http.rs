//! Real loopback HTTP execution for provider checkout, retrieval, confirmation,
//! and held-response boundaries.

use std::{collections::BTreeSet, time::Duration};

use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use thiserror::Error;
use tiv_core::{
    plan::{PlanActionKind, ProviderOutcome, ProviderOutcomeScript},
    trace::{CaseCapturedValue, CaseOutputRef, CaseOutputSlot},
};
use tokio::{task::JoinHandle, time::timeout};

use crate::{
    campaign::{CaseEffectAdapter, CaseEffectFuture, CaseEffectOutput, CaseEffectRequest},
    journal::{JournalError, ObservationEvent, ObservationProducer},
};

const MAX_DRIVER_BODY_BYTES: usize = 16 * 1024;
const MAX_CONTROL_TOKEN_BYTES: usize = 1_024;

/// Secret-bearing, loopback-only configuration for one provider case.
///
/// This type intentionally implements neither `Debug` nor `Serialize`.
pub struct ProviderHttpConfig {
    driver_url: Url,
    driver_body: serde_json::Value,
    expected_operation_id: String,
    expected_amount_minor: i64,
    expected_currency: String,
    fixture_data_url: Url,
    fixture_control_url: Url,
    fixture_control_token: String,
    control_sequence: u64,
    timeout: Duration,
    poll_interval: Duration,
}

impl ProviderHttpConfig {
    /// Validates the bounded driver and fixture-control contract.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderHttpConfigError`] unless all three targets are
    /// explicit loopback HTTP endpoints, the driver body is a small JSON object
    /// with an operation ID, the control token is usable as a header, and all
    /// sequence and time bounds are positive.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        driver_url: impl AsRef<str>,
        driver_body: serde_json::Value,
        fixture_data_url: impl AsRef<str>,
        fixture_control_url: impl AsRef<str>,
        fixture_control_token: impl Into<String>,
        control_sequence: u64,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self, ProviderHttpConfigError> {
        let driver_url = normalize_loopback_url(driver_url.as_ref(), true)?;
        let fixture_data_url = normalize_loopback_url(fixture_data_url.as_ref(), false)?;
        let fixture_control_url = normalize_loopback_url(fixture_control_url.as_ref(), false)?;
        let encoded_body = serde_json::to_vec(&driver_body)
            .map_err(|_| ProviderHttpConfigError::InvalidDriverBody)?;
        let body = driver_body
            .as_object()
            .ok_or(ProviderHttpConfigError::InvalidDriverBody)?;
        let expected_operation_id = body
            .get("operation_id")
            .and_then(serde_json::Value::as_str)
            .filter(|operation_id| valid_operation_id(operation_id))
            .ok_or(ProviderHttpConfigError::InvalidDriverBody)?
            .to_owned();
        let expected_amount_minor = body
            .get("amount_minor")
            .and_then(serde_json::Value::as_i64)
            .filter(|amount| *amount > 0)
            .ok_or(ProviderHttpConfigError::InvalidDriverBody)?;
        let expected_currency = body
            .get("currency")
            .and_then(serde_json::Value::as_str)
            .filter(|currency| {
                currency.len() == 3 && currency.bytes().all(|byte| byte.is_ascii_lowercase())
            })
            .ok_or(ProviderHttpConfigError::InvalidDriverBody)?
            .to_owned();
        if encoded_body.len() > MAX_DRIVER_BODY_BYTES {
            return Err(ProviderHttpConfigError::InvalidDriverBody);
        }
        let fixture_control_token = fixture_control_token.into();
        if fixture_control_token.trim().is_empty()
            || fixture_control_token.len() > MAX_CONTROL_TOKEN_BYTES
            || reqwest::header::HeaderValue::from_str(&fixture_control_token).is_err()
        {
            return Err(ProviderHttpConfigError::InvalidControlToken);
        }
        if control_sequence == 0
            || control_sequence == u64::MAX
            || timeout.is_zero()
            || poll_interval.is_zero()
            || poll_interval > timeout
        {
            return Err(ProviderHttpConfigError::InvalidBounds);
        }
        Ok(Self {
            driver_url,
            driver_body,
            expected_operation_id,
            expected_amount_minor,
            expected_currency,
            fixture_data_url,
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

fn normalize_loopback_url(value: &str, allow_path: bool) -> Result<Url, ProviderHttpConfigError> {
    if value.chars().any(char::is_whitespace) {
        return Err(ProviderHttpConfigError::NonLoopbackUrl);
    }
    let url = Url::parse(value).map_err(|_| ProviderHttpConfigError::NonLoopbackUrl)?;
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
        return Err(ProviderHttpConfigError::NonLoopbackUrl);
    }
    Ok(url)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProviderHttpConfigError {
    #[error("provider HTTP URLs must be explicit loopback HTTP endpoints")]
    NonLoopbackUrl,
    #[error("provider driver body is outside the supported contract")]
    InvalidDriverBody,
    #[error("fixture control token is outside the supported header contract")]
    InvalidControlToken,
    #[error("provider sequence and timeout bounds must be positive")]
    InvalidBounds,
}

/// Stateful adapter for real `PaymentIntent` checkout, retrieval, confirmation,
/// and commit-then-delay release boundaries.
pub struct ProviderHttpAdapter {
    config: ProviderHttpConfig,
    client: Client,
    pending: Option<PendingProviderRequest>,
    known_payment_intents: BTreeSet<String>,
    fixture_producer_sequence: u64,
}

impl ProviderHttpAdapter {
    /// Creates a redirect-free HTTP client for the validated loopback targets.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderHttpError::ClientBuild`] when the client cannot
    /// be constructed.
    pub fn new(config: ProviderHttpConfig) -> Result<Self, ProviderHttpError> {
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(config.timeout)
            .timeout(config.timeout)
            .build()
            .map_err(ProviderHttpError::ClientBuild)?;
        Ok(Self {
            config,
            client,
            pending: None,
            known_payment_intents: BTreeSet::new(),
            fixture_producer_sequence: 0,
        })
    }

    async fn drive_checkout(
        &mut self,
        request: &CaseEffectRequest<'_>,
        provider_script: ProviderOutcomeScript,
    ) -> Result<CaseEffectOutput, ProviderHttpError> {
        if self.pending.is_some() {
            return Err(ProviderHttpError::RequestAlreadyPending);
        }
        require_checkout_outputs(
            request.expected_outputs(),
            provider_script.committed_count(),
        )?;
        let baseline = self.fixture_state().await?;
        self.validate_control_sequence(&baseline)?;
        if !baseline.held_gates.is_empty() {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }
        let known = baseline
            .payment_intents
            .iter()
            .map(|payment_intent| payment_intent.id.as_str())
            .collect::<BTreeSet<_>>();

        let response = self
            .client
            .post(self.config.driver_url.clone())
            .json(&self.config.driver_body)
            .send()
            .await
            .map_err(ProviderHttpError::HttpRequest)?;
        let expected_status = if provider_script.terminal_outcome() == ProviderOutcome::Normal {
            StatusCode::OK
        } else {
            StatusCode::BAD_GATEWAY
        };
        if response.status() != expected_status {
            return Err(ProviderHttpError::UnexpectedStatus {
                step: "driver checkout",
                status: response.status(),
            });
        }
        let driver = if expected_status == StatusCode::OK {
            Some(
                response
                    .json::<DriverCheckoutResponse>()
                    .await
                    .map_err(ProviderHttpError::HttpRequest)?,
            )
        } else {
            None
        };

        let state = self.fixture_state().await?;
        self.validate_control_sequence(&state)?;
        let consumed = provider_script.outcomes().count();
        if !state.held_gates.is_empty()
            || baseline.remaining_outcomes.checked_sub(consumed) != Some(state.remaining_outcomes)
        {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }
        let new_payment_intents = state
            .payment_intents
            .iter()
            .filter(|payment_intent| !known.contains(payment_intent.id.as_str()))
            .collect::<Vec<_>>();
        if new_payment_intents.len() != usize::from(provider_script.committed_count())
            || new_payment_intents
                .iter()
                .any(|payment_intent| !self.valid_created_payment_intent(payment_intent))
        {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }
        let payment_intent_ids = new_payment_intents
            .iter()
            .map(|payment_intent| payment_intent.id.clone())
            .collect::<Vec<_>>();
        if let Some(driver) = driver
            && (payment_intent_ids.last() != Some(&driver.payment_intent_id)
                || driver.operation_id != self.config.expected_operation_id)
        {
            return Err(ProviderHttpError::ResponseMismatch);
        }
        let captured = payment_intent_captures(request.expected_outputs(), &payment_intent_ids)?;
        self.known_payment_intents
            .extend(payment_intent_ids.into_iter());
        Ok(captured)
    }

    async fn confirm_payment_intent(
        &mut self,
        request: &CaseEffectRequest<'_>,
        provider_script: ProviderOutcomeScript,
    ) -> Result<CaseEffectOutput, ProviderHttpError> {
        if self.pending.is_some() {
            return Err(ProviderHttpError::RequestAlreadyPending);
        }
        require_output_contract(request.expected_outputs(), &[])?;
        let outcome = single_provider_outcome(provider_script)?;
        let payment_intent_id = payment_intent_input(request)?;
        let baseline = self.fixture_state().await?;
        self.validate_control_sequence(&baseline)?;
        if !baseline.held_gates.is_empty() {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }
        let baseline_payment_intent = baseline
            .payment_intents
            .iter()
            .find(|payment_intent| payment_intent.id == payment_intent_id)
            .ok_or(ProviderHttpError::UnexpectedFixtureState)?;
        if !self.valid_payment_intent(baseline_payment_intent) {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }
        let baseline_status = baseline_payment_intent.status.clone();

        let response = self.send_confirmation(&payment_intent_id).await;
        let provider_response = match (outcome, response) {
            (ProviderOutcome::CommitThenClose, Err(_)) => None,
            (ProviderOutcome::CommitThenClose, Ok(_)) => {
                return Err(ProviderHttpError::ExpectedTransportClose);
            }
            (_, Err(error)) => return Err(ProviderHttpError::HttpRequest(error)),
            (ProviderOutcome::Normal, Ok(response)) if response.status() == StatusCode::OK => Some(
                response
                    .json::<ProviderPaymentIntentResponse>()
                    .await
                    .map_err(ProviderHttpError::HttpRequest)?,
            ),
            (ProviderOutcome::PreExecute429, Ok(response))
                if response.status() == StatusCode::TOO_MANY_REQUESTS =>
            {
                None
            }
            (ProviderOutcome::PreExecute500 | ProviderOutcome::PostExecute500, Ok(response))
                if response.status() == StatusCode::INTERNAL_SERVER_ERROR =>
            {
                None
            }
            (_, Ok(response)) => {
                return Err(ProviderHttpError::UnexpectedStatus {
                    step: "provider confirmation",
                    status: response.status(),
                });
            }
        };

        let state = self.fixture_state().await?;
        self.validate_control_sequence(&state)?;
        let payment_intent = state
            .payment_intents
            .iter()
            .find(|payment_intent| payment_intent.id == payment_intent_id)
            .ok_or(ProviderHttpError::UnexpectedFixtureState)?;
        let expected_status = if provider_outcome_commits(outcome) {
            "succeeded"
        } else {
            &baseline_status
        };
        if !state.held_gates.is_empty()
            || baseline.remaining_outcomes.checked_sub(1) != Some(state.remaining_outcomes)
            || state.payment_intents.len() != baseline.payment_intents.len()
            || payment_intent.status != expected_status
            || !self.valid_payment_intent(payment_intent)
        {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }
        if let Some(response) = provider_response
            && !response.matches_confirmation(
                &payment_intent_id,
                &self.config.expected_operation_id,
                self.config.expected_amount_minor,
                &self.config.expected_currency,
            )
        {
            return Err(ProviderHttpError::ResponseMismatch);
        }
        Ok(Vec::new())
    }

    async fn retrieve_payment_intent(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<CaseEffectOutput, ProviderHttpError> {
        if self.pending.is_some() {
            return Err(ProviderHttpError::RequestAlreadyPending);
        }
        require_output_contract(request.expected_outputs(), &[])?;
        let payment_intent_id = payment_intent_input(request)?;
        let baseline = self.fixture_state().await?;
        self.validate_control_sequence(&baseline)?;
        let baseline_payment_intent = baseline
            .payment_intents
            .iter()
            .find(|payment_intent| payment_intent.id == payment_intent_id)
            .ok_or(ProviderHttpError::UnexpectedFixtureState)?;
        if !baseline.held_gates.is_empty() || !self.valid_payment_intent(baseline_payment_intent) {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }

        let response = self
            .client
            .get(self.payment_intent_url(&payment_intent_id))
            .send()
            .await
            .map_err(ProviderHttpError::HttpRequest)?;
        if response.status() != StatusCode::OK {
            return Err(ProviderHttpError::UnexpectedStatus {
                step: "provider retrieval",
                status: response.status(),
            });
        }
        let provider_response = response
            .json::<ProviderPaymentIntentResponse>()
            .await
            .map_err(ProviderHttpError::HttpRequest)?;
        if !provider_response.matches_payment_intent(
            &payment_intent_id,
            &self.config.expected_operation_id,
            self.config.expected_amount_minor,
            &self.config.expected_currency,
            &baseline_payment_intent.status,
        ) {
            return Err(ProviderHttpError::ResponseMismatch);
        }

        let state = self.fixture_state().await?;
        self.validate_control_sequence(&state)?;
        let retrieved = state
            .payment_intents
            .iter()
            .find(|payment_intent| payment_intent.id == payment_intent_id)
            .ok_or(ProviderHttpError::UnexpectedFixtureState)?;
        if !state.held_gates.is_empty()
            || state.remaining_outcomes != baseline.remaining_outcomes
            || state.payment_intents.len() != baseline.payment_intents.len()
            || retrieved.status != baseline_payment_intent.status
            || !self.valid_payment_intent(retrieved)
        {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }
        Ok(Vec::new())
    }

    async fn start_held_confirmation(
        &mut self,
        request: &CaseEffectRequest<'_>,
        provider_script: ProviderOutcomeScript,
    ) -> Result<CaseEffectOutput, ProviderHttpError> {
        if self.pending.is_some() {
            return Err(ProviderHttpError::RequestAlreadyPending);
        }
        if single_provider_outcome(provider_script)? != ProviderOutcome::CommitThenDelay {
            return Err(ProviderHttpError::UnsupportedAction);
        }
        require_output_contract(
            request.expected_outputs(),
            &[CaseOutputSlot::ProviderGateId],
        )?;
        let payment_intent_id = payment_intent_input(request)?;
        let baseline = self.fixture_state().await?;
        self.validate_control_sequence(&baseline)?;
        let payment_intent = baseline
            .payment_intents
            .iter()
            .find(|payment_intent| payment_intent.id == payment_intent_id)
            .ok_or(ProviderHttpError::UnexpectedFixtureState)?;
        if !baseline.held_gates.is_empty() || !self.valid_payment_intent(payment_intent) {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }

        let client = self.client.clone();
        let endpoint = self.confirmation_url(&payment_intent_id);
        let request_timeout = self.config.timeout;
        let task = tokio::spawn(async move {
            timeout(request_timeout, async move {
                let response = client
                    .post(endpoint)
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body("")
                    .send()
                    .await
                    .map_err(ProviderHttpError::HttpRequest)?;
                if response.status() != StatusCode::OK {
                    return Err(ProviderHttpError::UnexpectedStatus {
                        step: "provider confirmation",
                        status: response.status(),
                    });
                }
                response
                    .json::<ProviderPaymentIntentResponse>()
                    .await
                    .map(PendingResponse::Confirm)
                    .map_err(ProviderHttpError::HttpRequest)
            })
            .await
            .map_err(|_| ProviderHttpError::Timeout("provider confirmation"))?
        });

        let gate_id = match self
            .wait_for_held_confirmation(
                &payment_intent_id,
                baseline.payment_intents.len(),
                baseline.remaining_outcomes,
            )
            .await
        {
            Ok(gate_id) if !task.is_finished() => gate_id,
            Ok(_) => {
                abort_task(task).await;
                return Err(ProviderHttpError::RequestCompletedBeforeRelease);
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
        if let Err(error) = request
            .record_observation(
                ObservationProducer::Fixture,
                next_sequence,
                ObservationEvent::ProviderResponseHeld { gate_id },
            )
            .await
        {
            abort_task(task).await;
            return Err(ProviderHttpError::Journal(error));
        }
        self.fixture_producer_sequence = next_sequence;
        self.pending = Some(PendingProviderRequest {
            task,
            gate_id,
            kind: PendingRequestKind::Confirm { payment_intent_id },
        });
        provider_gate_capture(request.expected_outputs(), gate_id)
    }

    async fn start_held_checkout(
        &mut self,
        request: &CaseEffectRequest<'_>,
        provider_script: ProviderOutcomeScript,
    ) -> Result<CaseEffectOutput, ProviderHttpError> {
        if self.pending.is_some() {
            return Err(ProviderHttpError::RequestAlreadyPending);
        }
        require_held_checkout_outputs(
            request.expected_outputs(),
            provider_script.committed_count(),
        )?;
        let baseline = self.fixture_state().await?;
        self.validate_control_sequence(&baseline)?;
        if !baseline.held_gates.is_empty() {
            return Err(ProviderHttpError::UnexpectedFixtureState);
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
                    .map_err(ProviderHttpError::HttpRequest)?;
                if !response.status().is_success() {
                    return Err(ProviderHttpError::UnexpectedStatus {
                        step: "driver checkout",
                        status: response.status(),
                    });
                }
                response
                    .json::<DriverCheckoutResponse>()
                    .await
                    .map(PendingResponse::Checkout)
                    .map_err(ProviderHttpError::HttpRequest)
            })
            .await
            .map_err(|_| ProviderHttpError::Timeout("driver checkout"))?
        });

        let held = match self
            .wait_for_held_response(provider_script.committed_count())
            .await
        {
            Ok(held) if !task.is_finished() => held,
            Ok(_) => {
                abort_task(task).await;
                return Err(ProviderHttpError::RequestCompletedBeforeRelease);
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
            return Err(ProviderHttpError::Journal(error));
        }
        self.fixture_producer_sequence = next_sequence;
        self.known_payment_intents
            .extend(held.payment_intent_ids.iter().cloned());
        self.pending = Some(PendingProviderRequest {
            task,
            gate_id: held.gate_id,
            kind: PendingRequestKind::Checkout {
                payment_intent_ids: held.payment_intent_ids,
            },
        });
        Ok(captured)
    }

    async fn release_held_response(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<CaseEffectOutput, ProviderHttpError> {
        require_output_contract(request.expected_outputs(), &[])?;
        let requested_gate_id = provider_gate_input(request)?;
        let pending_gate_id = self
            .pending
            .as_ref()
            .ok_or(ProviderHttpError::NoPendingRequest)?
            .gate_id;
        if requested_gate_id != pending_gate_id {
            return Err(ProviderHttpError::UnexpectedInputContract);
        }
        let pending = self
            .pending
            .take()
            .ok_or(ProviderHttpError::NoPendingRequest)?;
        let next_control_sequence = self
            .config
            .control_sequence
            .checked_add(1)
            .ok_or(ProviderHttpError::SequenceExhausted)?;
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
            || !pending.payment_intent_ids().iter().all(|expected| {
                state
                    .payment_intents
                    .iter()
                    .any(|payment_intent| &payment_intent.id == expected)
            })
        {
            abort_task(pending.task).await;
            return Err(ProviderHttpError::UnexpectedFixtureState);
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
            return Err(ProviderHttpError::Journal(error));
        }
        self.fixture_producer_sequence = next_fixture_sequence;

        let response = pending
            .task
            .await
            .map_err(ProviderHttpError::RequestJoin)??;
        match (pending.kind, response) {
            (
                PendingRequestKind::Checkout { payment_intent_ids },
                PendingResponse::Checkout(driver),
            ) if payment_intent_ids.last() == Some(&driver.payment_intent_id)
                && driver.operation_id == self.config.expected_operation_id => {}
            (
                PendingRequestKind::Confirm { payment_intent_id },
                PendingResponse::Confirm(confirmed),
            ) if confirmed.matches_confirmation(
                &payment_intent_id,
                &self.config.expected_operation_id,
                self.config.expected_amount_minor,
                &self.config.expected_currency,
            ) => {}
            _ => return Err(ProviderHttpError::ResponseMismatch),
        }
        Ok(Vec::new())
    }

    async fn wait_for_held_response(
        &self,
        expected_new_objects: u8,
    ) -> Result<HeldResponse, ProviderHttpError> {
        timeout(self.config.timeout, async {
            loop {
                let state = self.fixture_state().await?;
                self.validate_control_sequence(&state)?;
                let new_payment_intents = state
                    .payment_intents
                    .iter()
                    .filter(|payment_intent| {
                        !self.known_payment_intents.contains(&payment_intent.id)
                    })
                    .collect::<Vec<_>>();
                if new_payment_intents.len() > usize::from(expected_new_objects)
                    || new_payment_intents
                        .iter()
                        .any(|payment_intent| !self.valid_created_payment_intent(payment_intent))
                {
                    return Err(ProviderHttpError::UnexpectedFixtureState);
                }
                match state.held_gates.as_slice() {
                    [] => {}
                    [gate] => {
                        if new_payment_intents.len() != usize::from(expected_new_objects) {
                            return Err(ProviderHttpError::UnexpectedFixtureState);
                        }
                        return Ok(HeldResponse {
                            gate_id: gate.gate_id,
                            payment_intent_ids: new_payment_intents
                                .into_iter()
                                .map(|payment_intent| payment_intent.id.clone())
                                .collect(),
                        });
                    }
                    _ => return Err(ProviderHttpError::UnexpectedFixtureState),
                }
                tokio::time::sleep(self.config.poll_interval).await;
            }
        })
        .await
        .map_err(|_| ProviderHttpError::Timeout("fixture held response"))?
    }

    fn valid_created_payment_intent(&self, payment_intent: &FixturePaymentIntent) -> bool {
        payment_intent.status == "requires_confirmation"
            && self.valid_payment_intent(payment_intent)
    }

    fn valid_payment_intent(&self, payment_intent: &FixturePaymentIntent) -> bool {
        payment_intent.amount_minor == self.config.expected_amount_minor
            && payment_intent.currency == self.config.expected_currency
            && payment_intent.operation_id.as_deref()
                == Some(self.config.expected_operation_id.as_str())
    }

    async fn wait_for_held_confirmation(
        &self,
        payment_intent_id: &str,
        expected_object_count: usize,
        baseline_remaining_outcomes: usize,
    ) -> Result<u64, ProviderHttpError> {
        timeout(self.config.timeout, async {
            loop {
                let state = self.fixture_state().await?;
                self.validate_control_sequence(&state)?;
                let confirmed = state
                    .payment_intents
                    .iter()
                    .find(|payment_intent| payment_intent.id == payment_intent_id)
                    .is_some_and(|payment_intent| {
                        payment_intent.status == "succeeded"
                            && self.valid_payment_intent(payment_intent)
                    });
                match state.held_gates.as_slice() {
                    [] => {}
                    [gate]
                        if confirmed
                            && state.payment_intents.len() == expected_object_count
                            && baseline_remaining_outcomes.checked_sub(1)
                                == Some(state.remaining_outcomes) =>
                    {
                        return Ok(gate.gate_id);
                    }
                    _ => return Err(ProviderHttpError::UnexpectedFixtureState),
                }
                tokio::time::sleep(self.config.poll_interval).await;
            }
        })
        .await
        .map_err(|_| ProviderHttpError::Timeout("fixture held confirmation"))?
    }

    async fn send_confirmation(
        &self,
        payment_intent_id: &str,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client
            .post(self.confirmation_url(payment_intent_id))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body("")
            .send()
            .await
    }

    fn confirmation_url(&self, payment_intent_id: &str) -> Url {
        let mut url = self.config.fixture_data_url.clone();
        url.set_path(&format!("/v1/payment_intents/{payment_intent_id}/confirm"));
        url
    }

    fn payment_intent_url(&self, payment_intent_id: &str) -> Url {
        let mut url = self.config.fixture_data_url.clone();
        url.set_path(&format!("/v1/payment_intents/{payment_intent_id}"));
        url
    }

    async fn fixture_state(&self) -> Result<FixtureState, ProviderHttpError> {
        let response = self
            .client
            .get(
                self.config
                    .fixture_control_url
                    .join("v1/control/state")
                    .map_err(|_| ProviderHttpError::UnexpectedFixtureState)?,
            )
            .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
            .send()
            .await
            .map_err(ProviderHttpError::ControlRequest)?;
        require_status(response.status(), "fixture state")?;
        response
            .json()
            .await
            .map_err(ProviderHttpError::ControlRequest)
    }

    async fn release_gate(
        &self,
        command_sequence: u64,
        gate_id: u64,
    ) -> Result<FixtureState, ProviderHttpError> {
        let response = self
            .client
            .post(
                self.config
                    .fixture_control_url
                    .join("v1/control/release-gate")
                    .map_err(|_| ProviderHttpError::UnexpectedFixtureState)?,
            )
            .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
            .json(&serde_json::json!({
                "command_sequence": command_sequence,
                "gate_id": gate_id,
            }))
            .send()
            .await
            .map_err(ProviderHttpError::ControlRequest)?;
        require_status(response.status(), "fixture gate release")?;
        response
            .json()
            .await
            .map_err(ProviderHttpError::ControlRequest)
    }

    fn validate_control_sequence(&self, state: &FixtureState) -> Result<(), ProviderHttpError> {
        if state.command_sequence != self.config.control_sequence {
            return Err(ProviderHttpError::UnexpectedFixtureState);
        }
        Ok(())
    }

    fn next_fixture_sequence(&self) -> Result<u64, ProviderHttpError> {
        self.fixture_producer_sequence
            .checked_add(1)
            .ok_or(ProviderHttpError::SequenceExhausted)
    }
}

impl CaseEffectAdapter for ProviderHttpAdapter {
    type Error = ProviderHttpError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        Box::pin(async move {
            match request.action().kind() {
                PlanActionKind::DriveCheckout { provider_script }
                | PlanActionKind::RetryBusinessRequest { provider_script }
                    if provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay =>
                {
                    self.start_held_checkout(&request, *provider_script).await
                }
                PlanActionKind::DriveCheckout { provider_script }
                | PlanActionKind::RetryBusinessRequest { provider_script } => {
                    self.drive_checkout(&request, *provider_script).await
                }
                PlanActionKind::ConfirmPaymentIntent { provider_script }
                | PlanActionKind::RetryProviderRequest { provider_script }
                    if provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay =>
                {
                    self.start_held_confirmation(&request, *provider_script)
                        .await
                }
                PlanActionKind::ConfirmPaymentIntent { provider_script }
                | PlanActionKind::RetryProviderRequest { provider_script } => {
                    self.confirm_payment_intent(&request, *provider_script)
                        .await
                }
                PlanActionKind::RetrievePaymentIntent => {
                    self.retrieve_payment_intent(&request).await
                }
                PlanActionKind::ReleaseProviderGate => self.release_held_response(&request).await,
                _ => Err(ProviderHttpError::UnsupportedAction),
            }
        })
    }
}

impl Drop for ProviderHttpAdapter {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.task.abort();
        }
    }
}

fn require_output_contract(
    outputs: &[CaseOutputRef],
    expected: &[CaseOutputSlot],
) -> Result<(), ProviderHttpError> {
    let actual = outputs
        .iter()
        .map(|output| output.slot())
        .collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    if actual != expected || outputs.len() != expected.len() {
        return Err(ProviderHttpError::UnexpectedOutputContract);
    }
    Ok(())
}

fn require_held_checkout_outputs(
    outputs: &[CaseOutputRef],
    committed_count: u8,
) -> Result<(), ProviderHttpError> {
    let payment_intents = outputs
        .iter()
        .copied()
        .filter(|output| output.slot() == CaseOutputSlot::PaymentIntentId)
        .collect::<Vec<_>>();
    let gates = outputs
        .iter()
        .copied()
        .filter(|output| output.slot() == CaseOutputSlot::ProviderGateId)
        .collect::<Vec<_>>();
    if payment_intents.len() != usize::from(committed_count)
        || payment_intents
            .iter()
            .enumerate()
            .any(|(index, output)| output.occurrence() != u8::try_from(index).unwrap_or(u8::MAX))
        || gates.len() != 1
        || gates[0].occurrence() != 0
        || outputs.len() != payment_intents.len() + 1
    {
        return Err(ProviderHttpError::UnexpectedOutputContract);
    }
    Ok(())
}

fn require_checkout_outputs(
    outputs: &[CaseOutputRef],
    committed_count: u8,
) -> Result<(), ProviderHttpError> {
    if outputs.len() != usize::from(committed_count)
        || outputs.iter().enumerate().any(|(index, output)| {
            output.slot() != CaseOutputSlot::PaymentIntentId
                || output.occurrence() != u8::try_from(index).unwrap_or(u8::MAX)
        })
    {
        return Err(ProviderHttpError::UnexpectedOutputContract);
    }
    Ok(())
}

fn payment_intent_captures(
    outputs: &[CaseOutputRef],
    payment_intent_ids: &[String],
) -> Result<CaseEffectOutput, ProviderHttpError> {
    if outputs.len() != payment_intent_ids.len() {
        return Err(ProviderHttpError::UnexpectedOutputContract);
    }
    outputs
        .iter()
        .copied()
        .zip(payment_intent_ids)
        .map(|(output, payment_intent_id)| {
            CaseCapturedValue::payment_intent_id(payment_intent_id.clone())
                .map(|value| (output, value))
                .map_err(|_| ProviderHttpError::UnexpectedFixtureState)
        })
        .collect()
}

fn provider_gate_capture(
    outputs: &[CaseOutputRef],
    gate_id: u64,
) -> Result<CaseEffectOutput, ProviderHttpError> {
    require_output_contract(outputs, &[CaseOutputSlot::ProviderGateId])?;
    let value = CaseCapturedValue::provider_gate_id(gate_id)
        .map_err(|_| ProviderHttpError::UnexpectedFixtureState)?;
    Ok(vec![(outputs[0], value)])
}

fn payment_intent_input(request: &CaseEffectRequest<'_>) -> Result<String, ProviderHttpError> {
    match request.input(tiv_core::trace::CaseInputSlot::PaymentIntentId) {
        Some(CaseCapturedValue::PaymentIntentId(payment_intent_id)) => {
            Ok(payment_intent_id.as_str().to_owned())
        }
        _ => Err(ProviderHttpError::UnexpectedInputContract),
    }
}

fn provider_gate_input(request: &CaseEffectRequest<'_>) -> Result<u64, ProviderHttpError> {
    match request.input(tiv_core::trace::CaseInputSlot::ProviderGateId) {
        Some(CaseCapturedValue::ProviderGateId(gate_id)) => Ok(gate_id.value()),
        _ => Err(ProviderHttpError::UnexpectedInputContract),
    }
}

fn single_provider_outcome(
    provider_script: ProviderOutcomeScript,
) -> Result<ProviderOutcome, ProviderHttpError> {
    let mut outcomes = provider_script.outcomes();
    let outcome = outcomes
        .next()
        .ok_or(ProviderHttpError::UnsupportedAction)?;
    if outcomes.next().is_some() {
        return Err(ProviderHttpError::UnsupportedAction);
    }
    Ok(outcome)
}

const fn provider_outcome_commits(outcome: ProviderOutcome) -> bool {
    !matches!(
        outcome,
        ProviderOutcome::PreExecute429 | ProviderOutcome::PreExecute500
    )
}

fn held_captures(
    outputs: &[CaseOutputRef],
    held: &HeldResponse,
) -> Result<CaseEffectOutput, ProviderHttpError> {
    let mut payment_intent_ids = held.payment_intent_ids.iter();
    outputs
        .iter()
        .copied()
        .map(|output| {
            let value = match output.slot() {
                CaseOutputSlot::PaymentIntentId => {
                    let payment_intent_id = payment_intent_ids
                        .next()
                        .ok_or(ProviderHttpError::UnexpectedOutputContract)?;
                    CaseCapturedValue::payment_intent_id(payment_intent_id.clone())
                        .map_err(|_| ProviderHttpError::UnexpectedFixtureState)?
                }
                CaseOutputSlot::ProviderGateId => CaseCapturedValue::provider_gate_id(held.gate_id)
                    .map_err(|_| ProviderHttpError::UnexpectedFixtureState)?,
                CaseOutputSlot::EventId => {
                    return Err(ProviderHttpError::UnexpectedOutputContract);
                }
            };
            Ok((output, value))
        })
        .collect()
}

fn require_status(status: StatusCode, step: &'static str) -> Result<(), ProviderHttpError> {
    if status.is_success() {
        Ok(())
    } else {
        Err(ProviderHttpError::UnexpectedStatus { step, status })
    }
}

async fn abort_task(task: JoinHandle<Result<PendingResponse, ProviderHttpError>>) {
    task.abort();
    let _ = task.await;
}

struct PendingProviderRequest {
    task: JoinHandle<Result<PendingResponse, ProviderHttpError>>,
    gate_id: u64,
    kind: PendingRequestKind,
}

impl PendingProviderRequest {
    fn payment_intent_ids(&self) -> &[String] {
        match &self.kind {
            PendingRequestKind::Checkout { payment_intent_ids } => payment_intent_ids,
            PendingRequestKind::Confirm { payment_intent_id } => {
                std::slice::from_ref(payment_intent_id)
            }
        }
    }
}

enum PendingRequestKind {
    Checkout { payment_intent_ids: Vec<String> },
    Confirm { payment_intent_id: String },
}

enum PendingResponse {
    Checkout(DriverCheckoutResponse),
    Confirm(ProviderPaymentIntentResponse),
}

struct HeldResponse {
    gate_id: u64,
    payment_intent_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DriverCheckoutResponse {
    payment_intent_id: String,
    operation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderPaymentIntentResponse {
    id: String,
    object: String,
    amount: i64,
    currency: String,
    status: String,
    metadata: ProviderPaymentIntentMetadata,
}

impl ProviderPaymentIntentResponse {
    fn matches_payment_intent(
        &self,
        payment_intent_id: &str,
        operation_id: &str,
        amount_minor: i64,
        currency: &str,
        status: &str,
    ) -> bool {
        self.id == payment_intent_id
            && self.object == "payment_intent"
            && self.amount == amount_minor
            && self.currency == currency
            && self.status == status
            && self.metadata.operation_id == operation_id
    }

    fn matches_confirmation(
        &self,
        payment_intent_id: &str,
        operation_id: &str,
        amount_minor: i64,
        currency: &str,
    ) -> bool {
        self.matches_payment_intent(
            payment_intent_id,
            operation_id,
            amount_minor,
            currency,
            "succeeded",
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderPaymentIntentMetadata {
    operation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureState {
    command_sequence: u64,
    remaining_outcomes: usize,
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
pub enum ProviderHttpError {
    #[error("could not build the provider HTTP client: {0}")]
    ClientBuild(reqwest::Error),
    #[error("provider HTTP request failed: {0}")]
    HttpRequest(reqwest::Error),
    #[error("fixture control request failed: {0}")]
    ControlRequest(reqwest::Error),
    #[error("provider HTTP step {step} returned HTTP {status}")]
    UnexpectedStatus {
        step: &'static str,
        status: StatusCode,
    },
    #[error("provider HTTP step timed out: {0}")]
    Timeout(&'static str),
    #[error("fixture state did not match the exact provider action contract")]
    UnexpectedFixtureState,
    #[error("provider action declared an unexpected output contract")]
    UnexpectedOutputContract,
    #[error("provider action did not receive its exact captured input")]
    UnexpectedInputContract,
    #[error("a provider HTTP request is already pending")]
    RequestAlreadyPending,
    #[error("no provider HTTP request is pending")]
    NoPendingRequest,
    #[error("provider HTTP request completed before fixture release")]
    RequestCompletedBeforeRelease,
    #[error("provider HTTP response did not match the captured provider object")]
    ResponseMismatch,
    #[error("provider returned a response where the plan required a transport close")]
    ExpectedTransportClose,
    #[error("provider HTTP sequence exhausted")]
    SequenceExhausted,
    #[error("provider HTTP request task failed: {0}")]
    RequestJoin(tokio::task::JoinError),
    #[error("provider HTTP observation journal failed: {0}")]
    Journal(#[from] JournalError),
    #[error("action is outside the provider HTTP adapter boundary")]
    UnsupportedAction,
}
