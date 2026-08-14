//! Generic bounded invariant execution against one stable `PostgreSQL` snapshot.

use std::{collections::BTreeSet, fs, io::Read, path::PathBuf, time::Duration};

use serde_json::{Map, Value};
use thiserror::Error;
use tiv_core::result::{CheckpointId, FailureIdentity, InvariantId};
use tokio_postgres::{IsolationLevel, Transaction, types::Type};

use crate::config::ResolvedConfig;

use super::oracle::{ProviderPaymentIntent, QuiescencePermit, load_provider_projection};

pub const V1_INVARIANT_IDS: [&str; 5] = [
    "provider-object-unique",
    "webhook-effect-at-most-once",
    "paid-order-amount-conservation",
    "terminal-success-monotonic",
    "balanced-ledger",
];

const CHECKPOINT_ID: &str = "checkout-quiescent";
const MAX_QUERY_BYTES: usize = 64 * 1024;
const MAX_COLUMNS: usize = 32;
const MAX_WITNESS_ROWS: usize = 100;
const MAX_VALUE_BYTES: usize = 16 * 1024;
const MAX_TOTAL_EVIDENCE_BYTES: usize = 256 * 1024;
const MAX_STATEMENT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LOCK_TIMEOUT: Duration = Duration::from_millis(500);
const FORBIDDEN_QUERY_TOKENS: [&str; 28] = [
    "alter",
    "call",
    "copy",
    "create",
    "deallocate",
    "delete",
    "discard",
    "do",
    "drop",
    "execute",
    "grant",
    "insert",
    "into",
    "listen",
    "lock",
    "merge",
    "notify",
    "prepare",
    "refresh",
    "reindex",
    "reset",
    "revoke",
    "set",
    "truncate",
    "unlisten",
    "update",
    "vacuum",
    "analyze",
];
const FORBIDDEN_FUNCTION_TOKENS: [&str; 10] = [
    "lo_export",
    "lo_import",
    "lo_unlink",
    "nextval",
    "pg_advisory_lock",
    "pg_cancel_backend",
    "pg_notify",
    "pg_terminate_backend",
    "set_config",
    "setval",
];

/// One validated repository-owned witness query.
pub struct InvariantQuery {
    id: InvariantId,
    sql: String,
}

impl InvariantQuery {
    /// Validates the identifier, input bound, and initial read-only query shape.
    /// `PostgreSQL` preparation remains the final single-statement and
    /// parameter-free parser.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotContractError`] for invalid IDs, overlarge input, or
    /// anything other than one `SELECT`/`WITH` witness-query shape.
    pub fn new(
        id: impl Into<String>,
        sql: impl Into<String>,
    ) -> Result<Self, SnapshotContractError> {
        let id = InvariantId::new(id).map_err(|_| SnapshotContractError::InvalidInvariantId)?;
        let sql = sql.into();
        if sql.len() > MAX_QUERY_BYTES {
            return Err(SnapshotContractError::QueryTooLarge);
        }
        let normalized =
            normalize_query(&sql).ok_or(SnapshotContractError::UnsupportedQueryShape)?;
        Ok(Self {
            id,
            sql: normalized.to_owned(),
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        self.id.as_str()
    }

    fn sql(&self) -> &str {
        &self.sql
    }
}

/// The fixed five v1 queries in canonical evaluation order.
pub struct InvariantSuite {
    queries: [InvariantQuery; 5],
}

/// The five queries and timeout budgets loaded from one resolved config.
pub struct ConfiguredSnapshot {
    suite: InvariantSuite,
    budgets: SnapshotBudgets,
}

impl ConfiguredSnapshot {
    #[must_use]
    pub const fn suite(&self) -> &InvariantSuite {
        &self.suite
    }

    #[must_use]
    pub const fn budgets(&self) -> SnapshotBudgets {
        self.budgets
    }
}

/// Reads the five canonical query files and timeout budgets from the same
/// typed config accepted by `doctor`.
///
/// # Errors
///
/// Returns [`ConfiguredSnapshotError`] for bounded file I/O, query-contract,
/// or budget failures.
pub fn load_configured_snapshot(
    config: &ResolvedConfig,
) -> Result<ConfiguredSnapshot, ConfiguredSnapshotError> {
    let queries = config
        .invariant_files()
        .iter()
        .map(|file| {
            let sql = read_bounded_query(file.path())?;
            InvariantQuery::new(file.id(), sql).map_err(ConfiguredSnapshotError::Contract)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let suite = InvariantSuite::new(queries)?;
    let budgets = SnapshotBudgets::new(config.statement_timeout(), config.lock_timeout())
        .map_err(|_| ConfiguredSnapshotError::Budget)?;
    Ok(ConfiguredSnapshot { suite, budgets })
}

impl InvariantSuite {
    /// Builds the exact fixed v1 suite.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotContractError`] unless all five supported IDs appear
    /// once and in canonical order.
    pub fn new(queries: Vec<InvariantQuery>) -> Result<Self, SnapshotContractError> {
        let actual = queries.len();
        let queries: [InvariantQuery; 5] = queries
            .try_into()
            .map_err(|_| SnapshotContractError::InvariantCount { actual })?;
        if !queries.iter().map(InvariantQuery::id).eq(V1_INVARIANT_IDS) {
            return Err(SnapshotContractError::UnsupportedInvariantSet);
        }
        Ok(Self { queries })
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.queries.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    #[must_use]
    pub fn ids(&self) -> [&str; 5] {
        std::array::from_fn(|index| self.queries[index].id())
    }
}

/// Runtime query timeouts, capped by the v1 safety contract.
#[derive(Clone, Copy)]
pub struct SnapshotBudgets {
    statement_timeout: Duration,
    lock_timeout: Duration,
}

impl SnapshotBudgets {
    /// Validates non-zero timeouts at or below the v1 maximums.
    ///
    /// # Errors
    ///
    /// Returns [`SnapshotBudgetError`] for zero or expanded budgets.
    pub fn new(
        statement_timeout: Duration,
        lock_timeout: Duration,
    ) -> Result<Self, SnapshotBudgetError> {
        if statement_timeout.is_zero()
            || lock_timeout.is_zero()
            || statement_timeout > MAX_STATEMENT_TIMEOUT
            || lock_timeout > MAX_LOCK_TIMEOUT
        {
            return Err(SnapshotBudgetError);
        }
        Ok(Self {
            statement_timeout,
            lock_timeout,
        })
    }

    #[must_use]
    pub const fn statement_timeout(&self) -> Duration {
        self.statement_timeout
    }

    #[must_use]
    pub const fn lock_timeout(&self) -> Duration {
        self.lock_timeout
    }

    #[cfg(test)]
    const fn v1_maximums() -> Self {
        Self {
            statement_timeout: MAX_STATEMENT_TIMEOUT,
            lock_timeout: MAX_LOCK_TIMEOUT,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotBudgetError;

/// In-memory result for all five queries. It intentionally implements neither
/// `Debug` nor `Serialize`, because repository-authored witnesses may contain
/// sensitive customer-shaped diagnostics.
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
        self.outcomes.iter().find(|outcome| outcome.id() == id)
    }
}

/// One held or violated invariant at the fixed checkout checkpoint.
pub struct InvariantOutcome {
    identity: FailureIdentity,
    verdict: InvariantVerdict,
}

impl InvariantOutcome {
    #[must_use]
    pub fn id(&self) -> &str {
        self.identity.invariant().as_str()
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

pub enum InvariantVerdict {
    Held,
    Violated(Vec<WitnessRow>),
}

/// One bounded diagnostic row. Callers must route it through the artifact
/// redaction layer before serialization.
pub struct WitnessRow {
    columns: Map<String, Value>,
}

impl WitnessRow {
    #[must_use]
    pub const fn columns(&self) -> &Map<String, Value> {
        &self.columns
    }
}

/// Loads the provider projection and evaluates exactly five witness queries in
/// one read-only, repeatable-read transaction.
///
/// # Errors
///
/// Returns [`SnapshotError`] for provider projection, transaction, query
/// contract, type, evidence-bound, or rollback failures.
pub async fn run_snapshot(
    client: &mut tokio_postgres::Client,
    provider_objects: &[ProviderPaymentIntent],
    suite: &InvariantSuite,
    budgets: SnapshotBudgets,
    _quiescence: QuiescencePermit,
) -> Result<SnapshotReport, SnapshotError> {
    load_provider_projection(client, provider_objects)
        .await
        .map_err(SnapshotError::ProviderProjection)?;
    let transaction = client
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .map_err(SnapshotError::BeginSnapshot)?;

    let result = run_queries(&transaction, suite, budgets).await;
    transaction
        .rollback()
        .await
        .map_err(SnapshotError::Rollback)?;
    result
}

async fn run_queries(
    transaction: &Transaction<'_>,
    suite: &InvariantSuite,
    budgets: SnapshotBudgets,
) -> Result<SnapshotReport, SnapshotError> {
    set_timeouts(transaction, budgets).await?;
    let checkpoint =
        CheckpointId::new(CHECKPOINT_ID).map_err(|_| SnapshotError::InvalidBuiltInIdentity)?;
    let mut outcomes = Vec::with_capacity(suite.len());
    let mut total_evidence_bytes = 0_usize;
    for query in &suite.queries {
        let witnesses = execute_invariant(transaction, query, &mut total_evidence_bytes).await?;
        let verdict = if witnesses.is_empty() {
            InvariantVerdict::Held
        } else {
            InvariantVerdict::Violated(witnesses)
        };
        outcomes.push(InvariantOutcome {
            identity: FailureIdentity::new(query.id.clone(), checkpoint.clone()),
            verdict,
        });
    }
    Ok(SnapshotReport { outcomes })
}

async fn set_timeouts(
    transaction: &Transaction<'_>,
    budgets: SnapshotBudgets,
) -> Result<(), SnapshotError> {
    let statement_timeout = duration_setting(budgets.statement_timeout)?;
    let lock_timeout = duration_setting(budgets.lock_timeout)?;
    transaction
        .query_one(
            "SELECT set_config('statement_timeout', $1, true), \
                    set_config('lock_timeout', $2, true)",
            &[&statement_timeout, &lock_timeout],
        )
        .await
        .map_err(SnapshotError::ConfigureSnapshot)?;
    Ok(())
}

async fn execute_invariant(
    transaction: &Transaction<'_>,
    query: &InvariantQuery,
    total_evidence_bytes: &mut usize,
) -> Result<Vec<WitnessRow>, SnapshotError> {
    let statement = transaction.prepare(query.sql()).await.map_err(|source| {
        SnapshotError::InvariantDatabase {
            invariant_id: query.id().to_owned(),
            source,
        }
    })?;
    if !statement.params().is_empty() {
        return Err(SnapshotError::ParametersForbidden(query.id().to_owned()));
    }
    validate_columns(query.id(), statement.columns())?;

    let wrapper = format!(
        "SELECT row_to_json(tiv_witness)::text \
         FROM ({}) AS tiv_witness \
         LIMIT {}",
        query.sql(),
        MAX_WITNESS_ROWS + 1
    );
    let rows = transaction.query(&wrapper, &[]).await.map_err(|source| {
        SnapshotError::InvariantDatabase {
            invariant_id: query.id().to_owned(),
            source,
        }
    })?;
    if rows.len() > MAX_WITNESS_ROWS {
        return Err(SnapshotError::WitnessRowLimit(query.id().to_owned()));
    }

    rows.into_iter()
        .map(|row| {
            let encoded: String = row.get(0);
            *total_evidence_bytes = total_evidence_bytes
                .checked_add(encoded.len())
                .ok_or(SnapshotError::TotalEvidenceLimit)?;
            if *total_evidence_bytes > MAX_TOTAL_EVIDENCE_BYTES {
                return Err(SnapshotError::TotalEvidenceLimit);
            }
            let value: Value = serde_json::from_str(&encoded)
                .map_err(|_| SnapshotError::InvalidWitness(query.id().to_owned()))?;
            let Value::Object(columns) = value else {
                return Err(SnapshotError::InvalidWitness(query.id().to_owned()));
            };
            for value in columns.values() {
                let bytes = serde_json::to_vec(value)
                    .map_err(|_| SnapshotError::InvalidWitness(query.id().to_owned()))?;
                if bytes.len() > MAX_VALUE_BYTES {
                    return Err(SnapshotError::WitnessValueLimit(query.id().to_owned()));
                }
            }
            Ok(WitnessRow { columns })
        })
        .collect()
}

fn validate_columns(
    invariant_id: &str,
    columns: &[tokio_postgres::Column],
) -> Result<(), SnapshotError> {
    if columns.is_empty() || columns.len() > MAX_COLUMNS {
        return Err(SnapshotError::UnsupportedColumns(invariant_id.to_owned()));
    }
    let mut names = BTreeSet::new();
    for column in columns {
        if column.name().is_empty()
            || !names.insert(column.name())
            || !supported_type(column.type_())
        {
            return Err(SnapshotError::UnsupportedColumns(invariant_id.to_owned()));
        }
    }
    Ok(())
}

fn supported_type(found: &Type) -> bool {
    [
        Type::BOOL,
        Type::BOOL_ARRAY,
        Type::INT2,
        Type::INT2_ARRAY,
        Type::INT4,
        Type::INT4_ARRAY,
        Type::INT8,
        Type::INT8_ARRAY,
        Type::NUMERIC,
        Type::NUMERIC_ARRAY,
        Type::TEXT,
        Type::TEXT_ARRAY,
        Type::VARCHAR,
        Type::VARCHAR_ARRAY,
        Type::BPCHAR,
        Type::BPCHAR_ARRAY,
        Type::UUID,
        Type::UUID_ARRAY,
        Type::DATE,
        Type::DATE_ARRAY,
        Type::TIME,
        Type::TIME_ARRAY,
        Type::TIMETZ,
        Type::TIMETZ_ARRAY,
        Type::TIMESTAMP,
        Type::TIMESTAMP_ARRAY,
        Type::TIMESTAMPTZ,
        Type::TIMESTAMPTZ_ARRAY,
    ]
    .contains(found)
}

fn normalize_query(sql: &str) -> Option<&str> {
    if sql.contains('\0') {
        return None;
    }
    let sql = sql.trim();
    let sql = sql.strip_suffix(';').unwrap_or(sql).trim_end();
    let tokens = query_tokens(sql)?;
    if !matches!(tokens.first().map(String::as_str), Some("select" | "with"))
        || tokens.iter().any(|token| {
            FORBIDDEN_QUERY_TOKENS.contains(&token.as_str())
                || FORBIDDEN_FUNCTION_TOKENS.contains(&token.as_str())
                || token.starts_with("pg_advisory_")
                || token.starts_with("pg_try_advisory_")
        })
    {
        return None;
    }
    Some(sql)
}

fn query_tokens(sql: &str) -> Option<Vec<String>> {
    let bytes = sql.as_bytes();
    let mut index = 0;
    let mut tokens = Vec::new();
    while index < bytes.len() {
        match bytes[index] {
            byte if byte.is_ascii_whitespace() => index += 1,
            b'-' if bytes.get(index + 1) == Some(&b'-') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index + 2)?;
            }
            b'\'' => index = skip_quoted(bytes, index + 1, b'\'')?,
            b'"' => index = skip_quoted(bytes, index + 1, b'"')?,
            b'$' => {
                if let Some(after_literal) = skip_dollar_quoted(bytes, index) {
                    index = after_literal;
                } else {
                    index += 1;
                }
            }
            b';' => return None,
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = index;
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$'))
                {
                    index += 1;
                }
                tokens.push(sql[start..index].to_ascii_lowercase());
            }
            _ => index += 1,
        }
    }
    (!tokens.is_empty()).then_some(tokens)
}

fn skip_quoted(bytes: &[u8], mut index: usize, quote: u8) -> Option<usize> {
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            return None;
        }
        if bytes[index] == quote {
            if bytes.get(index + 1) == Some(&quote) {
                index += 2;
            } else {
                return Some(index + 1);
            }
        } else {
            index += 1;
        }
    }
    None
}

fn skip_block_comment(bytes: &[u8], mut index: usize) -> Option<usize> {
    let mut depth = 1_usize;
    while index < bytes.len() {
        if bytes.get(index..index + 2) == Some(b"/*") {
            depth += 1;
            index += 2;
        } else if bytes.get(index..index + 2) == Some(b"*/") {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return Some(index);
            }
        } else {
            index += 1;
        }
    }
    None
}

fn skip_dollar_quoted(bytes: &[u8], start: usize) -> Option<usize> {
    let mut delimiter_end = start + 1;
    while delimiter_end < bytes.len()
        && (bytes[delimiter_end].is_ascii_alphanumeric() || bytes[delimiter_end] == b'_')
    {
        delimiter_end += 1;
    }
    if bytes.get(delimiter_end) != Some(&b'$') {
        return None;
    }
    let delimiter = &bytes[start..=delimiter_end];
    let body_start = delimiter_end + 1;
    bytes[body_start..]
        .windows(delimiter.len())
        .position(|window| window == delimiter)
        .map(|offset| body_start + offset + delimiter.len())
}

fn duration_setting(duration: Duration) -> Result<String, SnapshotError> {
    u64::try_from(duration.as_millis())
        .map(|millis| format!("{millis}ms"))
        .map_err(|_| SnapshotError::InvalidBudget)
}

fn read_bounded_query(path: &std::path::Path) -> Result<String, ConfiguredSnapshotError> {
    let file = fs::File::open(path).map_err(|source| ConfiguredSnapshotError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut sql = String::new();
    file.take((MAX_QUERY_BYTES + 1) as u64)
        .read_to_string(&mut sql)
        .map_err(|source| ConfiguredSnapshotError::Read {
            path: path.to_owned(),
            source,
        })?;
    if sql.len() > MAX_QUERY_BYTES {
        return Err(ConfiguredSnapshotError::Contract(
            SnapshotContractError::QueryTooLarge,
        ));
    }
    Ok(sql)
}

#[derive(Debug, Error)]
pub enum ConfiguredSnapshotError {
    #[error("could not read invariant query {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("configured invariant query is invalid: {0}")]
    Contract(#[from] SnapshotContractError),
    #[error("configured snapshot budgets are invalid")]
    Budget,
}

#[derive(Debug, Error)]
pub enum SnapshotContractError {
    #[error("invariant ID is invalid")]
    InvalidInvariantId,
    #[error("invariant query exceeds the 64 KiB input limit")]
    QueryTooLarge,
    #[error("invariant query must begin with SELECT or WITH")]
    UnsupportedQueryShape,
    #[error("exactly five invariant queries are required, found {actual}")]
    InvariantCount { actual: usize },
    #[error("invariant queries must use the fixed five v1 IDs in canonical order")]
    UnsupportedInvariantSet,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("could not load the temporary provider projection")]
    ProviderProjection(#[source] tokio_postgres::Error),
    #[error("could not begin the read-only repeatable-read snapshot")]
    BeginSnapshot(#[source] tokio_postgres::Error),
    #[error("could not configure snapshot query timeouts")]
    ConfigureSnapshot(#[source] tokio_postgres::Error),
    #[error("invariant {invariant_id} failed inside PostgreSQL")]
    InvariantDatabase {
        invariant_id: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("invariant {0} may not contain query parameters")]
    ParametersForbidden(String),
    #[error("invariant {0} has duplicate, absent, excessive, or unsupported diagnostic columns")]
    UnsupportedColumns(String),
    #[error("invariant {0} exceeded the 100-row witness limit")]
    WitnessRowLimit(String),
    #[error("invariant {0} exceeded the 16 KiB per-value witness limit")]
    WitnessValueLimit(String),
    #[error("all invariant witnesses exceeded the 256 KiB evidence limit")]
    TotalEvidenceLimit,
    #[error("invariant {0} returned an invalid JSON witness")]
    InvalidWitness(String),
    #[error("snapshot budget could not be represented")]
    InvalidBudget,
    #[error("the fixed snapshot identity is invalid")]
    InvalidBuiltInIdentity,
    #[error("could not roll back the invariant snapshot")]
    Rollback(#[source] tokio_postgres::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn five_noop_invariants_hold_in_one_real_read_only_snapshot() {
        let (mut client, connection) = connect_test_postgres().await;
        let suite = no_op_suite();

        let report = run_snapshot(
            &mut client,
            &[],
            &suite,
            SnapshotBudgets::v1_maximums(),
            QuiescencePermit::after_synthetic_driver_stopped(),
        )
        .await;
        let report = match report {
            Ok(report) => report,
            Err(error) => panic!("the no-op snapshot failed: {error}"),
        };
        assert_eq!(report.outcomes().len(), 5);
        assert!(
            report
                .outcomes()
                .iter()
                .all(|outcome| matches!(outcome.verdict(), InvariantVerdict::Held))
        );

        drop(client);
        connection
            .await
            .expect("the PostgreSQL connection task exits")
            .expect("the PostgreSQL connection closes cleanly");
    }

    #[tokio::test]
    #[ignore = "requires the isolated tiv-truth-spike-postgres Compose project"]
    async fn runtime_rejects_parameters_types_and_evidence_expansion_then_recovers() {
        let (mut client, connection) = connect_test_postgres().await;
        let cases = [
            ("SELECT $1::bigint AS parameterized", ErrorKind::Parameters),
            (
                "SELECT 1.5::double precision AS floating_diagnostic",
                ErrorKind::Columns,
            ),
            (
                "SELECT value FROM generate_series(1, 101) AS value",
                ErrorKind::Rows,
            ),
            (
                "SELECT repeat('x', 16385) AS oversized_value",
                ErrorKind::Value,
            ),
            (
                "SELECT missing FROM relation_that_does_not_exist",
                ErrorKind::Database,
            ),
            (
                "SELECT 1::bigint AS delayed FROM pg_sleep(3)",
                ErrorKind::Database,
            ),
        ];
        for (sql, expected) in cases {
            let error = run_failure_case(&mut client, first_query_suite(sql)).await;
            assert!(expected.matches(&error), "unexpected error: {error}");
        }

        let total_suite = InvariantSuite::new(
            V1_INVARIANT_IDS
                .into_iter()
                .map(|id| {
                    InvariantQuery::new(
                        id,
                        "SELECT repeat('x', 3000) AS diagnostic \
                         FROM generate_series(1, 20)",
                    )
                    .expect("the bounded total-evidence query is valid")
                })
                .collect(),
        )
        .expect("the fixed suite is valid");
        let error = run_failure_case(&mut client, total_suite).await;
        assert!(matches!(error, SnapshotError::TotalEvidenceLimit));

        let recovery = run_snapshot(
            &mut client,
            &[],
            &no_op_suite(),
            SnapshotBudgets::v1_maximums(),
            QuiescencePermit::after_synthetic_driver_stopped(),
        )
        .await;
        assert!(recovery.is_ok(), "rollback did not recover the session");

        drop(client);
        connection
            .await
            .expect("the PostgreSQL connection task exits")
            .expect("the PostgreSQL connection closes cleanly");
    }

    async fn connect_test_postgres() -> (
        tokio_postgres::Client,
        tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
    ) {
        let port = std::env::var("TIV_POSTGRES_TEST_PORT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(15_432);
        let mut config = tokio_postgres::Config::new();
        config
            .host("127.0.0.1")
            .port(port)
            .user("tiv_admin")
            .password("tiv-local-only-password")
            .dbname("postgres");
        let (client, connection) = config
            .connect(tokio_postgres::NoTls)
            .await
            .expect("the isolated truth-spike PostgreSQL accepts the admin test role");
        (client, tokio::spawn(connection))
    }

    fn no_op_suite() -> InvariantSuite {
        InvariantSuite::new(
            V1_INVARIANT_IDS
                .into_iter()
                .enumerate()
                .map(|(index, id)| {
                    InvariantQuery::new(
                        id,
                        format!("SELECT {}::bigint AS unreachable WHERE FALSE", index + 1),
                    )
                    .expect("the no-op invariant is valid")
                })
                .collect(),
        )
        .expect("the fixed suite is valid")
    }

    fn first_query_suite(first: &str) -> InvariantSuite {
        InvariantSuite::new(
            V1_INVARIANT_IDS
                .into_iter()
                .enumerate()
                .map(|(index, id)| {
                    let sql = if index == 0 {
                        first.to_owned()
                    } else {
                        "SELECT 1 AS unreachable WHERE FALSE".to_owned()
                    };
                    InvariantQuery::new(id, sql).expect("the runtime test query shape is valid")
                })
                .collect(),
        )
        .expect("the fixed suite is valid")
    }

    async fn run_failure_case(
        client: &mut tokio_postgres::Client,
        suite: InvariantSuite,
    ) -> SnapshotError {
        match run_snapshot(
            client,
            &[],
            &suite,
            SnapshotBudgets::v1_maximums(),
            QuiescencePermit::after_synthetic_driver_stopped(),
        )
        .await
        {
            Ok(_) => panic!("the unsafe runtime case unexpectedly succeeded"),
            Err(error) => error,
        }
    }

    enum ErrorKind {
        Parameters,
        Columns,
        Rows,
        Value,
        Database,
    }

    impl ErrorKind {
        fn matches(&self, error: &SnapshotError) -> bool {
            matches!(
                (self, error),
                (Self::Parameters, SnapshotError::ParametersForbidden(_))
                    | (Self::Columns, SnapshotError::UnsupportedColumns(_))
                    | (Self::Rows, SnapshotError::WitnessRowLimit(_))
                    | (Self::Value, SnapshotError::WitnessValueLimit(_))
                    | (Self::Database, SnapshotError::InvariantDatabase { .. })
            )
        }
    }
}
