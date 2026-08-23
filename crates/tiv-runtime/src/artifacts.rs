//! Private, collision-safe configured campaign run artifacts.

use std::{
    fmt::Write as _,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use serde::Serialize;
use thiserror::Error;

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PartialRunClass {
    Configuration,
    Infrastructure,
    Inconclusive,
    Interrupted,
}

pub(crate) struct RunArtifactStaging {
    run_id: String,
    base: PathBuf,
    staging: PathBuf,
    final_path: PathBuf,
}

impl RunArtifactStaging {
    pub(crate) fn create(root: &Path, base: &Path, run_id: &str) -> Result<Self, ArtifactError> {
        validate_run_id(run_id)?;
        let root = root.canonicalize().map_err(|source| ArtifactError::Io {
            path: root.to_owned(),
            source,
        })?;
        if !base.starts_with(&root) {
            return Err(ArtifactError::UnsafePath(base.to_owned()));
        }
        create_private_tree(&root, base)?;
        let canonical_base = base.canonicalize().map_err(|source| ArtifactError::Io {
            path: base.to_owned(),
            source,
        })?;
        if !canonical_base.starts_with(&root) {
            return Err(ArtifactError::UnsafePath(base.to_owned()));
        }
        let staging = canonical_base.join(format!(".{run_id}.staging"));
        let final_path = canonical_base.join(run_id);
        if fs::symlink_metadata(&final_path).is_ok() {
            return Err(ArtifactError::Collision(final_path));
        }
        DirBuilder::new()
            .mode(DIRECTORY_MODE)
            .create(&staging)
            .map_err(|source| ArtifactError::Io {
                path: staging.clone(),
                source,
            })?;
        set_mode(&staging, DIRECTORY_MODE)?;
        sync_directory(&canonical_base)?;
        Ok(Self {
            run_id: run_id.to_owned(),
            base: canonical_base,
            staging,
            final_path,
        })
    }

    pub(crate) fn write_json(
        &mut self,
        relative: impl AsRef<Path>,
        value: &impl Serialize,
    ) -> Result<(), ArtifactError> {
        let mut bytes = serde_json::to_vec_pretty(value).map_err(ArtifactError::Serialize)?;
        bytes.push(b'\n');
        self.write_bytes(relative, &bytes)
    }

    pub(crate) fn write_bytes(
        &mut self,
        relative: impl AsRef<Path>,
        bytes: &[u8],
    ) -> Result<(), ArtifactError> {
        let path = self.prepare_path(relative)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(&path)
            .map_err(|source| ArtifactError::Io {
                path: path.clone(),
                source,
            })?;
        set_mode(&path, FILE_MODE)?;
        file.write_all(bytes).map_err(|source| ArtifactError::Io {
            path: path.clone(),
            source,
        })?;
        file.sync_all().map_err(|source| ArtifactError::Io {
            path: path.clone(),
            source,
        })?;
        Ok(())
    }

    pub(crate) fn prepare_path(
        &mut self,
        relative: impl AsRef<Path>,
    ) -> Result<PathBuf, ArtifactError> {
        let relative = validate_relative(relative.as_ref())?;
        let path = self.staging.join(relative);
        let parent = path
            .parent()
            .ok_or_else(|| ArtifactError::UnsafePath(path.clone()))?;
        create_private_tree(&self.staging, parent)?;
        if fs::symlink_metadata(&path).is_ok() {
            return Err(ArtifactError::Collision(path));
        }
        Ok(path)
    }

    pub(crate) fn finalize(self) -> Result<PathBuf, ArtifactError> {
        self.finalize_with_manifest(true, None, None)
    }

    pub(crate) fn finalize_partial(
        self,
        failure_class: PartialRunClass,
        failure_code: &'static str,
    ) -> Result<PathBuf, ArtifactError> {
        if !valid_failure_code(failure_code) {
            return Err(ArtifactError::InvalidFailureCode);
        }
        self.finalize_with_manifest(false, Some(failure_class), Some(failure_code))
    }

    fn finalize_with_manifest(
        mut self,
        complete: bool,
        failure_class: Option<PartialRunClass>,
        failure_code: Option<&'static str>,
    ) -> Result<PathBuf, ArtifactError> {
        let files = collect_regular_files(&self.staging)?;
        if files
            .iter()
            .any(|path| matches!(path.as_str(), "checksums.txt" | "manifest.json"))
        {
            return Err(ArtifactError::ReservedArtifact);
        }
        let mut checksums = String::new();
        for relative in &files {
            let digest = checksum_file(&self.staging.join(relative))?;
            writeln!(checksums, "{digest}  {relative}")
                .map_err(|_| ArtifactError::ChecksumFormatting)?;
        }
        self.write_bytes("checksums.txt", checksums.as_bytes())?;
        self.write_json(
            "manifest.json",
            &RunManifest {
                schema_version: 1,
                run_id: self.run_id.clone(),
                complete,
                status: if complete {
                    RunManifestStatus::Complete
                } else {
                    RunManifestStatus::Partial
                },
                failure_class,
                failure_code,
                checksums_file: "checksums.txt",
                artifact_count: files.len() + 2,
            },
        )?;
        sync_directory(&self.staging)?;
        fs::rename(&self.staging, &self.final_path).map_err(|source| ArtifactError::Io {
            path: self.final_path.clone(),
            source,
        })?;
        sync_directory(&self.base)?;
        Ok(self.final_path)
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RunManifestStatus {
    Complete,
    Partial,
}

#[derive(Serialize)]
struct RunManifest<'a> {
    schema_version: u16,
    run_id: String,
    complete: bool,
    status: RunManifestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_class: Option<PartialRunClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_code: Option<&'a str>,
    checksums_file: &'static str,
    artifact_count: usize,
}

fn validate_run_id(run_id: &str) -> Result<(), ArtifactError> {
    if !(run_id.starts_with("run_")
        && (5..=80).contains(&run_id.len())
        && run_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'))
    {
        return Err(ArtifactError::InvalidRunId);
    }
    Ok(())
}

fn valid_failure_code(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn validate_relative(path: &Path) -> Result<&Path, ArtifactError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ArtifactError::UnsafePath(path.to_owned()));
    }
    Ok(path)
}

fn create_private_tree(root: &Path, target: &Path) -> Result<(), ArtifactError> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| ArtifactError::UnsafePath(target.to_owned()))?;
    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(ArtifactError::UnsafePath(target.to_owned()));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
                set_mode(&current, DIRECTORY_MODE)?;
            }
            Ok(_) => return Err(ArtifactError::UnsafePath(current)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DirBuilder::new()
                    .mode(DIRECTORY_MODE)
                    .create(&current)
                    .map_err(|source| ArtifactError::Io {
                        path: current.clone(),
                        source,
                    })?;
                set_mode(&current, DIRECTORY_MODE)?;
            }
            Err(source) => {
                return Err(ArtifactError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn collect_regular_files(root: &Path) -> Result<Vec<String>, ArtifactError> {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<String>) -> Result<(), ArtifactError> {
        let mut entries = fs::read_dir(directory)
            .map_err(|source| ArtifactError::Io {
                path: directory.to_owned(),
                source,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| ArtifactError::Io {
                path: directory.to_owned(),
                source,
            })?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| ArtifactError::Io {
                path: path.clone(),
                source,
            })?;
            if metadata.file_type().is_symlink() {
                return Err(ArtifactError::UnsafePath(path));
            }
            if metadata.is_dir() {
                if metadata.permissions().mode() & 0o777 != DIRECTORY_MODE {
                    return Err(ArtifactError::UnsafePermissions(path));
                }
                visit(root, &path, files)?;
            } else if metadata.is_file() {
                if metadata.permissions().mode() & 0o777 != FILE_MODE {
                    return Err(ArtifactError::UnsafePermissions(path));
                }
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| ArtifactError::UnsafePath(path.clone()))?
                    .to_str()
                    .ok_or_else(|| ArtifactError::UnsafePath(path.clone()))?;
                files.push(relative.replace(std::path::MAIN_SEPARATOR, "/"));
            } else {
                return Err(ArtifactError::UnsafePath(path));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn checksum_file(path: &Path) -> Result<String, ArtifactError> {
    let mut file = File::open(path).map_err(|source| ArtifactError::Io {
        path: path.to_owned(),
        source,
    })?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|source| ArtifactError::Io {
            path: path.to_owned(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), ArtifactError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
        ArtifactError::Io {
            path: path.to_owned(),
            source,
        }
    })
}

fn sync_directory(path: &Path) -> Result<(), ArtifactError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| ArtifactError::Io {
            path: path.to_owned(),
            source,
        })
}

#[derive(Debug, Error)]
pub(crate) enum ArtifactError {
    #[error("artifact path is outside the private run boundary: {0}")]
    UnsafePath(PathBuf),
    #[error("artifact path already exists: {0}")]
    Collision(PathBuf),
    #[error("run identifier is outside the v1 artifact grammar")]
    InvalidRunId,
    #[error("partial-run failure code is outside the v1 artifact grammar")]
    InvalidFailureCode,
    #[error("artifact has unsafe filesystem permissions: {0}")]
    UnsafePermissions(PathBuf),
    #[error("manifest or checksum artifact was written before finalization")]
    ReservedArtifact,
    #[error("artifact checksum index could not be formatted")]
    ChecksumFormatting,
    #[error("artifact JSON serialization failed: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("artifact I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use uuid::Uuid;

    use super::{PartialRunClass, RunArtifactStaging};

    #[test]
    fn finalized_run_is_private_checksummed_and_manifest_complete() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(&root, &base, "run_deadbeef").unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "held"}))
            .unwrap();

        let final_path = staging.finalize().unwrap();

        assert!(final_path.join("manifest.json").is_file());
        assert!(final_path.join("checksums.txt").is_file());
        assert_eq!(
            fs::metadata(&final_path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(final_path.join("summary.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(final_path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["complete"], true);
        assert_eq!(manifest["run_id"], "run_deadbeef");
        let checksums = fs::read_to_string(final_path.join("checksums.txt")).unwrap();
        assert!(checksums.contains("  summary.json\n"));
        assert!(!checksums.contains("manifest.json"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_run_is_atomically_promoted_but_never_presented_as_complete() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(&root, &base, "run_partial").unwrap();
        staging
            .write_json(
                "summary.json",
                &serde_json::json!({"status": "inconclusive"}),
            )
            .unwrap();

        let final_path = staging
            .finalize_partial(PartialRunClass::Inconclusive, "case_timeout")
            .unwrap();

        assert!(final_path.is_dir());
        assert!(!base.join(".run_partial.staging").exists());
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(final_path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["complete"], false);
        assert_eq!(manifest["status"], "partial");
        assert_eq!(manifest["failure_class"], "inconclusive");
        assert_eq!(manifest["failure_code"], "case_timeout");
        let checksums = fs::read_to_string(final_path.join("checksums.txt")).unwrap();
        assert!(checksums.contains("  summary.json\n"));
        assert!(!checksums.contains("manifest.json"));

        fs::remove_dir_all(root).unwrap();
    }
}
