use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

const MAX_PROVIDER_ID_BYTES: usize = 255;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidProviderId;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PaymentIntentId(String);

impl PaymentIntentId {
    /// Creates a validated Stripe-shaped `PaymentIntent` identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProviderId`] unless the value begins with `pi_`, has a
    /// non-empty suffix, contains only ASCII alphanumerics or underscores, and
    /// is at most 255 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidProviderId> {
        let value = value.into();
        validate_provider_id(&value, "pi_")?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PaymentIntentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?)
            .map_err(|_| D::Error::custom("invalid PaymentIntent ID"))
    }
}

impl fmt::Display for PaymentIntentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EventId(String);

impl EventId {
    /// Creates a validated Stripe-shaped event identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidProviderId`] unless the value begins with `evt_`, has
    /// a non-empty suffix, contains only ASCII alphanumerics or underscores,
    /// and is at most 255 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidProviderId> {
        let value = value.into();
        validate_provider_id(&value, "evt_")?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for EventId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?)
            .map_err(|_| D::Error::custom("invalid event ID"))
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn validate_provider_id(value: &str, prefix: &str) -> Result<(), InvalidProviderId> {
    let suffix = value.strip_prefix(prefix).ok_or(InvalidProviderId)?;
    if suffix.is_empty()
        || value.len() > MAX_PROVIDER_ID_BYTES
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(InvalidProviderId);
    }
    Ok(())
}
