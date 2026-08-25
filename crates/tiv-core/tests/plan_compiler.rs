use std::collections::BTreeSet;

use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CasePlanCompiler, Checkpoint, PlanActionKind, PlanCompileError, PlanSpec,
        PlanValidationError, PlannedAction, PlannedCase, ProcessCutPoint, ProcessFaultSpec,
        ProviderOutcome, ProviderOutcomeScript, SCHEDULER_ALGORITHM, WebhookFaultSpec,
    },
    trace::ActionId,
};

#[test]
fn provider_scripts_match_the_reference_apps_single_transport_retry_policy() {
    let script = ProviderOutcomeScript::with_transport_retry(
        ProviderOutcome::CommitThenClose,
        ProviderOutcome::Normal,
    )
    .expect("a transport close may be followed by one retry");

    assert_eq!(
        script.outcomes().collect::<Vec<_>>(),
        vec![ProviderOutcome::CommitThenClose, ProviderOutcome::Normal]
    );
    assert_eq!(script.committed_count(), 2);
    assert!(
        ProviderOutcomeScript::with_transport_retry(
            ProviderOutcome::PostExecute500,
            ProviderOutcome::Normal,
        )
        .is_err(),
        "an HTTP 500 is a response, so the reference app does not retry it internally"
    );
    assert!(
        serde_json::from_value::<ProviderOutcomeScript>(serde_json::json!({
            "first": "post_execute_500",
            "retry": "normal"
        }))
        .is_err(),
        "untrusted scripts must revalidate the same retry boundary"
    );
}

#[test]
fn compiled_business_actions_include_the_apps_internal_transport_retry() {
    let mut saw_transport_retry = false;
    for seed in 0..512 {
        let plan = CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
            Seed::new(seed),
            ActionBudget::new(40).unwrap(),
        ))
        .expect("the v1 plan is feasible");

        for action in plan.actions() {
            let (PlanActionKind::DriveCheckout {
                provider_script: script,
            }
            | PlanActionKind::RetryBusinessRequest {
                provider_script: script,
            }) = action.kind()
            else {
                continue;
            };
            let outcomes = script.outcomes().collect::<Vec<_>>();
            if outcomes[0] == ProviderOutcome::CommitThenClose {
                saw_transport_retry = true;
                assert_eq!(
                    outcomes.len(),
                    2,
                    "the reference app always performs its internal retry before the business action ends"
                );
            } else {
                assert_eq!(outcomes.len(), 1);
            }
        }
    }
    assert!(saw_transport_retry);
}

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
fn the_v3_plan_artifact_matches_its_golden_contract() {
    let spec = PlanSpec::payment_intent_v1(Seed::new(42), ActionBudget::new(40).unwrap());
    let plan = CasePlanCompiler::compile(&spec).expect("the v1 plan is feasible");
    let actual = format!("{}\n", serde_json::to_string_pretty(&plan).unwrap());

    assert_eq!(
        actual,
        include_str!("golden/planned-case-v3.json"),
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
            if action_provider_script(action.kind())
                .is_some_and(|script| script.terminal_outcome() == ProviderOutcome::CommitThenDelay)
            {
                provider_gate_is_held = true;
                continue;
            }
            match action.kind() {
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
            .filter_map(|action| match action.kind() {
                PlanActionKind::DriveCheckout { provider_script }
                | PlanActionKind::RetryBusinessRequest { provider_script } => {
                    Some(usize::from(provider_script.committed_count()))
                }
                _ => None,
            })
            .sum::<usize>();
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
                PlanActionKind::DriveCheckout { provider_script }
                | PlanActionKind::RetryBusinessRequest { provider_script }
                | PlanActionKind::ConfirmPaymentIntent { provider_script }
                | PlanActionKind::RetryProviderRequest { provider_script } => {
                    provider_outcomes.extend(provider_script.outcomes());
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

const fn action_provider_script(kind: &PlanActionKind) -> Option<ProviderOutcomeScript> {
    match kind {
        PlanActionKind::DriveCheckout { provider_script }
        | PlanActionKind::RetryBusinessRequest { provider_script }
        | PlanActionKind::ConfirmPaymentIntent { provider_script }
        | PlanActionKind::RetryProviderRequest { provider_script } => Some(*provider_script),
        _ => None,
    }
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

    let plan = CasePlanCompiler::compile(&spec).expect("the v3 plan is feasible");
    let mut obsolete = serde_json::to_value(plan).unwrap();
    obsolete["schema_version"] = serde_json::json!(2);
    assert!(serde_json::from_value::<PlannedCase>(obsolete).is_err());
}

#[test]
fn response_observed_crash_makes_a_new_caller_request_eligible() {
    let process_faults =
        ProcessFaultSpec::new([ProcessCutPoint::ClientResponseObserved], 1).unwrap();
    let plan = (0..512)
        .find_map(|seed| {
            let spec = PlanSpec::new_payment_intent_v1(
                Seed::new(seed),
                ActionBudget::new(40).unwrap(),
                [ProviderOutcome::Normal],
                WebhookFaultSpec::new(0, [], false, false).unwrap(),
                process_faults.clone(),
            )
            .unwrap();
            let plan = CasePlanCompiler::compile(&spec).ok()?;
            let actions = plan.actions();
            (matches!(
                actions.first().map(PlannedAction::kind),
                Some(PlanActionKind::DriveCheckout { provider_script })
                    if provider_script.terminal_outcome() == ProviderOutcome::Normal
            ) && matches!(
                actions.get(1).map(PlannedAction::kind),
                Some(PlanActionKind::KillApplication {
                    cut_point: ProcessCutPoint::ClientResponseObserved
                })
            ) && matches!(
                actions.get(2).map(PlannedAction::kind),
                Some(PlanActionKind::RestartAndAwaitHealth)
            ) && matches!(
                actions.get(3).map(PlannedAction::kind),
                Some(PlanActionKind::RetryBusinessRequest { provider_script })
                    if provider_script.terminal_outcome() == ProviderOutcome::Normal
            ))
            .then_some(plan)
        })
        .expect("the bounded seed corpus reaches an unacknowledged caller retry");

    assert!(plan.validate().is_ok());
    assert_eq!(plan.seed().value(), 1);
}

#[test]
fn stale_event_capability_can_compile_two_snapshots_and_a_reversed_delivery() {
    let webhook_faults = WebhookFaultSpec::new(0, [], true, false)
        .unwrap()
        .with_stale_event(true);
    let plan = (0..512)
        .find_map(|seed| {
            let spec = PlanSpec::new_payment_intent_v1(
                Seed::new(seed),
                ActionBudget::new(40).unwrap(),
                [ProviderOutcome::Normal],
                webhook_faults.clone(),
                ProcessFaultSpec::new([], 0).unwrap(),
            )
            .unwrap();
            let plan = CasePlanCompiler::compile(&spec).ok()?;
            let actions = plan.actions();
            let generated = actions
                .iter()
                .filter(|action| matches!(action.kind(), PlanActionKind::GenerateProviderEvent))
                .count();
            let reordered = actions
                .iter()
                .filter(|action| matches!(action.kind(), PlanActionKind::ReorderWebhooks))
                .count();
            let delivered = actions
                .iter()
                .filter(|action| matches!(action.kind(), PlanActionKind::DeliverWebhook))
                .count();
            (generated == 2 && reordered == 1 && delivered == 2).then_some(plan)
        })
        .expect("the bounded seed corpus reaches reversed event history");

    assert!(plan.validate().is_ok());
    assert_eq!(plan.seed().value(), 2);
}
