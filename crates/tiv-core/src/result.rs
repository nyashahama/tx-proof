#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvalidIdentifier {
    Empty,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct InvariantId(String);

impl InvariantId {
    /// Creates an invariant identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidIdentifier::Empty`] for an empty value.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdentifier> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(InvalidIdentifier::Empty);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CheckpointId(String);

impl CheckpointId {
    /// Creates a checkpoint identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidIdentifier::Empty`] for an empty value.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdentifier> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(InvalidIdentifier::Empty);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailureIdentity {
    invariant: InvariantId,
    checkpoint: CheckpointId,
}

impl FailureIdentity {
    #[must_use]
    pub const fn new(invariant: InvariantId, checkpoint: CheckpointId) -> Self {
        Self {
            invariant,
            checkpoint,
        }
    }

    #[must_use]
    pub const fn invariant(&self) -> &InvariantId {
        &self.invariant
    }

    #[must_use]
    pub const fn checkpoint(&self) -> &CheckpointId {
        &self.checkpoint
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttemptResult {
    Held,
    Violation(FailureIdentity),
    Inconclusive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReproductionClass {
    Stable,
    Reproducible,
    Inconclusive,
}

#[must_use]
pub fn classify_reproduction(
    expected: &FailureIdentity,
    attempts: &[AttemptResult; 3],
) -> ReproductionClass {
    let matching = attempts
        .iter()
        .filter(|attempt| matches!(attempt, AttemptResult::Violation(found) if found == expected))
        .count();

    match matching {
        3 => ReproductionClass::Stable,
        2 => ReproductionClass::Reproducible,
        _ => ReproductionClass::Inconclusive,
    }
}
