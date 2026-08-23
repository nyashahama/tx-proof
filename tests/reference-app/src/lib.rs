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
    create_with_changed_retry_key_in_scope(
        client,
        fixture_base_url,
        operation,
        operation.operation_id(),
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
    let idempotency_scope = format!(
        "{}-business-{business_request_id}",
        operation.operation_id()
    );
    create_with_changed_retry_key_in_scope(client, fixture_base_url, operation, &idempotency_scope)
        .await
}

async fn create_with_changed_retry_key_in_scope(
    client: &reqwest::Client,
    fixture_base_url: &str,
    operation: &CheckoutOperation,
    idempotency_scope: &str,
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
        let retry_key = format!("{idempotency_scope}-attempt-2");
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
    if provider_proxy_payment_intent_id(request.method(), request.uri().path()).is_some() {
        return handle_provider_proxy(request, &app).await;
    }
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
            create_with_changed_retry_key_for_business_request(
                &app.http_client,
                &app.config.fixture_base_url,
                &operation,
                business_request_id,
            )
            .await
        }
        None => {
            create_with_changed_retry_key(
                &app.http_client,
                &app.config.fixture_base_url,
                &operation,
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

#[cfg(test)]
mod tests {
    use super::*;

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
