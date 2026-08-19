use std::{
    convert::Infallible,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use hmac::{Hmac, KeyInit as _, Mac as _};
use http_body_util::{BodyExt as _, Full};
use hyper::{
    Method, Request, Response, StatusCode, body::Incoming, server::conn::http1, service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde_json::json;
use sha2::Sha256;
use tiv_core::decision::Seed;
use tiv_stripe_pi::{
    CreatePaymentIntent, FaultOutcome, IdempotencyKey, ManagedFixture, OperationId,
    control::{ControlToken, WebhookSigningSecret, WebhookTarget, serve_http1_connection},
};
use tokio::{net::TcpListener, sync::Mutex, sync::mpsc, task::JoinHandle, time::timeout};

#[test]
fn webhook_target_rejects_public_and_ambiguous_destinations() {
    for target in [
        "https://reference-app:18080/webhooks/stripe",
        "http://example.com/webhooks/stripe",
        "http://192.0.2.1:18080/webhooks/stripe",
        "http://user@reference-app:18080/webhooks/stripe",
        "http://reference-app:18080/webhooks/stripe?next=public",
        "http://reference-app:18080/#fragment",
        "http://reference-app:18080/",
    ] {
        assert!(WebhookTarget::new(target, Duration::from_secs(1)).is_err());
    }
    assert!(
        WebhookTarget::new("http://reference-app:18080/webhooks/stripe", Duration::ZERO,).is_err()
    );
    assert!(
        WebhookTarget::new(
            "http://reference-app:18080/webhooks/stripe",
            Duration::from_secs(31),
        )
        .is_err()
    );
    assert!(
        WebhookTarget::new(
            "http://reference-app:18080/webhooks/stripe",
            Duration::from_secs(1),
        )
        .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_trace_bound_provider_event_is_signed_and_delivered_by_the_fixture() {
    let (target_address, mut deliveries, target_server) = start_webhook_target().await;
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(73))));
    fixture
        .lock()
        .await
        .reset(1, Seed::new(73), vec![FaultOutcome::Normal])
        .unwrap();
    fixture
        .lock()
        .await
        .create_data_plane(
            IdempotencyKey::new("op-73-attempt-1").unwrap(),
            CreatePaymentIntent::new(2_500, "usd")
                .unwrap()
                .with_operation_id(OperationId::new("op_73").unwrap()),
        )
        .unwrap();
    let payment_intent_id = fixture.lock().await.snapshot().payment_intents()[0]
        .id()
        .to_owned();
    let (control_address, control_server) =
        start_control_server(Arc::clone(&fixture), target_address).await;
    let client = reqwest::Client::new();
    let timestamp = current_timestamp();

    let generated = client
        .post(format!(
            "http://{control_address}/v1/control/generate-event"
        ))
        .header("X-Tiv-Control-Token", "run-scoped-control-token")
        .json(&json!({
            "command_sequence": 2,
            "payment_intent_id": payment_intent_id,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(generated.status(), StatusCode::OK);
    let generated: serde_json::Value = generated.json().await.unwrap();
    let event_id = generated["event_id"].as_str().unwrap().to_owned();
    assert!(event_id.starts_with("evt_tiv_"));
    assert_eq!(generated["payment_intent_id"], payment_intent_id);
    assert_eq!(generated["command_sequence"], 2);

    let delivered = client
        .post(format!("http://{control_address}/v1/control/deliver-event"))
        .header("X-Tiv-Control-Token", "run-scoped-control-token")
        .json(&json!({
            "command_sequence": 3,
            "event_id": event_id,
            "timestamp": timestamp,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(delivered.status(), StatusCode::OK);
    let delivered: serde_json::Value = delivered.json().await.unwrap();
    assert_eq!(delivered["command_sequence"], 3);
    assert_eq!(delivered["event_id"], event_id);
    assert_eq!(delivered["timestamp"], timestamp);
    assert_eq!(delivered["status"], 200);

    let observed = timeout(Duration::from_secs(2), deliveries.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed.event_id(), event_id);
    assert_eq!(observed.payment_intent_id(), payment_intent_id);
    assert_eq!(observed.operation_id(), "op_73");
    observed.verify_signature(b"whsec_case_test", timestamp);

    drop(client);
    control_server.abort();
    let _ = control_server.await;
    target_server.await.unwrap();
}

struct ObservedDelivery {
    raw_body: Vec<u8>,
    signature: String,
}

impl ObservedDelivery {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.raw_body).unwrap()
    }

    fn event_id(&self) -> String {
        self.json()["id"].as_str().unwrap().to_owned()
    }

    fn payment_intent_id(&self) -> String {
        self.json()["data"]["object"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn operation_id(&self) -> String {
        self.json()["data"]["object"]["metadata"]["operation_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn verify_signature(&self, secret: &[u8], timestamp: i64) {
        let signature = self
            .signature
            .strip_prefix(&format!("t={timestamp},v1="))
            .unwrap();
        let mut signer = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        signer.update(timestamp.to_string().as_bytes());
        signer.update(b".");
        signer.update(&self.raw_body);
        assert_eq!(signature, hex::encode(signer.finalize().into_bytes()));
    }
}

async fn start_webhook_target() -> (
    std::net::SocketAddr,
    mpsc::Receiver<ObservedDelivery>,
    JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel(1);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        http1::Builder::new()
            .serve_connection(
                TokioIo::new(stream),
                service_fn(move |request| record_delivery(request, sender.clone())),
            )
            .await
            .unwrap();
    });
    (address, receiver, server)
}

async fn record_delivery(
    request: Request<Incoming>,
    sender: mpsc::Sender<ObservedDelivery>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.uri().path(), "/webhooks/stripe");
    let signature = request
        .headers()
        .get("stripe-signature")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let raw_body = request
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    sender
        .send(ObservedDelivery {
            raw_body,
            signature,
        })
        .await
        .unwrap();
    Ok(Response::new(Full::new(Bytes::from_static(b"accepted"))))
}

async fn start_control_server(
    fixture: Arc<Mutex<ManagedFixture>>,
    target_address: std::net::SocketAddr,
) -> (std::net::SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let target = WebhookTarget::new(
        format!("http://{target_address}/webhooks/stripe"),
        Duration::from_secs(2),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fixture = Arc::clone(&fixture);
            let target = target.clone();
            tokio::spawn(async move {
                let _result = serve_http1_connection(
                    stream,
                    fixture,
                    ControlToken::new("run-scoped-control-token").unwrap(),
                    WebhookSigningSecret::new(b"whsec_case_test").unwrap(),
                    target,
                )
                .await;
            });
        }
    });
    (address, server)
}

fn current_timestamp() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}
