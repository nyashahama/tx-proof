//! Fixture-side webhook generation and deterministic delivery-queue execution.

use std::{
    collections::{BTreeSet, VecDeque},
    time::{Duration, Instant},
};

use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use thiserror::Error;
use tiv_core::{
    plan::PlanActionKind,
    trace::{CaseCapturedValue, CaseInputSlot, CaseOutputRef, CaseOutputSlot},
};

use crate::campaign::{CaseEffectAdapter, CaseEffectFuture, CaseEffectOutput, CaseEffectRequest};

const MAX_CONTROL_TOKEN_BYTES: usize = 1_024;
const MAX_WEBHOOK_DELAY: Duration = Duration::from_secs(5);
const MAX_WEBHOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Secret-bearing configuration for one loopback fixture-control session.
///
/// This type intentionally implements neither `Debug` nor `Serialize`.
pub struct WebhookHttpConfig {
    fixture_control_url: Url,
    fixture_control_token: String,
    control_sequence: u64,
    next_timestamp: i64,
    timeout: Duration,
}

impl WebhookHttpConfig {
    /// Validates the fixture-control and delivery-attempt bounds.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookHttpConfigError`] unless the control target is an
    /// explicit loopback HTTP origin, the token is header-safe, and sequence,
    /// timestamp, and timeout values leave room for at least one action.
    pub fn new(
        fixture_control_url: impl AsRef<str>,
        fixture_control_token: impl Into<String>,
        control_sequence: u64,
        first_timestamp: i64,
        timeout: Duration,
    ) -> Result<Self, WebhookHttpConfigError> {
        let fixture_control_url = normalize_control_url(fixture_control_url.as_ref())?;
        let fixture_control_token = fixture_control_token.into();
        if fixture_control_token.trim().is_empty()
            || fixture_control_token.len() > MAX_CONTROL_TOKEN_BYTES
            || reqwest::header::HeaderValue::from_str(&fixture_control_token).is_err()
        {
            return Err(WebhookHttpConfigError::InvalidControlToken);
        }
        if control_sequence == 0
            || control_sequence == u64::MAX
            || first_timestamp <= 0
            || first_timestamp == i64::MAX
            || timeout.is_zero()
            || timeout > MAX_WEBHOOK_TIMEOUT
        {
            return Err(WebhookHttpConfigError::InvalidBounds);
        }
        Ok(Self {
            fixture_control_url,
            fixture_control_token,
            control_sequence,
            next_timestamp: first_timestamp,
            timeout,
        })
    }
}

fn normalize_control_url(value: &str) -> Result<Url, WebhookHttpConfigError> {
    if value.chars().any(char::is_whitespace) {
        return Err(WebhookHttpConfigError::NonLoopbackUrl);
    }
    let url = Url::parse(value).map_err(|_| WebhookHttpConfigError::NonLoopbackUrl)?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "" | "/")
    {
        return Err(WebhookHttpConfigError::NonLoopbackUrl);
    }
    Ok(url)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum WebhookHttpConfigError {
    #[error("fixture control URL must be an explicit loopback HTTP origin")]
    NonLoopbackUrl,
    #[error("fixture control token is outside the supported header contract")]
    InvalidControlToken,
    #[error("webhook sequence, timestamp, and timeout bounds are invalid")]
    InvalidBounds,
}

/// Executes one case's event generation and deterministic webhook queue.
pub struct WebhookHttpAdapter {
    config: WebhookHttpConfig,
    client: Client,
    pending: VecDeque<PendingWebhook>,
    generated_events: BTreeSet<String>,
    last_delivered: Option<String>,
}

impl WebhookHttpAdapter {
    /// Creates a redirect-free client for the validated fixture control target.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookHttpError::ClientBuild`] when the client cannot be
    /// constructed.
    pub fn new(config: WebhookHttpConfig) -> Result<Self, WebhookHttpError> {
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(config.timeout)
            .timeout(config.timeout)
            .build()
            .map_err(WebhookHttpError::ClientBuild)?;
        Ok(Self {
            config,
            client,
            pending: VecDeque::new(),
            generated_events: BTreeSet::new(),
            last_delivered: None,
        })
    }

    pub(crate) const fn control_sequence(&self) -> u64 {
        self.config.control_sequence
    }

    pub(crate) fn synchronize_control_sequence(
        &mut self,
        observed: u64,
    ) -> Result<(), WebhookHttpError> {
        if observed < self.config.control_sequence || observed == u64::MAX {
            return Err(WebhookHttpError::UnexpectedControlResponse);
        }
        self.config.control_sequence = observed;
        Ok(())
    }

    async fn generate_event(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<CaseEffectOutput, WebhookHttpError> {
        let output = require_event_output(request.expected_outputs())?;
        let payment_intent_id = payment_intent_input(request)?;
        let next_sequence = self.next_control_sequence()?;
        let response = self
            .client
            .post(self.control_endpoint("/v1/control/generate-event"))
            .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
            .json(&serde_json::json!({
                "command_sequence": next_sequence,
                "payment_intent_id": payment_intent_id,
            }))
            .send()
            .await
            .map_err(WebhookHttpError::ControlRequest)?;
        require_status(response.status(), "generate event")?;
        let generated = response
            .json::<GeneratedEventResponse>()
            .await
            .map_err(WebhookHttpError::ControlRequest)?;
        if generated.command_sequence != next_sequence
            || generated.payment_intent_id != payment_intent_id
            || self.generated_events.contains(&generated.event_id)
        {
            return Err(WebhookHttpError::UnexpectedControlResponse);
        }
        let captured = CaseCapturedValue::event_id(generated.event_id.clone())
            .map_err(|_| WebhookHttpError::UnexpectedControlResponse)?;
        self.config.control_sequence = next_sequence;
        self.generated_events.insert(generated.event_id.clone());
        self.pending.push_back(PendingWebhook {
            event_id: generated.event_id,
            not_before: None,
        });
        Ok(vec![(output, captured)])
    }

    async fn deliver_pending(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<CaseEffectOutput, WebhookHttpError> {
        require_no_outputs(request.expected_outputs())?;
        let pending = self
            .pending
            .front()
            .cloned()
            .ok_or(WebhookHttpError::UnexpectedQueueState)?;
        if let Some(not_before) = pending.not_before {
            tokio::time::sleep_until(not_before.into()).await;
        }
        self.deliver_event(&pending.event_id).await?;
        let removed = self
            .pending
            .pop_front()
            .ok_or(WebhookHttpError::UnexpectedQueueState)?;
        if removed.event_id != pending.event_id {
            return Err(WebhookHttpError::UnexpectedQueueState);
        }
        self.last_delivered = Some(pending.event_id);
        Ok(Vec::new())
    }

    async fn duplicate_last(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<CaseEffectOutput, WebhookHttpError> {
        require_no_outputs(request.expected_outputs())?;
        let event_id = self
            .last_delivered
            .clone()
            .ok_or(WebhookHttpError::UnexpectedQueueState)?;
        self.deliver_event(&event_id).await?;
        Ok(Vec::new())
    }

    fn delay_next(
        &mut self,
        request: &CaseEffectRequest<'_>,
        milliseconds: u64,
    ) -> Result<CaseEffectOutput, WebhookHttpError> {
        require_no_outputs(request.expected_outputs())?;
        let delay = Duration::from_millis(milliseconds);
        if delay > MAX_WEBHOOK_DELAY {
            return Err(WebhookHttpError::UnexpectedQueueState);
        }
        let pending = self
            .pending
            .front_mut()
            .ok_or(WebhookHttpError::UnexpectedQueueState)?;
        if pending.not_before.is_some() {
            return Err(WebhookHttpError::UnexpectedQueueState);
        }
        pending.not_before = Some(
            Instant::now()
                .checked_add(delay)
                .ok_or(WebhookHttpError::SequenceExhausted)?,
        );
        Ok(Vec::new())
    }

    fn reorder_next_two(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<CaseEffectOutput, WebhookHttpError> {
        require_no_outputs(request.expected_outputs())?;
        if self.pending.len() < 2 {
            return Err(WebhookHttpError::UnexpectedQueueState);
        }
        self.pending.swap(0, 1);
        Ok(Vec::new())
    }

    fn drop_next(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<CaseEffectOutput, WebhookHttpError> {
        require_no_outputs(request.expected_outputs())?;
        self.pending
            .pop_front()
            .ok_or(WebhookHttpError::UnexpectedQueueState)?;
        Ok(Vec::new())
    }

    async fn deliver_event(&mut self, event_id: &str) -> Result<(), WebhookHttpError> {
        let next_sequence = self.next_control_sequence()?;
        let timestamp = self.config.next_timestamp;
        let next_timestamp = timestamp
            .checked_add(1)
            .ok_or(WebhookHttpError::SequenceExhausted)?;
        let response = self
            .client
            .post(self.control_endpoint("/v1/control/deliver-event"))
            .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
            .json(&serde_json::json!({
                "command_sequence": next_sequence,
                "event_id": event_id,
                "timestamp": timestamp,
            }))
            .send()
            .await
            .map_err(WebhookHttpError::ControlRequest)?;
        require_status(response.status(), "deliver event")?;
        let delivered = response
            .json::<DeliveredEventResponse>()
            .await
            .map_err(WebhookHttpError::ControlRequest)?;
        if delivered.command_sequence != next_sequence
            || delivered.event_id != event_id
            || delivered.timestamp != timestamp
        {
            return Err(WebhookHttpError::UnexpectedControlResponse);
        }
        self.config.control_sequence = next_sequence;
        self.config.next_timestamp = next_timestamp;
        if !(200..300).contains(&delivered.status) {
            return Err(WebhookHttpError::UnexpectedDeliveryStatus(delivered.status));
        }
        Ok(())
    }

    fn next_control_sequence(&self) -> Result<u64, WebhookHttpError> {
        self.config
            .control_sequence
            .checked_add(1)
            .ok_or(WebhookHttpError::SequenceExhausted)
    }

    fn control_endpoint(&self, path: &str) -> Url {
        let mut url = self.config.fixture_control_url.clone();
        url.set_path(path);
        url
    }
}

impl CaseEffectAdapter for WebhookHttpAdapter {
    type Error = WebhookHttpError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        Box::pin(async move {
            match request.action().kind() {
                PlanActionKind::GenerateProviderEvent => self.generate_event(&request).await,
                PlanActionKind::DeliverWebhook => self.deliver_pending(&request).await,
                PlanActionKind::DuplicateWebhook => self.duplicate_last(&request).await,
                PlanActionKind::DelayWebhook { milliseconds } => {
                    self.delay_next(&request, *milliseconds)
                }
                PlanActionKind::ReorderWebhooks => self.reorder_next_two(&request),
                PlanActionKind::DropWebhook => self.drop_next(&request),
                _ => Err(WebhookHttpError::UnsupportedAction),
            }
        })
    }
}

fn require_event_output(outputs: &[CaseOutputRef]) -> Result<CaseOutputRef, WebhookHttpError> {
    match outputs {
        [output] if output.slot() == CaseOutputSlot::EventId && output.occurrence() == 0 => {
            Ok(*output)
        }
        _ => Err(WebhookHttpError::UnexpectedOutputContract),
    }
}

fn require_no_outputs(outputs: &[CaseOutputRef]) -> Result<(), WebhookHttpError> {
    if outputs.is_empty() {
        Ok(())
    } else {
        Err(WebhookHttpError::UnexpectedOutputContract)
    }
}

fn payment_intent_input(request: &CaseEffectRequest<'_>) -> Result<String, WebhookHttpError> {
    match request.input(CaseInputSlot::PaymentIntentId) {
        Some(CaseCapturedValue::PaymentIntentId(payment_intent_id)) => {
            Ok(payment_intent_id.as_str().to_owned())
        }
        _ => Err(WebhookHttpError::UnexpectedInputContract),
    }
}

fn require_status(status: StatusCode, step: &'static str) -> Result<(), WebhookHttpError> {
    if status.is_success() {
        Ok(())
    } else {
        Err(WebhookHttpError::UnexpectedControlStatus { step, status })
    }
}

#[derive(Clone)]
struct PendingWebhook {
    event_id: String,
    not_before: Option<Instant>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedEventResponse {
    command_sequence: u64,
    event_id: String,
    payment_intent_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveredEventResponse {
    command_sequence: u64,
    event_id: String,
    timestamp: i64,
    status: u16,
}

#[derive(Debug, Error)]
pub enum WebhookHttpError {
    #[error("could not build the webhook control HTTP client: {0}")]
    ClientBuild(reqwest::Error),
    #[error("fixture webhook control request failed: {0}")]
    ControlRequest(reqwest::Error),
    #[error("fixture webhook control step {step} returned HTTP {status}")]
    UnexpectedControlStatus {
        step: &'static str,
        status: StatusCode,
    },
    #[error("fixture webhook control response did not match the planned action")]
    UnexpectedControlResponse,
    #[error("webhook target returned HTTP {0}")]
    UnexpectedDeliveryStatus(u16),
    #[error("webhook action declared an unexpected output contract")]
    UnexpectedOutputContract,
    #[error("webhook action did not receive its exact captured input")]
    UnexpectedInputContract,
    #[error("webhook action did not match the deterministic event queue")]
    UnexpectedQueueState,
    #[error("webhook sequence or timestamp exhausted")]
    SequenceExhausted,
    #[error("action is outside the webhook HTTP adapter boundary")]
    UnsupportedAction,
}
