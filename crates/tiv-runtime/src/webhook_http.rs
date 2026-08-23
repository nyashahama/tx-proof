//! Fixture-side webhook generation and deterministic delivery-queue execution.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::{Duration, Instant},
};

use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use thiserror::Error;
use tiv_core::{
    plan::PlanActionKind,
    trace::{ActionId, CaseCapturedValue, CaseInputSlot, CaseOutputRef, CaseOutputSlot},
};
use tokio::task::JoinHandle;

use crate::{
    campaign::{CaseEffectAdapter, CaseEffectFuture, CaseEffectOutput, CaseEffectRequest},
    journal::{JournalError, ObservationEvent, ObservationProducer},
};

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
    generated_events: BTreeMap<String, String>,
    last_delivered: Option<String>,
    request_cut_points: BTreeSet<ActionId>,
    response_cut_points: BTreeSet<ActionId>,
    pending_request: Option<PendingWebhookRequest>,
    pending_response: Option<PendingWebhookResponse>,
    fixture_producer_sequence: u64,
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
            generated_events: BTreeMap::new(),
            last_delivered: None,
            request_cut_points: BTreeSet::new(),
            response_cut_points: BTreeSet::new(),
            pending_request: None,
            pending_response: None,
            fixture_producer_sequence: 0,
        })
    }

    /// Marks delivery actions that must pause after the fixture observes the
    /// application's webhook response.
    #[must_use]
    pub fn with_webhook_response_cut_points<I>(mut self, action_ids: I) -> Self
    where
        I: IntoIterator<Item = ActionId>,
    {
        self.response_cut_points = action_ids.into_iter().collect();
        self
    }

    /// Marks delivery actions that must pause after the instrumented
    /// application validates ingress and before it persists the webhook.
    #[must_use]
    pub fn with_webhook_request_cut_points<I>(mut self, action_ids: I) -> Self
    where
        I: IntoIterator<Item = ActionId>,
    {
        self.request_cut_points = action_ids.into_iter().collect();
        self
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

    pub(crate) fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.pending_request.is_none() && self.pending_response.is_none()
    }

    pub(crate) const fn fixture_producer_sequence(&self) -> u64 {
        self.fixture_producer_sequence
    }

    pub(crate) fn synchronize_fixture_producer_sequence(
        &mut self,
        observed: u64,
    ) -> Result<(), WebhookHttpError> {
        if observed < self.fixture_producer_sequence {
            return Err(WebhookHttpError::UnexpectedControlResponse);
        }
        self.fixture_producer_sequence = observed;
        Ok(())
    }

    pub(crate) fn require_observed_response(&self) -> Result<(), WebhookHttpError> {
        self.pending_response
            .as_ref()
            .map(|_| ())
            .ok_or(WebhookHttpError::NoPendingWebhookResponse)
    }

    pub(crate) fn require_forwarded_request(&self) -> Result<(), WebhookHttpError> {
        self.pending_request
            .as_ref()
            .map(|_| ())
            .ok_or(WebhookHttpError::NoPendingWebhookRequest)
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
            || matches!(
                self.generated_events.get(&generated.event_id),
                Some(existing_payment_intent_id)
                    if existing_payment_intent_id != &generated.payment_intent_id
            )
        {
            return Err(WebhookHttpError::UnexpectedControlResponse);
        }
        let captured = CaseCapturedValue::event_id(generated.event_id.clone())
            .map_err(|_| WebhookHttpError::UnexpectedControlResponse)?;
        self.config.control_sequence = next_sequence;
        self.generated_events
            .entry(generated.event_id.clone())
            .or_insert_with(|| generated.payment_intent_id.clone());
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
        self.deliver_event(request, &pending.event_id).await?;
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
        self.deliver_event(request, &event_id).await?;
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

    async fn deliver_event(
        &mut self,
        request: &CaseEffectRequest<'_>,
        event_id: &str,
    ) -> Result<(), WebhookHttpError> {
        let next_sequence = self.next_control_sequence()?;
        let timestamp = self.config.next_timestamp;
        let next_timestamp = timestamp
            .checked_add(1)
            .ok_or(WebhookHttpError::SequenceExhausted)?;
        let request_held = self.request_cut_points.contains(&request.action().id());
        let response_held = self.response_cut_points.contains(&request.action().id());
        if request_held && response_held {
            return Err(WebhookHttpError::ConflictingWebhookCutPoints);
        }
        let progress = if request_held {
            self.start_forwarded_request_delivery(request, event_id, next_sequence, timestamp)
                .await?;
            DeliveryProgress::RequestForwarded
        } else if response_held {
            DeliveryProgress::Complete(
                self.start_held_delivery(request, event_id, next_sequence, timestamp)
                    .await?,
            )
        } else {
            DeliveryProgress::Complete(
                send_delivery_request(
                    &self.client,
                    self.control_endpoint("/v1/control/deliver-event"),
                    &self.config.fixture_control_token,
                    next_sequence,
                    event_id,
                    timestamp,
                )
                .await?,
            )
        };
        self.config.control_sequence = next_sequence;
        self.config.next_timestamp = next_timestamp;
        if let DeliveryProgress::Complete(delivered) = progress
            && !(200..300).contains(&delivered.status)
        {
            return Err(WebhookHttpError::UnexpectedDeliveryStatus(delivered.status));
        }
        Ok(())
    }

    async fn start_forwarded_request_delivery(
        &mut self,
        request: &CaseEffectRequest<'_>,
        event_id: &str,
        command_sequence: u64,
        timestamp: i64,
    ) -> Result<(), WebhookHttpError> {
        if self.pending_request.is_some() || self.pending_response.is_some() {
            return Err(WebhookHttpError::WebhookRequestAlreadyPending);
        }
        let client = self.client.clone();
        let endpoint = self.control_endpoint("/v1/control/deliver-event-held-request");
        let token = self.config.fixture_control_token.clone();
        let task_event_id = event_id.to_owned();
        let task = tokio::spawn(async move {
            send_delivery_request(
                &client,
                endpoint,
                &token,
                command_sequence,
                &task_event_id,
                timestamp,
            )
            .await
        });
        let gate = match self
            .wait_for_forwarded_webhook_request(event_id, command_sequence, &task)
            .await
        {
            Ok(gate) if !task.is_finished() => gate,
            Ok(_) => {
                task.abort();
                return Err(WebhookHttpError::RequestCompletedBeforeWebhookGate);
            }
            Err(error) => {
                task.abort();
                return Err(error);
            }
        };
        let next_observation = self
            .fixture_producer_sequence
            .checked_add(1)
            .ok_or(WebhookHttpError::SequenceExhausted)?;
        if let Err(error) = request
            .record_observation(
                ObservationProducer::Fixture,
                next_observation,
                ObservationEvent::WebhookRequestForwarded {
                    gate_id: gate.gate_id,
                },
            )
            .await
        {
            task.abort();
            return Err(WebhookHttpError::Journal(error));
        }
        self.fixture_producer_sequence = next_observation;
        self.pending_request = Some(PendingWebhookRequest {
            task: Some(task),
            gate_id: gate.gate_id,
        });
        Ok(())
    }

    async fn start_held_delivery(
        &mut self,
        request: &CaseEffectRequest<'_>,
        event_id: &str,
        command_sequence: u64,
        timestamp: i64,
    ) -> Result<DeliveredEventResponse, WebhookHttpError> {
        if self.pending_response.is_some() {
            return Err(WebhookHttpError::WebhookResponseAlreadyPending);
        }
        let client = self.client.clone();
        let endpoint = self.control_endpoint("/v1/control/deliver-event-held-response");
        let token = self.config.fixture_control_token.clone();
        let task_event_id = event_id.to_owned();
        let task = tokio::spawn(async move {
            send_delivery_request(
                &client,
                endpoint,
                &token,
                command_sequence,
                &task_event_id,
                timestamp,
            )
            .await
        });
        let gate = match self
            .wait_for_held_webhook_response(event_id, command_sequence, &task)
            .await
        {
            Ok(gate) if !task.is_finished() => gate,
            Ok(_) => {
                task.abort();
                return Err(WebhookHttpError::RequestCompletedBeforeWebhookGate);
            }
            Err(error) => {
                task.abort();
                return Err(error);
            }
        };
        let next_observation = self
            .fixture_producer_sequence
            .checked_add(1)
            .ok_or(WebhookHttpError::SequenceExhausted)?;
        if let Err(error) = request
            .record_observation(
                ObservationProducer::Fixture,
                next_observation,
                ObservationEvent::WebhookResponseObserved {
                    gate_id: gate.gate_id,
                },
            )
            .await
        {
            task.abort();
            return Err(WebhookHttpError::Journal(error));
        }
        self.fixture_producer_sequence = next_observation;
        self.pending_response = Some(PendingWebhookResponse {
            task: Some(task),
            gate_id: gate.gate_id,
        });
        Ok(DeliveredEventResponse {
            command_sequence,
            event_id: event_id.to_owned(),
            timestamp,
            status: gate.status,
        })
    }

    async fn wait_for_held_webhook_response(
        &self,
        event_id: &str,
        command_sequence: u64,
        task: &JoinHandle<Result<DeliveredEventResponse, WebhookHttpError>>,
    ) -> Result<HeldWebhookResponse, WebhookHttpError> {
        tokio::time::timeout(self.config.timeout, async {
            loop {
                if task.is_finished() {
                    return Err(WebhookHttpError::RequestCompletedBeforeWebhookGate);
                }
                let response = self
                    .client
                    .get(self.control_endpoint("/v1/control/webhook-response-state"))
                    .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
                    .send()
                    .await
                    .map_err(WebhookHttpError::ControlRequest)?;
                require_status(response.status(), "webhook response state")?;
                let state = response
                    .json::<WebhookResponseState>()
                    .await
                    .map_err(WebhookHttpError::ControlRequest)?;
                if state.command_sequence < self.config.control_sequence
                    || state.command_sequence > command_sequence
                {
                    return Err(WebhookHttpError::UnexpectedControlResponse);
                }
                match state.held_webhook_responses.as_slice() {
                    [] => tokio::time::sleep(Duration::from_millis(10)).await,
                    [gate]
                        if state.command_sequence == command_sequence
                            && gate.event_id == event_id
                            && (200..300).contains(&gate.status) =>
                    {
                        return Ok(gate.clone());
                    }
                    _ => return Err(WebhookHttpError::UnexpectedControlResponse),
                }
            }
        })
        .await
        .map_err(|_| WebhookHttpError::Timeout("webhook response gate"))?
    }

    async fn wait_for_forwarded_webhook_request(
        &self,
        event_id: &str,
        command_sequence: u64,
        task: &JoinHandle<Result<DeliveredEventResponse, WebhookHttpError>>,
    ) -> Result<HeldWebhookRequest, WebhookHttpError> {
        tokio::time::timeout(self.config.timeout, async {
            loop {
                if task.is_finished() {
                    return Err(WebhookHttpError::RequestCompletedBeforeWebhookGate);
                }
                let response = self
                    .client
                    .get(self.control_endpoint("/v1/control/webhook-request-state"))
                    .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
                    .send()
                    .await
                    .map_err(WebhookHttpError::ControlRequest)?;
                require_status(response.status(), "webhook request state")?;
                let state = response
                    .json::<WebhookRequestState>()
                    .await
                    .map_err(WebhookHttpError::ControlRequest)?;
                if state.command_sequence < self.config.control_sequence
                    || state.command_sequence > command_sequence
                {
                    return Err(WebhookHttpError::UnexpectedControlResponse);
                }
                match state.held_webhook_requests.as_slice() {
                    [] => tokio::time::sleep(Duration::from_millis(10)).await,
                    [gate]
                        if state.command_sequence == command_sequence
                            && gate.event_id == event_id
                            && gate.forwarded =>
                    {
                        return Ok(gate.clone());
                    }
                    [gate]
                        if state.command_sequence == command_sequence
                            && gate.event_id == event_id
                            && !gate.forwarded =>
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    _ => return Err(WebhookHttpError::UnexpectedControlResponse),
                }
            }
        })
        .await
        .map_err(|_| WebhookHttpError::Timeout("webhook request gate"))?
    }

    pub(crate) async fn discard_forwarded_request(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<(), WebhookHttpError> {
        let mut pending = self
            .pending_request
            .take()
            .ok_or(WebhookHttpError::NoPendingWebhookRequest)?;
        let next_sequence = self.next_control_sequence()?;
        let response = self
            .client
            .post(self.control_endpoint("/v1/control/discard-webhook-request"))
            .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
            .json(&serde_json::json!({
                "command_sequence": next_sequence,
                "gate_id": pending.gate_id,
            }))
            .send()
            .await
            .map_err(WebhookHttpError::ControlRequest)?;
        require_status(response.status(), "discard webhook request")?;
        let state = response
            .json::<WebhookRequestState>()
            .await
            .map_err(WebhookHttpError::ControlRequest)?;
        if state.command_sequence != next_sequence || !state.held_webhook_requests.is_empty() {
            return Err(WebhookHttpError::UnexpectedControlResponse);
        }
        self.config.control_sequence = next_sequence;
        let task = pending
            .task
            .take()
            .expect("a pending webhook request always owns its sender task");
        let task_result = tokio::time::timeout(self.config.timeout, task)
            .await
            .map_err(|_| WebhookHttpError::Timeout("discarded webhook request"))?
            .map_err(WebhookHttpError::RequestJoin)?;
        if !matches!(
            task_result,
            Err(WebhookHttpError::UnexpectedControlStatus {
                step: "deliver event",
                status: StatusCode::BAD_GATEWAY,
            })
        ) {
            return Err(WebhookHttpError::ExpectedDiscardedWebhookConnection);
        }
        let next_observation = self
            .fixture_producer_sequence
            .checked_add(1)
            .ok_or(WebhookHttpError::SequenceExhausted)?;
        request
            .record_observation(
                ObservationProducer::Fixture,
                next_observation,
                ObservationEvent::WebhookRequestDiscarded {
                    gate_id: pending.gate_id,
                },
            )
            .await?;
        self.fixture_producer_sequence = next_observation;
        Ok(())
    }

    pub(crate) async fn discard_observed_response(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<(), WebhookHttpError> {
        let mut pending = self
            .pending_response
            .take()
            .ok_or(WebhookHttpError::NoPendingWebhookResponse)?;
        let next_sequence = self.next_control_sequence()?;
        let response = self
            .client
            .post(self.control_endpoint("/v1/control/discard-webhook-response"))
            .header("X-Tiv-Control-Token", &self.config.fixture_control_token)
            .json(&serde_json::json!({
                "command_sequence": next_sequence,
                "gate_id": pending.gate_id,
            }))
            .send()
            .await
            .map_err(WebhookHttpError::ControlRequest)?;
        require_status(response.status(), "discard webhook response")?;
        let state = response
            .json::<WebhookResponseState>()
            .await
            .map_err(WebhookHttpError::ControlRequest)?;
        if state.command_sequence != next_sequence || !state.held_webhook_responses.is_empty() {
            return Err(WebhookHttpError::UnexpectedControlResponse);
        }
        self.config.control_sequence = next_sequence;
        let task_result = pending
            .task
            .take()
            .expect("a pending webhook response always owns its sender task")
            .await
            .map_err(WebhookHttpError::RequestJoin)?;
        if !matches!(task_result, Err(WebhookHttpError::ControlRequest(_))) {
            return Err(WebhookHttpError::ExpectedDiscardedWebhookConnection);
        }
        let next_observation = self
            .fixture_producer_sequence
            .checked_add(1)
            .ok_or(WebhookHttpError::SequenceExhausted)?;
        request
            .record_observation(
                ObservationProducer::Fixture,
                next_observation,
                ObservationEvent::WebhookResponseDiscarded {
                    gate_id: pending.gate_id,
                },
            )
            .await?;
        self.fixture_producer_sequence = next_observation;
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

impl Drop for WebhookHttpAdapter {
    fn drop(&mut self) {
        self.pending_request.take();
        self.pending_response.take();
    }
}

async fn send_delivery_request(
    client: &Client,
    endpoint: Url,
    token: &str,
    command_sequence: u64,
    event_id: &str,
    timestamp: i64,
) -> Result<DeliveredEventResponse, WebhookHttpError> {
    let response = client
        .post(endpoint)
        .header("X-Tiv-Control-Token", token)
        .json(&serde_json::json!({
            "command_sequence": command_sequence,
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
    if delivered.command_sequence != command_sequence
        || delivered.event_id != event_id
        || delivered.timestamp != timestamp
    {
        return Err(WebhookHttpError::UnexpectedControlResponse);
    }
    Ok(delivered)
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

struct PendingWebhookResponse {
    task: Option<JoinHandle<Result<DeliveredEventResponse, WebhookHttpError>>>,
    gate_id: u64,
}

struct PendingWebhookRequest {
    task: Option<JoinHandle<Result<DeliveredEventResponse, WebhookHttpError>>>,
    gate_id: u64,
}

impl Drop for PendingWebhookRequest {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for PendingWebhookResponse {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeldWebhookResponse {
    gate_id: u64,
    event_id: String,
    status: u16,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeldWebhookRequest {
    gate_id: u64,
    event_id: String,
    forwarded: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookResponseState {
    command_sequence: u64,
    held_webhook_responses: Vec<HeldWebhookResponse>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookRequestState {
    command_sequence: u64,
    held_webhook_requests: Vec<HeldWebhookRequest>,
}

enum DeliveryProgress {
    Complete(DeliveredEventResponse),
    RequestForwarded,
}

#[derive(Debug, Error)]
pub enum WebhookHttpError {
    #[error("could not build the webhook control HTTP client: {0}")]
    ClientBuild(reqwest::Error),
    #[error("fixture webhook control request failed: {0}")]
    ControlRequest(reqwest::Error),
    #[error("fixture webhook control step timed out: {0}")]
    Timeout(&'static str),
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
    #[error("a webhook response is already pending at a process cut point")]
    WebhookResponseAlreadyPending,
    #[error("a webhook request is already pending at a process cut point")]
    WebhookRequestAlreadyPending,
    #[error("one delivery cannot own both webhook request and response cut points")]
    ConflictingWebhookCutPoints,
    #[error("no webhook response is pending at the process cut point")]
    NoPendingWebhookResponse,
    #[error("no webhook request is pending at the process cut point")]
    NoPendingWebhookRequest,
    #[error("the webhook delivery completed before its response gate was observed")]
    RequestCompletedBeforeWebhookGate,
    #[error("the discarded webhook response unexpectedly became an acknowledgment")]
    ExpectedDiscardedWebhookConnection,
    #[error("webhook sequence or timestamp exhausted")]
    SequenceExhausted,
    #[error("fixture webhook response task failed: {0}")]
    RequestJoin(tokio::task::JoinError),
    #[error("fixture webhook observation journal failed: {0}")]
    Journal(#[from] JournalError),
    #[error("action is outside the webhook HTTP adapter boundary")]
    UnsupportedAction,
}

impl WebhookHttpError {
    pub(crate) const fn is_inconclusive(&self) -> bool {
        matches!(self, Self::Timeout(_))
    }
}
