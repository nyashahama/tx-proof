use std::{
    fs::{self, DirBuilder, File, OpenOptions, TryLockError},
    future::Future,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

use thiserror::Error;
use tokio::sync::watch;

const LOCK_DIRECTORY_MODE: u32 = 0o700;
const LOCK_FILE_MODE: u32 = 0o600;
const LOCAL_DOCKER_AUTHORITY: &str = "unix:///var/run/docker.sock";

/// Process-local handle for cancelling one configured campaign.
#[derive(Clone, Debug)]
pub struct RunCancellation {
    sender: watch::Sender<bool>,
}

impl RunCancellation {
    #[must_use]
    pub fn new() -> Self {
        let (sender, _) = watch::channel(false);
        Self { sender }
    }

    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }

    async fn cancelled(&self) {
        let mut receiver = self.sender.subscribe();
        loop {
            if *receiver.borrow_and_update() {
                return;
            }
            receiver
                .changed()
                .await
                .expect("the cancellation token retains its sender");
        }
    }
}

impl Default for RunCancellation {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) type RecoveryFuture<'a, E> = Pin<Box<dyn Future<Output = Result<(), E>> + Send + 'a>>;

pub(crate) trait RecoverableProcess: Send {
    type Error;

    fn recover(&mut self) -> RecoveryFuture<'_, Self::Error>;
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SupervisedCaseOutcome<T, E> {
    Completed(Result<T, E>),
    TimedOut,
    Cancelled,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SupervisedCaseResult<T, E, R> {
    outcome: SupervisedCaseOutcome<T, E>,
    recovery: Result<(), R>,
}

impl<T, E, R> SupervisedCaseResult<T, E, R> {
    #[cfg(test)]
    pub(crate) const fn outcome(&self) -> &SupervisedCaseOutcome<T, E> {
        &self.outcome
    }

    #[cfg(test)]
    pub(crate) const fn recovery(&self) -> &Result<(), R> {
        &self.recovery
    }

    pub(crate) fn into_parts(self) -> (SupervisedCaseOutcome<T, E>, Result<(), R>) {
        (self.outcome, self.recovery)
    }
}

pub(crate) async fn supervise_execution<T, E, F>(
    case_timeout: Duration,
    cancellation: &RunCancellation,
    execution: F,
) -> SupervisedCaseOutcome<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    tokio::pin!(execution);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => SupervisedCaseOutcome::Cancelled,
        timed = tokio::time::timeout(case_timeout, &mut execution) => match timed {
            Ok(result) => SupervisedCaseOutcome::Completed(result),
            Err(_) => SupervisedCaseOutcome::TimedOut,
        },
    }
}

pub(crate) async fn complete_supervision<P, T, E>(
    process: &mut P,
    outcome: SupervisedCaseOutcome<T, E>,
) -> SupervisedCaseResult<T, E, P::Error>
where
    P: RecoverableProcess,
{
    let recovery = process.recover().await;
    SupervisedCaseResult { outcome, recovery }
}

/// Kernel-held advisory exclusion for one local-Docker Compose project.
#[derive(Debug)]
pub(crate) struct ComposeProjectLock {
    _file: File,
    #[cfg(test)]
    path: PathBuf,
}

impl ComposeProjectLock {
    pub(crate) fn try_acquire(root: &Path, project: &str) -> Result<Self, RunSupervisorError> {
        let root = root
            .canonicalize()
            .map_err(|source| RunSupervisorError::Io {
                path: root.to_owned(),
                source,
            })?;
        let owner = fs::metadata(&root)
            .map_err(|source| RunSupervisorError::Io {
                path: root.clone(),
                source,
            })?
            .uid();
        let lock_directory =
            std::env::temp_dir().join(format!("txproof-{owner}-compose-project-locks"));
        ensure_private_lock_directory(&lock_directory, owner)?;
        let key = blake3::hash(format!("{LOCAL_DOCKER_AUTHORITY}\0{project}").as_bytes()).to_hex();
        let path = lock_directory.join(format!("{key}.lock"));
        let file = open_private_lock_file(&path, owner)?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                _file: file,
                #[cfg(test)]
                path,
            }),
            Err(TryLockError::WouldBlock) => Err(RunSupervisorError::Busy(path)),
            Err(TryLockError::Error(source)) => Err(RunSupervisorError::Io { path, source }),
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

fn ensure_private_lock_directory(path: &Path, owner: u32) -> Result<(), RunSupervisorError> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_private_path(path, owner, LOCK_DIRECTORY_MODE, true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match DirBuilder::new().mode(LOCK_DIRECTORY_MODE).create(path) {
                Ok(()) => {
                    fs::set_permissions(path, fs::Permissions::from_mode(LOCK_DIRECTORY_MODE))
                        .map_err(|source| RunSupervisorError::Io {
                            path: path.to_owned(),
                            source,
                        })?;
                }
                Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(RunSupervisorError::Io {
                        path: path.to_owned(),
                        source,
                    });
                }
            }
            validate_private_path(path, owner, LOCK_DIRECTORY_MODE, true)
        }
        Err(source) => Err(RunSupervisorError::Io {
            path: path.to_owned(),
            source,
        }),
    }
}

fn open_private_lock_file(path: &Path, owner: u32) -> Result<File, RunSupervisorError> {
    let created = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(LOCK_FILE_MODE)
        .open(path);
    let file = match created {
        Ok(file) => {
            fs::set_permissions(path, fs::Permissions::from_mode(LOCK_FILE_MODE)).map_err(
                |source| RunSupervisorError::Io {
                    path: path.to_owned(),
                    source,
                },
            )?;
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_private_path(path, owner, LOCK_FILE_MODE, false)?;
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|source| RunSupervisorError::Io {
                    path: path.to_owned(),
                    source,
                })?
        }
        Err(source) => {
            return Err(RunSupervisorError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    validate_open_file(path, &file, owner)?;
    Ok(file)
}

fn validate_private_path(
    path: &Path,
    owner: u32,
    mode: u32,
    directory: bool,
) -> Result<(), RunSupervisorError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| RunSupervisorError::Io {
        path: path.to_owned(),
        source,
    })?;
    let expected_kind = if directory {
        metadata.file_type().is_dir()
    } else {
        metadata.file_type().is_file()
    };
    if metadata.file_type().is_symlink()
        || !expected_kind
        || metadata.uid() != owner
        || metadata.permissions().mode() & 0o777 != mode
    {
        return Err(RunSupervisorError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn validate_open_file(path: &Path, file: &File, owner: u32) -> Result<(), RunSupervisorError> {
    validate_private_path(path, owner, LOCK_FILE_MODE, false)?;
    let path_metadata = fs::metadata(path).map_err(|source| RunSupervisorError::Io {
        path: path.to_owned(),
        source,
    })?;
    let file_metadata = file.metadata().map_err(|source| RunSupervisorError::Io {
        path: path.to_owned(),
        source,
    })?;
    if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino() {
        return Err(RunSupervisorError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub(crate) enum RunSupervisorError {
    #[error("another TxProof run owns the configured Compose project lock: {0}")]
    Busy(PathBuf),
    #[error("configured Compose project lock path is unsafe: {0}")]
    UnsafePath(PathBuf),
    #[error("configured Compose project lock I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl RunSupervisorError {
    #[must_use]
    #[cfg(test)]
    pub(crate) const fn is_busy(&self) -> bool {
        matches!(self, Self::Busy(_))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        future::{Future, pending},
        os::unix::fs::DirBuilderExt,
        path::PathBuf,
        pin::Pin,
        time::Duration,
    };

    use uuid::Uuid;

    use super::{
        ComposeProjectLock, RecoverableProcess, RunCancellation, SupervisedCaseOutcome,
        complete_supervision, supervise_execution,
    };

    #[derive(Default)]
    struct FakeProcess {
        recovery_calls: usize,
        recovery_error: Option<&'static str>,
    }

    impl RecoverableProcess for FakeProcess {
        type Error = &'static str;

        fn recover(
            &mut self,
        ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + '_>> {
            Box::pin(async move {
                self.recovery_calls += 1;
                self.recovery_error.map_or(Ok(()), Err)
            })
        }
    }

    #[tokio::test]
    async fn work_failure_is_returned_only_after_process_recovery() {
        let mut process = FakeProcess::default();
        let outcome = supervise_execution(Duration::from_secs(1), &RunCancellation::new(), async {
            Err::<(), _>("case failed")
        })
        .await;
        let result = complete_supervision(&mut process, outcome).await;

        assert_eq!(process.recovery_calls, 1);
        assert!(matches!(
            result.outcome(),
            SupervisedCaseOutcome::Completed(Err("case failed"))
        ));
        assert_eq!(result.recovery(), &Ok(()));
    }

    #[tokio::test]
    async fn whole_case_timeout_recovers_before_returning_inconclusive() {
        let mut process = FakeProcess::default();
        let outcome = supervise_execution(
            Duration::from_millis(1),
            &RunCancellation::new(),
            pending::<Result<(), &'static str>>(),
        )
        .await;
        let result = complete_supervision(&mut process, outcome).await;

        assert_eq!(process.recovery_calls, 1);
        assert!(matches!(result.outcome(), SupervisedCaseOutcome::TimedOut));
        assert_eq!(result.recovery(), &Ok(()));
    }

    #[tokio::test]
    async fn cancellation_and_recovery_failure_are_both_retained() {
        let cancellation = RunCancellation::new();
        cancellation.cancel();
        let mut process = FakeProcess {
            recovery_error: Some("restart failed"),
            ..FakeProcess::default()
        };
        let outcome = supervise_execution(
            Duration::from_secs(1),
            &cancellation,
            pending::<Result<(), &'static str>>(),
        )
        .await;
        let result = complete_supervision(&mut process, outcome).await;

        assert_eq!(process.recovery_calls, 1);
        assert!(matches!(result.outcome(), SupervisedCaseOutcome::Cancelled));
        assert_eq!(result.recovery(), &Err("restart failed"));
    }

    #[test]
    fn compose_project_lock_excludes_only_the_same_live_project_owner() {
        let root = private_test_root();
        let project = format!("tiv-lock-{}", Uuid::new_v4().simple());
        let other_project = format!("{project}-other");
        let first = ComposeProjectLock::try_acquire(&root, &project).unwrap();

        let busy = ComposeProjectLock::try_acquire(&root, &project).unwrap_err();
        assert!(busy.is_busy());
        let other = ComposeProjectLock::try_acquire(&root, &other_project).unwrap();
        let first_path = first.path().to_owned();
        let other_path = other.path().to_owned();

        drop(first);
        let reacquired = ComposeProjectLock::try_acquire(&root, &project).unwrap();
        drop(reacquired);
        drop(other);
        fs::remove_file(first_path).unwrap();
        fs::remove_file(other_path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn private_test_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("tiv-lock-root-{}", Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        root
    }
}
