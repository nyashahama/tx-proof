//! Deterministic, allowlisted human and CI reports for finalized artifacts.

use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use quick_xml::{
    Writer,
    events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event},
};

use crate::artifacts::{ArtifactError, RunArtifactStaging};

const SUMMARY_MARKDOWN_FILE: &str = "summary.md";
const JUNIT_FILE: &str = "junit.xml";
const REPLAY_FILE: &str = "replay.txt";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReportArtifactKind {
    Campaign,
    Replay,
    Shrink,
    MinimizedReplay,
}

impl ReportArtifactKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Campaign => "configured_campaign",
            Self::Replay => "configured_replay",
            Self::Shrink => "configured_shrink",
            Self::MinimizedReplay => "configured_minimized_replay",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReportConclusion {
    Held,
    Counterexample,
    BudgetExhausted,
    Inconclusive,
    ConfigurationFailure,
    InfrastructureFailure,
    Interrupted,
}

impl ReportConclusion {
    const fn label(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Counterexample => "counterexample",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Inconclusive => "inconclusive",
            Self::ConfigurationFailure => "configuration_failure",
            Self::InfrastructureFailure => "infrastructure_failure",
            Self::Interrupted => "interrupted",
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Held => "held-invariants",
            Self::Counterexample => "counterexample",
            Self::BudgetExhausted => "counterexample retained; shrink budget exhausted",
            Self::Inconclusive => "inconclusive",
            Self::ConfigurationFailure => "configuration failure",
            Self::InfrastructureFailure => "infrastructure failure",
            Self::Interrupted => "interrupted execution",
        }
    }

    const fn exit_code(self) -> u8 {
        match self {
            Self::Held => 0,
            Self::Counterexample => 10,
            Self::BudgetExhausted => 11,
            Self::Inconclusive => 4,
            Self::ConfigurationFailure => 2,
            Self::InfrastructureFailure => 3,
            Self::Interrupted => 130,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PartialReportClass {
    Configuration,
    Infrastructure,
    Inconclusive,
    Interrupted,
}

impl PartialReportClass {
    const fn conclusion(self) -> ReportConclusion {
        match self {
            Self::Configuration => ReportConclusion::ConfigurationFailure,
            Self::Infrastructure => ReportConclusion::InfrastructureFailure,
            Self::Inconclusive => ReportConclusion::Inconclusive,
            Self::Interrupted => ReportConclusion::Interrupted,
        }
    }

    const fn check_outcome(self) -> ReportCheckOutcome {
        match self {
            Self::Inconclusive => ReportCheckOutcome::Skipped,
            Self::Configuration | Self::Infrastructure | Self::Interrupted => {
                ReportCheckOutcome::Error
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReportCheckOutcome {
    Passed,
    Failed,
    Error,
    Skipped,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReportCheck {
    name: String,
    outcome: ReportCheckOutcome,
    message: String,
}

impl ReportCheck {
    pub(crate) fn new(
        name: impl Into<String>,
        outcome: ReportCheckOutcome,
        message: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            outcome,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReportFailure {
    invariant_id: String,
    checkpoint_id: String,
    witness_count: Option<usize>,
}

impl ReportFailure {
    pub(crate) fn new(
        invariant_id: impl Into<String>,
        checkpoint_id: impl Into<String>,
        witness_count: Option<usize>,
    ) -> Self {
        Self {
            invariant_id: invariant_id.into(),
            checkpoint_id: checkpoint_id.into(),
            witness_count,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReportFact {
    label: &'static str,
    value: String,
}

impl ReportFact {
    pub(crate) fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplayCommand {
    purpose: &'static str,
    arguments: Vec<ReplayArgument>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReplayArgument {
    Literal(&'static str),
    Path(PathBuf),
    Value(String),
}

impl ReplayCommand {
    pub(crate) fn configured(
        artifact: impl Into<PathBuf>,
        config: impl Into<PathBuf>,
        case: u32,
    ) -> Self {
        let (artifact, config) = command_paths(artifact.into(), config.into());
        Self {
            purpose: "Replay the recorded campaign case on three fresh baselines.",
            arguments: vec![
                ReplayArgument::Literal("tiv"),
                ReplayArgument::Literal("replay"),
                ReplayArgument::Literal("configured"),
                ReplayArgument::Literal("--artifact"),
                ReplayArgument::Path(artifact),
                ReplayArgument::Literal("--config"),
                ReplayArgument::Path(config),
                ReplayArgument::Literal("--case"),
                ReplayArgument::Value(case.to_string()),
            ],
        }
    }

    pub(crate) fn minimized(artifact: impl Into<PathBuf>, config: impl Into<PathBuf>) -> Self {
        let (artifact, config) = command_paths(artifact.into(), config.into());
        Self {
            purpose: "Replay the authority-bound minimized trace on three fresh baselines.",
            arguments: vec![
                ReplayArgument::Literal("tiv"),
                ReplayArgument::Literal("replay"),
                ReplayArgument::Literal("minimized"),
                ReplayArgument::Literal("--artifact"),
                ReplayArgument::Path(artifact),
                ReplayArgument::Literal("--config"),
                ReplayArgument::Path(config),
            ],
        }
    }

    pub(crate) fn shrink(
        artifact: impl Into<PathBuf>,
        config: impl Into<PathBuf>,
        max_candidates: u8,
        max_time_milliseconds: u64,
    ) -> Self {
        let (artifact, config) = command_paths(artifact.into(), config.into());
        Self {
            purpose: "Repeat the bounded same-failure shrink from the verified replay artifact.",
            arguments: vec![
                ReplayArgument::Literal("tiv"),
                ReplayArgument::Literal("shrink"),
                ReplayArgument::Literal("configured"),
                ReplayArgument::Literal("--artifact"),
                ReplayArgument::Path(artifact),
                ReplayArgument::Literal("--config"),
                ReplayArgument::Path(config),
                ReplayArgument::Literal("--max-candidates"),
                ReplayArgument::Value(max_candidates.to_string()),
                ReplayArgument::Literal("--max-time"),
                ReplayArgument::Value(format!("{max_time_milliseconds}ms")),
            ],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactReport {
    run_id: String,
    kind: ReportArtifactKind,
    conclusion: ReportConclusion,
    checks: Vec<ReportCheck>,
    failures: Vec<ReportFailure>,
    facts: Vec<ReportFact>,
    replay_commands: Vec<ReplayCommand>,
    budget: String,
    replay_stability: String,
}

impl ArtifactReport {
    pub(crate) fn new(
        run_id: impl Into<String>,
        kind: ReportArtifactKind,
        conclusion: ReportConclusion,
        budget: impl Into<String>,
        replay_stability: impl Into<String>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            kind,
            conclusion,
            checks: Vec::new(),
            failures: Vec::new(),
            facts: Vec::new(),
            replay_commands: Vec::new(),
            budget: budget.into(),
            replay_stability: replay_stability.into(),
        }
    }

    pub(crate) fn add_check(&mut self, check: ReportCheck) {
        self.checks.push(check);
    }

    pub(crate) fn add_failure(&mut self, failure: ReportFailure) {
        self.failures.push(failure);
    }

    pub(crate) fn add_fact(&mut self, fact: ReportFact) {
        self.facts.push(fact);
    }

    pub(crate) fn add_replay_command(&mut self, command: ReplayCommand) {
        self.replay_commands.push(command);
    }

    #[cfg(test)]
    pub(crate) const fn conclusion(&self) -> ReportConclusion {
        self.conclusion
    }

    #[cfg(test)]
    pub(crate) fn check_outcomes(&self) -> Vec<ReportCheckOutcome> {
        self.checks.iter().map(|check| check.outcome).collect()
    }

    #[cfg(test)]
    pub(crate) const fn replay_command_count(&self) -> usize {
        self.replay_commands.len()
    }
}

pub(crate) fn write_report_bundle(
    artifacts: &mut RunArtifactStaging,
    report: &ArtifactReport,
) -> Result<(), ArtifactError> {
    let markdown = render_markdown(report)?;
    let junit = render_junit(report)?;
    let replay = render_replay(report)?;
    artifacts.write_bytes(SUMMARY_MARKDOWN_FILE, markdown.as_bytes())?;
    artifacts.write_bytes(JUNIT_FILE, &junit)?;
    artifacts.write_bytes(REPLAY_FILE, replay.as_bytes())
}

pub(crate) fn partial_artifact_report(
    run_id: impl Into<String>,
    kind: ReportArtifactKind,
    class: PartialReportClass,
    failure_code: &'static str,
) -> ArtifactReport {
    let mut report = ArtifactReport::new(
        run_id,
        kind,
        class.conclusion(),
        "Execution ended before a complete product conclusion was reached.",
        "Unavailable because this artifact records a partial execution.",
    );
    report.add_check(ReportCheck::new(
        "partial execution",
        class.check_outcome(),
        format!("execution ended with allowlisted failure code {failure_code}"),
    ));
    report.add_fact(ReportFact::new("Failure code", failure_code));
    report
}

pub(crate) fn validate_replay_command_paths(
    artifact: &Path,
    config: &Path,
) -> Result<(), ArtifactError> {
    let (artifact, config) = command_paths(artifact.to_owned(), config.to_owned());
    shell_quote_path(&artifact)?;
    shell_quote_path(&config)?;
    Ok(())
}

fn render_markdown(report: &ArtifactReport) -> Result<String, ArtifactError> {
    validate_report_text(report)?;
    let mut output = String::new();
    writeln!(output, "# TxProof {} report", report.conclusion.title())
        .expect("writing to a String is infallible");
    writeln!(output).expect("writing to a String is infallible");
    writeln!(output, "- Run: {}", markdown_code(&report.run_id))
        .expect("writing to a String is infallible");
    writeln!(output, "- Artifact: {}", markdown_code(report.kind.label()))
        .expect("writing to a String is infallible");
    writeln!(
        output,
        "- Result: {}",
        markdown_code(report.conclusion.label())
    )
    .expect("writing to a String is infallible");
    writeln!(
        output,
        "- Exit code: {}",
        markdown_code(&report.conclusion.exit_code().to_string())
    )
    .expect("writing to a String is infallible");

    if !report.failures.is_empty() {
        writeln!(output, "\n## Failure identity").expect("writing to a String is infallible");
        for failure in &report.failures {
            write!(
                output,
                "\n- {} at {}",
                markdown_code(&failure.invariant_id),
                markdown_code(&failure.checkpoint_id)
            )
            .expect("writing to a String is infallible");
            if let Some(witness_count) = failure.witness_count {
                write!(
                    output,
                    "; bounded witness rows: {}",
                    markdown_code(&witness_count.to_string())
                )
                .expect("writing to a String is infallible");
            }
        }
        writeln!(output).expect("writing to a String is infallible");
    }

    if !report.facts.is_empty() {
        writeln!(output, "\n## Evidence summary").expect("writing to a String is infallible");
        for fact in &report.facts {
            writeln!(output, "\n- {}: {}", fact.label, markdown_code(&fact.value))
                .expect("writing to a String is infallible");
        }
    }

    writeln!(output, "\n## Replay").expect("writing to a String is infallible");
    if report.replay_commands.is_empty() {
        writeln!(
            output,
            "\nNo executable counterexample replay command is available for this result."
        )
        .expect("writing to a String is infallible");
    } else {
        writeln!(
            output,
            "\nRun from the repository directory containing the recorded configuration, with the same disposable stack credentials available locally:"
        )
        .expect("writing to a String is infallible");
        for command in &report.replay_commands {
            writeln!(output, "\n{}", command.purpose).expect("writing to a String is infallible");
            writeln!(output, "\n    {}", render_command(command)?)
                .expect("writing to a String is infallible");
        }
    }

    writeln!(output, "\n## Scope and limits").expect("writing to a String is infallible");
    writeln!(
        output,
        "\n- Model: seeded, state-valid external schedules; the compiled trace—not the seed—is replay authority."
    )
    .expect("writing to a String is infallible");
    writeln!(output, "- Budget: {}", report.budget).expect("writing to a String is infallible");
    writeln!(output, "- Replay stability: {}", report.replay_stability)
        .expect("writing to a String is infallible");
    writeln!(
        output,
        "- Exclusions: customer thread scheduling, PostgreSQL internals, kernel timing, wall-clock calls, and entropy remain uncontrolled."
    )
    .expect("writing to a String is infallible");
    writeln!(
        output,
        "- Claim boundary: bounded counterexample search—not proof of correctness or a global minimum."
    )
    .expect("writing to a String is infallible");
    Ok(output)
}

fn render_replay(report: &ArtifactReport) -> Result<String, ArtifactError> {
    validate_report_text(report)?;
    let mut output = String::new();
    writeln!(output, "TxProof replay instructions").expect("writing to a String is infallible");
    writeln!(output, "Run: {}", report.run_id).expect("writing to a String is infallible");
    writeln!(output, "Artifact: {}", report.kind.label())
        .expect("writing to a String is infallible");
    writeln!(output, "Result: {}", report.conclusion.label())
        .expect("writing to a String is infallible");
    writeln!(
        output,
        "Recorded exit code: {}",
        report.conclusion.exit_code()
    )
    .expect("writing to a String is infallible");
    writeln!(output).expect("writing to a String is infallible");
    writeln!(
        output,
        "Run from the repository directory containing the recorded configuration."
    )
    .expect("writing to a String is infallible");
    writeln!(
        output,
        "Each command re-verifies the complete source artifact and exact compatibility before mutation."
    )
    .expect("writing to a String is infallible");
    writeln!(
        output,
        "Replay executes the compiled trace directly; it does not rerun campaign randomness."
    )
    .expect("writing to a String is infallible");
    writeln!(
        output,
        "A replay exits 10 when the same failure is stable/reproducible and 4 when inconclusive."
    )
    .expect("writing to a String is infallible");

    if report.replay_commands.is_empty() {
        writeln!(
            output,
            "\nNo executable counterexample replay command is available for this result."
        )
        .expect("writing to a String is infallible");
    } else {
        for command in &report.replay_commands {
            writeln!(output, "\n{}", command.purpose).expect("writing to a String is infallible");
            writeln!(output, "{}", render_command(command)?)
                .expect("writing to a String is infallible");
        }
    }
    Ok(output)
}

fn render_junit(report: &ArtifactReport) -> Result<Vec<u8>, ArtifactError> {
    validate_report_text(report)?;
    let failure_count = report
        .checks
        .iter()
        .filter(|check| check.outcome == ReportCheckOutcome::Failed)
        .count();
    let skipped_count = report
        .checks
        .iter()
        .filter(|check| check.outcome == ReportCheckOutcome::Skipped)
        .count();
    let error_count = report
        .checks
        .iter()
        .filter(|check| check.outcome == ReportCheckOutcome::Error)
        .count();
    let tests = report.checks.len().to_string();
    let failures = failure_count.to_string();
    let skipped = skipped_count.to_string();
    let errors = error_count.to_string();
    let suite_name = format!("TxProof {}", report.kind.label());
    let mut writer = Writer::new_with_indent(Vec::new(), b' ', 2);
    write_xml_event(
        &mut writer,
        Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)),
    )?;

    let mut suite = BytesStart::new("testsuite");
    suite.push_attribute(("name", suite_name.as_str()));
    suite.push_attribute(("tests", tests.as_str()));
    suite.push_attribute(("failures", failures.as_str()));
    suite.push_attribute(("errors", errors.as_str()));
    suite.push_attribute(("skipped", skipped.as_str()));
    write_xml_event(&mut writer, Event::Start(suite))?;
    write_xml_event(&mut writer, Event::Start(BytesStart::new("properties")))?;
    write_property(&mut writer, "txproof.run_id", &report.run_id)?;
    write_property(&mut writer, "txproof.artifact", report.kind.label())?;
    write_property(&mut writer, "txproof.result", report.conclusion.label())?;
    write_property(
        &mut writer,
        "txproof.exit_code",
        &report.conclusion.exit_code().to_string(),
    )?;
    write_xml_event(&mut writer, Event::End(BytesEnd::new("properties")))?;

    for check in &report.checks {
        let mut testcase = BytesStart::new("testcase");
        testcase.push_attribute(("classname", "txproof.transactional_invariants"));
        testcase.push_attribute(("name", check.name.as_str()));
        write_xml_event(&mut writer, Event::Start(testcase))?;
        match check.outcome {
            ReportCheckOutcome::Passed => {}
            ReportCheckOutcome::Failed => {
                let mut failure = BytesStart::new("failure");
                failure.push_attribute(("type", "txproof.counterexample"));
                failure.push_attribute(("message", check.message.as_str()));
                write_xml_event(&mut writer, Event::Start(failure))?;
                write_xml_event(&mut writer, Event::Text(BytesText::new(&check.message)))?;
                write_xml_event(&mut writer, Event::End(BytesEnd::new("failure")))?;
            }
            ReportCheckOutcome::Error => {
                let mut error = BytesStart::new("error");
                error.push_attribute(("type", "txproof.execution"));
                error.push_attribute(("message", check.message.as_str()));
                write_xml_event(&mut writer, Event::Start(error))?;
                write_xml_event(&mut writer, Event::Text(BytesText::new(&check.message)))?;
                write_xml_event(&mut writer, Event::End(BytesEnd::new("error")))?;
            }
            ReportCheckOutcome::Skipped => {
                let mut skipped = BytesStart::new("skipped");
                skipped.push_attribute(("message", check.message.as_str()));
                write_xml_event(&mut writer, Event::Empty(skipped))?;
            }
        }
        write_xml_event(&mut writer, Event::End(BytesEnd::new("testcase")))?;
    }
    write_xml_event(&mut writer, Event::End(BytesEnd::new("testsuite")))?;
    let mut bytes = writer.into_inner();
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_property(
    writer: &mut Writer<Vec<u8>>,
    name: &str,
    value: &str,
) -> Result<(), ArtifactError> {
    let mut property = BytesStart::new("property");
    property.push_attribute(("name", name));
    property.push_attribute(("value", value));
    write_xml_event(writer, Event::Empty(property))?;
    Ok(())
}

fn write_xml_event(writer: &mut Writer<Vec<u8>>, event: Event<'_>) -> Result<(), ArtifactError> {
    writer.write_event(event).map_err(ArtifactError::ReportXml)
}

fn render_command(command: &ReplayCommand) -> Result<String, ArtifactError> {
    command
        .arguments
        .iter()
        .map(|argument| match argument {
            ReplayArgument::Literal(value) => Ok((*value).to_owned()),
            ReplayArgument::Path(path) => shell_quote_path(path),
            ReplayArgument::Value(value) => Ok(shell_quote(value)),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|arguments| arguments.join(" "))
}

fn validate_report_text(report: &ArtifactReport) -> Result<(), ArtifactError> {
    let fixed = [
        report.run_id.as_str(),
        report.budget.as_str(),
        report.replay_stability.as_str(),
    ];
    let checks = report
        .checks
        .iter()
        .flat_map(|check| [check.name.as_str(), check.message.as_str()]);
    let failures = report.failures.iter().flat_map(|failure| {
        [
            failure.invariant_id.as_str(),
            failure.checkpoint_id.as_str(),
        ]
    });
    let facts = report
        .facts
        .iter()
        .flat_map(|fact| [fact.label, fact.value.as_str()]);
    let commands = report.replay_commands.iter().flat_map(|command| {
        std::iter::once(command.purpose).chain(command.arguments.iter().filter_map(|argument| {
            match argument {
                ReplayArgument::Literal(value) => Some(*value),
                ReplayArgument::Value(value) => Some(value.as_str()),
                ReplayArgument::Path(_) => None,
            }
        }))
    });
    if fixed
        .into_iter()
        .chain(checks)
        .chain(failures)
        .chain(facts)
        .chain(commands)
        .all(valid_report_value)
    {
        Ok(())
    } else {
        Err(ArtifactError::InvalidReport)
    }
}

fn valid_report_value(value: &str) -> bool {
    value
        .chars()
        .all(|character| !character.is_control() && !matches!(character, '\u{fffe}' | '\u{ffff}'))
}

fn markdown_code(value: &str) -> String {
    let longest_run = value
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat(longest_run.saturating_add(1));
    if value.contains('`')
        || value.starts_with(char::is_whitespace)
        || value.ends_with(char::is_whitespace)
    {
        format!("{fence} {value} {fence}")
    } else {
        format!("{fence}{value}{fence}")
    }
}

fn command_paths(artifact: PathBuf, config: PathBuf) -> (PathBuf, PathBuf) {
    let Some(root) = config.parent() else {
        return (artifact, config);
    };
    let artifact_argument = artifact
        .strip_prefix(root)
        .map_or_else(|_| artifact.clone(), Path::to_owned);
    let config_argument = config
        .strip_prefix(root)
        .map_or_else(|_| config.clone(), Path::to_owned);
    (artifact_argument, config_argument)
}

fn shell_quote_path(path: &Path) -> Result<String, ArtifactError> {
    let value = path
        .to_str()
        .ok_or_else(|| ArtifactError::ReportPath(path.to_owned()))?;
    if value.chars().any(char::is_control) {
        return Err(ArtifactError::ReportPath(path.to_owned()));
    }
    Ok(shell_quote(value))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use quick_xml::{Reader, events::Event};
    use tiv_core::trace::{CASE_TRACE_SCHEMA_VERSION, TRACE_SCHEMA_VERSION};
    use uuid::Uuid;

    use super::{
        ArtifactReport, PartialReportClass, ReplayCommand, ReportArtifactKind, ReportCheck,
        ReportCheckOutcome, ReportConclusion, ReportFact, ReportFailure, partial_artifact_report,
        render_junit, render_markdown, render_replay, write_report_bundle,
    };
    use crate::artifacts::{
        ArtifactAuthority, ArtifactKind, ArtifactResult, ManifestSeed, PartialRunClass,
        RepositoryProvenance, RunArtifactStaging, WorktreeState, verify_complete_run_artifact,
    };

    #[test]
    #[allow(clippy::too_many_lines)]
    fn complete_report_bundle_is_human_readable_ci_valid_and_replay_exact() {
        let root = std::env::temp_dir().join(format!("tiv-report-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let base = root.join("customer's evidence");
        let config = root.join("config with spaces.toml");
        let mut staging = RunArtifactStaging::create_v2(
            &root,
            &base,
            "run_report",
            ManifestSeed::new(
                ArtifactKind::Campaign,
                RepositoryProvenance::captured(
                    &"a".repeat(40),
                    WorktreeState::Clean,
                    &"b".repeat(64),
                )
                .unwrap(),
                Vec::new(),
            ),
        )
        .unwrap();
        let source = staging.planned_final_path().to_owned();
        staging
            .write_json(
                "config.redacted.json",
                &serde_json::json!({"schema_version": 1}),
            )
            .unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        staging
            .write_json(
                "campaign-plan.json",
                &serde_json::json!({"schema_version": 1}),
            )
            .unwrap();
        staging
            .write_json(
                "cases/case_0001/trace.json",
                &serde_json::json!({"schema_version": CASE_TRACE_SCHEMA_VERSION}),
            )
            .unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"schema_version": 1}))
            .unwrap();

        let mut report = ArtifactReport::new(
            "run_report",
            ReportArtifactKind::Campaign,
            ReportConclusion::Counterexample,
            "Exactly 3 fresh-baseline attempts.",
            "Stable: the same failure identity reproduced 3/3 times.",
        );
        report.add_failure(ReportFailure::new(
            "provider_object_uniqueness",
            "checkout_complete",
            Some(2),
        ));
        report.add_fact(ReportFact::new("Case", "case_0001"));
        report.add_check(ReportCheck::new(
            "provider_object_uniqueness at checkout_complete",
            ReportCheckOutcome::Failed,
            "counterexample reproduced 3/3 times & remained <bounded>",
        ));
        report.add_replay_command(ReplayCommand::configured(&source, &config, 1));

        write_report_bundle(&mut staging, &report).unwrap();
        let finalized = staging
            .finalize_complete(
                ArtifactResult::Counterexample,
                vec![
                    ArtifactAuthority::campaign_plan(),
                    ArtifactAuthority::campaign_case_trace("case_0001").unwrap(),
                ],
            )
            .unwrap();
        let verified = verify_complete_run_artifact(&finalized).unwrap();

        let markdown = fs::read_to_string(finalized.join("summary.md")).unwrap();
        assert!(markdown.contains("# TxProof counterexample report"));
        assert!(markdown.contains("provider_object_uniqueness"));
        assert!(markdown.contains("counterexample search—not proof"));
        assert!(!markdown.contains("sk_test_secret_canary"));

        let replay = fs::read_to_string(finalized.join("replay.txt")).unwrap();
        assert!(replay.contains("--artifact 'customer"));
        assert!(replay.contains("customer'\"'\"'s evidence/run_report'"));
        assert!(replay.contains("--config 'config with spaces.toml' --case '1'"));
        assert!(!replay.contains(root.to_str().unwrap()));

        let junit = fs::read_to_string(finalized.join("junit.xml")).unwrap();
        assert!(junit.contains("failures=\"1\""));
        assert!(junit.contains("&amp; remained &lt;bounded&gt;"));
        let mut reader = Reader::from_str(&junit);
        loop {
            if reader.read_event().unwrap() == Event::Eof {
                break;
            }
        }

        for relative in ["summary.md", "junit.xml", "replay.txt"] {
            assert_eq!(
                verified.read_indexed_bytes(Path::new(relative)).unwrap(),
                fs::read(finalized.join(relative)).unwrap()
            );
        }

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn junit_and_markdown_share_the_exact_result_and_exit_contract() {
        let fixtures = [
            (
                ReportArtifactKind::Campaign,
                ReportConclusion::Held,
                ReportCheckOutcome::Passed,
                "failures=\"0\"",
                "errors=\"0\"",
                "skipped=\"0\"",
                "txproof.exit_code\" value=\"0\"",
            ),
            (
                ReportArtifactKind::Replay,
                ReportConclusion::Counterexample,
                ReportCheckOutcome::Failed,
                "failures=\"1\"",
                "errors=\"0\"",
                "skipped=\"0\"",
                "txproof.exit_code\" value=\"10\"",
            ),
            (
                ReportArtifactKind::Shrink,
                ReportConclusion::BudgetExhausted,
                ReportCheckOutcome::Failed,
                "failures=\"1\"",
                "errors=\"0\"",
                "skipped=\"0\"",
                "txproof.exit_code\" value=\"11\"",
            ),
            (
                ReportArtifactKind::MinimizedReplay,
                ReportConclusion::Inconclusive,
                ReportCheckOutcome::Skipped,
                "failures=\"0\"",
                "errors=\"0\"",
                "skipped=\"1\"",
                "txproof.exit_code\" value=\"4\"",
            ),
        ];

        for (kind, conclusion, outcome, failures, errors, skipped, exit) in fixtures {
            let mut report = ArtifactReport::new(
                "run_contract",
                kind,
                conclusion,
                "bounded fixture",
                "fixture stability",
            );
            report.add_check(ReportCheck::new(
                "invariant contract",
                outcome,
                "fixture result",
            ));
            let junit = String::from_utf8(render_junit(&report).unwrap()).unwrap();
            let markdown = render_markdown(&report).unwrap();
            assert!(junit.contains(failures));
            assert!(junit.contains(errors));
            assert!(junit.contains(skipped));
            assert!(junit.contains(exit));
            assert!(markdown.contains(&format!("- Exit code: `{}`", conclusion.exit_code())));
            assert!(markdown.contains(&format!("- Result: `{}`", conclusion.label())));
        }
    }

    #[test]
    fn partial_report_contract_maps_every_exit_and_seals_the_bundle() {
        let fixtures = [
            (
                PartialReportClass::Configuration,
                "configuration_failure",
                2,
                "errors=\"1\"",
                "skipped=\"0\"",
            ),
            (
                PartialReportClass::Infrastructure,
                "infrastructure_failure",
                3,
                "errors=\"1\"",
                "skipped=\"0\"",
            ),
            (
                PartialReportClass::Inconclusive,
                "inconclusive",
                4,
                "errors=\"0\"",
                "skipped=\"1\"",
            ),
            (
                PartialReportClass::Interrupted,
                "interrupted",
                130,
                "errors=\"1\"",
                "skipped=\"0\"",
            ),
        ];

        for (class, result, exit_code, errors, skipped) in fixtures {
            let report = partial_artifact_report(
                "run_partial_contract",
                ReportArtifactKind::Campaign,
                class,
                "fixture_failure",
            );
            let junit = String::from_utf8(render_junit(&report).unwrap()).unwrap();
            let markdown = render_markdown(&report).unwrap();
            let replay = render_replay(&report).unwrap();
            assert!(junit.contains(errors));
            assert!(junit.contains(skipped));
            assert!(junit.contains(&format!("txproof.exit_code\" value=\"{exit_code}\"")));
            assert!(markdown.contains(&format!("- Result: `{result}`")));
            assert!(markdown.contains("Failure code: `fixture_failure`"));
            assert!(replay.contains("No executable counterexample replay command"));
        }

        let root = std::env::temp_dir().join(format!("tiv-report-partial-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let repository =
            RepositoryProvenance::captured(&"a".repeat(40), WorktreeState::Clean, &"b".repeat(64))
                .unwrap();
        let mut staging = RunArtifactStaging::create_v2(
            &root,
            &base,
            "run_partial_report",
            ManifestSeed::new(ArtifactKind::Campaign, repository, Vec::new()),
        )
        .unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "failed"}))
            .unwrap();
        let report = partial_artifact_report(
            "run_partial_report",
            ReportArtifactKind::Campaign,
            PartialReportClass::Infrastructure,
            "fixture_failure",
        );
        write_report_bundle(&mut staging, &report).unwrap();
        let finalized = staging
            .finalize_partial_v2(
                PartialRunClass::Infrastructure,
                "fixture_failure",
                Vec::new(),
            )
            .unwrap();
        let checksums = fs::read_to_string(finalized.join("checksums.txt")).unwrap();
        for relative in ["summary.md", "junit.xml", "replay.txt"] {
            assert!(checksums.contains(&format!("  {relative}\n")));
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(finalized.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["complete"], false);
        assert_eq!(manifest["failure_class"], "infrastructure");
        assert_eq!(manifest["failure_code"], "fixture_failure");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_instructions_reject_control_characters_in_paths() {
        let mut report = ArtifactReport::new(
            "run_control_path",
            ReportArtifactKind::Replay,
            ReportConclusion::Counterexample,
            "Exactly 3 attempts.",
            "Stable 3/3.",
        );
        report.add_check(ReportCheck::new(
            "invariant",
            ReportCheckOutcome::Failed,
            "counterexample",
        ));
        report.add_replay_command(ReplayCommand::configured(
            Path::new("/tmp/artifact\nspoof"),
            Path::new("/tmp/tiv.toml"),
            1,
        ));

        assert!(matches!(
            super::validate_replay_command_paths(
                Path::new("/tmp/artifact\nspoof"),
                Path::new("/tmp/tiv.toml")
            ),
            Err(crate::artifacts::ArtifactError::ReportPath(_))
        ));
        assert!(matches!(
            render_markdown(&report),
            Err(crate::artifacts::ArtifactError::ReportPath(_))
        ));
    }

    #[test]
    fn markdown_escapes_backticks_and_all_renderers_reject_control_text() {
        let mut escaped = ArtifactReport::new(
            "run_markdown",
            ReportArtifactKind::Campaign,
            ReportConclusion::Counterexample,
            "one case",
            "not classified",
        );
        escaped.add_failure(ReportFailure::new(
            "provider`object",
            "checkout``complete",
            None,
        ));
        escaped.add_check(ReportCheck::new(
            "provider`object at checkout``complete",
            ReportCheckOutcome::Failed,
            "counterexample",
        ));
        let markdown = render_markdown(&escaped).unwrap();
        assert!(markdown.contains("`` provider`object ``"));
        assert!(markdown.contains("``` checkout``complete ```"));

        let mut invalid = ArtifactReport::new(
            "run_invalid_text",
            ReportArtifactKind::Campaign,
            ReportConclusion::Counterexample,
            "one case",
            "not classified",
        );
        invalid.add_failure(ReportFailure::new(
            "provider_object\nspoof",
            "checkout_complete",
            None,
        ));
        invalid.add_check(ReportCheck::new(
            "provider_object\nspoof",
            ReportCheckOutcome::Failed,
            "counterexample",
        ));

        assert!(matches!(
            render_markdown(&invalid),
            Err(crate::artifacts::ArtifactError::InvalidReport)
        ));
        assert!(matches!(
            render_junit(&invalid),
            Err(crate::artifacts::ArtifactError::InvalidReport)
        ));
        assert!(matches!(
            render_replay(&invalid),
            Err(crate::artifacts::ArtifactError::InvalidReport)
        ));
    }

    fn compatibility_fixture() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "tool": {
                "package_version": "0.0.0",
                "executable_digest": "a".repeat(64),
                "trace_schema": TRACE_SCHEMA_VERSION,
                "case_trace_schema": CASE_TRACE_SCHEMA_VERSION,
                "fixture_control_protocol": 1
            },
            "platform_os": "linux",
            "platform_arch": "x86_64",
            "config_digest": "a".repeat(64),
            "compose": {
                "version": "5.4.0",
                "services": ["postgres", "reference-app", "stripe-fixture"],
                "resolved_redacted_hash": "a".repeat(64)
            },
            "sources": [{
                "kind": "invariant",
                "id": "provider-object-unique",
                "digest": "a".repeat(64)
            }],
            "baseline": {
                "server_fingerprint": "postgres-system-id:123456789",
                "endpoint_port": 15432,
                "database_name": "tiv_base_deadbeef",
                "database_oid": 16384,
                "owner_oid": 10,
                "marker_uuid": "00000000-0000-4000-8000-000000000001",
                "compose_project": "tiv-reference-app-spike",
                "application_role": "tiv_app"
            },
            "services": [{
                "service": "reference-app",
                "compose_config_hash": "a".repeat(64),
                "image_id": format!("sha256:{}", "a".repeat(64))
            }]
        })
    }
}
