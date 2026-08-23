//! Configured serial campaign preparation and execution.

use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use reqwest::{StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiv_core::{
    decision::Seed,
    plan::{CampaignCompileError, CampaignPlan, CampaignPlanner, CaseCount},
};

use crate::{
    artifacts::{ArtifactError, PartialRunClass, RunArtifactStaging},
    baseline::{BaselineError, ConfiguredBaselineSession},
    compatibility::{CompatibilityCaptureError, capture_run_compatibility},
    config::{ConfigError, EnvironmentLookup, ResolvedConfig, load_resolved_config},
    configured_database::ConfiguredDatabaseError,
    configured_process::{
        ConfiguredProcessControl, ConfiguredProcessError, attest_configured_service_images,
    },
    doctor::{DoctorError, collect_compose_facts},
    postgres::{
        probe::{ConfiguredSqlProbe, ConfiguredSqlProbeError, load_configured_sql_probe},
        quiescence::{ConfiguredQuiescence, ConfiguredQuiescenceError, load_configured_quiescence},
        safety::DatabaseName,
        snapshot::{
            ConfiguredSnapshot, ConfiguredSnapshotError, InvariantVerdict, load_configured_snapshot,
        },
    },
    reference_case::{
        CaseSqlProbe, ReferenceCaseRunConfig, ReferenceCaseRunConfigError, ReferenceCaseRunError,
        preflight_reference_planned_case, run_configured_planned_case_with_process,
    },
    run_supervisor::{
        ComposeProjectLock, SupervisedCaseOutcome, complete_supervision, supervise_execution,
    },
};

pub use crate::run_supervisor::RunCancellation;

const MAX_FIXTURE_STATE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConfiguredCampaignOptions {
    seed: Option<u64>,
    cases: Option<u32>,
    ci: bool,
}

impl ConfiguredCampaignOptions {
    /// Creates typed command-line overrides without expanding v1 case limits.
    ///
    /// # Errors
    ///
    /// Returns [`ConfiguredCampaignOptionsError`] for an invalid case count.
    pub fn new(
        seed: Option<u64>,
        cases: Option<u32>,
        ci: bool,
    ) -> Result<Self, ConfiguredCampaignOptionsError> {
        if cases.is_some_and(|cases| CaseCount::new(cases).is_err()) {
            return Err(ConfiguredCampaignOptionsError);
        }
        Ok(Self { seed, cases, ci })
    }

    #[must_use]
    pub const fn seed(self) -> Option<u64> {
        self.seed
    }

    #[must_use]
    pub const fn cases(self) -> Option<u32> {
        self.cases
    }

    #[must_use]
    pub const fn ci(self) -> bool {
        self.ci
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("configured campaign case override is outside the v1 limit")]
pub struct ConfiguredCampaignOptionsError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfiguredCampaignVerdict {
    Held,
    Violated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfiguredCampaignFailureClass {
    Configuration,
    Infrastructure,
    Inconclusive,
    Interrupted,
}

impl ConfiguredCampaignFailureClass {
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Configuration => 2,
            Self::Infrastructure => 3,
            Self::Inconclusive => 4,
            Self::Interrupted => 130,
        }
    }
}

#[derive(Serialize)]
pub struct ConfiguredCampaignOutput {
    schema_version: u16,
    status: &'static str,
    run_id: String,
    verdict: ConfiguredCampaignVerdict,
    completed_cases: usize,
    artifact_path: PathBuf,
}

impl ConfiguredCampaignOutput {
    #[must_use]
    pub const fn verdict(&self) -> ConfiguredCampaignVerdict {
        self.verdict
    }

    #[must_use]
    pub fn artifact_path(&self) -> &Path {
        &self.artifact_path
    }

    /// Serializes the secret-free command result.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if the allowlisted report cannot encode.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Executes one fully prepared configured campaign against the attested local
/// disposable stack.
///
/// All plans and repository-owned SQL contracts are loaded and preflighted
/// before the first database reset. A completed artifact directory is exposed
/// only after its manifest is written and the staging directory is atomically
/// renamed.
///
/// # Errors
///
/// Returns [`ConfiguredCampaignError`] for configuration, preparation, safety,
/// infrastructure, case execution, oracle, or artifact-finalization failures.
pub async fn run_configured_campaign(
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    options: ConfiguredCampaignOptions,
) -> Result<ConfiguredCampaignOutput, ConfiguredCampaignError> {
    Box::pin(run_configured_campaign_with_cancellation(
        config_path,
        environment,
        options,
        &RunCancellation::new(),
    ))
    .await
}

/// Executes a configured campaign that can be cancelled by the command root.
///
/// # Errors
///
/// Returns [`ConfiguredCampaignError`] after recovery and partial-evidence
/// finalization when execution has already entered the mutable run boundary.
pub async fn run_configured_campaign_with_cancellation(
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    options: ConfiguredCampaignOptions,
    cancellation: &RunCancellation,
) -> Result<ConfiguredCampaignOutput, ConfiguredCampaignError> {
    let config = load_resolved_config(config_path, environment)?;
    let case_override = options
        .cases
        .map(CaseCount::new)
        .transpose()
        .map_err(|_| ConfiguredCampaignError::Options(ConfiguredCampaignOptionsError))?;
    let effective_spec = config
        .campaign_spec()
        .with_run_overrides(options.seed.map(Seed::new), case_override);
    let campaign = CampaignPlanner::compile(&effective_spec)
        .map_err(ConfiguredCampaignError::CampaignCompile)?;
    let configured_probe = load_configured_sql_probe(&config)?;
    let configured_quiescence = load_configured_quiescence(&config)?;
    let configured_snapshot = load_configured_snapshot(&config)?;
    for case in campaign.cases() {
        preflight_reference_planned_case(case.plan(), true, true)?;
    }
    if cancellation.is_cancelled() {
        return Err(ConfiguredCampaignError::Interrupted { case_id: None });
    }
    let _project_lock = ComposeProjectLock::try_acquire(config.root(), config.compose_project())
        .map_err(|error| ConfiguredCampaignError::ProjectLock(Box::new(error)))?;

    let run_id = format!("run_{}", uuid::Uuid::new_v4().simple());
    let mut artifacts = RunArtifactStaging::create(config.root(), config.artifact_dir(), &run_id)?;
    artifacts.write_json("config.redacted.json", config.redacted())?;
    artifacts.write_json("campaign-plan.json", &campaign)?;
    let mut case_results = Vec::with_capacity(campaign.cases().len());

    let execution = execute_staged_campaign(
        &config,
        &campaign,
        &configured_probe,
        &configured_quiescence,
        &configured_snapshot,
        &run_id,
        &mut artifacts,
        &mut case_results,
        cancellation,
    )
    .await;
    let campaign_verdict = match execution {
        Ok(verdict) => verdict,
        Err(cause) => {
            let failure_class = cause.failure_class();
            let failure_code = cause.failure_code();
            let summary = PartialCampaignSummary {
                schema_version: 1,
                status: partial_status(failure_class),
                run_id: &run_id,
                failure_class,
                failure_code,
                configured_cases: campaign.cases().len(),
                completed_cases: case_results.len(),
                cases: &case_results,
            };
            if let Err(artifact_error) = artifacts.write_json("summary.json", &summary) {
                return Err(ConfiguredCampaignError::PartialFinalization {
                    cause: Box::new(cause),
                    artifact: Box::new(artifact_error),
                });
            }
            let artifact_path = match artifacts
                .finalize_partial(partial_artifact_class(failure_class), failure_code)
            {
                Ok(path) => path,
                Err(artifact_error) => {
                    return Err(ConfiguredCampaignError::PartialFinalization {
                        cause: Box::new(cause),
                        artifact: Box::new(artifact_error),
                    });
                }
            };
            return Err(ConfiguredCampaignError::RunFailed {
                cause: Box::new(cause),
                artifact_path,
            });
        }
    };

    let completed_cases = case_results.len();
    let summary = CampaignSummary {
        schema_version: 1,
        run_id: run_id.clone(),
        verdict: campaign_verdict,
        configured_cases: campaign.cases().len(),
        completed_cases,
        cases: case_results,
    };
    artifacts.write_json("summary.json", &summary)?;
    let artifact_path = artifacts.finalize()?;
    Ok(ConfiguredCampaignOutput {
        schema_version: 1,
        status: "campaign_complete",
        run_id,
        verdict: campaign_verdict,
        completed_cases,
        artifact_path,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn execute_staged_campaign(
    config: &ResolvedConfig,
    campaign: &CampaignPlan,
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
    run_id: &str,
    artifacts: &mut RunArtifactStaging,
    case_results: &mut Vec<CaseArtifact>,
    cancellation: &RunCancellation,
) -> Result<ConfiguredCampaignVerdict, ConfiguredCampaignError> {
    if cancellation.is_cancelled() {
        return Err(ConfiguredCampaignError::Interrupted { case_id: None });
    }

    let compose = collect_compose_facts(config).await?;
    let mut baseline = ConfiguredBaselineSession::attest(config).await?;
    ConfiguredProcessControl::attest(config).await?;
    let service_images = attest_configured_service_images(config, &compose).await?;
    let compatibility = capture_run_compatibility(
        config,
        &compose,
        baseline.baseline_identity(),
        &service_images,
        configured_probe,
        configured_quiescence,
        configured_snapshot,
    )?;
    artifacts.write_json("compatibility.json", &compatibility)?;
    let case_database_name = DatabaseName::parse(config.case_database())
        .map_err(|_| ConfiguredCampaignError::CaseDatabaseName)?;
    let provider_proxy_url = driver_origin(config.driver_url())?;
    let mut campaign_verdict = ConfiguredCampaignVerdict::Held;

    for case in campaign.cases() {
        let case_id = format!("case_{:04}", case.id().value());
        if cancellation.is_cancelled() {
            return Err(ConfiguredCampaignError::Interrupted {
                case_id: Some(case_id),
            });
        }
        let (fresh_baseline, reset) = baseline.reset_case().await?;
        baseline = fresh_baseline;
        let database = baseline.attest_case_database().await?;
        let mut process = ConfiguredProcessControl::attest(config).await?;
        let reset_sequence = fixture_control_sequence(config)
            .await?
            .checked_add(1)
            .ok_or(ConfiguredCampaignError::FixtureSequenceExhausted)?;
        let run_config = ReferenceCaseRunConfig::from_http_contract(
            &case_database_name,
            config.driver_url().as_str(),
            config.driver_body().clone(),
            provider_proxy_url.as_str(),
            config.fixture_control_url().as_str(),
            config.fixture_control_token().to_owned(),
            reset_sequence,
            current_unix_timestamp()?,
            config.driver_timeout(),
            config.fixture_poll_interval(),
        )?;
        let mut sql_probe = database
            .open_sql_probe(
                configured_probe.clone(),
                config.case_timeout(),
                config.fixture_poll_interval(),
            )
            .await?;
        let mut quiescence = database
            .open_quiescence(
                configured_quiescence.clone(),
                config.fixture_poll_interval(),
            )
            .await?;
        let journal_path =
            artifacts.prepare_path(format!("cases/{case_id}/observations.ndjson"))?;
        let outcome = supervise_execution(
            config.case_timeout(),
            cancellation,
            run_configured_planned_case_with_process(
                run_id.to_owned(),
                case_id.clone(),
                case.plan(),
                journal_path,
                run_config,
                &mut process,
                Some(&mut sql_probe as &mut dyn CaseSqlProbe),
                &mut quiescence,
            ),
        )
        .await;
        let supervised = complete_supervision(&mut process, outcome).await;
        let probe_close = sql_probe.close().await;
        let quiescence_close = quiescence.close().await;
        let (outcome, process_recovery) = supervised.into_parts();
        let primary = match outcome {
            SupervisedCaseOutcome::Completed(result) => {
                result.map_err(ConfiguredCampaignError::CaseRun)
            }
            SupervisedCaseOutcome::TimedOut => Err(ConfiguredCampaignError::CaseTimedOut {
                case_id: case_id.clone(),
            }),
            SupervisedCaseOutcome::Cancelled => Err(ConfiguredCampaignError::Interrupted {
                case_id: Some(case_id.clone()),
            }),
        };
        let cleanup =
            configured_case_cleanup_error(process_recovery, probe_close, quiescence_close);
        let receipt = match (primary, cleanup) {
            (Ok(receipt), None) => receipt,
            (Err(cause), None) => return Err(cause),
            (Ok(_), Some(recovery)) => {
                return Err(ConfiguredCampaignError::RecoveryFailed {
                    cause: None,
                    recovery,
                });
            }
            (Err(cause), Some(recovery)) => {
                return Err(ConfiguredCampaignError::RecoveryFailed {
                    cause: Some(Box::new(cause)),
                    recovery,
                });
            }
        };
        if cancellation.is_cancelled() {
            return Err(ConfiguredCampaignError::Interrupted {
                case_id: Some(case_id),
            });
        }
        let (executed, checkpoint) = receipt.into_parts();
        artifacts.write_json(format!("cases/{case_id}/trace.json"), executed.trace())?;
        let action_count = executed.trace().action_count();
        let journal_record_count = executed.journal_summary().record_count();
        let journal_last_record_hash = executed
            .journal_summary()
            .last_record_hash()
            .map(str::to_owned);
        let (provider_objects, quiescence_permit) = checkpoint.into_oracle_parts();
        let snapshot = database
            .open_snapshot(configured_snapshot.clone())
            .await?
            .run(&provider_objects, quiescence_permit)
            .await?;
        let invariants = snapshot
            .outcomes()
            .iter()
            .map(|outcome| {
                let (verdict, witness_count) = match outcome.verdict() {
                    InvariantVerdict::Held => (ConfiguredCampaignVerdict::Held, 0),
                    InvariantVerdict::Violated(witnesses) => {
                        campaign_verdict = ConfiguredCampaignVerdict::Violated;
                        (ConfiguredCampaignVerdict::Violated, witnesses.len())
                    }
                };
                InvariantArtifact {
                    invariant_id: outcome.id().to_owned(),
                    checkpoint_id: outcome.identity().checkpoint().as_str().to_owned(),
                    verdict,
                    witness_count,
                }
            })
            .collect::<Vec<_>>();
        let result = CaseArtifact {
            schema_version: 1,
            case_id: case_id.clone(),
            seed: case.plan().seed().value(),
            planned_action_count: case.plan().actions().len(),
            executed_action_count: action_count,
            journal_record_count,
            journal_last_record_hash,
            before_database_oid: reset.before_database_oid(),
            after_database_oid: reset.after_database_oid(),
            before_marker_uuid: reset.before_marker_uuid().to_owned(),
            after_marker_uuid: reset.after_marker_uuid().to_owned(),
            provider_object_count: provider_objects.len(),
            invariants,
        };
        artifacts.write_json(format!("cases/{case_id}/result.json"), &result)?;
        case_results.push(result);
    }

    Ok(campaign_verdict)
}

fn driver_origin(driver_url: &reqwest::Url) -> Result<reqwest::Url, ConfiguredCampaignError> {
    let mut origin = driver_url.clone();
    origin.set_path("/");
    origin.set_query(None);
    origin.set_fragment(None);
    if origin.username().is_empty() && origin.password().is_none() {
        Ok(origin)
    } else {
        Err(ConfiguredCampaignError::DriverOrigin)
    }
}

fn current_unix_timestamp() -> Result<i64, ConfiguredCampaignError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ConfiguredCampaignError::Clock)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| ConfiguredCampaignError::Clock)
}

async fn fixture_control_sequence(
    config: &crate::config::ResolvedConfig,
) -> Result<u64, ConfiguredCampaignError> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(config.driver_timeout())
        .timeout(config.driver_timeout())
        .build()
        .map_err(ConfiguredCampaignError::FixtureRequest)?;
    let response = client
        .get(
            config
                .fixture_control_url()
                .join("v1/control/state")
                .map_err(|_| ConfiguredCampaignError::FixtureState)?,
        )
        .header("X-Tiv-Control-Token", config.fixture_control_token())
        .send()
        .await
        .map_err(ConfiguredCampaignError::FixtureRequest)?;
    if response.status() != StatusCode::OK {
        return Err(ConfiguredCampaignError::FixtureState);
    }
    let body = response
        .bytes()
        .await
        .map_err(ConfiguredCampaignError::FixtureRequest)?;
    if body.len() > MAX_FIXTURE_STATE_BYTES {
        return Err(ConfiguredCampaignError::FixtureState);
    }
    let state: FixtureState =
        serde_json::from_slice(&body).map_err(|_| ConfiguredCampaignError::FixtureState)?;
    if !state.held_gates.is_empty() {
        return Err(ConfiguredCampaignError::FixtureState);
    }
    Ok(state.command_sequence)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureState {
    command_sequence: u64,
    #[serde(rename = "remaining_outcomes")]
    _remaining_outcomes: usize,
    held_gates: Vec<serde_json::Value>,
    #[serde(rename = "payment_intents")]
    _payment_intents: Vec<serde_json::Value>,
}

#[derive(Serialize)]
struct CampaignSummary {
    schema_version: u16,
    run_id: String,
    verdict: ConfiguredCampaignVerdict,
    configured_cases: usize,
    completed_cases: usize,
    cases: Vec<CaseArtifact>,
}

#[derive(Serialize)]
struct PartialCampaignSummary<'a> {
    schema_version: u16,
    status: &'static str,
    run_id: &'a str,
    failure_class: ConfiguredCampaignFailureClass,
    failure_code: &'static str,
    configured_cases: usize,
    completed_cases: usize,
    cases: &'a [CaseArtifact],
}

const fn partial_status(class: ConfiguredCampaignFailureClass) -> &'static str {
    match class {
        ConfiguredCampaignFailureClass::Configuration
        | ConfiguredCampaignFailureClass::Infrastructure => "failed",
        ConfiguredCampaignFailureClass::Inconclusive => "inconclusive",
        ConfiguredCampaignFailureClass::Interrupted => "interrupted",
    }
}

const fn partial_artifact_class(class: ConfiguredCampaignFailureClass) -> PartialRunClass {
    match class {
        ConfiguredCampaignFailureClass::Configuration => PartialRunClass::Configuration,
        ConfiguredCampaignFailureClass::Infrastructure => PartialRunClass::Infrastructure,
        ConfiguredCampaignFailureClass::Inconclusive => PartialRunClass::Inconclusive,
        ConfiguredCampaignFailureClass::Interrupted => PartialRunClass::Interrupted,
    }
}

fn configured_case_cleanup_error(
    process: Result<(), ConfiguredProcessError>,
    probe: Result<(), BaselineError>,
    quiescence: Result<(), BaselineError>,
) -> Option<Box<dyn std::error::Error + Send + Sync>> {
    if process.is_ok() && probe.is_ok() && quiescence.is_ok() {
        None
    } else {
        Some(Box::new(ConfiguredCaseCleanupError {
            process: process.err(),
            probe: probe.err(),
            quiescence: quiescence.err(),
        }))
    }
}

#[derive(Debug)]
struct ConfiguredCaseCleanupError {
    process: Option<ConfiguredProcessError>,
    probe: Option<BaselineError>,
    quiescence: Option<BaselineError>,
}

impl std::fmt::Display for ConfiguredCaseCleanupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "process recovery: {}; SQL-probe close: {}; quiescence close: {}",
            self.process
                .as_ref()
                .map_or_else(|| "ok".to_owned(), ToString::to_string),
            self.probe
                .as_ref()
                .map_or_else(|| "ok".to_owned(), ToString::to_string),
            self.quiescence
                .as_ref()
                .map_or_else(|| "ok".to_owned(), ToString::to_string),
        )
    }
}

impl std::error::Error for ConfiguredCaseCleanupError {}

#[derive(Serialize)]
struct CaseArtifact {
    schema_version: u16,
    case_id: String,
    seed: u64,
    planned_action_count: usize,
    executed_action_count: usize,
    journal_record_count: usize,
    journal_last_record_hash: Option<String>,
    before_database_oid: u32,
    after_database_oid: u32,
    before_marker_uuid: String,
    after_marker_uuid: String,
    provider_object_count: usize,
    invariants: Vec<InvariantArtifact>,
}

#[derive(Serialize)]
struct InvariantArtifact {
    invariant_id: String,
    checkpoint_id: String,
    verdict: ConfiguredCampaignVerdict,
    witness_count: usize,
}

#[derive(Debug, Error)]
pub enum ConfiguredCampaignError {
    #[error("configured campaign options are invalid: {0}")]
    Options(#[from] ConfiguredCampaignOptionsError),
    #[error("configured campaign configuration failed: {0}")]
    Config(#[from] ConfigError),
    #[error("configured campaign could not be compiled")]
    CampaignCompile(CampaignCompileError),
    #[error("configured SQL probe could not be prepared: {0}")]
    SqlProbe(#[from] ConfiguredSqlProbeError),
    #[error("configured quiescence could not be prepared: {0}")]
    Quiescence(#[from] ConfiguredQuiescenceError),
    #[error("configured invariant suite could not be prepared: {0}")]
    Snapshot(#[from] ConfiguredSnapshotError),
    #[error("configured Compose compatibility preflight failed: {0}")]
    Compose(#[from] DoctorError),
    #[error("configured replay compatibility capture failed: {0}")]
    Compatibility(#[from] CompatibilityCaptureError),
    #[error("configured case preflight failed: {0}")]
    CasePreflight(#[from] ReferenceCaseRunError),
    #[error("configured run artifact failed: {0}")]
    Artifact(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured baseline or case database failed: {0}")]
    Baseline(#[from] BaselineError),
    #[error("configured application process attestation failed: {0}")]
    Process(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured Compose project exclusion failed: {0}")]
    ProjectLock(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("configured case database name is invalid")]
    CaseDatabaseName,
    #[error("configured driver origin is invalid")]
    DriverOrigin,
    #[error("configured fixture sequence is exhausted")]
    FixtureSequenceExhausted,
    #[error("configured fixture request failed: {0}")]
    FixtureRequest(#[source] reqwest::Error),
    #[error("configured fixture state was incoherent")]
    FixtureState,
    #[error("configured case HTTP contract is invalid: {0}")]
    CaseConfig(#[from] ReferenceCaseRunConfigError),
    #[error("configured case execution failed: {0}")]
    CaseRun(ReferenceCaseRunError),
    #[error("configured case {case_id} exceeded its total timeout")]
    CaseTimedOut { case_id: String },
    #[error("configured campaign was interrupted")]
    Interrupted { case_id: Option<String> },
    #[error("configured campaign cleanup failed: {recovery}")]
    RecoveryFailed {
        #[source]
        cause: Option<Box<ConfiguredCampaignError>>,
        recovery: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("configured campaign failed; partial evidence retained at {artifact_path}: {cause}")]
    RunFailed {
        #[source]
        cause: Box<ConfiguredCampaignError>,
        artifact_path: PathBuf,
    },
    #[error("configured campaign partial-evidence finalization failed: {artifact}")]
    PartialFinalization {
        #[source]
        cause: Box<ConfiguredCampaignError>,
        artifact: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("configured invariant execution failed: {0}")]
    Database(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("system clock cannot represent a fixture timestamp")]
    Clock,
}

impl ConfiguredCampaignError {
    #[must_use]
    pub fn failure_class(&self) -> ConfiguredCampaignFailureClass {
        match self {
            Self::Options(_)
            | Self::Config(_)
            | Self::CampaignCompile(_)
            | Self::SqlProbe(_)
            | Self::Quiescence(_)
            | Self::Snapshot(_)
            | Self::CasePreflight(_)
            | Self::CaseDatabaseName
            | Self::DriverOrigin
            | Self::CaseConfig(_) => ConfiguredCampaignFailureClass::Configuration,
            Self::Compose(error) if !error.is_infrastructure_failure() => {
                ConfiguredCampaignFailureClass::Configuration
            }
            Self::Compatibility(error) if !error.is_infrastructure_failure() => {
                ConfiguredCampaignFailureClass::Configuration
            }
            Self::Baseline(error) if !error.is_infrastructure_failure() => {
                ConfiguredCampaignFailureClass::Configuration
            }
            Self::CaseRun(error) if error.is_inconclusive() => {
                ConfiguredCampaignFailureClass::Inconclusive
            }
            Self::CaseTimedOut { .. } => ConfiguredCampaignFailureClass::Inconclusive,
            Self::Interrupted { .. } => ConfiguredCampaignFailureClass::Interrupted,
            Self::RecoveryFailed {
                cause: Some(cause), ..
            }
            | Self::RunFailed { cause, .. }
            | Self::PartialFinalization { cause, .. } => cause.failure_class(),
            Self::Artifact(_)
            | Self::Compose(_)
            | Self::Compatibility(_)
            | Self::Baseline(_)
            | Self::Process(_)
            | Self::ProjectLock(_)
            | Self::FixtureSequenceExhausted
            | Self::FixtureRequest(_)
            | Self::FixtureState
            | Self::CaseRun(_)
            | Self::RecoveryFailed { cause: None, .. }
            | Self::Database(_)
            | Self::Clock => ConfiguredCampaignFailureClass::Infrastructure,
        }
    }

    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.failure_class().exit_code()
    }

    #[must_use]
    pub fn failure_code(&self) -> &'static str {
        match self {
            Self::Options(_) => "invalid_options",
            Self::Config(_) => "invalid_config",
            Self::CampaignCompile(_) => "campaign_compile",
            Self::SqlProbe(_) => "invalid_sql_probe",
            Self::Quiescence(_) => "invalid_quiescence",
            Self::Snapshot(_) => "invalid_snapshot",
            Self::Compose(_) => "compose_preflight",
            Self::Compatibility(_) => "compatibility_capture",
            Self::CasePreflight(_) => "case_preflight",
            Self::Artifact(_) => "artifact_failure",
            Self::Baseline(_) => "baseline_failure",
            Self::Process(_) => "process_failure",
            Self::ProjectLock(_) => "project_locked",
            Self::CaseDatabaseName => "invalid_case_database",
            Self::DriverOrigin => "invalid_driver_origin",
            Self::FixtureSequenceExhausted => "fixture_sequence_exhausted",
            Self::FixtureRequest(_) => "fixture_request",
            Self::FixtureState => "fixture_state",
            Self::CaseConfig(_) => "invalid_case_http_contract",
            Self::CaseRun(_) => "case_execution",
            Self::CaseTimedOut { .. } => "case_timeout",
            Self::Interrupted { .. } => "interrupted",
            Self::RecoveryFailed { .. } => "recovery_failed",
            Self::RunFailed { cause, .. } => cause.failure_code(),
            Self::PartialFinalization { .. } => "partial_finalization_failed",
            Self::Database(_) => "invariant_execution",
            Self::Clock => "system_clock",
        }
    }

    #[must_use]
    pub fn artifact_path(&self) -> Option<&Path> {
        match self {
            Self::RunFailed { artifact_path, .. } => Some(artifact_path),
            _ => None,
        }
    }
}

impl From<ArtifactError> for ConfiguredCampaignError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(Box::new(error))
    }
}

impl From<ConfiguredProcessError> for ConfiguredCampaignError {
    fn from(error: ConfiguredProcessError) -> Self {
        Self::Process(Box::new(error))
    }
}

impl From<ConfiguredDatabaseError> for ConfiguredCampaignError {
    fn from(error: ConfiguredDatabaseError) -> Self {
        Self::Database(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use tiv_core::{
        decision::Seed,
        plan::{
            ActionBudget, CampaignPlanner, CampaignSpec, CaseCount, ProcessCutPoint,
            ProcessFaultSpec, ProviderOutcome, WebhookFaultSpec,
        },
    };

    use super::{
        ConfiguredCampaignError, ConfiguredCampaignFailureClass, ConfiguredCampaignOptions,
        ConfiguredCampaignOptionsError,
    };
    use crate::{
        compatibility::{CompatibilityCaptureError, CompatibilityError},
        doctor::DoctorError,
        postgres::quiescence::QuiescenceError,
        reference_case::{
            ReferenceCaseError, ReferenceCaseRunError, preflight_reference_planned_case,
        },
    };

    #[test]
    fn configured_campaign_options_are_explicit_and_bounded_by_typed_core_values() {
        let options = ConfiguredCampaignOptions::new(Some(99), Some(2), true).unwrap();

        assert_eq!(options.seed(), Some(99));
        assert_eq!(options.cases(), Some(2));
        assert!(options.ci());
        assert!(ConfiguredCampaignOptions::new(None, Some(0), false).is_err());
        assert!(ConfiguredCampaignOptions::new(None, Some(501), false).is_err());
    }

    #[test]
    fn configured_failures_keep_the_non_overlapping_cli_classification() {
        let configuration = ConfiguredCampaignError::Options(ConfiguredCampaignOptionsError);
        let infrastructure =
            ConfiguredCampaignError::Process(Box::new(std::io::Error::other("docker unavailable")));
        let inconclusive = ConfiguredCampaignError::CaseTimedOut {
            case_id: "case_0001".to_owned(),
        };
        let interrupted = ConfiguredCampaignError::Interrupted {
            case_id: Some("case_0001".to_owned()),
        };
        let incompatible = ConfiguredCampaignError::Compatibility(
            CompatibilityCaptureError::Contract(CompatibilityError::InvalidDocument),
        );
        let compose_unavailable = ConfiguredCampaignError::Compose(DoctorError::DockerUnavailable);

        assert_eq!(
            configuration.failure_class(),
            ConfiguredCampaignFailureClass::Configuration
        );
        assert_eq!(infrastructure.exit_code(), 3);
        assert_eq!(inconclusive.exit_code(), 4);
        assert_eq!(interrupted.exit_code(), 130);
        assert_eq!(inconclusive.failure_code(), "case_timeout");
        assert_eq!(
            incompatible.failure_class(),
            ConfiguredCampaignFailureClass::Configuration
        );
        assert_eq!(compose_unavailable.exit_code(), 3);
    }

    #[test]
    fn finalized_partial_evidence_does_not_erase_the_original_failure_class() {
        let error = ConfiguredCampaignError::RunFailed {
            cause: Box::new(ConfiguredCampaignError::CaseTimedOut {
                case_id: "case_0001".to_owned(),
            }),
            artifact_path: PathBuf::from("/tmp/run_partial"),
        };

        assert_eq!(error.exit_code(), 4);
        assert_eq!(error.failure_code(), "case_timeout");
        assert_eq!(error.artifact_path(), Some(Path::new("/tmp/run_partial")));
    }

    #[test]
    fn quiescence_timeout_is_inconclusive_and_never_an_invariant_violation() {
        let error = ConfiguredCampaignError::CaseRun(ReferenceCaseRunError::Lifecycle(
            ReferenceCaseError::DatabaseQuiescence(QuiescenceError::Timeout),
        ));

        assert_eq!(
            error.failure_class(),
            ConfiguredCampaignFailureClass::Inconclusive
        );
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn configured_fault_model_has_a_one_case_seed_supported_by_current_adapters() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let seed = (0..1_024)
            .find(|seed| {
                let spec = CampaignSpec::new_payment_intent_v1(
                    Seed::new(*seed),
                    CaseCount::new(1).unwrap(),
                    ActionBudget::new(40).unwrap(),
                    [
                        ProviderOutcome::Normal,
                        ProviderOutcome::PreExecute429,
                        ProviderOutcome::PreExecute500,
                        ProviderOutcome::PostExecute500,
                        ProviderOutcome::CommitThenClose,
                        ProviderOutcome::CommitThenDelay,
                    ],
                    WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
                    process_faults.clone(),
                )
                .unwrap();
                CampaignPlanner::compile(&spec).is_ok_and(|campaign| {
                    preflight_reference_planned_case(campaign.cases()[0].plan(), true, true).is_ok()
                })
            })
            .expect("the bounded corpus contains an adapter-supported one-case campaign");

        assert_eq!(seed, 4);
    }

    #[test]
    fn configured_fault_model_reaches_the_webhook_request_forwarded_handshake() {
        let process_faults =
            ProcessFaultSpec::new([ProcessCutPoint::WebhookRequestForwarded], 1).unwrap();
        let seed = (0..4_096)
            .find(|seed| {
                let spec = CampaignSpec::new_payment_intent_v1(
                    Seed::new(*seed),
                    CaseCount::new(1).unwrap(),
                    ActionBudget::new(40).unwrap(),
                    [ProviderOutcome::Normal],
                    WebhookFaultSpec::new(1, [], false, false).unwrap(),
                    process_faults.clone(),
                )
                .unwrap();
                CampaignPlanner::compile(&spec).is_ok_and(|campaign| {
                    campaign.cases()[0]
                        .plan()
                        .actions()
                        .windows(2)
                        .any(|actions| {
                            matches!(
                                actions[0].kind(),
                                tiv_core::plan::PlanActionKind::DeliverWebhook
                                    | tiv_core::plan::PlanActionKind::DuplicateWebhook
                            ) && matches!(
                                actions[1].kind(),
                                tiv_core::plan::PlanActionKind::KillApplication {
                                    cut_point: ProcessCutPoint::WebhookRequestForwarded
                                }
                            )
                        })
                })
            })
            .expect("the bounded corpus reaches the instrumented ingress handshake");

        assert_eq!(seed, 2);
    }

    #[test]
    fn full_configured_fault_model_has_a_deterministic_webhook_request_cut_seed() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let seed = (0..4_096)
            .find(|seed| {
                let spec = CampaignSpec::new_payment_intent_v1(
                    Seed::new(*seed),
                    CaseCount::new(1).unwrap(),
                    ActionBudget::new(40).unwrap(),
                    [
                        ProviderOutcome::Normal,
                        ProviderOutcome::PreExecute429,
                        ProviderOutcome::PreExecute500,
                        ProviderOutcome::PostExecute500,
                        ProviderOutcome::CommitThenClose,
                        ProviderOutcome::CommitThenDelay,
                    ],
                    WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
                    process_faults.clone(),
                )
                .unwrap();
                CampaignPlanner::compile(&spec).is_ok_and(|campaign| {
                    campaign.cases()[0]
                        .plan()
                        .actions()
                        .windows(2)
                        .any(|actions| {
                            matches!(
                                actions[0].kind(),
                                tiv_core::plan::PlanActionKind::DeliverWebhook
                                    | tiv_core::plan::PlanActionKind::DuplicateWebhook
                            ) && matches!(
                                actions[1].kind(),
                                tiv_core::plan::PlanActionKind::KillApplication {
                                    cut_point: ProcessCutPoint::WebhookRequestForwarded
                                }
                            )
                        })
                })
            })
            .expect("the bounded full-config corpus reaches the instrumented ingress handshake");

        assert_eq!(seed, 67);
    }

    #[test]
    fn full_configured_fault_model_has_a_supported_two_case_seed() {
        let process_faults = ProcessFaultSpec::new(
            [
                ProcessCutPoint::ClientRequestForwarded,
                ProcessCutPoint::ClientResponseObserved,
                ProcessCutPoint::WebhookRequestForwarded,
                ProcessCutPoint::WebhookResponseObserved,
                ProcessCutPoint::SqlProbe,
            ],
            1,
        )
        .unwrap();
        let seed = (0..4_096)
            .find(|seed| {
                let spec = CampaignSpec::new_payment_intent_v1(
                    Seed::new(*seed),
                    CaseCount::new(2).unwrap(),
                    ActionBudget::new(40).unwrap(),
                    [
                        ProviderOutcome::Normal,
                        ProviderOutcome::PreExecute429,
                        ProviderOutcome::PreExecute500,
                        ProviderOutcome::PostExecute500,
                        ProviderOutcome::CommitThenClose,
                        ProviderOutcome::CommitThenDelay,
                    ],
                    WebhookFaultSpec::new(3, [0, 10, 100, 1_000, 5_000], true, true).unwrap(),
                    process_faults.clone(),
                )
                .unwrap();
                CampaignPlanner::compile(&spec).is_ok_and(|campaign| {
                    campaign.cases().iter().all(|case| {
                        preflight_reference_planned_case(case.plan(), true, true).is_ok()
                            && case.plan().actions().windows(2).all(|actions| {
                                if !matches!(
                                    actions[1].kind(),
                                    tiv_core::plan::PlanActionKind::KillApplication {
                                        cut_point: ProcessCutPoint::SqlProbe
                                    }
                                ) {
                                    return true;
                                }
                                matches!(
                                    actions[0].kind(),
                                    tiv_core::plan::PlanActionKind::DriveCheckout {
                                        provider_script
                                    } if provider_script.terminal_outcome() == ProviderOutcome::Normal
                                )
                            })
                    })
                })
            })
            .expect("the bounded full-config corpus contains two supported serial cases");

        assert_eq!(seed, 48);
    }
}
