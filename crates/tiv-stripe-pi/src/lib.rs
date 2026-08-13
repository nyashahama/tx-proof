//! Stripe `PaymentIntent` fixture for `TxProof`.

use std::collections::{BTreeMap, VecDeque};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tiv_core::decision::Seed;

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
    capture_method: Option<String>,
    metadata: BTreeMap<String, String>,
    receipt_email: Option<String>,
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
            capture_method: None,
            metadata: BTreeMap::new(),
            receipt_email: None,
        })
    }

    #[must_use]
    pub fn with_operation_id(mut self, operation_id: &OperationId) -> Self {
        self.metadata
            .insert("operation_id".to_owned(), operation_id.as_str().to_owned());
        self
    }

    pub(crate) fn set_capture_method(
        &mut self,
        capture_method: impl Into<String>,
    ) -> Result<(), InvalidCreateRequest> {
        let capture_method = capture_method.into();
        if !matches!(capture_method.as_str(), "automatic" | "manual") {
            return Err(InvalidCreateRequest::InvalidCaptureMethod);
        }
        self.capture_method = Some(capture_method);
        Ok(())
    }

    pub(crate) fn insert_metadata(
        &mut self,
        key: &'static str,
        value: impl Into<String>,
    ) -> Result<(), InvalidOperationId> {
        let value = OperationId::new(value)?;
        self.metadata
            .insert(key.to_owned(), value.as_str().to_owned());
        Ok(())
    }

    pub(crate) fn set_receipt_email(
        &mut self,
        receipt_email: impl Into<String>,
    ) -> Result<(), InvalidCreateRequest> {
        let receipt_email = receipt_email.into();
        if receipt_email.trim().is_empty() || receipt_email.chars().count() > 320 {
            return Err(InvalidCreateRequest::InvalidReceiptEmail);
        }
        self.receipt_email = Some(receipt_email);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCreateRequest {
    InvalidCaptureMethod,
    InvalidCurrency,
    InvalidReceiptEmail,
    NonPositiveAmount,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultOutcome {
    Normal,
    PreExecute429,
    PreExecute500,
    PostExecute500,
    CommitThenClose,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentIntent {
    id: String,
    amount_minor: i64,
    currency: String,
    metadata: BTreeMap<String, String>,
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
        self.metadata.get("operation_id").map(String::as_str)
    }

    #[must_use]
    pub fn metadata_value(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).map(String::as_str)
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
    amount_received: i64,
    client_secret: String,
    currency: &'a str,
    status: &'static str,
    metadata: &'a BTreeMap<String, String>,
}

fn payment_intent_json(payment_intent: &PaymentIntent) -> Result<Vec<u8>, FixtureError> {
    let status = match payment_intent.status {
        PaymentIntentStatus::RequiresConfirmation => "requires_confirmation",
        PaymentIntentStatus::Succeeded => "succeeded",
    };
    serde_json::to_vec(&payment_intent_wire(payment_intent, status))
        .map_err(|_| FixtureError::Serialization)
}

fn payment_intent_wire<'a>(
    payment_intent: &'a PaymentIntent,
    status: &'static str,
) -> PaymentIntentWire<'a> {
    PaymentIntentWire {
        id: &payment_intent.id,
        object: "payment_intent",
        amount: payment_intent.amount_minor,
        amount_received: if status == "succeeded" {
            payment_intent.amount_minor
        } else {
            0
        },
        client_secret: payment_intent_client_secret(&payment_intent.id),
        currency: &payment_intent.currency,
        status,
        metadata: &payment_intent.metadata,
    }
}

fn payment_intent_client_secret(id: &str) -> String {
    format!("{id}_secret_tiv")
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
    command_sequence: u64,
}

impl ManagedFixture {
    #[must_use]
    pub fn new(seed: Seed) -> Self {
        Self {
            fixture: PaymentIntentFixture::new(seed),
            planned_outcomes: VecDeque::new(),
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
        self.fixture = PaymentIntentFixture::new(seed);
        self.planned_outcomes = outcomes.into();
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
    ) -> Result<DataPlaneDisposition, FixtureServiceError> {
        let outcome = self
            .planned_outcomes
            .pop_front()
            .ok_or(FixtureServiceError::FaultPlanExhausted)?;
        self.fixture
            .create_data_plane(key, request, outcome)
            .map_err(FixtureServiceError::Fixture)
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

    #[must_use]
    pub fn snapshot(&self) -> FixtureSnapshot {
        FixtureSnapshot {
            command_sequence: self.command_sequence,
            remaining_outcomes: self.planned_outcomes.len(),
            payment_intents: self
                .fixture
                .payment_intents()
                .iter()
                .map(PaymentIntentSnapshot::from)
                .collect(),
        }
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FixtureSnapshot {
    command_sequence: u64,
    remaining_outcomes: usize,
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
    pub fn payment_intents(&self) -> &[PaymentIntentSnapshot] {
        &self.payment_intents
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PaymentIntentSnapshot {
    id: String,
    amount_minor: i64,
    currency: String,
    status: &'static str,
    metadata: BTreeMap<String, String>,
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

    #[must_use]
    pub fn metadata_value(&self, key: &str) -> Option<&str> {
        self.metadata.get(key).map(String::as_str)
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
            metadata: payment_intent.metadata.clone(),
            operation_id: payment_intent.operation_id().map(str::to_owned),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfirmationResult {
    command_sequence: u64,
    attempts: Vec<SignedWebhookAttempt>,
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
    FaultPlanExhausted,
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
            (DataPlaneDisposition::Response(response), Some(payment_intent))
                if response.status_code == 200 =>
            {
                Ok(payment_intent)
            }
            (DataPlaneDisposition::Response(response), _) if response.status_code == 429 => {
                Err(FixtureError::RateLimited)
            }
            (DataPlaneDisposition::Response(_), _) => Err(FixtureError::ServerError),
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
            FaultOutcome::Normal | FaultOutcome::PostExecute500 | FaultOutcome::CommitThenClose => {
                None
            }
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
            metadata: request.metadata.clone(),
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
        let disposition = if outcome == FaultOutcome::CommitThenClose {
            DataPlaneDisposition::CloseConnection
        } else {
            DataPlaneDisposition::Response(response)
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
                    object: payment_intent_wire(payment_intent, "succeeded"),
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
