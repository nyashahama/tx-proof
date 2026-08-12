use std::{sync::Arc, time::Duration};

use reqwest::StatusCode;
use serde_json::json;
use tiv_reference_app::{ReferenceApp, ReferenceAppConfig, serve_http1_connection};
use tokio::{net::TcpListener, time::timeout};

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

fn test_app(control_probe_address: &str) -> ReferenceApp {
    ReferenceApp::new(
        ReferenceAppConfig::new(
            "http://127.0.0.1:1",
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
