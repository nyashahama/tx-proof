//! Serial composition of the provider and webhook HTTP effect boundaries.

use thiserror::Error;
use tiv_core::plan::PlanActionKind;

use crate::{
    campaign::{CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest},
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
}
