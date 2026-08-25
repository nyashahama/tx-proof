use std::path::PathBuf;

use tiv_core::{
    decision::Seed,
    plan::{ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec},
    shrink::{CandidateGenerator, CandidateLimit, ShrinkCandidate},
    trace::{CaseCapturedValue, CaseInputSlot, CaseOutputSlot},
};
use tiv_runtime::campaign::{
    CaseCaptureError, CaseEffectAdapter, CaseEffectFuture, CaseEffectRequest, CaseExecutionCause,
    CaseExecutionError, execute_planned_case, execute_shrink_candidate,
};
use uuid::Uuid;

struct RecordingAdapter {
    journal_path: PathBuf,
    calls: usize,
    fail_on_call: Option<usize>,
    omit_first_capture: bool,
}

impl RecordingAdapter {
    fn success(journal_path: PathBuf) -> Self {
        Self {
            journal_path,
            calls: 0,
            fail_on_call: None,
            omit_first_capture: false,
        }
    }
}

impl CaseEffectAdapter for RecordingAdapter {
    type Error = &'static str;

    fn execute<'a>(
        &'a mut self,
        request: CaseEffectRequest<'a>,
    ) -> CaseEffectFuture<'a, Self::Error> {
        Box::pin(async move {
            let on_disk = tokio::fs::read_to_string(&self.journal_path)
                .await
                .expect("the action intent is durable before the effect starts");
            let records = on_disk.lines().collect::<Vec<_>>();
            assert_eq!(records.len(), self.calls * 2 + 1);
            let last: serde_json::Value = serde_json::from_str(records.last().unwrap()).unwrap();
            assert_eq!(last["observation_kind"], serde_json::json!("action_intent"));
            assert_eq!(last["action_id"], serde_json::json!(request.action().id()));
            match request.action().kind() {
                PlanActionKind::RetrievePaymentIntent
                | PlanActionKind::ConfirmPaymentIntent { .. }
                | PlanActionKind::RetryProviderRequest { .. }
                | PlanActionKind::GenerateProviderEvent => assert!(
                    request.input(CaseInputSlot::PaymentIntentId).is_some(),
                    "provider actions receive the exact previously captured PaymentIntent"
                ),
                PlanActionKind::ReleaseProviderGate => assert!(
                    request.input(CaseInputSlot::ProviderGateId).is_some(),
                    "release receives the exact previously captured gate"
                ),
                _ => {}
            }

            let call = self.calls;
            self.calls += 1;
            if self.fail_on_call == Some(call) {
                return Err("planned adapter failure");
            }
            if self.omit_first_capture && call == 0 {
                return Ok(Vec::new());
            }
            Ok(request
                .expected_outputs()
                .iter()
                .copied()
                .enumerate()
                .map(|(index, output)| {
                    let value = match output.slot() {
                        CaseOutputSlot::PaymentIntentId => {
                            CaseCapturedValue::payment_intent_id(format!("pi_tiv_{call}_{index}"))
                                .unwrap()
                        }
                        CaseOutputSlot::EventId => {
                            CaseCapturedValue::event_id(format!("evt_tiv_{call}_{index}")).unwrap()
                        }
                        CaseOutputSlot::ProviderGateId => CaseCapturedValue::provider_gate_id(
                            u64::try_from(call + index + 1).unwrap(),
                        )
                        .unwrap(),
                    };
                    (output, value)
                })
                .collect())
        })
    }
}

fn golden_case() -> tiv_core::plan::PlannedCase {
    CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
        Seed::new(42),
        ActionBudget::new(40).unwrap(),
    ))
    .expect("the pinned case is feasible")
}

fn shrink_candidate() -> ShrinkCandidate {
    let plan = golden_case();
    CandidateGenerator::new(&plan)
        .unwrap()
        .candidates(None, CandidateLimit::default())
        .unwrap()
        .into_iter()
        .next()
        .expect("the pinned case has a valid simplification")
}

fn journal_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("tiv-{label}-{}.ndjson", Uuid::new_v4()))
}

#[tokio::test]
async fn actions_run_serially_after_durable_intent_and_materialize_one_trace() {
    let path = journal_path("serial-case");
    let plan = golden_case();
    let mut adapter = RecordingAdapter::success(path.clone());

    let execution = execute_planned_case("run_1", "case_1", &plan, &path, &mut adapter)
        .await
        .expect("the complete case executes");

    assert_eq!(adapter.calls, plan.actions().len());
    assert_eq!(execution.trace().action_count(), plan.actions().len());
    assert_eq!(
        execution.journal_summary().record_count(),
        plan.actions().len() * 2
    );
    let records = tokio::fs::read_to_string(&path).await.unwrap();
    for (index, record) in records.lines().enumerate() {
        let record: serde_json::Value = serde_json::from_str(record).unwrap();
        let expected = if index % 2 == 0 {
            "action_intent"
        } else {
            "action_outcome"
        };
        assert_eq!(record["observation_kind"], serde_json::json!(expected));
    }

    tokio::fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn shrink_candidates_use_the_same_serial_journaled_execution_boundary() {
    let path = journal_path("serial-shrink-case");
    let candidate = shrink_candidate();
    let mut adapter = RecordingAdapter::success(path.clone());

    let execution = execute_shrink_candidate(
        "run_shrink_1",
        "candidate_1_attempt_1",
        &candidate,
        &path,
        &mut adapter,
    )
    .await
    .expect("the validated candidate executes");

    assert_eq!(adapter.calls, candidate.actions().len());
    assert_eq!(execution.trace().candidate(), &candidate);
    assert_eq!(execution.trace().action_count(), candidate.actions().len());
    assert_eq!(
        execution.journal_summary().record_count(),
        candidate.actions().len() * 2
    );

    tokio::fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn an_effect_failure_finalizes_an_intent_only_journal() {
    let path = journal_path("failed-case");
    let plan = golden_case();
    let mut adapter = RecordingAdapter {
        fail_on_call: Some(0),
        ..RecordingAdapter::success(path.clone())
    };

    let error = execute_planned_case("run_1", "case_1", &plan, &path, &mut adapter)
        .await
        .expect_err("the adapter failure stops the serial case");
    let CaseExecutionError::Failed(failure) = error else {
        panic!("the created journal must be finalized on failure");
    };
    assert!(matches!(
        failure.cause(),
        CaseExecutionCause::Effect("planned adapter failure")
    ));
    assert_eq!(
        failure
            .journal_result()
            .as_ref()
            .expect("the failed case journal finalized")
            .record_count(),
        1
    );
    assert_eq!(
        tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .lines()
            .count(),
        1
    );

    tokio::fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn missing_action_captures_fail_before_an_outcome_is_recorded() {
    let path = journal_path("missing-capture");
    let plan = golden_case();
    let mut adapter = RecordingAdapter {
        omit_first_capture: true,
        ..RecordingAdapter::success(path.clone())
    };

    let error = execute_planned_case("run_1", "case_1", &plan, &path, &mut adapter)
        .await
        .expect_err("an incomplete capture set cannot become replay authority");
    let CaseExecutionError::Failed(failure) = error else {
        panic!("the created journal must be finalized on failure");
    };
    assert!(matches!(
        failure.cause(),
        CaseExecutionCause::Capture(CaseCaptureError::MissingOutput(_))
    ));
    assert_eq!(
        tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .lines()
            .count(),
        1
    );

    tokio::fs::remove_file(path).await.unwrap();
}

#[tokio::test]
async fn invalid_identity_and_journal_collisions_fail_before_effects_or_overwrite() {
    let invalid_path = journal_path("invalid-context");
    let plan = golden_case();
    let mut adapter = RecordingAdapter::success(invalid_path.clone());

    let invalid = execute_planned_case("bad run id", "case_1", &plan, &invalid_path, &mut adapter)
        .await
        .expect_err("journal identities are narrow before file creation");
    assert!(matches!(invalid, CaseExecutionError::InvalidContext(_)));
    assert_eq!(adapter.calls, 0);
    assert!(!tokio::fs::try_exists(&invalid_path).await.unwrap());

    let collision_path = journal_path("collision");
    tokio::fs::write(&collision_path, b"user-owned\n")
        .await
        .unwrap();
    adapter.journal_path.clone_from(&collision_path);
    let collision = execute_planned_case("run_1", "case_1", &plan, &collision_path, &mut adapter)
        .await
        .expect_err("an existing artifact is never replaced");
    assert!(matches!(collision, CaseExecutionError::JournalCreate(_)));
    assert_eq!(adapter.calls, 0);
    assert_eq!(
        tokio::fs::read_to_string(&collision_path).await.unwrap(),
        "user-owned\n"
    );

    tokio::fs::remove_file(collision_path).await.unwrap();
}
