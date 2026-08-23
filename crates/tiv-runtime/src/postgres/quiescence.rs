//! Repository-owned stable quiescence predicates for configured case checkpoints.

use std::{fs, io::Read, path::PathBuf, time::Duration};

use thiserror::Error;
use tokio::time::{Instant, sleep, timeout};
use tokio_postgres::{Transaction, types::Type};

use crate::config::ResolvedConfig;

use super::snapshot::{
    InvariantRoleError, InvariantRoleName, SnapshotBudgets, attest_invariant_role,
};

const MAX_QUERY_BYTES: usize = 64 * 1024;

struct StableWindow {
    stable_for: Duration,
    true_since: Option<Duration>,
    last_observed: Option<Duration>,
}

impl StableWindow {
    const fn new(stable_for: Duration) -> Self {
        Self {
            stable_for,
            true_since: None,
            last_observed: None,
        }
    }

    fn observe(
        &mut self,
        elapsed: Duration,
        is_quiescent: bool,
    ) -> Result<bool, QuiescenceWindowError> {
        if self
            .last_observed
            .is_some_and(|last_observed| elapsed < last_observed)
        {
            return Err(QuiescenceWindowError);
        }
        self.last_observed = Some(elapsed);
        if !is_quiescent {
            self.true_since = None;
            return Ok(false);
        }
        let true_since = *self.true_since.get_or_insert(elapsed);
        Ok(elapsed.saturating_sub(true_since) >= self.stable_for)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("quiescence observations were not monotonic")]
struct QuiescenceWindowError;

/// One validated parameter-free, read-only quiescence predicate.
#[derive(Clone)]
pub struct QuiescenceQuery {
    sql: String,
}

impl QuiescenceQuery {
    /// Validates the input bound and initial read-only query shape.
    /// `PostgreSQL` preparation remains the final single-statement/type check.
    ///
    /// # Errors
    ///
    /// Returns [`QuiescenceContractError`] for overlarge or unsafe SQL.
    pub fn new(sql: impl Into<String>) -> Result<Self, QuiescenceContractError> {
        let sql = sql.into();
        if sql.len() > MAX_QUERY_BYTES {
            return Err(QuiescenceContractError::QueryTooLarge);
        }
        let normalized = super::snapshot::normalize_query(&sql)
            .ok_or(QuiescenceContractError::UnsupportedQueryShape)?;
        Ok(Self {
            sql: normalized.to_owned(),
        })
    }

    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

/// One query plus the stable-window bounds approved by typed configuration.
#[derive(Clone)]
pub struct ConfiguredQuiescence {
    query: QuiescenceQuery,
    budgets: SnapshotBudgets,
    role: InvariantRoleName,
    stable_for: Duration,
    timeout: Duration,
}

impl ConfiguredQuiescence {
    #[must_use]
    pub const fn query(&self) -> &QuiescenceQuery {
        &self.query
    }

    #[must_use]
    pub const fn budgets(&self) -> SnapshotBudgets {
        self.budgets
    }

    #[must_use]
    pub const fn role(&self) -> &InvariantRoleName {
        &self.role
    }

    #[must_use]
    pub const fn stable_for(&self) -> Duration {
        self.stable_for
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Polls fresh read-only snapshots until the configured predicate remains
    /// true for one uninterrupted stable window.
    ///
    /// # Errors
    ///
    /// Returns [`QuiescenceError`] for invalid bounds, an unsafe role, an
    /// invalid predicate result, a database failure, or timeout.
    pub async fn await_stable(
        &self,
        client: &mut tokio_postgres::Client,
        poll_interval: Duration,
    ) -> Result<DatabaseQuiescenceCompletion, QuiescenceError> {
        if poll_interval.is_zero()
            || poll_interval >= self.timeout
            || self.stable_for.is_zero()
            || self.stable_for >= self.timeout
        {
            return Err(QuiescenceError::InvalidBudget);
        }
        let started = Instant::now();
        let mut window = StableWindow::new(self.stable_for);
        timeout(self.timeout, async {
            loop {
                let observed = self.evaluate(client).await?;
                if window
                    .observe(started.elapsed(), observed)
                    .map_err(|_| QuiescenceError::NonMonotonicTime)?
                {
                    return Ok(DatabaseQuiescenceCompletion { _private: () });
                }
                sleep(poll_interval).await;
            }
        })
        .await
        .map_err(|_| QuiescenceError::Timeout)?
    }

    async fn evaluate(&self, client: &mut tokio_postgres::Client) -> Result<bool, QuiescenceError> {
        attest_invariant_role(client, self.role.clone())
            .await
            .map_err(QuiescenceError::InvariantRoleAttestation)?;
        let transaction = client
            .build_transaction()
            .read_only(true)
            .start()
            .await
            .map_err(QuiescenceError::BeginTransaction)?;
        let result = evaluate_transaction(&transaction, self).await;
        transaction
            .rollback()
            .await
            .map_err(QuiescenceError::Rollback)?;
        result
    }
}

/// Opaque proof that the configured database predicate held continuously for
/// its complete stable window.
pub struct DatabaseQuiescenceCompletion {
    _private: (),
}

async fn evaluate_transaction(
    transaction: &Transaction<'_>,
    configured: &ConfiguredQuiescence,
) -> Result<bool, QuiescenceError> {
    let statement_timeout = duration_setting(configured.budgets.statement_timeout())?;
    let lock_timeout = duration_setting(configured.budgets.lock_timeout())?;
    transaction
        .query_one(
            "SELECT set_config('statement_timeout', $1, true), \
                    set_config('lock_timeout', $2, true)",
            &[&statement_timeout, &lock_timeout],
        )
        .await
        .map_err(QuiescenceError::ConfigureTransaction)?;
    transaction
        .batch_execute(&format!("SET LOCAL ROLE {}", configured.role.as_str()))
        .await
        .map_err(QuiescenceError::ActivateInvariantRole)?;
    let boundary = transaction
        .query_one(
            "SELECT current_user::text, current_setting('transaction_read_only')",
            &[],
        )
        .await
        .map_err(QuiescenceError::ActivateInvariantRole)?;
    if boundary.get::<_, &str>(0) != configured.role.as_str() || boundary.get::<_, &str>(1) != "on"
    {
        return Err(QuiescenceError::InvariantRoleBoundary);
    }
    let statement = transaction
        .prepare(configured.query.sql())
        .await
        .map_err(QuiescenceError::Prepare)?;
    if !statement.params().is_empty() {
        return Err(QuiescenceError::ParametersForbidden);
    }
    if statement.columns().len() != 1 || statement.columns()[0].type_() != &Type::BOOL {
        return Err(QuiescenceError::InvalidResultShape);
    }
    let rows = transaction
        .query(&statement, &[])
        .await
        .map_err(QuiescenceError::Execute)?;
    if rows.len() != 1 {
        return Err(QuiescenceError::InvalidResultShape);
    }
    rows[0]
        .try_get::<_, bool>(0)
        .map_err(|_| QuiescenceError::InvalidResultShape)
}

fn duration_setting(duration: Duration) -> Result<String, QuiescenceError> {
    u64::try_from(duration.as_millis())
        .map(|millis| format!("{millis}ms"))
        .map_err(|_| QuiescenceError::InvalidBudget)
}

/// Loads the configured repository query before any case mutation.
///
/// # Errors
///
/// Returns [`ConfiguredQuiescenceError`] for bounded I/O or query-contract
/// failures.
pub fn load_configured_quiescence(
    config: &ResolvedConfig,
) -> Result<ConfiguredQuiescence, ConfiguredQuiescenceError> {
    let query = QuiescenceQuery::new(read_bounded_query(config.quiescence_sql_file())?)?;
    let budgets = SnapshotBudgets::new(config.statement_timeout(), config.lock_timeout())
        .map_err(|_| ConfiguredQuiescenceError::Budget)?;
    let role = InvariantRoleName::new(config.invariant_role())
        .map_err(|_| ConfiguredQuiescenceError::Role)?;
    Ok(ConfiguredQuiescence {
        query,
        budgets,
        role,
        stable_for: config.quiescence_stable_for(),
        timeout: config.quiescence_timeout(),
    })
}

fn read_bounded_query(path: &std::path::Path) -> Result<String, ConfiguredQuiescenceError> {
    let file = fs::File::open(path).map_err(|source| ConfiguredQuiescenceError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut sql = String::new();
    file.take((MAX_QUERY_BYTES + 1) as u64)
        .read_to_string(&mut sql)
        .map_err(|source| ConfiguredQuiescenceError::Read {
            path: path.to_owned(),
            source,
        })?;
    if sql.len() > MAX_QUERY_BYTES {
        return Err(ConfiguredQuiescenceError::Contract(
            QuiescenceContractError::QueryTooLarge,
        ));
    }
    Ok(sql)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum QuiescenceContractError {
    #[error("quiescence query exceeds the 64 KiB input limit")]
    QueryTooLarge,
    #[error("quiescence query must be one read-only SELECT or WITH statement")]
    UnsupportedQueryShape,
}

#[derive(Debug, Error)]
pub enum ConfiguredQuiescenceError {
    #[error("could not read configured quiescence query {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("configured quiescence query is invalid: {0}")]
    Contract(#[from] QuiescenceContractError),
    #[error("configured quiescence execution budgets are invalid")]
    Budget,
    #[error("configured quiescence invariant role is invalid")]
    Role,
}

#[derive(Debug, Error)]
pub enum QuiescenceError {
    #[error("configured quiescence polling bounds are invalid")]
    InvalidBudget,
    #[error("configured quiescence observation time was not monotonic")]
    NonMonotonicTime,
    #[error("configured quiescence predicate did not remain true before timeout")]
    Timeout,
    #[error("could not attest the configured invariant role: {0}")]
    InvariantRoleAttestation(#[source] InvariantRoleError),
    #[error("could not begin the quiescence read-only transaction: {0}")]
    BeginTransaction(#[source] tokio_postgres::Error),
    #[error("could not configure quiescence transaction bounds: {0}")]
    ConfigureTransaction(#[source] tokio_postgres::Error),
    #[error("could not activate the configured invariant role: {0}")]
    ActivateInvariantRole(#[source] tokio_postgres::Error),
    #[error("the configured invariant role boundary was not effective")]
    InvariantRoleBoundary,
    #[error("could not prepare the configured quiescence predicate: {0}")]
    Prepare(#[source] tokio_postgres::Error),
    #[error("configured quiescence predicates cannot take parameters")]
    ParametersForbidden,
    #[error("configured quiescence predicate must return one non-null boolean row")]
    InvalidResultShape,
    #[error("could not execute the configured quiescence predicate: {0}")]
    Execute(#[source] tokio_postgres::Error),
    #[error("could not roll back the quiescence transaction: {0}")]
    Rollback(#[source] tokio_postgres::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_window_resets_after_a_false_observation() {
        let mut window = StableWindow::new(Duration::from_millis(500));

        assert!(!window.observe(Duration::ZERO, true).unwrap());
        assert!(!window.observe(Duration::from_millis(400), true).unwrap());
        assert!(!window.observe(Duration::from_millis(450), false).unwrap());
        assert!(!window.observe(Duration::from_millis(500), true).unwrap());
        assert!(window.observe(Duration::from_millis(1_000), true).unwrap());
    }

    #[test]
    fn stable_window_rejects_non_monotonic_observation_time() {
        let mut window = StableWindow::new(Duration::from_millis(100));
        window.observe(Duration::from_millis(50), true).unwrap();

        assert!(window.observe(Duration::from_millis(49), true).is_err());
    }
}
