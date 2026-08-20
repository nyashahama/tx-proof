//! Reference-application planned-case execution and checkpoint evaluation.

use std::{future::Future, path::Path, pin::Pin, time::Duration};

use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use thiserror::Error;
use tiv_core::plan::{
    PlanActionKind, PlanValidationError, PlannedCase, ProcessCutPoint, ProviderOutcome,
};

use crate::{
    campaign::{
        CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest, CaseExecutionError, ExecutedCase,
        execute_planned_case,
    },
    case_http::{CaseHttpAdapter, CaseHttpError, ReferenceCaseHttpCompletion},
    postgres::{
        oracle::{ProviderPaymentIntent, QuiescencePermit},
        safety::{DatabaseKind, DatabaseName},
        spike::ReferenceSqlProbe,
    },
    provider_http::{
        ProviderHttpAdapter, ProviderHttpConfig, ProviderHttpConfigError, ProviderHttpError,
    },
    webhook_http::{
        WebhookHttpAdapter, WebhookHttpConfig, WebhookHttpConfigError, WebhookHttpError,
    },
};

const MAX_RESET_RESPONSE_BYTES: usize = 16 * 1024;

pub(crate) type ReferenceProcessControlFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ReferenceProcessControlError>> + Send + 'a>>;

pub(crate) trait ReferenceProcessControl: Send {
    fn kill_application(&mut self) -> ReferenceProcessControlFuture<'_>;
    fn restart_and_await_health(&mut self) -> ReferenceProcessControlFuture<'_>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ReferenceProcessControlError {
    #[error("the attested reference application could not be killed")]
    Kill,
    #[error("the attested reference application could not be restarted")]
    Restart,
    #[error("the restarted reference application did not become healthy and fully attested")]
    HealthOrAttestation,
}

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
    run_reference_planned_case_inner(
        run_id,
        case_id,
        planned_case,
        journal_path,
        config,
        None,
        None,
    )
    .await
}

pub(crate) async fn run_reference_planned_case_with_process(
    run_id: impl Into<String>,
    case_id: impl Into<String>,
    planned_case: &PlannedCase,
    journal_path: impl AsRef<Path>,
    config: ReferenceCaseRunConfig,
    process: &mut dyn ReferenceProcessControl,
    sql_probe: Option<&mut ReferenceSqlProbe>,
) -> Result<ReferenceCaseRunReceipt, ReferenceCaseRunError> {
    run_reference_planned_case_inner(
        run_id,
        case_id,
        planned_case,
        journal_path,
        config,
        Some(process),
        sql_probe,
    )
    .await
}

async fn run_reference_planned_case_inner<'a>(
    run_id: impl Into<String>,
    case_id: impl Into<String>,
    planned_case: &PlannedCase,
    journal_path: impl AsRef<Path>,
    config: ReferenceCaseRunConfig,
    process: Option<&'a mut dyn ReferenceProcessControl>,
    sql_probe: Option<&'a mut ReferenceSqlProbe>,
) -> Result<ReferenceCaseRunReceipt, ReferenceCaseRunError> {
    preflight_reference_planned_case(planned_case, process.is_some(), sql_probe.is_some())?;
    let client_response_cut_points = client_response_cut_point_actions(planned_case)?;
    let webhook_response_cut_points = webhook_response_cut_point_actions(planned_case)?;
    let sql_probe_cut_points = sql_probe_cut_point_actions(planned_case)?;
    let outcomes = provider_outcomes(planned_case);
    reset_fixture(&config, planned_case, &outcomes).await?;
    let mut adapter = ReferenceCaseAdapter::new(
        CaseHttpAdapter::new(
            ProviderHttpAdapter::new(config.provider)?
                .with_client_response_cut_points(client_response_cut_points),
            WebhookHttpAdapter::new(config.webhook)?
                .with_webhook_response_cut_points(webhook_response_cut_points),
        ),
        process,
        sql_probe,
        sql_probe_cut_points,
    );
    let executed =
        execute_planned_case(run_id, case_id, planned_case, journal_path, &mut adapter).await?;
    let checkpoint = adapter.finish()?;
    Ok(ReferenceCaseRunReceipt {
        executed,
        checkpoint,
    })
}

pub(crate) fn preflight_reference_planned_case(
    planned_case: &PlannedCase,
    process_control_available: bool,
    sql_probe_available: bool,
) -> Result<(), ReferenceCaseRunError> {
    planned_case
        .validate()
        .map_err(ReferenceCaseRunError::InvalidPlan)?;
    let contains_process_fault = planned_case
        .actions()
        .iter()
        .any(|action| matches!(action.kind(), PlanActionKind::KillApplication { .. }));
    if client_response_cut_point_actions(planned_case).is_err()
        || webhook_response_cut_point_actions(planned_case).is_err()
        || sql_probe_cut_point_actions(planned_case).is_err()
        || planned_case.actions().iter().any(|action| {
            matches!(
                action.kind(),
                PlanActionKind::KillApplication { cut_point }
                    if !matches!(
                        cut_point,
                        ProcessCutPoint::ClientRequestForwarded
                            | ProcessCutPoint::ClientResponseObserved
                            | ProcessCutPoint::WebhookResponseObserved
                            | ProcessCutPoint::SqlProbe
                    )
            )
        })
        || (contains_process_fault && !process_control_available)
        || (planned_case.actions().iter().any(|action| {
            matches!(
                action.kind(),
                PlanActionKind::KillApplication {
                    cut_point: ProcessCutPoint::SqlProbe
                }
            )
        }) && !sql_probe_available)
    {
        return Err(ReferenceCaseRunError::UnsupportedProcessFault);
    }
    let outcomes = provider_outcomes(planned_case);
    if outcomes.is_empty() {
        return Err(ReferenceCaseRunError::MissingProviderOutcomes);
    }
    Ok(())
}

fn client_response_cut_point_actions(
    planned_case: &PlannedCase,
) -> Result<std::collections::BTreeSet<tiv_core::trace::ActionId>, ReferenceCaseRunError> {
    let mut action_ids = std::collections::BTreeSet::new();
    for actions in planned_case.actions().windows(2) {
        if !matches!(
            actions[1].kind(),
            PlanActionKind::KillApplication {
                cut_point: ProcessCutPoint::ClientResponseObserved
            }
        ) {
            continue;
        }
        match actions[0].kind() {
            PlanActionKind::DriveCheckout { provider_script }
            | PlanActionKind::RetryBusinessRequest { provider_script }
                if provider_script.terminal_outcome() != ProviderOutcome::CommitThenDelay =>
            {
                action_ids.insert(actions[0].id());
            }
            _ => return Err(ReferenceCaseRunError::UnsupportedProcessFault),
        }
    }
    let expected = planned_case
        .actions()
        .iter()
        .filter(|action| {
            matches!(
                action.kind(),
                PlanActionKind::KillApplication {
                    cut_point: ProcessCutPoint::ClientResponseObserved
                }
            )
        })
        .count();
    if action_ids.len() != expected {
        return Err(ReferenceCaseRunError::UnsupportedProcessFault);
    }
    Ok(action_ids)
}

fn webhook_response_cut_point_actions(
    planned_case: &PlannedCase,
) -> Result<std::collections::BTreeSet<tiv_core::trace::ActionId>, ReferenceCaseRunError> {
    let mut action_ids = std::collections::BTreeSet::new();
    for actions in planned_case.actions().windows(2) {
        if !matches!(
            actions[1].kind(),
            PlanActionKind::KillApplication {
                cut_point: ProcessCutPoint::WebhookResponseObserved
            }
        ) {
            continue;
        }
        if matches!(
            actions[0].kind(),
            PlanActionKind::DeliverWebhook | PlanActionKind::DuplicateWebhook
        ) {
            action_ids.insert(actions[0].id());
        } else {
            return Err(ReferenceCaseRunError::UnsupportedProcessFault);
        }
    }
    let expected = planned_case
        .actions()
        .iter()
        .filter(|action| {
            matches!(
                action.kind(),
                PlanActionKind::KillApplication {
                    cut_point: ProcessCutPoint::WebhookResponseObserved
                }
            )
        })
        .count();
    if action_ids.len() != expected {
        return Err(ReferenceCaseRunError::UnsupportedProcessFault);
    }
    Ok(action_ids)
}

fn sql_probe_cut_point_actions(
    planned_case: &PlannedCase,
) -> Result<std::collections::BTreeSet<tiv_core::trace::ActionId>, ReferenceCaseRunError> {
    let mut action_ids = std::collections::BTreeSet::new();
    for actions in planned_case.actions().windows(2) {
        if !matches!(
            actions[1].kind(),
            PlanActionKind::KillApplication {
                cut_point: ProcessCutPoint::SqlProbe
            }
        ) {
            continue;
        }
        if matches!(actions[0].kind(), PlanActionKind::DriveCheckout { .. }) {
            action_ids.insert(actions[0].id());
        } else {
            return Err(ReferenceCaseRunError::UnsupportedProcessFault);
        }
    }
    let expected = planned_case
        .actions()
        .iter()
        .filter(|action| {
            matches!(
                action.kind(),
                PlanActionKind::KillApplication {
                    cut_point: ProcessCutPoint::SqlProbe
                }
            )
        })
        .count();
    if action_ids.len() != expected {
        return Err(ReferenceCaseRunError::UnsupportedProcessFault);
    }
    Ok(action_ids)
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
pub struct ReferenceCaseAdapter<'a> {
    http: CaseHttpAdapter,
    process: Option<&'a mut dyn ReferenceProcessControl>,
    sql_probe: Option<&'a mut ReferenceSqlProbe>,
    sql_probe_action_ids: std::collections::BTreeSet<tiv_core::trace::ActionId>,
    postgres_producer_sequence: u64,
    sql_probe_observed: bool,
    application_healthy: bool,
    quiescence: Option<ReferenceCaseHttpCompletion>,
    checkpoint: Option<ReferenceCaseCheckpoint>,
}

impl<'a> ReferenceCaseAdapter<'a> {
    #[must_use]
    pub(crate) const fn new(
        http: CaseHttpAdapter,
        process: Option<&'a mut dyn ReferenceProcessControl>,
        sql_probe: Option<&'a mut ReferenceSqlProbe>,
        sql_probe_action_ids: std::collections::BTreeSet<tiv_core::trace::ActionId>,
    ) -> Self {
        Self {
            http,
            process,
            sql_probe,
            sql_probe_action_ids,
            postgres_producer_sequence: 0,
            sql_probe_observed: false,
            application_healthy: true,
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

    async fn kill_application(
        &mut self,
        request: &CaseEffectRequest<'_>,
        cut_point: ProcessCutPoint,
    ) -> Result<crate::campaign::CaseEffectOutput, ReferenceCaseError> {
        require_no_outputs(request)?;
        if !matches!(
            cut_point,
            ProcessCutPoint::ClientRequestForwarded
                | ProcessCutPoint::ClientResponseObserved
                | ProcessCutPoint::WebhookResponseObserved
                | ProcessCutPoint::SqlProbe
        ) || !self.application_healthy
        {
            return Err(ReferenceCaseError::InvalidLifecycleOrder);
        }
        if cut_point == ProcessCutPoint::SqlProbe {
            if !self.sql_probe_observed
                || !self
                    .sql_probe
                    .as_deref()
                    .is_some_and(ReferenceSqlProbe::observed)
            {
                return Err(ReferenceCaseError::InvalidLifecycleOrder);
            }
        } else {
            self.http.mark_application_killed(cut_point)?;
        }
        self.process
            .as_deref_mut()
            .ok_or(ReferenceCaseError::MissingProcessControl)?
            .kill_application()
            .await?;
        self.http
            .complete_application_kill(request, cut_point)
            .await?;
        self.application_healthy = false;
        Ok(Vec::new())
    }

    async fn execute_http(
        &mut self,
        request: CaseEffectRequest<'_>,
    ) -> Result<crate::campaign::CaseEffectOutput, ReferenceCaseError> {
        if !self.sql_probe_action_ids.contains(&request.action().id()) {
            return self
                .http
                .execute(request)
                .await
                .map_err(ReferenceCaseError::Http);
        }
        let observation_request = request;
        let probe = self
            .sql_probe
            .as_deref_mut()
            .ok_or(ReferenceCaseError::MissingSqlProbe)?;
        probe
            .require_false()
            .await
            .map_err(|_| ReferenceCaseError::SqlProbe)?;
        let (http_result, probe_result) =
            tokio::join!(self.http.execute(request), probe.observe_true(),);
        let output = http_result.map_err(ReferenceCaseError::Http)?;
        probe_result.map_err(|_| ReferenceCaseError::SqlProbe)?;
        let next_sequence = self
            .postgres_producer_sequence
            .checked_add(1)
            .ok_or(ReferenceCaseError::PostgresObservationSequenceExhausted)?;
        observation_request
            .record_observation(
                crate::journal::ObservationProducer::Postgres,
                next_sequence,
                crate::journal::ObservationEvent::SqlProbeTrue,
            )
            .await
            .map_err(ReferenceCaseError::SqlProbeJournal)?;
        self.postgres_producer_sequence = next_sequence;
        self.sql_probe_observed = true;
        Ok(output)
    }

    async fn restart_application(
        &mut self,
        request: &CaseEffectRequest<'_>,
    ) -> Result<crate::campaign::CaseEffectOutput, ReferenceCaseError> {
        require_no_outputs(request)?;
        if self.application_healthy {
            return Err(ReferenceCaseError::InvalidLifecycleOrder);
        }
        self.process
            .as_deref_mut()
            .ok_or(ReferenceCaseError::MissingProcessControl)?
            .restart_and_await_health()
            .await?;
        self.application_healthy = true;
        Ok(Vec::new())
    }
}

impl CaseEffectAdapter for ReferenceCaseAdapter<'_> {
    type Error = ReferenceCaseError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        Box::pin(async move {
            match request.action().kind() {
                PlanActionKind::KillApplication { cut_point } => {
                    self.kill_application(&request, *cut_point).await
                }
                PlanActionKind::RestartAndAwaitHealth => self.restart_application(&request).await,
                PlanActionKind::WaitForQuiescence => self.wait_for_quiescence(&request).await,
                PlanActionKind::CheckCheckpoint { .. } => self.check_checkpoint(&request),
                _ if self.quiescence.is_some() || self.checkpoint.is_some() => {
                    Err(ReferenceCaseError::InvalidLifecycleOrder)
                }
                _ => self.execute_http(request).await,
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
    #[error("reference case process control is unavailable")]
    MissingProcessControl,
    #[error("reference case SQL probe is unavailable")]
    MissingSqlProbe,
    #[error("reference case SQL probe failed")]
    SqlProbe,
    #[error("reference case SQL-probe journal append failed: {0}")]
    SqlProbeJournal(#[source] crate::journal::JournalError),
    #[error("reference case PostgreSQL observation sequence exhausted")]
    PostgresObservationSequenceExhausted,
    #[error("reference case process action failed: {0}")]
    ProcessControl(#[from] ReferenceProcessControlError),
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
    #[error("reference planned-case process cut point is not supported")]
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

#[cfg(test)]
mod tests {
    use super::*;
    use tiv_core::{
        decision::Seed,
        plan::{ActionBudget, CasePlanCompiler, PlanSpec, ProcessFaultSpec, WebhookFaultSpec},
    };

    #[test]
    fn unsupported_cut_points_fail_preflight_even_when_process_control_exists() {
        let plan = (0..512)
            .find_map(|seed| {
                let spec = PlanSpec::new_payment_intent_v1(
                    Seed::new(seed),
                    ActionBudget::new(40).unwrap(),
                    [ProviderOutcome::Normal, ProviderOutcome::CommitThenDelay],
                    WebhookFaultSpec::new(0, [], false, false).unwrap(),
                    ProcessFaultSpec::new([ProcessCutPoint::SqlProbe], 1).unwrap(),
                )
                .unwrap();
                let plan = CasePlanCompiler::compile(&spec).unwrap();
                plan.actions()
                    .iter()
                    .any(|action| matches!(action.kind(), PlanActionKind::KillApplication { .. }))
                    .then_some(plan)
            })
            .expect("the bounded seed corpus contains a SQL-probe kill");

        assert!(matches!(
            preflight_reference_planned_case(&plan, true, true),
            Err(ReferenceCaseRunError::UnsupportedProcessFault)
        ));
    }

    #[test]
    fn client_response_observed_passes_preflight_with_process_control() {
        let plan = (0..512)
            .find_map(|seed| {
                let spec = PlanSpec::new_payment_intent_v1(
                    Seed::new(seed),
                    ActionBudget::new(40).unwrap(),
                    [ProviderOutcome::Normal],
                    WebhookFaultSpec::new(0, [], false, false).unwrap(),
                    ProcessFaultSpec::new([ProcessCutPoint::ClientResponseObserved], 1).unwrap(),
                )
                .unwrap();
                let plan = CasePlanCompiler::compile(&spec).unwrap();
                plan.actions()
                    .iter()
                    .any(|action| matches!(action.kind(), PlanActionKind::KillApplication { .. }))
                    .then_some(plan)
            })
            .expect("the bounded seed corpus contains a client-response kill");

        preflight_reference_planned_case(&plan, true, false)
            .expect("client-response process control is supported");
    }

    #[test]
    fn client_response_observed_rejects_a_non_application_predecessor() {
        let plan = (0..4_096)
            .find_map(|seed| {
                let spec = PlanSpec::new_payment_intent_v1(
                    Seed::new(seed),
                    ActionBudget::new(40).unwrap(),
                    [ProviderOutcome::Normal],
                    WebhookFaultSpec::new(0, [], false, false).unwrap(),
                    ProcessFaultSpec::new([ProcessCutPoint::ClientResponseObserved], 1).unwrap(),
                )
                .unwrap();
                let plan = CasePlanCompiler::compile(&spec).unwrap();
                plan.actions()
                    .windows(2)
                    .any(|actions| {
                        matches!(
                            actions[1].kind(),
                            PlanActionKind::KillApplication {
                                cut_point: ProcessCutPoint::ClientResponseObserved
                            }
                        ) && !matches!(
                            actions[0].kind(),
                            PlanActionKind::DriveCheckout { .. }
                                | PlanActionKind::RetryBusinessRequest { .. }
                        )
                    })
                    .then_some(plan)
            })
            .expect("the seed corpus reaches an abstract non-application response cut point");

        assert!(matches!(
            preflight_reference_planned_case(&plan, true, false),
            Err(ReferenceCaseRunError::UnsupportedProcessFault)
        ));
    }

    #[test]
    fn webhook_response_observed_passes_preflight_after_a_real_delivery() {
        let plan = (0..4_096)
            .find_map(|seed| {
                let spec = PlanSpec::new_payment_intent_v1(
                    Seed::new(seed),
                    ActionBudget::new(40).unwrap(),
                    [ProviderOutcome::Normal],
                    WebhookFaultSpec::new(1, [], false, false).unwrap(),
                    ProcessFaultSpec::new([ProcessCutPoint::WebhookResponseObserved], 1).unwrap(),
                )
                .unwrap();
                let plan = CasePlanCompiler::compile(&spec).unwrap();
                plan.actions()
                    .windows(2)
                    .any(|actions| {
                        matches!(
                            actions[0].kind(),
                            PlanActionKind::DeliverWebhook | PlanActionKind::DuplicateWebhook
                        ) && matches!(
                            actions[1].kind(),
                            PlanActionKind::KillApplication {
                                cut_point: ProcessCutPoint::WebhookResponseObserved
                            }
                        )
                    })
                    .then_some(plan)
            })
            .expect("the seed corpus contains a webhook-response delivery cut point");

        preflight_reference_planned_case(&plan, true, false)
            .expect("a real fixture webhook response can own the process cut point");
    }

    #[test]
    fn sql_probe_passes_preflight_only_after_the_initial_checkout() {
        let plan = (0..4_096)
            .find_map(|seed| {
                let spec = PlanSpec::new_payment_intent_v1(
                    Seed::new(seed),
                    ActionBudget::new(40).unwrap(),
                    [ProviderOutcome::Normal],
                    WebhookFaultSpec::new(0, [], false, false).unwrap(),
                    ProcessFaultSpec::new([ProcessCutPoint::SqlProbe], 1).unwrap(),
                )
                .unwrap();
                let plan = CasePlanCompiler::compile(&spec).unwrap();
                plan.actions()
                    .windows(2)
                    .any(|actions| {
                        matches!(actions[0].kind(), PlanActionKind::DriveCheckout { .. })
                            && matches!(
                                actions[1].kind(),
                                PlanActionKind::KillApplication {
                                    cut_point: ProcessCutPoint::SqlProbe
                                }
                            )
                    })
                    .then_some(plan)
            })
            .expect("the seed corpus contains a checkout-owned SQL probe cut point");

        assert!(matches!(
            preflight_reference_planned_case(&plan, true, false),
            Err(ReferenceCaseRunError::UnsupportedProcessFault)
        ));
        preflight_reference_planned_case(&plan, true, true)
            .expect("a real read-only payment probe can own this cut point");
    }
}
