use serde::Serialize;
use thiserror::Error;
use tiv_core::result::{AttemptResult, FailureIdentity, ReproductionClass, classify_reproduction};

const EVIDENCE_SCHEMA_VERSION: u16 = 2;
const SCENARIO: &str = "commit_then_close_changed_idempotency_key";
const ATTEMPT_COUNT: usize = 3;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TruthSpikeEvidence {
    schema_version: u16,
    scenario: &'static str,
    provider_object_counts: [usize; ATTEMPT_COUNT],
    database_resets: [DatabaseResetEvidence; ATTEMPT_COUNT - 1],
    failure_identity: FailureIdentityEvidence,
    reproduction: ReproductionEvidence,
}

impl TruthSpikeEvidence {
    /// Builds bounded evidence for three fresh-baseline truth-spike attempts.
    ///
    /// # Errors
    ///
    /// Returns [`EvidenceError`] unless every attempt observed the two-object
    /// fixture contract and both resets produced a new database OID. Held or
    /// non-matching outcomes remain valid evidence and affect classification.
    pub fn new(
        provider_object_counts: [usize; ATTEMPT_COUNT],
        database_oids: [u32; ATTEMPT_COUNT],
        expected_failure: &FailureIdentity,
        attempts: &[AttemptResult; ATTEMPT_COUNT],
    ) -> Result<Self, EvidenceError> {
        if provider_object_counts.iter().any(|count| *count < 2) {
            return Err(EvidenceError::TooFewProviderObjects);
        }
        if database_oids.contains(&0) || database_oids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(EvidenceError::DatabaseWasNotRecreated);
        }
        let matching_failure_count = attempts
            .iter()
            .filter(|attempt| {
                matches!(attempt, AttemptResult::Violation(found) if found == expected_failure)
            })
            .count();
        Ok(Self {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            scenario: SCENARIO,
            provider_object_counts,
            database_resets: [
                DatabaseResetEvidence {
                    before_oid: database_oids[0],
                    after_oid: database_oids[1],
                },
                DatabaseResetEvidence {
                    before_oid: database_oids[1],
                    after_oid: database_oids[2],
                },
            ],
            failure_identity: FailureIdentityEvidence {
                invariant_id: expected_failure.invariant().as_str().to_owned(),
                checkpoint_id: expected_failure.checkpoint().as_str().to_owned(),
            },
            reproduction: ReproductionEvidence {
                attempt_count: ATTEMPT_COUNT,
                matching_failure_count,
                classification: classify_reproduction(expected_failure, attempts).into(),
            },
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
struct ReproductionEvidence {
    attempt_count: usize,
    matching_failure_count: usize,
    classification: ReproductionClassificationEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReproductionClassificationEvidence {
    Stable,
    Reproducible,
    Inconclusive,
}

impl From<ReproductionClass> for ReproductionClassificationEvidence {
    fn from(value: ReproductionClass) -> Self {
        match value {
            ReproductionClass::Stable => Self::Stable,
            ReproductionClass::Reproducible => Self::Reproducible,
            ReproductionClass::Inconclusive => Self::Inconclusive,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum EvidenceError {
    #[error("each truth-spike attempt requires at least two provider objects")]
    TooFewProviderObjects,
    #[error("each template reset must produce a new non-zero database identity")]
    DatabaseWasNotRecreated,
}
