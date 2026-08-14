use tiv_core::{
    decision::Seed,
    plan::{ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec},
    trace::{
        ActionId, CaseCapturedValue, CaseInputSlot, CaseOutputRef, CaseOutputSlot,
        CaseTraceMaterializationError, CaseTraceMaterializer, CompiledCaseTrace,
    },
};

fn golden_case() -> tiv_core::plan::PlannedCase {
    CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
        Seed::new(42),
        ActionBudget::new(40).unwrap(),
    ))
    .expect("the pinned case is feasible")
}

fn complete_captures() -> Vec<(CaseOutputRef, CaseCapturedValue)> {
    vec![
        (
            CaseOutputRef::new(ActionId::new(1), CaseOutputSlot::PaymentIntentId),
            CaseCapturedValue::payment_intent_id("pi_tiv_42_1").unwrap(),
        ),
        (
            CaseOutputRef::new(ActionId::new(1), CaseOutputSlot::ProviderGateId),
            CaseCapturedValue::provider_gate_id(1).unwrap(),
        ),
        (
            CaseOutputRef::new(ActionId::new(6), CaseOutputSlot::EventId),
            CaseCapturedValue::event_id("evt_tiv_42_1").unwrap(),
        ),
    ]
}

#[test]
fn a_full_planned_case_materializes_every_action_and_dynamic_binding() {
    let plan = golden_case();
    let payment_intent = complete_captures()[0].1.clone();
    let gate = complete_captures()[1].1.clone();

    let compiled = CaseTraceMaterializer::materialize(&plan, complete_captures())
        .expect("every reserved output was captured");

    assert_eq!(compiled.schema_version(), 2);
    assert_eq!(compiled.action_count(), plan.actions().len());
    assert_eq!(compiled.planned_case(), &plan);
    assert_eq!(
        compiled
            .replay_action(ActionId::new(4))
            .unwrap()
            .input(CaseInputSlot::ProviderGateId),
        Some(&gate)
    );
    assert_eq!(
        compiled
            .replay_action(ActionId::new(5))
            .unwrap()
            .input(CaseInputSlot::PaymentIntentId),
        Some(&payment_intent)
    );
    assert_eq!(
        compiled
            .replay_action(ActionId::new(6))
            .unwrap()
            .input(CaseInputSlot::PaymentIntentId),
        Some(&payment_intent)
    );
    assert!(matches!(
        compiled.replay_action(ActionId::new(2)).unwrap().kind(),
        PlanActionKind::KillApplication { .. }
    ));
}

#[test]
fn a_materialized_case_round_trips_and_revalidates_the_complete_artifact() {
    let compiled = CaseTraceMaterializer::materialize(&golden_case(), complete_captures())
        .expect("every reserved output was captured");
    let encoded = serde_json::to_value(&compiled).unwrap();
    let decoded = serde_json::from_value::<CompiledCaseTrace>(encoded.clone())
        .expect("the materialized case revalidates");

    assert_eq!(decoded, compiled);
    assert_eq!(encoded["schema_version"], serde_json::json!(2));
    assert_eq!(
        decoded.resolve(CaseOutputRef::new(
            ActionId::new(6),
            CaseOutputSlot::EventId
        )),
        Some(&CaseCapturedValue::event_id("evt_tiv_42_1").unwrap())
    );
}

#[test]
fn materialization_rejects_missing_unexpected_duplicate_and_wrong_typed_values() {
    let plan = golden_case();
    let mut missing = complete_captures();
    missing.pop();
    assert!(matches!(
        CaseTraceMaterializer::materialize(&plan, missing),
        Err(CaseTraceMaterializationError::MissingOutput(_))
    ));

    let mut unexpected = complete_captures();
    unexpected.push((
        CaseOutputRef::new(ActionId::new(2), CaseOutputSlot::EventId),
        CaseCapturedValue::event_id("evt_tiv_unexpected").unwrap(),
    ));
    assert!(matches!(
        CaseTraceMaterializer::materialize(&plan, unexpected),
        Err(CaseTraceMaterializationError::UnexpectedOutput(_))
    ));

    let mut duplicate = complete_captures();
    duplicate.push(duplicate[0].clone());
    assert!(matches!(
        CaseTraceMaterializer::materialize(&plan, duplicate),
        Err(CaseTraceMaterializationError::DuplicateOutput(_))
    ));

    let mut wrong_type = complete_captures();
    wrong_type[0].1 = CaseCapturedValue::event_id("evt_tiv_wrong_kind").unwrap();
    assert!(matches!(
        CaseTraceMaterializer::materialize(&plan, wrong_type),
        Err(CaseTraceMaterializationError::OutputTypeMismatch { .. })
    ));
}

#[test]
fn deserialization_rejects_tampered_actions_and_invalid_gate_ids() {
    assert!(CaseCapturedValue::provider_gate_id(0).is_err());

    let compiled = CaseTraceMaterializer::materialize(&golden_case(), complete_captures())
        .expect("every reserved output was captured");
    let mut invalid_gate = serde_json::to_value(&compiled).unwrap();
    invalid_gate["captured"][1]["value"]["ProviderGateId"] = serde_json::json!(0);
    assert!(serde_json::from_value::<CompiledCaseTrace>(invalid_gate).is_err());

    let mut unsupported = serde_json::to_value(&compiled).unwrap();
    unsupported["schema_version"] = serde_json::json!(1);
    assert!(serde_json::from_value::<CompiledCaseTrace>(unsupported).is_err());

    let mut encoded = serde_json::to_value(compiled).unwrap();
    encoded["actions"][4]["kind"] = serde_json::json!({
        "kind": "confirm_payment_intent",
        "outcome": "pre_execute_500"
    });

    assert!(serde_json::from_value::<CompiledCaseTrace>(encoded).is_err());
}

#[test]
fn capture_requirements_are_exact_and_every_seeded_plan_materializes() {
    assert_eq!(
        CaseTraceMaterializer::required_outputs(&golden_case()).unwrap(),
        vec![
            CaseOutputRef::new(ActionId::new(1), CaseOutputSlot::PaymentIntentId),
            CaseOutputRef::new(ActionId::new(1), CaseOutputSlot::ProviderGateId),
            CaseOutputRef::new(ActionId::new(6), CaseOutputSlot::EventId),
        ]
    );

    for seed in 0..128_u64 {
        let plan = CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
            Seed::new(seed),
            ActionBudget::new(40).unwrap(),
        ))
        .expect("the pinned v1 campaign space is feasible");
        let captured = CaseTraceMaterializer::required_outputs(&plan)
            .unwrap()
            .into_iter()
            .enumerate()
            .map(|(index, output)| {
                let value = match output.slot() {
                    CaseOutputSlot::PaymentIntentId => {
                        CaseCapturedValue::payment_intent_id(format!("pi_tiv_{seed}_{index}"))
                            .unwrap()
                    }
                    CaseOutputSlot::EventId => {
                        CaseCapturedValue::event_id(format!("evt_tiv_{seed}_{index}")).unwrap()
                    }
                    CaseOutputSlot::ProviderGateId => {
                        CaseCapturedValue::provider_gate_id(u64::try_from(index + 1).unwrap())
                            .unwrap()
                    }
                };
                (output, value)
            });

        let compiled = CaseTraceMaterializer::materialize(&plan, captured)
            .expect("every reserved output materializes");
        assert_eq!(compiled.action_count(), plan.actions().len());
        compiled.replay_actions().for_each(|action| {
            assert_eq!(
                action.id(),
                plan.actions()[action.logical_sequence() as usize - 1].id()
            );
        });
    }
}
