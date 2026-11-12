//! Secrets: values that must never reach a log line, an error message or the
//! disk.
//!
//! Only the [`Secret`] type exists so far. Where secrets come from (the
//! `SecretStore` trait and its Bitwarden implementation) arrives with build
//! step 3; until then the LLM key is read from an environment variable.

/// A secret value. It deliberately has no `Debug`, `Display` or `Clone`, so
/// it cannot be printed or copied around by accident.
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Secret(value)
    }

    /// The value itself. Call this only where the secret is actually used.
    pub fn expose(&self) -> &str {
        &self.0
    }
}
