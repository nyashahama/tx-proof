//! Private, collision-safe configured run artifacts and strict verification.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiv_core::{
    plan::CAMPAIGN_SCHEMA_VERSION,
    trace::{CASE_TRACE_SCHEMA_VERSION, SHRINK_TRACE_SCHEMA_VERSION},
};

use crate::compatibility::{CompatibilityError, RunCompatibilityV1};

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const MAX_ARTIFACT_COUNT: usize = 2_048;
const MAX_RUN_BYTES: u64 = 25 * 1024 * 1024;
const CONFIG_FILE: &str = "config.redacted.json";
const COMPATIBILITY_FILE: &str = "compatibility.json";
const CHECKSUMS_FILE: &str = "checksums.txt";
const MANIFEST_FILE: &str = "manifest.json";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactKind {
    #[serde(rename = "configured_campaign")]
    Campaign,
    #[serde(rename = "configured_replay")]
    Replay,
    #[serde(rename = "configured_shrink")]
    Shrink,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ArtifactResult {
    Held,
    Counterexample,
    Inconclusive,
    BudgetExhausted,
}

impl ArtifactResult {
    const fn exit_code(self) -> u8 {
        match self {
            Self::Held => 0,
            Self::Counterexample => 10,
            Self::Inconclusive => 4,
            Self::BudgetExhausted => 11,
        }
    }

    const fn valid_for(self, kind: ArtifactKind) -> bool {
        matches!(
            (kind, self),
            (ArtifactKind::Campaign, Self::Held | Self::Counterexample)
                | (
                    ArtifactKind::Replay,
                    Self::Counterexample | Self::Inconclusive
                )
                | (
                    ArtifactKind::Shrink,
                    Self::Counterexample | Self::Inconclusive | Self::BudgetExhausted
                )
        )
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorktreeState {
    Clean,
    Dirty,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RepositoryStatusFormat {
    GitPorcelainV1Z,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RepositoryStatusScope {
    TrackedIndexWorktreeAndNonIgnoredUntrackedWithNonRecursiveSubmodules,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepositoryProvenance {
    commit: String,
    worktree_state: WorktreeState,
    status_format: RepositoryStatusFormat,
    status_scope: RepositoryStatusScope,
    status_digest: String,
}

impl RepositoryProvenance {
    pub(crate) fn captured(
        commit: &str,
        worktree_state: WorktreeState,
        status_digest: &str,
    ) -> Result<Self, ArtifactError> {
        if !valid_commit(commit) || !valid_digest(status_digest) {
            return Err(ArtifactError::InvalidManifest);
        }
        Ok(Self {
            commit: commit.to_owned(),
            worktree_state,
            status_format: RepositoryStatusFormat::GitPorcelainV1Z,
            status_scope:
                RepositoryStatusScope::TrackedIndexWorktreeAndNonIgnoredUntrackedWithNonRecursiveSubmodules,
            status_digest: status_digest.to_owned(),
        })
    }

    fn valid(&self) -> bool {
        valid_commit(&self.commit) && valid_digest(&self.status_digest)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceArtifactIdentity {
    manifest_schema_version: u16,
    run_id: String,
    manifest_digest: String,
    checksums_digest: String,
    compatibility_digest: String,
}

impl SourceArtifactIdentity {
    fn valid(&self) -> bool {
        matches!(self.manifest_schema_version, 1 | 2)
            && validate_run_id(&self.run_id).is_ok()
            && valid_digest(&self.manifest_digest)
            && valid_digest(&self.checksums_digest)
            && valid_digest(&self.compatibility_digest)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ManifestSeed {
    kind: ArtifactKind,
    repository: RepositoryProvenance,
    source_artifacts: Vec<SourceArtifactIdentity>,
}

impl ManifestSeed {
    pub(crate) fn new(
        kind: ArtifactKind,
        repository: RepositoryProvenance,
        source_artifacts: Vec<SourceArtifactIdentity>,
    ) -> Self {
        Self {
            kind,
            repository,
            source_artifacts,
        }
    }
}

struct VersionTwoFinalization {
    result: Option<ArtifactResult>,
    authorities: Vec<ArtifactAuthority>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuthorityRole {
    CampaignPlan,
    CampaignCaseTrace,
    ReplaySource,
    OriginalTrace,
    ShrinkSource,
    MinimizedTrace,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AuthorityFormat {
    CampaignPlan,
    SourceDocument,
    CompiledCaseTrace,
    CompiledShrinkTrace,
}

#[derive(Clone, Debug)]
pub(crate) struct ArtifactAuthority {
    role: AuthorityRole,
    path: PathBuf,
}

impl ArtifactAuthority {
    pub(crate) fn campaign_plan() -> Self {
        Self {
            role: AuthorityRole::CampaignPlan,
            path: PathBuf::from("campaign-plan.json"),
        }
    }

    pub(crate) fn campaign_case_trace(case_id: &str) -> Result<Self, ArtifactError> {
        if !valid_case_id(case_id) {
            return Err(ArtifactError::InvalidManifest);
        }
        Ok(Self {
            role: AuthorityRole::CampaignCaseTrace,
            path: PathBuf::from(format!("cases/{case_id}/trace.json")),
        })
    }

    pub(crate) fn replay_source() -> Self {
        Self {
            role: AuthorityRole::ReplaySource,
            path: PathBuf::from("source.json"),
        }
    }

    pub(crate) fn original_trace() -> Self {
        Self {
            role: AuthorityRole::OriginalTrace,
            path: PathBuf::from("trace.original.json"),
        }
    }

    pub(crate) fn shrink_source() -> Self {
        Self {
            role: AuthorityRole::ShrinkSource,
            path: PathBuf::from("source.json"),
        }
    }

    pub(crate) fn minimized_trace() -> Self {
        Self {
            role: AuthorityRole::MinimizedTrace,
            path: PathBuf::from("trace.minimized.json"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    manifest_seed: Option<ManifestSeed>,
}

impl RunArtifactStaging {
    #[cfg(test)]
    pub(crate) fn create(root: &Path, base: &Path, run_id: &str) -> Result<Self, ArtifactError> {
        Self::create_inner(root, base, run_id, None)
    }

    pub(crate) fn create_v2(
        root: &Path,
        base: &Path,
        run_id: &str,
        manifest_seed: ManifestSeed,
    ) -> Result<Self, ArtifactError> {
        Self::create_inner(root, base, run_id, Some(manifest_seed))
    }

    fn create_inner(
        root: &Path,
        base: &Path,
        run_id: &str,
        manifest_seed: Option<ManifestSeed>,
    ) -> Result<Self, ArtifactError> {
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
            manifest_seed,
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

    #[cfg(test)]
    pub(crate) fn finalize(self) -> Result<PathBuf, ArtifactError> {
        self.finalize_with_manifest(true, None, None, None)
    }

    #[cfg(test)]
    pub(crate) fn finalize_partial(
        self,
        failure_class: PartialRunClass,
        failure_code: &'static str,
    ) -> Result<PathBuf, ArtifactError> {
        if !valid_failure_code(failure_code) {
            return Err(ArtifactError::InvalidFailureCode);
        }
        self.finalize_with_manifest(false, Some(failure_class), Some(failure_code), None)
    }

    pub(crate) fn finalize_complete(
        self,
        result: ArtifactResult,
        authorities: Vec<ArtifactAuthority>,
    ) -> Result<PathBuf, ArtifactError> {
        self.finalize_with_manifest(
            true,
            None,
            None,
            Some(VersionTwoFinalization {
                result: Some(result),
                authorities,
            }),
        )
    }

    pub(crate) fn finalize_partial_v2(
        self,
        failure_class: PartialRunClass,
        failure_code: &'static str,
        authorities: Vec<ArtifactAuthority>,
    ) -> Result<PathBuf, ArtifactError> {
        if !valid_failure_code(failure_code) {
            return Err(ArtifactError::InvalidFailureCode);
        }
        self.finalize_with_manifest(
            false,
            Some(failure_class),
            Some(failure_code),
            Some(VersionTwoFinalization {
                result: None,
                authorities,
            }),
        )
    }

    fn finalize_with_manifest(
        mut self,
        complete: bool,
        failure_class: Option<PartialRunClass>,
        failure_code: Option<&'static str>,
        version_two: Option<VersionTwoFinalization>,
    ) -> Result<PathBuf, ArtifactError> {
        if version_two.is_some() != self.manifest_seed.is_some() {
            return Err(ArtifactError::InvalidManifest);
        }
        let files = collect_regular_files(&self.staging)?;
        if files
            .iter()
            .any(|path| matches!(path.as_str(), CHECKSUMS_FILE | MANIFEST_FILE))
        {
            return Err(ArtifactError::ReservedArtifact);
        }
        if complete && !files.iter().any(|path| path == COMPATIBILITY_FILE) {
            return Err(ArtifactError::MissingRequiredArtifact(PathBuf::from(
                COMPATIBILITY_FILE,
            )));
        }
        if complete && version_two.is_some() && !files.iter().any(|path| path == CONFIG_FILE) {
            return Err(ArtifactError::MissingRequiredArtifact(PathBuf::from(
                CONFIG_FILE,
            )));
        }
        if files.len().saturating_add(2) > MAX_ARTIFACT_COUNT {
            return Err(ArtifactError::TooManyArtifacts);
        }
        enforce_artifact_budget(&self.staging, &files)?;
        let mut checksums = String::new();
        let mut required_files = BTreeMap::new();
        for relative in &files {
            let digest = checksum_file(&self.staging.join(relative))?;
            writeln!(checksums, "{digest}  {relative}")
                .map_err(|_| ArtifactError::ChecksumFormatting)?;
            required_files.insert(relative.clone(), digest);
        }
        let checksums_digest = checksum_bytes(checksums.as_bytes());
        self.write_bytes(CHECKSUMS_FILE, checksums.as_bytes())?;
        let status = if complete {
            RunManifestStatus::Complete
        } else {
            RunManifestStatus::Partial
        };
        if let Some(version_two) = version_two {
            let seed = self
                .manifest_seed
                .take()
                .ok_or(ArtifactError::InvalidManifest)?;
            let artifact = build_v2_artifact(seed.kind, complete, version_two.result)?;
            let provenance =
                build_v2_provenance(seed, complete, &version_two.authorities, &required_files)?;
            self.write_json(
                MANIFEST_FILE,
                &RunManifestV2 {
                    schema_version: 2,
                    run_id: self.run_id.clone(),
                    complete,
                    status,
                    failure_class,
                    failure_code: failure_code.map(str::to_owned),
                    artifact,
                    provenance,
                    checksums_file: CHECKSUMS_FILE.to_owned(),
                    checksums_digest,
                    required_files,
                    artifact_count: files.len() + 2,
                },
            )?;
        } else {
            self.write_json(
                MANIFEST_FILE,
                &RunManifestV1 {
                    schema_version: 1,
                    run_id: self.run_id.clone(),
                    complete,
                    status,
                    failure_class,
                    failure_code: failure_code.map(str::to_owned),
                    checksums_file: CHECKSUMS_FILE.to_owned(),
                    checksums_digest,
                    required_files,
                    artifact_count: files.len() + 2,
                },
            )?;
        }
        let finalized_files = collect_regular_files(&self.staging)?;
        enforce_artifact_budget(&self.staging, &finalized_files)?;
        sync_directory(&self.staging)?;
        fs::rename(&self.staging, &self.final_path).map_err(|source| ArtifactError::Io {
            path: self.final_path.clone(),
            source,
        })?;
        sync_directory(&self.base)?;
        Ok(self.final_path)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RunManifestStatus {
    Complete,
    Partial,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunManifestV1 {
    schema_version: u16,
    run_id: String,
    complete: bool,
    status: RunManifestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_class: Option<PartialRunClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_code: Option<String>,
    checksums_file: String,
    checksums_digest: String,
    required_files: BTreeMap<String, String>,
    artifact_count: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactEnvelopeV2 {
    kind: ArtifactKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<ArtifactResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BoundFileV2 {
    path: String,
    digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BoundAuthorityV2 {
    role: AuthorityRole,
    path: String,
    format: AuthorityFormat,
    schema_version: u16,
    digest: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "state", rename_all = "snake_case")]
enum SafetyProvenanceV2 {
    NotReached,
    InitialExecutionBoundaryAttested,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestProvenanceV2 {
    repository: RepositoryProvenance,
    source_artifacts: Vec<SourceArtifactIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<BoundFileV2>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compatibility: Option<BoundFileV2>,
    safety: SafetyProvenanceV2,
    authorities: Vec<BoundAuthorityV2>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunManifestV2 {
    schema_version: u16,
    run_id: String,
    complete: bool,
    status: RunManifestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_class: Option<PartialRunClass>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_code: Option<String>,
    artifact: ArtifactEnvelopeV2,
    provenance: ManifestProvenanceV2,
    checksums_file: String,
    checksums_digest: String,
    required_files: BTreeMap<String, String>,
    artifact_count: usize,
}

fn build_v2_artifact(
    kind: ArtifactKind,
    complete: bool,
    result: Option<ArtifactResult>,
) -> Result<ArtifactEnvelopeV2, ArtifactError> {
    if complete {
        let result = result.ok_or(ArtifactError::InvalidManifest)?;
        if !result.valid_for(kind) {
            return Err(ArtifactError::InvalidManifest);
        }
        Ok(ArtifactEnvelopeV2 {
            kind,
            result: Some(result),
            exit_code: Some(result.exit_code()),
        })
    } else if result.is_none() {
        Ok(ArtifactEnvelopeV2 {
            kind,
            result: None,
            exit_code: None,
        })
    } else {
        Err(ArtifactError::InvalidManifest)
    }
}

fn build_v2_provenance(
    seed: ManifestSeed,
    complete: bool,
    authorities: &[ArtifactAuthority],
    required_files: &BTreeMap<String, String>,
) -> Result<ManifestProvenanceV2, ArtifactError> {
    if !seed.repository.valid()
        || seed.source_artifacts.iter().any(|source| !source.valid())
        || has_duplicate_source_identities(&seed.source_artifacts)
    {
        return Err(ArtifactError::InvalidManifest);
    }
    let config = bind_optional_file(required_files, CONFIG_FILE)?;
    let compatibility = bind_optional_file(required_files, COMPATIBILITY_FILE)?;
    if complete && (config.is_none() || compatibility.is_none()) {
        return Err(ArtifactError::InvalidManifest);
    }
    let authorities = bind_authorities(seed.kind, complete, authorities, required_files)?;
    let safety = if complete || compatibility.is_some() {
        SafetyProvenanceV2::InitialExecutionBoundaryAttested
    } else {
        SafetyProvenanceV2::NotReached
    };
    Ok(ManifestProvenanceV2 {
        repository: seed.repository,
        source_artifacts: seed.source_artifacts,
        config,
        compatibility,
        safety,
        authorities,
    })
}

fn bind_optional_file(
    required_files: &BTreeMap<String, String>,
    path: &'static str,
) -> Result<Option<BoundFileV2>, ArtifactError> {
    required_files.get(path).map_or(Ok(None), |digest| {
        if valid_digest(digest) {
            Ok(Some(BoundFileV2 {
                path: path.to_owned(),
                digest: digest.clone(),
            }))
        } else {
            Err(ArtifactError::InvalidManifest)
        }
    })
}

fn bind_authorities(
    kind: ArtifactKind,
    complete: bool,
    authorities: &[ArtifactAuthority],
    required_files: &BTreeMap<String, String>,
) -> Result<Vec<BoundAuthorityV2>, ArtifactError> {
    let mut bound = authorities
        .iter()
        .map(|authority| bind_authority(authority, required_files))
        .collect::<Result<Vec<_>, _>>()?;
    bound.sort_by(|left, right| (&left.role, &left.path).cmp(&(&right.role, &right.path)));
    if bound
        .windows(2)
        .any(|pair| pair[0].path == pair[1].path || pair[0] == pair[1])
        || (complete && !valid_complete_authorities(kind, &bound))
        || bound
            .iter()
            .any(|authority| !authority_valid_for_kind(kind, authority.role))
    {
        return Err(ArtifactError::InvalidManifest);
    }
    Ok(bound)
}

fn bind_authority(
    authority: &ArtifactAuthority,
    required_files: &BTreeMap<String, String>,
) -> Result<BoundAuthorityV2, ArtifactError> {
    let relative = validate_relative(&authority.path)?;
    let path = relative
        .to_str()
        .ok_or_else(|| ArtifactError::UnsafePath(relative.to_owned()))?;
    if !authority_path_valid(authority.role, path) {
        return Err(ArtifactError::InvalidManifest);
    }
    let digest = required_files
        .get(path)
        .filter(|digest| valid_digest(digest))
        .ok_or_else(|| ArtifactError::MissingRequiredArtifact(relative.to_owned()))?;
    let (format, schema_version) = authority_contract(authority.role);
    Ok(BoundAuthorityV2 {
        role: authority.role,
        path: path.to_owned(),
        format,
        schema_version,
        digest: digest.clone(),
    })
}

const fn authority_contract(role: AuthorityRole) -> (AuthorityFormat, u16) {
    match role {
        AuthorityRole::CampaignPlan => (AuthorityFormat::CampaignPlan, CAMPAIGN_SCHEMA_VERSION),
        AuthorityRole::CampaignCaseTrace | AuthorityRole::OriginalTrace => (
            AuthorityFormat::CompiledCaseTrace,
            CASE_TRACE_SCHEMA_VERSION,
        ),
        AuthorityRole::ReplaySource | AuthorityRole::ShrinkSource => {
            (AuthorityFormat::SourceDocument, 1)
        }
        AuthorityRole::MinimizedTrace => (
            AuthorityFormat::CompiledShrinkTrace,
            SHRINK_TRACE_SCHEMA_VERSION,
        ),
    }
}

fn valid_complete_authorities(kind: ArtifactKind, authorities: &[BoundAuthorityV2]) -> bool {
    let count = |role| authorities.iter().filter(|item| item.role == role).count();
    match kind {
        ArtifactKind::Campaign => {
            count(AuthorityRole::CampaignPlan) == 1
                && count(AuthorityRole::CampaignCaseTrace) >= 1
                && authorities.len() == 1 + count(AuthorityRole::CampaignCaseTrace)
        }
        ArtifactKind::Replay => {
            count(AuthorityRole::ReplaySource) == 1
                && count(AuthorityRole::OriginalTrace) == 1
                && authorities.len() == 2
        }
        ArtifactKind::Shrink => {
            count(AuthorityRole::ShrinkSource) == 1
                && count(AuthorityRole::OriginalTrace) == 1
                && count(AuthorityRole::MinimizedTrace) <= 1
                && authorities.len() == 2 + count(AuthorityRole::MinimizedTrace)
        }
    }
}

const fn authority_valid_for_kind(kind: ArtifactKind, role: AuthorityRole) -> bool {
    matches!(
        (kind, role),
        (
            ArtifactKind::Campaign,
            AuthorityRole::CampaignPlan | AuthorityRole::CampaignCaseTrace
        ) | (
            ArtifactKind::Replay,
            AuthorityRole::ReplaySource | AuthorityRole::OriginalTrace
        ) | (
            ArtifactKind::Shrink,
            AuthorityRole::ShrinkSource
                | AuthorityRole::OriginalTrace
                | AuthorityRole::MinimizedTrace
        )
    )
}

fn authority_path_valid(role: AuthorityRole, path: &str) -> bool {
    match role {
        AuthorityRole::CampaignPlan => path == "campaign-plan.json",
        AuthorityRole::CampaignCaseTrace => path
            .strip_prefix("cases/")
            .and_then(|path| path.strip_suffix("/trace.json"))
            .is_some_and(valid_case_id),
        AuthorityRole::ReplaySource | AuthorityRole::ShrinkSource => path == "source.json",
        AuthorityRole::OriginalTrace => path == "trace.original.json",
        AuthorityRole::MinimizedTrace => path == "trace.minimized.json",
    }
}

fn has_duplicate_source_identities(sources: &[SourceArtifactIdentity]) -> bool {
    let mut identities = sources
        .iter()
        .map(|source| (&source.run_id, &source.manifest_digest))
        .collect::<Vec<_>>();
    identities.sort_unstable();
    identities.windows(2).any(|pair| pair[0] == pair[1])
}

struct VerifiedManifest {
    schema_version: u16,
    run_id: String,
    checksums_digest: String,
    required_files: BTreeMap<String, String>,
    artifact_kind: Option<ArtifactKind>,
    artifact_result: Option<ArtifactResult>,
}

#[derive(Deserialize)]
struct ManifestVersionProbe {
    schema_version: u16,
}

/// A complete, private run directory whose manifest and byte digests were
/// verified without executing customer code.
pub struct VerifiedRunArtifact {
    root: PathBuf,
    run_id: String,
    manifest_schema_version: u16,
    manifest_digest: String,
    checksums_digest: String,
    compatibility_digest: String,
    artifact_kind: Option<ArtifactKind>,
    artifact_result: Option<ArtifactResult>,
    compatibility: RunCompatibilityV1,
    required_files: BTreeMap<String, String>,
}

impl VerifiedRunArtifact {
    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    #[must_use]
    pub const fn manifest_schema_version(&self) -> u16 {
        self.manifest_schema_version
    }

    #[must_use]
    pub(crate) const fn artifact_kind(&self) -> Option<ArtifactKind> {
        self.artifact_kind
    }

    #[must_use]
    pub(crate) const fn artifact_result(&self) -> Option<ArtifactResult> {
        self.artifact_result
    }

    #[must_use]
    pub(crate) fn source_identity(&self) -> SourceArtifactIdentity {
        SourceArtifactIdentity {
            manifest_schema_version: self.manifest_schema_version,
            run_id: self.run_id.clone(),
            manifest_digest: self.manifest_digest.clone(),
            checksums_digest: self.checksums_digest.clone(),
            compatibility_digest: self.compatibility_digest.clone(),
        }
    }

    #[must_use]
    pub fn compatibility_path(&self) -> PathBuf {
        self.root.join(COMPATIBILITY_FILE)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn compatibility(&self) -> &RunCompatibilityV1 {
        &self.compatibility
    }

    /// Returns the number of content files covered by the verified manifest
    /// and checksum index. The structural manifest and checksum-index files
    /// are not included in this count.
    #[must_use]
    pub fn indexed_file_count(&self) -> usize {
        self.required_files.len()
    }

    pub(crate) fn read_indexed_bytes(&self, relative: &Path) -> Result<Vec<u8>, ArtifactError> {
        let relative = validate_relative(relative)?;
        let key = relative
            .to_str()
            .ok_or_else(|| ArtifactError::UnsafePath(relative.to_owned()))?;
        let expected = self
            .required_files
            .get(key)
            .ok_or_else(|| ArtifactError::MissingRequiredArtifact(relative.to_owned()))?;
        let path = self.root.join(relative);
        let metadata = fs::symlink_metadata(&path).map_err(|source| ArtifactError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.permissions().mode() & 0o777 != FILE_MODE
        {
            return Err(ArtifactError::UnsafePath(path));
        }
        if metadata.len() > MAX_RUN_BYTES {
            return Err(ArtifactError::ArtifactTooLarge);
        }
        let file = File::open(&path).map_err(|source| ArtifactError::Io {
            path: path.clone(),
            source,
        })?;
        let mut bytes = Vec::new();
        file.take(MAX_RUN_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| ArtifactError::Io {
                path: path.clone(),
                source,
            })?;
        if u64::try_from(bytes.len()).map_or(true, |size| size > MAX_RUN_BYTES) {
            return Err(ArtifactError::ArtifactTooLarge);
        }
        if checksum_bytes(&bytes) != *expected {
            return Err(ArtifactError::DigestMismatch(relative.to_owned()));
        }
        Ok(bytes)
    }
}

/// Verifies a finalized complete run without executing customer code or
/// mutating the configured stack.
///
/// # Errors
///
/// Returns [`ArtifactError`] for unsafe paths or permissions, incomplete or
/// malformed manifests, excessive evidence, missing files, or any digest/index
/// mismatch.
pub fn verify_complete_run_artifact(root: &Path) -> Result<VerifiedRunArtifact, ArtifactError> {
    let root_metadata = fs::symlink_metadata(root).map_err(|source| ArtifactError::Io {
        path: root.to_owned(),
        source,
    })?;
    if root_metadata.file_type().is_symlink()
        || !root_metadata.is_dir()
        || root_metadata.permissions().mode() & 0o777 != DIRECTORY_MODE
    {
        return Err(ArtifactError::UnsafePath(root.to_owned()));
    }
    let files = collect_regular_files(root)?;
    enforce_artifact_budget(root, &files)?;

    for required in [MANIFEST_FILE, CHECKSUMS_FILE, COMPATIBILITY_FILE] {
        if !files.iter().any(|path| path == required) {
            return Err(ArtifactError::MissingRequiredArtifact(PathBuf::from(
                required,
            )));
        }
    }

    let manifest_bytes =
        fs::read(root.join(MANIFEST_FILE)).map_err(|source| ArtifactError::Io {
            path: root.join(MANIFEST_FILE),
            source,
        })?;
    let directory_run_id = root.file_name().and_then(|name| name.to_str());
    let manifest = decode_complete_manifest(&manifest_bytes, directory_run_id)?;
    let mut expected_files = manifest
        .required_files
        .keys()
        .cloned()
        .chain([CHECKSUMS_FILE.to_owned(), MANIFEST_FILE.to_owned()])
        .collect::<Vec<_>>();
    expected_files.sort();
    if expected_files != files {
        return Err(ArtifactError::InvalidManifest);
    }

    let mut expected_checksums = String::new();
    for (relative, expected_digest) in &manifest.required_files {
        validate_relative(Path::new(relative))?;
        if !valid_digest(expected_digest) {
            return Err(ArtifactError::InvalidManifest);
        }
        let actual = checksum_file(&root.join(relative))?;
        if actual != *expected_digest {
            return Err(ArtifactError::DigestMismatch(PathBuf::from(relative)));
        }
        writeln!(expected_checksums, "{expected_digest}  {relative}")
            .map_err(|_| ArtifactError::ChecksumFormatting)?;
    }
    if !valid_digest(&manifest.checksums_digest) {
        return Err(ArtifactError::InvalidManifest);
    }
    let checksums = fs::read(root.join(CHECKSUMS_FILE)).map_err(|source| ArtifactError::Io {
        path: root.join(CHECKSUMS_FILE),
        source,
    })?;
    if checksum_bytes(&checksums) != manifest.checksums_digest {
        return Err(ArtifactError::ChecksumsDigestMismatch);
    }
    if checksums != expected_checksums.as_bytes() {
        return Err(ArtifactError::ChecksumIndexMismatch);
    }
    let compatibility = load_compatibility(root)?;
    let compatibility_digest = manifest
        .required_files
        .get(COMPATIBILITY_FILE)
        .cloned()
        .ok_or(ArtifactError::InvalidManifest)?;
    let required_files = manifest.required_files.clone();

    Ok(VerifiedRunArtifact {
        root: root.to_owned(),
        run_id: manifest.run_id,
        manifest_schema_version: manifest.schema_version,
        manifest_digest: checksum_bytes(&manifest_bytes),
        checksums_digest: manifest.checksums_digest,
        compatibility_digest,
        artifact_kind: manifest.artifact_kind,
        artifact_result: manifest.artifact_result,
        compatibility,
        required_files,
    })
}

fn decode_complete_manifest(
    document: &[u8],
    directory_run_id: Option<&str>,
) -> Result<VerifiedManifest, ArtifactError> {
    let version: ManifestVersionProbe =
        serde_json::from_slice(document).map_err(ArtifactError::Manifest)?;
    match version.schema_version {
        1 => {
            let manifest: RunManifestV1 =
                serde_json::from_slice(document).map_err(ArtifactError::Manifest)?;
            if !common_complete_manifest_valid(
                manifest.schema_version,
                &manifest.run_id,
                manifest.complete,
                manifest.status,
                manifest.failure_class,
                manifest.failure_code.as_deref(),
                &manifest.checksums_file,
                &manifest.checksums_digest,
                &manifest.required_files,
                manifest.artifact_count,
                directory_run_id,
            ) {
                return Err(ArtifactError::InvalidManifest);
            }
            Ok(VerifiedManifest {
                schema_version: 1,
                run_id: manifest.run_id,
                checksums_digest: manifest.checksums_digest,
                required_files: manifest.required_files,
                artifact_kind: None,
                artifact_result: None,
            })
        }
        2 => {
            let manifest: RunManifestV2 =
                serde_json::from_slice(document).map_err(ArtifactError::Manifest)?;
            if !common_complete_manifest_valid(
                manifest.schema_version,
                &manifest.run_id,
                manifest.complete,
                manifest.status,
                manifest.failure_class,
                manifest.failure_code.as_deref(),
                &manifest.checksums_file,
                &manifest.checksums_digest,
                &manifest.required_files,
                manifest.artifact_count,
                directory_run_id,
            ) || !v2_complete_provenance_valid(
                &manifest.artifact,
                &manifest.provenance,
                &manifest.required_files,
            ) {
                return Err(ArtifactError::InvalidManifest);
            }
            Ok(VerifiedManifest {
                schema_version: 2,
                run_id: manifest.run_id,
                checksums_digest: manifest.checksums_digest,
                required_files: manifest.required_files,
                artifact_kind: Some(manifest.artifact.kind),
                artifact_result: manifest.artifact.result,
            })
        }
        _ => Err(ArtifactError::InvalidManifest),
    }
}

#[allow(clippy::too_many_arguments)]
fn common_complete_manifest_valid(
    schema_version: u16,
    run_id: &str,
    complete: bool,
    status: RunManifestStatus,
    failure_class: Option<PartialRunClass>,
    failure_code: Option<&str>,
    checksums_file: &str,
    checksums_digest: &str,
    required_files: &BTreeMap<String, String>,
    artifact_count: usize,
    directory_run_id: Option<&str>,
) -> bool {
    matches!(schema_version, 1 | 2)
        && complete
        && status == RunManifestStatus::Complete
        && failure_class.is_none()
        && failure_code.is_none()
        && checksums_file == CHECKSUMS_FILE
        && valid_digest(checksums_digest)
        && artifact_count == required_files.len() + 2
        && directory_run_id == Some(run_id)
        && validate_run_id(run_id).is_ok()
        && required_files.contains_key(COMPATIBILITY_FILE)
}

fn v2_complete_provenance_valid(
    artifact: &ArtifactEnvelopeV2,
    provenance: &ManifestProvenanceV2,
    required_files: &BTreeMap<String, String>,
) -> bool {
    let Some(result) = artifact.result else {
        return false;
    };
    if artifact.exit_code != Some(result.exit_code())
        || !result.valid_for(artifact.kind)
        || !provenance.repository.valid()
        || provenance
            .source_artifacts
            .iter()
            .any(|source| !source.valid())
        || has_duplicate_source_identities(&provenance.source_artifacts)
        || !valid_source_count(artifact.kind, provenance.source_artifacts.len())
        || provenance.safety != SafetyProvenanceV2::InitialExecutionBoundaryAttested
        || !bound_file_valid(provenance.config.as_ref(), CONFIG_FILE, required_files)
        || !bound_file_valid(
            provenance.compatibility.as_ref(),
            COMPATIBILITY_FILE,
            required_files,
        )
        || !valid_complete_authorities(artifact.kind, &provenance.authorities)
    {
        return false;
    }
    provenance.authorities.iter().all(|authority| {
        authority_valid_for_kind(artifact.kind, authority.role)
            && authority_path_valid(authority.role, &authority.path)
            && authority_contract(authority.role) == (authority.format, authority.schema_version)
            && validate_relative(Path::new(&authority.path)).is_ok()
            && required_files.get(&authority.path) == Some(&authority.digest)
            && valid_digest(&authority.digest)
    })
}

const fn valid_source_count(kind: ArtifactKind, count: usize) -> bool {
    match kind {
        ArtifactKind::Campaign => count == 0,
        ArtifactKind::Replay | ArtifactKind::Shrink => count == 1,
    }
}

fn bound_file_valid(
    bound: Option<&BoundFileV2>,
    expected_path: &str,
    required_files: &BTreeMap<String, String>,
) -> bool {
    bound.is_some_and(|bound| {
        bound.path == expected_path
            && valid_digest(&bound.digest)
            && required_files.get(expected_path) == Some(&bound.digest)
    })
}

fn enforce_artifact_budget(root: &Path, files: &[String]) -> Result<(), ArtifactError> {
    if files.len() > MAX_ARTIFACT_COUNT {
        return Err(ArtifactError::TooManyArtifacts);
    }
    let mut total_bytes = 0_u64;
    for relative in files {
        let size = fs::metadata(root.join(relative))
            .map_err(|source| ArtifactError::Io {
                path: root.join(relative),
                source,
            })?
            .len();
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or(ArtifactError::ArtifactTooLarge)?;
        if total_bytes > MAX_RUN_BYTES {
            return Err(ArtifactError::ArtifactTooLarge);
        }
    }
    Ok(())
}

fn load_compatibility(root: &Path) -> Result<RunCompatibilityV1, ArtifactError> {
    let path = root.join(COMPATIBILITY_FILE);
    let bytes = fs::read(&path).map_err(|source| ArtifactError::Io { path, source })?;
    RunCompatibilityV1::from_json(bytes).map_err(ArtifactError::Compatibility)
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

fn valid_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_case_id(value: &str) -> bool {
    value.starts_with("case_")
        && (6..=80).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

fn checksum_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
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
pub enum ArtifactError {
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
    #[error("complete artifact is missing required file {0}")]
    MissingRequiredArtifact(PathBuf),
    #[error("complete artifact manifest is invalid")]
    InvalidManifest,
    #[error("complete artifact manifest could not be decoded: {0}")]
    Manifest(#[source] serde_json::Error),
    #[error("complete artifact contains too many files")]
    TooManyArtifacts,
    #[error("complete artifact exceeds the v1 size budget")]
    ArtifactTooLarge,
    #[error("artifact digest does not match the manifest: {0}")]
    DigestMismatch(PathBuf),
    #[error("checksums.txt digest does not match the manifest")]
    ChecksumsDigestMismatch,
    #[error("checksums.txt does not exactly match the manifest file index")]
    ChecksumIndexMismatch,
    #[error("compatibility.json is outside the supported replay contract: {0}")]
    Compatibility(#[source] CompatibilityError),
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
    use std::{
        fs::{self, OpenOptions},
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
        path::Path,
    };

    use tiv_core::trace::{CASE_TRACE_SCHEMA_VERSION, TRACE_SCHEMA_VERSION};
    use uuid::Uuid;

    use super::{
        ArtifactAuthority, ArtifactError, ArtifactKind, ArtifactResult, ManifestSeed,
        PartialRunClass, RepositoryProvenance, RunArtifactStaging, WorktreeState,
        verify_complete_run_artifact,
    };

    #[test]
    fn finalized_run_is_private_checksummed_and_manifest_complete() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(&root, &base, "run_deadbeef").unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "held"}))
            .unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
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
        assert_eq!(
            manifest["required_files"]["compatibility.json"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(manifest["checksums_digest"].as_str().unwrap().len(), 64);
        let checksums = fs::read_to_string(final_path.join("checksums.txt")).unwrap();
        assert!(checksums.contains("  summary.json\n"));
        assert!(!checksums.contains("manifest.json"));
        let verified = verify_complete_run_artifact(&final_path).unwrap();
        assert_eq!(verified.run_id(), "run_deadbeef");
        assert_eq!(verified.manifest_schema_version(), 1);
        assert_eq!(verified.artifact_kind(), None);
        assert_eq!(verified.artifact_result(), None);
        assert_eq!(
            verified.compatibility_path(),
            final_path.join("compatibility.json")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn version_two_manifest_binds_typed_result_repository_and_authority_digests() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-v2-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let repository =
            RepositoryProvenance::captured(&"a".repeat(40), WorktreeState::Clean, &"b".repeat(64))
                .unwrap();
        let mut staging = RunArtifactStaging::create_v2(
            &root,
            &base,
            "run_manifest_v2",
            ManifestSeed::new(ArtifactKind::Campaign, repository, Vec::new()),
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
            .write_json(
                "campaign-plan.json",
                &serde_json::json!({"schema_version": 1}),
            )
            .unwrap();
        staging
            .write_json(
                "cases/case_0001/trace.json",
                &serde_json::json!({"schema_version": 3}),
            )
            .unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"verdict": "held"}))
            .unwrap();

        let final_path = staging
            .finalize_complete(
                ArtifactResult::Held,
                vec![
                    ArtifactAuthority::campaign_plan(),
                    ArtifactAuthority::campaign_case_trace("case_0001").unwrap(),
                ],
            )
            .unwrap();

        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(final_path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema_version"], 2);
        assert_eq!(manifest["artifact"]["kind"], "configured_campaign");
        assert_eq!(manifest["artifact"]["result"], "held");
        assert_eq!(manifest["artifact"]["exit_code"], 0);
        assert_eq!(
            manifest["provenance"]["repository"]["commit"],
            "a".repeat(40)
        );
        assert_eq!(
            manifest["provenance"]["repository"]["worktree_state"],
            "clean"
        );
        assert_eq!(
            manifest["provenance"]["repository"]["status_format"],
            "git_porcelain_v1_z"
        );
        assert_eq!(
            manifest["provenance"]["repository"]["status_scope"],
            "tracked_index_worktree_and_non_ignored_untracked_with_non_recursive_submodules"
        );
        assert_eq!(
            manifest["provenance"]["safety"]["state"],
            "initial_execution_boundary_attested"
        );
        assert_eq!(
            manifest["provenance"]["compatibility"]["digest"],
            manifest["required_files"]["compatibility.json"]
        );
        assert_eq!(
            manifest["provenance"]["config"]["digest"],
            manifest["required_files"]["config.redacted.json"]
        );
        assert_eq!(
            manifest["provenance"]["authorities"][0]["role"],
            "campaign_plan"
        );
        assert_eq!(
            manifest["provenance"]["authorities"][0]["digest"],
            manifest["required_files"]["campaign-plan.json"]
        );
        assert_eq!(
            manifest["provenance"]["authorities"][1]["format"],
            "compiled_case_trace"
        );
        assert_eq!(
            manifest["provenance"]["authorities"][1]["schema_version"],
            3
        );

        let verified = verify_complete_run_artifact(&final_path).unwrap();
        assert_eq!(verified.manifest_schema_version(), 2);
        assert_eq!(verified.artifact_kind(), Some(ArtifactKind::Campaign));
        assert_eq!(verified.artifact_result(), Some(ArtifactResult::Held));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_two_verifier_rejects_an_authority_relabelled_to_an_indexed_file() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-v2-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let final_path = finalized_v2_campaign(&root, "run_authority_relabel");
        let manifest_path = final_path.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["provenance"]["authorities"][0]["path"] = "summary.json".into();
        manifest["provenance"]["authorities"][0]["digest"] =
            manifest["required_files"]["summary.json"].clone();
        let mut bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        bytes.push(b'\n');
        fs::write(&manifest_path, bytes).unwrap();

        assert!(matches!(
            verify_complete_run_artifact(&final_path),
            Err(ArtifactError::InvalidManifest)
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_two_verifier_rejects_a_result_that_is_invalid_for_the_artifact_kind() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-v2-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let final_path = finalized_v2_campaign(&root, "run_result_relabel");
        let manifest_path = final_path.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["artifact"]["result"] = "inconclusive".into();
        manifest["artifact"]["exit_code"] = 4.into();
        let mut bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        bytes.push(b'\n');
        fs::write(&manifest_path, bytes).unwrap();

        assert!(matches!(
            verify_complete_run_artifact(&final_path),
            Err(ArtifactError::InvalidManifest)
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_two_replay_cryptographically_links_an_existing_version_one_source() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-v2-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut source = RunArtifactStaging::create(&root, &base, "run_legacy_source").unwrap();
        source
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        source
            .write_json("summary.json", &serde_json::json!({"verdict": "violated"}))
            .unwrap();
        let source_path = source.finalize().unwrap();
        let verified_source = verify_complete_run_artifact(&source_path).unwrap();
        let source_manifest_bytes = fs::read(source_path.join("manifest.json")).unwrap();
        let source_manifest: serde_json::Value =
            serde_json::from_slice(&source_manifest_bytes).unwrap();

        let repository =
            RepositoryProvenance::captured(&"a".repeat(40), WorktreeState::Clean, &"b".repeat(64))
                .unwrap();
        let mut replay = RunArtifactStaging::create_v2(
            &root,
            &base,
            "run_linked_replay",
            ManifestSeed::new(
                ArtifactKind::Replay,
                repository,
                vec![verified_source.source_identity()],
            ),
        )
        .unwrap();
        replay
            .write_json(
                "config.redacted.json",
                &serde_json::json!({"schema_version": 1}),
            )
            .unwrap();
        replay
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        replay
            .write_json("source.json", &serde_json::json!({"schema_version": 1}))
            .unwrap();
        replay
            .write_json(
                "trace.original.json",
                &serde_json::json!({"schema_version": CASE_TRACE_SCHEMA_VERSION}),
            )
            .unwrap();
        replay
            .write_json(
                "summary.json",
                &serde_json::json!({"classification": "stable"}),
            )
            .unwrap();
        let replay_path = replay
            .finalize_complete(
                ArtifactResult::Counterexample,
                vec![
                    ArtifactAuthority::replay_source(),
                    ArtifactAuthority::original_trace(),
                ],
            )
            .unwrap();

        let replay_manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(replay_path.join("manifest.json")).unwrap()).unwrap();
        let linked = &replay_manifest["provenance"]["source_artifacts"][0];
        assert_eq!(linked["manifest_schema_version"], 1);
        assert_eq!(linked["run_id"], "run_legacy_source");
        assert_eq!(
            linked["manifest_digest"],
            super::checksum_bytes(&source_manifest_bytes)
        );
        assert_eq!(
            linked["checksums_digest"],
            source_manifest["checksums_digest"]
        );
        assert_eq!(
            linked["compatibility_digest"],
            source_manifest["required_files"]["compatibility.json"]
        );
        assert!(verify_complete_run_artifact(&replay_path).is_ok());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn version_two_partial_manifest_never_claims_an_unreached_safety_attestation() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-v2-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let repository =
            RepositoryProvenance::captured(&"a".repeat(40), WorktreeState::Dirty, &"b".repeat(64))
                .unwrap();
        let mut staging = RunArtifactStaging::create_v2(
            &root,
            &base,
            "run_manifest_partial",
            ManifestSeed::new(ArtifactKind::Campaign, repository, Vec::new()),
        )
        .unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "failed"}))
            .unwrap();

        let final_path = staging
            .finalize_partial_v2(
                PartialRunClass::Configuration,
                "compatibility_capture",
                Vec::new(),
            )
            .unwrap();

        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(final_path.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["schema_version"], 2);
        assert_eq!(manifest["complete"], false);
        assert_eq!(manifest["status"], "partial");
        assert_eq!(manifest["provenance"]["safety"]["state"], "not_reached");
        assert!(manifest["provenance"].get("compatibility").is_none());
        assert!(matches!(
            verify_complete_run_artifact(&final_path),
            Err(ArtifactError::MissingRequiredArtifact(ref path))
                if path == Path::new("compatibility.json")
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_finalization_requires_compatibility_evidence() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(&root, &base, "run_missing").unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "held"}))
            .unwrap();

        let error = staging.finalize().unwrap_err();

        assert!(matches!(
            error,
            ArtifactError::MissingRequiredArtifact(ref path)
                if path == Path::new("compatibility.json")
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_artifact_verification_detects_file_and_checksum_index_tampering() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(&root, &base, "run_tamper").unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "held"}))
            .unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        let final_path = staging.finalize().unwrap();

        fs::write(
            final_path.join("summary.json"),
            b"{\"status\":\"changed\"}\n",
        )
        .unwrap();
        assert!(matches!(
            verify_complete_run_artifact(&final_path),
            Err(ArtifactError::DigestMismatch(ref path)) if path == Path::new("summary.json")
        ));

        fs::remove_dir_all(&root).unwrap();
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(&root, &base, "run_index").unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "held"}))
            .unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        let final_path = staging.finalize().unwrap();
        fs::write(final_path.join("checksums.txt"), b"tampered\n").unwrap();
        assert!(matches!(
            verify_complete_run_artifact(&final_path),
            Err(ArtifactError::ChecksumsDigestMismatch)
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verified_artifact_rechecks_only_manifest_indexed_files_when_they_are_loaded() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(&root, &base, "run_indexed_read").unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "held"}))
            .unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        let final_path = staging.finalize().unwrap();
        let verified = verify_complete_run_artifact(&final_path).unwrap();

        assert_eq!(
            verified
                .read_indexed_bytes(Path::new("summary.json"))
                .unwrap(),
            fs::read(final_path.join("summary.json")).unwrap()
        );
        assert!(matches!(
            verified.read_indexed_bytes(Path::new("not-indexed.json")),
            Err(ArtifactError::MissingRequiredArtifact(ref path))
                if path == Path::new("not-indexed.json")
        ));

        fs::write(
            final_path.join("summary.json"),
            b"{\"status\":\"changed\"}\n",
        )
        .unwrap();
        assert!(matches!(
            verified.read_indexed_bytes(Path::new("summary.json")),
            Err(ArtifactError::DigestMismatch(ref path)) if path == Path::new("summary.json")
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_artifact_verification_rejects_a_malformed_compatibility_contract() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(&root, &base, "run_invalid").unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"status": "held"}))
            .unwrap();
        staging
            .write_json(
                "compatibility.json",
                &serde_json::json!({"schema_version": 1, "fingerprint": "invalid"}),
            )
            .unwrap();
        let final_path = staging.finalize().unwrap();

        assert!(matches!(
            verify_complete_run_artifact(&final_path),
            Err(ArtifactError::Compatibility(_))
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finalization_rejects_an_artifact_over_the_v1_size_bound() {
        let root = std::env::temp_dir().join(format!("tiv-artifact-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let base = root.join(".tiv/runs");
        let mut staging = RunArtifactStaging::create(&root, &base, "run_oversized").unwrap();
        staging
            .write_json("compatibility.json", &compatibility_fixture())
            .unwrap();
        let oversized = staging.prepare_path("oversized.bin").unwrap();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(oversized)
            .unwrap();
        file.set_len(super::MAX_RUN_BYTES + 1).unwrap();

        assert!(matches!(
            staging.finalize(),
            Err(ArtifactError::ArtifactTooLarge)
        ));

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

    fn finalized_v2_campaign(root: &Path, run_id: &str) -> std::path::PathBuf {
        let base = root.join(".tiv/runs");
        let repository =
            RepositoryProvenance::captured(&"a".repeat(40), WorktreeState::Clean, &"b".repeat(64))
                .unwrap();
        let mut staging = RunArtifactStaging::create_v2(
            root,
            &base,
            run_id,
            ManifestSeed::new(ArtifactKind::Campaign, repository, Vec::new()),
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
            .write_json(
                "campaign-plan.json",
                &serde_json::json!({"schema_version": 1}),
            )
            .unwrap();
        staging
            .write_json(
                "cases/case_0001/trace.json",
                &serde_json::json!({"schema_version": CASE_TRACE_SCHEMA_VERSION}),
            )
            .unwrap();
        staging
            .write_json("summary.json", &serde_json::json!({"verdict": "held"}))
            .unwrap();
        staging
            .finalize_complete(
                ArtifactResult::Held,
                vec![
                    ArtifactAuthority::campaign_plan(),
                    ArtifactAuthority::campaign_case_trace("case_0001").unwrap(),
                ],
            )
            .unwrap()
    }
}
