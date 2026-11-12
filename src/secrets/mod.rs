//! Secrets module: where secret values come from.
//!
//! The core only talks to [`SecretStore`]. The config file holds references
//! such as `bitwarden://<item>/password`, never values. Every reference is
//! resolved once, in the secrets phase at the start of `up`; the values then
//! live in memory only.

pub mod bitwarden;

use anyhow::{anyhow, bail, Result};

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

pub trait SecretStore {
    /// URI scheme of this store's references, for example `bitwarden`.
    fn scheme(&self) -> &'static str;
    /// Make the store readable. This is the only call that may prompt, and it
    /// must say which password it asks for and why.
    fn unlock(&mut self, reason: &str) -> Result<()>;
    /// Resolve the part of a reference after `<scheme>://`. Never prompts.
    fn resolve(&self, path: &str) -> Result<Secret>;
}

/// Resolves the values in the config file. Unlocks the store only if a value
/// actually refers to it, so a config without references never prompts.
pub struct Resolver<'a> {
    store: &'a mut dyn SecretStore,
    unlocked: bool,
}

impl<'a> Resolver<'a> {
    pub fn new(store: &'a mut dyn SecretStore) -> Self {
        Resolver { store, unlocked: false }
    }

    /// A value that must not be written into the config itself.
    pub fn secret(&mut self, value: &str, what: &str) -> Result<Secret> {
        match self.reference(value, what)? {
            Some(secret) => Ok(secret),
            None => bail!(
                "{what} must be a reference such as {}://<item>/password, not the value itself",
                self.store.scheme()
            ),
        }
    }

    /// A value that may be written into the config or be a reference.
    pub fn plain_or_reference(&mut self, value: &str, what: &str) -> Result<String> {
        Ok(match self.reference(value, what)? {
            Some(secret) => secret.expose().to_string(),
            None => value.to_string(),
        })
    }

    fn reference(&mut self, value: &str, what: &str) -> Result<Option<Secret>> {
        // `env://NAME` reads an environment variable: for tests and for
        // setups without a password manager.
        if let Some(name) = value.strip_prefix("env://") {
            let found = std::env::var(name).map_err(|_| anyhow!("{what}: the environment variable {name} is not set"))?;
            return Ok(Some(Secret::new(found)));
        }
        let prefix = format!("{}://", self.store.scheme());
        let Some(path) = value.strip_prefix(&prefix) else {
            return Ok(None);
        };
        if !self.unlocked {
            self.store.unlock(&format!("read {what}"))?;
            self.unlocked = true;
        }
        self.store.resolve(path).map(Some).map_err(|error| error.context(format!("could not read {what}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeStore {
        unlocks: u32,
    }

    impl SecretStore for FakeStore {
        fn scheme(&self) -> &'static str {
            "fake"
        }
        fn unlock(&mut self, _reason: &str) -> Result<()> {
            self.unlocks += 1;
            Ok(())
        }
        fn resolve(&self, path: &str) -> Result<Secret> {
            Ok(Secret::new(format!("value-of-{path}")))
        }
    }

    #[test]
    fn references_are_resolved_and_the_store_unlocks_once() {
        let mut store = FakeStore { unlocks: 0 };
        let mut resolver = Resolver::new(&mut store);
        assert_eq!(resolver.secret("fake://item/password", "the key").unwrap().expose(), "value-of-item/password");
        assert_eq!(resolver.plain_or_reference("fake://item/uri", "the URL").unwrap(), "value-of-item/uri");
        assert_eq!(store.unlocks, 1);
    }

    #[test]
    fn plain_values_never_unlock_the_store() {
        let mut store = FakeStore { unlocks: 0 };
        let mut resolver = Resolver::new(&mut store);
        assert_eq!(resolver.plain_or_reference("https://gw.example", "the URL").unwrap(), "https://gw.example");
        assert_eq!(store.unlocks, 0);
    }

    #[test]
    fn a_secret_written_into_the_config_is_rejected() {
        let mut store = FakeStore { unlocks: 0 };
        let error = Resolver::new(&mut store).secret("sk-the-actual-key", "the LLM key").err().unwrap().to_string();
        assert!(error.contains("must be a reference"), "{error}");
        assert!(!error.contains("sk-the-actual-key"), "the value must not be echoed");
    }

    #[test]
    fn env_references_read_the_environment() {
        let mut store = FakeStore { unlocks: 0 };
        let mut resolver = Resolver::new(&mut store);
        assert_eq!(resolver.secret("env://HOME", "x").unwrap().expose(), std::env::var("HOME").unwrap());
        assert!(resolver.secret("env://SANDCASTLE_SURELY_UNSET", "x").is_err());
        assert_eq!(store.unlocks, 0);
    }
}
