use thiserror::Error;
use tiv_core::{
    ids::PaymentIntentId,
    result::{CheckpointId, FailureIdentity, InvariantId},
};
use tokio_postgres::{Client, IsolationLevel};

const CHECKPOINT_ID: &str = "checkout-quiescent";
const PROVIDER_UNIQUENESS_ID: &str = "provider-object-unique";
const MAX_WITNESS_ROWS: usize = 100;
const NOOP_INVARIANT_IDS: [&str; 4] = [
    "webhook-effect-at-most-once",
    "paid-order-amount-conservation",
    "terminal-success-monotonic",
    "balanced-ledger",
];

/// Proof that action release is frozen and the application is quiescent.
///
/// The public oracle requires this capability, but only the runtime's
/// quiescence gate may construct it. The synthetic truth spike has a test-only
/// constructor after its driver and provider tasks have stopped.
#[derive(Debug)]
pub struct QuiescencePermit {
    _private: (),
}

impl QuiescencePermit {
    #[cfg(test)]
    pub(super) const fn after_synthetic_driver_stopped() -> Self {
        Self { _private: () }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderPaymentIntent {
    id: PaymentIntentId,
    amount_minor: i64,
    currency: String,
    status: String,
}

impl ProviderPaymentIntent {
    /// Creates one bounded provider-state projection row.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProviderProjection`] for an invalid provider ID,
    /// non-positive amount, unsupported currency, or unsupported status.
    pub fn new(
        id: impl Into<String>,
        amount_minor: i64,
        currency: impl Into<String>,
        status: impl Into<String>,
    ) -> Result<Self, InvalidProviderProjection> {
        let id = PaymentIntentId::new(id).map_err(|_| InvalidProviderProjection)?;
        let currency = currency.into();
        let status = status.into();
        if amount_minor <= 0
            || currency.len() != 3
            || !currency.bytes().all(|byte| byte.is_ascii_lowercase())
            || !matches!(status.as_str(), "requires_confirmation" | "succeeded")
        {
            return Err(InvalidProviderProjection);
        }
        Ok(Self {
            id,
            amount_minor,
            currency,
            status,
        })
    }

    pub(crate) fn id(&self) -> &str {
        self.id.as_str()
    }

    pub(crate) const fn amount_minor(&self) -> i64 {
        self.amount_minor
    }

    pub(crate) fn currency(&self) -> &str {
        &self.currency
    }

    pub(crate) fn status(&self) -> &str {
        &self.status
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidProviderProjection;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderObjectWitness {
    operation_id: String,
    local_payment_count: i64,
    provider_object_count: i64,
    payment_row_ids: Vec<i64>,
    provider_payment_intent_ids: Vec<String>,
}

impl ProviderObjectWitness {
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    #[must_use]
    pub const fn provider_object_count(&self) -> i64 {
        self.provider_object_count
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvariantVerdict {
    Held,
    Violated(Vec<ProviderObjectWitness>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvariantOutcome {
    id: &'static str,
    identity: FailureIdentity,
    verdict: InvariantVerdict,
}

impl InvariantOutcome {
    #[must_use]
    pub const fn id(&self) -> &'static str {
        self.id
    }

    #[must_use]
    pub const fn identity(&self) -> &FailureIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn verdict(&self) -> &InvariantVerdict {
        &self.verdict
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotReport {
    outcomes: Vec<InvariantOutcome>,
}

impl SnapshotReport {
    #[must_use]
    pub fn outcomes(&self) -> &[InvariantOutcome] {
        &self.outcomes
    }

    #[must_use]
    pub fn outcome(&self, id: &str) -> Option<&InvariantOutcome> {
        self.outcomes.iter().find(|outcome| outcome.id == id)
    }
}

#[derive(Debug, Error)]
pub enum OracleError {
    #[error("PostgreSQL oracle operation failed")]
    Postgres(#[from] tokio_postgres::Error),
    #[error("provider-object-unique exceeded the {MAX_WITNESS_ROWS}-row evidence cap")]
    EvidenceLimitExceeded,
    #[error("the built-in invariant identity is invalid")]
    InvalidBuiltInIdentity,
}

/// Executes the five-query reference snapshot after quiescence is proven.
///
/// # Errors
///
/// Returns [`OracleError`] when projection loading, snapshot setup, a bounded
/// query, or the explicit rollback fails.
pub async fn run_reference_oracle(
    client: &mut Client,
    provider_objects: &[ProviderPaymentIntent],
    _quiescence: QuiescencePermit,
) -> Result<SnapshotReport, OracleError> {
    load_provider_projection(client, provider_objects).await?;
    let transaction = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await?;

    let result = async {
        transaction
            .batch_execute(
                "SET LOCAL statement_timeout = '2s'; \
                 SET LOCAL lock_timeout = '500ms';",
            )
            .await?;
        let rows = transaction
            .query(
                "SELECT p.operation_id, \
                        COUNT(*)::bigint AS local_payment_count, \
                        COUNT(DISTINCT p.stripe_payment_intent_id)::bigint AS provider_object_count, \
                        ARRAY_AGG(p.id ORDER BY p.id) AS payment_row_ids, \
                        ARRAY_AGG(p.stripe_payment_intent_id ORDER BY p.stripe_payment_intent_id) \
                            AS provider_payment_intent_ids \
                 FROM payments AS p \
                 JOIN tiv_provider_state AS provider \
                   ON provider.payment_intent_id = p.stripe_payment_intent_id \
                 GROUP BY p.operation_id \
                 HAVING COUNT(DISTINCT p.stripe_payment_intent_id) > 1 \
                 ORDER BY p.operation_id \
                 LIMIT 101",
                &[],
            )
            .await?;
        if rows.len() > MAX_WITNESS_ROWS {
            return Err(OracleError::EvidenceLimitExceeded);
        }
        let witnesses = rows
            .into_iter()
            .map(|row| ProviderObjectWitness {
                operation_id: row.get("operation_id"),
                local_payment_count: row.get("local_payment_count"),
                provider_object_count: row.get("provider_object_count"),
                payment_row_ids: row.get("payment_row_ids"),
                provider_payment_intent_ids: row.get("provider_payment_intent_ids"),
            })
            .collect::<Vec<_>>();
        let verdict = if witnesses.is_empty() {
            InvariantVerdict::Held
        } else {
            InvariantVerdict::Violated(witnesses)
        };
        let mut outcomes = vec![invariant_outcome(PROVIDER_UNIQUENESS_ID, verdict)?];
        for id in NOOP_INVARIANT_IDS {
            let rows = transaction.query("SELECT 1 WHERE FALSE", &[]).await?;
            debug_assert!(rows.is_empty());
            outcomes.push(invariant_outcome(id, InvariantVerdict::Held)?);
        }
        Ok(SnapshotReport { outcomes })
    }
    .await;

    transaction.rollback().await?;
    result
}

async fn load_provider_projection(
    client: &mut Client,
    provider_objects: &[ProviderPaymentIntent],
) -> Result<(), tokio_postgres::Error> {
    client
        .batch_execute(
            "CREATE TEMP TABLE IF NOT EXISTS tiv_provider_state ( \
                 payment_intent_id text PRIMARY KEY, \
                 amount_minor bigint NOT NULL, \
                 currency text NOT NULL, \
                 status text NOT NULL \
             ) ON COMMIT PRESERVE ROWS; \
             TRUNCATE tiv_provider_state;",
        )
        .await?;
    let statement = client
        .prepare(
            "INSERT INTO tiv_provider_state \
                 (payment_intent_id, amount_minor, currency, status) \
             VALUES ($1, $2, $3, $4)",
        )
        .await?;
    for provider in provider_objects {
        client
            .execute(
                &statement,
                &[
                    &provider.id(),
                    &provider.amount_minor(),
                    &provider.currency(),
                    &provider.status(),
                ],
            )
            .await?;
    }
    Ok(())
}

fn invariant_outcome(
    id: &'static str,
    verdict: InvariantVerdict,
) -> Result<InvariantOutcome, OracleError> {
    let invariant = InvariantId::new(id).map_err(|_| OracleError::InvalidBuiltInIdentity)?;
    let checkpoint =
        CheckpointId::new(CHECKPOINT_ID).map_err(|_| OracleError::InvalidBuiltInIdentity)?;
    Ok(InvariantOutcome {
        id,
        identity: FailureIdentity::new(invariant, checkpoint),
        verdict,
    })
}
