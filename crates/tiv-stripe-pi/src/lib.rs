//! Stripe `PaymentIntent` fixture for `TxProof`.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tiv_core::decision::Seed;
use tokio::sync::Notify;

pub mod control;
pub mod http;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Creates an idempotency key.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidIdempotencyKey`] when the key is blank or exceeds 255
    /// characters.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdempotencyKey> {
        let value = value.into();
        if value.trim().is_empty() || value.chars().count() > 255 {
            return Err(InvalidIdempotencyKey);
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidIdempotencyKey;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationId(String);

impl OperationId {
    /// Creates a semantic operation identifier for provider metadata.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidOperationId`] when the value is blank or exceeds 255
    /// characters.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidOperationId> {
        let value = value.into();
        if value.trim().is_empty() || value.chars().count() > 255 {
            return Err(InvalidOperationId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidOperationId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreatePaymentIntent {
    amount_minor: i64,
    currency: String,
    operation_id: Option<OperationId>,
}

impl CreatePaymentIntent {
    /// Creates validated `PaymentIntent` create parameters.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidCreateRequest::NonPositiveAmount`] unless the amount is
    /// a positive integer number of minor currency units. Returns
    /// [`InvalidCreateRequest::InvalidCurrency`] unless the currency is exactly
    /// three lowercase ASCII letters.
    pub fn new(
        amount_minor: i64,
        currency: impl Into<String>,
    ) -> Result<Self, InvalidCreateRequest> {
        if amount_minor <= 0 {
            return Err(InvalidCreateRequest::NonPositiveAmount);
        }
        let currency = currency.into();
        if currency.len() != 3 || !currency.bytes().all(|byte| byte.is_ascii_lowercase()) {
            return Err(InvalidCreateRequest::InvalidCurrency);
        }
        Ok(Self {
            amount_minor,
            currency,
            operation_id: None,
        })
    }

    #[must_use]
    pub fn with_operation_id(mut self, operation_id: OperationId) -> Self {
        self.operation_id = Some(operation_id);
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCreateRequest {
    InvalidCurrency,
    NonPositiveAmount,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultOutcome {
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentIntent {
    id: String,
    amount_minor: i64,
    currency: String,
    operation_id: Option<OperationId>,
    status: PaymentIntentStatus,
}

impl PaymentIntent {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn status(&self) -> PaymentIntentStatus {
        self.status
    }

    #[must_use]
    pub fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_ref().map(OperationId::as_str)
    }

    #[must_use]
    pub const fn amount_minor(&self) -> i64 {
        self.amount_minor
    }

    #[must_use]
    pub fn currency(&self) -> &str {
        &self.currency
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaymentIntentStatus {
    RequiresConfirmation,
    Succeeded,
}

/// An exact provider response retained by the idempotency cache.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataPlaneResponse {
    status_code: u16,
    content_type: &'static str,
    raw_body: Vec<u8>,
}

impl DataPlaneResponse {
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        self.status_code
    }

    #[must_use]
    pub const fn content_type(&self) -> &'static str {
        self.content_type
    }

    #[must_use]
    pub fn raw_body(&self) -> &[u8] {
        &self.raw_body
    }

    fn json(status_code: u16, raw_body: Vec<u8>) -> Self {
        Self {
            status_code,
            content_type: "application/json",
            raw_body,
        }
    }
}

/// The provider data-plane action that the HTTP adapter must perform.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataPlaneDisposition {
    Response(DataPlaneResponse),
    CloseConnection,
    DelayResponse(DataPlaneResponse),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct GateId(u64);

#[derive(Debug)]
pub enum ManagedDataPlaneDisposition {
    Response(DataPlaneResponse),
    CloseConnection,
    Held(HeldDataPlaneResponse),
}

#[derive(Debug)]
pub struct HeldDataPlaneResponse {
    gate_id: GateId,
    response: DataPlaneResponse,
    signal: Arc<GateSignal>,
}

/// A fixture webhook sender paused after observing the application's response.
#[derive(Debug)]
pub struct HeldWebhookResponse {
    gate_id: GateId,
    signal: Arc<GateSignal>,
}

/// A single-use, event-bound capability prepared before fixture webhook
/// delivery. This type intentionally exposes no printable representation.
pub struct PreparedWebhookRequestGate {
    gate_id: GateId,
    capability: String,
}

impl PreparedWebhookRequestGate {
    #[must_use]
    pub const fn gate_id(&self) -> GateId {
        self.gate_id
    }

    #[must_use]
    pub fn capability(&self) -> &str {
        &self.capability
    }
}

/// An application ingress callback paused before webhook persistence.
#[derive(Debug)]
pub struct HeldWebhookRequest {
    gate_id: GateId,
    signal: Arc<GateSignal>,
}

impl HeldWebhookRequest {
    #[must_use]
    pub const fn gate_id(&self) -> GateId {
        self.gate_id
    }

    /// Waits until control discards this exact forwarded request.
    ///
    /// # Errors
    ///
    /// Always returns [`HeldResponseCancelled`] after discard or reset. A
    /// request-forwarded crash cut never releases the application to persist.
    pub async fn wait(self) -> Result<(), HeldResponseCancelled> {
        self.signal.wait().await
    }
}

impl HeldWebhookResponse {
    #[must_use]
    pub const fn gate_id(&self) -> GateId {
        self.gate_id
    }

    /// Waits until control explicitly discards this observed acknowledgment.
    ///
    /// # Errors
    ///
    /// Returns [`HeldResponseCancelled`] after the exact discard or a reset
    /// wakes the sender without acknowledging the response.
    pub async fn wait(self) -> Result<(), HeldResponseCancelled> {
        self.signal.wait().await
    }
}

impl HeldDataPlaneResponse {
    #[must_use]
    pub const fn gate_id(&self) -> GateId {
        self.gate_id
    }

    /// Waits for the control plane to release this exact response.
    ///
    /// # Errors
    ///
    /// Returns [`HeldResponseCancelled`] when a reset replaces the fixture
    /// state that owned this gate.
    pub async fn wait(self) -> Result<DataPlaneResponse, HeldResponseCancelled> {
        self.signal.wait().await.map(|()| self.response)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeldResponseCancelled;

impl std::fmt::Display for HeldResponseCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("held response was cancelled")
    }
}

impl std::error::Error for HeldResponseCancelled {}

const GATE_PENDING: u8 = 0;
const GATE_RELEASED: u8 = 1;
const GATE_CANCELLED: u8 = 2;

#[derive(Debug)]
struct GateSignal {
    state: AtomicU8,
    notify: Notify,
}

impl Default for GateSignal {
    fn default() -> Self {
        Self {
            state: AtomicU8::new(GATE_PENDING),
            notify: Notify::new(),
        }
    }
}

impl GateSignal {
    fn release(&self) {
        self.state.store(GATE_RELEASED, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn cancel(&self) {
        self.state.store(GATE_CANCELLED, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) -> Result<(), HeldResponseCancelled> {
        loop {
            let notified = self.notify.notified();
            match self.state.load(Ordering::Acquire) {
                GATE_RELEASED => return Ok(()),
                GATE_CANCELLED => return Err(HeldResponseCancelled),
                GATE_PENDING => {}
                _ => unreachable!("gate state is private and closed"),
            }
            notified.await;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventKind {
    PaymentIntentSucceeded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderEvent {
    id: String,
    kind: EventKind,
    payment_intent_id: String,
    raw_body: Vec<u8>,
}

impl ProviderEvent {
    #[must_use]
    pub const fn kind(&self) -> EventKind {
        self.kind
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn payment_intent_id(&self) -> &str {
        &self.payment_intent_id
    }

    /// Signs a delivery attempt over the immutable raw event bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookSignatureError::EmptySecret`] for an empty secret or
    /// [`WebhookSignatureError::InvalidSecretLength`] if the HMAC
    /// implementation rejects the supplied secret.
    pub fn webhook_attempt(
        &self,
        timestamp: i64,
        secret: &[u8],
    ) -> Result<WebhookAttempt, WebhookSignatureError> {
        if secret.is_empty() {
            return Err(WebhookSignatureError::EmptySecret);
        }
        let mut signer = Hmac::<Sha256>::new_from_slice(secret)
            .map_err(|_| WebhookSignatureError::InvalidSecretLength)?;
        signer.update(timestamp.to_string().as_bytes());
        signer.update(b".");
        signer.update(&self.raw_body);
        let signature = hex::encode(signer.finalize().into_bytes());

        Ok(WebhookAttempt {
            event_id: self.id.clone(),
            timestamp,
            raw_body: self.raw_body.clone(),
            signature_header: format!("t={timestamp},v1={signature}"),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebhookSignatureError {
    EmptySecret,
    InvalidSecretLength,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookAttempt {
    event_id: String,
    timestamp: i64,
    raw_body: Vec<u8>,
    signature_header: String,
}

impl WebhookAttempt {
    #[must_use]
    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    #[must_use]
    pub const fn timestamp(&self) -> i64 {
        self.timestamp
    }

    #[must_use]
    pub fn raw_body(&self) -> &[u8] {
        &self.raw_body
    }

    #[must_use]
    pub fn signature_header(&self) -> &str {
        &self.signature_header
    }
}

#[derive(Serialize)]
struct EventWire<'a> {
    id: &'a str,
    object: &'static str,
    #[serde(rename = "type")]
    event_type: &'static str,
    data: EventDataWire<'a>,
}

#[derive(Serialize)]
struct EventDataWire<'a> {
    object: PaymentIntentWire<'a>,
}

#[derive(Serialize)]
struct PaymentIntentWire<'a> {
    id: &'a str,
    object: &'static str,
    amount: i64,
    currency: &'a str,
    status: &'static str,
    metadata: PaymentIntentMetadataWire<'a>,
}

#[derive(Serialize)]
struct PaymentIntentMetadataWire<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_id: Option<&'a str>,
}

#[derive(Serialize)]
struct PaymentIntentListWire<'a> {
    object: &'static str,
    data: Vec<PaymentIntentWire<'a>>,
    has_more: bool,
}

fn payment_intent_wire(payment_intent: &PaymentIntent) -> PaymentIntentWire<'_> {
    let status = match payment_intent.status {
        PaymentIntentStatus::RequiresConfirmation => "requires_confirmation",
        PaymentIntentStatus::Succeeded => "succeeded",
    };
    PaymentIntentWire {
        id: &payment_intent.id,
        object: "payment_intent",
        amount: payment_intent.amount_minor,
        currency: &payment_intent.currency,
        status,
        metadata: PaymentIntentMetadataWire {
            operation_id: payment_intent.operation_id(),
        },
    }
}

fn payment_intent_json(payment_intent: &PaymentIntent) -> Result<Vec<u8>, FixtureError> {
    serde_json::to_vec(&payment_intent_wire(payment_intent))
        .map_err(|_| FixtureError::Serialization)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IdempotencyEntry {
    request: CreatePaymentIntent,
    payment_intent: PaymentIntent,
    response: DataPlaneResponse,
}

struct CreateExecution {
    disposition: DataPlaneDisposition,
    payment_intent: Option<PaymentIntent>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureError {
    ConnectionClosed,
    IdempotencyConflict,
    NotFound,
    RateLimited,
    ServerError,
    Serialization,
}

pub struct PaymentIntentFixture {
    seed: Seed,
    payment_intents: Vec<PaymentIntent>,
    events: Vec<ProviderEvent>,
    idempotency: BTreeMap<IdempotencyKey, IdempotencyEntry>,
}

/// A fixture plus the explicit fault plan owned by the local control plane.
pub struct ManagedFixture {
    fixture: PaymentIntentFixture,
    planned_outcomes: VecDeque<FaultOutcome>,
    held_gates: BTreeMap<GateId, Arc<GateSignal>>,
    held_webhook_requests: BTreeMap<GateId, HeldWebhookRequestState>,
    held_webhook_responses: BTreeMap<GateId, HeldWebhookResponseState>,
    next_gate_sequence: u64,
    command_sequence: u64,
}

impl ManagedFixture {
    #[must_use]
    pub fn new(seed: Seed) -> Self {
        Self {
            fixture: PaymentIntentFixture::new(seed),
            planned_outcomes: VecDeque::new(),
            held_gates: BTreeMap::new(),
            held_webhook_requests: BTreeMap::new(),
            held_webhook_responses: BTreeMap::new(),
            next_gate_sequence: 0,
            command_sequence: 0,
        }
    }

    /// Replaces all provider state and installs a complete ordered fault plan.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureServiceError::UnexpectedCommandSequence`] unless this
    /// is exactly the next mutating command, or
    /// [`FixtureServiceError::EmptyFaultPlan`] for an empty plan.
    pub fn reset(
        &mut self,
        command_sequence: u64,
        seed: Seed,
        outcomes: Vec<FaultOutcome>,
    ) -> Result<FixtureSnapshot, FixtureServiceError> {
        self.require_next_sequence(command_sequence)?;
        if outcomes.is_empty() {
            return Err(FixtureServiceError::EmptyFaultPlan);
        }
        for signal in self.held_gates.values() {
            signal.cancel();
        }
        for request in self.held_webhook_requests.values() {
            request.signal.cancel();
        }
        for response in self.held_webhook_responses.values() {
            response.signal.cancel();
        }
        self.fixture = PaymentIntentFixture::new(seed);
        self.planned_outcomes = outcomes.into();
        self.held_gates.clear();
        self.held_webhook_requests.clear();
        self.held_webhook_responses.clear();
        self.command_sequence = command_sequence;
        Ok(self.snapshot())
    }

    /// Executes one valid create against the next planned provider outcome.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureServiceError::FaultPlanExhausted`] when no outcome is
    /// left, or wraps a provider fixture error.
    pub fn create_data_plane(
        &mut self,
        key: IdempotencyKey,
        request: CreatePaymentIntent,
    ) -> Result<ManagedDataPlaneDisposition, FixtureServiceError> {
        let outcome = self
            .planned_outcomes
            .pop_front()
            .ok_or(FixtureServiceError::FaultPlanExhausted)?;
        let disposition = self
            .fixture
            .create_data_plane(key, request, outcome)
            .map_err(FixtureServiceError::Fixture)?;
        self.manage_disposition(disposition)
    }

    fn manage_disposition(
        &mut self,
        disposition: DataPlaneDisposition,
    ) -> Result<ManagedDataPlaneDisposition, FixtureServiceError> {
        match disposition {
            DataPlaneDisposition::Response(response) => {
                Ok(ManagedDataPlaneDisposition::Response(response))
            }
            DataPlaneDisposition::CloseConnection => {
                Ok(ManagedDataPlaneDisposition::CloseConnection)
            }
            DataPlaneDisposition::DelayResponse(response) => {
                let gate_id = self.next_gate_id()?;
                let signal = Arc::new(GateSignal::default());
                self.held_gates.insert(gate_id, Arc::clone(&signal));
                Ok(ManagedDataPlaneDisposition::Held(HeldDataPlaneResponse {
                    gate_id,
                    response,
                    signal,
                }))
            }
        }
    }

    /// Releases one exact held provider response.
    ///
    /// # Errors
    ///
    /// Returns an error when the command is out of sequence or the gate does
    /// not exist in the current run-scoped fixture state.
    pub fn release_gate(
        &mut self,
        command_sequence: u64,
        gate_id: GateId,
    ) -> Result<FixtureSnapshot, FixtureServiceError> {
        self.require_next_sequence(command_sequence)?;
        let signal = self
            .held_gates
            .remove(&gate_id)
            .ok_or(FixtureServiceError::GateNotFound)?;
        signal.release();
        self.command_sequence = command_sequence;
        Ok(self.snapshot())
    }

    /// Prepares one single-use application ingress capability before sending
    /// an exact provider event.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown event, an already-active webhook
    /// ingress gate, or an exhausted gate sequence.
    pub fn prepare_webhook_request_gate(
        &mut self,
        event_id: &str,
    ) -> Result<PreparedWebhookRequestGate, FixtureServiceError> {
        if !self.held_webhook_requests.is_empty()
            || !self
                .fixture
                .events()
                .iter()
                .any(|event| event.id() == event_id)
        {
            return Err(FixtureServiceError::InvalidWebhookRequestGate);
        }
        let gate_id = self.next_gate_id()?;
        let capability = self.webhook_request_capability(gate_id, event_id);
        let signal = Arc::new(GateSignal::default());
        self.held_webhook_requests.insert(
            gate_id,
            HeldWebhookRequestState {
                event_id: event_id.to_owned(),
                capability: capability.clone(),
                forwarded: false,
                signal,
            },
        );
        Ok(PreparedWebhookRequestGate {
            gate_id,
            capability,
        })
    }

    /// Atomically consumes the exact application-facing ingress capability and
    /// marks the webhook request forwarded before application persistence.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing, wrong, or replayed capability.
    pub fn hold_webhook_request_forwarded(
        &mut self,
        capability: &str,
    ) -> Result<HeldWebhookRequest, FixtureServiceError> {
        let Some((gate_id, state)) = self
            .held_webhook_requests
            .iter_mut()
            .find(|(_, state)| state.capability == capability && !state.forwarded)
        else {
            return Err(FixtureServiceError::InvalidWebhookRequestGate);
        };
        state.forwarded = true;
        Ok(HeldWebhookRequest {
            gate_id: *gate_id,
            signal: Arc::clone(&state.signal),
        })
    }

    /// Discards one exact forwarded webhook request without letting the
    /// application proceed to persistence.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-order command or a gate that is absent
    /// or has not reached the forwarded boundary.
    pub fn discard_webhook_request(
        &mut self,
        command_sequence: u64,
        gate_id: GateId,
    ) -> Result<WebhookRequestState, FixtureServiceError> {
        self.require_next_sequence(command_sequence)?;
        if !self
            .held_webhook_requests
            .get(&gate_id)
            .is_some_and(|state| state.forwarded)
        {
            return Err(FixtureServiceError::GateNotFound);
        }
        let held = self
            .held_webhook_requests
            .remove(&gate_id)
            .ok_or(FixtureServiceError::GateNotFound)?;
        held.signal.cancel();
        self.command_sequence = command_sequence;
        Ok(self.webhook_request_state())
    }

    pub(crate) fn abort_unforwarded_webhook_request(&mut self, gate_id: GateId) {
        if self
            .held_webhook_requests
            .get(&gate_id)
            .is_some_and(|state| !state.forwarded)
            && let Some(held) = self.held_webhook_requests.remove(&gate_id)
        {
            held.signal.cancel();
        }
    }

    /// Holds one fixture sender after a real application webhook response.
    ///
    /// # Errors
    ///
    /// Returns an error if another webhook response is already held, the
    /// response is invalid, or the run-scoped gate sequence is exhausted.
    pub fn hold_webhook_response(
        &mut self,
        event_id: &str,
        status: u16,
    ) -> Result<HeldWebhookResponse, FixtureServiceError> {
        if !self.held_webhook_responses.is_empty()
            || !(100..=599).contains(&status)
            || !self
                .fixture
                .events()
                .iter()
                .any(|event| event.id() == event_id)
        {
            return Err(FixtureServiceError::InvalidWebhookResponseGate);
        }
        let gate_id = self.next_gate_id()?;
        let signal = Arc::new(GateSignal::default());
        self.held_webhook_responses.insert(
            gate_id,
            HeldWebhookResponseState {
                event_id: event_id.to_owned(),
                status,
                signal: Arc::clone(&signal),
            },
        );
        Ok(HeldWebhookResponse { gate_id, signal })
    }

    /// Discards one exact observed webhook response without acknowledging it.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-order command or a gate that is not a
    /// held webhook-response gate in the current run.
    pub fn discard_webhook_response(
        &mut self,
        command_sequence: u64,
        gate_id: GateId,
    ) -> Result<WebhookResponseState, FixtureServiceError> {
        self.require_next_sequence(command_sequence)?;
        let held = self
            .held_webhook_responses
            .remove(&gate_id)
            .ok_or(FixtureServiceError::GateNotFound)?;
        held.signal.cancel();
        self.command_sequence = command_sequence;
        Ok(self.webhook_response_state())
    }

    /// Executes one confirmation against the next planned provider outcome.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureServiceError::FaultPlanExhausted`] when no outcome is
    /// left, or wraps a provider fixture error.
    pub fn confirm_data_plane(
        &mut self,
        payment_intent_id: &str,
    ) -> Result<ManagedDataPlaneDisposition, FixtureServiceError> {
        let outcome = self
            .planned_outcomes
            .pop_front()
            .ok_or(FixtureServiceError::FaultPlanExhausted)?;
        let disposition = self
            .fixture
            .confirm_data_plane(payment_intent_id, outcome)
            .map_err(FixtureServiceError::Fixture)?;
        self.manage_disposition(disposition)
    }

    pub(crate) fn retrieve_data_plane(
        &self,
        payment_intent_id: &str,
    ) -> Result<DataPlaneResponse, FixtureError> {
        self.fixture.retrieve_data_plane(payment_intent_id)
    }

    pub(crate) fn search_data_plane(
        &self,
        operation_id: &str,
    ) -> Result<DataPlaneResponse, FixtureError> {
        self.fixture.search_data_plane(operation_id)
    }

    /// Confirms every provider object and returns signed, losslessly encoded
    /// webhook attempts.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-order command or an unexpected fixture
    /// or signing failure.
    pub fn confirm_all(
        &mut self,
        command_sequence: u64,
        timestamp: i64,
        secret: &control::WebhookSigningSecret,
    ) -> Result<ConfirmationResult, FixtureServiceError> {
        self.require_next_sequence(command_sequence)?;
        let ids = self
            .fixture
            .payment_intents()
            .iter()
            .map(|payment_intent| payment_intent.id().to_owned())
            .collect::<Vec<_>>();
        for id in ids {
            self.fixture
                .confirm(&id)
                .map_err(FixtureServiceError::Fixture)?;
        }
        let attempts = self
            .fixture
            .events()
            .iter()
            .map(|event| {
                event
                    .webhook_attempt(timestamp, secret.as_bytes())
                    .map(SignedWebhookAttempt::from)
                    .map_err(FixtureServiceError::WebhookSignature)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.command_sequence = command_sequence;
        Ok(ConfirmationResult {
            command_sequence,
            attempts,
        })
    }

    /// Confirms one exact provider object and exposes its immutable event.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-order command, an unknown provider
    /// object, or an unexpected fixture failure.
    pub fn generate_event(
        &mut self,
        command_sequence: u64,
        payment_intent_id: &str,
    ) -> Result<GeneratedEvent, FixtureServiceError> {
        self.require_next_sequence(command_sequence)?;
        self.fixture
            .confirm(payment_intent_id)
            .map_err(FixtureServiceError::Fixture)?;
        let event = self
            .fixture
            .events()
            .iter()
            .find(|event| event.payment_intent_id() == payment_intent_id)
            .ok_or(FixtureServiceError::EventNotFound)?;
        self.command_sequence = command_sequence;
        Ok(GeneratedEvent {
            command_sequence,
            event_id: event.id().to_owned(),
            payment_intent_id: payment_intent_id.to_owned(),
        })
    }

    /// Signs one exact immutable provider event for a fresh delivery attempt.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-order command, an unknown event, or a
    /// signing failure.
    pub fn sign_event(
        &mut self,
        command_sequence: u64,
        event_id: &str,
        timestamp: i64,
        secret: &control::WebhookSigningSecret,
    ) -> Result<SignedWebhookAttempt, FixtureServiceError> {
        self.require_next_sequence(command_sequence)?;
        let attempt = self
            .fixture
            .events()
            .iter()
            .find(|event| event.id() == event_id)
            .ok_or(FixtureServiceError::EventNotFound)?
            .webhook_attempt(timestamp, secret.as_bytes())
            .map(SignedWebhookAttempt::from)
            .map_err(FixtureServiceError::WebhookSignature)?;
        self.command_sequence = command_sequence;
        Ok(attempt)
    }

    #[must_use]
    pub fn snapshot(&self) -> FixtureSnapshot {
        FixtureSnapshot {
            command_sequence: self.command_sequence,
            remaining_outcomes: self.planned_outcomes.len(),
            held_gates: self
                .held_gates
                .keys()
                .copied()
                .map(|gate_id| HeldGateSnapshot { gate_id })
                .collect(),
            payment_intents: self
                .fixture
                .payment_intents()
                .iter()
                .map(PaymentIntentSnapshot::from)
                .collect(),
        }
    }

    #[must_use]
    pub fn webhook_response_state(&self) -> WebhookResponseState {
        WebhookResponseState {
            command_sequence: self.command_sequence,
            held_webhook_responses: self
                .held_webhook_responses
                .iter()
                .map(|(gate_id, held)| HeldWebhookResponseSnapshot {
                    gate_id: *gate_id,
                    event_id: held.event_id.clone(),
                    status: held.status,
                })
                .collect(),
        }
    }

    #[must_use]
    pub fn webhook_request_state(&self) -> WebhookRequestState {
        WebhookRequestState {
            command_sequence: self.command_sequence,
            held_webhook_requests: self
                .held_webhook_requests
                .iter()
                .map(|(gate_id, held)| HeldWebhookRequestSnapshot {
                    gate_id: *gate_id,
                    event_id: held.event_id.clone(),
                    forwarded: held.forwarded,
                })
                .collect(),
        }
    }

    fn webhook_request_capability(&self, gate_id: GateId, event_id: &str) -> String {
        let mut hasher =
            blake3::Hasher::new_derive_key("dev.txproof.webhook-ingress-capability.v1");
        hasher.update(&self.fixture.seed.value().to_le_bytes());
        hasher.update(&gate_id.0.to_le_bytes());
        hasher.update(&self.command_sequence.to_le_bytes());
        hasher.update(event_id.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    fn next_gate_id(&mut self) -> Result<GateId, FixtureServiceError> {
        self.next_gate_sequence = self
            .next_gate_sequence
            .checked_add(1)
            .ok_or(FixtureServiceError::GateSequenceExhausted)?;
        Ok(GateId(self.next_gate_sequence))
    }

    fn require_next_sequence(&self, received: u64) -> Result<(), FixtureServiceError> {
        let expected = self
            .command_sequence
            .checked_add(1)
            .ok_or(FixtureServiceError::CommandSequenceExhausted)?;
        if received != expected {
            return Err(FixtureServiceError::UnexpectedCommandSequence { expected, received });
        }
        Ok(())
    }
}

struct HeldWebhookResponseState {
    event_id: String,
    status: u16,
    signal: Arc<GateSignal>,
}

struct HeldWebhookRequestState {
    event_id: String,
    capability: String,
    forwarded: bool,
    signal: Arc<GateSignal>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebhookRequestState {
    command_sequence: u64,
    held_webhook_requests: Vec<HeldWebhookRequestSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HeldWebhookRequestSnapshot {
    gate_id: GateId,
    event_id: String,
    forwarded: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WebhookResponseState {
    command_sequence: u64,
    held_webhook_responses: Vec<HeldWebhookResponseSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HeldWebhookResponseSnapshot {
    gate_id: GateId,
    event_id: String,
    status: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FixtureSnapshot {
    command_sequence: u64,
    remaining_outcomes: usize,
    held_gates: Vec<HeldGateSnapshot>,
    payment_intents: Vec<PaymentIntentSnapshot>,
}

impl FixtureSnapshot {
    #[must_use]
    pub const fn command_sequence(&self) -> u64 {
        self.command_sequence
    }

    #[must_use]
    pub const fn remaining_outcomes(&self) -> usize {
        self.remaining_outcomes
    }

    #[must_use]
    pub fn held_gates(&self) -> &[HeldGateSnapshot] {
        &self.held_gates
    }

    #[must_use]
    pub fn payment_intents(&self) -> &[PaymentIntentSnapshot] {
        &self.payment_intents
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct HeldGateSnapshot {
    gate_id: GateId,
}

impl HeldGateSnapshot {
    #[must_use]
    pub const fn gate_id(&self) -> GateId {
        self.gate_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PaymentIntentSnapshot {
    id: String,
    amount_minor: i64,
    currency: String,
    status: &'static str,
    operation_id: Option<String>,
}

impl PaymentIntentSnapshot {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn amount_minor(&self) -> i64 {
        self.amount_minor
    }

    #[must_use]
    pub fn currency(&self) -> &str {
        &self.currency
    }

    #[must_use]
    pub fn status(&self) -> &str {
        self.status
    }

    #[must_use]
    pub fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }
}

impl From<&PaymentIntent> for PaymentIntentSnapshot {
    fn from(payment_intent: &PaymentIntent) -> Self {
        let status = match payment_intent.status() {
            PaymentIntentStatus::RequiresConfirmation => "requires_confirmation",
            PaymentIntentStatus::Succeeded => "succeeded",
        };
        Self {
            id: payment_intent.id().to_owned(),
            amount_minor: payment_intent.amount_minor(),
            currency: payment_intent.currency().to_owned(),
            status,
            operation_id: payment_intent.operation_id().map(str::to_owned),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfirmationResult {
    command_sequence: u64,
    attempts: Vec<SignedWebhookAttempt>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GeneratedEvent {
    command_sequence: u64,
    event_id: String,
    payment_intent_id: String,
}

impl GeneratedEvent {
    #[must_use]
    pub const fn command_sequence(&self) -> u64 {
        self.command_sequence
    }

    #[must_use]
    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    #[must_use]
    pub fn payment_intent_id(&self) -> &str {
        &self.payment_intent_id
    }
}

impl ConfirmationResult {
    #[must_use]
    pub const fn command_sequence(&self) -> u64 {
        self.command_sequence
    }

    #[must_use]
    pub fn attempts(&self) -> &[SignedWebhookAttempt] {
        &self.attempts
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SignedWebhookAttempt {
    event_id: String,
    timestamp: i64,
    raw_body_hex: String,
    signature_header: String,
}

impl SignedWebhookAttempt {
    #[must_use]
    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    #[must_use]
    pub const fn timestamp(&self) -> i64 {
        self.timestamp
    }

    #[must_use]
    pub fn raw_body_hex(&self) -> &str {
        &self.raw_body_hex
    }

    #[must_use]
    pub fn signature_header(&self) -> &str {
        &self.signature_header
    }
}

impl From<WebhookAttempt> for SignedWebhookAttempt {
    fn from(attempt: WebhookAttempt) -> Self {
        Self {
            event_id: attempt.event_id,
            timestamp: attempt.timestamp,
            raw_body_hex: hex::encode(attempt.raw_body),
            signature_header: attempt.signature_header,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixtureServiceError {
    CommandSequenceExhausted,
    EmptyFaultPlan,
    EventNotFound,
    FaultPlanExhausted,
    GateNotFound,
    GateSequenceExhausted,
    InvalidWebhookResponseGate,
    InvalidWebhookRequestGate,
    Fixture(FixtureError),
    UnexpectedCommandSequence { expected: u64, received: u64 },
    WebhookSignature(WebhookSignatureError),
}

impl PaymentIntentFixture {
    #[must_use]
    pub fn new(seed: Seed) -> Self {
        Self {
            seed,
            payment_intents: Vec::new(),
            events: Vec::new(),
            idempotency: BTreeMap::new(),
        }
    }

    /// Creates a `PaymentIntent` or returns the cached idempotent result.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::IdempotencyConflict`] when a previously executed
    /// key is reused with different create parameters, or
    /// [`FixtureError::RateLimited`] for a pre-execution 429 injection, or
    /// [`FixtureError::ServerError`] for a pre- or post-execution 500
    /// injection, or
    /// [`FixtureError::ConnectionClosed`] after a committed result is cached.
    pub fn create(
        &mut self,
        key: IdempotencyKey,
        request: CreatePaymentIntent,
        outcome: FaultOutcome,
    ) -> Result<PaymentIntent, FixtureError> {
        let execution = self.execute_create(key, request, outcome)?;
        match (execution.disposition, execution.payment_intent) {
            (DataPlaneDisposition::CloseConnection, _) => Err(FixtureError::ConnectionClosed),
            (DataPlaneDisposition::DelayResponse(_), Some(payment_intent)) => Ok(payment_intent),
            (DataPlaneDisposition::Response(response), Some(payment_intent))
                if response.status_code == 200 =>
            {
                Ok(payment_intent)
            }
            (DataPlaneDisposition::Response(response), _) if response.status_code == 429 => {
                Err(FixtureError::RateLimited)
            }
            (DataPlaneDisposition::DelayResponse(_), None)
            | (DataPlaneDisposition::Response(_), _) => Err(FixtureError::ServerError),
        }
    }

    /// Executes a create through the provider data-plane contract.
    ///
    /// Executed requests retain the exact first status and raw response body.
    /// A matching retry receives those cached bytes even when the first
    /// response was a 500. Pre-execution failures are not cached.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::IdempotencyConflict`] when a key is reused with
    /// different parameters, or [`FixtureError::Serialization`] when the
    /// supported response cannot be encoded.
    pub fn create_data_plane(
        &mut self,
        key: IdempotencyKey,
        request: CreatePaymentIntent,
        outcome: FaultOutcome,
    ) -> Result<DataPlaneDisposition, FixtureError> {
        self.execute_create(key, request, outcome)
            .map(|execution| execution.disposition)
    }

    fn execute_create(
        &mut self,
        key: IdempotencyKey,
        request: CreatePaymentIntent,
        outcome: FaultOutcome,
    ) -> Result<CreateExecution, FixtureError> {
        if let Some(entry) = self.idempotency.get(&key) {
            if entry.request != request {
                return Err(FixtureError::IdempotencyConflict);
            }
            return Ok(CreateExecution {
                disposition: DataPlaneDisposition::Response(entry.response.clone()),
                payment_intent: Some(entry.payment_intent.clone()),
            });
        }

        let pre_execution_response = match outcome {
            FaultOutcome::PreExecute429 => Some(DataPlaneResponse::json(
                429,
                br#"{"error":{"type":"rate_limit_error"}}"#.to_vec(),
            )),
            FaultOutcome::PreExecute500 => Some(DataPlaneResponse::json(
                500,
                br#"{"error":{"type":"api_error"}}"#.to_vec(),
            )),
            FaultOutcome::Normal
            | FaultOutcome::PostExecute500
            | FaultOutcome::CommitThenClose
            | FaultOutcome::CommitThenDelay => None,
        };
        if let Some(response) = pre_execution_response {
            return Ok(CreateExecution {
                disposition: DataPlaneDisposition::Response(response),
                payment_intent: None,
            });
        }

        let sequence = self.payment_intents.len() as u64 + 1;
        let payment_intent = PaymentIntent {
            id: self.payment_intent_id(sequence),
            amount_minor: request.amount_minor,
            currency: request.currency.clone(),
            operation_id: request.operation_id.clone(),
            status: PaymentIntentStatus::RequiresConfirmation,
        };
        self.payment_intents.push(payment_intent.clone());
        let response = if outcome == FaultOutcome::PostExecute500 {
            DataPlaneResponse::json(500, br#"{"error":{"type":"api_error"}}"#.to_vec())
        } else {
            DataPlaneResponse::json(200, payment_intent_json(&payment_intent)?)
        };
        self.idempotency.insert(
            key,
            IdempotencyEntry {
                request,
                payment_intent: payment_intent.clone(),
                response: response.clone(),
            },
        );
        let disposition = match outcome {
            FaultOutcome::CommitThenClose => DataPlaneDisposition::CloseConnection,
            FaultOutcome::CommitThenDelay => DataPlaneDisposition::DelayResponse(response),
            FaultOutcome::Normal
            | FaultOutcome::PreExecute429
            | FaultOutcome::PreExecute500
            | FaultOutcome::PostExecute500 => DataPlaneDisposition::Response(response),
        };
        Ok(CreateExecution {
            disposition,
            payment_intent: Some(payment_intent),
        })
    }

    #[must_use]
    pub fn payment_intent_count(&self) -> usize {
        self.payment_intents.len()
    }

    #[must_use]
    pub fn payment_intents(&self) -> &[PaymentIntent] {
        &self.payment_intents
    }

    /// Confirms a `PaymentIntent` in the supported checkout path.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::NotFound`] when the ID is unknown.
    pub fn confirm(&mut self, id: &str) -> Result<PaymentIntent, FixtureError> {
        let index = self
            .payment_intents
            .iter()
            .position(|payment_intent| payment_intent.id == id)
            .ok_or(FixtureError::NotFound)?;
        if self.payment_intents[index].status != PaymentIntentStatus::Succeeded {
            self.payment_intents[index].status = PaymentIntentStatus::Succeeded;
            let event_sequence = self.events.len() as u64 + 1;
            let event_id = self.provider_event_id(event_sequence);
            let payment_intent = &self.payment_intents[index];
            let raw_body = serde_json::to_vec(&EventWire {
                id: &event_id,
                object: "event",
                event_type: "payment_intent.succeeded",
                data: EventDataWire {
                    object: PaymentIntentWire {
                        id: &payment_intent.id,
                        object: "payment_intent",
                        amount: payment_intent.amount_minor,
                        currency: &payment_intent.currency,
                        status: "succeeded",
                        metadata: PaymentIntentMetadataWire {
                            operation_id: payment_intent.operation_id(),
                        },
                    },
                },
            })
            .map_err(|_| FixtureError::Serialization)?;
            self.events.push(ProviderEvent {
                id: event_id,
                kind: EventKind::PaymentIntentSucceeded,
                payment_intent_id: id.to_owned(),
                raw_body,
            });
        }
        Ok(self.payment_intents[index].clone())
    }

    /// Retrieves a `PaymentIntent` from fixture state.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::NotFound`] when the ID is unknown.
    pub fn get(&self, id: &str) -> Result<PaymentIntent, FixtureError> {
        self.payment_intents
            .iter()
            .find(|payment_intent| payment_intent.id == id)
            .cloned()
            .ok_or(FixtureError::NotFound)
    }

    fn confirm_data_plane(
        &mut self,
        id: &str,
        outcome: FaultOutcome,
    ) -> Result<DataPlaneDisposition, FixtureError> {
        let pre_execution_response = match outcome {
            FaultOutcome::PreExecute429 => Some(DataPlaneResponse::json(
                429,
                br#"{"error":{"type":"rate_limit_error"}}"#.to_vec(),
            )),
            FaultOutcome::PreExecute500 => Some(DataPlaneResponse::json(
                500,
                br#"{"error":{"type":"api_error"}}"#.to_vec(),
            )),
            FaultOutcome::Normal
            | FaultOutcome::PostExecute500
            | FaultOutcome::CommitThenClose
            | FaultOutcome::CommitThenDelay => None,
        };
        if let Some(response) = pre_execution_response {
            return Ok(DataPlaneDisposition::Response(response));
        }

        let payment_intent = self.confirm(id)?;
        let response = if outcome == FaultOutcome::PostExecute500 {
            DataPlaneResponse::json(500, br#"{"error":{"type":"api_error"}}"#.to_vec())
        } else {
            DataPlaneResponse::json(200, payment_intent_json(&payment_intent)?)
        };
        Ok(match outcome {
            FaultOutcome::CommitThenClose => DataPlaneDisposition::CloseConnection,
            FaultOutcome::CommitThenDelay => DataPlaneDisposition::DelayResponse(response),
            FaultOutcome::Normal
            | FaultOutcome::PreExecute429
            | FaultOutcome::PreExecute500
            | FaultOutcome::PostExecute500 => DataPlaneDisposition::Response(response),
        })
    }

    fn retrieve_data_plane(&self, id: &str) -> Result<DataPlaneResponse, FixtureError> {
        let payment_intent = self.get(id)?;
        Ok(DataPlaneResponse::json(
            200,
            payment_intent_json(&payment_intent)?,
        ))
    }

    fn search_data_plane(&self, operation_id: &str) -> Result<DataPlaneResponse, FixtureError> {
        let data = self
            .payment_intents
            .iter()
            .filter(|payment_intent| payment_intent.operation_id() == Some(operation_id))
            .map(payment_intent_wire)
            .collect();
        let raw_body = serde_json::to_vec(&PaymentIntentListWire {
            object: "list",
            data,
            has_more: false,
        })
        .map_err(|_| FixtureError::Serialization)?;
        Ok(DataPlaneResponse::json(200, raw_body))
    }

    #[must_use]
    pub fn events(&self) -> &[ProviderEvent] {
        &self.events
    }

    fn payment_intent_id(&self, sequence: u64) -> String {
        self.derived_id("payment-intent-id", "pi_tiv_", sequence)
    }

    fn provider_event_id(&self, sequence: u64) -> String {
        self.derived_id("provider-event-id", "evt_tiv_", sequence)
    }

    fn derived_id(&self, domain: &str, prefix: &str, sequence: u64) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"tiv/stripe-pi/");
        hasher.update(domain.as_bytes());
        hasher.update(b"/v1");
        hasher.update(&self.seed.value().to_le_bytes());
        hasher.update(&sequence.to_le_bytes());
        let digest = hasher.finalize().to_hex();
        format!("{prefix}{}", &digest.as_str()[..24])
    }
}
