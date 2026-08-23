use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::{self, DirBuilder, OpenOptions},
    io::Write as _,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::json;
use tiv_core::trace::{CASE_TRACE_SCHEMA_VERSION, TRACE_SCHEMA_VERSION};
use uuid::Uuid;

const SECRET_CANARY: &str = "tiv-secret-must-not-leak";

#[test]
fn inspect_emits_only_the_verified_complete_artifact_receipt() {
    let fixture = ArtifactFixture::complete();

    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args(["inspect", fixture.path.to_str().unwrap()])
        .current_dir(&fixture.root)
        .env("TIV_POSTGRES_ADMIN_PASSWORD", SECRET_CANARY)
        .output()
        .expect("artifact inspection command runs");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("inspection stdout is JSON");
    assert_eq!(
        receipt,
        json!({
            "schema_version": 1,
            "status": "complete_artifact_verified",
            "run_id": "run_inspect_deadbeef",
            "complete": true,
            "checksums_verified": true,
            "compatibility_verified": true,
            "indexed_file_count": fixture.indexed_files.len(),
        })
    );
    assert_no_secret(&output.stdout);
    assert_no_secret(&output.stderr);
}

#[test]
fn inspect_rejects_corrupt_or_partial_artifacts_without_stdout_or_secret_leakage() {
    let corrupt = ArtifactFixture::complete();
    write_private(
        &corrupt.path.join("summary.json"),
        br#"{"status":"changed","secret":"tiv-secret-must-not-leak"}"#,
    );

    assert_rejected(&corrupt);

    let corrupt_index = ArtifactFixture::complete();
    write_private(&corrupt_index.path.join("checksums.txt"), b"tampered\n");
    assert_rejected(&corrupt_index);

    let partial = ArtifactFixture::partial();
    assert_rejected(&partial);
}

fn assert_rejected(fixture: &ArtifactFixture) {
    let output = Command::new(env!("CARGO_BIN_EXE_tiv"))
        .args(["inspect", fixture.path.to_str().unwrap()])
        .current_dir(&fixture.root)
        .env("TIV_POSTGRES_ADMIN_PASSWORD", SECRET_CANARY)
        .output()
        .expect("artifact inspection command runs");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(!output.stderr.is_empty(), "{output:?}");
    assert_no_secret(&output.stdout);
    assert_no_secret(&output.stderr);
}

fn assert_no_secret(bytes: &[u8]) {
    assert!(
        !String::from_utf8_lossy(bytes).contains(SECRET_CANARY),
        "secret canary leaked"
    );
}

struct ArtifactFixture {
    root: PathBuf,
    path: PathBuf,
    indexed_files: BTreeMap<String, String>,
}

impl ArtifactFixture {
    fn complete() -> Self {
        Self::new(true)
    }

    fn partial() -> Self {
        Self::new(false)
    }

    fn new(complete: bool) -> Self {
        let root = std::env::temp_dir().join(format!("tiv-inspect-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let run_id = if complete {
            "run_inspect_deadbeef"
        } else {
            "run_inspect_partial"
        };
        let path = root.join(run_id);
        DirBuilder::new().mode(0o700).create(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();

        let compatibility = pretty_json(&compatibility_fixture());
        let summary = pretty_json(&json!({
            "status": if complete { "held" } else { "inconclusive" },
            "opaque_detail": SECRET_CANARY,
        }));
        write_private(&path.join("compatibility.json"), &compatibility);
        write_private(&path.join("summary.json"), &summary);

        let indexed_files = BTreeMap::from([
            (
                "compatibility.json".to_owned(),
                blake3::hash(&compatibility).to_hex().to_string(),
            ),
            (
                "summary.json".to_owned(),
                blake3::hash(&summary).to_hex().to_string(),
            ),
        ]);
        let mut checksums = String::new();
        for (relative, digest) in &indexed_files {
            writeln!(checksums, "{digest}  {relative}").unwrap();
        }
        write_private(&path.join("checksums.txt"), checksums.as_bytes());

        let mut manifest = json!({
            "schema_version": 1,
            "run_id": run_id,
            "complete": complete,
            "status": if complete { "complete" } else { "partial" },
            "checksums_file": "checksums.txt",
            "checksums_digest": blake3::hash(checksums.as_bytes()).to_hex().to_string(),
            "required_files": indexed_files,
            "artifact_count": indexed_files.len() + 2,
        });
        if !complete {
            manifest["failure_class"] = json!("inconclusive");
            manifest["failure_code"] = json!("case_timeout");
        }
        write_private(&path.join("manifest.json"), &pretty_json(&manifest));

        Self {
            root,
            path,
            indexed_files,
        }
    }
}

impl Drop for ArtifactFixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn write_private(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn pretty_json(value: &serde_json::Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).unwrap();
    bytes.push(b'\n');
    bytes
}

fn compatibility_fixture() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "tool": {
            "package_version": "0.0.0",
            "executable_digest": "a".repeat(64),
            "trace_schema": TRACE_SCHEMA_VERSION,
            "case_trace_schema": CASE_TRACE_SCHEMA_VERSION,
            "fixture_control_protocol": 1
        },
        "platform_os": "linux",
        "platform_arch": "x86_64",
        "config_digest": "a".repeat(64),
        "compose": {
            "version": "5.4.0",
            "services": ["postgres", "reference-app", "stripe-fixture"],
            "resolved_redacted_hash": "a".repeat(64)
        },
        "sources": [{
            "kind": "invariant",
            "id": "provider-object-unique",
            "digest": "a".repeat(64)
        }],
        "baseline": {
            "server_fingerprint": "postgres-system-id:123456789",
            "endpoint_port": 15432,
            "database_name": "tiv_base_deadbeef",
            "database_oid": 16384,
            "owner_oid": 10,
            "marker_uuid": "00000000-0000-4000-8000-000000000001",
            "compose_project": "tiv-reference-app-spike",
            "application_role": "tiv_app"
        },
        "services": [{
            "service": "reference-app",
            "compose_config_hash": "a".repeat(64),
            "image_id": format!("sha256:{}", "a".repeat(64))
        }]
    })
}
