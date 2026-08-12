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
    FixtureServiceError, IdempotencyKey, ManagedFixture, OperationId, PaymentIntentFixture,
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
/// Invalid requests do not consume an outcome. A valid create consumes exactly
/// one planned outcome.
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
    if request.method() != Method::POST || request.uri().path() != "/v1/payment_intents" {
        return Ok(response(StatusCode::NOT_FOUND, b"not found".as_slice()));
    }

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

    let result = backend.create_data_plane(key, create).await;
    match result {
        Ok(DataPlaneDisposition::Response(provider_response)) => {
            data_plane_response(&provider_response)
        }
        Ok(DataPlaneDisposition::CloseConnection) => {
            close_connection.notify_one();
            std::future::pending().await
        }
        Err(DataPlaneExecutionError::Fixture(FixtureError::IdempotencyConflict)) => Ok(response(
            StatusCode::CONFLICT,
            "idempotency key conflicts with prior parameters",
        )),
        Err(DataPlaneExecutionError::Fixture(FixtureError::ConnectionClosed)) => {
            unreachable!("the data plane uses a disposition")
        }
        Err(DataPlaneExecutionError::Fixture(FixtureError::RateLimited)) => {
            Ok(response(StatusCode::TOO_MANY_REQUESTS, "rate limited"))
        }
        Err(DataPlaneExecutionError::Fixture(FixtureError::NotFound)) => {
            Ok(response(StatusCode::NOT_FOUND, "not found"))
        }
        Err(DataPlaneExecutionError::Fixture(
            FixtureError::Serialization | FixtureError::ServerError,
        )) => Ok(response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "provider error",
        )),
        Err(DataPlaneExecutionError::Service(_)) => Ok(response(
            StatusCode::SERVICE_UNAVAILABLE,
            "fixture fault plan unavailable",
        )),
    }
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
    ) -> Result<DataPlaneDisposition, DataPlaneExecutionError> {
        match self {
            Self::Fixed { fixture, outcome } => fixture
                .lock()
                .await
                .create_data_plane(key, request, *outcome)
                .map_err(DataPlaneExecutionError::Fixture),
            Self::Managed(fixture) => fixture
                .lock()
                .await
                .create_data_plane(key, request)
                .map_err(|error| match error {
                    FixtureServiceError::Fixture(error) => DataPlaneExecutionError::Fixture(error),
                    error => DataPlaneExecutionError::Service(error),
                }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DataPlaneExecutionError {
    Fixture(FixtureError),
    Service(FixtureServiceError),
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
    InvalidStatusCode,
}

impl fmt::Display for FixtureHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStatusCode => {
                formatter.write_str("fixture produced an invalid HTTP status")
            }
        }
    }
}

impl Error for FixtureHttpError {}
