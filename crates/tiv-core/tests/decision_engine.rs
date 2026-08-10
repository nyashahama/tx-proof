use tiv_core::decision::{DecisionEngine, Seed};

#[test]
fn same_seed_chooses_the_same_action_regardless_of_input_order() {
    let mut first = DecisionEngine::new(Seed::new(42));
    let mut second = DecisionEngine::new(Seed::new(42));

    let first_decision = first
        .choose(["deliver", "checkpoint", "crash"])
        .expect("three actions are eligible");
    let second_decision = second
        .choose(["crash", "deliver", "checkpoint"])
        .expect("three actions are eligible");

    assert_eq!(first_decision, second_decision);
    assert_eq!(first_decision.decision_number(), 0);
    assert_eq!(first_decision.eligible_count(), 3);
}

#[test]
fn no_eligible_actions_returns_an_error_without_consuming_a_decision() {
    let mut engine = DecisionEngine::new(Seed::new(42));

    let no_decision = engine.choose::<&str, _>([]);
    let first_decision = engine
        .choose(["checkpoint"])
        .expect("one action is eligible");

    assert!(no_decision.is_err());
    assert_eq!(first_decision.decision_number(), 0);
}
