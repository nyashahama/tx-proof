use std::{sync::Arc, time::Duration};

use reqwest::StatusCode;
use serde_json::json;
use tiv_core::decision::Seed;
use tiv_stripe_pi::{
    CreatePaymentIntent, FaultOutcome, FixtureServiceError, IdempotencyKey,
    ManagedDataPlaneDisposition, ManagedFixture, OperationId,
    control::{ControlToken, WebhookSigningSecret, serve_http1_connection},
    http::serve_managed_http1_connection,
};
use tokio::{net::TcpListener, sync::Mutex, time::timeout};

#[tokio::test]
async fn commit_then_delay_is_observable_until_the_exact_gate_is_released() {
    let mut fixture = ManagedFixture::new(Seed::new(42));
    fixture
        .reset(1, Seed::new(42), vec![FaultOutcome::CommitThenDelay])
        .expect("the delay plan is installed");

    let held = fixture
        .create_data_plane(
            IdempotencyKey::new("op-1-attempt-1").expect("the key is valid"),
            valid_create(),
        )
        .expect("the create commits before it is held");
    let ManagedDataPlaneDisposition::Held(held) = held else {
        panic!("commit-then-delay must return an observable held response");
    };
    let gate_id = held.gate_id();

    assert_eq!(fixture.snapshot().payment_intents().len(), 1);
    assert_eq!(fixture.snapshot().held_gates()[0].gate_id(), gate_id);
    assert_eq!(
        fixture.release_gate(1, gate_id),
        Err(FixtureServiceError::UnexpectedCommandSequence {
            expected: 2,
            received: 1,
        })
    );

    let released = fixture
        .release_gate(2, gate_id)
        .expect("the exact next command releases the gate");
    let response = held
        .wait()
        .await
        .expect("the exact release wakes the held response");

    assert_eq!(released.command_sequence(), 2);
    assert!(released.held_gates().is_empty());
    assert_eq!(response.status_code(), 200);
    assert!(fixture.snapshot().held_gates().is_empty());
}

#[tokio::test]
async fn reset_cancels_every_response_held_by_the_previous_fixture_state() {
    let mut fixture = ManagedFixture::new(Seed::new(42));
    fixture
        .reset(1, Seed::new(42), vec![FaultOutcome::CommitThenDelay])
        .expect("the delay plan is installed");
    let held = fixture
        .create_data_plane(
            IdempotencyKey::new("op-1-attempt-1").expect("the key is valid"),
            valid_create(),
        )
        .expect("the create commits before it is held");
    let ManagedDataPlaneDisposition::Held(held) = held else {
        panic!("commit-then-delay must return an observable held response");
    };

    fixture
        .reset(2, Seed::new(43), vec![FaultOutcome::Normal])
        .expect("the next reset replaces fixture state");
    let cancellation = timeout(Duration::from_millis(100), held.wait())
        .await
        .expect("reset must wake the held data task")
        .expect_err("reset must not release an obsolete response");

    assert_eq!(cancellation.to_string(), "held response was cancelled");
    assert!(fixture.snapshot().held_gates().is_empty());
}

#[test]
fn held_control_state_matches_the_v1_golden_protocol() {
    let mut fixture = ManagedFixture::new(Seed::new(42));
    fixture
        .reset(1, Seed::new(42), vec![FaultOutcome::CommitThenDelay])
        .expect("the delay plan is installed");
    let disposition = fixture
        .create_data_plane(
            IdempotencyKey::new("op-1-attempt-1").expect("the key is valid"),
            valid_create(),
        )
        .expect("the create commits before it is held");
    assert!(matches!(disposition, ManagedDataPlaneDisposition::Held(_)));

    let encoded =
        serde_json::to_string(&fixture.snapshot()).expect("the closed control DTO is serializable");
    assert_eq!(
        encoded,
        include_str!("../../../tests/golden/stripe-fixture-state-v1.json").trim_end()
    );
}

#[test]
fn mutating_control_commands_are_strictly_sequenced() {
    let mut fixture = ManagedFixture::new(Seed::new(42));

    let reset = fixture
        .reset(
            1,
            Seed::new(7),
            vec![FaultOutcome::CommitThenClose, FaultOutcome::Normal],
        )
        .expect("the first command is accepted");
    let stale = fixture
        .reset(1, Seed::new(8), vec![FaultOutcome::Normal])
        .expect_err("a repeated sequence cannot mutate state");

    assert_eq!(reset.command_sequence(), 1);
    assert_eq!(reset.payment_intents().len(), 0);
    assert_eq!(reset.remaining_outcomes(), 2);
    assert_eq!(
        stale,
        FixtureServiceError::UnexpectedCommandSequence {
            expected: 2,
            received: 1,
        }
    );
}

#[test]
fn the_fault_plan_is_consumed_only_by_valid_provider_creates() {
    let mut fixture = ManagedFixture::new(Seed::new(42));
    fixture
        .reset(
            1,
            Seed::new(42),
            vec![FaultOutcome::CommitThenClose, FaultOutcome::Normal],
        )
        .expect("the plan is installed");
    let create = valid_create();

    let first = fixture
        .create_data_plane(
            IdempotencyKey::new("op-1-attempt-1").expect("the key is valid"),
            create.clone(),
        )
        .expect("the first planned action executes");
    let second = fixture
        .create_data_plane(
            IdempotencyKey::new("op-1-attempt-2").expect("the key is valid"),
            create,
        )
        .expect("the second planned action executes");

    assert!(matches!(
        first,
        ManagedDataPlaneDisposition::CloseConnection
    ));
    assert!(matches!(second, ManagedDataPlaneDisposition::Response(_)));
    assert_eq!(fixture.snapshot().payment_intents().len(), 2);
    assert_eq!(fixture.snapshot().remaining_outcomes(), 0);
}

#[test]
fn confirming_all_exports_lossless_signed_attempts() {
    let mut fixture = ManagedFixture::new(Seed::new(42));
    fixture
        .reset(1, Seed::new(42), vec![FaultOutcome::Normal])
        .expect("the plan is installed");
    fixture
        .create_data_plane(
            IdempotencyKey::new("op-1-attempt-1").expect("the key is valid"),
            valid_create(),
        )
        .expect("the provider object is created");
    let secret = WebhookSigningSecret::new("whsec_test_secret")
        .expect("the synthetic webhook secret is valid");

    let confirmation = fixture
        .confirm_all(2, 1_700_000_000, &secret)
        .expect("the second sequenced command confirms every object");
    let attempt = confirmation
        .attempts()
        .first()
        .expect("one provider event is exported");
    let raw_body = hex::decode(attempt.raw_body_hex()).expect("the transport is lossless hex");
    let raw_json: serde_json::Value =
        serde_json::from_slice(&raw_body).expect("the exact webhook bytes are JSON");

    assert_eq!(confirmation.command_sequence(), 2);
    assert_eq!(confirmation.attempts().len(), 1);
    assert_eq!(
        raw_json["data"]["object"]["metadata"]["operation_id"],
        "op_1"
    );
    assert!(attempt.signature_header().starts_with("t=1700000000,v1="));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_control_listener_requires_its_run_scoped_token() {
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(42))));
    let token = ControlToken::new("run-scoped-control-token")
        .expect("the synthetic control token is valid");
    let secret = WebhookSigningSecret::new("whsec_test_secret")
        .expect("the synthetic webhook secret is valid");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("a client connects");
        serve_http1_connection(stream, server_fixture, token, secret)
            .await
            .expect("the control connection is served");
    });
    let client = reqwest::Client::new();
    let endpoint = format!("http://{address}/v1/control/reset");

    let unauthorized = client
        .post(&endpoint)
        .json(&json!({
            "command_sequence": 1,
            "seed": 7,
            "outcomes": ["commit_then_close", "normal"]
        }))
        .send()
        .await
        .expect("the rejection is an HTTP response");
    let unauthorized_status = unauthorized.status();
    let _unauthorized_body = unauthorized
        .bytes()
        .await
        .expect("the rejection body is readable");
    let authorized = client
        .post(endpoint)
        .header("X-Tiv-Control-Token", "run-scoped-control-token")
        .json(&json!({
            "command_sequence": 1,
            "seed": 7,
            "outcomes": ["commit_then_close", "normal"]
        }))
        .send()
        .await
        .expect("the command is an HTTP response");

    assert_eq!(unauthorized_status, StatusCode::UNAUTHORIZED);
    assert_eq!(authorized.status(), StatusCode::OK);
    let state: serde_json::Value = authorized.json().await.expect("the state is JSON");
    assert_eq!(state["command_sequence"], 1);
    assert_eq!(state["remaining_outcomes"], 2);
    drop(client);
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the control connection stops")
        .expect("the server task does not panic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_data_response_waits_for_its_observed_control_gate() {
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(42))));
    fixture
        .lock()
        .await
        .reset(1, Seed::new(42), vec![FaultOutcome::CommitThenDelay])
        .expect("the delay plan is installed");
    let token = ControlToken::new("run-scoped-control-token")
        .expect("the synthetic control token is valid");
    let secret = WebhookSigningSecret::new("whsec_test_secret")
        .expect("the synthetic webhook secret is valid");
    let data_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a data port is available");
    let data_address = data_listener
        .local_addr()
        .expect("the data listener has an address");
    let control_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a control port is available");
    let control_address = control_listener
        .local_addr()
        .expect("the control listener has an address");

    let data_fixture = Arc::clone(&fixture);
    let data_server = tokio::spawn(async move {
        let (stream, _) = data_listener.accept().await.expect("a client connects");
        serve_managed_http1_connection(stream, data_fixture)
            .await
            .expect("the held data connection is served");
    });
    let control_fixture = Arc::clone(&fixture);
    let control_server = tokio::spawn(async move {
        let (stream, _) = control_listener
            .accept()
            .await
            .expect("a control client connects");
        serve_http1_connection(stream, control_fixture, token, secret)
            .await
            .expect("the control connection is served");
    });

    let data_request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://{data_address}/v1/payment_intents"))
            .header("Idempotency-Key", "op-1-attempt-1")
            .form(&[
                ("amount", "2500"),
                ("currency", "usd"),
                ("metadata[operation_id]", "op_1"),
            ])
            .send()
            .await
    });
    let control_client = reqwest::Client::new();
    let control_base = format!("http://{control_address}/v1/control");
    let gate_id = timeout(Duration::from_secs(2), async {
        loop {
            let state: serde_json::Value = control_client
                .get(format!("{control_base}/state"))
                .header("X-Tiv-Control-Token", "run-scoped-control-token")
                .send()
                .await
                .expect("control state is available")
                .json()
                .await
                .expect("control state is JSON");
            if let Some(gate_id) = state["held_gates"][0]["gate_id"].as_u64() {
                break gate_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the committed response reaches an observable gate");
    assert!(
        !data_request.is_finished(),
        "the response cannot escape before the gate is released"
    );

    let released: serde_json::Value = control_client
        .post(format!("{control_base}/release-gate"))
        .header("X-Tiv-Control-Token", "run-scoped-control-token")
        .json(&json!({"command_sequence": 2, "gate_id": gate_id}))
        .send()
        .await
        .expect("the release command returns an HTTP response")
        .error_for_status()
        .expect("the release command succeeds")
        .json()
        .await
        .expect("the released state is JSON");
    let response = timeout(Duration::from_secs(2), data_request)
        .await
        .expect("the released response arrives")
        .expect("the data task does not panic")
        .expect("the response has valid HTTP framing");

    assert_eq!(released["command_sequence"], 2);
    assert_eq!(released["held_gates"], json!([]));
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    drop(control_client);
    join_server(data_server).await;
    join_server(control_server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_data_plane_requests_do_not_consume_the_fault_plan() {
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(42))));
    fixture
        .lock()
        .await
        .reset(
            1,
            Seed::new(42),
            vec![FaultOutcome::CommitThenClose, FaultOutcome::Normal],
        )
        .expect("the fault plan is installed");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server_fixture = Arc::clone(&fixture);
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.expect("a client connects");
            let _result = serve_managed_http1_connection(stream, Arc::clone(&server_fixture)).await;
        }
    });
    let endpoint = format!("http://{address}/v1/payment_intents");

    let invalid = reqwest::Client::new()
        .get(format!("http://{address}/health"))
        .header("Connection", "close")
        .send()
        .await
        .expect("the invalid request receives a response");
    assert_eq!(invalid.status(), StatusCode::NOT_FOUND);
    let first = reqwest::Client::new()
        .post(&endpoint)
        .header("Connection", "close")
        .header("Idempotency-Key", "op-1-attempt-1")
        .form(&[
            ("amount", "2500"),
            ("currency", "usd"),
            ("metadata[operation_id]", "op_1"),
        ])
        .send()
        .await;
    assert!(first.is_err(), "the first valid create commits then closes");
    let retry = reqwest::Client::new()
        .post(endpoint)
        .header("Connection", "close")
        .header("Idempotency-Key", "op-1-attempt-2")
        .form(&[
            ("amount", "2500"),
            ("currency", "usd"),
            ("metadata[operation_id]", "op_1"),
        ])
        .send()
        .await
        .expect("the second valid create receives a response");
    assert_eq!(retry.status(), StatusCode::OK);

    timeout(Duration::from_secs(2), server)
        .await
        .expect("the three-connection data server stops")
        .expect("the data server task does not panic");
    let snapshot = fixture.lock().await.snapshot();
    assert_eq!(snapshot.remaining_outcomes(), 0);
    assert_eq!(snapshot.payment_intents().len(), 2);
}

fn valid_create() -> CreatePaymentIntent {
    CreatePaymentIntent::new(2_500, "usd")
        .expect("the create is valid")
        .with_operation_id(OperationId::new("op_1").expect("the operation ID is valid"))
}

async fn join_server(server: tokio::task::JoinHandle<()>) {
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the fixture connection stops")
        .expect("the fixture server does not panic");
}
