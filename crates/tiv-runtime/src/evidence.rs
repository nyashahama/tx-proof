use serde::Serialize;
use thiserror::Error;
use tiv_core::result::FailureIdentity;

const EVIDENCE_SCHEMA_VERSION: u16 = 1;
const SCENARIO: &str = "commit_then_close_changed_idempotency_key";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TruthSpikeEvidence {
    schema_version: u16,
    scenario: &'static str,
    provider_object_count: usize,
    database_reset: DatabaseResetEvidence,
    failure_identity: FailureIdentityEvidence,
    fresh_replay_same_identity: bool,
}

impl TruthSpikeEvidence {
    /// Builds bounded evidence for the first truth-spike chain.
    ///
    /// # Errors
    ///
    /// Returns [`EvidenceError`] unless two provider objects were observed, the
    /// reset produced a new database OID, and fresh replay failed with the same
    /// invariant/checkpoint identity.
    pub fn new(
        provider_object_count: usize,
        before_database_oid: u32,
        after_database_oid: u32,
        first_failure: &FailureIdentity,
        replay_failure: &FailureIdentity,
    ) -> Result<Self, EvidenceError> {
        if provider_object_count < 2 {
            return Err(EvidenceError::TooFewProviderObjects);
        }
        if before_database_oid == 0
            || after_database_oid == 0
            || before_database_oid == after_database_oid
        {
            return Err(EvidenceError::DatabaseWasNotRecreated);
        }
        if first_failure != replay_failure {
            return Err(EvidenceError::ReplayFailureChanged);
        }
        Ok(Self {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            scenario: SCENARIO,
            provider_object_count,
            database_reset: DatabaseResetEvidence {
                before_oid: before_database_oid,
                after_oid: after_database_oid,
            },
            failure_identity: FailureIdentityEvidence {
                invariant_id: first_failure.invariant().as_str().to_owned(),
                checkpoint_id: first_failure.checkpoint().as_str().to_owned(),
            },
            fresh_replay_same_identity: true,
        })
    }

    /// Encodes the allowlisted evidence document.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] if serialization fails.
    pub fn to_pretty_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct DatabaseResetEvidence {
    before_oid: u32,
    after_oid: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct FailureIdentityEvidence {
    invariant_id: String,
    checkpoint_id: String,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum EvidenceError {
    #[error("truth-spike evidence requires at least two provider objects")]
    TooFewProviderObjects,
    #[error("template reset did not produce a new database identity")]
    DatabaseWasNotRecreated,
    #[error("fresh replay did not preserve the invariant/checkpoint identity")]
    ReplayFailureChanged,
}
