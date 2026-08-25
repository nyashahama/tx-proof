use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use uuid::Uuid;

const CONFIG_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/golden/doctor-project/tiv.toml"
);

#[test]
fn sigint_for_non_run_commands_keeps_the_operating_system_default() {
    let fixture = SignalFixture::new();
    let mut child = configured_doctor_command()
        .env("PATH", fixture.path())
        .env("TIV_SIGNAL_READY", fixture.ready())
        .process_group(0)
        .spawn()
        .expect("the doctor child starts");

    for _ in 0..500 {
        if fixture.ready().is_file() {
            break;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "doctor exited before reaching the controlled Docker boundary"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(fixture.ready().is_file());
    thread::sleep(Duration::from_millis(100));

    let group = format!("-{}", child.id());
    let signal = Command::new("kill")
        .args(["-INT", "--", &group])
        .status()
        .expect("the test sends SIGINT to the isolated process group");
    assert!(signal.success());
    let status = child.wait().expect("the interrupted child is reaped");

    assert_eq!(status.signal(), Some(2));
}

#[test]
#[ignore = "requires the local Docker Compose plugin"]
fn doctor_ignores_remote_context_emits_no_secrets_and_starts_no_containers() {
    let output = configured_doctor_command()
        .env("DOCKER_HOST", "tcp://127.0.0.1:9")
        .env("DOCKER_CONTEXT", "intentionally-remote")
        .output()
        .expect("the tiv binary executes");

    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("doctor output is UTF-8");
    for canary in ["admin-canary", "application-canary", "webhook-canary"] {
        assert!(!stdout.contains(canary), "doctor leaked {canary}");
    }
    let report: serde_json::Value =
        serde_json::from_str(&stdout).expect("doctor stdout is one JSON report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["status"], "ready");
    assert_eq!(report["mutation_authorized"], false);

    let project = report["probe_project_name"]
        .as_str()
        .expect("the report contains its isolated probe project");
    let containers = Command::new("docker")
        .args([
            "--host",
            "unix:///var/run/docker.sock",
            "ps",
            "--all",
            "--quiet",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
        ])
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .output()
        .expect("local Docker can attest the probe project");
    assert!(containers.status.success());
    assert!(
        containers.stdout.is_empty(),
        "doctor must not create containers"
    );
}

fn configured_doctor_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tiv"));
    command
        .args(["doctor", "--config", CONFIG_PATH])
        .env(
            "TIV_POSTGRES_ADMIN_URL",
            "postgresql://tiv_admin:admin-canary@127.0.0.1:15432/postgres",
        )
        .env(
            "DATABASE_URL",
            "postgresql://tiv_app:application-canary@127.0.0.1:15432/tiv_case_checkout",
        )
        .env("TIV_STRIPE_WEBHOOK_SECRET", "whsec_webhook-canary")
        .env("TIV_FIXTURE_CONTROL_TOKEN", "fixture-control-canary");
    command
}

struct SignalFixture {
    root: PathBuf,
    ready: PathBuf,
    path: std::ffi::OsString,
}

impl SignalFixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("txproof-signal-{}", Uuid::new_v4()));
        let bin = root.join("bin");
        let ready = root.join("ready");
        fs::create_dir_all(&bin).unwrap();
        let docker = bin.join("docker");
        fs::write(
            &docker,
            "#!/bin/sh\n: > \"$TIV_SIGNAL_READY\"\nexec sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
        let original = std::env::var_os("PATH").unwrap_or_default();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&original)))
                .unwrap();
        Self { root, ready, path }
    }

    fn ready(&self) -> &Path {
        &self.ready
    }

    fn path(&self) -> &std::ffi::OsStr {
        &self.path
    }
}

impl Drop for SignalFixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
