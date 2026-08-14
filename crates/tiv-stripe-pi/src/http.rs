use std::{collections::BTreeMap, error::Error, fmt, sync::Arc};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::{
    Method, Request, Response, StatusCode,
    body::Incoming,
    header::{CONTENT_TYPE, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::{
    net::TcpStream,
    sync::{Mutex, Notify},
};

use crate::{
    CreatePaymentIntent, DataPlaneDisposition, DataPlaneResponse, FaultOutcome, FixtureError,
    FixtureServiceError, HeldDataPlaneResponse, IdempotencyKey, ManagedDataPlaneDisposition,
    ManagedFixture, OperationId, PaymentIntentFixture,
};

const MAX_REQUEST_BODY_BYTES: usize = 8 * 1024;

type ResponseBody = Full<Bytes>;

/// Serves one HTTP/1 connection for the narrow `PaymentIntent` fixture.
///
/// A [`FaultOutcome::CommitThenClose`] request mutates and caches provider
/// state, then returns a service error so Hyper closes the connection without
/// manufacturing an HTTP response.
///
/// # Errors
///
/// Returns Hyper's connection error when the peer disconnects, parsing fails,
/// or the fixture intentionally closes a committed connection.
pub async fn serve_http1_connection(
    stream: TcpStream,
    fixture: Arc<Mutex<PaymentIntentFixture>>,
    outcome: FaultOutcome,
) -> Result<(), hyper::Error> {
    serve_data_plane_connection(stream, DataPlaneBackend::Fixed { fixture, outcome }).await
}

/// Serves one HTTP/1 connection using the fault plan installed through the
/// local control plane.
///
/// Invalid requests do not consume an outcome. A valid create or confirm
/// consumes exactly one planned outcome.
///
/// # Errors
///
/// Returns Hyper's connection error when the peer disconnects, parsing fails,
/// or the fixture intentionally closes a committed connection.
pub async fn serve_managed_http1_connection(
    stream: TcpStream,
    fixture: Arc<Mutex<ManagedFixture>>,
) -> Result<(), hyper::Error> {
    serve_data_plane_connection(stream, DataPlaneBackend::Managed(fixture)).await
}

async fn serve_data_plane_connection(
    stream: TcpStream,
    backend: DataPlaneBackend,
) -> Result<(), hyper::Error> {
    let close_connection = Arc::new(Notify::new());
    let close_signal = Arc::clone(&close_connection);
    let service = service_fn(move |request| {
        handle_request(request, backend.clone(), Arc::clone(&close_signal))
    });
    let connection = http1::Builder::new().serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    tokio::select! {
        result = &mut connection => result,
        () = close_connection.notified() => Ok(()),
    }
}

async fn handle_request(
    request: Request<Incoming>,
    backend: DataPlaneBackend,
    close_connection: Arc<Notify>,
) -> Result<Response<ResponseBody>, FixtureHttpError> {
    if request.method() == Method::GET
        && let Some(payment_intent_id) = retrieve_payment_intent_id(request.uri().path())
    {
        if request.uri().query().is_some() {
            return Ok(response(StatusCode::BAD_REQUEST, "unsupported query"));
        }
        return immediate_provider_result(backend.retrieve(payment_intent_id).await);
    }
    if request.method() == Method::POST
        && let Some(payment_intent_id) = confirm_payment_intent_id(request.uri().path())
    {
        let payment_intent_id = payment_intent_id.to_owned();
        return handle_confirm(request, &backend, &payment_intent_id, &close_connection).await;
    }
    if request.method() != Method::POST || request.uri().path() != "/v1/payment_intents" {
        return Ok(response(StatusCode::NOT_FOUND, b"not found".as_slice()));
    }
    handle_create(request, &backend, &close_connection).await
}

async fn handle_confirm(
    request: Request<Incoming>,
    backend: &DataPlaneBackend,
    payment_intent_id: &str,
    close_connection: &Notify,
) -> Result<Response<ResponseBody>, FixtureHttpError> {
    if request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/x-www-form-urlencoded")
    {
        return Ok(response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported content type",
        ));
    }
    let collected = match Limited::new(request.into_body(), MAX_REQUEST_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Ok(response(StatusCode::PAYLOAD_TOO_LARGE, "body too large")),
    };
    if !collected.is_empty() {
        return Ok(response(
            StatusCode::BAD_REQUEST,
            "unsupported confirm parameters",
        ));
    }
    data_plane_result(backend.confirm(payment_intent_id).await, close_connection).await
}

async fn handle_create(
    request: Request<Incoming>,
    backend: &DataPlaneBackend,
    close_connection: &Notify,
) -> Result<Response<ResponseBody>, FixtureHttpError> {
    let Some(key) = request
        .headers()
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .and_then(|value| IdempotencyKey::new(value).ok())
    else {
        return Ok(response(StatusCode::BAD_REQUEST, "invalid idempotency key"));
    };
    if request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some("application/x-www-form-urlencoded")
    {
        return Ok(response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported content type",
        ));
    }

    let collected = match Limited::new(request.into_body(), MAX_REQUEST_BODY_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Ok(response(StatusCode::PAYLOAD_TOO_LARGE, "body too large")),
    };
    let mut fields = BTreeMap::new();
    for (name, value) in form_urlencoded::parse(&collected) {
        let name = name.into_owned();
        if !matches!(
            name.as_str(),
            "amount" | "currency" | "metadata[operation_id]"
        ) || fields.insert(name, value.into_owned()).is_some()
        {
            return Ok(response(StatusCode::BAD_REQUEST, "invalid form parameters"));
        }
    }
    if !matches!(fields.len(), 2 | 3) {
        return Ok(response(StatusCode::BAD_REQUEST, "invalid form parameters"));
    }
    let Some(amount_minor) = fields
        .get("amount")
        .and_then(|amount| amount.parse::<i64>().ok())
    else {
        return Ok(response(StatusCode::BAD_REQUEST, "invalid amount"));
    };
    let Some(currency) = fields.get("currency") else {
        return Ok(response(StatusCode::BAD_REQUEST, "invalid currency"));
    };
    let Ok(mut create) = CreatePaymentIntent::new(amount_minor, currency) else {
        return Ok(response(StatusCode::BAD_REQUEST, "invalid create request"));
    };
    if let Some(operation_id) = fields.get("metadata[operation_id]") {
        let Ok(operation_id) = OperationId::new(operation_id) else {
            return Ok(response(
                StatusCode::BAD_REQUEST,
                "invalid operation metadata",
            ));
        };
        create = create.with_operation_id(operation_id);
    }

    data_plane_result(
        backend.create_data_plane(key, create).await,
        close_connection,
    )
    .await
}

async fn data_plane_result(
    result: Result<HttpDataPlaneDisposition, DataPlaneExecutionError>,
    close_connection: &Notify,
) -> Result<Response<ResponseBody>, FixtureHttpError> {
    match result {
        Ok(HttpDataPlaneDisposition::Response(provider_response)) => {
            data_plane_response(&provider_response)
        }
        Ok(HttpDataPlaneDisposition::CloseConnection) => {
            close_connection.notify_one();
            std::future::pending().await
        }
        Ok(HttpDataPlaneDisposition::Held(held)) => {
            let provider_response = held
                .wait()
                .await
                .map_err(|_| FixtureHttpError::HeldResponseCancelled)?;
            data_plane_response(&provider_response)
        }
        Ok(HttpDataPlaneDisposition::UnmanagedDelay) => std::future::pending().await,
        Err(error) => Ok(provider_error_response(error)),
    }
}

fn retrieve_payment_intent_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/v1/payment_intents/")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

fn confirm_payment_intent_id(path: &str) -> Option<&str> {
    let id = path
        .strip_prefix("/v1/payment_intents/")?
        .strip_suffix("/confirm")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

#[derive(Clone)]
enum DataPlaneBackend {
    Fixed {
        fixture: Arc<Mutex<PaymentIntentFixture>>,
        outcome: FaultOutcome,
    },
    Managed(Arc<Mutex<ManagedFixture>>),
}

impl DataPlaneBackend {
    async fn create_data_plane(
        &self,
        key: IdempotencyKey,
        request: CreatePaymentIntent,
    ) -> Result<HttpDataPlaneDisposition, DataPlaneExecutionError> {
        match self {
            Self::Fixed { fixture, outcome } => fixture
                .lock()
                .await
                .create_data_plane(key, request, *outcome)
                .map(fixed_disposition)
                .map_err(DataPlaneExecutionError::Fixture),
            Self::Managed(fixture) => fixture
                .lock()
                .await
                .create_data_plane(key, request)
                .map(managed_disposition)
                .map_err(|error| match error {
                    FixtureServiceError::Fixture(error) => DataPlaneExecutionError::Fixture(error),
                    error => DataPlaneExecutionError::Service(error),
                }),
        }
    }

    async fn confirm(
        &self,
        payment_intent_id: &str,
    ) -> Result<HttpDataPlaneDisposition, DataPlaneExecutionError> {
        match self {
            Self::Fixed { fixture, outcome } => fixture
                .lock()
                .await
                .confirm_data_plane(payment_intent_id, *outcome)
                .map(fixed_disposition)
                .map_err(DataPlaneExecutionError::Fixture),
            Self::Managed(fixture) => fixture
                .lock()
                .await
                .confirm_data_plane(payment_intent_id)
                .map(managed_disposition)
                .map_err(|error| match error {
                    FixtureServiceError::Fixture(error) => DataPlaneExecutionError::Fixture(error),
                    error => DataPlaneExecutionError::Service(error),
                }),
        }
    }

    async fn retrieve(
        &self,
        payment_intent_id: &str,
    ) -> Result<DataPlaneResponse, DataPlaneExecutionError> {
        match self {
            Self::Fixed { fixture, .. } => fixture
                .lock()
                .await
                .retrieve_data_plane(payment_intent_id)
                .map_err(DataPlaneExecutionError::Fixture),
            Self::Managed(fixture) => fixture
                .lock()
                .await
                .retrieve_data_plane(payment_intent_id)
                .map_err(DataPlaneExecutionError::Fixture),
        }
    }
}

fn fixed_disposition(disposition: DataPlaneDisposition) -> HttpDataPlaneDisposition {
    match disposition {
        DataPlaneDisposition::Response(response) => HttpDataPlaneDisposition::Response(response),
        DataPlaneDisposition::CloseConnection => HttpDataPlaneDisposition::CloseConnection,
        DataPlaneDisposition::DelayResponse(_) => HttpDataPlaneDisposition::UnmanagedDelay,
    }
}

fn managed_disposition(disposition: ManagedDataPlaneDisposition) -> HttpDataPlaneDisposition {
    match disposition {
        ManagedDataPlaneDisposition::Response(response) => {
            HttpDataPlaneDisposition::Response(response)
        }
        ManagedDataPlaneDisposition::CloseConnection => HttpDataPlaneDisposition::CloseConnection,
        ManagedDataPlaneDisposition::Held(held) => HttpDataPlaneDisposition::Held(held),
    }
}

#[derive(Debug)]
enum HttpDataPlaneDisposition {
    Response(DataPlaneResponse),
    CloseConnection,
    Held(HeldDataPlaneResponse),
    UnmanagedDelay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DataPlaneExecutionError {
    Fixture(FixtureError),
    Service(FixtureServiceError),
}

fn immediate_provider_result(
    result: Result<DataPlaneResponse, DataPlaneExecutionError>,
) -> Result<Response<ResponseBody>, FixtureHttpError> {
    match result {
        Ok(provider_response) => data_plane_response(&provider_response),
        Err(error) => Ok(provider_error_response(error)),
    }
}

fn provider_error_response(error: DataPlaneExecutionError) -> Response<ResponseBody> {
    match error {
        DataPlaneExecutionError::Fixture(FixtureError::IdempotencyConflict) => response(
            StatusCode::CONFLICT,
            "idempotency key conflicts with prior parameters",
        ),
        DataPlaneExecutionError::Fixture(FixtureError::ConnectionClosed) => {
            response(StatusCode::INTERNAL_SERVER_ERROR, "provider error")
        }
        DataPlaneExecutionError::Fixture(FixtureError::RateLimited) => {
            response(StatusCode::TOO_MANY_REQUESTS, "rate limited")
        }
        DataPlaneExecutionError::Fixture(FixtureError::NotFound) => {
            response(StatusCode::NOT_FOUND, "not found")
        }
        DataPlaneExecutionError::Fixture(
            FixtureError::Serialization | FixtureError::ServerError,
        ) => response(StatusCode::INTERNAL_SERVER_ERROR, "provider error"),
        DataPlaneExecutionError::Service(_) => response(
            StatusCode::SERVICE_UNAVAILABLE,
            "fixture fault plan unavailable",
        ),
    }
}

fn data_plane_response(
    provider_response: &DataPlaneResponse,
) -> Result<Response<ResponseBody>, FixtureHttpError> {
    let status = StatusCode::from_u16(provider_response.status_code())
        .map_err(|_| FixtureHttpError::InvalidStatusCode)?;
    let mut response = response(status, provider_response.raw_body().to_vec());
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(provider_response.content_type()),
    );
    Ok(response)
}

fn response(status: StatusCode, body: impl Into<Bytes>) -> Response<ResponseBody> {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = status;
    response
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FixtureHttpError {
    HeldResponseCancelled,
    InvalidStatusCode,
}

impl fmt::Display for FixtureHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HeldResponseCancelled => {
                formatter.write_str("held fixture response was cancelled")
            }
            Self::InvalidStatusCode => {
                formatter.write_str("fixture produced an invalid HTTP status")
            }
        }
    }
}

impl Error for FixtureHttpError {}
