//! Reference-application planned-case execution and checkpoint evaluation.

use std::{path::Path, time::Duration};

use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use thiserror::Error;
use tiv_core::plan::{PlanActionKind, PlanValidationError, PlannedCase, ProviderOutcome};

use crate::{
    campaign::{
        CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest, CaseExecutionError, ExecutedCase,
        execute_planned_case,
    },
    case_http::{CaseHttpAdapter, CaseHttpError, ReferenceCaseHttpCompletion},
    postgres::{
        oracle::{ProviderPaymentIntent, QuiescencePermit},
        safety::{DatabaseKind, DatabaseName},
    },
    provider_http::{
        ProviderHttpAdapter, ProviderHttpConfig, ProviderHttpConfigError, ProviderHttpError,
    },
    webhook_http::{
        WebhookHttpAdapter, WebhookHttpConfig, WebhookHttpConfigError, WebhookHttpError,
    },
};

const MAX_RESET_RESPONSE_BYTES: usize = 16 * 1024;

/// Validated, secret-bearing inputs for one reference planned-case run.
///
/// This type intentionally implements neither `Debug` nor `Serialize`.
pub struct ReferenceCaseRunConfig {
    fixture_control_url: Url,
    fixture_control_token: String,
    reset_sequence: u64,
    reset_client: Client,
    provider: ProviderHttpConfig,
    webhook: WebhookHttpConfig,
}

impl ReferenceCaseRunConfig {
    /// Builds both HTTP adapters and the fixture-reset boundary for one case.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceCaseRunConfigError`] unless the database is a
    /// generated case name and all endpoints, secrets, values, and bounds
    /// satisfy the underlying provider and webhook contracts.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        case_database: &DatabaseName,
        reference_app_url: impl AsRef<str>,
        fixture_control_url: impl AsRef<str>,
        fixture_control_token: impl Into<String>,
        reset_sequence: u64,
        first_webhook_timestamp: i64,
        operation_id: impl Into<String>,
        amount_minor: i64,
        currency: impl Into<String>,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self, ReferenceCaseRunConfigError> {
        if case_database.kind() != DatabaseKind::Case {
            return Err(ReferenceCaseRunConfigError::InvalidCaseDatabase);
        }
        let reference_app_url = reference_app_url.as_ref().trim_end_matches('/');
        let fixture_control_url_value = fixture_control_url.as_ref();
        let fixture_control_token = fixture_control_token.into();
        let operation_id = operation_id.into();
        let currency = currency.into();
        let provider = ProviderHttpConfig::new(
            format!("{reference_app_url}/checkout"),
            serde_json::json!({
                "database": case_database.as_str(),
                "operation_id": operation_id,
                "amount_minor": amount_minor,
                "currency": currency,
            }),
            reference_app_url,
            fixture_control_url_value,
            fixture_control_token.clone(),
            reset_sequence,
            timeout,
            poll_interval,
        )?;
        let webhook = WebhookHttpConfig::new(
            fixture_control_url_value,
            fixture_control_token.clone(),
            reset_sequence,
            first_webhook_timestamp,
            timeout,
        )?;
        let fixture_control_url = Url::parse(fixture_control_url_value)
            .map_err(|_| ReferenceCaseRunConfigError::InvalidControlUrl)?;
        let reset_client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(ReferenceCaseRunConfigError::ClientBuild)?;
        Ok(Self {
            fixture_control_url,
            fixture_control_token,
            reset_sequence,
            reset_client,
            provider,
            webhook,
        })
    }
}

/// Completed serial trace plus its quiescent final provider checkpoint.
pub struct ReferenceCaseRunReceipt {
    executed: ExecutedCase,
    checkpoint: ReferenceCaseCheckpoint,
}

impl ReferenceCaseRunReceipt {
    #[must_use]
    pub const fn executed(&self) -> &ExecutedCase {
        &self.executed
    }

    #[must_use]
    pub const fn checkpoint(&self) -> &ReferenceCaseCheckpoint {
        &self.checkpoint
    }

    #[must_use]
    pub fn into_parts(self) -> (ExecutedCase, ReferenceCaseCheckpoint) {
        (self.executed, self.checkpoint)
    }
}

/// Resets the fixture from a validated compiled plan and executes every action
/// through the real reference application and fixture control plane.
///
/// # Errors
///
/// Returns [`ReferenceCaseRunError`] before reset for an invalid or currently
/// unsupported plan, or after reset for a fixture, adapter, journal, or
/// lifecycle failure.
pub async fn run_reference_planned_case(
    run_id: impl Into<String>,
    case_id: impl Into<String>,
    planned_case: &PlannedCase,
    journal_path: impl AsRef<Path>,
    config: ReferenceCaseRunConfig,
) -> Result<ReferenceCaseRunReceipt, ReferenceCaseRunError> {
    planned_case
        .validate()
        .map_err(ReferenceCaseRunError::InvalidPlan)?;
    if planned_case.actions().iter().any(|action| {
        matches!(
            action.kind(),
            PlanActionKind::KillApplication { .. } | PlanActionKind::RestartAndAwaitHealth
        )
    }) {
        return Err(ReferenceCaseRunError::UnsupportedProcessFault);
    }
    let outcomes = provider_outcomes(planned_case);
    if outcomes.is_empty() {
        return Err(ReferenceCaseRunError::MissingProviderOutcomes);
    }
    reset_fixture(&config, planned_case, &outcomes).await?;
    let mut adapter = ReferenceCaseAdapter::new(CaseHttpAdapter::new(
        ProviderHttpAdapter::new(config.provider)?,
        WebhookHttpAdapter::new(config.webhook)?,
    ));
    let executed =
        execute_planned_case(run_id, case_id, planned_case, journal_path, &mut adapter).await?;
    let checkpoint = adapter.finish()?;
    Ok(ReferenceCaseRunReceipt {
        executed,
        checkpoint,
    })
}

fn provider_outcomes(planned_case: &PlannedCase) -> Vec<ProviderOutcome> {
    planned_case
        .actions()
        .iter()
        .filter_map(|action| match action.kind() {
            PlanActionKind::DriveCheckout { provider_script }
            | PlanActionKind::RetryBusinessRequest { provider_script }
            | PlanActionKind::ConfirmPaymentIntent { provider_script }
            | PlanActionKind::RetryProviderRequest { provider_script } => Some(*provider_script),
            _ => None,
        })
        .flat_map(tiv_core::plan::ProviderOutcomeScript::outcomes)
        .collect()
}

async fn reset_fixture(
    config: &ReferenceCaseRunConfig,
    planned_case: &PlannedCase,
    outcomes: &[ProviderOutcome],
) -> Result<(), ReferenceCaseRunError> {
    let mut endpoint = config.fixture_control_url.clone();
    endpoint.set_path("/v1/control/reset");
    let response = config
        .reset_client
        .post(endpoint)
        .header("X-Tiv-Control-Token", &config.fixture_control_token)
        .json(&serde_json::json!({
            "command_sequence": config.reset_sequence,
            "seed": planned_case.seed().value(),
            "outcomes": outcomes,
        }))
        .send()
        .await
        .map_err(ReferenceCaseRunError::ResetRequest)?;
    if response.status() != StatusCode::OK {
        return Err(ReferenceCaseRunError::UnexpectedResetStatus(
            response.status(),
        ));
    }
    let body = response
        .bytes()
        .await
        .map_err(ReferenceCaseRunError::ResetRequest)?;
    if body.len() > MAX_RESET_RESPONSE_BYTES {
        return Err(ReferenceCaseRunError::UnexpectedResetResponse);
    }
    let reset = serde_json::from_slice::<ResetResponse>(&body)
        .map_err(|_| ReferenceCaseRunError::UnexpectedResetResponse)?;
    if reset.command_sequence != config.reset_sequence
        || reset.remaining_outcomes != outcomes.len()
        || !reset.held_gates.is_empty()
        || !reset.payment_intents.is_empty()
    {
        return Err(ReferenceCaseRunError::UnexpectedResetResponse);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetResponse {
    command_sequence: u64,
    remaining_outcomes: usize,
    held_gates: Vec<serde_json::Value>,
    payment_intents: Vec<serde_json::Value>,
}

/// Executes HTTP effects plus the synthetic reference application's final
/// quiescence and checkpoint boundaries.
pub struct ReferenceCaseAdapter {
    http: CaseHttpAdapter,
    quiescence: Option<ReferenceCaseHttpCompletion>,
    checkpoint: Option<ReferenceCaseCheckpoint>,
}

impl ReferenceCaseAdapter {
    #[must_use]
    pub const fn new(http: CaseHttpAdapter) -> Self {
        Self {
            http,
            quiescence: None,
            checkpoint: None,
        }
    }

    /// Consumes a successfully executed adapter and returns its final
    /// checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceCaseError::MissingCheckpoint`] unless the planned
    /// case reached its exact final checkpoint.
    pub fn finish(self) -> Result<ReferenceCaseCheckpoint, ReferenceCaseError> {
        self.checkpoint.ok_or(ReferenceCaseError::MissingCheckpoint)
    }

    async fn wait_for_quiescence(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<
        Vec<(
            tiv_core::trace::CaseOutputRef,
            tiv_core::trace::CaseCapturedValue,
        )>,
        ReferenceCaseError,
    > {
        require_no_outputs(request)?;
        if self.quiescence.is_some() || self.checkpoint.is_some() {
            return Err(ReferenceCaseError::InvalidLifecycleOrder);
        }
        self.quiescence = Some(self.http.await_reference_quiescence().await?);
        Ok(Vec::new())
    }

    fn check_checkpoint(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<
        Vec<(
            tiv_core::trace::CaseOutputRef,
            tiv_core::trace::CaseCapturedValue,
        )>,
        ReferenceCaseError,
    > {
        require_no_outputs(request)?;
        if self.checkpoint.is_some() {
            return Err(ReferenceCaseError::InvalidLifecycleOrder);
        }
        let completion = self
            .quiescence
            .take()
            .ok_or(ReferenceCaseError::InvalidLifecycleOrder)?;
        let quiescence = QuiescencePermit::after_reference_case_http_quiescent(&completion);
        self.checkpoint = Some(ReferenceCaseCheckpoint {
            provider_payment_intents: completion.into_provider_payment_intents(),
            quiescence,
        });
        Ok(Vec::new())
    }
}

impl CaseEffectAdapter for ReferenceCaseAdapter {
    type Error = ReferenceCaseError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        Box::pin(async move {
            match request.action().kind() {
                PlanActionKind::WaitForQuiescence => self.wait_for_quiescence(&request).await,
                PlanActionKind::CheckCheckpoint { .. } => self.check_checkpoint(&request),
                _ if self.quiescence.is_some() || self.checkpoint.is_some() => {
                    Err(ReferenceCaseError::InvalidLifecycleOrder)
                }
                _ => self
                    .http
                    .execute(request)
                    .await
                    .map_err(ReferenceCaseError::Http),
            }
        })
    }
}

/// Provider truth and the capability proving the reference app was quiescent
/// at the final checkpoint.
pub struct ReferenceCaseCheckpoint {
    provider_payment_intents: Vec<ProviderPaymentIntent>,
    quiescence: QuiescencePermit,
}

impl ReferenceCaseCheckpoint {
    #[must_use]
    pub fn provider_payment_intents(&self) -> &[ProviderPaymentIntent] {
        &self.provider_payment_intents
    }

    /// Consumes the checkpoint into the provider projection and the
    /// unforgeable quiescence capability required by the `PostgreSQL` oracle.
    #[must_use]
    pub fn into_oracle_parts(self) -> (Vec<ProviderPaymentIntent>, QuiescencePermit) {
        (self.provider_payment_intents, self.quiescence)
    }
}

fn require_no_outputs(request: &CaseEffectRequest<'_>) -> Result<(), ReferenceCaseError> {
    if request.expected_outputs().is_empty() {
        Ok(())
    } else {
        Err(ReferenceCaseError::UnexpectedOutputContract)
    }
}

#[derive(Debug, Error)]
pub enum ReferenceCaseError {
    #[error("reference case HTTP effect failed: {0}")]
    Http(#[from] CaseHttpError),
    #[error("reference case lifecycle actions were not in the required order")]
    InvalidLifecycleOrder,
    #[error("reference case lifecycle action declared outputs")]
    UnexpectedOutputContract,
    #[error("reference case did not reach its final checkpoint")]
    MissingCheckpoint,
}

#[derive(Debug, Error)]
pub enum ReferenceCaseRunConfigError {
    #[error("reference planned case requires a generated case database")]
    InvalidCaseDatabase,
    #[error("reference planned-case provider configuration is invalid: {0}")]
    Provider(#[from] ProviderHttpConfigError),
    #[error("reference planned-case webhook configuration is invalid: {0}")]
    Webhook(#[from] WebhookHttpConfigError),
    #[error("reference planned-case fixture control URL is invalid")]
    InvalidControlUrl,
    #[error("could not build the reference planned-case reset client: {0}")]
    ClientBuild(reqwest::Error),
}

#[derive(Debug, Error)]
pub enum ReferenceCaseRunError {
    #[error("reference planned case failed pure validation")]
    InvalidPlan(PlanValidationError),
    #[error("reference planned-case process faults are not implemented")]
    UnsupportedProcessFault,
    #[error("reference planned case did not contain a provider fault script")]
    MissingProviderOutcomes,
    #[error("reference planned-case fixture reset request failed: {0}")]
    ResetRequest(reqwest::Error),
    #[error("reference planned-case fixture reset returned HTTP {0}")]
    UnexpectedResetStatus(StatusCode),
    #[error("reference planned-case fixture reset response was incoherent")]
    UnexpectedResetResponse,
    #[error("could not construct the reference planned-case provider adapter: {0}")]
    ProviderAdapter(#[from] ProviderHttpError),
    #[error("could not construct the reference planned-case webhook adapter: {0}")]
    WebhookAdapter(#[from] WebhookHttpError),
    #[error("reference planned-case serial execution failed")]
    Execution(CaseExecutionError<ReferenceCaseError>),
    #[error("reference planned-case lifecycle did not complete: {0}")]
    Lifecycle(#[from] ReferenceCaseError),
}

impl From<CaseExecutionError<ReferenceCaseError>> for ReferenceCaseRunError {
    fn from(error: CaseExecutionError<ReferenceCaseError>) -> Self {
        Self::Execution(error)
    }
}
