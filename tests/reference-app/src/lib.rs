use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::{
    Method, Request, Response, StatusCode,
    body::Incoming,
    header::{CONTENT_TYPE, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::{net::TcpStream, sync::Mutex, time::timeout};
use tokio_postgres::{Client, NoTls, Transaction};
use uuid::Uuid;

const DRIVER_ACTION_ID_HEADER: &str = "X-Tiv-Action-Id";
const WEBHOOK_INGRESS_CAPABILITY_HEADER: &str = "X-Tiv-Webhook-Ingress-Capability";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceDatabaseName(String);

impl ReferenceDatabaseName {
    /// Parses only the generated case-database grammar used by the disposable
    /// reference stack.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceAppError::InvalidDatabaseName`] for any other name.
    pub fn parse(value: impl Into<String>) -> Result<Self, ReferenceAppError> {
        let value = value.into();
        let Some(suffix) = value.strip_prefix("tiv_case_") else {
            return Err(ReferenceAppError::InvalidDatabaseName);
        };
        if !(8..=32).contains(&suffix.len())
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ReferenceAppError::InvalidDatabaseName);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the operation identity reserved for this generated case.
    #[must_use]
    pub fn operation_id(&self) -> String {
        format!("op_{}", self.0.trim_start_matches("tiv_case_"))
    }

    /// Recovers the only generated case that may own an operation identity.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceAppError::InvalidDatabaseName`] unless the operation
    /// uses the exact case-derived identity grammar.
    pub fn from_operation_id(value: &str) -> Result<Self, ReferenceAppError> {
        let suffix = value
            .strip_prefix("op_")
            .ok_or(ReferenceAppError::InvalidDatabaseName)?;
        Self::parse(format!("tiv_case_{suffix}"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckoutOperation {
    operation_id: String,
    amount_minor: i64,
    currency: String,
}

impl CheckoutOperation {
    /// Creates one supported synthetic checkout operation.
    ///
    /// # Errors
    ///
    /// Rejects unsafe identifiers, non-positive amounts, and currencies that
    /// are not three lowercase ASCII letters.
    pub fn new(
        operation_id: impl Into<String>,
        amount_minor: i64,
        currency: impl Into<String>,
    ) -> Result<Self, ReferenceAppError> {
        let operation_id = operation_id.into();
        let currency = currency.into();
        let mut operation_bytes = operation_id.bytes();
        let valid_operation_id = operation_bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && operation_bytes
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
        if operation_id.len() > 255 || !valid_operation_id {
            return Err(ReferenceAppError::InvalidOperation);
        }
        if amount_minor <= 0 {
            return Err(ReferenceAppError::InvalidOperation);
        }
        if currency.len() != 3 || !currency.bytes().all(|byte| byte.is_ascii_lowercase()) {
            return Err(ReferenceAppError::InvalidOperation);
        }
        Ok(Self {
            operation_id,
            amount_minor,
            currency,
        })
    }

    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
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

/// Controls whether an ambiguous provider create is retried with the known
/// faulty changed key or the repaired original key.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RetryKeyMode {
    /// Reproduces the reference bug by changing the key after a transport
    /// failure.
    #[default]
    FaultyChangedKey,
    /// Reuses the original key so an executed provider request is not repeated.
    RepairedSameKey,
}

impl RetryKeyMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FaultyChangedKey => "faulty_changed_key",
            Self::RepairedSameKey => "repaired_same_key",
        }
    }
}

impl FromStr for RetryKeyMode {
    type Err = InvalidRetryKeyMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "faulty_changed_key" => Ok(Self::FaultyChangedKey),
            "repaired_same_key" => Ok(Self::RepairedSameKey),
            _ => Err(InvalidRetryKeyMode),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidRetryKeyMode;

impl fmt::Display for InvalidRetryKeyMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid reference-app retry-key mode")
    }
}

impl Error for InvalidRetryKeyMode {}

/// Controls whether a new caller request blindly starts another provider
/// create or first recovers an object already committed for the operation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CallerRetryMode {
    /// Reproduces the bug by assigning every caller request a fresh scope.
    #[default]
    FaultyPerRequest,
    /// Recovers an existing provider object by immutable operation metadata.
    RepairedRecoverOperation,
}

impl CallerRetryMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FaultyPerRequest => "faulty_per_request",
            Self::RepairedRecoverOperation => "repaired_recover_operation",
        }
    }
}

impl FromStr for CallerRetryMode {
    type Err = InvalidCallerRetryMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "faulty_per_request" => Ok(Self::FaultyPerRequest),
            "repaired_recover_operation" => Ok(Self::RepairedRecoverOperation),
            _ => Err(InvalidCallerRetryMode),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCallerRetryMode;

impl fmt::Display for InvalidCallerRetryMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid reference-app caller-retry mode")
    }
}

impl Error for InvalidCallerRetryMode {}

/// Controls whether provider success depends only on webhook delivery or is
/// also reconciled from provider state within a bounded horizon.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReconciliationMode {
    /// Reproduces the bug by leaving a dropped success event unreconciled.
    #[default]
    FaultyWebhookOnly,
    /// Polls the exact provider object and converges local payment state.
    RepairedProviderReconcile,
}

impl ReconciliationMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FaultyWebhookOnly => "faulty_webhook_only",
            Self::RepairedProviderReconcile => "repaired_provider_reconcile",
        }
    }
}

impl FromStr for ReconciliationMode {
    type Err = InvalidReconciliationMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "faulty_webhook_only" => Ok(Self::FaultyWebhookOnly),
            "repaired_provider_reconcile" => Ok(Self::RepairedProviderReconcile),
            _ => Err(InvalidReconciliationMode),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidReconciliationMode;

impl fmt::Display for InvalidReconciliationMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid reference-app reconciliation mode")
    }
}

impl Error for InvalidReconciliationMode {}

/// Controls whether repeated delivery of one authenticated provider event
/// applies its business effect again or is durably deduplicated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WebhookEffectMode {
    /// Reproduces the reference bug by applying the effect for every delivery.
    FaultyDuplicateEffect,
    /// Applies the effect only when the immutable provider event is first seen.
    #[default]
    RepairedDeduplicate,
}

impl WebhookEffectMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FaultyDuplicateEffect => "faulty_duplicate_effect",
            Self::RepairedDeduplicate => "repaired_deduplicate",
        }
    }
}

impl FromStr for WebhookEffectMode {
    type Err = InvalidWebhookEffectMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "faulty_duplicate_effect" => Ok(Self::FaultyDuplicateEffect),
            "repaired_deduplicate" => Ok(Self::RepairedDeduplicate),
            _ => Err(InvalidWebhookEffectMode),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidWebhookEffectMode;

impl fmt::Display for InvalidWebhookEffectMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid reference-app webhook-effect mode")
    }
}

impl Error for InvalidWebhookEffectMode {}

/// Controls how the reference application posts its double-entry ledger.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LedgerBalanceMode {
    /// Reproduces a partial repeated-effect write with only the debit side.
    FaultyOneSidedOnDuplicate,
    /// Posts one balanced debit/credit pair for the first accepted effect.
    #[default]
    RepairedBalancedOnce,
}

impl LedgerBalanceMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FaultyOneSidedOnDuplicate => "faulty_one_sided_duplicate",
            Self::RepairedBalancedOnce => "repaired_balanced_once",
        }
    }
}

impl FromStr for LedgerBalanceMode {
    type Err = InvalidLedgerBalanceMode;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "faulty_one_sided_duplicate" => Ok(Self::FaultyOneSidedOnDuplicate),
            "repaired_balanced_once" => Ok(Self::RepairedBalancedOnce),
            _ => Err(InvalidLedgerBalanceMode),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidLedgerBalanceMode;

impl fmt::Display for InvalidLedgerBalanceMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid reference-app ledger-balance mode")
    }
}

impl Error for InvalidLedgerBalanceMode {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedPaymentIntent {
    id: String,
    operation_id: String,
    amount_minor: i64,
    currency: String,
    status: String,
}

impl ObservedPaymentIntent {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
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
        &self.status
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedWebhookEvent {
    id: String,
    payment_intent: ObservedPaymentIntent,
}

impl ObservedWebhookEvent {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn payment_intent(&self) -> &ObservedPaymentIntent {
        &self.payment_intent
    }

    #[must_use]
    pub fn into_payment_intent(self) -> ObservedPaymentIntent {
        self.payment_intent
    }
}

/// Deliberately reproduces the reference bug: after an ambiguous transport
/// failure, the retry changes its idempotency key.
///
/// # Errors
///
/// Returns [`ReferenceAppError`] when both attempts fail or the fixture returns
/// an invalid response.
pub async fn create_with_changed_retry_key(
    client: &reqwest::Client,
    fixture_base_url: &str,
    operation: &CheckoutOperation,
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    create_with_retry_key_mode_in_scope(
        client,
        fixture_base_url,
        operation,
        operation.operation_id(),
        RetryKeyMode::FaultyChangedKey,
    )
    .await
}

/// Executes one provider create with the selected retry-key behavior.
///
/// # Errors
///
/// Returns [`ReferenceAppError`] when both attempts fail or the fixture returns
/// an invalid response.
pub async fn create_with_retry_key_mode(
    client: &reqwest::Client,
    fixture_base_url: &str,
    operation: &CheckoutOperation,
    retry_key_mode: RetryKeyMode,
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    create_with_retry_key_mode_in_scope(
        client,
        fixture_base_url,
        operation,
        operation.operation_id(),
        retry_key_mode,
    )
    .await
}

/// Executes the deliberately faulty provider retry in one caller-owned
/// business-request scope.
///
/// A caller retry is a new business request, so its provider idempotency keys
/// must not alias the keys used by an earlier planned action. The two provider
/// attempts inside this request still intentionally use different keys.
///
/// # Errors
///
/// Returns [`ReferenceAppError`] when both attempts fail or the fixture returns
/// an invalid response.
pub async fn create_with_changed_retry_key_for_business_request(
    client: &reqwest::Client,
    fixture_base_url: &str,
    operation: &CheckoutOperation,
    business_request_id: u32,
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    create_with_retry_key_mode_for_business_request(
        client,
        fixture_base_url,
        operation,
        business_request_id,
        RetryKeyMode::FaultyChangedKey,
    )
    .await
}

/// Executes one caller-owned business request with the selected retry-key
/// behavior.
///
/// # Errors
///
/// Returns [`ReferenceAppError`] when both attempts fail or the fixture returns
/// an invalid response.
pub async fn create_with_retry_key_mode_for_business_request(
    client: &reqwest::Client,
    fixture_base_url: &str,
    operation: &CheckoutOperation,
    business_request_id: u32,
    retry_key_mode: RetryKeyMode,
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    let idempotency_scope = format!(
        "{}-business-{business_request_id}",
        operation.operation_id()
    );
    create_with_retry_key_mode_in_scope(
        client,
        fixture_base_url,
        operation,
        &idempotency_scope,
        retry_key_mode,
    )
    .await
}

async fn create_with_retry_key_mode_in_scope(
    client: &reqwest::Client,
    fixture_base_url: &str,
    operation: &CheckoutOperation,
    idempotency_scope: &str,
    retry_key_mode: RetryKeyMode,
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    let endpoint = format!(
        "{}/v1/payment_intents",
        fixture_base_url.trim_end_matches('/')
    );
    let first_key = format!("{idempotency_scope}-attempt-1");
    let first = send_create(client, &endpoint, &first_key, operation).await;
    if let Ok(response) = first {
        decode_provider_response(response, operation).await
    } else {
        let retry_key = match retry_key_mode {
            RetryKeyMode::FaultyChangedKey => format!("{idempotency_scope}-attempt-2"),
            RetryKeyMode::RepairedSameKey => first_key,
        };
        let retry = send_create(client, &endpoint, &retry_key, operation)
            .await
            .map_err(|_| ReferenceAppError::ProviderTransport)?;
        decode_provider_response(retry, operation).await
    }
}

async fn send_create(
    client: &reqwest::Client,
    endpoint: &str,
    idempotency_key: &str,
    operation: &CheckoutOperation,
) -> Result<reqwest::Response, reqwest::Error> {
    client
        .post(endpoint)
        .header("Idempotency-Key", idempotency_key)
        .form(&[
            ("amount", operation.amount_minor().to_string()),
            ("currency", operation.currency().to_owned()),
            (
                "metadata[operation_id]",
                operation.operation_id().to_owned(),
            ),
        ])
        .send()
        .await
}

async fn decode_provider_response(
    response: reqwest::Response,
    operation: &CheckoutOperation,
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    if response.status() != StatusCode::OK {
        return Err(ReferenceAppError::ProviderResponse);
    }
    let response = response
        .json::<PaymentIntentWire>()
        .await
        .map_err(|_| ReferenceAppError::ProviderResponse)?;
    observed_payment_intent(response, operation)
}

fn observed_payment_intent(
    response: PaymentIntentWire,
    operation: &CheckoutOperation,
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    observed_payment_intent_with_statuses(response, operation, &["requires_confirmation"])
}

fn observed_payment_intent_with_statuses(
    response: PaymentIntentWire,
    operation: &CheckoutOperation,
    allowed_statuses: &[&str],
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    if response.object != "payment_intent"
        || !response.id.starts_with("pi_tiv_")
        || response.amount != operation.amount_minor()
        || response.currency != operation.currency()
        || !allowed_statuses.contains(&response.status.as_str())
        || response.metadata.operation_id != operation.operation_id()
    {
        return Err(ReferenceAppError::ProviderResponse);
    }
    Ok(ObservedPaymentIntent {
        id: response.id,
        operation_id: response.metadata.operation_id,
        amount_minor: response.amount,
        currency: response.currency,
        status: response.status,
    })
}

async fn retrieve_payment_intent_for_reconciliation(
    client: &reqwest::Client,
    fixture_base_url: &str,
    expected: &ObservedPaymentIntent,
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    let operation = CheckoutOperation::new(
        expected.operation_id(),
        expected.amount_minor(),
        expected.currency(),
    )?;
    let response = client
        .get(format!(
            "{}/v1/payment_intents/{}",
            fixture_base_url.trim_end_matches('/'),
            expected.id()
        ))
        .send()
        .await
        .map_err(|_| ReferenceAppError::ProviderTransport)?;
    if response.status() != StatusCode::OK {
        return Err(ReferenceAppError::ProviderResponse);
    }
    let response = response
        .json::<PaymentIntentWire>()
        .await
        .map_err(|_| ReferenceAppError::ProviderResponse)?;
    let observed = observed_payment_intent_with_statuses(
        response,
        &operation,
        &["requires_confirmation", "succeeded"],
    )?;
    if observed.id() != expected.id() {
        return Err(ReferenceAppError::ProviderResponse);
    }
    Ok(observed)
}

async fn recover_payment_intent_by_operation(
    client: &reqwest::Client,
    fixture_base_url: &str,
    operation: &CheckoutOperation,
) -> Result<Option<ObservedPaymentIntent>, ReferenceAppError> {
    let mut endpoint = reqwest::Url::parse(&format!(
        "{}/v1/payment_intents/search",
        fixture_base_url.trim_end_matches('/')
    ))
    .map_err(|_| ReferenceAppError::ProviderResponse)?;
    endpoint
        .query_pairs_mut()
        .append_pair("operation_id", operation.operation_id());
    let response = client
        .get(endpoint)
        .send()
        .await
        .map_err(|_| ReferenceAppError::ProviderTransport)?;
    if response.status() != StatusCode::OK {
        return Err(ReferenceAppError::ProviderResponse);
    }
    let response = response
        .json::<PaymentIntentListWire>()
        .await
        .map_err(|_| ReferenceAppError::ProviderResponse)?;
    if response.object != "list" || response.has_more || response.data.len() > 1 {
        return Err(ReferenceAppError::ProviderResponse);
    }
    response
        .data
        .into_iter()
        .next()
        .map(|payment_intent| observed_payment_intent(payment_intent, operation))
        .transpose()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaymentIntentWire {
    id: String,
    object: String,
    amount: i64,
    currency: String,
    status: String,
    metadata: PaymentIntentMetadataWire,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaymentIntentMetadataWire {
    operation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PaymentIntentListWire {
    object: String,
    data: Vec<PaymentIntentWire>,
    has_more: bool,
}

/// Verifies the fixture's narrow `t=...,v1=...` HMAC header over the exact raw
/// request bytes.
///
/// # Errors
///
/// Returns [`ReferenceAppError::InvalidWebhookSignature`] for malformed or
/// non-matching signatures.
pub fn verify_webhook_signature(
    raw_body: &[u8],
    signature_header: &str,
    secret: &[u8],
) -> Result<i64, ReferenceAppError> {
    if secret.is_empty() {
        return Err(ReferenceAppError::InvalidWebhookSignature);
    }
    let fields = signature_header.split(',').collect::<Vec<_>>();
    let [timestamp, signature] = fields.as_slice() else {
        return Err(ReferenceAppError::InvalidWebhookSignature);
    };
    let timestamp = timestamp
        .strip_prefix("t=")
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or(ReferenceAppError::InvalidWebhookSignature)?;
    let timestamp_seconds =
        u64::try_from(timestamp).map_err(|_| ReferenceAppError::InvalidWebhookSignature)?;
    let now_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ReferenceAppError::InvalidWebhookSignature)?
        .as_secs();
    if now_seconds.abs_diff(timestamp_seconds) > WEBHOOK_TIMESTAMP_TOLERANCE_SECONDS {
        return Err(ReferenceAppError::InvalidWebhookSignature);
    }
    let signature = signature
        .strip_prefix("v1=")
        .and_then(|value| hex::decode(value).ok())
        .ok_or(ReferenceAppError::InvalidWebhookSignature)?;
    let mut verifier = Hmac::<Sha256>::new_from_slice(secret)
        .map_err(|_| ReferenceAppError::InvalidWebhookSignature)?;
    verifier.update(timestamp.to_string().as_bytes());
    verifier.update(b".");
    verifier.update(raw_body);
    verifier
        .verify_slice(&signature)
        .map_err(|_| ReferenceAppError::InvalidWebhookSignature)?;
    Ok(timestamp)
}

/// Verifies and decodes one supported `payment_intent.succeeded` event.
///
/// # Errors
///
/// Returns [`ReferenceAppError::InvalidWebhookSignature`] when authenticity
/// fails, or [`ReferenceAppError::InvalidWebhookEvent`] when the signed bytes
/// are outside the narrow reference contract.
pub fn parse_succeeded_webhook(
    raw_body: &[u8],
    signature_header: &str,
    secret: &[u8],
) -> Result<ObservedPaymentIntent, ReferenceAppError> {
    parse_succeeded_webhook_event(raw_body, signature_header, secret)
        .map(ObservedWebhookEvent::into_payment_intent)
}

/// Verifies and decodes one supported event while preserving its immutable
/// provider event identity for durable deduplication.
///
/// # Errors
///
/// Returns [`ReferenceAppError::InvalidWebhookSignature`] when authenticity
/// fails, or [`ReferenceAppError::InvalidWebhookEvent`] when the signed bytes
/// are outside the narrow reference contract.
pub fn parse_succeeded_webhook_event(
    raw_body: &[u8],
    signature_header: &str,
    secret: &[u8],
) -> Result<ObservedWebhookEvent, ReferenceAppError> {
    verify_webhook_signature(raw_body, signature_header, secret)?;
    let event = serde_json::from_slice::<ProviderEventWire>(raw_body)
        .map_err(|_| ReferenceAppError::InvalidWebhookEvent)?;
    let payment_intent = event.data.object;
    if event.object != "event"
        || event.event_type != "payment_intent.succeeded"
        || !event.id.starts_with("evt_tiv_")
        || payment_intent.object != "payment_intent"
        || !payment_intent.id.starts_with("pi_tiv_")
        || payment_intent.amount <= 0
        || payment_intent.currency.len() != 3
        || !payment_intent
            .currency
            .bytes()
            .all(|byte| byte.is_ascii_lowercase())
        || payment_intent.status != "succeeded"
        || CheckoutOperation::new(
            &payment_intent.metadata.operation_id,
            payment_intent.amount,
            &payment_intent.currency,
        )
        .is_err()
    {
        return Err(ReferenceAppError::InvalidWebhookEvent);
    }
    Ok(ObservedWebhookEvent {
        id: event.id,
        payment_intent: ObservedPaymentIntent {
            id: payment_intent.id,
            operation_id: payment_intent.metadata.operation_id,
            amount_minor: payment_intent.amount,
            currency: payment_intent.currency,
            status: payment_intent.status,
        },
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderEventWire {
    id: String,
    object: String,
    #[serde(rename = "type")]
    event_type: String,
    data: ProviderEventDataWire,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderEventDataWire {
    object: PaymentIntentWire,
}

const MAX_APP_REQUEST_BODY_BYTES: usize = 16 * 1024;
const CONTROL_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const RECONCILIATION_POLL_INTERVAL: Duration = Duration::from_millis(10);
const RECONCILIATION_HORIZON: Duration = Duration::from_secs(5);
const WEBHOOK_TIMESTAMP_TOLERANCE_SECONDS: u64 = 300;
const WEBHOOK_DELIVERY_IDENTITY_CONSTRAINT: &str = "webhook_deliveries_event_identity_fkey";

type AppResponseBody = Full<Bytes>;

/// Runtime configuration for the intentionally faulty synthetic application.
/// Secret fields intentionally have no printable representation.
pub struct ReferenceAppConfig {
    fixture_base_url: String,
    postgres_host: String,
    postgres_port: u16,
    postgres_role: String,
    postgres_password: String,
    webhook_secret: Vec<u8>,
    control_probe_address: String,
    retry_key_mode: RetryKeyMode,
    caller_retry_mode: CallerRetryMode,
    reconciliation_mode: ReconciliationMode,
    webhook_effect_mode: WebhookEffectMode,
    ledger_balance_mode: LedgerBalanceMode,
}

impl ReferenceAppConfig {
    /// Builds the bounded reference-app configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceAppError::InvalidConfiguration`] when any required
    /// endpoint, role, or secret is absent or malformed.
    pub fn new(
        fixture_base_url: impl Into<String>,
        postgres_host: impl Into<String>,
        postgres_port: u16,
        postgres_role: impl Into<String>,
        postgres_password: impl Into<String>,
        webhook_secret: impl AsRef<[u8]>,
        control_probe_address: impl Into<String>,
    ) -> Result<Self, ReferenceAppError> {
        let fixture_base_url = fixture_base_url.into();
        let postgres_host = postgres_host.into();
        let postgres_role = postgres_role.into();
        let postgres_password = postgres_password.into();
        let webhook_secret = webhook_secret.as_ref().to_vec();
        let control_probe_address = control_probe_address.into();
        if !fixture_base_url.starts_with("http://")
            || fixture_base_url.chars().any(char::is_whitespace)
            || postgres_host.trim().is_empty()
            || postgres_port == 0
            || postgres_role.trim().is_empty()
            || postgres_password.is_empty()
            || webhook_secret.is_empty()
            || control_probe_address.trim().is_empty()
        {
            return Err(ReferenceAppError::InvalidConfiguration);
        }
        Ok(Self {
            fixture_base_url,
            postgres_host,
            postgres_port,
            postgres_role,
            postgres_password,
            webhook_secret,
            control_probe_address,
            retry_key_mode: RetryKeyMode::default(),
            caller_retry_mode: CallerRetryMode::default(),
            reconciliation_mode: ReconciliationMode::default(),
            webhook_effect_mode: WebhookEffectMode::default(),
            ledger_balance_mode: LedgerBalanceMode::default(),
        })
    }

    #[must_use]
    pub const fn with_retry_key_mode(mut self, retry_key_mode: RetryKeyMode) -> Self {
        self.retry_key_mode = retry_key_mode;
        self
    }

    #[must_use]
    pub const fn retry_key_mode(&self) -> RetryKeyMode {
        self.retry_key_mode
    }

    #[must_use]
    pub const fn with_caller_retry_mode(mut self, caller_retry_mode: CallerRetryMode) -> Self {
        self.caller_retry_mode = caller_retry_mode;
        self
    }

    #[must_use]
    pub const fn caller_retry_mode(&self) -> CallerRetryMode {
        self.caller_retry_mode
    }

    #[must_use]
    pub const fn with_reconciliation_mode(
        mut self,
        reconciliation_mode: ReconciliationMode,
    ) -> Self {
        self.reconciliation_mode = reconciliation_mode;
        self
    }

    #[must_use]
    pub const fn reconciliation_mode(&self) -> ReconciliationMode {
        self.reconciliation_mode
    }

    #[must_use]
    pub const fn with_webhook_effect_mode(
        mut self,
        webhook_effect_mode: WebhookEffectMode,
    ) -> Self {
        self.webhook_effect_mode = webhook_effect_mode;
        self
    }

    #[must_use]
    pub const fn webhook_effect_mode(&self) -> WebhookEffectMode {
        self.webhook_effect_mode
    }

    #[must_use]
    pub const fn with_ledger_balance_mode(
        mut self,
        ledger_balance_mode: LedgerBalanceMode,
    ) -> Self {
        self.ledger_balance_mode = ledger_balance_mode;
        self
    }

    #[must_use]
    pub const fn ledger_balance_mode(&self) -> LedgerBalanceMode {
        self.ledger_balance_mode
    }

    /// Rejects a mode tuple whose duplicate-delivery faults have contradictory
    /// precedence.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceAppError::IncompatibleFaultModes`] when both the
    /// duplicate-effect and one-sided-ledger faults are selected.
    pub fn validate_modes(&self) -> Result<(), ReferenceAppError> {
        if self.webhook_effect_mode == WebhookEffectMode::FaultyDuplicateEffect
            && self.ledger_balance_mode == LedgerBalanceMode::FaultyOneSidedOnDuplicate
        {
            Err(ReferenceAppError::IncompatibleFaultModes)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegisteredOperation {
    database: ReferenceDatabaseName,
    operation: CheckoutOperation,
}

/// Shared state for the synthetic known-bug application.
pub struct ReferenceApp {
    config: ReferenceAppConfig,
    http_client: reqwest::Client,
    operations: Mutex<BTreeMap<String, RegisteredOperation>>,
    settled_payment_intents: Arc<Mutex<BTreeSet<String>>>,
}

impl ReferenceApp {
    #[must_use]
    pub fn new(config: ReferenceAppConfig) -> Self {
        Self {
            config,
            http_client: reqwest::Client::new(),
            operations: Mutex::new(BTreeMap::new()),
            settled_payment_intents: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    async fn register_operation(
        &self,
        database: ReferenceDatabaseName,
        operation: CheckoutOperation,
    ) -> Result<(), ReferenceAppError> {
        let key = operation.operation_id().to_owned();
        let registered = RegisteredOperation {
            database,
            operation,
        };
        let mut operations = self.operations.lock().await;
        if let Some(existing) = operations.get(&key) {
            if existing != &registered {
                return Err(ReferenceAppError::OperationConflict);
            }
            return Ok(());
        }
        operations.insert(key, registered);
        Ok(())
    }

    async fn registered_operation(
        &self,
        observed: &ObservedPaymentIntent,
    ) -> Result<RegisteredOperation, ReferenceAppError> {
        let registered = self
            .operations
            .lock()
            .await
            .get(observed.operation_id())
            .cloned();
        let registered = match registered {
            Some(registered) => registered,
            None => self.recover_registered_operation(observed).await?,
        };
        if registered.operation.amount_minor() != observed.amount_minor()
            || registered.operation.currency() != observed.currency()
        {
            return Err(ReferenceAppError::OperationConflict);
        }
        Ok(registered)
    }

    async fn recover_registered_operation(
        &self,
        observed: &ObservedPaymentIntent,
    ) -> Result<RegisteredOperation, ReferenceAppError> {
        let database = ReferenceDatabaseName::from_operation_id(observed.operation_id())
            .map_err(|_| ReferenceAppError::UnknownOperation)?;
        let (client, connection) = self.connect_database(&database).await?;
        let row = client
            .query_opt(
                "SELECT amount_minor, currency FROM orders WHERE operation_id = $1",
                &[&observed.operation_id()],
            )
            .await;
        drop(client);
        let connection_result = connection.await;
        let row = row.map_err(|_| ReferenceAppError::Database)?;
        connection_result
            .map_err(|_| ReferenceAppError::Database)?
            .map_err(|_| ReferenceAppError::Database)?;
        let row = row.ok_or(ReferenceAppError::UnknownOperation)?;
        let amount_minor = row.get::<_, i64>(0);
        let currency = row.get::<_, String>(1);
        if amount_minor != observed.amount_minor() || currency != observed.currency() {
            return Err(ReferenceAppError::OperationConflict);
        }
        let operation = CheckoutOperation::new(observed.operation_id(), amount_minor, currency)?;
        self.register_operation(database.clone(), operation.clone())
            .await?;
        Ok(RegisteredOperation {
            database,
            operation,
        })
    }

    async fn persist_checkout(
        &self,
        database: &ReferenceDatabaseName,
        payment_intent: &ObservedPaymentIntent,
    ) -> Result<(), ReferenceAppError> {
        let (client, connection) = self.connect_database(database).await?;
        let result = client
            .execute(
                "INSERT INTO payments \
                     (operation_id, stripe_payment_intent_id, amount_minor, currency, status) \
                 VALUES ($1, $2, $3, $4, 'pending') \
                 ON CONFLICT DO NOTHING",
                &[
                    &payment_intent.operation_id(),
                    &payment_intent.id(),
                    &payment_intent.amount_minor(),
                    &payment_intent.currency(),
                ],
            )
            .await;
        drop(client);
        let connection_result = connection.await;
        let inserted = result.map_err(|_| ReferenceAppError::Database)?;
        connection_result
            .map_err(|_| ReferenceAppError::Database)?
            .map_err(|_| ReferenceAppError::Database)?;
        if inserted == 1 {
            self.settled_payment_intents
                .lock()
                .await
                .remove(payment_intent.id());
        }
        Ok(())
    }

    async fn start_reconciliation(
        &self,
        database: &ReferenceDatabaseName,
        payment_intent: &ObservedPaymentIntent,
    ) -> Result<(), ReferenceAppError> {
        let (database_client, connection) = self.connect_database(database).await?;
        let http_client = self.http_client.clone();
        let fixture_base_url = self.config.fixture_base_url.clone();
        let payment_intent = payment_intent.clone();
        let reconciliation_mode = self.config.reconciliation_mode;
        let settled_payment_intents = Arc::clone(&self.settled_payment_intents);
        std::mem::drop(tokio::spawn(async move {
            let _result = reconcile_payment_until_horizon(
                &http_client,
                &fixture_base_url,
                &database_client,
                &payment_intent,
                reconciliation_mode,
                &settled_payment_intents,
            )
            .await;
            drop(database_client);
            let _connection_result = connection.await;
        }));
        Ok(())
    }

    async fn persist_webhook(
        &self,
        database: &ReferenceDatabaseName,
        event: &ObservedWebhookEvent,
    ) -> Result<(), ReferenceAppError> {
        let payment_intent = event.payment_intent();
        let delivery_id = Uuid::new_v4();
        let (mut client, connection) = self.connect_database(database).await?;
        let result = async {
            let transaction = client.transaction().await?;
            let first_processing = transaction
                .execute(
                    "INSERT INTO processed_webhook_events \
                     (provider_event_id, operation_id) \
                     VALUES ($1, $2) \
                     ON CONFLICT DO NOTHING",
                    &[&event.id(), &payment_intent.operation_id()],
                )
                .await?
                == 1;
            transaction
                .execute(
                    "INSERT INTO webhook_deliveries \
                         (delivery_id, provider_event_id, operation_id) \
                     VALUES ($1, $2, $3)",
                    &[
                        &delivery_id,
                        &event.id(),
                        &payment_intent.operation_id(),
                    ],
                )
                .await?;
            if first_processing {
                let updated = transaction
                    .execute(
                        "UPDATE payments SET status = 'succeeded' \
                         WHERE operation_id = $1 AND stripe_payment_intent_id = $2",
                        &[&payment_intent.operation_id(), &payment_intent.id()],
                    )
                    .await?;
                if updated == 0 {
                    transaction
                        .execute(
                            "INSERT INTO payments \
                                 (operation_id, stripe_payment_intent_id, amount_minor, currency, status) \
                             VALUES ($1, $2, $3, $4, 'succeeded')",
                            &[
                                &payment_intent.operation_id(),
                                &payment_intent.id(),
                                &payment_intent.amount_minor(),
                                &payment_intent.currency(),
                            ],
                        )
                        .await?;
                }
            }
            let applies_effect = first_processing
                || self.config.webhook_effect_mode == WebhookEffectMode::FaultyDuplicateEffect;
            if applies_effect {
                transaction
                    .execute(
                        "INSERT INTO webhook_effects (provider_event_id, operation_id) \
                         VALUES ($1, $2)",
                        &[&event.id(), &payment_intent.operation_id()],
                    )
                    .await?;
                persist_ledger_entry(&transaction, delivery_id, event, true).await?;
            } else if self.config.ledger_balance_mode
                == LedgerBalanceMode::FaultyOneSidedOnDuplicate
            {
                persist_ledger_entry(&transaction, delivery_id, event, false).await?;
            }
            transaction.commit().await
        }
        .await;
        drop(client);
        let connection_result = connection.await;
        result.map_err(|error| classify_webhook_database_error(&error))?;
        connection_result
            .map_err(|_| ReferenceAppError::Database)?
            .map_err(|_| ReferenceAppError::Database)?;
        self.settled_payment_intents
            .lock()
            .await
            .insert(payment_intent.id().to_owned());
        Ok(())
    }

    async fn connect_database(
        &self,
        database: &ReferenceDatabaseName,
    ) -> Result<DatabaseConnection, ReferenceAppError> {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&self.config.postgres_host)
            .port(self.config.postgres_port)
            .user(&self.config.postgres_role)
            .password(&self.config.postgres_password)
            .dbname(database.as_str());
        let (client, connection) = config
            .connect(NoTls)
            .await
            .map_err(|_| ReferenceAppError::Database)?;
        Ok((client, tokio::spawn(connection)))
    }

    async fn control_listener_is_reachable(&self) -> bool {
        timeout(
            CONTROL_PROBE_TIMEOUT,
            TcpStream::connect(&self.config.control_probe_address),
        )
        .await
        .is_ok_and(|result| result.is_ok())
    }
}

async fn reconcile_payment_until_horizon(
    http_client: &reqwest::Client,
    fixture_base_url: &str,
    database_client: &Client,
    expected: &ObservedPaymentIntent,
    reconciliation_mode: ReconciliationMode,
    settled_payment_intents: &Mutex<BTreeSet<String>>,
) -> Result<(), ReferenceAppError> {
    let deadline = tokio::time::Instant::now() + RECONCILIATION_HORIZON;
    loop {
        if settled_payment_intents.lock().await.contains(expected.id()) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(());
        }
        let Ok(observed) = tokio::time::timeout_at(
            deadline,
            retrieve_payment_intent_for_reconciliation(http_client, fixture_base_url, expected),
        )
        .await
        else {
            return Ok(());
        };
        if let Ok(observed) = observed
            && observed.status() == "succeeded"
            && reconciliation_mode == ReconciliationMode::RepairedProviderReconcile
        {
            let updated = database_client
                .execute(
                    "UPDATE payments SET status = 'succeeded' \
                     WHERE operation_id = $1 \
                       AND stripe_payment_intent_id = $2",
                    &[&observed.operation_id(), &observed.id()],
                )
                .await
                .map_err(|_| ReferenceAppError::Database)?;
            if updated != 1 {
                return Err(ReferenceAppError::Database);
            }
            settled_payment_intents
                .lock()
                .await
                .insert(observed.id().to_owned());
            return Ok(());
        }
        tokio::time::sleep(RECONCILIATION_POLL_INTERVAL).await;
    }
}

async fn persist_ledger_entry(
    transaction: &Transaction<'_>,
    delivery_id: Uuid,
    event: &ObservedWebhookEvent,
    include_credit: bool,
) -> Result<(), tokio_postgres::Error> {
    let entry_id = Uuid::new_v4();
    let payment_intent = event.payment_intent();
    transaction
        .execute(
            "INSERT INTO ledger_entries \
                 (entry_id, delivery_id, provider_event_id, operation_id, \
                  amount_minor, currency, entry_kind) \
             VALUES ($1, $2, $3, $4, $5, $6, 'payment_succeeded')",
            &[
                &entry_id,
                &delivery_id,
                &event.id(),
                &payment_intent.operation_id(),
                &payment_intent.amount_minor(),
                &payment_intent.currency(),
            ],
        )
        .await?;
    transaction
        .execute(
            "INSERT INTO ledger_postings \
                 (entry_id, account_code, entry_side, amount_minor, currency) \
             VALUES ($1, 'processor_clearing', 'debit', $2, $3)",
            &[
                &entry_id,
                &payment_intent.amount_minor(),
                &payment_intent.currency(),
            ],
        )
        .await?;
    if include_credit {
        transaction
            .execute(
                "INSERT INTO ledger_postings \
                     (entry_id, account_code, entry_side, amount_minor, currency) \
                 VALUES ($1, 'order_payment_liability', 'credit', $2, $3)",
                &[
                    &entry_id,
                    &payment_intent.amount_minor(),
                    &payment_intent.currency(),
                ],
            )
            .await?;
    }
    Ok(())
}

type DatabaseConnection = (
    Client,
    tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
);

/// Serves one HTTP/1 connection for the synthetic reference application.
///
/// # Errors
///
/// Returns Hyper's connection error for an invalid or disconnected HTTP
/// stream.
pub async fn serve_http1_connection(
    stream: TcpStream,
    app: Arc<ReferenceApp>,
) -> Result<(), hyper::Error> {
    let service = service_fn(move |request| handle_app_request(request, Arc::clone(&app)));
    http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
}

async fn handle_app_request(
    request: Request<Incoming>,
    app: Arc<ReferenceApp>,
) -> Result<Response<AppResponseBody>, AppHttpError> {
    if provider_proxy_payment_intent_id(request.method(), request.uri().path()).is_some() {
        return handle_provider_proxy(request, &app).await;
    }
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/health") => json_response(
            StatusCode::OK,
            &serde_json::json!({
                "status": "ok",
                "retry_key_mode": app.config.retry_key_mode.as_str(),
                "caller_retry_mode": app.config.caller_retry_mode.as_str(),
                "reconciliation_mode": app.config.reconciliation_mode.as_str(),
                "webhook_effect_mode": app.config.webhook_effect_mode.as_str(),
                "ledger_balance_mode": app.config.ledger_balance_mode.as_str(),
            }),
        ),
        (&Method::GET, "/probe-fixture-control") => {
            let reachable = app.control_listener_is_reachable().await;
            json_response(StatusCode::OK, &serde_json::json!({"reachable": reachable}))
        }
        (&Method::POST, "/checkout") => handle_checkout(request, &app).await,
        (&Method::POST, "/webhooks/stripe") => handle_webhook(request, &app).await,
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found")),
    }
}

async fn handle_provider_proxy(
    request: Request<Incoming>,
    app: &ReferenceApp,
) -> Result<Response<AppResponseBody>, AppHttpError> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    if request.uri().query().is_some() {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "invalid provider request",
        ));
    }
    if method == Method::POST
        && request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            != Some("application/x-www-form-urlencoded")
    {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "invalid provider request",
        ));
    }
    let body = Limited::new(request.into_body(), MAX_APP_REQUEST_BODY_BYTES)
        .collect()
        .await
        .map_err(|_| AppHttpError::ProviderResponse)?
        .to_bytes();
    if !body.is_empty() {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "invalid provider request",
        ));
    }
    let endpoint = format!(
        "{}{path}",
        app.config.fixture_base_url.trim_end_matches('/')
    );
    let upstream = app
        .http_client
        .request(method, endpoint)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|_| AppHttpError::ProviderTransport)?;
    let status = upstream.status();
    let content_type = upstream.headers().get(CONTENT_TYPE).cloned();
    let body = upstream
        .bytes()
        .await
        .map_err(|_| AppHttpError::ProviderResponse)?;
    if body.len() > MAX_APP_REQUEST_BODY_BYTES {
        return Err(AppHttpError::ProviderResponse);
    }
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response.headers_mut().insert(CONTENT_TYPE, content_type);
    }
    Ok(response)
}

fn valid_payment_intent_id(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("pi_tiv_") else {
        return false;
    };
    !suffix.is_empty()
        && suffix.len() <= 255
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn provider_proxy_payment_intent_id<'a>(method: &Method, path: &'a str) -> Option<&'a str> {
    let path = path.strip_prefix("/v1/payment_intents/")?;
    let payment_intent_id = match *method {
        Method::GET if !path.contains('/') => path,
        Method::POST => path.strip_suffix("/confirm")?,
        _ => return None,
    };
    valid_payment_intent_id(payment_intent_id).then_some(payment_intent_id)
}

async fn handle_checkout(
    request: Request<Incoming>,
    app: &ReferenceApp,
) -> Result<Response<AppResponseBody>, AppHttpError> {
    let Ok(business_request_id) = driver_business_request_id(&request) else {
        return Ok(text_response(StatusCode::BAD_REQUEST, "invalid action id"));
    };
    let Some(command) = decode_json::<CheckoutRequest>(request).await else {
        return Ok(text_response(StatusCode::BAD_REQUEST, "invalid checkout"));
    };
    let Ok(database) = ReferenceDatabaseName::parse(command.database) else {
        return Ok(text_response(StatusCode::BAD_REQUEST, "invalid checkout"));
    };
    let Ok(operation) =
        CheckoutOperation::new(command.operation_id, command.amount_minor, command.currency)
    else {
        return Ok(text_response(StatusCode::BAD_REQUEST, "invalid checkout"));
    };
    if operation.operation_id() != database.operation_id() {
        return Ok(text_response(StatusCode::BAD_REQUEST, "invalid checkout"));
    }
    if app
        .register_operation(database.clone(), operation.clone())
        .await
        .is_err()
    {
        return Ok(text_response(
            StatusCode::CONFLICT,
            "operation registration conflict",
        ));
    }
    let payment_intent = match business_request_id {
        Some(business_request_id) => {
            let recovered =
                if app.config.caller_retry_mode == CallerRetryMode::RepairedRecoverOperation {
                    recover_payment_intent_by_operation(
                        &app.http_client,
                        &app.config.fixture_base_url,
                        &operation,
                    )
                    .await
                } else {
                    Ok(None)
                };
            match recovered {
                Ok(Some(payment_intent)) => Ok(payment_intent),
                Ok(None) => {
                    create_with_retry_key_mode_for_business_request(
                        &app.http_client,
                        &app.config.fixture_base_url,
                        &operation,
                        business_request_id,
                        app.config.retry_key_mode,
                    )
                    .await
                }
                Err(error) => Err(error),
            }
        }
        None => {
            create_with_retry_key_mode(
                &app.http_client,
                &app.config.fixture_base_url,
                &operation,
                app.config.retry_key_mode,
            )
            .await
        }
    };
    let Ok(payment_intent) = payment_intent else {
        return Ok(text_response(StatusCode::BAD_GATEWAY, "provider failure"));
    };
    if app
        .persist_checkout(&database, &payment_intent)
        .await
        .is_err()
    {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "database failure",
        ));
    }
    if app
        .start_reconciliation(&database, &payment_intent)
        .await
        .is_err()
    {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reconciliation failure",
        ));
    }
    json_response(
        StatusCode::OK,
        &CheckoutResponse {
            payment_intent_id: payment_intent.id(),
            operation_id: payment_intent.operation_id(),
        },
    )
}

fn driver_business_request_id<B>(request: &Request<B>) -> Result<Option<u32>, ()> {
    let Some(value) = request.headers().get(DRIVER_ACTION_ID_HEADER) else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or(())
}

async fn handle_webhook(
    request: Request<Incoming>,
    app: &ReferenceApp,
) -> Result<Response<AppResponseBody>, AppHttpError> {
    let signature = request
        .headers()
        .get("stripe-signature")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let Some(signature) = signature else {
        return Ok(text_response(StatusCode::UNAUTHORIZED, "invalid signature"));
    };
    let Ok(ingress_capability) = webhook_ingress_capability(&request) else {
        return Ok(text_response(
            StatusCode::BAD_REQUEST,
            "invalid ingress capability",
        ));
    };
    let raw_body = match Limited::new(request.into_body(), MAX_APP_REQUEST_BODY_BYTES)
        .collect()
        .await
    {
        Ok(body) => body.to_bytes(),
        Err(_) => {
            return Ok(text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body too large",
            ));
        }
    };
    let event =
        match parse_succeeded_webhook_event(&raw_body, &signature, &app.config.webhook_secret) {
            Ok(event) => event,
            Err(ReferenceAppError::InvalidWebhookSignature) => {
                return Ok(text_response(StatusCode::UNAUTHORIZED, "invalid signature"));
            }
            Err(_) => return Ok(text_response(StatusCode::BAD_REQUEST, "invalid event")),
        };
    let payment_intent = event.payment_intent();
    let registered = match app.registered_operation(payment_intent).await {
        Ok(registered) => registered,
        Err(ReferenceAppError::UnknownOperation) => {
            return Ok(text_response(StatusCode::CONFLICT, "unknown operation"));
        }
        Err(_) => return Ok(text_response(StatusCode::BAD_REQUEST, "event mismatch")),
    };
    if let Some(capability) = ingress_capability {
        let callback = app
            .http_client
            .post(format!(
                "{}/v1/tiv/webhook-request-forwarded",
                app.config.fixture_base_url.trim_end_matches('/')
            ))
            .header(WEBHOOK_INGRESS_CAPABILITY_HEADER, capability)
            .body("")
            .send()
            .await;
        if !callback.is_ok_and(|response| response.status() == StatusCode::NO_CONTENT) {
            return Ok(text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "webhook ingress cancelled",
            ));
        }
    }
    match app.persist_webhook(&registered.database, &event).await {
        Ok(()) => {}
        Err(ReferenceAppError::WebhookIdentityConflict) => {
            return Ok(text_response(
                StatusCode::CONFLICT,
                "webhook identity conflict",
            ));
        }
        Err(_) => {
            return Ok(text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "database failure",
            ));
        }
    }
    json_response(StatusCode::OK, &serde_json::json!({"accepted": true}))
}

fn webhook_ingress_capability<B>(request: &Request<B>) -> Result<Option<String>, ()> {
    let Some(value) = request.headers().get(WEBHOOK_INGRESS_CAPABILITY_HEADER) else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .filter(|value| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .map(str::to_owned)
        .map(Some)
        .ok_or(())
}

async fn decode_json<T>(request: Request<Incoming>) -> Option<T>
where
    T: for<'de> Deserialize<'de>,
{
    if request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/json")
    {
        return None;
    }
    Limited::new(request.into_body(), MAX_APP_REQUEST_BODY_BYTES)
        .collect()
        .await
        .ok()
        .and_then(|body| serde_json::from_slice(&body.to_bytes()).ok())
}

fn json_response<T>(
    status: StatusCode,
    value: &T,
) -> Result<Response<AppResponseBody>, AppHttpError>
where
    T: Serialize,
{
    let body = serde_json::to_vec(value).map_err(|_| AppHttpError::Serialization)?;
    let mut response = text_response(status, body);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(response)
}

fn text_response(status: StatusCode, body: impl Into<Bytes>) -> Response<AppResponseBody> {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = status;
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutRequest {
    database: String,
    operation_id: String,
    amount_minor: i64,
    currency: String,
}

#[derive(Serialize)]
struct CheckoutResponse<'a> {
    payment_intent_id: &'a str,
    operation_id: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppHttpError {
    Serialization,
    ProviderTransport,
    ProviderResponse,
}

fn classify_webhook_database_error(error: &tokio_postgres::Error) -> ReferenceAppError {
    if error
        .as_db_error()
        .and_then(tokio_postgres::error::DbError::constraint)
        == Some(WEBHOOK_DELIVERY_IDENTITY_CONSTRAINT)
    {
        ReferenceAppError::WebhookIdentityConflict
    } else {
        ReferenceAppError::Database
    }
}

impl fmt::Display for AppHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Serialization => "reference app response serialization failed",
            Self::ProviderTransport => "reference app provider transport failed",
            Self::ProviderResponse => "reference app provider response failed",
        })
    }
}

impl Error for AppHttpError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceAppError {
    Database,
    IncompatibleFaultModes,
    InvalidDatabaseName,
    InvalidConfiguration,
    InvalidOperation,
    InvalidWebhookEvent,
    InvalidWebhookSignature,
    OperationConflict,
    ProviderResponse,
    ProviderTransport,
    UnknownOperation,
    WebhookIdentityConflict,
}

impl fmt::Display for ReferenceAppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Database => "reference database operation failed",
            Self::IncompatibleFaultModes => "reference application fault modes are incompatible",
            Self::InvalidDatabaseName => "invalid generated reference database name",
            Self::InvalidConfiguration => "invalid reference application configuration",
            Self::InvalidOperation => "invalid checkout operation",
            Self::InvalidWebhookEvent => "invalid webhook event",
            Self::InvalidWebhookSignature => "invalid webhook signature",
            Self::OperationConflict => "operation registration conflicts with prior state",
            Self::ProviderResponse => "invalid provider response",
            Self::ProviderTransport => "provider transport failed",
            Self::UnknownOperation => "webhook operation is not registered",
            Self::WebhookIdentityConflict => {
                "provider event identity conflicts with prior processing"
            }
        };
        formatter.write_str(message)
    }
}

impl Error for ReferenceAppError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_app_config_defaults_to_the_fault_and_can_select_the_repaired_control() {
        let config = ReferenceAppConfig::new(
            "http://127.0.0.1:12111",
            "127.0.0.1",
            5_432,
            "tiv_app",
            "synthetic-app-password",
            "whsec_test_secret",
            "127.0.0.1:12112",
        )
        .expect("the synthetic config is valid");
        assert_eq!(config.retry_key_mode(), RetryKeyMode::FaultyChangedKey);
        assert_eq!(
            config.caller_retry_mode(),
            CallerRetryMode::FaultyPerRequest
        );
        assert_eq!(
            config.reconciliation_mode(),
            ReconciliationMode::FaultyWebhookOnly
        );
        assert_eq!(
            config.webhook_effect_mode(),
            WebhookEffectMode::RepairedDeduplicate
        );
        assert_eq!(
            config.ledger_balance_mode(),
            LedgerBalanceMode::RepairedBalancedOnce
        );

        let repaired = config
            .with_retry_key_mode(RetryKeyMode::RepairedSameKey)
            .with_caller_retry_mode(CallerRetryMode::RepairedRecoverOperation)
            .with_reconciliation_mode(ReconciliationMode::RepairedProviderReconcile)
            .with_webhook_effect_mode(WebhookEffectMode::FaultyDuplicateEffect)
            .with_ledger_balance_mode(LedgerBalanceMode::FaultyOneSidedOnDuplicate);
        assert_eq!(repaired.retry_key_mode(), RetryKeyMode::RepairedSameKey);
        assert_eq!(
            repaired.caller_retry_mode(),
            CallerRetryMode::RepairedRecoverOperation
        );
        assert_eq!(
            repaired.reconciliation_mode(),
            ReconciliationMode::RepairedProviderReconcile
        );
        assert_eq!(
            repaired.webhook_effect_mode(),
            WebhookEffectMode::FaultyDuplicateEffect
        );
        assert_eq!(
            repaired.ledger_balance_mode(),
            LedgerBalanceMode::FaultyOneSidedOnDuplicate
        );
    }

    #[test]
    fn driver_action_identity_is_optional_but_strict_when_present() {
        let absent = Request::new(());
        assert_eq!(driver_business_request_id(&absent), Ok(None));

        let valid = Request::builder()
            .header(DRIVER_ACTION_ID_HEADER, "17")
            .body(())
            .unwrap();
        assert_eq!(driver_business_request_id(&valid), Ok(Some(17)));

        for invalid in ["0", "-1", "1.0", "action-1", " 1"] {
            let request = Request::builder()
                .header(DRIVER_ACTION_ID_HEADER, invalid)
                .body(())
                .unwrap();
            assert_eq!(driver_business_request_id(&request), Err(()));
        }
    }

    #[test]
    fn webhook_ingress_capability_is_optional_but_strict_when_present() {
        let absent = Request::new(());
        assert_eq!(webhook_ingress_capability(&absent), Ok(None));

        let capability = "a".repeat(64);
        let valid = Request::builder()
            .header(WEBHOOK_INGRESS_CAPABILITY_HEADER, &capability)
            .body(())
            .unwrap();
        assert_eq!(webhook_ingress_capability(&valid), Ok(Some(capability)));

        for invalid in ["", "abc", &"A".repeat(64), &"g".repeat(64), &"a".repeat(65)] {
            let request = Request::builder()
                .header(WEBHOOK_INGRESS_CAPABILITY_HEADER, invalid)
                .body(())
                .unwrap();
            assert_eq!(webhook_ingress_capability(&request), Err(()));
        }
    }

    #[tokio::test]
    async fn a_fresh_process_routes_a_case_derived_webhook_to_durable_state() {
        let app = ReferenceApp::new(
            ReferenceAppConfig::new(
                "http://127.0.0.1:1",
                "127.0.0.1",
                1,
                "tiv_app",
                "synthetic-app-password",
                "whsec_test_secret",
                "127.0.0.1:1",
            )
            .expect("the synthetic config is valid"),
        );
        let observed = ObservedPaymentIntent {
            id: "pi_tiv_restart".to_owned(),
            operation_id: "op_0123456789abcdef".to_owned(),
            amount_minor: 2_500,
            currency: "usd".to_owned(),
            status: "succeeded".to_owned(),
        };

        assert_eq!(
            app.registered_operation(&observed).await,
            Err(ReferenceAppError::Database),
            "a valid durable identity must reach PostgreSQL instead of being rejected as unknown RAM"
        );
    }
}
