//! Bounded, secret-free Git provenance capture for run manifests.

use std::{path::Path, process::Stdio, time::Duration};

use thiserror::Error;
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

use crate::artifacts::{RepositoryProvenance, WorktreeState};

const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_COMMIT_OUTPUT_BYTES: u64 = 256;
const MAX_STATUS_OUTPUT_BYTES: u64 = 1024 * 1024;

pub(crate) async fn capture_repository_provenance(
    root: &Path,
) -> Result<RepositoryProvenance, RepositoryProvenanceError> {
    let commit_bytes = run_git(
        root,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        MAX_COMMIT_OUTPUT_BYTES,
    )
    .await?;
    let commit = std::str::from_utf8(&commit_bytes)
        .map_err(|_| RepositoryProvenanceError::InvalidOutput)?
        .trim();
    let status = run_git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
        MAX_STATUS_OUTPUT_BYTES,
    )
    .await?;
    let worktree_state = if status.is_empty() {
        WorktreeState::Clean
    } else {
        WorktreeState::Dirty
    };
    let status_digest = blake3::hash(&status).to_hex().to_string();
    RepositoryProvenance::captured(commit, worktree_state, &status_digest)
        .map_err(|_| RepositoryProvenanceError::InvalidOutput)
}

async fn run_git(
    root: &Path,
    operation: &[&str],
    maximum_output_bytes: u64,
) -> Result<Vec<u8>, RepositoryProvenanceError> {
    let executable_path = std::env::var_os("PATH").ok_or(RepositoryProvenanceError::MissingPath)?;
    let mut command = Command::new("git");
    command
        .env_clear()
        .current_dir(root)
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(operation)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .env("PATH", executable_path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("LANG", "C");
    let mut child = command.spawn().map_err(RepositoryProvenanceError::Start)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(RepositoryProvenanceError::MissingOutput)?;
    let mut bounded = stdout.take(maximum_output_bytes + 1);
    let mut bytes = Vec::new();
    let read = timeout(GIT_TIMEOUT, bounded.read_to_end(&mut bytes)).await;
    match read {
        Ok(Ok(_)) => {}
        Ok(Err(source)) => {
            terminate(&mut child).await;
            return Err(RepositoryProvenanceError::Read(source));
        }
        Err(_) => {
            terminate(&mut child).await;
            return Err(RepositoryProvenanceError::TimedOut);
        }
    }
    if u64::try_from(bytes.len()).map_or(true, |size| size > maximum_output_bytes) {
        terminate(&mut child).await;
        return Err(RepositoryProvenanceError::OutputTooLarge);
    }
    let status = if let Ok(status) = timeout(GIT_TIMEOUT, child.wait()).await {
        status.map_err(RepositoryProvenanceError::Wait)?
    } else {
        terminate(&mut child).await;
        return Err(RepositoryProvenanceError::TimedOut);
    };
    if !status.success() {
        return Err(RepositoryProvenanceError::Failed);
    }
    Ok(bytes)
}

async fn terminate(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[derive(Debug, Error)]
pub(crate) enum RepositoryProvenanceError {
    #[error("the bounded Git provenance probe requires PATH")]
    MissingPath,
    #[error("could not start the bounded Git provenance probe")]
    Start(#[source] std::io::Error),
    #[error("the bounded Git provenance probe did not expose stdout")]
    MissingOutput,
    #[error("the bounded Git provenance probe output could not be read")]
    Read(#[source] std::io::Error),
    #[error("the bounded Git provenance probe timed out")]
    TimedOut,
    #[error("the bounded Git provenance probe exceeded its output limit")]
    OutputTooLarge,
    #[error("the bounded Git provenance probe could not be reaped")]
    Wait(#[source] std::io::Error),
    #[error("the bounded Git provenance probe failed")]
    Failed,
    #[error("the bounded Git provenance probe returned an invalid commit or digest")]
    InvalidOutput,
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use uuid::Uuid;

    use super::capture_repository_provenance;

    const SECRET_FILENAME: &str = "tiv-secret-must-not-enter-manifest";

    #[tokio::test]
    async fn capture_binds_commit_and_hashes_clean_or_dirty_status_without_paths() {
        let root = std::env::temp_dir().join(format!("tiv-repository-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        fs::write(root.join("tracked.txt"), b"committed\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(
            &root,
            &[
                "-c",
                "user.name=TxProof Test",
                "-c",
                "user.email=txproof@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );

        let clean = capture_repository_provenance(&root).await.unwrap();
        let clean_json = serde_json::to_value(&clean).unwrap();
        assert_eq!(clean_json["worktree_state"], "clean");
        assert_eq!(clean_json["commit"].as_str().unwrap().len(), 40);
        assert_eq!(clean_json["status_format"], "git_porcelain_v1_z");
        assert_eq!(
            clean_json["status_scope"],
            "tracked_index_worktree_and_non_ignored_untracked_with_non_recursive_submodules"
        );
        assert_eq!(clean_json["status_digest"].as_str().unwrap().len(), 64);

        fs::write(root.join("tracked.txt"), b"dirty\n").unwrap();
        fs::write(root.join(SECRET_FILENAME), b"opaque\n").unwrap();
        let dirty = capture_repository_provenance(&root).await.unwrap();
        let dirty_json = serde_json::to_string(&dirty).unwrap();
        assert!(dirty_json.contains("\"worktree_state\":\"dirty\""));
        assert!(!dirty_json.contains(SECRET_FILENAME));
        assert_ne!(
            serde_json::to_value(&dirty).unwrap()["status_digest"],
            clean_json["status_digest"]
        );
        fs::write(root.join("tracked.txt"), b"different dirty bytes\n").unwrap();
        let same_status = capture_repository_provenance(&root).await.unwrap();
        assert_eq!(
            serde_json::to_value(&dirty).unwrap()["status_digest"],
            serde_json::to_value(&same_status).unwrap()["status_digest"],
            "the documented status fingerprint must not be misread as a content digest"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn capture_fails_closed_outside_a_git_worktree() {
        let root = std::env::temp_dir().join(format!("tiv-repository-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();

        assert!(capture_repository_provenance(&root).await.is_err());

        fs::remove_dir_all(root).unwrap();
    }

    fn git(root: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
