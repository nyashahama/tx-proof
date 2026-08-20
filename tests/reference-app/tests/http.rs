use std::{sync::Arc, time::Duration};

use reqwest::StatusCode;
use serde_json::json;
use tiv_core::decision::Seed;
use tiv_reference_app::{ReferenceApp, ReferenceAppConfig, serve_http1_connection};
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
    assert_eq!(probe.status(), StatusCode::OK);
    let probe: serde_json::Value = probe.json().await.expect("the probe is JSON");
    assert_eq!(probe["reachable"], false);
    await_server(server).await;
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

fn test_app(control_probe_address: &str) -> ReferenceApp {
    test_app_with_fixture(control_probe_address, "http://127.0.0.1:1")
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
