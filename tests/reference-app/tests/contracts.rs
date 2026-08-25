use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use tiv_core::{
    decision::Seed,
    plan::{
        ActionBudget, CasePlanCompiler, PlanActionKind, PlanSpec, ProviderOutcome,
        ProviderOutcomeScript,
    },
};
use tiv_reference_app::{
    CallerRetryMode, CheckoutOperation, LedgerBalanceMode, ReconciliationMode,
    ReferenceDatabaseName, RetryKeyMode, TerminalStateMode, WebhookEffectMode,
    create_with_changed_retry_key, create_with_changed_retry_key_for_business_request,
    create_with_retry_key_mode, parse_succeeded_webhook_event, verify_webhook_signature,
};
use tiv_stripe_pi::{
    CreatePaymentIntent, FaultOutcome, IdempotencyKey, ManagedFixture, OperationId,
    PaymentIntentFixture, http::serve_managed_http1_connection,
};
use tokio::{net::TcpListener, sync::Mutex, time::timeout};

#[test]
fn only_generated_case_database_names_are_accepted() {
    assert!(ReferenceDatabaseName::parse("tiv_case_0123456789abcdef").is_ok());
    for invalid in [
        "postgres",
        "tiv_base_0123456789abcdef",
        "tiv_case_",
        "tiv_case_UPPER",
        "tiv_case_bad-name",
        "tiv_case_bad;drop_database",
        "tiv_case_1",
        "tiv_case_notgenerated",
        "tiv_case_0123456789abcdef_",
        "tiv_case_0123456789abcdef0123456789abcdef0",
    ] {
        assert!(
            ReferenceDatabaseName::parse(invalid).is_err(),
            "{invalid:?} must not be a reference-app target"
        );
    }
}

#[test]
fn generated_case_names_have_one_reversible_operation_identity() {
    let database = ReferenceDatabaseName::parse("tiv_case_0123456789abcdef")
        .expect("the generated case name is valid");

    assert_eq!(database.operation_id(), "op_0123456789abcdef");
    assert_eq!(
        ReferenceDatabaseName::from_operation_id(&database.operation_id()),
        Ok(database)
    );

    for unrelated in [
        "op_1",
        "op_nothexadecimal",
        "other_0123456789abcdef",
        "op_0123456789abcdef0123456789abcdef0",
    ] {
        assert!(
            ReferenceDatabaseName::from_operation_id(unrelated).is_err(),
            "{unrelated:?} must not select a reference database"
        );
    }
}

#[test]
fn retry_key_mode_accepts_only_the_two_explicit_contract_values() {
    assert_eq!(
        "faulty_changed_key".parse::<RetryKeyMode>(),
        Ok(RetryKeyMode::FaultyChangedKey)
    );
    assert_eq!(
        "repaired_same_key".parse::<RetryKeyMode>(),
        Ok(RetryKeyMode::RepairedSameKey)
    );
    for invalid in [
        "",
        "changed_key",
        "same_key",
        "FAULTY_CHANGED_KEY",
        "repaired_same_key ",
    ] {
        assert!(
            invalid.parse::<RetryKeyMode>().is_err(),
            "{invalid:?} must not select a retry mode"
        );
    }
}

#[test]
fn caller_retry_mode_accepts_only_the_fault_and_repaired_contract_values() {
    assert_eq!(
        "faulty_per_request".parse::<CallerRetryMode>(),
        Ok(CallerRetryMode::FaultyPerRequest)
    );
    assert_eq!(
        "repaired_recover_operation".parse::<CallerRetryMode>(),
        Ok(CallerRetryMode::RepairedRecoverOperation)
    );
    for invalid in [
        "",
        "faulty",
        "repaired",
        "FAULTY_PER_REQUEST",
        "repaired_recover_operation ",
    ] {
        assert!(
            invalid.parse::<CallerRetryMode>().is_err(),
            "{invalid:?} must not select a caller-retry mode"
        );
    }
}

#[test]
fn reconciliation_mode_accepts_only_webhook_only_and_provider_repair_values() {
    assert_eq!(
        "faulty_webhook_only".parse::<ReconciliationMode>(),
        Ok(ReconciliationMode::FaultyWebhookOnly)
    );
    assert_eq!(
        "repaired_provider_reconcile".parse::<ReconciliationMode>(),
        Ok(ReconciliationMode::RepairedProviderReconcile)
    );
    for invalid in [
        "",
        "faulty",
        "repaired",
        "FAULTY_WEBHOOK_ONLY",
        "repaired_provider_reconcile ",
    ] {
        assert!(
            invalid.parse::<ReconciliationMode>().is_err(),
            "{invalid:?} must not select a reconciliation mode"
        );
    }
}

#[test]
fn terminal_state_mode_accepts_only_arrival_order_and_monotonic_values() {
    assert_eq!(
        "faulty_arrival_order".parse::<TerminalStateMode>(),
        Ok(TerminalStateMode::FaultyArrivalOrder)
    );
    assert_eq!(
        "repaired_monotonic".parse::<TerminalStateMode>(),
        Ok(TerminalStateMode::RepairedMonotonic)
    );
    for invalid in [
        "",
        "faulty",
        "repaired",
        "FAULTY_ARRIVAL_ORDER",
        "repaired_monotonic ",
    ] {
        assert!(
            invalid.parse::<TerminalStateMode>().is_err(),
            "{invalid:?} must not select a terminal-state mode"
        );
    }
}

#[test]
fn webhook_effect_mode_accepts_only_the_fault_and_repaired_contract_values() {
    assert_eq!(
        "faulty_duplicate_effect".parse::<WebhookEffectMode>(),
        Ok(WebhookEffectMode::FaultyDuplicateEffect)
    );
    assert_eq!(
        "repaired_deduplicate".parse::<WebhookEffectMode>(),
        Ok(WebhookEffectMode::RepairedDeduplicate)
    );
    for invalid in [
        "",
        "faulty_duplicate",
        "repaired",
        "FAULTY_DUPLICATE_EFFECT",
        "repaired_deduplicate ",
    ] {
        assert!(
            invalid.parse::<WebhookEffectMode>().is_err(),
            "{invalid:?} must not select a webhook-effect mode"
        );
    }
}

#[test]
fn ledger_balance_mode_accepts_only_the_fault_and_repaired_contract_values() {
    assert_eq!(
        "faulty_one_sided_duplicate".parse::<LedgerBalanceMode>(),
        Ok(LedgerBalanceMode::FaultyOneSidedOnDuplicate)
    );
    assert_eq!(
        "repaired_balanced_once".parse::<LedgerBalanceMode>(),
        Ok(LedgerBalanceMode::RepairedBalancedOnce)
    );
    for invalid in [
        "",
        "faulty_one_sided",
        "repaired_balanced",
        "FAULTY_ONE_SIDED_DUPLICATE",
        "repaired_balanced_once ",
    ] {
        assert!(
            invalid.parse::<LedgerBalanceMode>().is_err(),
            "{invalid:?} must not select a ledger-balance mode"
        );
    }
}

#[test]
fn webhook_verification_covers_the_exact_raw_bytes_and_rejects_stale_signatures() {
    let secret = b"whsec_test_secret";
    let timestamp = current_unix_timestamp();
    let raw_body = br#"{"id":"evt_1","type":"payment_intent.succeeded"}"#;
    let signature = signature_header(timestamp, raw_body, secret);

    assert!(verify_webhook_signature(raw_body, &signature, secret).is_ok());
    assert!(
        verify_webhook_signature(
            br#"{"id":"evt_1", "type":"payment_intent.succeeded"}"#,
            &signature,
            secret,
        )
        .is_err(),
        "even semantically equivalent byte changes must fail verification"
    );
    let stale_timestamp = timestamp - 301;
    let stale_signature = signature_header(stale_timestamp, raw_body, secret);
    assert!(
        verify_webhook_signature(raw_body, &stale_signature, secret).is_err(),
        "a correctly signed webhook outside the five-minute tolerance must fail"
    );
}

#[test]
fn signed_fixture_webhook_is_decoded_into_the_same_operation_relation() {
    let mut fixture = PaymentIntentFixture::new(Seed::new(42));
    let created = fixture
        .create(
            IdempotencyKey::new("op-1-attempt-1").expect("the key is valid"),
            CreatePaymentIntent::new(2_500, "usd")
                .expect("the create is valid")
                .with_operation_id(OperationId::new("op_1").expect("the operation ID is valid")),
            FaultOutcome::Normal,
        )
        .expect("the provider object is created");
    fixture
        .confirm(created.id())
        .expect("the provider object confirms");
    let attempt = fixture.events()[0]
        .webhook_attempt(current_unix_timestamp(), b"whsec_test_secret")
        .expect("the event is signed");

    let event = parse_succeeded_webhook_event(
        attempt.raw_body(),
        attempt.signature_header(),
        b"whsec_test_secret",
    )
    .expect("the exact signed event is accepted");
    let observed = event.payment_intent();

    assert_eq!(event.id(), fixture.events()[0].id());
    assert_eq!(observed.id(), created.id());
    assert_eq!(observed.operation_id(), "op_1");
    assert_eq!(observed.amount_minor(), 2_500);
    assert_eq!(observed.currency(), "usd");
    assert_eq!(observed.status(), "succeeded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_transport_failure_is_retried_with_a_changed_key() {
    let (seed, provider_script) = compiled_changed_key_retry_script();
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(seed)));
    fixture
        .lock()
        .await
        .reset(
            1,
            seed,
            provider_script.outcomes().map(fixture_outcome).collect(),
        )
        .expect("the fault plan is installed");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.expect("the app connects");
            let _result = serve_managed_http1_connection(stream, Arc::clone(&server_fixture)).await;
        }
    });
    let operation =
        CheckoutOperation::new("op_1", 2_500, "usd").expect("the checkout operation is valid");

    let observed = create_with_changed_retry_key(
        &reqwest::Client::new(),
        &format!("http://{address}"),
        &operation,
    )
    .await
    .expect("the faulty retry receives the second provider object");

    assert!(observed.id().starts_with("pi_tiv_"));
    assert_eq!(observed.operation_id(), "op_1");
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the two-connection fixture stops")
        .expect("the fixture task does not panic");
    let snapshot = fixture.lock().await.snapshot();
    assert_eq!(
        snapshot.payment_intents().len(),
        usize::from(provider_script.committed_count()),
        "the action-scoped plan must count every provider object committed by one business request"
    );
    assert_ne!(
        snapshot.payment_intents()[0].id(),
        snapshot.payment_intents()[1].id()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ambiguous_transport_failure_retried_with_the_same_key_reuses_the_committed_object() {
    let (seed, provider_script) = compiled_changed_key_retry_script();
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(seed)));
    fixture
        .lock()
        .await
        .reset(
            1,
            seed,
            provider_script.outcomes().map(fixture_outcome).collect(),
        )
        .expect("the fault plan is installed");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.expect("the app connects");
            let _result = serve_managed_http1_connection(stream, Arc::clone(&server_fixture)).await;
        }
    });
    let operation =
        CheckoutOperation::new("op_1", 2_500, "usd").expect("the checkout operation is valid");

    let observed = create_with_retry_key_mode(
        &reqwest::Client::new(),
        &format!("http://{address}"),
        &operation,
        RetryKeyMode::RepairedSameKey,
    )
    .await
    .expect("the repaired retry receives the cached provider object");

    assert!(observed.id().starts_with("pi_tiv_"));
    assert_eq!(observed.operation_id(), "op_1");
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the two-connection fixture stops")
        .expect("the fixture task does not panic");
    let snapshot = fixture.lock().await.snapshot();
    assert_eq!(snapshot.payment_intents().len(), 1);
    assert_eq!(snapshot.remaining_outcomes(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separate_business_requests_use_separate_provider_idempotency_scopes() {
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(41))));
    fixture
        .lock()
        .await
        .reset(
            1,
            Seed::new(41),
            vec![FaultOutcome::Normal, FaultOutcome::Normal],
        )
        .expect("both business requests have one provider outcome");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("the app connects");
        serve_managed_http1_connection(stream, server_fixture)
            .await
            .expect("both provider requests are served on the pooled connection");
    });
    let operation = CheckoutOperation::new("op_1", 2_500, "usd").expect("the operation is valid");
    let client = reqwest::Client::new();

    let first = create_with_changed_retry_key_for_business_request(
        &client,
        &format!("http://{address}"),
        &operation,
        1,
    )
    .await
    .expect("the first business request succeeds");
    let second = create_with_changed_retry_key_for_business_request(
        &client,
        &format!("http://{address}"),
        &operation,
        2,
    )
    .await
    .expect("the caller retry is a distinct business request");

    assert_ne!(first.id(), second.id());
    assert_eq!(fixture.lock().await.snapshot().payment_intents().len(), 2);
    drop(client);
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the provider server stops")
        .expect("the provider server does not panic");
}

fn compiled_changed_key_retry_script() -> (Seed, ProviderOutcomeScript) {
    for raw_seed in 0..512 {
        let seed = Seed::new(raw_seed);
        let plan = CasePlanCompiler::compile(&PlanSpec::payment_intent_v1(
            seed,
            ActionBudget::new(40).unwrap(),
        ))
        .expect("the v1 plan is feasible");
        for action in plan.actions() {
            let (PlanActionKind::DriveCheckout { provider_script }
            | PlanActionKind::RetryBusinessRequest { provider_script }) = action.kind()
            else {
                continue;
            };
            let script = *provider_script;
            if script.outcomes().collect::<Vec<_>>()
                == [ProviderOutcome::CommitThenClose, ProviderOutcome::Normal]
            {
                return (seed, script);
            }
        }
    }
    panic!("the deterministic seed corpus must compile the changed-key retry script");
}

const fn fixture_outcome(outcome: ProviderOutcome) -> FaultOutcome {
    match outcome {
        ProviderOutcome::Normal => FaultOutcome::Normal,
        ProviderOutcome::PreExecute429 => FaultOutcome::PreExecute429,
        ProviderOutcome::PreExecute500 => FaultOutcome::PreExecute500,
        ProviderOutcome::PostExecute500 => FaultOutcome::PostExecute500,
        ProviderOutcome::CommitThenClose => FaultOutcome::CommitThenClose,
        ProviderOutcome::CommitThenDelay => FaultOutcome::CommitThenDelay,
    }
}

fn signature_header(timestamp: i64, body: &[u8], secret: &[u8]) -> String {
    let mut signer = Hmac::<Sha256>::new_from_slice(secret).expect("the test secret is valid");
    signer.update(timestamp.to_string().as_bytes());
    signer.update(b".");
    signer.update(body);
    format!(
        "t={timestamp},v1={}",
        hex::encode(signer.finalize().into_bytes())
    )
}

fn current_unix_timestamp() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the system clock is after the Unix epoch")
            .as_secs(),
    )
    .expect("the current Unix timestamp fits in i64")
}
