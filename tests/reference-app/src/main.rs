use std::{env, error::Error, sync::Arc};

use tiv_reference_app::{
    ReferenceApp, ReferenceAppConfig, RetryKeyMode, WebhookEffectMode, serve_http1_connection,
};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::args().nth(1).as_deref() == Some("healthcheck") {
        let bind = required_env("TIV_REFERENCE_APP_BIND")?;
        tokio::net::TcpStream::connect(bind).await?;
        return Ok(());
    }
    let bind = required_env("TIV_REFERENCE_APP_BIND")?;
    let postgres_port = required_env("TIV_POSTGRES_PORT")?
        .parse::<u16>()
        .map_err(|_| "invalid TIV_POSTGRES_PORT")?;
    let retry_key_mode = retry_key_mode_from_env()?;
    let webhook_effect_mode = webhook_effect_mode_from_env()?;
    let config = ReferenceAppConfig::new(
        required_env("TIV_FIXTURE_BASE_URL")?,
        required_env("TIV_POSTGRES_HOST")?,
        postgres_port,
        required_env("TIV_POSTGRES_ROLE")?,
        required_env("TIV_POSTGRES_PASSWORD")?,
        required_env("TIV_WEBHOOK_SECRET")?,
        required_env("TIV_FIXTURE_CONTROL_PROBE")?,
    )?
    .with_retry_key_mode(retry_key_mode)
    .with_webhook_effect_mode(webhook_effect_mode);
    let app = Arc::new(ReferenceApp::new(config));
    let listener = TcpListener::bind(bind).await?;

    loop {
        let (stream, _) = listener.accept().await?;
        let app = Arc::clone(&app);
        tokio::spawn(async move {
            let _result = serve_http1_connection(stream, app).await;
        });
    }
}

fn retry_key_mode_from_env() -> Result<RetryKeyMode, Box<dyn Error>> {
    match env::var("TIV_REFERENCE_APP_RETRY_KEY_MODE") {
        Ok(value) => value.parse().map_err(Into::into),
        Err(env::VarError::NotPresent) => Ok(RetryKeyMode::default()),
        Err(env::VarError::NotUnicode(_)) => Err("invalid TIV_REFERENCE_APP_RETRY_KEY_MODE".into()),
    }
}

fn webhook_effect_mode_from_env() -> Result<WebhookEffectMode, Box<dyn Error>> {
    match env::var("TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE") {
        Ok(value) => value.parse().map_err(Into::into),
        Err(env::VarError::NotPresent) => Ok(WebhookEffectMode::default()),
        Err(env::VarError::NotUnicode(_)) => {
            Err("invalid TIV_REFERENCE_APP_WEBHOOK_EFFECT_MODE".into())
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
