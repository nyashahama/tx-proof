//! Reference-application planned-case execution and checkpoint evaluation.

use std::{future::Future, path::Path, pin::Pin, time::Duration};

use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use thiserror::Error;
use tiv_core::{
    decision::Seed,
    plan::{
        PlanActionKind, PlanValidationError, PlannedAction, PlannedCase, ProcessCutPoint,
        ProviderOutcome,
    },
    shrink::ShrinkCandidate,
};

use crate::{
    campaign::{
        CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest, CaseExecutionCause,
        CaseExecutionError, ExecutedCase, ExecutedShrinkCase, execute_planned_case,
        execute_shrink_candidate,
    },
    case_http::{CaseHttpAdapter, CaseHttpError, ReferenceCaseHttpCompletion},
    postgres::{
        oracle::{ProviderPaymentIntent, QuiescencePermit},
        quiescence::{DatabaseQuiescenceCompletion, QuiescenceError},
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

pub(crate) type CaseSqlProbeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), CaseSqlProbeError>> + Send + 'a>>;

pub(crate) trait CaseSqlProbe: Send {
    fn require_false(&mut self) -> CaseSqlProbeFuture<'_>;
    fn observe_true(&mut self) -> CaseSqlProbeFuture<'_>;
    fn observed(&self) -> bool;
}

impl CaseSqlProbe for ReferenceSqlProbe {
    fn require_false(&mut self) -> CaseSqlProbeFuture<'_> {
        Box::pin(async move {
            ReferenceSqlProbe::require_false(self)
                .await
                .map_err(|_| CaseSqlProbeError)
        })
    }

    fn observe_true(&mut self) -> CaseSqlProbeFuture<'_> {
        Box::pin(async move {
            ReferenceSqlProbe::observe_true(self)
                .await
                .map_err(|_| CaseSqlProbeError)
        })
    }

    fn observed(&self) -> bool {
        ReferenceSqlProbe::observed(self)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("the configured SQL probe failed")]
pub(crate) struct CaseSqlProbeError;

pub(crate) type CaseQuiescenceFuture<'a> = Pin<
    Box<dyn Future<Output = Result<DatabaseQuiescenceCompletion, QuiescenceError>> + Send + 'a>,
>;

pub(crate) trait CaseQuiescenceGate: Send {
    fn await_stable(&mut self) -> CaseQuiescenceFuture<'_>;
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
        let reference_app_url = reference_app_url.as_ref().trim_end_matches('/');
        let operation_id = operation_id.into();
        let currency = currency.into();
        Self::from_http_contract(
            case_database,
            format!("{reference_app_url}/checkout"),
            serde_json::json!({
                "database": case_database.as_str(),
                "operation_id": operation_id,
                "amount_minor": amount_minor,
                "currency": currency,
            }),
            reference_app_url,
            fixture_control_url,
            fixture_control_token,
            reset_sequence,
            first_webhook_timestamp,
            timeout,
            poll_interval,
        )
    }

    /// Builds the case adapters from one explicit configured loopback HTTP
    /// contract rather than the reference application's fixed route/body.
    ///
    /// # Errors
    ///
    /// Returns [`ReferenceCaseRunConfigError`] unless the case database and all
    /// provider, driver, fixture-control, and timing inputs are bounded.
    #[allow(clippy::too_many_arguments)]
    pub fn from_http_contract(
        case_database: &DatabaseName,
        driver_url: impl AsRef<str>,
        driver_body: serde_json::Value,
        provider_proxy_url: impl AsRef<str>,
        fixture_control_url: impl AsRef<str>,
        fixture_control_token: impl Into<String>,
        reset_sequence: u64,
        first_webhook_timestamp: i64,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Self, ReferenceCaseRunConfigError> {
        if case_database.kind() != DatabaseKind::Case {
            return Err(ReferenceCaseRunConfigError::InvalidCaseDatabase);
        }
        let fixture_control_url_value = fixture_control_url.as_ref();
        let fixture_control_token = fixture_control_token.into();
        let provider = ProviderHttpConfig::new(
            driver_url,
            driver_body,
            provider_proxy_url,
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

pub(crate) struct ReferenceShrinkCaseRunReceipt {
    executed: ExecutedShrinkCase,
    checkpoint: ReferenceCaseCheckpoint,
}

impl ReferenceShrinkCaseRunReceipt {
    pub(crate) fn into_parts(self) -> (ExecutedShrinkCase, ReferenceCaseCheckpoint) {
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
    sql_probe: Option<&mut dyn CaseSqlProbe>,
) -> Result<ReferenceCaseRunReceipt, ReferenceCaseRunError> {
    run_reference_planned_case_inner(
        run_id,
        case_id,
        planned_case,
        journal_path,
        config,
        Some(process),
        sql_probe,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_planned_case_with_process(
    run_id: impl Into<String>,
    case_id: impl Into<String>,
    planned_case: &PlannedCase,
    journal_path: impl AsRef<Path>,
    config: ReferenceCaseRunConfig,
    process: &mut dyn ReferenceProcessControl,
    sql_probe: Option<&mut dyn CaseSqlProbe>,
    quiescence_gate: &mut dyn CaseQuiescenceGate,
) -> Result<ReferenceCaseRunReceipt, ReferenceCaseRunError> {
    run_reference_planned_case_inner(
        run_id,
        case_id,
        planned_case,
        journal_path,
        config,
        Some(process),
        sql_probe,
        Some(quiescence_gate),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_configured_shrink_candidate_with_process(
    run_id: impl Into<String>,
    case_id: impl Into<String>,
    candidate: &ShrinkCandidate,
    journal_path: impl AsRef<Path>,
    config: ReferenceCaseRunConfig,
    process: &mut dyn ReferenceProcessControl,
    sql_probe: Option<&mut dyn CaseSqlProbe>,
    quiescence_gate: &mut dyn CaseQuiescenceGate,
) -> Result<ReferenceShrinkCaseRunReceipt, ReferenceCaseRunError> {
    candidate
        .validate()
        .map_err(|_| ReferenceCaseRunError::InvalidShrinkCandidate)?;
    preflight_reference_actions(candidate.actions(), true, sql_probe.is_some())?;
    let client_response_cut_points = client_response_cut_point_actions(candidate.actions())?;
    let webhook_request_cut_points = webhook_request_cut_point_actions(candidate.actions())?;
    let webhook_response_cut_points = webhook_response_cut_point_actions(candidate.actions())?;
    let sql_probe_cut_points = sql_probe_cut_point_actions(candidate.actions())?;
    let outcomes = provider_outcomes(candidate.actions());
    reset_fixture(&config, candidate.source().seed(), &outcomes).await?;
    let mut adapter = ReferenceCaseAdapter::new(
        CaseHttpAdapter::new(
            ProviderHttpAdapter::new(config.provider)?
                .with_client_response_cut_points(client_response_cut_points),
            WebhookHttpAdapter::new(config.webhook)?
                .with_webhook_request_cut_points(webhook_request_cut_points)
                .with_webhook_response_cut_points(webhook_response_cut_points),
        ),
        Some(process),
        sql_probe,
        sql_probe_cut_points,
        Some(quiescence_gate),
    );
    let executed =
        execute_shrink_candidate(run_id, case_id, candidate, journal_path, &mut adapter).await?;
    let checkpoint = adapter.finish()?;
    Ok(ReferenceShrinkCaseRunReceipt {
        executed,
        checkpoint,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_reference_planned_case_inner(
    run_id: impl Into<String>,
    case_id: impl Into<String>,
    planned_case: &PlannedCase,
    journal_path: impl AsRef<Path>,
    config: ReferenceCaseRunConfig,
    process: Option<&mut dyn ReferenceProcessControl>,
    sql_probe: Option<&mut dyn CaseSqlProbe>,
    quiescence_gate: Option<&mut dyn CaseQuiescenceGate>,
) -> Result<ReferenceCaseRunReceipt, ReferenceCaseRunError> {
    preflight_reference_planned_case(planned_case, process.is_some(), sql_probe.is_some())?;
    let client_response_cut_points = client_response_cut_point_actions(planned_case.actions())?;
    let webhook_request_cut_points = webhook_request_cut_point_actions(planned_case.actions())?;
    let webhook_response_cut_points = webhook_response_cut_point_actions(planned_case.actions())?;
    let sql_probe_cut_points = sql_probe_cut_point_actions(planned_case.actions())?;
    let outcomes = provider_outcomes(planned_case.actions());
    reset_fixture(&config, planned_case.seed(), &outcomes).await?;
    let mut adapter = ReferenceCaseAdapter::new(
        CaseHttpAdapter::new(
            ProviderHttpAdapter::new(config.provider)?
                .with_client_response_cut_points(client_response_cut_points),
            WebhookHttpAdapter::new(config.webhook)?
                .with_webhook_request_cut_points(webhook_request_cut_points)
                .with_webhook_response_cut_points(webhook_response_cut_points),
        ),
        process,
        sql_probe,
        sql_probe_cut_points,
        quiescence_gate,
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
    preflight_reference_actions(
        planned_case.actions(),
        process_control_available,
        sql_probe_available,
    )
}

fn preflight_reference_actions(
    actions: &[PlannedAction],
    process_control_available: bool,
    sql_probe_available: bool,
) -> Result<(), ReferenceCaseRunError> {
    let contains_process_fault = actions
        .iter()
        .any(|action| matches!(action.kind(), PlanActionKind::KillApplication { .. }));
    if client_request_cut_point_actions(actions).is_err()
        || client_response_cut_point_actions(actions).is_err()
        || webhook_request_cut_point_actions(actions).is_err()
        || webhook_response_cut_point_actions(actions).is_err()
        || sql_probe_cut_point_actions(actions).is_err()
        || actions.iter().any(|action| {
            matches!(
                action.kind(),
                PlanActionKind::KillApplication { cut_point }
                    if !matches!(
                        cut_point,
                        ProcessCutPoint::ClientRequestForwarded
                            | ProcessCutPoint::ClientResponseObserved
                            | ProcessCutPoint::WebhookRequestForwarded
                            | ProcessCutPoint::WebhookResponseObserved
                            | ProcessCutPoint::SqlProbe
                    )
            )
        })
        || (contains_process_fault && !process_control_available)
        || (actions.iter().any(|action| {
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
    let outcomes = provider_outcomes(actions);
    if outcomes.is_empty() {
        return Err(ReferenceCaseRunError::MissingProviderOutcomes);
    }
    Ok(())
}

fn client_request_cut_point_actions(
    planned_actions: &[PlannedAction],
) -> Result<std::collections::BTreeSet<tiv_core::trace::ActionId>, ReferenceCaseRunError> {
    let mut action_ids = std::collections::BTreeSet::new();
    for actions in planned_actions.windows(2) {
        if !matches!(
            actions[1].kind(),
            PlanActionKind::KillApplication {
                cut_point: ProcessCutPoint::ClientRequestForwarded
            }
        ) {
            continue;
        }
        match actions[0].kind() {
            PlanActionKind::DriveCheckout { provider_script }
            | PlanActionKind::RetryBusinessRequest { provider_script }
                if provider_script.terminal_outcome() == ProviderOutcome::CommitThenDelay =>
            {
                action_ids.insert(actions[0].id());
            }
            _ => return Err(ReferenceCaseRunError::UnsupportedProcessFault),
        }
    }
    let expected = planned_actions
        .iter()
        .filter(|action| {
            matches!(
                action.kind(),
                PlanActionKind::KillApplication {
                    cut_point: ProcessCutPoint::ClientRequestForwarded
                }
            )
        })
        .count();
    if action_ids.len() != expected {
        return Err(ReferenceCaseRunError::UnsupportedProcessFault);
    }
    Ok(action_ids)
}

fn client_response_cut_point_actions(
    planned_actions: &[PlannedAction],
) -> Result<std::collections::BTreeSet<tiv_core::trace::ActionId>, ReferenceCaseRunError> {
    let mut action_ids = std::collections::BTreeSet::new();
    for actions in planned_actions.windows(2) {
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
    let expected = planned_actions
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
    planned_actions: &[PlannedAction],
) -> Result<std::collections::BTreeSet<tiv_core::trace::ActionId>, ReferenceCaseRunError> {
    let mut action_ids = std::collections::BTreeSet::new();
    for actions in planned_actions.windows(2) {
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
    let expected = planned_actions
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

fn webhook_request_cut_point_actions(
    planned_actions: &[PlannedAction],
) -> Result<std::collections::BTreeSet<tiv_core::trace::ActionId>, ReferenceCaseRunError> {
    let mut action_ids = std::collections::BTreeSet::new();
    for actions in planned_actions.windows(2) {
        if !matches!(
            actions[1].kind(),
            PlanActionKind::KillApplication {
                cut_point: ProcessCutPoint::WebhookRequestForwarded
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
    let expected = planned_actions
        .iter()
        .filter(|action| {
            matches!(
                action.kind(),
                PlanActionKind::KillApplication {
                    cut_point: ProcessCutPoint::WebhookRequestForwarded
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
    planned_actions: &[PlannedAction],
) -> Result<std::collections::BTreeSet<tiv_core::trace::ActionId>, ReferenceCaseRunError> {
    let mut action_ids = std::collections::BTreeSet::new();
    for actions in planned_actions.windows(2) {
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
    let expected = planned_actions
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

fn provider_outcomes(planned_actions: &[PlannedAction]) -> Vec<ProviderOutcome> {
    planned_actions
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
    seed: Seed,
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
            "seed": seed.value(),
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
pub struct ReferenceCaseAdapter<'process, 'probe, 'quiescence> {
    http: CaseHttpAdapter,
    process: Option<&'process mut dyn ReferenceProcessControl>,
    sql_probe: Option<&'probe mut dyn CaseSqlProbe>,
    quiescence_gate: Option<&'quiescence mut dyn CaseQuiescenceGate>,
    sql_probe_action_ids: std::collections::BTreeSet<tiv_core::trace::ActionId>,
    postgres_producer_sequence: u64,
    sql_probe_observed: bool,
    application_healthy: bool,
    quiescence: Option<ReferenceCaseHttpCompletion>,
    database_quiescence: Option<DatabaseQuiescenceCompletion>,
    checkpoint: Option<ReferenceCaseCheckpoint>,
}

impl<'process, 'probe, 'quiescence> ReferenceCaseAdapter<'process, 'probe, 'quiescence> {
    #[must_use]
    pub(crate) const fn new(
        http: CaseHttpAdapter,
        process: Option<&'process mut dyn ReferenceProcessControl>,
        sql_probe: Option<&'probe mut dyn CaseSqlProbe>,
        sql_probe_action_ids: std::collections::BTreeSet<tiv_core::trace::ActionId>,
        quiescence_gate: Option<&'quiescence mut dyn CaseQuiescenceGate>,
    ) -> Self {
        Self {
            http,
            process,
            sql_probe,
            quiescence_gate,
            sql_probe_action_ids,
            postgres_producer_sequence: 0,
            sql_probe_observed: false,
            application_healthy: true,
            quiescence: None,
            database_quiescence: None,
            checkpoint: None,
        }
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) const fn new_configured(
        http: CaseHttpAdapter,
        process: Option<&'process mut dyn ReferenceProcessControl>,
        sql_probe: Option<&'probe mut dyn CaseSqlProbe>,
        sql_probe_action_ids: std::collections::BTreeSet<tiv_core::trace::ActionId>,
        quiescence_gate: &'quiescence mut dyn CaseQuiescenceGate,
    ) -> Self {
        Self::new(
            http,
            process,
            sql_probe,
            sql_probe_action_ids,
            Some(quiescence_gate),
        )
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
        let http_completion = self.http.await_reference_quiescence().await?;
        let database_completion = match self.quiescence_gate.as_deref_mut() {
            Some(gate) => Some(
                gate.await_stable()
                    .await
                    .map_err(ReferenceCaseError::DatabaseQuiescence)?,
            ),
            None => None,
        };
        self.quiescence = Some(http_completion);
        self.database_quiescence = database_completion;
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
        let quiescence = match (
            self.quiescence_gate.is_some(),
            self.database_quiescence.take(),
        ) {
            (false, None) => QuiescencePermit::after_reference_case_http_quiescent(&completion),
            (true, Some(database_completion)) => {
                QuiescencePermit::after_configured_case_quiescent(&completion, database_completion)
            }
            _ => return Err(ReferenceCaseError::InvalidLifecycleOrder),
        };
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
                | ProcessCutPoint::WebhookRequestForwarded
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
                    .is_some_and(CaseSqlProbe::observed)
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

impl CaseEffectAdapter for ReferenceCaseAdapter<'_, '_, '_> {
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
    #[error("configured reference case database quiescence failed: {0}")]
    DatabaseQuiescence(#[source] QuiescenceError),
    #[error("reference case SQL-probe journal append failed: {0}")]
    SqlProbeJournal(#[source] crate::journal::JournalError),
    #[error("reference case PostgreSQL observation sequence exhausted")]
    PostgresObservationSequenceExhausted,
    #[error("reference case process action failed: {0}")]
    ProcessControl(#[from] ReferenceProcessControlError),
}

impl ReferenceCaseError {
    pub(crate) const fn is_inconclusive(&self) -> bool {
        match self {
            Self::Http(error) => error.is_inconclusive(),
            Self::SqlProbe | Self::DatabaseQuiescence(QuiescenceError::Timeout) => true,
            Self::InvalidLifecycleOrder
            | Self::UnexpectedOutputContract
            | Self::MissingCheckpoint
            | Self::MissingProcessControl
            | Self::MissingSqlProbe
            | Self::DatabaseQuiescence(_)
            | Self::SqlProbeJournal(_)
            | Self::PostgresObservationSequenceExhausted
            | Self::ProcessControl(_) => false,
        }
    }
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
    #[error("reference shrink candidate failed pure validation")]
    InvalidShrinkCandidate,
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
    #[error("reference planned-case serial execution failed: {0:?}")]
    Execution(CaseExecutionError<ReferenceCaseError>),
    #[error("reference planned-case lifecycle did not complete: {0}")]
    Lifecycle(#[from] ReferenceCaseError),
}

impl From<CaseExecutionError<ReferenceCaseError>> for ReferenceCaseRunError {
    fn from(error: CaseExecutionError<ReferenceCaseError>) -> Self {
        Self::Execution(error)
    }
}

impl ReferenceCaseRunError {
    pub(crate) fn is_inconclusive(&self) -> bool {
        match self {
            Self::Execution(CaseExecutionError::Failed(failure)) => {
                matches!(failure.cause(), CaseExecutionCause::Effect(error) if error.is_inconclusive())
            }
            Self::Lifecycle(error) => error.is_inconclusive(),
            Self::InvalidPlan(_)
            | Self::InvalidShrinkCandidate
            | Self::UnsupportedProcessFault
            | Self::MissingProviderOutcomes
            | Self::ResetRequest(_)
            | Self::UnexpectedResetStatus(_)
            | Self::UnexpectedResetResponse
            | Self::ProviderAdapter(_)
            | Self::WebhookAdapter(_)
            | Self::Execution(_) => false,
        }
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
    fn client_request_forwarded_rejects_a_provider_confirmation_predecessor() {
        let plan = (0..1_024)
            .find_map(|seed| {
                let spec = PlanSpec::new_payment_intent_v1(
                    Seed::new(seed),
                    ActionBudget::new(40).unwrap(),
                    [ProviderOutcome::Normal, ProviderOutcome::CommitThenDelay],
                    WebhookFaultSpec::new(0, [], false, false).unwrap(),
                    ProcessFaultSpec::new([ProcessCutPoint::ClientRequestForwarded], 1).unwrap(),
                )
                .unwrap();
                let plan = CasePlanCompiler::compile(&spec).unwrap();
                plan.actions()
                    .windows(2)
                    .any(|actions| {
                        matches!(
                            actions[0].kind(),
                            PlanActionKind::ConfirmPaymentIntent { .. }
                                | PlanActionKind::RetryProviderRequest { .. }
                        ) && matches!(
                            actions[1].kind(),
                            PlanActionKind::KillApplication {
                                cut_point: ProcessCutPoint::ClientRequestForwarded
                            }
                        )
                    })
                    .then_some(plan)
            })
            .expect("the bounded seed corpus contains a confirmation-owned request cut point");

        assert!(matches!(
            preflight_reference_planned_case(&plan, true, false),
            Err(ReferenceCaseRunError::UnsupportedProcessFault)
        ));
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
    fn webhook_request_forwarded_passes_preflight_only_for_an_adjacent_delivery() {
        let plan = (0..4_096)
            .find_map(|seed| {
                let spec = PlanSpec::new_payment_intent_v1(
                    Seed::new(seed),
                    ActionBudget::new(40).unwrap(),
                    [ProviderOutcome::Normal],
                    WebhookFaultSpec::new(1, [], false, false).unwrap(),
                    ProcessFaultSpec::new([ProcessCutPoint::WebhookRequestForwarded], 1).unwrap(),
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
                                cut_point: ProcessCutPoint::WebhookRequestForwarded
                            }
                        )
                    })
                    .then_some(plan)
            })
            .expect("the seed corpus contains a webhook-request delivery cut point");

        preflight_reference_planned_case(&plan, true, false)
            .expect("an instrumented fixture ingress owns the request-forwarded cut point");
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

    #[test]
    fn configured_case_accepts_an_explicit_driver_and_provider_proxy_contract() {
        let case_database = DatabaseName::parse("tiv_case_deadbeef").unwrap();

        ReferenceCaseRunConfig::from_http_contract(
            &case_database,
            "http://127.0.0.1:18080/custom-checkout",
            serde_json::json!({
                "database": "tiv_case_deadbeef",
                "operation_id": "op_deadbeef",
                "amount_minor": 2_500,
                "currency": "usd",
            }),
            "http://127.0.0.1:18080",
            "http://127.0.0.1:12112",
            "fixture-control-token",
            1,
            1_800_000_000,
            Duration::from_secs(10),
            Duration::from_millis(10),
        )
        .expect("the configured loopback payment_intent_v1 contract is valid");
    }

    struct NeverQuiescenceGate;

    impl CaseQuiescenceGate for NeverQuiescenceGate {
        fn await_stable(&mut self) -> CaseQuiescenceFuture<'_> {
            Box::pin(async { unreachable!("constructor contract does not execute the gate") })
        }
    }

    #[test]
    fn configured_adapter_requires_an_explicit_database_quiescence_gate() {
        let case_database = DatabaseName::parse("tiv_case_deadbeef").unwrap();
        let config = ReferenceCaseRunConfig::from_http_contract(
            &case_database,
            "http://127.0.0.1:18080/checkout",
            serde_json::json!({
                "operation_id": "op_deadbeef",
                "amount_minor": 2_500,
                "currency": "usd",
            }),
            "http://127.0.0.1:18080",
            "http://127.0.0.1:12112",
            "fixture-control-token",
            1,
            1_800_000_000,
            Duration::from_secs(10),
            Duration::from_millis(10),
        )
        .unwrap();
        let http = CaseHttpAdapter::new(
            ProviderHttpAdapter::new(config.provider).unwrap(),
            WebhookHttpAdapter::new(config.webhook).unwrap(),
        );
        let mut gate = NeverQuiescenceGate;

        let _adapter = ReferenceCaseAdapter::new_configured(
            http,
            None,
            None,
            std::collections::BTreeSet::new(),
            &mut gate,
        );
    }
}
