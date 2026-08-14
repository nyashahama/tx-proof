use std::collections::BTreeSet;

use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CasePlanCompiler, Checkpoint, PlanActionKind, PlanCompileError, PlanSpec,
        PlanValidationError, PlannedAction, PlannedCase, ProcessCutPoint, ProcessFaultSpec,
        ProviderOutcome, SCHEDULER_ALGORITHM, WebhookFaultSpec,
    },
    trace::ActionId,
};

#[test]
fn the_same_seed_and_spec_compile_to_the_same_plan() {
    let spec = PlanSpec::payment_intent_v1(Seed::new(42), ActionBudget::new(40).unwrap());

    let first = CasePlanCompiler::compile(&spec).expect("the v1 plan is feasible");
    let second = CasePlanCompiler::compile(&spec).expect("the v1 plan is feasible");

    assert_eq!(first, second);
    assert_eq!(first.scheduler_algorithm(), SCHEDULER_ALGORITHM);
    assert_eq!(first.decision_count(), first.actions().len() as u64);
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
}

#[test]
fn the_v2_plan_artifact_matches_its_golden_contract() {
    let spec = PlanSpec::payment_intent_v1(Seed::new(42), ActionBudget::new(40).unwrap());
    let plan = CasePlanCompiler::compile(&spec).expect("the v1 plan is feasible");
    let actual = format!("{}\n", serde_json::to_string_pretty(&plan).unwrap());

    assert_eq!(
        actual,
        include_str!("golden/planned-case-v2.json"),
        "intentional plan wire changes require an explicit golden update"
    );
    let decoded: PlannedCase = serde_json::from_str(&actual).expect("the golden plan revalidates");
    assert_eq!(decoded, plan);
}

#[test]
fn generated_plans_are_state_valid_bounded_and_serial() {
    for seed in 0..128 {
        let budget = ActionBudget::new(40).unwrap();
        let spec = PlanSpec::payment_intent_v1(Seed::new(seed), budget);
        let plan = CasePlanCompiler::compile(&spec).expect("the v1 plan is feasible");

        plan.validate().expect("the compiler emitted a valid plan");
        assert!(plan.actions().len() <= budget.value() as usize);
        assert!(matches!(
            plan.actions().first().map(PlannedAction::kind),
            Some(PlanActionKind::DriveCheckout { .. })
        ));
        assert!(matches!(
            plan.actions().last().map(PlannedAction::kind),
            Some(PlanActionKind::CheckCheckpoint {
                checkpoint: Checkpoint::Final
            })
        ));
        assert!(
            plan.actions()
                .iter()
                .filter(|action| matches!(action.kind(), PlanActionKind::KillApplication { .. }))
                .count()
                <= 1
        );

        let mut application_was_killed = false;
        let mut provider_gate_is_held = false;
        for action in plan.actions() {
            match action.kind() {
                PlanActionKind::DriveCheckout {
                    outcome: ProviderOutcome::CommitThenDelay,
                }
                | PlanActionKind::RetryBusinessRequest {
                    outcome: ProviderOutcome::CommitThenDelay,
                }
                | PlanActionKind::ConfirmPaymentIntent {
                    outcome: ProviderOutcome::CommitThenDelay,
                }
                | PlanActionKind::RetryProviderRequest {
                    outcome: ProviderOutcome::CommitThenDelay,
                } => provider_gate_is_held = true,
                PlanActionKind::ReleaseProviderGate => {
                    assert!(
                        provider_gate_is_held,
                        "only a held provider response is released"
                    );
                    provider_gate_is_held = false;
                }
                PlanActionKind::KillApplication { cut_point } => {
                    if provider_gate_is_held {
                        assert_ne!(*cut_point, ProcessCutPoint::ClientResponseObserved);
                        assert_ne!(*cut_point, ProcessCutPoint::WebhookResponseObserved);
                    }
                    application_was_killed = true;
                }
                PlanActionKind::RestartAndAwaitHealth => {
                    assert!(application_was_killed, "restart must follow a planned kill");
                    application_was_killed = false;
                }
                _ => assert!(
                    !provider_gate_is_held,
                    "a held provider response must be released before provider progress"
                ),
            }
        }
        assert!(
            !application_was_killed,
            "a planned kill must be followed by restart"
        );
        assert!(
            !provider_gate_is_held,
            "a plan cannot leave a provider gate held"
        );

        for (index, action) in plan.actions().iter().enumerate() {
            let action_number = u32::try_from(index + 1).expect("v1 plans have at most 40 actions");
            assert_eq!(action.id(), ActionId::new(action_number));
            assert_eq!(action.decision_number(), index as u64);
            let expected_dependencies = if index == 0 {
                BTreeSet::new()
            } else {
                BTreeSet::from([ActionId::new(
                    u32::try_from(index).expect("v1 plans have at most 40 actions"),
                )])
            };
            assert_eq!(action.dependencies(), &expected_dependencies);
        }
    }
}

#[test]
fn generated_events_match_committed_provider_objects_before_reorder() {
    for seed in 0..512 {
        let spec = PlanSpec::payment_intent_v1(Seed::new(seed), ActionBudget::new(40).unwrap());
        let plan = CasePlanCompiler::compile(&spec).expect("the v1 plan is feasible");
        let committed_provider_objects = plan
            .actions()
            .iter()
            .filter(|action| {
                matches!(
                    action.kind(),
                    PlanActionKind::DriveCheckout {
                        outcome: ProviderOutcome::Normal
                            | ProviderOutcome::PostExecute500
                            | ProviderOutcome::CommitThenClose
                            | ProviderOutcome::CommitThenDelay
                    } | PlanActionKind::RetryBusinessRequest {
                        outcome: ProviderOutcome::Normal
                            | ProviderOutcome::PostExecute500
                            | ProviderOutcome::CommitThenClose
                            | ProviderOutcome::CommitThenDelay
                    }
                )
            })
            .count();
        let generated_events = plan
            .actions()
            .iter()
            .filter(|action| matches!(action.kind(), PlanActionKind::GenerateProviderEvent))
            .count();

        assert_eq!(
            generated_events, committed_provider_objects,
            "one succeeded event exists for each committed provider object at seed {seed}"
        );
        if plan
            .actions()
            .iter()
            .any(|action| matches!(action.kind(), PlanActionKind::ReorderWebhooks))
        {
            assert!(
                committed_provider_objects >= 2,
                "reorder needs at least two real provider events at seed {seed}"
            );
        }
    }
}

#[test]
fn the_seeded_compiler_reaches_every_v1_fault_family() {
    let mut provider_outcomes = BTreeSet::new();
    let mut saw_business_retry = false;
    let mut saw_provider_retry = false;
    let mut saw_duplicate = false;
    let mut saw_delay = false;
    let mut saw_reorder = false;
    let mut saw_drop = false;
    let mut cut_points = BTreeSet::new();

    for seed in 0..512 {
        let spec = PlanSpec::payment_intent_v1(Seed::new(seed), ActionBudget::new(40).unwrap());
        let plan = CasePlanCompiler::compile(&spec).expect("the v1 plan is feasible");

        for action in plan.actions() {
            match action.kind() {
                PlanActionKind::DriveCheckout { outcome }
                | PlanActionKind::RetryBusinessRequest { outcome }
                | PlanActionKind::ConfirmPaymentIntent { outcome }
                | PlanActionKind::RetryProviderRequest { outcome } => {
                    provider_outcomes.insert(*outcome);
                }
                PlanActionKind::DuplicateWebhook => saw_duplicate = true,
                PlanActionKind::DelayWebhook { .. } => saw_delay = true,
                PlanActionKind::ReorderWebhooks => saw_reorder = true,
                PlanActionKind::DropWebhook => saw_drop = true,
                PlanActionKind::KillApplication { cut_point } => {
                    cut_points.insert(*cut_point);
                }
                _ => {}
            }
            saw_business_retry |=
                matches!(action.kind(), PlanActionKind::RetryBusinessRequest { .. });
            saw_provider_retry |=
                matches!(action.kind(), PlanActionKind::RetryProviderRequest { .. });
        }
    }

    assert_eq!(
        provider_outcomes,
        BTreeSet::from([
            ProviderOutcome::Normal,
            ProviderOutcome::PreExecute429,
            ProviderOutcome::PreExecute500,
            ProviderOutcome::PostExecute500,
            ProviderOutcome::CommitThenClose,
            ProviderOutcome::CommitThenDelay,
        ])
    );
    assert!(saw_business_retry);
    assert!(saw_provider_retry);
    assert!(saw_duplicate);
    assert!(saw_delay);
    assert!(saw_reorder);
    assert!(saw_drop);
    assert_eq!(
        cut_points,
        BTreeSet::from([
            ProcessCutPoint::ClientRequestForwarded,
            ProcessCutPoint::ClientResponseObserved,
            ProcessCutPoint::WebhookRequestForwarded,
            ProcessCutPoint::WebhookResponseObserved,
            ProcessCutPoint::SqlProbe,
        ])
    );
}

#[test]
fn a_narrow_capability_spec_never_invents_disabled_faults() {
    let spec = PlanSpec::new_payment_intent_v1(
        Seed::new(99),
        ActionBudget::new(40).unwrap(),
        [ProviderOutcome::Normal],
        WebhookFaultSpec::new(0, [], false, false).unwrap(),
        ProcessFaultSpec::new([], 0).unwrap(),
    )
    .expect("the golden-path-only capability set is valid");

    let plan = CasePlanCompiler::compile(&spec).expect("the golden path is feasible");

    assert!(plan.actions().iter().all(|action| {
        !matches!(
            action.kind(),
            PlanActionKind::RetryBusinessRequest { .. }
                | PlanActionKind::RetryProviderRequest { .. }
                | PlanActionKind::DuplicateWebhook
                | PlanActionKind::DelayWebhook { .. }
                | PlanActionKind::ReorderWebhooks
                | PlanActionKind::DropWebhook
                | PlanActionKind::KillApplication { .. }
                | PlanActionKind::RestartAndAwaitHealth
        )
    }));
}

#[test]
fn capabilities_outside_the_v1_limits_fail_closed() {
    assert_eq!(
        WebhookFaultSpec::new(4, [], false, false),
        Err(PlanValidationError::CapabilityOutsideV1Bounds)
    );
    assert_eq!(
        WebhookFaultSpec::new(0, [5_001], false, false),
        Err(PlanValidationError::CapabilityOutsideV1Bounds)
    );
    assert_eq!(
        ProcessFaultSpec::new([], 2),
        Err(PlanValidationError::CapabilityOutsideV1Bounds)
    );

    let error = PlanSpec::new_payment_intent_v1(
        Seed::new(7),
        ActionBudget::new(40).unwrap(),
        [ProviderOutcome::CommitThenClose],
        WebhookFaultSpec::new(0, [], false, false).unwrap(),
        ProcessFaultSpec::new([], 0).unwrap(),
    )
    .expect_err("every provider fault set requires a normal recovery path");
    assert_eq!(error, PlanValidationError::MissingNormalProviderOutcome);
}

#[test]
fn a_budget_that_cannot_reach_the_checkpoint_fails_before_execution() {
    let spec = PlanSpec::payment_intent_v1(Seed::new(7), ActionBudget::new(3).unwrap());

    let error = CasePlanCompiler::compile(&spec).expect_err("three actions cannot finish a case");

    assert_eq!(
        error,
        PlanCompileError::BudgetCannotReachCheckpoint { max_actions: 3 }
    );
}

#[test]
fn deserialization_rejects_a_tampered_or_state_invalid_plan() {
    let spec = PlanSpec::payment_intent_v1(Seed::new(7), ActionBudget::new(40).unwrap());
    let plan = CasePlanCompiler::compile(&spec).expect("the v1 plan is feasible");
    let mut encoded = serde_json::to_value(plan).unwrap();
    encoded["actions"][0]["kind"] = serde_json::json!({ "kind": "check_checkpoint" });

    let decoded = serde_json::from_value::<tiv_core::plan::PlannedCase>(encoded);

    assert!(decoded.is_err());

    let plan = CasePlanCompiler::compile(&spec).expect("the v1 plan is feasible");
    let mut incompatible = serde_json::to_value(plan).unwrap();
    incompatible["spec"]["provider_api_version"] = serde_json::json!("unsupported");

    assert!(serde_json::from_value::<PlannedCase>(incompatible).is_err());

    let plan = CasePlanCompiler::compile(&spec).expect("the v2 plan is feasible");
    let mut obsolete = serde_json::to_value(plan).unwrap();
    obsolete["schema_version"] = serde_json::json!(1);
    assert!(serde_json::from_value::<PlannedCase>(obsolete).is_err());
}
