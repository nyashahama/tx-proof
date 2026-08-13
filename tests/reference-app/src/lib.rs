use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
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
use tokio_postgres::{Client, NoTls};

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
    let endpoint = format!(
        "{}/v1/payment_intents",
        fixture_base_url.trim_end_matches('/')
    );
    let first_key = format!("{}-attempt-1", operation.operation_id());
    let first = send_create(client, &endpoint, &first_key, operation).await;
    if let Ok(response) = first {
        decode_provider_response(response, operation).await
    } else {
        let retry_key = format!("{}-attempt-2", operation.operation_id());
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
    if response.object != "payment_intent"
        || !response.id.starts_with("pi_tiv_")
        || response.amount != operation.amount_minor()
        || response.currency != operation.currency()
        || response.status != "requires_confirmation"
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
    Ok(ObservedPaymentIntent {
        id: payment_intent.id,
        operation_id: payment_intent.metadata.operation_id,
        amount_minor: payment_intent.amount,
        currency: payment_intent.currency,
        status: payment_intent.status,
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
const WEBHOOK_TIMESTAMP_TOLERANCE_SECONDS: u64 = 300;

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
        })
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
}

impl ReferenceApp {
    #[must_use]
    pub fn new(config: ReferenceAppConfig) -> Self {
        Self {
            config,
            http_client: reqwest::Client::new(),
            operations: Mutex::new(BTreeMap::new()),
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
            .cloned()
            .ok_or(ReferenceAppError::UnknownOperation)?;
        if registered.operation.amount_minor() != observed.amount_minor()
            || registered.operation.currency() != observed.currency()
        {
            return Err(ReferenceAppError::OperationConflict);
        }
        Ok(registered)
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
                 VALUES ($1, $2, $3, $4, 'pending')",
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
        result.map_err(|_| ReferenceAppError::Database)?;
        connection_result
            .map_err(|_| ReferenceAppError::Database)?
            .map_err(|_| ReferenceAppError::Database)?;
        Ok(())
    }

    async fn persist_webhook(
        &self,
        database: &ReferenceDatabaseName,
        payment_intent: &ObservedPaymentIntent,
    ) -> Result<(), ReferenceAppError> {
        let (mut client, connection) = self.connect_database(database).await?;
        let result = async {
            let transaction = client.transaction().await?;
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
            transaction.commit().await
        }
        .await;
        drop(client);
        let connection_result = connection.await;
        result.map_err(|_| ReferenceAppError::Database)?;
        connection_result
            .map_err(|_| ReferenceAppError::Database)?
            .map_err(|_| ReferenceAppError::Database)?;
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
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/health") => {
            json_response(StatusCode::OK, &serde_json::json!({"status": "ok"}))
        }
        (&Method::GET, "/probe-fixture-control") => {
            let reachable = app.control_listener_is_reachable().await;
            json_response(StatusCode::OK, &serde_json::json!({"reachable": reachable}))
        }
        (&Method::POST, "/checkout") => handle_checkout(request, &app).await,
        (&Method::POST, "/webhooks/stripe") => handle_webhook(request, &app).await,
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found")),
    }
}

async fn handle_checkout(
    request: Request<Incoming>,
    app: &ReferenceApp,
) -> Result<Response<AppResponseBody>, AppHttpError> {
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
    let Ok(payment_intent) =
        create_with_changed_retry_key(&app.http_client, &app.config.fixture_base_url, &operation)
            .await
    else {
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
    json_response(
        StatusCode::OK,
        &CheckoutResponse {
            payment_intent_id: payment_intent.id(),
            operation_id: payment_intent.operation_id(),
        },
    )
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
    let payment_intent =
        match parse_succeeded_webhook(&raw_body, &signature, &app.config.webhook_secret) {
            Ok(payment_intent) => payment_intent,
            Err(ReferenceAppError::InvalidWebhookSignature) => {
                return Ok(text_response(StatusCode::UNAUTHORIZED, "invalid signature"));
            }
            Err(_) => return Ok(text_response(StatusCode::BAD_REQUEST, "invalid event")),
        };
    let registered = match app.registered_operation(&payment_intent).await {
        Ok(registered) => registered,
        Err(ReferenceAppError::UnknownOperation) => {
            return Ok(text_response(StatusCode::CONFLICT, "unknown operation"));
        }
        Err(_) => return Ok(text_response(StatusCode::BAD_REQUEST, "event mismatch")),
    };
    if app
        .persist_webhook(&registered.database, &payment_intent)
        .await
        .is_err()
    {
        return Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "database failure",
        ));
    }
    json_response(StatusCode::OK, &serde_json::json!({"accepted": true}))
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
}

impl fmt::Display for AppHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("reference app response serialization failed")
    }
}

impl Error for AppHttpError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferenceAppError {
    Database,
    InvalidDatabaseName,
    InvalidConfiguration,
    InvalidOperation,
    InvalidWebhookEvent,
    InvalidWebhookSignature,
    OperationConflict,
    ProviderResponse,
    ProviderTransport,
    UnknownOperation,
}

impl fmt::Display for ReferenceAppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Database => "reference database operation failed",
            Self::InvalidDatabaseName => "invalid generated reference database name",
            Self::InvalidConfiguration => "invalid reference application configuration",
            Self::InvalidOperation => "invalid checkout operation",
            Self::InvalidWebhookEvent => "invalid webhook event",
            Self::InvalidWebhookSignature => "invalid webhook signature",
            Self::OperationConflict => "operation registration conflicts with prior state",
            Self::ProviderResponse => "invalid provider response",
            Self::ProviderTransport => "provider transport failed",
            Self::UnknownOperation => "webhook operation is not registered",
        };
        formatter.write_str(message)
    }
}

impl Error for ReferenceAppError {}
