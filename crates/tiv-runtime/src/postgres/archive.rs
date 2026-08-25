use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    task::JoinHandle,
    time::timeout,
};
use uuid::Uuid;

#[cfg(test)]
use std::io::Write;

use super::safety::{DatabaseKind, DatabaseName};

const LOCAL_DOCKER_HOST: &str = "unix:///var/run/docker.sock";
const TOOL_TIMEOUT: &str = "30s";
const HOST_COMMAND_TIMEOUT: Duration = Duration::from_secs(35);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const VERSION_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_DIAGNOSTIC_BYTES: u64 = 64 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const CUSTOM_ARCHIVE_MAGIC: &[u8; 5] = b"PGDMP";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PostgresTool {
    Dump,
    Restore,
}

impl PostgresTool {
    const fn program(self) -> &'static str {
        match self {
            Self::Dump => "pg_dump",
            Self::Restore => "pg_restore",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PostgresToolVersion {
    tool: PostgresTool,
    major: u16,
}

impl PostgresToolVersion {
    fn parse(tool: PostgresTool, banner: &str) -> Result<Self, ArchiveError> {
        let banner = banner.trim();
        let prefix = format!("{} (PostgreSQL) ", tool.program());
        if banner.len() > 256 || !banner.starts_with(&prefix) || banner.contains('\n') {
            return Err(ArchiveError::InvalidToolVersion(tool));
        }
        let version = banner[prefix.len()..]
            .split_ascii_whitespace()
            .next()
            .ok_or(ArchiveError::InvalidToolVersion(tool))?;
        let major = version
            .split('.')
            .next()
            .ok_or(ArchiveError::InvalidToolVersion(tool))?
            .parse::<u16>()
            .map_err(|_| ArchiveError::InvalidToolVersion(tool))?;
        if major == 0 {
            return Err(ArchiveError::InvalidToolVersion(tool));
        }
        Ok(Self { tool, major })
    }

    #[cfg(test)]
    const fn major(self) -> u16 {
        self.major
    }

    const fn require_server_major(self, server_major: u16) -> Result<(), ArchiveError> {
        if self.major == server_major {
            Ok(())
        } else {
            Err(ArchiveError::ClientServerVersionMismatch {
                tool: self.tool,
                client_major: self.major,
                server_major,
            })
        }
    }
}

#[derive(Debug)]
pub(crate) struct PostgresArchiveToolchain {
    container_id: String,
    admin_role: String,
    server_major: u16,
}

impl PostgresArchiveToolchain {
    pub(crate) async fn attest(
        container_id: impl Into<String>,
        admin_role: impl Into<String>,
        server_major: u16,
    ) -> Result<Self, ArchiveError> {
        let container_id = container_id.into();
        let admin_role = admin_role.into();
        validate_container_id(&container_id)?;
        validate_role_name(&admin_role)?;
        if server_major == 0 {
            return Err(ArchiveError::InvalidServerVersion);
        }
        for tool in [PostgresTool::Dump, PostgresTool::Restore] {
            let version = run_tool_version(&container_id, tool).await?;
            version.require_server_major(server_major)?;
        }
        Ok(Self {
            container_id,
            admin_role,
            server_major,
        })
    }

    pub(crate) async fn capture(
        &self,
        baseline: &DatabaseName,
    ) -> Result<BaselineArchive, ArchiveError> {
        if baseline.kind() != DatabaseKind::Baseline {
            return Err(ArchiveError::InvalidCommandPlan);
        }
        let archive = BaselineArchive::create(
            baseline.clone(),
            self.container_id.clone(),
            self.server_major,
        )?;
        let args = dump_command_args(&self.container_id, &self.admin_role, baseline);
        let mut command = docker_command(&args);
        command
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|_| ArchiveError::CommandStart(PostgresTool::Dump))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ArchiveError::CommandStart(PostgresTool::Dump))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(ArchiveError::CommandStart(PostgresTool::Dump))?;
        let archive_file = archive.file.try_clone()?;
        let writer = tokio::spawn(write_bounded_archive(stdout, archive_file));
        let stderr_reader = tokio::spawn(read_bounded(stderr));
        let status = wait_for_child(&mut child, PostgresTool::Dump, HOST_COMMAND_TIMEOUT).await;
        let (bytes_written, _stderr) = finish_dump_tasks(writer, stderr_reader).await?;
        let status = status?;
        if !status.success() {
            return Err(ArchiveError::ToolCommandFailed(PostgresTool::Dump));
        }
        bytes_written?;
        archive.file.sync_all()?;
        archive.validate()?;
        Ok(archive)
    }

    pub(crate) fn preflight(
        &self,
        archive: &BaselineArchive,
        baseline: &DatabaseName,
    ) -> Result<(), ArchiveError> {
        if !archive.matches_toolchain(self) || !archive.matches_baseline(baseline) {
            return Err(ArchiveError::ArchiveSourceMismatch);
        }
        archive.validate()
    }

    pub(crate) async fn restore(
        &self,
        archive: &BaselineArchive,
        case: &DatabaseName,
    ) -> Result<(), ArchiveError> {
        if !archive.matches_toolchain(self) || case.kind() != DatabaseKind::Case {
            return Err(ArchiveError::ArchiveSourceMismatch);
        }
        archive.validate()?;
        let mut source = archive.file.try_clone()?;
        source.seek(SeekFrom::Start(0))?;
        let args = restore_command_args(&self.container_id, &self.admin_role, case);
        let mut command = docker_command(&args);
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|_| ArchiveError::CommandStart(PostgresTool::Restore))?;
        let stdin = child
            .stdin
            .take()
            .ok_or(ArchiveError::CommandStart(PostgresTool::Restore))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(ArchiveError::CommandStart(PostgresTool::Restore))?;
        let stderr = child
            .stderr
            .take()
            .ok_or(ArchiveError::CommandStart(PostgresTool::Restore))?;
        let input_writer = tokio::spawn(write_archive_input(source, stdin));
        let stdout_reader = tokio::spawn(read_bounded(stdout));
        let stderr_reader = tokio::spawn(read_bounded(stderr));
        let status = wait_for_child(&mut child, PostgresTool::Restore, HOST_COMMAND_TIMEOUT).await;
        let (input, stdout, stderr) =
            finish_restore_tasks(input_writer, stdout_reader, stderr_reader).await?;
        let status = status?;
        if !status.success() {
            return Err(ArchiveError::ToolCommandFailed(PostgresTool::Restore));
        }
        input?;
        stdout?;
        stderr?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) struct ArchiveCommandPlan {
    program: &'static str,
    dump_args: Vec<String>,
    restore_args: Vec<String>,
}

#[cfg(test)]
impl ArchiveCommandPlan {
    fn new(
        container_id: &str,
        admin_role: &str,
        baseline: &DatabaseName,
        case: &DatabaseName,
    ) -> Result<Self, ArchiveError> {
        validate_container_id(container_id)?;
        validate_role_name(admin_role)?;
        if baseline.kind() != DatabaseKind::Baseline || case.kind() != DatabaseKind::Case {
            return Err(ArchiveError::InvalidCommandPlan);
        }
        let dump_args = dump_command_args(container_id, admin_role, baseline);
        let restore_args = restore_command_args(container_id, admin_role, case);
        Ok(Self {
            program: "docker",
            dump_args,
            restore_args,
        })
    }

    const fn program(&self) -> &'static str {
        self.program
    }

    fn dump_args(&self) -> &[String] {
        &self.dump_args
    }

    fn restore_args(&self) -> &[String] {
        &self.restore_args
    }
}

#[derive(Debug)]
pub(crate) struct BaselineArchive {
    file: File,
    path: PathBuf,
    directory: PathBuf,
    source_database: DatabaseName,
    container_id: String,
    server_major: u16,
}

impl BaselineArchive {
    fn create(
        source_database: DatabaseName,
        container_id: String,
        server_major: u16,
    ) -> Result<Self, ArchiveError> {
        if source_database.kind() != DatabaseKind::Baseline {
            return Err(ArchiveError::ArchiveSourceMismatch);
        }
        validate_container_id(&container_id)?;
        if server_major == 0 {
            return Err(ArchiveError::InvalidServerVersion);
        }
        let directory = std::env::temp_dir().join(format!("tiv-baseline-{}", Uuid::new_v4()));
        DirBuilder::new().mode(0o700).create(&directory)?;
        let path = directory.join("baseline.dump");
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) => {
                let _ = fs::remove_dir(&directory);
                return Err(error.into());
            }
        };
        Ok(Self {
            file,
            path,
            directory,
            source_database,
            container_id,
            server_major,
        })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn truncate_after_magic_for_test(&mut self) -> Result<(), io::Error> {
        self.file.set_len(CUSTOM_ARCHIVE_MAGIC.len() as u64)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(CUSTOM_ARCHIVE_MAGIC)?;
        self.file.sync_all()
    }

    pub(crate) fn matches_baseline(&self, baseline: &DatabaseName) -> bool {
        &self.source_database == baseline
    }

    fn matches_toolchain(&self, toolchain: &PostgresArchiveToolchain) -> bool {
        self.container_id == toolchain.container_id && self.server_major == toolchain.server_major
    }

    fn validate(&self) -> Result<(), ArchiveError> {
        let metadata = self.file.metadata()?;
        if !metadata.file_type().is_file()
            || metadata.permissions().mode() & 0o777 != 0o600
            || !(CUSTOM_ARCHIVE_MAGIC.len() as u64..=MAX_ARCHIVE_BYTES).contains(&metadata.len())
        {
            return Err(ArchiveError::InvalidArchive);
        }
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0_u8; CUSTOM_ARCHIVE_MAGIC.len()];
        file.read_exact(&mut magic)?;
        if &magic != CUSTOM_ARCHIVE_MAGIC {
            return Err(ArchiveError::InvalidArchive);
        }
        Ok(())
    }
}

impl Drop for BaselineArchive {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

fn validate_container_id(container_id: &str) -> Result<(), ArchiveError> {
    if (12..=64).contains(&container_id.len())
        && container_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(ArchiveError::InvalidContainerId)
    }
}

fn validate_role_name(role: &str) -> Result<(), ArchiveError> {
    let mut bytes = role.bytes();
    let Some(first) = bytes.next() else {
        return Err(ArchiveError::InvalidRoleName);
    };
    if role.len() <= 63
        && (first.is_ascii_lowercase() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        Ok(())
    } else {
        Err(ArchiveError::InvalidRoleName)
    }
}

fn dump_command_args(container_id: &str, admin_role: &str, baseline: &DatabaseName) -> Vec<String> {
    vec![
        "--host".to_owned(),
        LOCAL_DOCKER_HOST.to_owned(),
        "exec".to_owned(),
        container_id.to_owned(),
        "timeout".to_owned(),
        "--signal=KILL".to_owned(),
        TOOL_TIMEOUT.to_owned(),
        PostgresTool::Dump.program().to_owned(),
        "--format=custom".to_owned(),
        "--no-password".to_owned(),
        format!("--username={admin_role}"),
        format!("--dbname={}", baseline.as_str()),
    ]
}

fn restore_command_args(container_id: &str, admin_role: &str, case: &DatabaseName) -> Vec<String> {
    vec![
        "--host".to_owned(),
        LOCAL_DOCKER_HOST.to_owned(),
        "exec".to_owned(),
        "--interactive".to_owned(),
        container_id.to_owned(),
        "timeout".to_owned(),
        "--signal=KILL".to_owned(),
        TOOL_TIMEOUT.to_owned(),
        PostgresTool::Restore.program().to_owned(),
        "--single-transaction".to_owned(),
        "--exit-on-error".to_owned(),
        "--no-owner".to_owned(),
        "--no-password".to_owned(),
        format!("--username={admin_role}"),
        format!("--dbname={}", case.as_str()),
    ]
}

async fn run_tool_version(
    container_id: &str,
    tool: PostgresTool,
) -> Result<PostgresToolVersion, ArchiveError> {
    let args = vec![
        "--host".to_owned(),
        LOCAL_DOCKER_HOST.to_owned(),
        "exec".to_owned(),
        container_id.to_owned(),
        "timeout".to_owned(),
        "--signal=KILL".to_owned(),
        "10s".to_owned(),
        tool.program().to_owned(),
        "--version".to_owned(),
    ];
    let mut command = docker_command(&args);
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| ArchiveError::CommandStart(tool))?;
    let stdout = child
        .stdout
        .take()
        .ok_or(ArchiveError::CommandStart(tool))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(ArchiveError::CommandStart(tool))?;
    let stdout_reader = tokio::spawn(read_bounded(stdout));
    let stderr_reader = tokio::spawn(read_bounded(stderr));
    let status = wait_for_child(&mut child, tool, VERSION_COMMAND_TIMEOUT).await;
    let (stdout, _stderr) = finish_output_tasks(stdout_reader, stderr_reader).await?;
    let status = status?;
    if !status.success() {
        return Err(ArchiveError::ToolCommandFailed(tool));
    }
    let stdout = String::from_utf8(stdout?).map_err(|_| ArchiveError::InvalidToolVersion(tool))?;
    PostgresToolVersion::parse(tool, &stdout)
}

fn docker_command(args: &[String]) -> Command {
    let mut command = Command::new("docker");
    command
        .env_remove("DOCKER_HOST")
        .env_remove("DOCKER_CONTEXT")
        .env_remove("DOCKER_TLS_VERIFY")
        .env_remove("DOCKER_CERT_PATH")
        .args(args);
    command
}

async fn wait_for_child(
    child: &mut Child,
    tool: PostgresTool,
    command_timeout: Duration,
) -> Result<ExitStatus, ArchiveError> {
    match timeout(command_timeout, child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(_)) => {
            let _ = child.start_kill();
            let _ = timeout(CLEANUP_TIMEOUT, child.wait()).await;
            Err(ArchiveError::CommandWait(tool))
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = timeout(CLEANUP_TIMEOUT, child.wait()).await;
            Err(ArchiveError::CommandTimedOut(tool))
        }
    }
}

async fn write_bounded_archive(
    reader: impl AsyncRead + Unpin,
    file: File,
) -> Result<u64, ArchiveError> {
    let mut reader = reader.take(MAX_ARCHIVE_BYTES + 1);
    let mut file = tokio::fs::File::from_std(file);
    let bytes = tokio::io::copy(&mut reader, &mut file)
        .await
        .map_err(|_| ArchiveError::ArchiveIo)?;
    file.flush().await.map_err(|_| ArchiveError::ArchiveIo)?;
    file.sync_all().await.map_err(|_| ArchiveError::ArchiveIo)?;
    if bytes > MAX_ARCHIVE_BYTES {
        Err(ArchiveError::ArchiveTooLarge)
    } else {
        Ok(bytes)
    }
}

async fn write_archive_input(
    source: File,
    mut stdin: tokio::process::ChildStdin,
) -> Result<(), ArchiveError> {
    let mut source = tokio::fs::File::from_std(source);
    tokio::io::copy(&mut source, &mut stdin)
        .await
        .map_err(|_| ArchiveError::ArchiveIo)?;
    stdin.shutdown().await.map_err(|_| ArchiveError::ArchiveIo)
}

async fn read_bounded(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>, ArchiveError> {
    let mut output = Vec::new();
    reader
        .take(MAX_DIAGNOSTIC_BYTES + 1)
        .read_to_end(&mut output)
        .await
        .map_err(|_| ArchiveError::CommandOutput)?;
    if output.len() as u64 > MAX_DIAGNOSTIC_BYTES {
        Err(ArchiveError::CommandOutputTooLarge)
    } else {
        Ok(output)
    }
}

async fn finish_dump_tasks(
    mut writer: JoinHandle<Result<u64, ArchiveError>>,
    mut stderr: JoinHandle<Result<Vec<u8>, ArchiveError>>,
) -> Result<(Result<u64, ArchiveError>, Vec<u8>), ArchiveError> {
    let result = timeout(CLEANUP_TIMEOUT, async {
        let writer = (&mut writer)
            .await
            .map_err(|_| ArchiveError::CommandOutput)?;
        let stderr = (&mut stderr)
            .await
            .map_err(|_| ArchiveError::CommandOutput)??;
        Ok((writer, stderr))
    })
    .await;
    if let Ok(result) = result {
        result
    } else {
        writer.abort();
        stderr.abort();
        Err(ArchiveError::CommandOutput)
    }
}

async fn finish_output_tasks(
    mut stdout: JoinHandle<Result<Vec<u8>, ArchiveError>>,
    mut stderr: JoinHandle<Result<Vec<u8>, ArchiveError>>,
) -> Result<(Result<Vec<u8>, ArchiveError>, Vec<u8>), ArchiveError> {
    let result = timeout(CLEANUP_TIMEOUT, async {
        let stdout = (&mut stdout)
            .await
            .map_err(|_| ArchiveError::CommandOutput)?;
        let stderr = (&mut stderr)
            .await
            .map_err(|_| ArchiveError::CommandOutput)??;
        Ok((stdout, stderr))
    })
    .await;
    if let Ok(result) = result {
        result
    } else {
        stdout.abort();
        stderr.abort();
        Err(ArchiveError::CommandOutput)
    }
}

async fn finish_restore_tasks(
    mut input: JoinHandle<Result<(), ArchiveError>>,
    mut stdout: JoinHandle<Result<Vec<u8>, ArchiveError>>,
    mut stderr: JoinHandle<Result<Vec<u8>, ArchiveError>>,
) -> Result<
    (
        Result<(), ArchiveError>,
        Result<Vec<u8>, ArchiveError>,
        Result<Vec<u8>, ArchiveError>,
    ),
    ArchiveError,
> {
    let result = timeout(CLEANUP_TIMEOUT, async {
        let input = (&mut input)
            .await
            .map_err(|_| ArchiveError::CommandOutput)?;
        let stdout = (&mut stdout)
            .await
            .map_err(|_| ArchiveError::CommandOutput)?;
        let stderr = (&mut stderr)
            .await
            .map_err(|_| ArchiveError::CommandOutput)?;
        Ok((input, stdout, stderr))
    })
    .await;
    if let Ok(result) = result {
        result
    } else {
        input.abort();
        stdout.abort();
        stderr.abort();
        Err(ArchiveError::CommandOutput)
    }
}

#[derive(Debug, Error)]
pub(crate) enum ArchiveError {
    #[error("invalid PostgreSQL tool version banner for {0:?}")]
    InvalidToolVersion(PostgresTool),
    #[error("invalid PostgreSQL server version")]
    InvalidServerVersion,
    #[error(
        "PostgreSQL {tool:?} client major {client_major} does not match server major {server_major}"
    )]
    ClientServerVersionMismatch {
        tool: PostgresTool,
        client_major: u16,
        server_major: u16,
    },
    #[error("invalid attested Docker container ID")]
    InvalidContainerId,
    #[error("invalid PostgreSQL role name")]
    InvalidRoleName,
    #[error("invalid baseline or case database command plan")]
    InvalidCommandPlan,
    #[error("baseline archive does not match the attested toolchain or source")]
    ArchiveSourceMismatch,
    #[error("baseline archive is empty, malformed, oversized, or has unsafe permissions")]
    InvalidArchive,
    #[error("baseline archive exceeded the configured size cap")]
    ArchiveTooLarge,
    #[error("baseline archive streaming failed")]
    ArchiveIo,
    #[error("could not start the bounded PostgreSQL {0:?} command")]
    CommandStart(PostgresTool),
    #[error("bounded PostgreSQL {0:?} command timed out")]
    CommandTimedOut(PostgresTool),
    #[error("could not wait for the bounded PostgreSQL {0:?} command")]
    CommandWait(PostgresTool),
    #[error("bounded PostgreSQL command output failed")]
    CommandOutput,
    #[error("bounded PostgreSQL command output exceeded its cap")]
    CommandOutputTooLarge,
    #[error("bounded PostgreSQL {0:?} command failed")]
    ToolCommandFailed(PostgresTool),
    #[error("private baseline archive I/O failed")]
    Io(#[from] io::Error),
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::postgres::safety::DatabaseName;

    const CONTAINER_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn client_versions_must_parse_and_match_the_server_major() {
        let dump = PostgresToolVersion::parse(
            PostgresTool::Dump,
            "pg_dump (PostgreSQL) 18.4 (Debian 18.4-1.pgdg12+1)\n",
        )
        .expect("the exact pg_dump version banner parses");
        let restore = PostgresToolVersion::parse(
            PostgresTool::Restore,
            "pg_restore (PostgreSQL) 18.4 (Debian 18.4-1.pgdg12+1)\n",
        )
        .expect("the exact pg_restore version banner parses");

        assert_eq!(dump.major(), 18);
        assert_eq!(restore.major(), 18);
        assert!(dump.require_server_major(18).is_ok());
        assert!(matches!(
            dump.require_server_major(16),
            Err(ArchiveError::ClientServerVersionMismatch {
                tool: PostgresTool::Dump,
                client_major: 18,
                server_major: 16,
            })
        ));
        assert!(PostgresToolVersion::parse(PostgresTool::Dump, "PostgreSQL 18.4").is_err());
        assert!(
            PostgresToolVersion::parse(PostgresTool::Dump, "pg_restore (PostgreSQL) 18.4").is_err()
        );
    }

    #[test]
    fn archive_commands_are_local_bounded_argv_without_secrets() {
        let baseline = DatabaseName::parse("tiv_base_0123456789abcdef").expect("valid baseline");
        let case = DatabaseName::parse("tiv_case_0123456789abcdef").expect("valid case");
        let plan = ArchiveCommandPlan::new(CONTAINER_ID, "tiv_admin", &baseline, &case)
            .expect("attested inputs produce an exact plan");

        assert_eq!(plan.program(), "docker");
        assert_eq!(
            plan.dump_args(),
            [
                "--host",
                "unix:///var/run/docker.sock",
                "exec",
                CONTAINER_ID,
                "timeout",
                "--signal=KILL",
                "30s",
                "pg_dump",
                "--format=custom",
                "--no-password",
                "--username=tiv_admin",
                "--dbname=tiv_base_0123456789abcdef",
            ]
        );
        assert_eq!(
            plan.restore_args(),
            [
                "--host",
                "unix:///var/run/docker.sock",
                "exec",
                "--interactive",
                CONTAINER_ID,
                "timeout",
                "--signal=KILL",
                "30s",
                "pg_restore",
                "--single-transaction",
                "--exit-on-error",
                "--no-owner",
                "--no-password",
                "--username=tiv_admin",
                "--dbname=tiv_case_0123456789abcdef",
            ]
        );
        let rendered = format!("{:?}{:?}", plan.dump_args(), plan.restore_args());
        assert!(!rendered.contains("password="));
        assert!(!rendered.contains("postgres://"));
        assert!(!rendered.contains("postgresql://"));
    }

    #[test]
    fn archive_file_and_directory_are_private_and_removed_on_drop() {
        let archive = BaselineArchive::create(
            DatabaseName::parse("tiv_base_0123456789abcdef").expect("valid baseline"),
            CONTAINER_ID.to_owned(),
            18,
        )
        .expect("the private archive is created");
        let path = archive.path().to_owned();
        let directory = path
            .parent()
            .expect("the archive has a directory")
            .to_owned();

        assert_eq!(
            path.metadata()
                .expect("archive metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            directory
                .metadata()
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        drop(archive);
        assert!(!path.exists());
        assert!(!directory.exists());
    }
}
