use std::{marker::PhantomData, net::Ipv4Addr};

use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatabaseKind {
    Baseline,
    Case,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseName {
    value: String,
    kind: DatabaseKind,
}

impl DatabaseName {
    /// Parses a database name from the narrow generated-name grammar.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidDatabaseName`] unless the name starts with `tiv_base_`
    /// or `tiv_case_` and ends in 8 to 32 lowercase hexadecimal characters.
    pub fn parse(value: impl Into<String>) -> Result<Self, InvalidDatabaseName> {
        let value = value.into();
        let (kind, suffix) = value
            .strip_prefix("tiv_base_")
            .map(|suffix| (DatabaseKind::Baseline, suffix))
            .or_else(|| {
                value
                    .strip_prefix("tiv_case_")
                    .map(|suffix| (DatabaseKind::Case, suffix))
            })
            .ok_or(InvalidDatabaseName)?;
        if !(8..=32).contains(&suffix.len())
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(InvalidDatabaseName);
        }
        Ok(Self { value, kind })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    #[must_use]
    pub const fn kind(&self) -> DatabaseKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidDatabaseName;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposeProjectId(String);

impl ComposeProjectId {
    /// Creates a narrow Docker Compose project identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidComposeProjectId`] for an empty, overlong, uppercase,
    /// or punctuation-bearing identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidComposeProjectId> {
        let value = value.into();
        let mut bytes = value.bytes();
        let Some(first) = bytes.next() else {
            return Err(InvalidComposeProjectId);
        };
        if value.len() > 63
            || !(first.is_ascii_lowercase() || first.is_ascii_digit())
            || !bytes.all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            return Err(InvalidComposeProjectId);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidComposeProjectId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatabaseEndpoint {
    host: Ipv4Addr,
    port: u16,
}

impl DatabaseEndpoint {
    #[must_use]
    pub const fn loopback(port: u16) -> Self {
        Self {
            host: Ipv4Addr::LOCALHOST,
            port,
        }
    }

    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkerKind {
    Baseline,
    Case,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseMarker {
    marker_uuid: Uuid,
    kind: MarkerKind,
    compose_project: ComposeProjectId,
}

impl DatabaseMarker {
    #[must_use]
    pub const fn new(
        marker_uuid: Uuid,
        kind: MarkerKind,
        compose_project: ComposeProjectId,
    ) -> Self {
        Self {
            marker_uuid,
            kind,
            compose_project,
        }
    }

    #[must_use]
    pub const fn marker_uuid(&self) -> Uuid {
        self.marker_uuid
    }

    #[must_use]
    pub const fn kind(&self) -> MarkerKind {
        self.kind
    }

    #[must_use]
    pub const fn compose_project(&self) -> &ComposeProjectId {
        &self.compose_project
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseIdentity {
    server_fingerprint: String,
    endpoint: DatabaseEndpoint,
    database_name: DatabaseName,
    database_oid: u32,
    owner_oid: u32,
    marker: DatabaseMarker,
    expected_application_role: String,
}

impl DatabaseIdentity {
    /// Creates a complete observed or expected database identity tuple.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidDatabaseIdentity`] for zero OIDs or blank identity
    /// strings.
    pub fn new(
        server_fingerprint: impl Into<String>,
        endpoint: DatabaseEndpoint,
        database_name: DatabaseName,
        database_oid: u32,
        owner_oid: u32,
        marker: DatabaseMarker,
        expected_application_role: impl Into<String>,
    ) -> Result<Self, InvalidDatabaseIdentity> {
        let server_fingerprint = server_fingerprint.into();
        let expected_application_role = expected_application_role.into();
        if database_oid == 0
            || owner_oid == 0
            || server_fingerprint.trim().is_empty()
            || expected_application_role.trim().is_empty()
        {
            return Err(InvalidDatabaseIdentity);
        }
        Ok(Self {
            server_fingerprint,
            endpoint,
            database_name,
            database_oid,
            owner_oid,
            marker,
            expected_application_role,
        })
    }

    #[must_use]
    pub fn with_marker(mut self, marker: DatabaseMarker) -> Self {
        self.marker = marker;
        self
    }

    #[must_use]
    pub fn server_fingerprint(&self) -> &str {
        &self.server_fingerprint
    }

    #[must_use]
    pub const fn endpoint(&self) -> DatabaseEndpoint {
        self.endpoint
    }

    #[must_use]
    pub const fn database_name(&self) -> &DatabaseName {
        &self.database_name
    }

    #[must_use]
    pub const fn database_oid(&self) -> u32 {
        self.database_oid
    }

    #[must_use]
    pub const fn owner_oid(&self) -> u32 {
        self.owner_oid
    }

    #[must_use]
    pub const fn marker(&self) -> &DatabaseMarker {
        &self.marker
    }

    #[must_use]
    pub fn expected_application_role(&self) -> &str {
        &self.expected_application_role
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidDatabaseIdentity;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Unverified;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Verified;

#[derive(Debug)]
pub struct DatabaseTarget<State> {
    identity: DatabaseIdentity,
    state: PhantomData<State>,
}

impl DatabaseTarget<Unverified> {
    #[must_use]
    pub const fn new(identity: DatabaseIdentity) -> Self {
        Self {
            identity,
            state: PhantomData,
        }
    }

    /// Compares a freshly observed identity and authorizes one mutation.
    ///
    /// # Errors
    ///
    /// Returns [`SafetyError`] if the expected target is not a case database or
    /// any observed identity field differs.
    pub fn verify(
        self,
        observed: &DatabaseIdentity,
    ) -> Result<(DatabaseTarget<Verified>, MutationPermit), SafetyError> {
        if self.identity.marker.kind != MarkerKind::Case
            || self.identity.database_name.kind != DatabaseKind::Case
        {
            return Err(SafetyError::ExpectedCaseMarker);
        }
        compare_identity(&self.identity, observed)?;
        Ok((
            DatabaseTarget {
                identity: observed.clone(),
                state: PhantomData,
            },
            MutationPermit { _private: () },
        ))
    }
}

impl<State> DatabaseTarget<State> {
    #[must_use]
    pub const fn identity(&self) -> &DatabaseIdentity {
        &self.identity
    }
}

#[derive(Debug)]
pub struct MutationPermit {
    _private: (),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityField {
    ServerFingerprint,
    Endpoint,
    DatabaseName,
    DatabaseOid,
    OwnerOid,
    Marker,
    ApplicationRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SafetyError {
    ExpectedCaseMarker,
    IdentityMismatch(IdentityField),
}

fn compare_identity(
    expected: &DatabaseIdentity,
    observed: &DatabaseIdentity,
) -> Result<(), SafetyError> {
    let comparisons = [
        (
            expected.server_fingerprint == observed.server_fingerprint,
            IdentityField::ServerFingerprint,
        ),
        (
            expected.endpoint == observed.endpoint,
            IdentityField::Endpoint,
        ),
        (
            expected.database_name == observed.database_name,
            IdentityField::DatabaseName,
        ),
        (
            expected.database_oid == observed.database_oid,
            IdentityField::DatabaseOid,
        ),
        (
            expected.owner_oid == observed.owner_oid,
            IdentityField::OwnerOid,
        ),
        (expected.marker == observed.marker, IdentityField::Marker),
        (
            expected.expected_application_role == observed.expected_application_role,
            IdentityField::ApplicationRole,
        ),
    ];
    comparisons
        .into_iter()
        .find_map(|(matches, field)| (!matches).then_some(field))
        .map_or(Ok(()), |field| Err(SafetyError::IdentityMismatch(field)))
}
