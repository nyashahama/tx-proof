use std::{env, error::Error, sync::Arc};

use tiv_reference_app::{ReferenceApp, ReferenceAppConfig, serve_http1_connection};
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
    let config = ReferenceAppConfig::new(
        required_env("TIV_FIXTURE_BASE_URL")?,
        required_env("TIV_POSTGRES_HOST")?,
        postgres_port,
        required_env("TIV_POSTGRES_ROLE")?,
        required_env("TIV_POSTGRES_PASSWORD")?,
        required_env("TIV_WEBHOOK_SECRET")?,
        required_env("TIV_FIXTURE_CONTROL_PROBE")?,
    )?;
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

fn required_env(name: &str) -> Result<String, Box<dyn Error>> {
    let value =
        env::var(name).map_err(|_| format!("missing required environment variable {name}"))?;
    if value.trim().is_empty() {
        return Err(format!("empty required environment variable {name}").into());
    }
    Ok(value)
}
