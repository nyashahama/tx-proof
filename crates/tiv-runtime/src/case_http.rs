//! Serial composition of the provider and webhook HTTP effect boundaries.

use thiserror::Error;
use tiv_core::plan::{PlanActionKind, ProcessCutPoint};

use crate::{
    campaign::{CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest},
    postgres::oracle::ProviderPaymentIntent,
    provider_http::{ProviderHttpAdapter, ProviderHttpError},
    webhook_http::{WebhookHttpAdapter, WebhookHttpError},
};

/// Owns both fixture-facing HTTP adapters for one serial case.
pub struct CaseHttpAdapter {
    provider: ProviderHttpAdapter,
    webhook: WebhookHttpAdapter,
}

impl CaseHttpAdapter {
    #[must_use]
    pub const fn new(provider: ProviderHttpAdapter, webhook: WebhookHttpAdapter) -> Self {
        Self { provider, webhook }
    }

    pub(crate) async fn await_reference_quiescence(
        &self,
    ) -> Result<ReferenceCaseHttpCompletion, CaseHttpError> {
        if !self.provider.is_idle() || !self.webhook.is_idle() {
            return Err(CaseHttpError::NotQuiescent);
        }
        let provider_payment_intents = self
            .provider
            .quiescent_provider_projection()
            .await
            .map_err(CaseHttpError::Provider)?;
        Ok(ReferenceCaseHttpCompletion {
            provider_payment_intents,
        })
    }

    pub(crate) fn mark_application_killed(
        &mut self,
        cut_point: ProcessCutPoint,
    ) -> Result<(), CaseHttpError> {
        match cut_point {
            ProcessCutPoint::WebhookRequestForwarded => self
                .webhook
                .require_forwarded_request()
                .map_err(CaseHttpError::Webhook),
            ProcessCutPoint::WebhookResponseObserved => self
                .webhook
                .require_observed_response()
                .map_err(CaseHttpError::Webhook),
            _ => self
                .provider
                .mark_application_killed(cut_point)
                .map_err(CaseHttpError::Provider),
        }
    }

    pub(crate) async fn complete_application_kill(
        &mut self,
        request: &CaseEffectRequest<'_>,
        cut_point: ProcessCutPoint,
    ) -> Result<(), CaseHttpError> {
        if !matches!(
            cut_point,
            ProcessCutPoint::WebhookRequestForwarded | ProcessCutPoint::WebhookResponseObserved
        ) {
            return Ok(());
        }
        match cut_point {
            ProcessCutPoint::WebhookRequestForwarded => self
                .webhook
                .discard_forwarded_request(request)
                .await
                .map_err(CaseHttpError::Webhook)?,
            ProcessCutPoint::WebhookResponseObserved => self
                .webhook
                .discard_observed_response(request)
                .await
                .map_err(CaseHttpError::Webhook)?,
            _ => unreachable!("the webhook cut-point match is closed above"),
        }
        self.provider
            .synchronize_control_sequence(self.webhook.control_sequence())
            .map_err(CaseHttpError::Provider)?;
        self.provider
            .synchronize_fixture_producer_sequence(self.webhook.fixture_producer_sequence())
            .map_err(CaseHttpError::Provider)
    }

    async fn execute_provider(
        &mut self,
        request: CaseEffectRequest<'_>,
    ) -> Result<crate::campaign::CaseEffectOutput, CaseHttpError> {
        let output = self
            .provider
            .execute(request)
            .await
            .map_err(CaseHttpError::Provider)?;
        self.webhook
            .synchronize_control_sequence(self.provider.control_sequence())
            .map_err(CaseHttpError::Webhook)?;
        self.webhook
            .synchronize_fixture_producer_sequence(self.provider.fixture_producer_sequence())
            .map_err(CaseHttpError::Webhook)?;
        Ok(output)
    }

    async fn execute_webhook(
        &mut self,
        request: CaseEffectRequest<'_>,
    ) -> Result<crate::campaign::CaseEffectOutput, CaseHttpError> {
        let output = self
            .webhook
            .execute(request)
            .await
            .map_err(CaseHttpError::Webhook)?;
        self.provider
            .synchronize_control_sequence(self.webhook.control_sequence())
            .map_err(CaseHttpError::Provider)?;
        self.provider
            .synchronize_fixture_producer_sequence(self.webhook.fixture_producer_sequence())
            .map_err(CaseHttpError::Provider)?;
        Ok(output)
    }
}

impl CaseEffectAdapter for CaseHttpAdapter {
    type Error = CaseHttpError;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        Box::pin(async move {
            match request.action().kind() {
                PlanActionKind::DriveCheckout { .. }
                | PlanActionKind::RetrievePaymentIntent
                | PlanActionKind::ReleaseProviderGate
                | PlanActionKind::RetryBusinessRequest { .. }
                | PlanActionKind::ConfirmPaymentIntent { .. }
                | PlanActionKind::RetryProviderRequest { .. } => {
                    self.execute_provider(request).await
                }
                PlanActionKind::GenerateProviderEvent
                | PlanActionKind::DeliverWebhook
                | PlanActionKind::DuplicateWebhook
                | PlanActionKind::DelayWebhook { .. }
                | PlanActionKind::ReorderWebhooks
                | PlanActionKind::DropWebhook => self.execute_webhook(request).await,
                PlanActionKind::KillApplication { .. }
                | PlanActionKind::RestartAndAwaitHealth
                | PlanActionKind::WaitForQuiescence
                | PlanActionKind::CheckCheckpoint { .. } => Err(CaseHttpError::UnsupportedAction),
            }
        })
    }
}

#[derive(Debug, Error)]
pub enum CaseHttpError {
    #[error("provider HTTP action failed: {0}")]
    Provider(#[source] ProviderHttpError),
    #[error("webhook HTTP action failed: {0}")]
    Webhook(#[source] WebhookHttpError),
    #[error("action is outside the combined HTTP adapter boundary")]
    UnsupportedAction,
    #[error("the provider or webhook HTTP boundary is not quiescent")]
    NotQuiescent,
}

impl CaseHttpError {
    pub(crate) const fn is_inconclusive(&self) -> bool {
        match self {
            Self::Provider(error) => error.is_inconclusive(),
            Self::Webhook(error) => error.is_inconclusive(),
            Self::UnsupportedAction | Self::NotQuiescent => false,
        }
    }
}

pub(crate) struct ReferenceCaseHttpCompletion {
    provider_payment_intents: Vec<ProviderPaymentIntent>,
}

impl ReferenceCaseHttpCompletion {
    pub(crate) fn into_provider_payment_intents(self) -> Vec<ProviderPaymentIntent> {
        self.provider_payment_intents
    }
}
