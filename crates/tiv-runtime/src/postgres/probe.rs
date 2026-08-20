//! Bounded repository-owned predicates for observable SQL crash cut points.

use std::{fs, io::Read, path::PathBuf, time::Duration};

use thiserror::Error;
use tokio_postgres::{Transaction, types::Type};

use crate::config::ResolvedConfig;

use super::snapshot::{
    InvariantRoleError, InvariantRoleName, SnapshotBudgets, attest_invariant_role,
};

const MAX_QUERY_BYTES: usize = 64 * 1024;

/// One validated parameter-free predicate query.
pub struct SqlProbeQuery {
    sql: String,
}

/// One configured query plus the effective role and database-side budgets
/// approved by the same typed configuration as `tiv doctor`.
pub struct ConfiguredSqlProbe {
    query: SqlProbeQuery,
    budgets: SnapshotBudgets,
    role: InvariantRoleName,
}

impl ConfiguredSqlProbe {
    #[must_use]
    pub const fn query(&self) -> &SqlProbeQuery {
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

    /// Consumes the resolved boundary and creates its single-use observer.
    #[must_use]
    pub fn into_probe(self) -> SqlProbe {
        SqlProbe::new(self.query, self.budgets, self.role)
    }
}

/// One single-use observer for a false-to-true SQL predicate transition.
pub struct SqlProbe {
    query: SqlProbeQuery,
    budgets: SnapshotBudgets,
    role: InvariantRoleName,
    state: SqlProbeState,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum SqlProbeState {
    Ready,
    Armed,
    Observed,
    Failed,
}

impl SqlProbe {
    #[must_use]
    pub const fn new(
        query: SqlProbeQuery,
        budgets: SnapshotBudgets,
        role: InvariantRoleName,
    ) -> Self {
        Self {
            query,
            budgets,
            role,
            state: SqlProbeState::Ready,
        }
    }

    /// Proves the configured predicate is false immediately before its owning
    /// application action starts. The observer is single-use and becomes
    /// fail-closed after any unsuccessful arm attempt.
    ///
    /// # Errors
    ///
    /// Returns [`SqlProbeError`] for invalid lifecycle state, an expanded role,
    /// an invalid database result, or a predicate that already holds.
    pub async fn require_false(
        &mut self,
        client: &mut tokio_postgres::Client,
    ) -> Result<(), SqlProbeError> {
        if self.state != SqlProbeState::Ready {
            return Err(SqlProbeError::InvalidState);
        }
        self.state = SqlProbeState::Failed;
        if self.evaluate(client).await? {
            return Err(SqlProbeError::DidNotBeginFalse);
        }
        self.state = SqlProbeState::Armed;
        Ok(())
    }

    /// Polls committed snapshots until the armed predicate first becomes true.
    /// Every evaluation freshly attests the invariant role and uses a separate
    /// read-only transaction so later commits remain observable.
    ///
    /// # Errors
    ///
    /// Returns [`SqlProbeError`] for invalid lifecycle/budget state, database
    /// boundary failures, or expiry before the first true value.
    pub async fn observe_true(
        &mut self,
        client: &mut tokio_postgres::Client,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<(), SqlProbeError> {
        if self.state != SqlProbeState::Armed {
            return Err(SqlProbeError::InvalidState);
        }
        self.state = SqlProbeState::Failed;
        if timeout.is_zero() || poll_interval.is_zero() || poll_interval >= timeout {
            return Err(SqlProbeError::InvalidObservationBudget);
        }
        let observed = tokio::time::timeout(timeout, async {
            loop {
                if self.evaluate(client).await? {
                    return Ok(());
                }
                tokio::time::sleep(poll_interval).await;
            }
        })
        .await
        .map_err(|_| SqlProbeError::Timeout)?;
        observed?;
        self.state = SqlProbeState::Observed;
        Ok(())
    }

    #[must_use]
    pub const fn observed(&self) -> bool {
        matches!(self.state, SqlProbeState::Observed)
    }

    async fn evaluate(&self, client: &mut tokio_postgres::Client) -> Result<bool, SqlProbeError> {
        attest_invariant_role(client, self.role.clone())
            .await
            .map_err(SqlProbeError::InvariantRoleAttestation)?;
        let transaction = client
            .build_transaction()
            .read_only(true)
            .start()
            .await
            .map_err(SqlProbeError::BeginTransaction)?;
        let result =
            evaluate_transaction(&transaction, &self.query, self.budgets, &self.role).await;
        transaction
            .rollback()
            .await
            .map_err(SqlProbeError::Rollback)?;
        result
    }
}

async fn evaluate_transaction(
    transaction: &Transaction<'_>,
    query: &SqlProbeQuery,
    budgets: SnapshotBudgets,
    role: &InvariantRoleName,
) -> Result<bool, SqlProbeError> {
    let statement_timeout = duration_setting(budgets.statement_timeout())?;
    let lock_timeout = duration_setting(budgets.lock_timeout())?;
    transaction
        .query_one(
            "SELECT set_config('statement_timeout', $1, true), \
                    set_config('lock_timeout', $2, true)",
            &[&statement_timeout, &lock_timeout],
        )
        .await
        .map_err(SqlProbeError::ConfigureTransaction)?;
    transaction
        .batch_execute(&format!("SET LOCAL ROLE {}", role.as_str()))
        .await
        .map_err(SqlProbeError::ActivateInvariantRole)?;
    let boundary = transaction
        .query_one(
            "SELECT current_user::text, current_setting('transaction_read_only')",
            &[],
        )
        .await
        .map_err(SqlProbeError::ActivateInvariantRole)?;
    if boundary.get::<_, &str>(0) != role.as_str() || boundary.get::<_, &str>(1) != "on" {
        return Err(SqlProbeError::InvariantRoleBoundary);
    }
    let statement = transaction
        .prepare(query.sql())
        .await
        .map_err(SqlProbeError::Prepare)?;
    if !statement.params().is_empty() {
        return Err(SqlProbeError::ParametersForbidden);
    }
    if statement.columns().len() != 1 || statement.columns()[0].type_() != &Type::BOOL {
        return Err(SqlProbeError::InvalidResultShape);
    }
    let rows = transaction
        .query(&statement, &[])
        .await
        .map_err(SqlProbeError::Execute)?;
    if rows.len() != 1 {
        return Err(SqlProbeError::InvalidResultShape);
    }
    rows[0]
        .try_get::<_, bool>(0)
        .map_err(|_| SqlProbeError::InvalidResultShape)
}

fn duration_setting(duration: Duration) -> Result<String, SqlProbeError> {
    u64::try_from(duration.as_millis())
        .map(|millis| format!("{millis}ms"))
        .map_err(|_| SqlProbeError::InvalidExecutionBudget)
}

/// Loads the repository-owned probe and its execution boundary from one
/// already-resolved configuration.
///
/// # Errors
///
/// Returns [`ConfiguredSqlProbeError`] when the bounded file cannot be read or
/// does not satisfy the SQL probe contract.
pub fn load_configured_sql_probe(
    config: &ResolvedConfig,
) -> Result<ConfiguredSqlProbe, ConfiguredSqlProbeError> {
    let query = SqlProbeQuery::new(read_bounded_query(config.sql_probe_file())?)?;
    let budgets = SnapshotBudgets::new(config.statement_timeout(), config.lock_timeout())
        .map_err(|_| ConfiguredSqlProbeError::Budget)?;
    let role = InvariantRoleName::new(config.invariant_role())
        .map_err(|_| ConfiguredSqlProbeError::Role)?;
    Ok(ConfiguredSqlProbe {
        query,
        budgets,
        role,
    })
}

fn read_bounded_query(path: &std::path::Path) -> Result<String, ConfiguredSqlProbeError> {
    let file = fs::File::open(path).map_err(|source| ConfiguredSqlProbeError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut sql = String::new();
    file.take((MAX_QUERY_BYTES + 1) as u64)
        .read_to_string(&mut sql)
        .map_err(|source| ConfiguredSqlProbeError::Read {
            path: path.to_owned(),
            source,
        })?;
    if sql.len() > MAX_QUERY_BYTES {
        return Err(ConfiguredSqlProbeError::Contract(
            SqlProbeContractError::QueryTooLarge,
        ));
    }
    Ok(sql)
}

impl SqlProbeQuery {
    /// Validates the input bound and initial read-only query shape.
    /// `PostgreSQL` preparation remains the final single-statement and
    /// parameter-free parser.
    ///
    /// # Errors
    ///
    /// Returns [`SqlProbeContractError`] for overlarge input or anything other
    /// than one `SELECT`/`WITH` query shape.
    pub fn new(sql: impl Into<String>) -> Result<Self, SqlProbeContractError> {
        let sql = sql.into();
        if sql.len() > MAX_QUERY_BYTES {
            return Err(SqlProbeContractError::QueryTooLarge);
        }
        let normalized = super::snapshot::normalize_query(&sql)
            .ok_or(SqlProbeContractError::UnsupportedQueryShape)?;
        Ok(Self {
            sql: normalized.to_owned(),
        })
    }

    #[must_use]
    pub fn sql(&self) -> &str {
        &self.sql
    }
}

#[derive(Debug, Error)]
pub enum SqlProbeContractError {
    #[error("SQL probe query exceeds the 64 KiB input limit")]
    QueryTooLarge,
    #[error("SQL probe query must be one SELECT or WITH statement")]
    UnsupportedQueryShape,
}

#[derive(Debug, Error)]
pub enum ConfiguredSqlProbeError {
    #[error("could not read SQL probe query {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("configured SQL probe query is invalid: {0}")]
    Contract(#[from] SqlProbeContractError),
    #[error("configured SQL probe budgets are invalid")]
    Budget,
    #[error("configured SQL probe role is invalid")]
    Role,
}

#[derive(Debug, Error)]
pub enum SqlProbeError {
    #[error("the SQL probe lifecycle is invalid")]
    InvalidState,
    #[error("the SQL probe observation budget is invalid")]
    InvalidObservationBudget,
    #[error("fresh invariant-role attestation failed")]
    InvariantRoleAttestation(#[source] InvariantRoleError),
    #[error("could not begin the read-only SQL probe transaction")]
    BeginTransaction(#[source] tokio_postgres::Error),
    #[error("could not configure SQL probe transaction budgets")]
    ConfigureTransaction(#[source] tokio_postgres::Error),
    #[error("could not activate the SQL probe invariant role")]
    ActivateInvariantRole(#[source] tokio_postgres::Error),
    #[error("the SQL probe did not retain its role and read-only boundary")]
    InvariantRoleBoundary,
    #[error("could not prepare the SQL probe query")]
    Prepare(#[source] tokio_postgres::Error),
    #[error("the SQL probe query may not contain parameters")]
    ParametersForbidden,
    #[error("the SQL probe query must return exactly one non-null boolean row")]
    InvalidResultShape,
    #[error("could not execute the SQL probe query")]
    Execute(#[source] tokio_postgres::Error),
    #[error("the SQL probe execution budget could not be represented")]
    InvalidExecutionBudget,
    #[error("the SQL probe transaction could not be rolled back")]
    Rollback(#[source] tokio_postgres::Error),
    #[error("the SQL probe predicate did not begin false")]
    DidNotBeginFalse,
    #[error("the SQL probe did not become true before its deadline")]
    Timeout,
}
