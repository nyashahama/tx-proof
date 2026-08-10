use tiv_core::{
    decision::Seed,
    trace::{
        ActionId, ActionKind, CapturedValue, CompileError, CompiledTrace, InputSlot, OutputRef,
        OutputSlot, PlannedAction, PlannedTrace,
    },
};

#[test]
fn compilation_rejects_an_action_with_an_uncaptured_dynamic_output() {
    let create = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout)
        .declares_output(OutputSlot::PaymentIntentId);
    let plan = PlannedTrace::new(Seed::new(7), [create]);

    let error = plan
        .compile([])
        .expect_err("a replayable trace needs every declared output");

    assert_eq!(
        error,
        CompileError::MissingOutput(OutputRef::new(
            ActionId::new(1),
            OutputSlot::PaymentIntentId,
        ))
    );
}

#[test]
fn compilation_persists_the_concrete_value_used_by_replay() {
    let output_ref = OutputRef::new(ActionId::new(1), OutputSlot::PaymentIntentId);
    let create = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout)
        .declares_output(OutputSlot::PaymentIntentId);
    let plan = PlannedTrace::new(Seed::new(7), [create]);
    let captured = CapturedValue::payment_intent_id("pi_tiv_7_1").expect("the test ID is valid");

    let compiled = plan
        .compile([(output_ref, captured.clone())])
        .expect("the declared output was captured");

    assert_eq!(compiled.resolve(output_ref), Some(&captured));
}

#[test]
fn compilation_structurally_binds_a_required_action_input() {
    let payment_intent_output = OutputRef::new(ActionId::new(1), OutputSlot::PaymentIntentId);
    let create = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout)
        .declares_output(OutputSlot::PaymentIntentId);
    let confirm = PlannedAction::new(ActionId::new(2), ActionKind::ConfirmPaymentIntent)
        .depends_on(ActionId::new(1))
        .binds_input(InputSlot::PaymentIntentId, payment_intent_output);
    let captured = CapturedValue::payment_intent_id("pi_tiv_7_1").expect("the test ID is valid");

    let compiled = PlannedTrace::new(Seed::new(7), [create, confirm])
        .compile([(payment_intent_output, captured.clone())])
        .expect("the required input resolves to a captured producer output");
    let replay_action = compiled
        .replay_action(ActionId::new(2))
        .expect("the compiled action exists");

    assert_eq!(
        replay_action.input(InputSlot::PaymentIntentId),
        Some(&captured)
    );
}

#[test]
fn compilation_rejects_a_required_action_input_that_is_missing() {
    let confirm = PlannedAction::new(ActionId::new(1), ActionKind::ConfirmPaymentIntent);

    let error = PlannedTrace::new(Seed::new(7), [confirm])
        .compile([])
        .expect_err("a confirm cannot replay without its PaymentIntent ID");

    assert_eq!(
        error,
        CompileError::MissingInput {
            action_id: ActionId::new(1),
            input: InputSlot::PaymentIntentId,
        }
    );
}

#[test]
fn compilation_requires_an_input_source_to_be_an_explicit_dependency() {
    let payment_intent_output = OutputRef::new(ActionId::new(1), OutputSlot::PaymentIntentId);
    let create = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout)
        .declares_output(OutputSlot::PaymentIntentId);
    let confirm = PlannedAction::new(ActionId::new(2), ActionKind::ConfirmPaymentIntent)
        .binds_input(InputSlot::PaymentIntentId, payment_intent_output);

    let error = PlannedTrace::new(Seed::new(7), [create, confirm])
        .compile([(
            payment_intent_output,
            CapturedValue::payment_intent_id("pi_tiv_7_1").expect("the test ID is valid"),
        )])
        .expect_err("a data dependency must also appear in the action graph");

    assert_eq!(
        error,
        CompileError::InputSourceNotDependency {
            action_id: ActionId::new(2),
            input: InputSlot::PaymentIntentId,
            source_action_id: ActionId::new(1),
        }
    );
}

#[test]
fn compilation_rejects_an_input_source_the_producer_never_declared() {
    let payment_intent_output = OutputRef::new(ActionId::new(1), OutputSlot::PaymentIntentId);
    let create = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout);
    let confirm = PlannedAction::new(ActionId::new(2), ActionKind::ConfirmPaymentIntent)
        .depends_on(ActionId::new(1))
        .binds_input(InputSlot::PaymentIntentId, payment_intent_output);

    let error = PlannedTrace::new(Seed::new(7), [create, confirm])
        .compile([])
        .expect_err("replay cannot read an output the producer did not declare");

    assert_eq!(
        error,
        CompileError::InputSourceNotDeclared {
            action_id: ActionId::new(2),
            input: InputSlot::PaymentIntentId,
            source: payment_intent_output,
        }
    );
}

#[test]
fn compilation_rejects_a_captured_value_of_the_wrong_kind() {
    let output_ref = OutputRef::new(ActionId::new(1), OutputSlot::PaymentIntentId);
    let create = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout)
        .declares_output(OutputSlot::PaymentIntentId);
    let plan = PlannedTrace::new(Seed::new(7), [create]);

    let error = plan
        .compile([(
            output_ref,
            CapturedValue::event_id("evt_tiv_7_1").expect("the test ID is valid"),
        )])
        .expect_err("an event ID cannot bind a PaymentIntent output");

    assert_eq!(
        error,
        CompileError::OutputTypeMismatch {
            output_ref,
            actual: OutputSlot::EventId,
        }
    );
}

#[test]
fn compilation_rejects_an_action_whose_dependency_is_absent() {
    let checkpoint = PlannedAction::new(ActionId::new(2), ActionKind::DriveCheckout)
        .depends_on(ActionId::new(99));
    let plan = PlannedTrace::new(Seed::new(7), [checkpoint]);

    let error = plan
        .compile([])
        .expect_err("replay cannot execute an absent dependency");

    assert_eq!(
        error,
        CompileError::UnknownDependency {
            action_id: ActionId::new(2),
            dependency: ActionId::new(99),
        }
    );
}

#[test]
fn compilation_rejects_a_dependency_that_has_not_executed_yet() {
    let first = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout)
        .depends_on(ActionId::new(2));
    let second = PlannedAction::new(ActionId::new(2), ActionKind::DriveCheckout);
    let plan = PlannedTrace::new(Seed::new(7), [first, second]);

    let error = plan
        .compile([])
        .expect_err("a replay action can depend only on an earlier action");

    assert_eq!(
        error,
        CompileError::DependencyNotEarlier {
            action_id: ActionId::new(1),
            dependency: ActionId::new(2),
        }
    );
}

#[test]
fn compilation_rejects_duplicate_action_ids() {
    let first = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout);
    let duplicate = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout);
    let plan = PlannedTrace::new(Seed::new(7), [first, duplicate]);

    let error = plan
        .compile([])
        .expect_err("action IDs address replay steps and must be unique");

    assert_eq!(error, CompileError::DuplicateActionId(ActionId::new(1)));
}

#[test]
fn compilation_rejects_an_output_the_plan_never_declared() {
    let action = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout);
    let plan = PlannedTrace::new(Seed::new(7), [action]);
    let unexpected = OutputRef::new(ActionId::new(1), OutputSlot::EventId);

    let error = plan
        .compile([(
            unexpected,
            CapturedValue::event_id("evt_tiv_7_1").expect("the test ID is valid"),
        )])
        .expect_err("undeclared runtime values must not enter a replay trace");

    assert_eq!(error, CompileError::UnexpectedOutput(unexpected));
}

#[test]
fn a_compiled_trace_round_trips_with_an_explicit_schema_version() {
    let output_ref = OutputRef::new(ActionId::new(1), OutputSlot::PaymentIntentId);
    let create = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout)
        .declares_output(OutputSlot::PaymentIntentId);
    let compiled = PlannedTrace::new(Seed::new(7), [create])
        .compile([(
            output_ref,
            CapturedValue::payment_intent_id("pi_tiv_7_1").expect("the test ID is valid"),
        )])
        .expect("the trace has every declared output");

    let encoded = serde_json::to_value(&compiled).expect("compiled traces serialize");
    let decoded: CompiledTrace =
        serde_json::from_value(encoded.clone()).expect("compiled traces deserialize");

    assert_eq!(encoded["schema_version"], serde_json::json!(1));
    assert_eq!(encoded["seed"], serde_json::json!(7));
    assert_eq!(decoded, compiled);
}

#[test]
fn deserialization_rejects_an_unsupported_trace_schema() {
    let action = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout);
    let compiled = PlannedTrace::new(Seed::new(7), [action])
        .compile([])
        .expect("the trace has no dynamic outputs");
    let mut encoded = serde_json::to_value(compiled).expect("compiled traces serialize");
    encoded["schema_version"] = serde_json::json!(2);

    let decoded = serde_json::from_value::<CompiledTrace>(encoded);

    assert!(decoded.is_err());
}

#[test]
fn deserialization_revalidates_the_replay_action_graph() {
    let first = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout);
    let second = PlannedAction::new(ActionId::new(2), ActionKind::DriveCheckout)
        .depends_on(ActionId::new(1));
    let compiled = PlannedTrace::new(Seed::new(7), [first, second])
        .compile([])
        .expect("the dependency points backward");
    let mut encoded = serde_json::to_value(compiled).expect("compiled traces serialize");
    encoded["actions"][1]["id"] = serde_json::json!(1);

    let decoded = serde_json::from_value::<CompiledTrace>(encoded);

    assert!(decoded.is_err());
}

#[test]
fn deserialization_rejects_duplicate_captured_outputs() {
    let output_ref = OutputRef::new(ActionId::new(1), OutputSlot::PaymentIntentId);
    let action = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout)
        .declares_output(OutputSlot::PaymentIntentId);
    let compiled = PlannedTrace::new(Seed::new(7), [action])
        .compile([(
            output_ref,
            CapturedValue::payment_intent_id("pi_tiv_7_1").expect("the test ID is valid"),
        )])
        .expect("the trace has every declared output");
    let mut encoded = serde_json::to_value(compiled).expect("compiled traces serialize");
    let duplicate = encoded["captured"][0].clone();
    encoded["captured"]
        .as_array_mut()
        .expect("captured outputs serialize as an array")
        .push(duplicate);

    let decoded = serde_json::from_value::<CompiledTrace>(encoded);

    assert!(decoded.is_err());
}

#[test]
fn deserialization_rejects_a_malformed_captured_provider_id() {
    let output_ref = OutputRef::new(ActionId::new(1), OutputSlot::PaymentIntentId);
    let action = PlannedAction::new(ActionId::new(1), ActionKind::DriveCheckout)
        .declares_output(OutputSlot::PaymentIntentId);
    let compiled = PlannedTrace::new(Seed::new(7), [action])
        .compile([(
            output_ref,
            CapturedValue::payment_intent_id("pi_tiv_7_1").expect("the test ID is valid"),
        )])
        .expect("the trace has every declared output");
    let mut encoded = serde_json::to_value(compiled).expect("compiled traces serialize");
    encoded["captured"][0]["value"]["PaymentIntentId"] = serde_json::json!("");

    let decoded = serde_json::from_value::<CompiledTrace>(encoded);

    assert!(decoded.is_err());
}
