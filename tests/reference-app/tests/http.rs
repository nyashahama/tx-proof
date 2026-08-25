use std::{
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use reqwest::StatusCode;
use serde_json::json;
use tiv_core::decision::Seed;
use tiv_reference_app::{
    CallerRetryMode, ReferenceApp, ReferenceAppConfig, RetryKeyMode, WebhookEffectMode,
    serve_http1_connection,
};
use tiv_stripe_pi::{
    CreatePaymentIntent, FaultOutcome, IdempotencyKey, ManagedFixture, OperationId,
    http::serve_managed_http1_connection,
};
use tokio::{net::TcpListener, sync::Mutex, time::timeout};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_and_control_isolation_probe_do_not_touch_postgres() {
    let app = Arc::new(test_app("127.0.0.1:1"));
    let (address, server) = serve_connections(Arc::clone(&app), 2).await;

    let health = reqwest::Client::new()
        .get(format!("http://{address}/health"))
        .header("Connection", "close")
        .send()
        .await
        .expect("health is an HTTP response");
    let probe = reqwest::Client::new()
        .get(format!("http://{address}/probe-fixture-control"))
        .header("Connection", "close")
        .send()
        .await
        .expect("the isolation probe is an HTTP response");

    assert_eq!(health.status(), StatusCode::OK);
    let health: serde_json::Value = health.json().await.expect("health is JSON");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["retry_key_mode"], "faulty_changed_key");
    assert_eq!(health["webhook_effect_mode"], "repaired_deduplicate");
    assert_eq!(health["ledger_balance_mode"], "repaired_balanced_once");
    assert_eq!(health["caller_retry_mode"], "faulty_per_request");
    assert_eq!(health["reconciliation_mode"], "faulty_webhook_only");
    assert_eq!(health["terminal_state_mode"], "faulty_arrival_order");
    assert_eq!(probe.status(), StatusCode::OK);
    let probe: serde_json::Value = probe.json().await.expect("the probe is JSON");
    assert_eq!(probe["reachable"], false);
    await_server(server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_the_process_selected_repaired_retry_key_mode() {
    let app = Arc::new(test_app_with_modes(
        RetryKeyMode::RepairedSameKey,
        WebhookEffectMode::FaultyDuplicateEffect,
    ));
    let (address, server) = serve_connections(app, 1).await;

    let health = reqwest::Client::new()
        .get(format!("http://{address}/health"))
        .header("Connection", "close")
        .send()
        .await
        .expect("health is an HTTP response");

    assert_eq!(health.status(), StatusCode::OK);
    let health: serde_json::Value = health.json().await.expect("health is JSON");
    assert_eq!(health["status"], "ok");
    assert_eq!(health["retry_key_mode"], "repaired_same_key");
    assert_eq!(health["caller_retry_mode"], "faulty_per_request");
    assert_eq!(health["reconciliation_mode"], "faulty_webhook_only");
    assert_eq!(health["terminal_state_mode"], "faulty_arrival_order");
    assert_eq!(health["webhook_effect_mode"], "faulty_duplicate_effect");
    assert_eq!(health["ledger_balance_mode"], "repaired_balanced_once");
    await_server(server).await;
}

#[tokio::test]
async fn process_startup_selects_the_faulty_ledger_balance_mode() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut child = Command::new(env!("CARGO_BIN_EXE_tiv-reference-app"))
        .env("TIV_REFERENCE_APP_BIND", address.to_string())
        .env("TIV_FIXTURE_BASE_URL", "http://127.0.0.1:1")
        .env("TIV_FIXTURE_CONTROL_PROBE", "127.0.0.1:1")
        .env("TIV_POSTGRES_HOST", "127.0.0.1")
        .env("TIV_POSTGRES_PASSWORD", "synthetic-app-password")
        .env("TIV_POSTGRES_PORT", "1")
        .env("TIV_POSTGRES_ROLE", "tiv_app")
        .env("TIV_REFERENCE_APP_RETRY_KEY_MODE", "repaired_same_key")
        .env(
            "TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE",
            "repaired_deduplicate",
        )
        .env(
            "TIV_REFERENCE_APP_LEDGER_MODE",
            "faulty_one_sided_duplicate",
        )
        .env("TIV_WEBHOOK_SECRET", "whsec_test_secret")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the reference-app binary starts");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut observed_mode = None;
    for _ in 0..100 {
        if let Ok(response) = client.get(format!("http://{address}/health")).send().await
            && response.status() == StatusCode::OK
        {
            let health: serde_json::Value = response.json().await.unwrap();
            observed_mode = health["ledger_balance_mode"].as_str().map(str::to_owned);
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("the reference-app process is reaped");

    assert_eq!(
        observed_mode.as_deref(),
        Some("faulty_one_sided_duplicate"),
        "startup stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn process_startup_selects_the_repaired_caller_retry_mode() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut child = Command::new(env!("CARGO_BIN_EXE_tiv-reference-app"))
        .env("TIV_REFERENCE_APP_BIND", address.to_string())
        .env("TIV_FIXTURE_BASE_URL", "http://127.0.0.1:1")
        .env("TIV_FIXTURE_CONTROL_PROBE", "127.0.0.1:1")
        .env("TIV_POSTGRES_HOST", "127.0.0.1")
        .env("TIV_POSTGRES_PASSWORD", "synthetic-app-password")
        .env("TIV_POSTGRES_PORT", "1")
        .env("TIV_POSTGRES_ROLE", "tiv_app")
        .env("TIV_REFERENCE_APP_RETRY_KEY_MODE", "repaired_same_key")
        .env(
            "TIV_REFERENCE_APP_CALLER_RETRY_MODE",
            "repaired_recover_operation",
        )
        .env(
            "TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE",
            "repaired_deduplicate",
        )
        .env("TIV_REFERENCE_APP_LEDGER_MODE", "repaired_balanced_once")
        .env("TIV_WEBHOOK_SECRET", "whsec_test_secret")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the reference-app binary starts");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut observed_mode = None;
    for _ in 0..100 {
        if let Ok(response) = client.get(format!("http://{address}/health")).send().await
            && response.status() == StatusCode::OK
        {
            let health: serde_json::Value = response.json().await.unwrap();
            observed_mode = health["caller_retry_mode"].as_str().map(str::to_owned);
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("the reference-app process is reaped");

    assert_eq!(
        observed_mode.as_deref(),
        Some("repaired_recover_operation"),
        "startup stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn process_startup_selects_the_repaired_reconciliation_mode() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut child = Command::new(env!("CARGO_BIN_EXE_tiv-reference-app"))
        .env("TIV_REFERENCE_APP_BIND", address.to_string())
        .env("TIV_FIXTURE_BASE_URL", "http://127.0.0.1:1")
        .env("TIV_FIXTURE_CONTROL_PROBE", "127.0.0.1:1")
        .env("TIV_POSTGRES_HOST", "127.0.0.1")
        .env("TIV_POSTGRES_PASSWORD", "synthetic-app-password")
        .env("TIV_POSTGRES_PORT", "1")
        .env("TIV_POSTGRES_ROLE", "tiv_app")
        .env("TIV_REFERENCE_APP_RETRY_KEY_MODE", "repaired_same_key")
        .env("TIV_REFERENCE_APP_CALLER_RETRY_MODE", "faulty_per_request")
        .env(
            "TIV_REFERENCE_APP_RECONCILIATION_MODE",
            "repaired_provider_reconcile",
        )
        .env(
            "TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE",
            "repaired_deduplicate",
        )
        .env("TIV_REFERENCE_APP_LEDGER_MODE", "repaired_balanced_once")
        .env("TIV_WEBHOOK_SECRET", "whsec_test_secret")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the reference-app binary starts");
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut observed_mode = None;
    for _ in 0..100 {
        if let Ok(response) = client.get(format!("http://{address}/health")).send().await
            && response.status() == StatusCode::OK
        {
            let health: serde_json::Value = response.json().await.unwrap();
            observed_mode = health["reconciliation_mode"].as_str().map(str::to_owned);
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("the reference-app process is reaped");

    assert_eq!(
        observed_mode.as_deref(),
        Some("repaired_provider_reconcile"),
        "startup stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn process_startup_rejects_overlapping_effect_and_ledger_fault_modes() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut child = Command::new(env!("CARGO_BIN_EXE_tiv-reference-app"))
        .env("TIV_REFERENCE_APP_BIND", address.to_string())
        .env("TIV_FIXTURE_BASE_URL", "http://127.0.0.1:1")
        .env("TIV_FIXTURE_CONTROL_PROBE", "127.0.0.1:1")
        .env("TIV_POSTGRES_HOST", "127.0.0.1")
        .env("TIV_POSTGRES_PASSWORD", "synthetic-app-password")
        .env("TIV_POSTGRES_PORT", "1")
        .env("TIV_POSTGRES_ROLE", "tiv_app")
        .env("TIV_REFERENCE_APP_RETRY_KEY_MODE", "repaired_same_key")
        .env(
            "TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE",
            "faulty_duplicate_effect",
        )
        .env(
            "TIV_REFERENCE_APP_LEDGER_MODE",
            "faulty_one_sided_duplicate",
        )
        .env("TIV_WEBHOOK_SECRET", "whsec_test_secret")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the reference-app binary starts");
    let mut exit = None;
    for _ in 0..100 {
        if let Some(status) = child.try_wait().unwrap() {
            exit = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if exit.is_none() {
        let _ = child.kill();
    }
    let output = child
        .wait_with_output()
        .expect("the reference-app process is reaped");

    assert!(
        exit.is_some_and(|status| !status.success()),
        "the overlapping faults must fail startup; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsafe_checkout_target_and_unsigned_webhook_fail_before_side_effects() {
    let app = Arc::new(test_app("127.0.0.1:1"));
    let (address, server) = serve_connections(app, 2).await;

    let checkout = reqwest::Client::new()
        .post(format!("http://{address}/checkout"))
        .header("Connection", "close")
        .json(&json!({
            "database": "postgres",
            "operation_id": "op_1",
            "amount_minor": 2500,
            "currency": "usd"
        }))
        .send()
        .await
        .expect("the validation failure is an HTTP response");
    let webhook = reqwest::Client::new()
        .post(format!("http://{address}/webhooks/stripe"))
        .header("Connection", "close")
        .header("Stripe-Signature", "t=1700000000,v1=00")
        .body(br#"{"type":"payment_intent.succeeded"}"#.to_vec())
        .send()
        .await
        .expect("the signature failure is an HTTP response");

    assert_eq!(checkout.status(), StatusCode::BAD_REQUEST);
    assert_eq!(webhook.status(), StatusCode::UNAUTHORIZED);
    await_server(server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkout_rejects_an_operation_owned_by_another_generated_case() {
    let app = Arc::new(test_app("127.0.0.1:1"));
    let (address, server) = serve_connections(app, 1).await;

    let checkout = reqwest::Client::new()
        .post(format!("http://{address}/checkout"))
        .header("Connection", "close")
        .json(&json!({
            "database": "tiv_case_0123456789abcdef",
            "operation_id": "op_fedcba9876543210",
            "amount_minor": 2500,
            "currency": "usd"
        }))
        .send()
        .await
        .expect("the validation failure is an HTTP response");

    assert_eq!(checkout.status(), StatusCode::BAD_REQUEST);
    await_server(server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_confirmation_proxy_preserves_the_internal_fixture_response() {
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(19))));
    fixture
        .lock()
        .await
        .reset(
            1,
            Seed::new(19),
            vec![FaultOutcome::Normal, FaultOutcome::Normal],
        )
        .unwrap();
    fixture
        .lock()
        .await
        .create_data_plane(
            IdempotencyKey::new("op_19-attempt-1").unwrap(),
            CreatePaymentIntent::new(2_500, "usd")
                .unwrap()
                .with_operation_id(OperationId::new("op_19").unwrap()),
        )
        .unwrap();
    let payment_intent_id = fixture.lock().await.snapshot().payment_intents()[0]
        .id()
        .to_owned();
    let (fixture_address, fixture_server) = serve_fixture(Arc::clone(&fixture)).await;
    let app = Arc::new(test_app_with_fixture(
        "127.0.0.1:1",
        &format!("http://{fixture_address}"),
    ));
    let (app_address, app_server) = serve_connections(app, 1).await;

    let response = reqwest::Client::new()
        .post(format!(
            "http://{app_address}/v1/payment_intents/{payment_intent_id}/confirm"
        ))
        .header("Connection", "close")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await
        .expect("the application confirmation is an HTTP response");

    assert_eq!(response.status(), StatusCode::OK);
    let response: serde_json::Value = response.json().await.unwrap();
    assert_eq!(response["id"], payment_intent_id);
    assert_eq!(response["status"], "succeeded");
    assert_eq!(response["metadata"]["operation_id"], "op_19");
    assert_eq!(
        fixture.lock().await.snapshot().payment_intents()[0].status(),
        "succeeded"
    );
    await_server(app_server).await;
    await_server(fixture_server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_confirmation_proxy_preserves_an_internal_transport_close() {
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(23))));
    fixture
        .lock()
        .await
        .reset(
            1,
            Seed::new(23),
            vec![FaultOutcome::Normal, FaultOutcome::CommitThenClose],
        )
        .unwrap();
    fixture
        .lock()
        .await
        .create_data_plane(
            IdempotencyKey::new("op_23-attempt-1").unwrap(),
            CreatePaymentIntent::new(2_500, "usd")
                .unwrap()
                .with_operation_id(OperationId::new("op_23").unwrap()),
        )
        .unwrap();
    let payment_intent_id = fixture.lock().await.snapshot().payment_intents()[0]
        .id()
        .to_owned();
    let (fixture_address, fixture_server) = serve_fixture(Arc::clone(&fixture)).await;
    let app = Arc::new(test_app_with_fixture(
        "127.0.0.1:1",
        &format!("http://{fixture_address}"),
    ));
    let (app_address, app_server) = serve_fallible_connection(app).await;

    let result = reqwest::Client::new()
        .post(format!(
            "http://{app_address}/v1/payment_intents/{payment_intent_id}/confirm"
        ))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await;

    assert!(
        result.is_err(),
        "the proxy must not manufacture an HTTP response"
    );
    assert_eq!(
        fixture.lock().await.snapshot().payment_intents()[0].status(),
        "succeeded"
    );
    assert!(
        timeout(Duration::from_secs(2), app_server)
            .await
            .unwrap()
            .unwrap()
            .is_err(),
        "the application connection ends at the provider transport cut point"
    );
    await_server(fixture_server).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repaired_caller_retry_recovers_the_ambiguous_provider_object_before_recreating() {
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(47))));
    fixture
        .lock()
        .await
        .reset(
            1,
            Seed::new(47),
            vec![FaultOutcome::PostExecute500, FaultOutcome::Normal],
        )
        .unwrap();
    let (fixture_address, fixture_server) = serve_fixture(Arc::clone(&fixture)).await;
    let app = Arc::new(ReferenceApp::new(
        ReferenceAppConfig::new(
            format!("http://{fixture_address}"),
            "127.0.0.1",
            1,
            "tiv_app",
            "synthetic-app-password",
            "whsec_test_secret",
            "127.0.0.1:1",
        )
        .unwrap()
        .with_retry_key_mode(RetryKeyMode::RepairedSameKey)
        .with_caller_retry_mode(CallerRetryMode::RepairedRecoverOperation),
    ));
    let (app_address, app_server) = serve_connections(app, 2).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let checkout = |action_id: u32| {
        client
            .post(format!("http://{app_address}/checkout"))
            .header("Connection", "close")
            .header("X-Tiv-Action-Id", action_id)
            .json(&json!({
                "database": "tiv_case_deadbeef",
                "operation_id": "op_deadbeef",
                "amount_minor": 2500,
                "currency": "usd"
            }))
            .send()
    };

    let first = checkout(1).await.unwrap();
    let second = checkout(2).await.unwrap();

    assert_eq!(first.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(second.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let snapshot = fixture.lock().await.snapshot();
    assert_eq!(snapshot.payment_intents().len(), 1);
    assert_eq!(snapshot.remaining_outcomes(), 1);
    drop(client);
    await_server(app_server).await;
    await_server(fixture_server).await;
}

fn test_app(control_probe_address: &str) -> ReferenceApp {
    test_app_with_fixture(control_probe_address, "http://127.0.0.1:1")
}

fn test_app_with_modes(
    retry_key_mode: RetryKeyMode,
    webhook_effect_mode: WebhookEffectMode,
) -> ReferenceApp {
    ReferenceApp::new(
        ReferenceAppConfig::new(
            "http://127.0.0.1:1",
            "127.0.0.1",
            1,
            "tiv_app",
            "synthetic-app-password",
            "whsec_test_secret",
            "127.0.0.1:1",
        )
        .expect("the synthetic config is valid")
        .with_retry_key_mode(retry_key_mode)
        .with_webhook_effect_mode(webhook_effect_mode),
    )
}

fn test_app_with_fixture(control_probe_address: &str, fixture_base_url: &str) -> ReferenceApp {
    ReferenceApp::new(
        ReferenceAppConfig::new(
            fixture_base_url,
            "127.0.0.1",
            1,
            "tiv_app",
            "synthetic-app-password",
            "whsec_test_secret",
            control_probe_address,
        )
        .expect("the synthetic config is valid"),
    )
}

async fn serve_fixture(
    fixture: Arc<Mutex<ManagedFixture>>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_managed_http1_connection(stream, fixture)
            .await
            .unwrap();
    });
    (address, server)
}

async fn serve_fallible_connection(
    app: Arc<ReferenceApp>,
) -> (
    std::net::SocketAddr,
    tokio::task::JoinHandle<Result<(), hyper::Error>>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        serve_http1_connection(stream, app).await
    });
    (address, server)
}

async fn serve_connections(
    app: Arc<ReferenceApp>,
    count: usize,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port is available");
    let address = listener.local_addr().expect("the listener has an address");
    let server = tokio::spawn(async move {
        for _ in 0..count {
            let (stream, _) = listener.accept().await.expect("a client connects");
            serve_http1_connection(stream, Arc::clone(&app))
                .await
                .expect("the application connection is served");
        }
    });
    (address, server)
}

async fn await_server(server: tokio::task::JoinHandle<()>) {
    timeout(Duration::from_secs(2), server)
        .await
        .expect("the reference app server stops")
        .expect("the reference app task does not panic");
}
