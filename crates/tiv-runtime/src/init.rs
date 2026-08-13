//! Collision-safe, fail-closed project scaffold generation.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde::Serialize;
use thiserror::Error;

const INIT_SCHEMA_VERSION: u16 = 1;
const INVARIANT_DIRECTORY: &str = "invariants";

struct TemplateFile {
    relative_path: &'static str,
    contents: &'static str,
}

const TEMPLATE_FILES: [TemplateFile; 9] = [
    TemplateFile {
        relative_path: "checkout.json",
        contents: include_str!("../templates/checkout.json"),
    },
    TemplateFile {
        relative_path: "invariants/01_provider_object_unique.sql",
        contents: include_str!("../templates/invariants/01_provider_object_unique.sql"),
    },
    TemplateFile {
        relative_path: "invariants/02_webhook_effect_at_most_once.sql",
        contents: include_str!("../templates/invariants/02_webhook_effect_at_most_once.sql"),
    },
    TemplateFile {
        relative_path: "invariants/03_paid_order_amount_conservation.sql",
        contents: include_str!("../templates/invariants/03_paid_order_amount_conservation.sql"),
    },
    TemplateFile {
        relative_path: "invariants/04_terminal_success_monotonic.sql",
        contents: include_str!("../templates/invariants/04_terminal_success_monotonic.sql"),
    },
    TemplateFile {
        relative_path: "invariants/05_balanced_ledger.sql",
        contents: include_str!("../templates/invariants/05_balanced_ledger.sql"),
    },
    TemplateFile {
        relative_path: "kill_probe.sql",
        contents: include_str!("../templates/kill_probe.sql"),
    },
    TemplateFile {
        relative_path: "quiescence.sql",
        contents: include_str!("../templates/quiescence.sql"),
    },
    TemplateFile {
        relative_path: "tiv.toml",
        contents: include_str!("../templates/tiv.toml"),
    },
];

/// Machine-readable result of a successful project initialization.
#[derive(Debug, Serialize)]
pub struct InitReport {
    schema_version: u16,
    status: &'static str,
    root: String,
    files: Vec<&'static str>,
    next_command: &'static str,
}

impl InitReport {
    #[must_use]
    pub fn files(&self) -> &[&'static str] {
        &self.files
    }

    /// Serializes the allowlisted initialization report.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if JSON encoding fails.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Writes the exact version-one project scaffold into an empty Git root.
///
/// Every destination is inspected before the first write. Each file is then
/// opened with create-new semantics, so a concurrent collision is never
/// overwritten. Files created by a failed attempt are rolled back.
///
/// # Errors
///
/// Returns [`InitError`] when the directory is not the Git root, a destination
/// exists, a path cannot be reported safely, or file creation/rollback fails.
pub fn initialize_project(root: &Path) -> Result<InitReport, InitError> {
    let root = root
        .canonicalize()
        .map_err(|source| InitError::InspectRoot { source })?;
    let root_display = root.to_str().ok_or(InitError::NonUtf8Root)?.to_owned();
    if !root.join(".git").exists() {
        return Err(InitError::NotRepositoryRoot);
    }

    preflight_destinations(&root)?;
    let invariant_directory = root.join(INVARIANT_DIRECTORY);
    fs::create_dir(&invariant_directory).map_err(|source| InitError::Write {
        path: PathBuf::from(INVARIANT_DIRECTORY),
        source,
    })?;

    let mut created = Vec::with_capacity(TEMPLATE_FILES.len());
    for template in &TEMPLATE_FILES {
        let relative = PathBuf::from(template.relative_path);
        let destination = root.join(&relative);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
        {
            Ok(file) => file,
            Err(source) => {
                return rollback_after_error(&created, &invariant_directory, relative, source);
            }
        };
        created.push(destination);
        if let Err(source) = write_and_sync(&mut file, template.contents) {
            drop(file);
            return rollback_after_error(&created, &invariant_directory, relative, source);
        }
    }

    Ok(InitReport {
        schema_version: INIT_SCHEMA_VERSION,
        status: "initialized",
        root: root_display,
        files: TEMPLATE_FILES
            .iter()
            .map(|template| template.relative_path)
            .collect(),
        next_command: "tiv doctor --config tiv.toml",
    })
}

fn preflight_destinations(root: &Path) -> Result<(), InitError> {
    for template in &TEMPLATE_FILES {
        require_absent(root, Path::new(template.relative_path))?;
    }
    require_absent(root, Path::new(INVARIANT_DIRECTORY))
}

fn require_absent(root: &Path, relative: &Path) -> Result<(), InitError> {
    match fs::symlink_metadata(root.join(relative)) {
        Ok(_) => Err(InitError::DestinationExists(relative.to_owned())),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(InitError::InspectDestination {
            path: relative.to_owned(),
            source,
        }),
    }
}

fn write_and_sync(file: &mut fs::File, contents: &str) -> Result<(), std::io::Error> {
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

fn rollback_after_error(
    created: &[PathBuf],
    invariant_directory: &Path,
    failed_path: PathBuf,
    source: std::io::Error,
) -> Result<InitReport, InitError> {
    let mut rollback_failure = None;
    for path in created.iter().rev() {
        if let Err(rollback_source) = fs::remove_file(path)
            && rollback_source.kind() != std::io::ErrorKind::NotFound
            && rollback_failure.is_none()
        {
            rollback_failure = Some((path.to_owned(), rollback_source));
        }
    }
    if let Err(rollback_source) = fs::remove_dir(invariant_directory)
        && rollback_source.kind() != std::io::ErrorKind::NotFound
        && rollback_failure.is_none()
    {
        rollback_failure = Some((PathBuf::from(INVARIANT_DIRECTORY), rollback_source));
    }
    if let Some((path, rollback_source)) = rollback_failure {
        return Err(InitError::Rollback {
            path,
            source: rollback_source,
        });
    }
    Err(InitError::Write {
        path: failed_path,
        source,
    })
}

#[derive(Debug, Error)]
pub enum InitError {
    #[error("could not inspect the current directory: {source}")]
    InspectRoot { source: std::io::Error },
    #[error("tiv init must run at a Git repository root")]
    NotRepositoryRoot,
    #[error("the repository root is not valid UTF-8")]
    NonUtf8Root,
    #[error("destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("could not inspect destination {path}: {source}")]
    InspectDestination {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write scaffold destination {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not roll back scaffold destination {path}: {source}")]
    Rollback {
        path: PathBuf,
        source: std::io::Error,
    },
}
