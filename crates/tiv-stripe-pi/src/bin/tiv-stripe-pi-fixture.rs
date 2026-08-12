use std::{env, error::Error, sync::Arc};

use tiv_core::decision::Seed;
use tiv_stripe_pi::{
    ManagedFixture,
    control::{ControlToken, WebhookSigningSecret, serve_http1_connection as serve_control},
    http::serve_managed_http1_connection,
};
use tokio::{net::TcpListener, sync::Mutex};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::args().nth(1).as_deref() == Some("healthcheck") {
        let control_bind = required_env("TIV_FIXTURE_CONTROL_BIND")?;
        tokio::net::TcpStream::connect(control_bind).await?;
        return Ok(());
    }
    let data_bind = required_env("TIV_FIXTURE_DATA_BIND")?;
    let control_bind = required_env("TIV_FIXTURE_CONTROL_BIND")?;
    if data_bind == control_bind {
        return Err("fixture data and control listeners must be distinct".into());
    }
    let control_token = ControlToken::new(required_env("TIV_FIXTURE_CONTROL_TOKEN")?)
        .map_err(|_| "invalid fixture control token")?;
    let webhook_secret = WebhookSigningSecret::new(required_env("TIV_WEBHOOK_SECRET")?)
        .map_err(|_| "invalid webhook signing secret")?;
    let data_listener = TcpListener::bind(&data_bind).await?;
    let control_listener = TcpListener::bind(&control_bind).await?;
    let fixture = Arc::new(Mutex::new(ManagedFixture::new(Seed::new(0))));

    loop {
        tokio::select! {
            accepted = data_listener.accept() => {
                let (stream, _) = accepted?;
                let fixture = Arc::clone(&fixture);
                tokio::spawn(async move {
                    let _result = serve_managed_http1_connection(stream, fixture).await;
                });
            }
            accepted = control_listener.accept() => {
                let (stream, _) = accepted?;
                let fixture = Arc::clone(&fixture);
                let control_token = control_token.clone();
                let webhook_secret = webhook_secret.clone();
                tokio::spawn(async move {
                    let _result = serve_control(
                        stream,
                        fixture,
                        control_token,
                        webhook_secret,
                    ).await;
                });
            }
        }
    }
}

fn required_env(name: &str) -> Result<String, Box<dyn Error>> {
    let value =
        env::var(name).map_err(|_| format!("missing required environment variable {name}"))?;
    if value.trim().is_empty() {
        return Err(format!("empty required environment variable {name}").into());
    }
    Ok(value)
}
