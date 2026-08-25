use tiv_core::{
    decision::Seed,
    plan::{ActionBudget, CasePlanCompiler, PlanSpec},
    shrink::{CandidateGenerator, CandidateLimit, ShrinkCandidate},
    trace::{
        CaseCapturedValue, CaseOutputRef, CaseOutputSlot, CompiledShrinkTrace,
        ShrinkTraceMaterializer,
    },
};

fn candidate() -> ShrinkCandidate {
    let source = CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
        Seed::new(42),
        ActionBudget::new(40).unwrap(),
    ))
    .unwrap();
    CandidateGenerator::new(&source)
        .unwrap()
        .candidates(None, CandidateLimit::default())
        .unwrap()
        .into_iter()
        .next()
        .expect("seed 42 has at least one valid simplification")
}

fn captures(
    candidate: &ShrinkCandidate,
    gate_offset: u64,
) -> Vec<(CaseOutputRef, CaseCapturedValue)> {
    ShrinkTraceMaterializer::required_outputs(candidate)
        .unwrap()
        .into_iter()
        .enumerate()
        .map(|(index, output)| {
            let ordinal = index + 1;
            let value = match output.slot() {
                CaseOutputSlot::PaymentIntentId => {
                    CaseCapturedValue::payment_intent_id(format!("pi_shrink_{ordinal}")).unwrap()
                }
                CaseOutputSlot::EventId => {
                    CaseCapturedValue::event_id(format!("evt_shrink_{ordinal}")).unwrap()
                }
                CaseOutputSlot::ProviderGateId => CaseCapturedValue::provider_gate_id(
                    gate_offset + u64::try_from(ordinal).unwrap(),
                )
                .unwrap(),
            };
            (output, value)
        })
        .collect()
}

#[test]
fn a_shrink_candidate_materializes_and_round_trips_as_separate_authority() {
    let candidate = candidate();
    let compiled = ShrinkTraceMaterializer::materialize(&candidate, captures(&candidate, 0))
        .expect("every required candidate output was captured");

    assert_eq!(compiled.schema_version(), 1);
    assert_eq!(compiled.candidate(), &candidate);
    assert_eq!(compiled.action_count(), candidate.actions().len());
    assert_eq!(compiled.replay_actions().count(), candidate.actions().len());

    let encoded = serde_json::to_value(&compiled).unwrap();
    let decoded: CompiledShrinkTrace = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, compiled);
}

#[test]
fn shrink_replay_authority_rebinds_only_process_local_provider_gates() {
    let candidate = candidate();
    let recorded =
        ShrinkTraceMaterializer::materialize(&candidate, captures(&candidate, 0)).unwrap();
    let rebound =
        ShrinkTraceMaterializer::materialize(&candidate, captures(&candidate, 100)).unwrap();
    assert!(recorded.matches_replay_authority(&rebound));

    let mut changed = captures(&candidate, 0);
    let durable = changed
        .iter_mut()
        .find(|(_, value)| !matches!(value, CaseCapturedValue::ProviderGateId(_)))
        .unwrap();
    durable.1 = match durable.1.slot() {
        CaseOutputSlot::PaymentIntentId => {
            CaseCapturedValue::payment_intent_id("pi_shrink_changed").unwrap()
        }
        CaseOutputSlot::EventId => CaseCapturedValue::event_id("evt_shrink_changed").unwrap(),
        CaseOutputSlot::ProviderGateId => unreachable!(),
    };
    let changed = ShrinkTraceMaterializer::materialize(&candidate, changed).unwrap();
    assert!(!recorded.matches_replay_authority(&changed));
}

#[test]
fn shrink_trace_deserialization_rejects_tampered_materialized_actions() {
    let candidate = candidate();
    let compiled =
        ShrinkTraceMaterializer::materialize(&candidate, captures(&candidate, 0)).unwrap();
    let mut encoded = serde_json::to_value(compiled).unwrap();
    encoded["actions"][0]["logical_sequence"] = serde_json::json!(99);

    assert!(serde_json::from_value::<CompiledShrinkTrace>(encoded).is_err());
}
