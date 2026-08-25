//! Versioned, secret-free compatibility contract for configured replay.

use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tiv_core::trace::{CASE_TRACE_SCHEMA_VERSION, TRACE_SCHEMA_VERSION};

use crate::{
    config::ResolvedConfig,
    configured_process::ConfiguredServiceImage,
    doctor::ComposeFacts,
    postgres::{
        probe::ConfiguredSqlProbe, quiescence::ConfiguredQuiescence, safety::DatabaseIdentity,
        snapshot::ConfiguredSnapshot,
    },
};

const COMPATIBILITY_SCHEMA_VERSION: u16 = 1;
const FIXTURE_CONTROL_PROTOCOL_VERSION: u16 = 1;
const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;

/// Complete compatibility evidence required before a configured trace can be
/// replayed against a fresh disposable baseline.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunCompatibilityV1 {
    schema_version: u16,
    tool: ToolCompatibility,
    platform_os: String,
    platform_arch: String,
    config_digest: String,
    compose: ComposeCompatibility,
    sources: Vec<SourceCompatibility>,
    baseline: BaselineCompatibility,
    services: Vec<ServiceImageCompatibility>,
}

impl RunCompatibilityV1 {
    /// Decodes and validates a compatibility document without executing any
    /// customer code.
    ///
    /// # Errors
    ///
    /// Returns [`CompatibilityError`] for malformed JSON or a value outside
    /// the closed version-one contract.
    pub fn from_json(document: impl AsRef<[u8]>) -> Result<Self, CompatibilityError> {
        let decoded: Self = serde_json::from_slice(document.as_ref())
            .map_err(|error| CompatibilityError::Decode(error.to_string()))?;
        decoded.validate()?;
        Ok(decoded)
    }

    /// Encodes the compatibility document for a private run artifact.
    ///
    /// # Errors
    ///
    /// Returns [`CompatibilityError`] if the in-memory contract is invalid or
    /// cannot be encoded.
    pub fn to_pretty_json(&self) -> Result<String, CompatibilityError> {
        self.validate()?;
        serde_json::to_string_pretty(self)
            .map_err(|error| CompatibilityError::Encode(error.to_string()))
    }

    /// Requires every compatibility-relevant fact to match exactly.
    ///
    /// # Errors
    ///
    /// Returns [`CompatibilityError::Mismatch`] for any difference. Callers
    /// must invoke this before acquiring a configured case-reset permit.
    pub fn require_exact_match(&self, current: &Self) -> Result<(), CompatibilityError> {
        self.validate()?;
        if self != current {
            return Err(CompatibilityError::Mismatch);
        }
        current.validate()?;
        Ok(())
    }

    fn validate(&self) -> Result<(), CompatibilityError> {
        if self.schema_version != COMPATIBILITY_SCHEMA_VERSION
            || self.tool.package_version.is_empty()
            || self.tool.package_version.len() > 64
            || !valid_digest(&self.tool.executable_digest)
            || self.tool.trace_schema != TRACE_SCHEMA_VERSION
            || self.tool.case_trace_schema != CASE_TRACE_SCHEMA_VERSION
            || self.tool.fixture_control_protocol != FIXTURE_CONTROL_PROTOCOL_VERSION
            || !valid_label(&self.platform_os, 64)
            || !valid_label(&self.platform_arch, 64)
            || !valid_digest(&self.config_digest)
            || !valid_label(&self.compose.version, 128)
            || !valid_digest(&self.compose.resolved_redacted_hash)
            || !strictly_sorted_unique(&self.compose.services)
            || self.sources.is_empty()
            || !self
                .sources
                .windows(2)
                .all(|pair| (&pair[0].kind, &pair[0].id) < (&pair[1].kind, &pair[1].id))
            || self
                .sources
                .iter()
                .any(|source| !valid_label(&source.id, 128) || !valid_digest(&source.digest))
            || !valid_baseline(&self.baseline)
            || self.services.is_empty()
            || !self
                .services
                .windows(2)
                .all(|pair| pair[0].service < pair[1].service)
            || self.services.iter().any(|service| {
                !valid_label(&service.service, 128)
                    || !valid_digest(&service.compose_config_hash)
                    || !valid_image_id(&service.image_id)
                    || self
                        .compose
                        .services
                        .binary_search(&service.service)
                        .is_err()
            })
        {
            return Err(CompatibilityError::InvalidDocument);
        }
        Ok(())
    }
}

/// Captures the exact, secret-free execution boundary already prepared and
/// attested for a configured campaign.
///
/// Callers must persist this document before consuming a database mutation
/// permit for the customer case database. Replays must independently capture
/// the current boundary and require an exact match first.
///
/// # Errors
///
/// Returns [`CompatibilityCaptureError`] if JSON inputs cannot be encoded, the
/// executing binary cannot be hashed within the v1 bound, or the assembled
/// contract is invalid.
pub(crate) fn capture_run_compatibility(
    config: &ResolvedConfig,
    compose: &ComposeFacts,
    baseline: &DatabaseIdentity,
    service_images: &[ConfiguredServiceImage],
    configured_probe: &ConfiguredSqlProbe,
    configured_quiescence: &ConfiguredQuiescence,
    configured_snapshot: &ConfiguredSnapshot,
) -> Result<RunCompatibilityV1, CompatibilityCaptureError> {
    let executable = std::env::current_exe().map_err(CompatibilityCaptureError::ExecutablePath)?;
    let executable_digest = digest_bounded_file(&executable, MAX_EXECUTABLE_BYTES)?;
    let invariant_sources = configured_snapshot
        .suite()
        .queries()
        .iter()
        .map(|query| (query.id(), query.sql()))
        .collect::<Vec<_>>();
    let sources = source_compatibilities(
        config.driver_body(),
        configured_probe.query().sql(),
        configured_quiescence.query().sql(),
        &invariant_sources,
    )?;
    let mut services = service_images
        .iter()
        .map(|image| ServiceImageCompatibility {
            service: image.service().to_owned(),
            compose_config_hash: image.compose_config_hash().to_owned(),
            image_id: image.image_id().to_owned(),
        })
        .collect::<Vec<_>>();
    services.sort_by(|left, right| left.service.cmp(&right.service));

    let captured = RunCompatibilityV1 {
        schema_version: COMPATIBILITY_SCHEMA_VERSION,
        tool: ToolCompatibility {
            package_version: env!("CARGO_PKG_VERSION").to_owned(),
            executable_digest,
            trace_schema: TRACE_SCHEMA_VERSION,
            case_trace_schema: CASE_TRACE_SCHEMA_VERSION,
            fixture_control_protocol: FIXTURE_CONTROL_PROTOCOL_VERSION,
        },
        platform_os: std::env::consts::OS.to_owned(),
        platform_arch: std::env::consts::ARCH.to_owned(),
        config_digest: digest_pretty_json(config.redacted())?,
        compose: ComposeCompatibility {
            version: compose.compose_version().to_owned(),
            services: compose.services().to_vec(),
            resolved_redacted_hash: compose.resolved_redacted_hash().to_owned(),
        },
        sources,
        baseline: BaselineCompatibility {
            server_fingerprint: baseline.server_fingerprint().to_owned(),
            endpoint_port: baseline.endpoint().port(),
            database_name: baseline.database_name().as_str().to_owned(),
            database_oid: baseline.database_oid(),
            owner_oid: baseline.owner_oid(),
            marker_uuid: baseline.marker().marker_uuid().to_string(),
            compose_project: baseline.marker().compose_project().as_str().to_owned(),
            application_role: baseline.expected_application_role().to_owned(),
        },
        services,
    };
    captured
        .validate()
        .map_err(CompatibilityCaptureError::Contract)?;
    Ok(captured)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolCompatibility {
    package_version: String,
    executable_digest: String,
    trace_schema: u16,
    case_trace_schema: u16,
    fixture_control_protocol: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ComposeCompatibility {
    version: String,
    services: Vec<String>,
    resolved_redacted_hash: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum SourceKind {
    DriverBody,
    SqlProbe,
    Quiescence,
    Invariant,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceCompatibility {
    kind: SourceKind,
    id: String,
    digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct BaselineCompatibility {
    server_fingerprint: String,
    endpoint_port: u16,
    database_name: String,
    database_oid: u32,
    owner_oid: u32,
    marker_uuid: String,
    compose_project: String,
    application_role: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceImageCompatibility {
    service: String,
    compose_config_hash: String,
    image_id: String,
}

fn valid_baseline(value: &BaselineCompatibility) -> bool {
    value.endpoint_port != 0
        && value.database_oid != 0
        && value.owner_oid != 0
        && valid_server_fingerprint(&value.server_fingerprint)
        && value.database_name.starts_with("tiv_base_")
        && valid_label(&value.database_name, 63)
        && uuid::Uuid::parse_str(&value.marker_uuid).is_ok()
        && valid_label(&value.compose_project, 63)
        && valid_label(&value.application_role, 63)
}

fn source_compatibilities(
    driver_body: &serde_json::Value,
    sql_probe: &str,
    quiescence: &str,
    invariants: &[(&str, &str)],
) -> Result<Vec<SourceCompatibility>, CompatibilityCaptureError> {
    let driver = serde_json::to_vec(driver_body).map_err(CompatibilityCaptureError::Json)?;
    let mut sources = vec![
        source_compatibility(SourceKind::DriverBody, "driver-body", &driver),
        source_compatibility(SourceKind::SqlProbe, "sql-probe", sql_probe.as_bytes()),
        source_compatibility(SourceKind::Quiescence, "quiescence", quiescence.as_bytes()),
    ];
    sources.extend(
        invariants
            .iter()
            .map(|(id, sql)| source_compatibility(SourceKind::Invariant, id, sql.as_bytes())),
    );
    sources.sort_by(|left, right| (&left.kind, &left.id).cmp(&(&right.kind, &right.id)));
    Ok(sources)
}

fn source_compatibility(kind: SourceKind, id: &str, bytes: &[u8]) -> SourceCompatibility {
    SourceCompatibility {
        kind,
        id: id.to_owned(),
        digest: blake3::hash(bytes).to_hex().to_string(),
    }
}

fn digest_pretty_json(value: &impl Serialize) -> Result<String, CompatibilityCaptureError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(CompatibilityCaptureError::Json)?;
    bytes.push(b'\n');
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn digest_bounded_file(path: &Path, maximum: u64) -> Result<String, CompatibilityCaptureError> {
    let file = File::open(path).map_err(|source| CompatibilityCaptureError::ExecutableRead {
        path: path.to_owned(),
        source,
    })?;
    let mut reader = file.take(maximum + 1);
    let mut hasher = blake3::Hasher::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(|source| {
            CompatibilityCaptureError::ExecutableRead {
                path: path.to_owned(),
                source,
            }
        })?;
        if read == 0 {
            break;
        }
        total += u64::try_from(read).expect("buffer reads fit in u64");
        if total > maximum {
            return Err(CompatibilityCaptureError::ExecutableTooLarge);
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn valid_server_fingerprint(value: &str) -> bool {
    value
        .strip_prefix("postgres-system-id:")
        .is_some_and(|identifier| {
            !identifier.is_empty()
                && identifier.len() <= 64
                && identifier.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn valid_label(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_image_id(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(valid_digest)
}

fn strictly_sorted_unique(values: &[String]) -> bool {
    !values.is_empty() && values.windows(2).all(|pair| pair[0] < pair[1])
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CompatibilityError {
    #[error("compatibility document could not be decoded: {0}")]
    Decode(String),
    #[error("compatibility document could not be encoded: {0}")]
    Encode(String),
    #[error("compatibility document is outside the v1 contract")]
    InvalidDocument,
    #[error("recorded run compatibility does not match the current execution boundary")]
    Mismatch,
}

#[derive(Debug, Error)]
pub enum CompatibilityCaptureError {
    #[error("compatibility JSON input could not be encoded: {0}")]
    Json(#[source] serde_json::Error),
    #[error("the current executable path could not be resolved: {0}")]
    ExecutablePath(#[source] std::io::Error),
    #[error("the current executable could not be read at {path}: {source}")]
    ExecutableRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the current executable exceeds the v1 compatibility size bound")]
    ExecutableTooLarge,
    #[error("captured execution facts are outside the compatibility contract: {0}")]
    Contract(#[source] CompatibilityError),
}

impl CompatibilityCaptureError {
    #[must_use]
    pub fn is_infrastructure_failure(&self) -> bool {
        matches!(
            self,
            Self::ExecutablePath(_) | Self::ExecutableRead { .. } | Self::ExecutableTooLarge
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BaselineCompatibility, CompatibilityError, ComposeCompatibility, RunCompatibilityV1,
        ServiceImageCompatibility, SourceCompatibility, SourceKind, ToolCompatibility,
        digest_pretty_json, source_compatibilities,
    };

    #[test]
    fn compatibility_comparison_fails_closed_for_every_execution_boundary() {
        let recorded = fixture();

        let mut changed = recorded.clone();
        changed.tool.case_trace_schema += 1;
        assert_eq!(
            recorded.require_exact_match(&changed),
            Err(CompatibilityError::Mismatch)
        );

        let mut changed = recorded.clone();
        changed.config_digest.replace_range(..1, "b");
        assert_eq!(
            recorded.require_exact_match(&changed),
            Err(CompatibilityError::Mismatch)
        );

        let mut changed = recorded.clone();
        changed
            .compose
            .resolved_redacted_hash
            .replace_range(..1, "b");
        assert_eq!(
            recorded.require_exact_match(&changed),
            Err(CompatibilityError::Mismatch)
        );

        let mut changed = recorded.clone();
        changed.services[0].image_id.replace_range(7..8, "b");
        assert_eq!(
            recorded.require_exact_match(&changed),
            Err(CompatibilityError::Mismatch)
        );

        let mut changed = recorded.clone();
        changed.services[0]
            .compose_config_hash
            .replace_range(..1, "b");
        assert_eq!(
            recorded.require_exact_match(&changed),
            Err(CompatibilityError::Mismatch)
        );

        let mut changed = recorded.clone();
        changed.baseline.server_fingerprint.push('x');
        assert_eq!(
            recorded.require_exact_match(&changed),
            Err(CompatibilityError::Mismatch)
        );
    }

    #[test]
    fn compatibility_document_round_trips_but_rejects_unknown_or_malformed_fields() {
        let document = fixture();
        let bytes = document.to_pretty_json().unwrap();

        assert_eq!(RunCompatibilityV1::from_json(&bytes).unwrap(), document);

        let unknown = bytes.replacen('{', "{\"unexpected\":true,", 1);
        assert!(matches!(
            RunCompatibilityV1::from_json(unknown.as_bytes()),
            Err(CompatibilityError::Decode(_))
        ));

        let malformed = bytes.replacen(&"a".repeat(64), "not-a-digest", 1);
        assert_eq!(
            RunCompatibilityV1::from_json(malformed.as_bytes()),
            Err(CompatibilityError::InvalidDocument)
        );
    }

    #[test]
    fn compatibility_document_binds_the_exact_postgres_system_identifier() {
        let document = fixture();
        assert!(document.validate().is_ok());

        for invalid in [
            "postgres-system-id:",
            "postgres-system-id:not-digits",
            "other-system-id:123456789",
        ] {
            let mut changed = document.clone();
            changed.baseline.server_fingerprint = invalid.to_owned();
            assert_eq!(changed.validate(), Err(CompatibilityError::InvalidDocument));
        }
    }

    #[test]
    fn service_image_must_belong_to_the_resolved_compose_graph() {
        let mut document = fixture();
        document.services[0].service = "unconfigured-service".to_owned();

        assert_eq!(
            document.validate(),
            Err(CompatibilityError::InvalidDocument)
        );
    }

    #[test]
    fn capture_hashes_the_executed_sources_and_orders_invariants_lexically() {
        let driver = serde_json::json!({"operation_id": "checkout-1"});
        let invariants = [("z-invariant", "SELECT 2"), ("a-invariant", "SELECT 1")];

        let sources =
            source_compatibilities(&driver, "SELECT probe", "SELECT quiescence", &invariants)
                .unwrap();

        assert_eq!(
            sources
                .iter()
                .map(|source| (source.kind, source.id.as_str()))
                .collect::<Vec<_>>(),
            [
                (SourceKind::DriverBody, "driver-body"),
                (SourceKind::SqlProbe, "sql-probe"),
                (SourceKind::Quiescence, "quiescence"),
                (SourceKind::Invariant, "a-invariant"),
                (SourceKind::Invariant, "z-invariant"),
            ]
        );
        assert_eq!(
            sources[0].digest,
            blake3::hash(&serde_json::to_vec(&driver).unwrap())
                .to_hex()
                .to_string()
        );
        assert_eq!(
            sources[3].digest,
            blake3::hash(b"SELECT 1").to_hex().to_string()
        );
    }

    #[test]
    fn redacted_config_digest_matches_the_pretty_artifact_bytes() {
        let redacted = serde_json::json!({"schema_version": 1});
        let expected = blake3::hash(b"{\n  \"schema_version\": 1\n}\n")
            .to_hex()
            .to_string();

        assert_eq!(digest_pretty_json(&redacted).unwrap(), expected);
    }

    fn fixture() -> RunCompatibilityV1 {
        RunCompatibilityV1 {
            schema_version: 1,
            tool: ToolCompatibility {
                package_version: "0.0.0".to_owned(),
                executable_digest: "a".repeat(64),
                trace_schema: 1,
                case_trace_schema: 3,
                fixture_control_protocol: 1,
            },
            platform_os: "linux".to_owned(),
            platform_arch: "x86_64".to_owned(),
            config_digest: "a".repeat(64),
            compose: ComposeCompatibility {
                version: "5.5.0".to_owned(),
                services: vec![
                    "postgres".to_owned(),
                    "reference-app".to_owned(),
                    "stripe-fixture".to_owned(),
                ],
                resolved_redacted_hash: "a".repeat(64),
            },
            sources: vec![SourceCompatibility {
                kind: SourceKind::Invariant,
                id: "provider-object-unique".to_owned(),
                digest: "a".repeat(64),
            }],
            baseline: BaselineCompatibility {
                server_fingerprint: "postgres-system-id:123456789".to_owned(),
                endpoint_port: 15_432,
                database_name: "tiv_base_deadbeef".to_owned(),
                database_oid: 16_384,
                owner_oid: 10,
                marker_uuid: "00000000-0000-4000-8000-000000000001".to_owned(),
                compose_project: "tiv-reference-app-spike".to_owned(),
                application_role: "tiv_app".to_owned(),
            },
            services: vec![ServiceImageCompatibility {
                service: "reference-app".to_owned(),
                compose_config_hash: "a".repeat(64),
                image_id: format!("sha256:{}", "a".repeat(64)),
            }],
        }
    }
}
