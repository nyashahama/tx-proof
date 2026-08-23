use std::{process::Stdio, time::Duration};

use reqwest::{Client, Url, redirect::Policy};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinHandle,
    time::timeout,
};

use crate::{
    config::ResolvedConfig,
    doctor::ComposeFacts,
    reference_case::{
        ReferenceProcessControl, ReferenceProcessControlError, ReferenceProcessControlFuture,
    },
    run_supervisor::{RecoverableProcess, RecoveryFuture},
};

const LOCAL_DOCKER_HOST: &str = "unix:///var/run/docker.sock";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// One exact running Compose service and its immutable local image content ID.
pub(crate) struct ConfiguredServiceImage {
    service: String,
    compose_config_hash: String,
    image_id: String,
}

impl ConfiguredServiceImage {
    pub(crate) fn service(&self) -> &str {
        &self.service
    }

    pub(crate) fn image_id(&self) -> &str {
        &self.image_id
    }

    pub(crate) fn compose_config_hash(&self) -> &str {
        &self.compose_config_hash
    }
}

/// Attests every configured execution service against the local Compose
/// project and records its immutable Docker image content ID.
pub(crate) async fn attest_configured_service_images(
    config: &ResolvedConfig,
    compose: &ComposeFacts,
) -> Result<Vec<ConfiguredServiceImage>, ConfiguredProcessError> {
    let mut services = std::iter::once(config.application_service())
        .chain(std::iter::once(config.postgres_service()))
        .chain(std::iter::once(config.stripe_service()))
        .chain(config.worker_services().iter().map(String::as_str))
        .collect::<Vec<_>>();
    services.sort_unstable();
    services.dedup();

    let mut images = Vec::with_capacity(services.len());
    for service in services {
        let config_hash = compose
            .service_config_hash(service)
            .ok_or(ConfiguredProcessError::ContainerMismatch)?;
        images.push(attest_service_image(config.compose_project(), service, config_hash).await?);
    }
    Ok(images)
}

async fn attest_service_image(
    compose_project: &str,
    service: &str,
    expected_config_hash: &str,
) -> Result<ConfiguredServiceImage, ConfiguredProcessError> {
    let project_filter = format!("label=com.docker.compose.project={compose_project}");
    let service_filter = format!("label=com.docker.compose.service={service}");
    let output = docker_output(&[
        "ps",
        "--filter",
        &project_filter,
        "--filter",
        &service_filter,
        "--format",
        "{{.ID}}",
    ])
    .await?;
    let ids = output.lines().collect::<Vec<_>>();
    let [container_id] = ids.as_slice() else {
        return Err(ConfiguredProcessError::ContainerMismatch);
    };
    validate_container_id(container_id)?;
    let inspection = docker_output(&[
        "inspect",
        "--format",
        concat!(
            "{{.State.Running}}\n",
            "{{index .Config.Labels \"com.docker.compose.project\"}}\n",
            "{{index .Config.Labels \"com.docker.compose.service\"}}\n",
            "{{index .Config.Labels \"com.docker.compose.config-hash\"}}\n",
            "{{.Image}}"
        ),
        container_id,
    ])
    .await?;
    let image_id = validate_service_image_inspection(
        &inspection,
        compose_project,
        service,
        expected_config_hash,
    )?;
    Ok(ConfiguredServiceImage {
        service: service.to_owned(),
        compose_config_hash: expected_config_hash.to_owned(),
        image_id,
    })
}

/// Exact configured application-container authority for process-fault actions.
pub(crate) struct ConfiguredProcessControl {
    container_id: String,
    compose_project: String,
    application_service: String,
    health_url: Url,
    health_timeout: Duration,
    health_client: Client,
    recovery: ProcessRecoveryState,
}

impl ConfiguredProcessControl {
    /// Attests one running application container and its loopback health port.
    pub(crate) async fn attest(config: &ResolvedConfig) -> Result<Self, ConfiguredProcessError> {
        let health_client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_millis(500))
            .timeout(Duration::from_millis(500))
            .build()
            .map_err(|_| ConfiguredProcessError::HealthClient)?;
        let mut control = Self {
            container_id: String::new(),
            compose_project: config.compose_project().to_owned(),
            application_service: config.application_service().to_owned(),
            health_url: config.health_url().clone(),
            health_timeout: config.health_timeout(),
            health_client,
            recovery: ProcessRecoveryState::default(),
        };
        control.container_id = control.attest_running_container().await?;
        if !control.http_healthy().await {
            return Err(ConfiguredProcessError::Health);
        }
        Ok(control)
    }

    async fn attest_running_container(&self) -> Result<String, ConfiguredProcessError> {
        let project_filter = format!("label=com.docker.compose.project={}", self.compose_project);
        let service_filter = format!(
            "label=com.docker.compose.service={}",
            self.application_service
        );
        let output = docker_output(&[
            "ps",
            "--filter",
            &project_filter,
            "--filter",
            &service_filter,
            "--format",
            "{{.ID}}",
        ])
        .await?;
        let ids = output.lines().collect::<Vec<_>>();
        let [container_id] = ids.as_slice() else {
            return Err(ConfiguredProcessError::ContainerMismatch);
        };
        validate_container_id(container_id)?;
        let inspection = docker_output(&[
            "inspect",
            "--format",
            concat!(
                "{{.State.Running}}\n",
                "{{if .State.Health}}{{.State.Health.Status}}{{end}}\n",
                "{{index .Config.Labels \"com.docker.compose.project\"}}\n",
                "{{index .Config.Labels \"com.docker.compose.service\"}}\n",
                "{{json .NetworkSettings.Ports}}"
            ),
            container_id,
        ])
        .await?;
        let port = self
            .health_url
            .port()
            .ok_or(ConfiguredProcessError::ContainerMismatch)?;
        validate_application_inspection(
            &inspection,
            &self.compose_project,
            &self.application_service,
            port,
        )?;
        Ok((*container_id).to_owned())
    }

    async fn http_healthy(&self) -> bool {
        self.health_client
            .get(self.health_url.clone())
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }

    async fn await_restarted(&self) -> Result<(), ConfiguredProcessError> {
        timeout(self.health_timeout, async {
            loop {
                let same_container = self
                    .attest_running_container()
                    .await
                    .is_ok_and(|observed| observed == self.container_id);
                if same_container && self.http_healthy().await {
                    return;
                }
                tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
            }
        })
        .await
        .map_err(|_| ConfiguredProcessError::Health)
    }

    async fn recover_if_needed(&mut self) -> Result<(), ConfiguredProcessError> {
        let inspection = docker_output(&[
            "inspect",
            "--format",
            concat!(
                "{{.State.Running}}\n",
                "{{index .Config.Labels \"com.docker.compose.project\"}}\n",
                "{{index .Config.Labels \"com.docker.compose.service\"}}"
            ),
            self.container_id.as_str(),
        ])
        .await?;
        let running = validate_recovery_inspection(
            &inspection,
            &self.compose_project,
            &self.application_service,
        )?;
        if !running {
            let started = docker_output(&["start", self.container_id.as_str()]).await?;
            if started.trim() != self.container_id {
                return Err(ConfiguredProcessError::Docker);
            }
        }
        self.await_restarted().await?;
        self.recovery.mark_recovered();
        Ok(())
    }
}

impl ReferenceProcessControl for ConfiguredProcessControl {
    fn kill_application(&mut self) -> ReferenceProcessControlFuture<'_> {
        Box::pin(async move {
            self.recovery.mark_required();
            let killed = docker_output(&["kill", "--signal", "KILL", self.container_id.as_str()])
                .await
                .map_err(|_| ReferenceProcessControlError::Kill)?;
            if killed.trim() != self.container_id {
                return Err(ReferenceProcessControlError::Kill);
            }
            let inspection = docker_output(&[
                "inspect",
                "--format",
                concat!(
                    "{{.State.Running}}\n",
                    "{{index .Config.Labels \"com.docker.compose.project\"}}\n",
                    "{{index .Config.Labels \"com.docker.compose.service\"}}"
                ),
                self.container_id.as_str(),
            ])
            .await
            .map_err(|_| ReferenceProcessControlError::Kill)?;
            let expected = format!(
                "false\n{}\n{}\n",
                self.compose_project, self.application_service
            );
            if inspection != expected {
                return Err(ReferenceProcessControlError::Kill);
            }
            Ok(())
        })
    }

    fn restart_and_await_health(&mut self) -> ReferenceProcessControlFuture<'_> {
        Box::pin(async move {
            let started = docker_output(&["start", self.container_id.as_str()])
                .await
                .map_err(|_| ReferenceProcessControlError::Restart)?;
            if started.trim() != self.container_id {
                return Err(ReferenceProcessControlError::Restart);
            }
            self.await_restarted()
                .await
                .map_err(|_| ReferenceProcessControlError::HealthOrAttestation)?;
            self.recovery.mark_recovered();
            Ok(())
        })
    }
}

impl RecoverableProcess for ConfiguredProcessControl {
    type Error = ConfiguredProcessError;

    fn recover(&mut self) -> RecoveryFuture<'_, Self::Error> {
        Box::pin(self.recover_if_needed())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ProcessRecoveryState {
    required: bool,
}

impl ProcessRecoveryState {
    const fn mark_required(&mut self) {
        self.required = true;
    }

    const fn mark_recovered(&mut self) {
        self.required = false;
    }

    #[cfg(test)]
    const fn is_required(self) -> bool {
        self.required
    }
}

fn validate_container_id(value: &str) -> Result<(), ConfiguredProcessError> {
    if !(12..=64).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ConfiguredProcessError::ContainerMismatch);
    }
    Ok(())
}

fn validate_application_inspection(
    inspection: &str,
    compose_project: &str,
    application_service: &str,
    health_port: u16,
) -> Result<(), ConfiguredProcessError> {
    let mut lines = inspection.lines();
    if lines.next() != Some("true")
        || lines.next() != Some("healthy")
        || lines.next() != Some(compose_project)
        || lines.next() != Some(application_service)
    {
        return Err(ConfiguredProcessError::ContainerMismatch);
    }
    let ports: Value = serde_json::from_str(
        lines
            .next()
            .ok_or(ConfiguredProcessError::ContainerMismatch)?,
    )
    .map_err(|_| ConfiguredProcessError::ContainerMismatch)?;
    if lines.next().is_some() {
        return Err(ConfiguredProcessError::ContainerMismatch);
    }
    let matches = ports
        .as_object()
        .ok_or(ConfiguredProcessError::ContainerMismatch)?
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .filter(|binding| {
            binding.get("HostIp").and_then(Value::as_str) == Some("127.0.0.1")
                && binding
                    .get("HostPort")
                    .and_then(Value::as_str)
                    .and_then(|value| value.parse::<u16>().ok())
                    == Some(health_port)
        })
        .count();
    if matches != 1 {
        return Err(ConfiguredProcessError::ContainerMismatch);
    }
    Ok(())
}

fn validate_recovery_inspection(
    inspection: &str,
    compose_project: &str,
    application_service: &str,
) -> Result<bool, ConfiguredProcessError> {
    let mut lines = inspection.lines();
    let running = match lines.next() {
        Some("true") => true,
        Some("false") => false,
        _ => return Err(ConfiguredProcessError::ContainerMismatch),
    };
    if lines.next() != Some(compose_project)
        || lines.next() != Some(application_service)
        || lines.next().is_some()
    {
        return Err(ConfiguredProcessError::ContainerMismatch);
    }
    Ok(running)
}

fn validate_service_image_inspection(
    inspection: &str,
    compose_project: &str,
    service: &str,
    expected_config_hash: &str,
) -> Result<String, ConfiguredProcessError> {
    let mut lines = inspection.lines();
    if lines.next() != Some("true")
        || lines.next() != Some(compose_project)
        || lines.next() != Some(service)
        || lines.next() != Some(expected_config_hash)
    {
        return Err(ConfiguredProcessError::ContainerMismatch);
    }
    let image_id = lines
        .next()
        .ok_or(ConfiguredProcessError::ContainerMismatch)?;
    let digest = image_id
        .strip_prefix("sha256:")
        .ok_or(ConfiguredProcessError::ContainerMismatch)?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || lines.next().is_some()
    {
        return Err(ConfiguredProcessError::ContainerMismatch);
    }
    Ok(image_id.to_owned())
}

async fn docker_output(args: &[&str]) -> Result<String, ConfiguredProcessError> {
    let mut command = Command::new("docker");
    command
        .args(["--host", LOCAL_DOCKER_HOST])
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .args(args)
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| ConfiguredProcessError::Docker)?;
    let stdout = child.stdout.take().ok_or(ConfiguredProcessError::Docker)?;
    let stderr = child.stderr.take().ok_or(ConfiguredProcessError::Docker)?;
    let stdout_reader = tokio::spawn(read_bounded(stdout));
    let stderr_reader = tokio::spawn(read_bounded(stderr));
    let status = match timeout(COMMAND_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            drop(child);
            finish_readers(stdout_reader, stderr_reader).await?;
            return Err(ConfiguredProcessError::Docker);
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = timeout(CLEANUP_TIMEOUT, child.wait()).await;
            drop(child);
            finish_readers(stdout_reader, stderr_reader).await?;
            return Err(ConfiguredProcessError::Docker);
        }
    };
    let (stdout, _stderr) = finish_readers(stdout_reader, stderr_reader).await?;
    if !status.success() {
        return Err(ConfiguredProcessError::Docker);
    }
    String::from_utf8(stdout).map_err(|_| ConfiguredProcessError::Docker)
}

async fn read_bounded(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>, ConfiguredProcessError> {
    let mut output = Vec::new();
    reader
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut output)
        .await
        .map_err(|_| ConfiguredProcessError::Docker)?;
    if output.len() as u64 > MAX_OUTPUT_BYTES {
        return Err(ConfiguredProcessError::Docker);
    }
    Ok(output)
}

async fn finish_readers(
    mut stdout: JoinHandle<Result<Vec<u8>, ConfiguredProcessError>>,
    mut stderr: JoinHandle<Result<Vec<u8>, ConfiguredProcessError>>,
) -> Result<(Vec<u8>, Vec<u8>), ConfiguredProcessError> {
    let result = timeout(CLEANUP_TIMEOUT, async {
        let stdout = (&mut stdout)
            .await
            .map_err(|_| ConfiguredProcessError::Docker)??;
        let stderr = (&mut stderr)
            .await
            .map_err(|_| ConfiguredProcessError::Docker)??;
        Ok((stdout, stderr))
    })
    .await;
    if let Ok(result) = result {
        result
    } else {
        stdout.abort();
        stderr.abort();
        let _ = stdout.await;
        let _ = stderr.await;
        Err(ConfiguredProcessError::Docker)
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfiguredProcessError {
    #[error("configured container does not match the local Compose boundary")]
    ContainerMismatch,
    #[error("configured application Docker inspection failed")]
    Docker,
    #[error("configured application health client could not be built")]
    HealthClient,
    #[error("configured application health or re-attestation failed")]
    Health,
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_INSPECTION: &str = concat!(
        "true\n",
        "healthy\n",
        "tiv-reference-app-spike\n",
        "reference-app\n",
        r#"{"18080/tcp":[{"HostIp":"127.0.0.1","HostPort":"18080"}]}"#,
        "\n",
    );

    const VALID_SERVICE_IMAGE_INSPECTION: &str = concat!(
        "true\n",
        "tiv-reference-app-spike\n",
        "reference-app\n",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n",
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
    );

    #[test]
    fn configured_application_attestation_binds_labels_health_and_loopback_port() {
        assert!(
            validate_application_inspection(
                VALID_INSPECTION,
                "tiv-reference-app-spike",
                "reference-app",
                18_080,
            )
            .is_ok()
        );

        for invalid in [
            VALID_INSPECTION.replace("true\n", "false\n"),
            VALID_INSPECTION.replace("healthy\n", "unhealthy\n"),
            VALID_INSPECTION.replace("tiv-reference-app-spike", "other-project"),
            VALID_INSPECTION.replace("reference-app\n", "other-service\n"),
            VALID_INSPECTION.replace("127.0.0.1", "0.0.0.0"),
            VALID_INSPECTION.replace("18080\"}]", "18081\"}]"),
        ] {
            assert!(
                validate_application_inspection(
                    &invalid,
                    "tiv-reference-app-spike",
                    "reference-app",
                    18_080,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn an_attempted_kill_requires_recovery_until_health_is_re_attested() {
        let mut state = ProcessRecoveryState::default();

        assert!(!state.is_required());
        state.mark_required();
        assert!(state.is_required());
        state.mark_recovered();
        assert!(!state.is_required());
    }

    #[test]
    fn configured_service_image_attestation_binds_state_labels_and_content_id() {
        assert_eq!(
            validate_service_image_inspection(
                VALID_SERVICE_IMAGE_INSPECTION,
                "tiv-reference-app-spike",
                "reference-app",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .unwrap(),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        for invalid in [
            VALID_SERVICE_IMAGE_INSPECTION.replace("true\n", "false\n"),
            VALID_SERVICE_IMAGE_INSPECTION.replace("tiv-reference-app-spike", "other-project"),
            VALID_SERVICE_IMAGE_INSPECTION.replace("reference-app\n", "other-service\n"),
            VALID_SERVICE_IMAGE_INSPECTION.replace(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            ),
            VALID_SERVICE_IMAGE_INSPECTION.replace("sha256:", "sha512:"),
            VALID_SERVICE_IMAGE_INSPECTION.replace(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "latest",
            ),
        ] {
            assert!(
                validate_service_image_inspection(
                    &invalid,
                    "tiv-reference-app-spike",
                    "reference-app",
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                )
                .is_err()
            );
        }
    }
}
