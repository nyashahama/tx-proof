//! Read-only replay preparation for compiled traces.

use crate::postgres::safety::{DatabaseKind, DatabaseName};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiv_core::{
    decision::Seed,
    trace::{ActionId, ActionKind, CapturedValue, CompiledTrace, InputSlot, OutputRef, OutputSlot},
};

/// A read-only runtime replay plan compiled from a fully bound trace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReplayPlan {
    schema_version: u16,
    seed: Seed,
    action_count: usize,
    steps: Vec<ReplayStep>,
}

impl ReplayPlan {
    /// Converts a validated compiled trace into runtime-executable step
    /// descriptors without opening sockets, touching databases, or releasing
    /// customer-code effects.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayPlanError`] when a trace is structurally valid but not
    /// executable by the current runtime slice.
    pub fn from_trace(trace: &CompiledTrace) -> Result<Self, ReplayPlanError> {
        let steps = trace
            .replay_actions()
            .map(|action| {
                let operation = match action.kind() {
                    ActionKind::DriveCheckout => {
                        let output_ref = OutputRef::new(action.id(), OutputSlot::PaymentIntentId);
                        let captured_payment_intent_id =
                            payment_intent_id(trace.resolve(output_ref))
                                .ok_or(ReplayPlanError::MissingPaymentIntentOutput(action.id()))?;
                        ReplayOperation::DriveCheckout {
                            captured_payment_intent_id,
                        }
                    }
                    ActionKind::ConfirmPaymentIntent => {
                        let payment_intent_id =
                            payment_intent_id(action.input(InputSlot::PaymentIntentId))
                                .ok_or(ReplayPlanError::MissingPaymentIntentInput(action.id()))?;
                        ReplayOperation::ConfirmPaymentIntent { payment_intent_id }
                    }
                };
                Ok(ReplayStep {
                    action_id: action.id(),
                    operation,
                })
            })
            .collect::<Result<Vec<_>, ReplayPlanError>>()?;
        Ok(Self {
            schema_version: trace.schema_version(),
            seed: trace.seed(),
            action_count: trace.action_count(),
            steps,
        })
    }

    #[must_use]
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    #[must_use]
    pub const fn seed(&self) -> Seed {
        self.seed
    }

    #[must_use]
    pub const fn action_count(&self) -> usize {
        self.action_count
    }

    #[must_use]
    pub fn steps(&self) -> &[ReplayStep] {
        &self.steps
    }

    #[must_use]
    pub fn step(&self, action_id: ActionId) -> Option<&ReplayStep> {
        self.steps.iter().find(|step| step.action_id == action_id)
    }
}

/// One ordered runtime replay step.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ReplayStep {
    action_id: ActionId,
    #[serde(flatten)]
    operation: ReplayOperation,
}

impl ReplayStep {
    #[must_use]
    pub const fn action_id(&self) -> ActionId {
        self.action_id
    }

    #[must_use]
    pub const fn operation(&self) -> &ReplayOperation {
        &self.operation
    }
}

/// The current runtime's supported replay operations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ReplayOperation {
    DriveCheckout { captured_payment_intent_id: String },
    ConfirmPaymentIntent { payment_intent_id: String },
}

/// The executable script shape currently supported by the reference app spike.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceReplayScript {
    fixture_seed: Seed,
    expected_payment_intent_id: String,
}

impl ReferenceReplayScript {
    /// Derives the reference app replay script from a runtime plan without
    /// hard-coding fixture identities in the executor.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceReplayScriptError`] when the plan is not the current
    /// commit-then-close checkout shape supported by the reference app replay.
    pub fn from_plan(plan: &ReplayPlan) -> Result<Self, ReferenceReplayScriptError> {
        let steps = plan.steps();
        if steps.len() != 2 {
            return Err(ReferenceReplayScriptError::UnexpectedStepCount {
                actual: steps.len(),
            });
        }
        let ReplayOperation::DriveCheckout {
            captured_payment_intent_id,
        } = steps[0].operation()
        else {
            return Err(ReferenceReplayScriptError::ExpectedDriveCheckout);
        };
        let ReplayOperation::ConfirmPaymentIntent { payment_intent_id } = steps[1].operation()
        else {
            return Err(ReferenceReplayScriptError::ExpectedConfirmPaymentIntent);
        };
        if captured_payment_intent_id != payment_intent_id {
            return Err(ReferenceReplayScriptError::PaymentIntentMismatch);
        }
        Ok(Self {
            fixture_seed: plan.seed(),
            expected_payment_intent_id: captured_payment_intent_id.clone(),
        })
    }

    #[must_use]
    pub const fn fixture_seed(&self) -> Seed {
        self.fixture_seed
    }

    #[must_use]
    pub fn expected_payment_intent_id(&self) -> &str {
        &self.expected_payment_intent_id
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ReferenceReplayScriptError {
    #[error("reference replay requires exactly two trace steps, got {actual}")]
    UnexpectedStepCount { actual: usize },
    #[error("first replay step must drive checkout")]
    ExpectedDriveCheckout,
    #[error("second replay step must confirm the PaymentIntent")]
    ExpectedConfirmPaymentIntent,
    #[error("confirm step targets a different PaymentIntent than checkout produced")]
    PaymentIntentMismatch,
}

/// Loopback-only execution contract for the known-bug reference app replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceAppReplayConfig {
    case_database: DatabaseName,
    reference_app_url: String,
    fixture_control_url: String,
    fixture_control_token: String,
    reset_sequence: u64,
    confirm_sequence: u64,
    webhook_timestamp: i64,
}

impl ReferenceAppReplayConfig {
    /// Builds the bounded reference app replay configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceAppReplayConfigError`] unless the target database is
    /// a generated case database, both HTTP targets are loopback-only URLs, the
    /// fixture control token is non-empty, and command sequences are ordered.
    pub fn new(
        case_database: DatabaseName,
        reference_app_url: impl Into<String>,
        fixture_control_url: impl Into<String>,
        fixture_control_token: impl Into<String>,
        reset_sequence: u64,
        confirm_sequence: u64,
        webhook_timestamp: i64,
    ) -> Result<Self, ReferenceAppReplayConfigError> {
        if case_database.kind() != DatabaseKind::Case {
            return Err(ReferenceAppReplayConfigError::InvalidCaseDatabase);
        }
        if reset_sequence == 0 || confirm_sequence <= reset_sequence {
            return Err(ReferenceAppReplayConfigError::InvalidSequence);
        }
        if webhook_timestamp <= 0 {
            return Err(ReferenceAppReplayConfigError::InvalidTimestamp);
        }
        let fixture_control_token = fixture_control_token.into();
        if fixture_control_token.trim().is_empty() {
            return Err(ReferenceAppReplayConfigError::MissingControlToken);
        }
        Ok(Self {
            case_database,
            reference_app_url: normalize_loopback_http_url(reference_app_url)?,
            fixture_control_url: normalize_loopback_http_url(fixture_control_url)?,
            fixture_control_token,
            reset_sequence,
            confirm_sequence,
            webhook_timestamp,
        })
    }

    #[must_use]
    pub const fn case_database(&self) -> &DatabaseName {
        &self.case_database
    }

    /// Returns the operation identity reserved for this generated case.
    #[must_use]
    pub fn operation_id(&self) -> String {
        format!(
            "op_{}",
            self.case_database.as_str().trim_start_matches("tiv_case_")
        )
    }

    #[must_use]
    pub fn reference_app_url(&self) -> &str {
        &self.reference_app_url
    }

    #[must_use]
    pub fn fixture_control_url(&self) -> &str {
        &self.fixture_control_url
    }

    #[must_use]
    pub fn fixture_control_token(&self) -> &str {
        &self.fixture_control_token
    }

    #[must_use]
    pub const fn reset_sequence(&self) -> u64 {
        self.reset_sequence
    }

    #[must_use]
    pub const fn confirm_sequence(&self) -> u64 {
        self.confirm_sequence
    }

    #[must_use]
    pub const fn webhook_timestamp(&self) -> i64 {
        self.webhook_timestamp
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ReferenceAppReplayConfigError {
    #[error("reference app replay requires a generated case database")]
    InvalidCaseDatabase,
    #[error("reference app replay URLs must be loopback http endpoints without paths")]
    NonLoopbackUrl,
    #[error("fixture control token is required")]
    MissingControlToken,
    #[error("fixture control sequences must start at a positive reset sequence and confirm later")]
    InvalidSequence,
    #[error("webhook timestamp must be positive")]
    InvalidTimestamp,
}

/// Result of one reference app replay execution.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct ReferenceAppReplayReceipt {
    fixture_seed: u64,
    expected_payment_intent_id: String,
    checkout_payment_intent_id: String,
    fixture_control_isolated: bool,
    delivered_webhook_count: usize,
    provider_payment_intents: Vec<ReferenceProviderPaymentIntent>,
    #[serde(skip)]
    completion: ReferenceAppReplayCompletion,
}

impl ReferenceAppReplayReceipt {
    #[must_use]
    pub const fn fixture_seed(&self) -> u64 {
        self.fixture_seed
    }

    #[must_use]
    pub fn expected_payment_intent_id(&self) -> &str {
        &self.expected_payment_intent_id
    }

    #[must_use]
    pub fn checkout_payment_intent_id(&self) -> &str {
        &self.checkout_payment_intent_id
    }

    #[must_use]
    pub const fn fixture_control_isolated(&self) -> bool {
        self.fixture_control_isolated
    }

    #[must_use]
    pub const fn delivered_webhook_count(&self) -> usize {
        self.delivered_webhook_count
    }

    #[must_use]
    pub fn provider_payment_intents(&self) -> &[ReferenceProviderPaymentIntent] {
        &self.provider_payment_intents
    }

    pub(crate) fn into_oracle_input(
        self,
    ) -> (
        Vec<ReferenceProviderPaymentIntent>,
        ReferenceAppReplayCompletion,
    ) {
        (self.provider_payment_intents, self.completion)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ReferenceAppReplayCompletion {
    _private: (),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReferenceProviderPaymentIntent {
    id: String,
    amount_minor: i64,
    currency: String,
    status: String,
    operation_id: Option<String>,
}

impl ReferenceProviderPaymentIntent {
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
        &self.status
    }

    #[must_use]
    pub fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }
}

/// Executes one bounded checkout replay against the isolated reference app.
///
/// # Errors
///
/// Returns [`ReferenceAppReplayError`] when the trace cannot form the current
/// reference script, the loopback services reject a step, or returned JSON is
/// outside the narrow reference contract.
pub async fn run_reference_app_replay(
    plan: &ReplayPlan,
    config: &ReferenceAppReplayConfig,
) -> Result<ReferenceAppReplayReceipt, ReferenceAppReplayError> {
    let script = ReferenceReplayScript::from_plan(plan)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    reset_fixture(&client, config, script.fixture_seed()).await?;
    let checkout_payment_intent_id = drive_reference_checkout(&client, config, &script).await?;
    let fixture_control_isolated = assert_fixture_control_is_isolated(&client, config).await?;
    let attempts = confirm_fixture(&client, config).await?;
    deliver_webhook_attempts(&client, config, &attempts).await?;
    let provider_payment_intents = fixture_provider_projection(&client, config).await?;
    if !provider_payment_intents
        .iter()
        .any(|payment_intent| payment_intent.id == script.expected_payment_intent_id())
    {
        return Err(ReferenceAppReplayError::MissingExpectedPaymentIntent);
    }

    Ok(ReferenceAppReplayReceipt {
        fixture_seed: script.fixture_seed().value(),
        expected_payment_intent_id: script.expected_payment_intent_id().to_owned(),
        checkout_payment_intent_id,
        fixture_control_isolated,
        delivered_webhook_count: attempts.len(),
        provider_payment_intents,
        completion: ReferenceAppReplayCompletion { _private: () },
    })
}

#[derive(Debug, Error)]
pub enum ReferenceAppReplayError {
    #[error("trace cannot form a reference app replay script: {0}")]
    Script(#[from] ReferenceReplayScriptError),
    #[error("reference app replay HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("reference app replay step {step} returned HTTP {status}")]
    UnexpectedStatus { step: &'static str, status: u16 },
    #[error("reference app checkout did not return the trace-bound PaymentIntent")]
    CheckoutPaymentIntentMismatch,
    #[error("reference app checkout returned an unexpected operation ID")]
    CheckoutOperationMismatch,
    #[error("reference app can reach the fixture control listener")]
    FixtureControlReachable,
    #[error("fixture confirmation did not return exactly two webhook attempts")]
    UnexpectedWebhookAttemptCount,
    #[error("fixture confirmation returned an unexpected command sequence")]
    UnexpectedConfirmSequence,
    #[error("fixture confirmation returned an unexpected webhook attempt")]
    UnexpectedWebhookAttempt,
    #[error("fixture state did not match the completed replay contract")]
    UnexpectedFixtureState,
    #[error("fixture webhook attempt body is not valid hex")]
    InvalidWebhookBodyHex,
    #[error("fixture projection did not contain the expected PaymentIntent")]
    MissingExpectedPaymentIntent,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ReplayPlanError {
    #[error("drive checkout action {0:?} did not capture a PaymentIntent ID")]
    MissingPaymentIntentOutput(ActionId),
    #[error("confirm action {0:?} did not resolve a PaymentIntent ID")]
    MissingPaymentIntentInput(ActionId),
}

fn payment_intent_id(value: Option<&CapturedValue>) -> Option<String> {
    match value {
        Some(CapturedValue::PaymentIntentId(id)) => Some(id.as_str().to_owned()),
        Some(CapturedValue::EventId(_)) | None => None,
    }
}

fn normalize_loopback_http_url(
    value: impl Into<String>,
) -> Result<String, ReferenceAppReplayConfigError> {
    let value = value.into();
    if value.chars().any(char::is_whitespace) {
        return Err(ReferenceAppReplayConfigError::NonLoopbackUrl);
    }
    let url =
        reqwest::Url::parse(&value).map_err(|_| ReferenceAppReplayConfigError::NonLoopbackUrl)?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost"))
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ReferenceAppReplayConfigError::NonLoopbackUrl);
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

async fn reset_fixture(
    client: &reqwest::Client,
    config: &ReferenceAppReplayConfig,
    seed: Seed,
) -> Result<(), ReferenceAppReplayError> {
    let response = client
        .post(format!("{}/v1/control/reset", config.fixture_control_url()))
        .header("X-Tiv-Control-Token", config.fixture_control_token())
        .json(&serde_json::json!({
            "command_sequence": config.reset_sequence(),
            "seed": seed.value(),
            "outcomes": ["commit_then_close", "normal"]
        }))
        .send()
        .await?;
    let _response = require_ok(response, "fixture reset")?;
    Ok(())
}

async fn drive_reference_checkout(
    client: &reqwest::Client,
    config: &ReferenceAppReplayConfig,
    script: &ReferenceReplayScript,
) -> Result<String, ReferenceAppReplayError> {
    let operation_id = config.operation_id();
    let response = client
        .post(format!("{}/checkout", config.reference_app_url()))
        .json(&serde_json::json!({
            "database": config.case_database().as_str(),
            "operation_id": operation_id,
            "amount_minor": 2500,
            "currency": "usd"
        }))
        .send()
        .await?;
    let response = require_ok(response, "reference checkout")?;
    let checkout = response.json::<CheckoutResponse>().await?;
    if checkout.payment_intent_id != script.expected_payment_intent_id() {
        return Err(ReferenceAppReplayError::CheckoutPaymentIntentMismatch);
    }
    if checkout.operation_id != config.operation_id() {
        return Err(ReferenceAppReplayError::CheckoutOperationMismatch);
    }
    Ok(checkout.payment_intent_id)
}

async fn assert_fixture_control_is_isolated(
    client: &reqwest::Client,
    config: &ReferenceAppReplayConfig,
) -> Result<bool, ReferenceAppReplayError> {
    let response = client
        .get(format!(
            "{}/probe-fixture-control",
            config.reference_app_url()
        ))
        .send()
        .await?;
    let response = require_ok(response, "fixture isolation probe")?;
    let isolation = response.json::<FixtureIsolationResponse>().await?;
    if isolation.reachable {
        return Err(ReferenceAppReplayError::FixtureControlReachable);
    }
    Ok(true)
}

async fn confirm_fixture(
    client: &reqwest::Client,
    config: &ReferenceAppReplayConfig,
) -> Result<Vec<SignedWebhookAttempt>, ReferenceAppReplayError> {
    let response = client
        .post(format!(
            "{}/v1/control/confirm-all",
            config.fixture_control_url()
        ))
        .header("X-Tiv-Control-Token", config.fixture_control_token())
        .json(&serde_json::json!({
            "command_sequence": config.confirm_sequence(),
            "timestamp": config.webhook_timestamp()
        }))
        .send()
        .await?;
    let response = require_ok(response, "fixture confirm")?;
    let confirmation = response.json::<ConfirmationResponse>().await?;
    if confirmation.command_sequence != config.confirm_sequence() {
        return Err(ReferenceAppReplayError::UnexpectedConfirmSequence);
    }
    if confirmation.attempts.len() != 2 {
        return Err(ReferenceAppReplayError::UnexpectedWebhookAttemptCount);
    }
    if confirmation.attempts.iter().any(|attempt| {
        attempt.timestamp != config.webhook_timestamp() || !attempt.event_id.starts_with("evt_tiv_")
    }) {
        return Err(ReferenceAppReplayError::UnexpectedWebhookAttempt);
    }
    Ok(confirmation.attempts)
}

async fn deliver_webhook_attempts(
    client: &reqwest::Client,
    config: &ReferenceAppReplayConfig,
    attempts: &[SignedWebhookAttempt],
) -> Result<(), ReferenceAppReplayError> {
    for attempt in attempts {
        let raw_body = hex::decode(&attempt.raw_body_hex)
            .map_err(|_| ReferenceAppReplayError::InvalidWebhookBodyHex)?;
        let response = client
            .post(format!("{}/webhooks/stripe", config.reference_app_url()))
            .header("Stripe-Signature", &attempt.signature_header)
            .body(raw_body)
            .send()
            .await?;
        let _response = require_ok(response, "webhook delivery")?;
    }
    Ok(())
}

async fn fixture_provider_projection(
    client: &reqwest::Client,
    config: &ReferenceAppReplayConfig,
) -> Result<Vec<ReferenceProviderPaymentIntent>, ReferenceAppReplayError> {
    let operation_id = config.operation_id();
    let response = client
        .get(format!("{}/v1/control/state", config.fixture_control_url()))
        .header("X-Tiv-Control-Token", config.fixture_control_token())
        .send()
        .await?;
    let response = require_ok(response, "fixture state")?;
    let state = response.json::<FixtureStateResponse>().await?;
    if state.command_sequence != config.confirm_sequence()
        || state.remaining_outcomes != 0
        || !state.held_gates.is_empty()
        || state.payment_intents.len() != 2
        || state.payment_intents.iter().any(|payment_intent| {
            payment_intent.operation_id() != Some(operation_id.as_str())
                || payment_intent.amount_minor() != 2_500
                || payment_intent.currency() != "usd"
        })
    {
        return Err(ReferenceAppReplayError::UnexpectedFixtureState);
    }
    Ok(state.payment_intents)
}

fn require_ok(
    response: reqwest::Response,
    step: &'static str,
) -> Result<reqwest::Response, ReferenceAppReplayError> {
    let status = response.status();
    if !status.is_success() {
        return Err(ReferenceAppReplayError::UnexpectedStatus {
            step,
            status: status.as_u16(),
        });
    }
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutResponse {
    payment_intent_id: String,
    operation_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureIsolationResponse {
    reachable: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmationResponse {
    command_sequence: u64,
    attempts: Vec<SignedWebhookAttempt>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedWebhookAttempt {
    event_id: String,
    timestamp: i64,
    raw_body_hex: String,
    signature_header: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureStateResponse {
    payment_intents: Vec<ReferenceProviderPaymentIntent>,
    command_sequence: u64,
    remaining_outcomes: usize,
    held_gates: Vec<HeldGateResponse>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeldGateResponse {
    #[serde(rename = "gate_id")]
    _gate_id: u64,
}

#[cfg(test)]
mod tests {
    use super::FixtureStateResponse;

    #[test]
    fn fixture_state_contract_recognizes_the_v1_held_gate_field() {
        let state: FixtureStateResponse = serde_json::from_str(
            r#"{
                "payment_intents": [],
                "command_sequence": 2,
                "remaining_outcomes": 0,
                "held_gates": []
            }"#,
        )
        .expect("the current fixture control state is recognized");

        assert!(state.held_gates.is_empty());
    }
}
