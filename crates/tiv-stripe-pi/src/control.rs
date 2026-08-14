use std::{error::Error, fmt, sync::Arc};

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
use serde::Deserialize;
use tiv_core::decision::Seed;
use tokio::{net::TcpStream, sync::Mutex};

use crate::{FaultOutcome, FixtureServiceError, GateId, ManagedFixture};

const MAX_CONTROL_BODY_BYTES: usize = 16 * 1024;
const CONTROL_TOKEN_HEADER: &str = "x-tiv-control-token";

type ResponseBody = Full<Bytes>;

#[derive(Clone)]
pub struct ControlToken(String);

impl ControlToken {
    /// Creates a run-scoped control token without providing a printable
    /// representation of the secret.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSecret`] for a blank or overlong token.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidSecret> {
        let value = value.into();
        if value.trim().is_empty() || value.len() > 1_024 {
            return Err(InvalidSecret);
        }
        Ok(Self(value))
    }

    fn matches(&self, supplied: &str) -> bool {
        self.0 == supplied
    }
}

#[derive(Clone)]
pub struct WebhookSigningSecret(Vec<u8>);

impl WebhookSigningSecret {
    /// Creates a webhook signing secret without providing a printable
    /// representation of the secret.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidSecret`] for a blank or overlong secret.
    pub fn new(value: impl AsRef<[u8]>) -> Result<Self, InvalidSecret> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > 1_024 {
            return Err(InvalidSecret);
        }
        Ok(Self(value.to_vec()))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidSecret;

/// Serves one HTTP/1 connection on the fixture's isolated control listener.
///
/// # Errors
///
/// Returns a Hyper connection error if the peer disconnects or sends an
/// invalid HTTP stream.
pub async fn serve_http1_connection(
    stream: TcpStream,
    fixture: Arc<Mutex<ManagedFixture>>,
    token: ControlToken,
    webhook_secret: WebhookSigningSecret,
) -> Result<(), hyper::Error> {
    let service = service_fn(move |request| {
        handle_request(
            request,
            Arc::clone(&fixture),
            token.clone(),
            webhook_secret.clone(),
        )
    });
    http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await
}

async fn handle_request(
    request: Request<Incoming>,
    fixture: Arc<Mutex<ManagedFixture>>,
    token: ControlToken,
    webhook_secret: WebhookSigningSecret,
) -> Result<Response<ResponseBody>, ControlHttpError> {
    if request.method() == Method::GET && request.uri().path() == "/health" {
        return json_response(StatusCode::OK, &serde_json::json!({"protocol_version": 1}));
    }

    let authorized = request
        .headers()
        .get(CONTROL_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| token.matches(value));
    if !authorized {
        return Ok(text_response(StatusCode::UNAUTHORIZED, "unauthorized"));
    }

    match (request.method(), request.uri().path()) {
        (&Method::GET, "/v1/control/state") => {
            let snapshot = fixture.lock().await.snapshot();
            json_response(StatusCode::OK, &snapshot)
        }
        (&Method::POST, "/v1/control/reset") => {
            let Some(command) = decode_json::<ResetCommand>(request).await else {
                return Ok(text_response(StatusCode::BAD_REQUEST, "invalid command"));
            };
            let result = fixture.lock().await.reset(
                command.command_sequence,
                Seed::new(command.seed),
                command.outcomes,
            );
            service_result(result)
        }
        (&Method::POST, "/v1/control/confirm-all") => {
            let Some(command) = decode_json::<ConfirmAllCommand>(request).await else {
                return Ok(text_response(StatusCode::BAD_REQUEST, "invalid command"));
            };
            let result = fixture.lock().await.confirm_all(
                command.command_sequence,
                command.timestamp,
                &webhook_secret,
            );
            service_result(result)
        }
        (&Method::POST, "/v1/control/release-gate") => {
            let Some(command) = decode_json::<ReleaseGateCommand>(request).await else {
                return Ok(text_response(StatusCode::BAD_REQUEST, "invalid command"));
            };
            let result = fixture
                .lock()
                .await
                .release_gate(command.command_sequence, command.gate_id);
            service_result(result)
        }
        _ => Ok(text_response(StatusCode::NOT_FOUND, "not found")),
    }
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
    Limited::new(request.into_body(), MAX_CONTROL_BODY_BYTES)
        .collect()
        .await
        .ok()
        .and_then(|body| serde_json::from_slice(&body.to_bytes()).ok())
}

fn service_result<T>(
    result: Result<T, FixtureServiceError>,
) -> Result<Response<ResponseBody>, ControlHttpError>
where
    T: serde::Serialize,
{
    match result {
        Ok(value) => json_response(StatusCode::OK, &value),
        Err(FixtureServiceError::UnexpectedCommandSequence { .. }) => Ok(text_response(
            StatusCode::CONFLICT,
            "unexpected command sequence",
        )),
        Err(FixtureServiceError::EmptyFaultPlan) => {
            Ok(text_response(StatusCode::BAD_REQUEST, "empty fault plan"))
        }
        Err(FixtureServiceError::GateNotFound) => {
            Ok(text_response(StatusCode::NOT_FOUND, "gate not found"))
        }
        Err(
            FixtureServiceError::CommandSequenceExhausted
            | FixtureServiceError::FaultPlanExhausted
            | FixtureServiceError::GateSequenceExhausted
            | FixtureServiceError::Fixture(_)
            | FixtureServiceError::WebhookSignature(_),
        ) => Ok(text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "fixture control error",
        )),
    }
}

fn json_response<T>(
    status: StatusCode,
    value: &T,
) -> Result<Response<ResponseBody>, ControlHttpError>
where
    T: serde::Serialize,
{
    let body = serde_json::to_vec(value).map_err(|_| ControlHttpError::Serialization)?;
    let mut response = text_response(status, body);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(response)
}

fn text_response(status: StatusCode, body: impl Into<Bytes>) -> Response<ResponseBody> {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = status;
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetCommand {
    command_sequence: u64,
    seed: u64,
    outcomes: Vec<FaultOutcome>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmAllCommand {
    command_sequence: u64,
    timestamp: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseGateCommand {
    command_sequence: u64,
    gate_id: GateId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlHttpError {
    Serialization,
}

impl fmt::Display for ControlHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("fixture control response serialization failed")
    }
}

impl Error for ControlHttpError {}
