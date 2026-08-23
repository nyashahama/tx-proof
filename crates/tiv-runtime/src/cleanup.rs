//! Exact-run artifact cleanup with fail-closed filesystem and provenance gates.

use std::{
    fs::{self, File, Metadata},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use serde::Serialize;
use thiserror::Error;

use crate::{
    artifacts::{ArtifactError, validate_run_id, verify_complete_run_artifact},
    config::{ConfigError, EnvironmentLookup, load_resolved_config},
    run_supervisor::{ComposeProjectLock, RunSupervisorError},
};

const DIRECTORY_MODE: u32 = 0o700;
const MAX_SIBLING_ENTRIES: usize = 2_048;

/// Validated, exact cleanup selection. It never accepts an artifact path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CleanupOptions {
    run_id: String,
}

impl CleanupOptions {
    /// Validates one run identifier against the artifact grammar.
    ///
    /// # Errors
    ///
    /// Returns [`CleanupError::InvalidRunId`] for paths, staging names, or any
    /// noncanonical identifier.
    pub fn new(run_id: impl Into<String>) -> Result<Self, CleanupError> {
        let run_id = run_id.into();
        validate_run_id(&run_id).map_err(|_| CleanupError::InvalidRunId)?;
        Ok(Self { run_id })
    }
}

/// Stable result of an idempotent exact-run cleanup request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupStatus {
    Removed,
    AlreadyAbsent,
}

/// Allowlisted cleanup receipt. Filesystem paths and configuration never enter
/// the successful output document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CleanupReceipt {
    schema_version: u16,
    status: CleanupStatus,
    run_id: String,
}

impl CleanupReceipt {
    fn new(status: CleanupStatus, run_id: &str) -> Self {
        Self {
            schema_version: 1,
            status,
            run_id: run_id.to_owned(),
        }
    }

    #[must_use]
    pub const fn status(&self) -> CleanupStatus {
        self.status
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Encodes the allowlisted receipt without configuration or paths.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] only if receipt serialization fails.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Failures at the exact-run cleanup boundary.
#[derive(Debug, Error)]
pub enum CleanupError {
    #[error("run identifier is outside the artifact grammar")]
    InvalidRunId,
    #[error("project configuration is invalid: {0}")]
    Config(#[from] ConfigError),
    #[error("another TxProof command owns the configured Compose project")]
    ProjectBusy,
    #[error("the configured Compose project lock boundary is unsafe")]
    UnsafeProjectLock,
    #[error("the configured artifact directory boundary is unsafe")]
    UnsafeArtifactBase,
    #[error("the selected run still has a staging directory")]
    ActiveStaging,
    #[error("the selected run is not an eligible verified complete artifact: {0}")]
    IneligibleArtifact(#[source] ArtifactError),
    #[error("the selected run is referenced by local descendant {run_id}")]
    ReferencedByLocalDescendant { run_id: String },
    #[error("the artifact directory contains too many entries for bounded cleanup")]
    TooManySiblingEntries,
    #[error("the selected run changed while cleanup was verifying it")]
    ArtifactChanged,
    #[error("the selected run reappeared before cleanup completed")]
    ArtifactReappeared,
    #[error("artifact cleanup I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl CleanupError {
    /// Maps cleanup failures onto the public configuration/infrastructure
    /// boundary without implying a product conclusion.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::ProjectBusy
            | Self::UnsafeProjectLock
            | Self::ArtifactChanged
            | Self::ArtifactReappeared
            | Self::Io { .. } => 3,
            Self::InvalidRunId
            | Self::Config(_)
            | Self::UnsafeArtifactBase
            | Self::ActiveStaging
            | Self::IneligibleArtifact(_)
            | Self::ReferencedByLocalDescendant { .. }
            | Self::TooManySiblingEntries => 2,
        }
    }
}

/// Removes exactly one eligible finalized artifact selected by configured run
/// identity. It never executes Compose, Docker, SQL, or baseline operations.
///
/// # Errors
///
/// Returns [`CleanupError`] before deletion when configuration, locking,
/// filesystem integrity, artifact integrity, or local provenance checks fail.
pub fn cleanup_configured_run(
    config_path: &Path,
    environment: &impl EnvironmentLookup,
    options: &CleanupOptions,
) -> Result<CleanupReceipt, CleanupError> {
    let config = load_resolved_config(config_path, environment)?;
    cleanup_run_artifact(
        config.root(),
        config.artifact_dir(),
        config.compose_project(),
        options,
    )
}

pub(crate) fn cleanup_run_artifact(
    root: &Path,
    artifact_base: &Path,
    compose_project: &str,
    options: &CleanupOptions,
) -> Result<CleanupReceipt, CleanupError> {
    let _project_lock =
        ComposeProjectLock::try_acquire(root, compose_project).map_err(map_project_lock_error)?;
    let Some(base) = existing_private_artifact_base(root, artifact_base)? else {
        return Ok(CleanupReceipt::new(
            CleanupStatus::AlreadyAbsent,
            &options.run_id,
        ));
    };

    let staging = base.join(format!(".{}.staging", options.run_id));
    match fs::symlink_metadata(&staging) {
        Ok(_) => return Err(CleanupError::ActiveStaging),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(CleanupError::Io {
                path: staging,
                source,
            });
        }
    }

    let target = base.join(&options.run_id);
    let initial_metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CleanupReceipt::new(
                CleanupStatus::AlreadyAbsent,
                &options.run_id,
            ));
        }
        Err(source) => {
            return Err(CleanupError::Io {
                path: target,
                source,
            });
        }
    };

    let verified =
        verify_complete_run_artifact(&target).map_err(CleanupError::IneligibleArtifact)?;
    refuse_locally_referenced_source(&base, &target, &verified)?;

    let final_metadata = fs::symlink_metadata(&target).map_err(|source| CleanupError::Io {
        path: target.clone(),
        source,
    })?;
    if !same_directory_identity(&initial_metadata, &final_metadata) {
        return Err(CleanupError::ArtifactChanged);
    }

    fs::remove_dir_all(&target).map_err(|source| CleanupError::Io {
        path: target.clone(),
        source,
    })?;
    sync_directory(&base)?;
    match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err(CleanupError::ArtifactReappeared),
        Err(source) => {
            return Err(CleanupError::Io {
                path: target,
                source,
            });
        }
    }

    Ok(CleanupReceipt::new(CleanupStatus::Removed, &options.run_id))
}

fn map_project_lock_error(error: RunSupervisorError) -> CleanupError {
    match error {
        RunSupervisorError::Busy(_) => CleanupError::ProjectBusy,
        RunSupervisorError::UnsafePath(_) => CleanupError::UnsafeProjectLock,
        RunSupervisorError::Io { path, source } => CleanupError::Io { path, source },
    }
}

fn existing_private_artifact_base(
    root: &Path,
    artifact_base: &Path,
) -> Result<Option<PathBuf>, CleanupError> {
    let canonical_root = root.canonicalize().map_err(|source| CleanupError::Io {
        path: root.to_owned(),
        source,
    })?;
    let relative = artifact_base
        .strip_prefix(root)
        .or_else(|_| artifact_base.strip_prefix(&canonical_root))
        .map_err(|_| CleanupError::UnsafeArtifactBase)?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CleanupError::UnsafeArtifactBase);
    }

    let mut current = canonical_root;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(CleanupError::UnsafeArtifactBase);
        };
        current.push(name);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CleanupError::Io {
                    path: current,
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.permissions().mode() & 0o777 != DIRECTORY_MODE
        {
            return Err(CleanupError::UnsafeArtifactBase);
        }
    }
    Ok(Some(current))
}

fn refuse_locally_referenced_source(
    base: &Path,
    target: &Path,
    source: &crate::artifacts::VerifiedRunArtifact,
) -> Result<(), CleanupError> {
    let source_identity = source.source_identity();
    let entries = fs::read_dir(base)
        .map_err(|source| CleanupError::Io {
            path: base.to_owned(),
            source,
        })?
        .take(MAX_SIBLING_ENTRIES + 1)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| CleanupError::Io {
            path: base.to_owned(),
            source,
        })?;
    if entries.len() > MAX_SIBLING_ENTRIES {
        return Err(CleanupError::TooManySiblingEntries);
    }

    for entry in entries {
        let path = entry.path();
        if path == target {
            continue;
        }
        let Some(run_id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if validate_run_id(&run_id).is_err() {
            continue;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|source| CleanupError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        let Ok(candidate) = verify_complete_run_artifact(&path) else {
            continue;
        };
        if candidate.references_source(&source_identity) {
            return Err(CleanupError::ReferencedByLocalDescendant { run_id });
        }
    }
    Ok(())
}

fn same_directory_identity(initial: &Metadata, current: &Metadata) -> bool {
    !current.file_type().is_symlink()
        && current.is_dir()
        && current.permissions().mode() & 0o777 == DIRECTORY_MODE
        && initial.dev() == current.dev()
        && initial.ino() == current.ino()
        && initial.mode() == current.mode()
}

fn sync_directory(path: &Path) -> Result<(), CleanupError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| CleanupError::Io {
            path: path.to_owned(),
            source,
        })
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, DirBuilder},
        os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
        path::{Path, PathBuf},
    };

    use tiv_core::trace::{CASE_TRACE_SCHEMA_VERSION, TRACE_SCHEMA_VERSION};
    use uuid::Uuid;

    use crate::{
        artifacts::{
            ArtifactAuthority, ArtifactKind, ArtifactResult, ManifestSeed, PartialRunClass,
            RepositoryProvenance, RunArtifactStaging, WorktreeState, verify_complete_run_artifact,
        },
        run_supervisor::ComposeProjectLock,
    };

    use super::{CleanupError, CleanupOptions, CleanupStatus, cleanup_run_artifact};

    #[test]
    fn exact_complete_run_is_removed_idempotently_without_touching_siblings() {
        let root = test_root();
        let base = root.join(".tiv/runs");
        let target = complete_v1_artifact(&root, "run_cleanup_target");
        let sibling = complete_v1_artifact(&root, "run_cleanup_sibling");
        let sentinel = root.join("sentinel.txt");
        fs::write(&sentinel, b"preserve me").unwrap();
        let options = CleanupOptions::new("run_cleanup_target").unwrap();

        let first = cleanup_run_artifact(&root, &base, "tiv-cleanup-test", &options).unwrap();
        assert_eq!(first.status(), CleanupStatus::Removed);
        assert_eq!(first.run_id(), "run_cleanup_target");
        let receipt = first.to_pretty_json().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&receipt).unwrap(),
            serde_json::json!({
                "schema_version": 1,
                "status": "removed",
                "run_id": "run_cleanup_target"
            })
        );
        assert!(!receipt.contains(root.to_string_lossy().as_ref()));
        assert!(!target.exists());
        assert!(sibling.is_dir());
        assert_eq!(fs::read(&sentinel).unwrap(), b"preserve me");
        assert!(base.is_dir());

        let second = cleanup_run_artifact(&root, &base, "tiv-cleanup-test", &options).unwrap();
        assert_eq!(second.status(), CleanupStatus::AlreadyAbsent);
        assert!(sibling.is_dir());

        remove_test_root(&root);
    }

    #[test]
    fn run_identifier_rejects_paths_and_noncanonical_names() {
        for invalid in [
            "../run_escape",
            "/tmp/run_escape",
            "run/path",
            ".run_deadbeef.staging",
            "run_Uppercase",
            "run_",
            "run-0123",
        ] {
            assert!(
                matches!(
                    CleanupOptions::new(invalid),
                    Err(CleanupError::InvalidRunId)
                ),
                "accepted invalid run identifier {invalid:?}"
            );
        }
        assert!(matches!(
            CleanupOptions::new(format!("run_{}", "a".repeat(77))),
            Err(CleanupError::InvalidRunId)
        ));
    }

    #[test]
    fn active_staging_directory_blocks_cleanup() {
        let root = test_root();
        let base = root.join(".tiv/runs");
        let target = complete_v1_artifact(&root, "run_active_target");
        DirBuilder::new()
            .mode(0o700)
            .create(base.join(".run_active_target.staging"))
            .unwrap();
        let options = CleanupOptions::new("run_active_target").unwrap();

        assert!(matches!(
            cleanup_run_artifact(&root, &base, "tiv-cleanup-active", &options),
            Err(CleanupError::ActiveStaging)
        ));
        assert!(target.is_dir());

        remove_test_root(&root);
    }

    #[test]
    fn partial_corrupt_and_permission_unsafe_targets_are_preserved() {
        let root = test_root();
        let base = root.join(".tiv/runs");

        let partial = partial_artifact(&root, "run_partial_target");
        assert_ineligible_and_preserved(&root, &base, "run_partial_target", &partial);

        let corrupt = complete_v1_artifact(&root, "run_corrupt_target");
        fs::write(corrupt.join("summary.json"), b"{\"status\":\"changed\"}\n").unwrap();
        assert_ineligible_and_preserved(&root, &base, "run_corrupt_target", &corrupt);

        let wrong_mode = complete_v1_artifact(&root, "run_mode_target");
        fs::set_permissions(&wrong_mode, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ineligible_and_preserved(&root, &base, "run_mode_target", &wrong_mode);

        remove_test_root(&root);
    }

    #[test]
    fn symlink_or_non_directory_targets_and_symlinked_bases_are_preserved() {
        let root = test_root();
        let base = root.join(".tiv/runs");
        create_private_dir(&root.join(".tiv"));
        create_private_dir(&base);
        let external = root.join("external");
        create_private_dir(&external);

        symlink(&external, base.join("run_symlink_target")).unwrap();
        assert_ineligible_and_preserved(
            &root,
            &base,
            "run_symlink_target",
            &base.join("run_symlink_target"),
        );

        let file_target = base.join("run_file_target");
        fs::write(&file_target, b"not a run").unwrap();
        fs::set_permissions(&file_target, fs::Permissions::from_mode(0o600)).unwrap();
        assert_ineligible_and_preserved(&root, &base, "run_file_target", &file_target);

        fs::remove_file(base.join("run_symlink_target")).unwrap();
        fs::remove_file(&file_target).unwrap();
        fs::remove_dir(&base).unwrap();
        symlink(&external, &base).unwrap();
        let options = CleanupOptions::new("run_missing_target").unwrap();
        assert!(matches!(
            cleanup_run_artifact(&root, &base, "tiv-cleanup-symlink", &options),
            Err(CleanupError::UnsafeArtifactBase)
        ));
        assert!(external.is_dir());

        remove_test_root(&root);
    }

    #[test]
    fn repository_root_cannot_be_used_as_the_artifact_deletion_base() {
        let root = test_root();
        let mut staging =
            RunArtifactStaging::create(&root, &root, "run_repository_root_target").unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "held"}))
            .unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        let target = staging.finalize().unwrap();
        let options = CleanupOptions::new("run_repository_root_target").unwrap();

        assert!(matches!(
            cleanup_run_artifact(&root, &root, "tiv-cleanup-root", &options),
            Err(CleanupError::UnsafeArtifactBase)
        ));
        assert!(target.is_dir());

        remove_test_root(&root);
    }

    #[test]
    fn held_compose_project_lock_blocks_cleanup_without_mutation() {
        let root = test_root();
        let base = root.join(".tiv/runs");
        let target = complete_v1_artifact(&root, "run_locked_target");
        let _lock = ComposeProjectLock::try_acquire(&root, "tiv-cleanup-locked").unwrap();
        let options = CleanupOptions::new("run_locked_target").unwrap();

        let error = cleanup_run_artifact(&root, &base, "tiv-cleanup-locked", &options)
            .expect_err("the live project lock must exclude cleanup");
        assert!(matches!(&error, CleanupError::ProjectBusy));
        assert_eq!(error.exit_code(), 3);
        assert!(target.is_dir());

        remove_test_root(&root);
    }

    #[test]
    fn locally_referenced_source_is_preserved_until_the_leaf_is_removed() {
        let root = test_root();
        let base = root.join(".tiv/runs");
        let source = complete_v1_artifact(&root, "run_source_campaign");
        let verified_source = verify_complete_run_artifact(&source).unwrap();
        let leaf = complete_v2_replay(&root, "run_replay_leaf", &verified_source);

        let source_options = CleanupOptions::new("run_source_campaign").unwrap();
        let error = cleanup_run_artifact(&root, &base, "tiv-cleanup-references", &source_options)
            .expect_err("a locally referenced source must be preserved");
        assert!(matches!(
            error,
            CleanupError::ReferencedByLocalDescendant { ref run_id }
                if run_id == "run_replay_leaf"
        ));
        assert!(source.is_dir());

        let leaf_options = CleanupOptions::new("run_replay_leaf").unwrap();
        let receipt =
            cleanup_run_artifact(&root, &base, "tiv-cleanup-references", &leaf_options).unwrap();
        assert_eq!(receipt.status(), CleanupStatus::Removed);
        assert!(!leaf.exists());
        assert!(source.is_dir());

        remove_test_root(&root);
    }

    fn assert_ineligible_and_preserved(root: &Path, base: &Path, run_id: &str, target: &Path) {
        let options = CleanupOptions::new(run_id).unwrap();
        assert!(matches!(
            cleanup_run_artifact(root, base, "tiv-cleanup-ineligible", &options),
            Err(CleanupError::IneligibleArtifact(_))
        ));
        assert!(fs::symlink_metadata(target).is_ok());
    }

    fn test_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("tiv-cleanup-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        root
    }

    fn remove_test_root(root: &Path) {
        fs::remove_dir_all(root).unwrap();
    }

    fn create_private_dir(path: &Path) {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn complete_v1_artifact(root: &Path, run_id: &str) -> PathBuf {
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(root, &base, run_id).unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "held"}))
            .unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        staging.finalize().unwrap()
    }

    fn partial_artifact(root: &Path, run_id: &str) -> PathBuf {
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(root, &base, run_id).unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "partial"}))
            .unwrap();
        staging
            .finalize_partial(PartialRunClass::Interrupted, "interrupted")
            .unwrap()
    }

    fn complete_v2_replay(
        root: &Path,
        run_id: &str,
        source: &crate::artifacts::VerifiedRunArtifact,
    ) -> PathBuf {
        let base = root.join(".tiv/runs");
        let repository =
            RepositoryProvenance::captured(&"a".repeat(40), WorktreeState::Clean, &"b".repeat(64))
                .unwrap();
        let mut staging = RunArtifactStaging::create_v2(
            root,
            &base,
            run_id,
            ManifestSeed::new(
                ArtifactKind::Replay,
                repository,
                vec![source.source_identity()],
            ),
        )
        .unwrap();
        staging
            .write_json(
                "config.redacted.json",
                &serde_json::json!({"schema_version": 1}),
            )
            .unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        staging
            .write_json("source.json", &serde_json::json!({"schema_version": 1}))
            .unwrap();
        staging
            .write_json(
                "trace.original.json",
                &serde_json::json!({"schema_version": CASE_TRACE_SCHEMA_VERSION}),
            )
            .unwrap();
        staging
            .write_json(
                "summary.json",
                &serde_json::json!({"classification": "stable"}),
            )
            .unwrap();
        staging
            .finalize_complete(
                ArtifactResult::Counterexample,
                vec![
                    ArtifactAuthority::replay_source(),
                    ArtifactAuthority::original_trace(),
                ],
            )
            .unwrap()
    }

    fn compatibility_fixture() -> serde_json::Value {
        serde_json::json!({
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
}
