//! Versioned configuration loading, semantic safety validation, and redaction.

use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    net::IpAddr,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use reqwest::header::HeaderValue;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CampaignSpec, CaseCount, ProcessCutPoint as CoreProcessCutPoint,
        ProcessFaultSpec, ProviderOutcome, WebhookFaultSpec,
    },
    result::InvariantId,
};
use url::Url;

const CONFIG_SCHEMA_VERSION: u16 = 1;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CASES: u32 = 500;
const MAX_ACTIONS_PER_CASE: u32 = 40;
const MAX_CASE_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DRIVER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_QUIESCENCE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_STATEMENT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_LOCK_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_DATABASE_BYTES: u64 = 2_147_483_648;
const MAX_WEBHOOK_DUPLICATES: u32 = 3;
const MAX_DELAY_MILLIS: u64 = 5_000;
const MAX_DRIVER_BODY_BYTES: usize = 16 * 1024;
const MAX_CONTROL_TOKEN_BYTES: usize = 1_024;
const SUPPORTED_STRIPE_API_VERSION: &str = "2026-02-25.clover";
const SUPPORTED_INVARIANT_IDS: [&str; 5] = [
    "provider-object-unique",
    "webhook-effect-at-most-once",
    "paid-order-amount-conservation",
    "terminal-success-monotonic",
    "balanced-ledger",
];

/// Environment lookup boundary used by config resolution.
pub trait EnvironmentLookup {
    fn get(&self, name: &str) -> Option<String>;
}

/// Process environment implementation for the public CLI boundary.
pub struct ProcessEnvironment;

impl EnvironmentLookup for ProcessEnvironment {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "TxProof configuration v1")]
struct RawConfig {
    schema_version: u16,
    run: RawRunConfig,
    safety: RawSafetyConfig,
    compose: RawComposeConfig,
    database: RawDatabaseConfig,
    stripe: RawStripeConfig,
    driver: RawDriverConfig,
    faults: RawFaultConfig,
    invariants: Vec<RawInvariantConfig>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawRunConfig {
    cases: u32,
    seed: u64,
    max_actions_per_case: u32,
    case_timeout: String,
    parallelism: u32,
    artifact_dir: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawSafetyConfig {
    mode: SafetyMode,
    deny_public_ips: bool,
    deny_live_stripe_keys: bool,
    database_name_prefix: String,
    require_database_marker: bool,
    max_database_bytes: u64,
}

#[derive(Clone, Copy, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum SafetyMode {
    LocalDisposableOnly,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawComposeConfig {
    project_name: String,
    files: Vec<PathBuf>,
    application_service: String,
    postgres_service: String,
    worker_services: Vec<String>,
    health_url: String,
    health_timeout: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawDatabaseConfig {
    admin_url_env: String,
    case_url_env: String,
    strategy: DatabaseStrategy,
    case_database: String,
    baseline_database: String,
    invariant_role: String,
    quiescence_sql: PathBuf,
    quiescence_stable_for: String,
    quiescence_timeout: String,
    statement_timeout: String,
    lock_timeout: String,
}

#[derive(Clone, Copy, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum DatabaseStrategy {
    TemplateThenDumpFallback,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawStripeConfig {
    adapter: StripeAdapter,
    api_version: String,
    application_base_url: String,
    webhook_url: String,
    webhook_secret_env: String,
    control_url: String,
    control_token_env: String,
    poll_interval: String,
}

#[derive(Clone, Copy, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum StripeAdapter {
    PaymentIntentV1,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawDriverConfig {
    method: DriverMethod,
    url: String,
    body_file: PathBuf,
    timeout: String,
}

#[derive(Clone, Copy, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "UPPERCASE")]
enum DriverMethod {
    Post,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawFaultConfig {
    webhooks: RawWebhookFaultConfig,
    stripe_api: RawStripeApiFaultConfig,
    process: RawProcessFaultConfig,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawWebhookFaultConfig {
    duplicate_max: u32,
    allow_reorder: bool,
    allow_drop: bool,
    delay_ms: Vec<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawStripeApiFaultConfig {
    outcomes: Vec<StripeApiOutcome>,
}

#[derive(Clone, Copy, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum StripeApiOutcome {
    Normal,
    #[serde(rename = "pre_execute_429")]
    PreExecute429,
    #[serde(rename = "pre_execute_500")]
    PreExecute500,
    #[serde(rename = "post_execute_500")]
    PostExecute500,
    CommitThenClose,
    CommitThenDelay,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawProcessFaultConfig {
    service: String,
    max_kills_per_case: u32,
    cut_points: Vec<ProcessCutPoint>,
    sql_probe_file: PathBuf,
}

#[derive(Clone, Copy, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
enum ProcessCutPoint {
    ClientRequestForwarded,
    ClientResponseObserved,
    WebhookRequestForwarded,
    WebhookResponseObserved,
    SqlProbe,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RawInvariantConfig {
    id: String,
    sql_file: PathBuf,
    expect: InvariantExpectation,
}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum InvariantExpectation {
    ZeroRows,
}

/// Fully resolved config. It deliberately implements neither `Debug` nor
/// `Serialize`, because it owns validated secret-bearing URLs.
pub struct ResolvedConfig {
    root: PathBuf,
    source_path: PathBuf,
    compose_project: String,
    compose_files: Vec<PathBuf>,
    application_service: String,
    postgres_service: String,
    stripe_service: String,
    worker_services: Vec<String>,
    campaign_spec: CampaignSpec,
    private: ResolvedPrivate,
    redacted: RedactedConfig,
}

impl ResolvedConfig {
    #[must_use]
    pub const fn redacted(&self) -> &RedactedConfig {
        &self.redacted
    }

    #[must_use]
    pub const fn campaign_spec(&self) -> &CampaignSpec {
        &self.campaign_spec
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }

    pub(crate) fn compose_files(&self) -> &[PathBuf] {
        &self.compose_files
    }

    pub(crate) fn compose_project(&self) -> &str {
        &self.compose_project
    }

    pub(crate) fn admin_url(&self) -> &Url {
        &self.private.admin_url.value
    }

    pub(crate) fn case_url(&self) -> &Url {
        &self.private.case_url.value
    }

    pub(crate) fn case_database(&self) -> &str {
        &self.private.case_database
    }

    pub(crate) fn baseline_database(&self) -> &str {
        &self.private.baseline_database
    }

    pub(crate) const fn max_database_bytes(&self) -> u64 {
        self.private.max_database_bytes
    }

    pub(crate) fn application_service(&self) -> &str {
        &self.application_service
    }

    pub(crate) fn postgres_service(&self) -> &str {
        &self.postgres_service
    }

    pub(crate) fn stripe_service(&self) -> &str {
        &self.stripe_service
    }

    pub(crate) fn worker_services(&self) -> &[String] {
        &self.worker_services
    }

    pub(crate) const fn statement_timeout(&self) -> Duration {
        self.private.statement_timeout
    }

    pub(crate) const fn lock_timeout(&self) -> Duration {
        self.private.lock_timeout
    }

    pub(crate) fn invariant_files(&self) -> &[ResolvedInvariantFile] {
        &self.private.invariant_files
    }

    pub(crate) fn invariant_role(&self) -> &str {
        &self.private.invariant_role
    }

    pub(crate) fn sql_probe_file(&self) -> &Path {
        &self.private.sql_probe
    }

    pub(crate) fn artifact_dir(&self) -> &Path {
        &self.private.artifact_dir
    }

    pub(crate) const fn case_timeout(&self) -> Duration {
        self.private.case_timeout
    }

    pub(crate) fn health_url(&self) -> &Url {
        &self.private.health_url
    }

    pub(crate) const fn health_timeout(&self) -> Duration {
        self.private.health_timeout
    }

    pub(crate) fn fixture_control_url(&self) -> &Url {
        &self.private.fixture_control_url
    }

    pub(crate) fn fixture_control_token(&self) -> &str {
        &self.private.fixture_control_token.value
    }

    pub(crate) const fn fixture_poll_interval(&self) -> Duration {
        self.private.fixture_poll_interval
    }

    pub(crate) fn quiescence_sql_file(&self) -> &Path {
        &self.private.quiescence_sql
    }

    pub(crate) const fn quiescence_stable_for(&self) -> Duration {
        self.private.quiescence_stable_for
    }

    pub(crate) const fn quiescence_timeout(&self) -> Duration {
        self.private.quiescence_timeout
    }

    pub(crate) fn driver_url(&self) -> &Url {
        &self.private.driver_url
    }

    pub(crate) fn driver_body(&self) -> &serde_json::Value {
        &self.private.driver_body
    }

    pub(crate) const fn driver_timeout(&self) -> Duration {
        self.private.driver_timeout
    }

    pub(crate) fn into_redacted(self) -> RedactedConfig {
        self.redacted
    }
}

struct SecretUrl {
    value: Url,
}

struct SecretString {
    value: String,
}

struct ResolvedPrivate {
    artifact_dir: PathBuf,
    case_timeout: Duration,
    health_url: Url,
    health_timeout: Duration,
    admin_url: SecretUrl,
    case_url: SecretUrl,
    case_database: String,
    baseline_database: String,
    max_database_bytes: u64,
    _webhook_secret: SecretString,
    fixture_control_url: Url,
    fixture_control_token: SecretString,
    fixture_poll_interval: Duration,
    quiescence_sql: PathBuf,
    quiescence_stable_for: Duration,
    quiescence_timeout: Duration,
    statement_timeout: Duration,
    lock_timeout: Duration,
    _stripe_base: Url,
    _webhook_url: Url,
    driver_url: Url,
    driver_body: serde_json::Value,
    driver_timeout: Duration,
    sql_probe: PathBuf,
    invariant_role: String,
    invariant_files: Vec<ResolvedInvariantFile>,
}

pub(crate) struct ResolvedInvariantFile {
    id: InvariantId,
    path: PathBuf,
}

impl ResolvedInvariantFile {
    pub(crate) fn id(&self) -> &str {
        self.id.as_str()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Allowlisted projection safe for persistence and standard output.
#[derive(Serialize)]
pub struct RedactedConfig {
    schema_version: u16,
    run: RedactedRunConfig,
    safety: RedactedSafetyConfig,
    compose: RedactedComposeConfig,
    database: RedactedDatabaseConfig,
    stripe: RedactedStripeConfig,
    driver: RedactedDriverConfig,
    invariants: Vec<RedactedInvariantConfig>,
}

#[derive(Serialize)]
struct RedactedRunConfig {
    cases: u32,
    seed: u64,
    max_actions_per_case: u32,
    case_timeout_ms: u64,
    parallelism: u32,
    artifact_dir: String,
}

#[derive(Serialize)]
struct RedactedSafetyConfig {
    mode: SafetyMode,
    deny_public_ips: bool,
    deny_live_stripe_keys: bool,
    database_name_prefix: String,
    require_database_marker: bool,
    max_database_bytes: u64,
}

#[derive(Serialize)]
struct RedactedComposeConfig {
    project_name: String,
    files: Vec<String>,
    application_service: String,
    postgres_service: String,
    worker_services: Vec<String>,
    health_url: String,
    health_timeout_ms: u64,
}

#[derive(Serialize)]
struct RedactedDatabaseConfig {
    admin_url_env: String,
    case_url_env: String,
    strategy: DatabaseStrategy,
    case_database: String,
    baseline_database: String,
    invariant_role: String,
    quiescence_sql: String,
    quiescence_stable_for_ms: u64,
    quiescence_timeout_ms: u64,
    statement_timeout_ms: u64,
    lock_timeout_ms: u64,
}

#[derive(Serialize)]
struct RedactedStripeConfig {
    adapter: StripeAdapter,
    api_version: String,
    application_base_url: String,
    webhook_url: String,
    webhook_secret_env: String,
    control_url: String,
    control_token_env: String,
    poll_interval_ms: u64,
}

#[derive(Serialize)]
struct RedactedDriverConfig {
    method: DriverMethod,
    url: String,
    body_file: String,
    timeout_ms: u64,
}

#[derive(Serialize)]
struct RedactedInvariantConfig {
    id: String,
    expect: &'static str,
}

/// Loads a versioned config file and resolves it relative to its repository.
///
/// # Errors
///
/// Returns [`ConfigError`] for I/O, TOML, environment, path, or semantic
/// safety failures.
pub fn load_resolved_config(
    path: &Path,
    environment: &impl EnvironmentLookup,
) -> Result<ResolvedConfig, ConfigError> {
    let canonical_path = path.canonicalize().map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let file = fs::File::open(&canonical_path).map_err(|source| ConfigError::Read {
        path: canonical_path.clone(),
        source,
    })?;
    let mut document = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut document)
        .map_err(|source| ConfigError::Read {
            path: canonical_path.clone(),
            source,
        })?;
    reject_oversized_document(&document)?;
    let root = canonical_path
        .parent()
        .ok_or(ConfigError::ConfigHasNoParent)?;
    let mut resolved = resolve_config_document(&document, root, environment)?;
    resolved.source_path = canonical_path;
    Ok(resolved)
}

/// Resolves an in-memory config document against one repository directory.
///
/// # Errors
///
/// Returns [`ConfigError`] for TOML, environment, path, or semantic safety
/// failures.
pub fn resolve_config_document(
    document: &str,
    root: &Path,
    environment: &impl EnvironmentLookup,
) -> Result<ResolvedConfig, ConfigError> {
    reject_oversized_document(document)?;
    let raw: RawConfig = toml::from_str(document).map_err(|_| ConfigError::Parse)?;
    resolve_raw_config(raw, root, environment)
}

/// Generates the pinned development JSON Schema for version-one config.
///
/// # Errors
///
/// Returns [`serde_json::Error`] if schema serialization fails.
pub fn config_schema_json() -> Result<String, serde_json::Error> {
    let schema = schema_for!(RawConfig);
    let mut schema = serde_json::to_value(schema)?;
    schema["properties"]["schema_version"] = serde_json::json!({
        "const": CONFIG_SCHEMA_VERSION,
        "type": "integer"
    });
    serde_json::to_string_pretty(&schema).map(|mut encoded| {
        encoded.push('\n');
        encoded
    })
}

fn reject_oversized_document(document: &str) -> Result<(), ConfigError> {
    if document.len() as u64 > MAX_CONFIG_BYTES {
        Err(ConfigError::ConfigTooLarge)
    } else {
        Ok(())
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "keep cross-field safety validation in one auditable sequence"
)]
fn resolve_raw_config(
    raw: RawConfig,
    root: &Path,
    environment: &impl EnvironmentLookup,
) -> Result<ResolvedConfig, ConfigError> {
    if raw.schema_version != CONFIG_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchemaVersion {
            actual: raw.schema_version,
        });
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|_| ConfigError::UnsafePath(root.to_owned()))?;
    let repository_root = repository_root(&canonical_root)?;

    let case_timeout =
        bounded_duration(&raw.run.case_timeout, MAX_CASE_TIMEOUT, "run.case_timeout")?;
    if !(1..=MAX_CASES).contains(&raw.run.cases)
        || !(1..=MAX_ACTIONS_PER_CASE).contains(&raw.run.max_actions_per_case)
    {
        return Err(ConfigError::UnsafeRunBudget);
    }
    if raw.run.parallelism != 1 {
        return Err(ConfigError::UnsafeParallelism);
    }
    let artifact_dir = safe_output_path(&canonical_root, &raw.run.artifact_dir)?;

    if !raw.safety.deny_public_ips
        || !raw.safety.deny_live_stripe_keys
        || !raw.safety.require_database_marker
        || raw.safety.database_name_prefix != "tiv_case_"
        || raw.safety.max_database_bytes == 0
        || raw.safety.max_database_bytes > MAX_DATABASE_BYTES
    {
        return Err(ConfigError::UnsafeSafetyPolicy);
    }

    validate_compose_project(&raw.compose.project_name)?;
    validate_service_name(&raw.compose.application_service)?;
    validate_service_name(&raw.compose.postgres_service)?;
    if raw.compose.application_service == raw.compose.postgres_service {
        return Err(ConfigError::AmbiguousComposeServices);
    }
    let mut services = BTreeSet::from([
        raw.compose.application_service.clone(),
        raw.compose.postgres_service.clone(),
    ]);
    for worker in &raw.compose.worker_services {
        validate_service_name(worker)?;
        if !services.insert(worker.clone()) {
            return Err(ConfigError::AmbiguousComposeServices);
        }
    }
    if raw.compose.files.is_empty() {
        return Err(ConfigError::MissingComposeFiles);
    }
    let compose_files = raw
        .compose
        .files
        .iter()
        .map(|path| existing_repository_file(&canonical_root, &repository_root, path))
        .collect::<Result<Vec<_>, _>>()?;
    let health_url = local_http_url(&raw.compose.health_url)?;
    let health_timeout = bounded_duration(
        &raw.compose.health_timeout,
        MAX_HEALTH_TIMEOUT,
        "compose.health_timeout",
    )?;

    validate_environment_name(&raw.database.admin_url_env)?;
    validate_environment_name(&raw.database.case_url_env)?;
    validate_environment_name(&raw.stripe.webhook_secret_env)?;
    validate_environment_name(&raw.stripe.control_token_env)?;
    validate_database_name(
        &raw.database.case_database,
        &raw.safety.database_name_prefix,
    )?;
    validate_database_name(&raw.database.baseline_database, "tiv_base_")?;
    validate_role_name(&raw.database.invariant_role)?;
    let admin_url = secret_database_url(
        environment,
        &raw.database.admin_url_env,
        &raw.compose.postgres_service,
        None,
    )?;
    let case_url = secret_database_url(
        environment,
        &raw.database.case_url_env,
        &raw.compose.postgres_service,
        Some(&raw.database.case_database),
    )?;
    let webhook_secret = environment
        .get(&raw.stripe.webhook_secret_env)
        .ok_or_else(|| ConfigError::MissingEnvironment(raw.stripe.webhook_secret_env.clone()))?;
    if webhook_secret.trim().is_empty() {
        return Err(ConfigError::MissingEnvironment(
            raw.stripe.webhook_secret_env.clone(),
        ));
    }
    reject_live_stripe_material(&webhook_secret)?;
    let fixture_control_token = environment
        .get(&raw.stripe.control_token_env)
        .ok_or_else(|| ConfigError::MissingEnvironment(raw.stripe.control_token_env.clone()))?;
    if fixture_control_token.trim().is_empty()
        || fixture_control_token.len() > MAX_CONTROL_TOKEN_BYTES
        || HeaderValue::from_str(&fixture_control_token).is_err()
    {
        return Err(ConfigError::InvalidFixtureControlToken);
    }
    reject_live_stripe_material(&fixture_control_token)?;

    let quiescence_sql = existing_repository_file(
        &canonical_root,
        &repository_root,
        &raw.database.quiescence_sql,
    )?;
    let quiescence_stable_for = bounded_duration(
        &raw.database.quiescence_stable_for,
        MAX_QUIESCENCE_TIMEOUT,
        "database.quiescence_stable_for",
    )?;
    let quiescence_timeout = bounded_duration(
        &raw.database.quiescence_timeout,
        MAX_QUIESCENCE_TIMEOUT,
        "database.quiescence_timeout",
    )?;
    if quiescence_stable_for >= quiescence_timeout {
        return Err(ConfigError::UnsafeBudget("database.quiescence_stable_for"));
    }
    let statement_timeout = bounded_duration(
        &raw.database.statement_timeout,
        MAX_STATEMENT_TIMEOUT,
        "database.statement_timeout",
    )?;
    let lock_timeout = bounded_duration(
        &raw.database.lock_timeout,
        MAX_LOCK_TIMEOUT,
        "database.lock_timeout",
    )?;

    if raw.stripe.api_version != SUPPORTED_STRIPE_API_VERSION {
        return Err(ConfigError::UnsupportedStripeApiVersion);
    }
    let stripe_base = internal_http_url(&raw.stripe.application_base_url)?;
    reject_production_stripe_host(&stripe_base)?;
    let stripe_service = stripe_base
        .host_str()
        .ok_or(ConfigError::InvalidStripeTarget)?
        .to_owned();
    validate_service_name(&stripe_service).map_err(|_| ConfigError::InvalidStripeTarget)?;
    if services.contains(&stripe_service) {
        return Err(ConfigError::InvalidStripeTarget);
    }
    let webhook_url = internal_http_url(&raw.stripe.webhook_url)?;
    if webhook_url.host_str() != Some(raw.compose.application_service.as_str()) {
        return Err(ConfigError::InvalidWebhookTarget);
    }
    let fixture_control_url = local_http_origin(&raw.stripe.control_url)?;

    let driver_url = local_http_url(&raw.driver.url)?;
    if driver_url.port().is_none() {
        return Err(ConfigError::InvalidHttpUrl);
    }
    let driver_body_file =
        existing_repository_file(&canonical_root, &repository_root, &raw.driver.body_file)?;
    let driver_body = read_driver_body(&driver_body_file, &raw.database.case_database)?;
    let driver_timeout =
        bounded_duration(&raw.driver.timeout, MAX_DRIVER_TIMEOUT, "driver.timeout")?;
    let fixture_poll_interval = bounded_duration(
        &raw.stripe.poll_interval,
        MAX_DRIVER_TIMEOUT,
        "stripe.poll_interval",
    )?;
    if fixture_poll_interval > driver_timeout {
        return Err(ConfigError::UnsafeBudget("stripe.poll_interval"));
    }

    validate_faults(&raw.faults, &raw.compose.application_service)?;
    let campaign_spec = campaign_spec(&raw)?;
    let sql_probe = existing_repository_file(
        &canonical_root,
        &repository_root,
        &raw.faults.process.sql_probe_file,
    )?;

    if raw.invariants.len() != 5 {
        return Err(ConfigError::InvariantCount {
            actual: raw.invariants.len(),
        });
    }
    if !raw
        .invariants
        .iter()
        .map(|invariant| invariant.id.as_str())
        .eq(SUPPORTED_INVARIANT_IDS)
    {
        return Err(ConfigError::UnsupportedInvariantSet);
    }
    let mut invariant_files = Vec::with_capacity(raw.invariants.len());
    let mut redacted_invariants = Vec::with_capacity(raw.invariants.len());
    for invariant in &raw.invariants {
        let id = InvariantId::new(&invariant.id).map_err(|_| ConfigError::InvalidInvariantId)?;
        let path =
            existing_repository_file(&canonical_root, &repository_root, &invariant.sql_file)?;
        invariant_files.push(ResolvedInvariantFile { id, path });
        let expect = match invariant.expect {
            InvariantExpectation::ZeroRows => "zero_rows",
        };
        redacted_invariants.push(RedactedInvariantConfig {
            id: invariant.id.clone(),
            expect,
        });
    }

    let redacted = RedactedConfig {
        schema_version: raw.schema_version,
        run: RedactedRunConfig {
            cases: raw.run.cases,
            seed: raw.run.seed,
            max_actions_per_case: raw.run.max_actions_per_case,
            case_timeout_ms: duration_millis(case_timeout)?,
            parallelism: raw.run.parallelism,
            artifact_dir: display_path(&artifact_dir),
        },
        safety: RedactedSafetyConfig {
            mode: raw.safety.mode,
            deny_public_ips: raw.safety.deny_public_ips,
            deny_live_stripe_keys: raw.safety.deny_live_stripe_keys,
            database_name_prefix: raw.safety.database_name_prefix,
            require_database_marker: raw.safety.require_database_marker,
            max_database_bytes: raw.safety.max_database_bytes,
        },
        compose: RedactedComposeConfig {
            project_name: raw.compose.project_name.clone(),
            files: compose_files
                .iter()
                .map(|path| display_path(path))
                .collect(),
            application_service: raw.compose.application_service.clone(),
            postgres_service: raw.compose.postgres_service.clone(),
            worker_services: raw.compose.worker_services.clone(),
            health_url: health_url.to_string(),
            health_timeout_ms: duration_millis(health_timeout)?,
        },
        database: RedactedDatabaseConfig {
            admin_url_env: raw.database.admin_url_env,
            case_url_env: raw.database.case_url_env,
            strategy: raw.database.strategy,
            case_database: raw.database.case_database.clone(),
            baseline_database: raw.database.baseline_database.clone(),
            invariant_role: raw.database.invariant_role.clone(),
            quiescence_sql: display_path(&quiescence_sql),
            quiescence_stable_for_ms: duration_millis(quiescence_stable_for)?,
            quiescence_timeout_ms: duration_millis(quiescence_timeout)?,
            statement_timeout_ms: duration_millis(statement_timeout)?,
            lock_timeout_ms: duration_millis(lock_timeout)?,
        },
        stripe: RedactedStripeConfig {
            adapter: raw.stripe.adapter,
            api_version: raw.stripe.api_version,
            application_base_url: stripe_base.to_string(),
            webhook_url: webhook_url.to_string(),
            webhook_secret_env: raw.stripe.webhook_secret_env,
            control_url: fixture_control_url.to_string(),
            control_token_env: raw.stripe.control_token_env,
            poll_interval_ms: duration_millis(fixture_poll_interval)?,
        },
        driver: RedactedDriverConfig {
            method: raw.driver.method,
            url: driver_url.to_string(),
            body_file: display_path(&driver_body_file),
            timeout_ms: duration_millis(driver_timeout)?,
        },
        invariants: redacted_invariants,
    };

    Ok(ResolvedConfig {
        root: canonical_root.clone(),
        source_path: canonical_root.join("tiv.toml"),
        compose_project: raw.compose.project_name,
        compose_files,
        application_service: raw.compose.application_service,
        postgres_service: raw.compose.postgres_service,
        stripe_service,
        worker_services: raw.compose.worker_services,
        campaign_spec,
        private: ResolvedPrivate {
            artifact_dir,
            case_timeout,
            health_url,
            health_timeout,
            admin_url: SecretUrl { value: admin_url },
            case_url: SecretUrl { value: case_url },
            case_database: raw.database.case_database,
            baseline_database: raw.database.baseline_database,
            max_database_bytes: raw.safety.max_database_bytes,
            _webhook_secret: SecretString {
                value: webhook_secret,
            },
            fixture_control_url,
            fixture_control_token: SecretString {
                value: fixture_control_token,
            },
            fixture_poll_interval,
            quiescence_sql,
            quiescence_stable_for,
            quiescence_timeout,
            statement_timeout,
            lock_timeout,
            _stripe_base: stripe_base,
            _webhook_url: webhook_url,
            driver_url,
            driver_body,
            driver_timeout,
            sql_probe,
            invariant_role: raw.database.invariant_role,
            invariant_files,
        },
        redacted,
    })
}

fn repository_root(root: &Path) -> Result<PathBuf, ConfigError> {
    root.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .and_then(|ancestor| ancestor.canonicalize().ok())
        .ok_or(ConfigError::RepositoryRootNotFound)
}

fn existing_repository_file(
    root: &Path,
    repository_root: &Path,
    path: &Path,
) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty() {
        return Err(ConfigError::UnsafePath(path.to_owned()));
    }
    let candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    };
    let canonical = candidate
        .canonicalize()
        .map_err(|_| ConfigError::UnsafePath(path.to_owned()))?;
    if !canonical.starts_with(repository_root) || !canonical.is_file() {
        return Err(ConfigError::UnsafePath(path.to_owned()));
    }
    Ok(canonical)
}

fn safe_output_path(root: &Path, path: &Path) -> Result<PathBuf, ConfigError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(ConfigError::UnsafePath(path.to_owned()));
    }
    Ok(root.join(path))
}

fn read_driver_body(
    path: &Path,
    configured_case_database: &str,
) -> Result<serde_json::Value, ConfigError> {
    let file = fs::File::open(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut document = String::new();
    file.take((MAX_DRIVER_BODY_BYTES + 1) as u64)
        .read_to_string(&mut document)
        .map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
    if document.len() > MAX_DRIVER_BODY_BYTES {
        return Err(ConfigError::InvalidDriverBody);
    }
    reject_live_stripe_material(&document)?;
    let body: serde_json::Value =
        serde_json::from_str(&document).map_err(|_| ConfigError::InvalidDriverBody)?;
    let object = body.as_object().ok_or(ConfigError::InvalidDriverBody)?;
    let operation_id = object
        .get("operation_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| valid_operation_id(value))
        .ok_or(ConfigError::InvalidDriverBody)?;
    let amount_minor = object
        .get("amount_minor")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value > 0)
        .ok_or(ConfigError::InvalidDriverBody)?;
    let currency = object
        .get("currency")
        .and_then(serde_json::Value::as_str)
        .filter(|value| value.len() == 3 && value.bytes().all(|byte| byte.is_ascii_lowercase()))
        .ok_or(ConfigError::InvalidDriverBody)?;
    if object
        .get("database")
        .is_some_and(|value| value.as_str() != Some(configured_case_database))
    {
        return Err(ConfigError::InvalidDriverBody);
    }
    let _ = (operation_id, amount_minor, currency);
    Ok(body)
}

fn valid_operation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn validate_environment_name(name: &str) -> Result<(), ConfigError> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err(ConfigError::InvalidEnvironmentName);
    };
    if name.len() > 128
        || !(first.is_ascii_uppercase() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ConfigError::InvalidEnvironmentName);
    }
    Ok(())
}

fn validate_service_name(name: &str) -> Result<(), ConfigError> {
    if name.is_empty()
        || name.len() > 63
        || !name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
    {
        return Err(ConfigError::InvalidComposeService);
    }
    Ok(())
}

fn validate_compose_project(name: &str) -> Result<(), ConfigError> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err(ConfigError::InvalidComposeProject);
    };
    if name.len() > 63
        || !(first.is_ascii_lowercase() || first.is_ascii_digit())
        || !bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(ConfigError::InvalidComposeProject);
    }
    Ok(())
}

fn validate_database_name(name: &str, prefix: &str) -> Result<(), ConfigError> {
    let Some(suffix) = name.strip_prefix(prefix) else {
        return Err(ConfigError::InvalidDatabaseName);
    };
    if !(8..=54).contains(&suffix.len())
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ConfigError::InvalidDatabaseName);
    }
    Ok(())
}

fn validate_role_name(role: &str) -> Result<(), ConfigError> {
    let mut bytes = role.bytes();
    let Some(first) = bytes.next() else {
        return Err(ConfigError::InvalidDatabaseRole);
    };
    if role.len() > 63
        || !(first.is_ascii_lowercase() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ConfigError::InvalidDatabaseRole);
    }
    Ok(())
}

fn local_http_url(value: &str) -> Result<Url, ConfigError> {
    let url = internal_http_url(value)?;
    if !url.host_str().is_some_and(|host| {
        host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    }) {
        return Err(ConfigError::NonLocalHttpUrl);
    }
    Ok(url)
}

fn local_http_origin(value: &str) -> Result<Url, ConfigError> {
    let url = local_http_url(value)?;
    if url.port().is_none() || !matches!(url.path(), "" | "/") {
        return Err(ConfigError::InvalidHttpUrl);
    }
    Ok(url)
}

fn internal_http_url(value: &str) -> Result<Url, ConfigError> {
    let url = Url::parse(value).map_err(|_| ConfigError::InvalidHttpUrl)?;
    if url.scheme() != "http"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::InvalidHttpUrl);
    }
    Ok(url)
}

fn secret_database_url(
    environment: &impl EnvironmentLookup,
    name: &str,
    postgres_service: &str,
    expected_database: Option<&str>,
) -> Result<Url, ConfigError> {
    let value = environment
        .get(name)
        .ok_or_else(|| ConfigError::MissingEnvironment(name.to_owned()))?;
    reject_live_stripe_material(&value)?;
    let url = Url::parse(&value).map_err(|_| ConfigError::InvalidDatabaseUrl)?;
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        return Err(ConfigError::InvalidDatabaseUrl);
    }
    let host = url.host_str().ok_or(ConfigError::InvalidDatabaseUrl)?;
    let local = host == "localhost"
        || host == postgres_service
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if !local {
        return Err(ConfigError::NonLocalDatabaseUrl);
    }
    if expected_database.is_some_and(|database| url.path().trim_start_matches('/') != database) {
        return Err(ConfigError::InvalidDatabaseUrl);
    }
    Ok(url)
}

pub(crate) fn reject_live_stripe_material(value: &str) -> Result<(), ConfigError> {
    if ["sk_live_", "rk_live_", "pk_live_"]
        .iter()
        .any(|prefix| value.contains(prefix))
    {
        return Err(ConfigError::LiveStripeKey);
    }
    Ok(())
}

fn reject_production_stripe_host(url: &Url) -> Result<(), ConfigError> {
    let host = url.host_str().ok_or(ConfigError::InvalidHttpUrl)?;
    if host == "stripe.com" || host.ends_with(".stripe.com") {
        return Err(ConfigError::ProductionStripeHost);
    }
    Ok(())
}

fn validate_faults(faults: &RawFaultConfig, application_service: &str) -> Result<(), ConfigError> {
    if faults.webhooks.duplicate_max > MAX_WEBHOOK_DUPLICATES
        || !faults.webhooks.allow_reorder
        || !faults.webhooks.allow_drop
        || faults.webhooks.delay_ms.is_empty()
        || faults
            .webhooks
            .delay_ms
            .iter()
            .any(|delay| *delay > MAX_DELAY_MILLIS)
    {
        return Err(ConfigError::UnsafeFaultBudget);
    }
    let outcomes = faults
        .stripe_api
        .outcomes
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if outcomes.len() != 6 {
        return Err(ConfigError::UnsupportedFaultModel);
    }
    let cut_points = faults
        .process
        .cut_points
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if faults.process.service != application_service
        || faults.process.max_kills_per_case != 1
        || cut_points.len() != 5
    {
        return Err(ConfigError::UnsupportedFaultModel);
    }
    Ok(())
}

fn campaign_spec(raw: &RawConfig) -> Result<CampaignSpec, ConfigError> {
    let case_count = CaseCount::new(raw.run.cases).map_err(|_| ConfigError::UnsafeRunBudget)?;
    let action_budget = ActionBudget::new(raw.run.max_actions_per_case)
        .map_err(|_| ConfigError::UnsafeRunBudget)?;
    let duplicate_max = u8::try_from(raw.faults.webhooks.duplicate_max)
        .map_err(|_| ConfigError::UnsafeFaultBudget)?;
    let webhook_faults = WebhookFaultSpec::new(
        duplicate_max,
        raw.faults.webhooks.delay_ms.iter().copied(),
        raw.faults.webhooks.allow_reorder,
        raw.faults.webhooks.allow_drop,
    )
    .map_err(|_| ConfigError::UnsafeFaultBudget)?;
    let max_kills = u8::try_from(raw.faults.process.max_kills_per_case)
        .map_err(|_| ConfigError::UnsafeFaultBudget)?;
    let process_faults = ProcessFaultSpec::new(
        raw.faults
            .process
            .cut_points
            .iter()
            .copied()
            .map(|cut_point| match cut_point {
                ProcessCutPoint::ClientRequestForwarded => {
                    CoreProcessCutPoint::ClientRequestForwarded
                }
                ProcessCutPoint::ClientResponseObserved => {
                    CoreProcessCutPoint::ClientResponseObserved
                }
                ProcessCutPoint::WebhookRequestForwarded => {
                    CoreProcessCutPoint::WebhookRequestForwarded
                }
                ProcessCutPoint::WebhookResponseObserved => {
                    CoreProcessCutPoint::WebhookResponseObserved
                }
                ProcessCutPoint::SqlProbe => CoreProcessCutPoint::SqlProbe,
            }),
        max_kills,
    )
    .map_err(|_| ConfigError::UnsafeFaultBudget)?;
    let provider_outcomes = raw
        .faults
        .stripe_api
        .outcomes
        .iter()
        .copied()
        .map(|outcome| match outcome {
            StripeApiOutcome::Normal => ProviderOutcome::Normal,
            StripeApiOutcome::PreExecute429 => ProviderOutcome::PreExecute429,
            StripeApiOutcome::PreExecute500 => ProviderOutcome::PreExecute500,
            StripeApiOutcome::PostExecute500 => ProviderOutcome::PostExecute500,
            StripeApiOutcome::CommitThenClose => ProviderOutcome::CommitThenClose,
            StripeApiOutcome::CommitThenDelay => ProviderOutcome::CommitThenDelay,
        });

    CampaignSpec::new_payment_intent_v1(
        Seed::new(raw.run.seed),
        case_count,
        action_budget,
        provider_outcomes,
        webhook_faults,
        process_faults,
    )
    .map_err(|_| ConfigError::UnsupportedFaultModel)
}

fn bounded_duration(
    value: &str,
    maximum: Duration,
    field: &'static str,
) -> Result<Duration, ConfigError> {
    let (digits, multiplier) = if let Some(digits) = value.strip_suffix("ms") {
        (digits, 1_u64)
    } else if let Some(digits) = value.strip_suffix('s') {
        (digits, 1_000)
    } else if let Some(digits) = value.strip_suffix('m') {
        (digits, 60_000)
    } else {
        return Err(ConfigError::UnsafeBudget(field));
    };
    let millis = digits
        .parse::<u64>()
        .ok()
        .and_then(|amount| amount.checked_mul(multiplier))
        .ok_or(ConfigError::UnsafeBudget(field))?;
    let duration = Duration::from_millis(millis);
    if duration.is_zero() || duration > maximum {
        return Err(ConfigError::UnsafeBudget(field));
    }
    Ok(duration)
}

fn duration_millis(duration: Duration) -> Result<u64, ConfigError> {
    u64::try_from(duration.as_millis()).map_err(|_| ConfigError::DurationOverflow)
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("config TOML is invalid or contains unknown fields")]
    Parse,
    #[error("config exceeds the 1 MiB input limit")]
    ConfigTooLarge,
    #[error("config schema version {actual} is not supported")]
    UnsupportedSchemaVersion { actual: u16 },
    #[error("config path has no parent directory")]
    ConfigHasNoParent,
    #[error("config is not inside a Git repository")]
    RepositoryRootNotFound,
    #[error("config path is missing, outside the repository, or not a file: {0}")]
    UnsafePath(PathBuf),
    #[error("run budget is outside the v0 limits")]
    UnsafeRunBudget,
    #[error("v0 requires run.parallelism = 1")]
    UnsafeParallelism,
    #[error("local-disposable safety controls cannot be disabled or expanded")]
    UnsafeSafetyPolicy,
    #[error("Compose service name is invalid")]
    InvalidComposeService,
    #[error("Compose project name is invalid")]
    InvalidComposeProject,
    #[error("Compose service mappings overlap")]
    AmbiguousComposeServices,
    #[error("at least one Compose file is required")]
    MissingComposeFiles,
    #[error("environment-variable reference is invalid")]
    InvalidEnvironmentName,
    #[error("required environment variable {0} is missing")]
    MissingEnvironment(String),
    #[error("database name is outside the generated lowercase ASCII contract")]
    InvalidDatabaseName,
    #[error("database role is invalid")]
    InvalidDatabaseRole,
    #[error("database URL is invalid")]
    InvalidDatabaseUrl,
    #[error("database URL is not loopback or the configured Compose service")]
    NonLocalDatabaseUrl,
    #[error("HTTP URL is invalid")]
    InvalidHttpUrl,
    #[error("host-facing HTTP URL is not loopback")]
    NonLocalHttpUrl,
    #[error("configured value contains a live or restricted-live Stripe key")]
    LiveStripeKey,
    #[error("production Stripe hosts are forbidden")]
    ProductionStripeHost,
    #[error("Stripe fixture URL must target a distinct local Compose service")]
    InvalidStripeTarget,
    #[error("Stripe API version is unsupported")]
    UnsupportedStripeApiVersion,
    #[error("webhook URL does not target the configured application service")]
    InvalidWebhookTarget,
    #[error("fixture control token is outside the bounded HTTP header contract")]
    InvalidFixtureControlToken,
    #[error("driver body is not a bounded payment_intent_v1 JSON object")]
    InvalidDriverBody,
    #[error("fault budget is outside the v0 limits")]
    UnsafeFaultBudget,
    #[error("fault model differs from the supported v0 model")]
    UnsupportedFaultModel,
    #[error("exactly five invariants are required, found {actual}")]
    InvariantCount { actual: usize },
    #[error("invariants must be the fixed five version-one IDs in canonical order")]
    UnsupportedInvariantSet,
    #[error("invariant ID is invalid")]
    InvalidInvariantId,
    #[error("duration budget is invalid for {0}")]
    UnsafeBudget(&'static str),
    #[error("duration cannot be represented in the redacted contract")]
    DurationOverflow,
}
