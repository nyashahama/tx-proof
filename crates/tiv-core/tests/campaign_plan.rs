use std::collections::BTreeSet;

use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CampaignPlanner, CampaignSpec, CaseCount, CaseId, PlanActionKind,
        ProcessFaultSpec, ProviderOutcome, WebhookFaultSpec,
    },
};

#[test]
fn a_campaign_compiles_distinct_self_validating_cases_in_serial_order() {
    let spec = CampaignSpec::payment_intent_v1(
        Seed::new(424_242),
        CaseCount::new(20).unwrap(),
        ActionBudget::new(40).unwrap(),
    );

    let first = CampaignPlanner::compile(&spec).expect("the campaign is feasible");
    let second = CampaignPlanner::compile(&spec).expect("the campaign is feasible");

    assert_eq!(first, second);
    assert_eq!(first.cases().len(), 20);
    first.validate().expect("the campaign revalidates");

    let seeds = first
        .cases()
        .iter()
        .map(|case| case.plan().seed().value())
        .collect::<BTreeSet<_>>();
    assert_eq!(seeds.len(), 20);
    for (index, case) in first.cases().iter().enumerate() {
        assert_eq!(
            case.id(),
            CaseId::new(u32::try_from(index + 1).unwrap()).unwrap()
        );
        assert!(case.plan().actions().len() <= 40);
        case.plan().validate().expect("each case plan revalidates");
    }
}

#[test]
fn case_seed_derivation_has_a_pinned_v1_sequence() {
    let spec = CampaignSpec::payment_intent_v1(
        Seed::new(424_242),
        CaseCount::new(3).unwrap(),
        ActionBudget::new(40).unwrap(),
    );
    let campaign = CampaignPlanner::compile(&spec).expect("the campaign is feasible");

    assert_eq!(
        campaign
            .cases()
            .iter()
            .map(|case| case.plan().seed().value())
            .collect::<Vec<_>>(),
        vec![
            13_917_578_233_472_882_488,
            4_551_032_050_508_465_243,
            6_026_433_034_610_612_016,
        ]
    );
}

#[test]
fn campaign_capabilities_are_applied_to_every_case() {
    let spec = CampaignSpec::new_payment_intent_v1(
        Seed::new(7),
        CaseCount::new(8).unwrap(),
        ActionBudget::new(40).unwrap(),
        [ProviderOutcome::Normal],
        WebhookFaultSpec::new(0, [], false, false).unwrap(),
        ProcessFaultSpec::new([], 0).unwrap(),
    )
    .expect("the narrow campaign is valid");
    let campaign = CampaignPlanner::compile(&spec).expect("the campaign is feasible");

    assert!(campaign.cases().iter().all(|case| {
        case.plan().actions().iter().all(|action| {
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
        })
    }));
}

#[test]
fn campaign_limits_and_untrusted_artifacts_fail_closed() {
    assert!(CaseCount::new(0).is_err());
    assert!(CaseCount::new(501).is_err());

    let spec = CampaignSpec::payment_intent_v1(
        Seed::new(7),
        CaseCount::new(2).unwrap(),
        ActionBudget::new(40).unwrap(),
    );
    let campaign = CampaignPlanner::compile(&spec).expect("the campaign is feasible");
    let mut encoded = serde_json::to_value(campaign).unwrap();
    encoded["cases"][1]["case_id"] = serde_json::json!(1);

    assert!(serde_json::from_value::<tiv_core::plan::CampaignPlan>(encoded).is_err());
}
