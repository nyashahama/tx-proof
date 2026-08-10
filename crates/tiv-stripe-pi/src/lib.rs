//! Stripe `PaymentIntent` fixture for `TxProof`.

use std::collections::BTreeMap;

use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::Sha256;
use tiv_core::decision::Seed;

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
pub struct CreatePaymentIntent {
    amount_minor: i64,
    currency: String,
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
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidCreateRequest {
    InvalidCurrency,
    NonPositiveAmount,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    currency: &'a str,
    status: &'static str,
}

fn payment_intent_json(payment_intent: &PaymentIntent) -> Result<Vec<u8>, FixtureError> {
    let status = match payment_intent.status {
        PaymentIntentStatus::RequiresConfirmation => "requires_confirmation",
        PaymentIntentStatus::Succeeded => "succeeded",
    };
    serde_json::to_vec(&PaymentIntentWire {
        id: &payment_intent.id,
        object: "payment_intent",
        amount: payment_intent.amount_minor,
        currency: &payment_intent.currency,
        status,
    })
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
                    object: PaymentIntentWire {
                        id: &payment_intent.id,
                        object: "payment_intent",
                        amount: payment_intent.amount_minor,
                        currency: &payment_intent.currency,
                        status: "succeeded",
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
