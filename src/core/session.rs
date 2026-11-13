//! A session: everything that needs a password, obtained once.
//!
//! Opening a session is the secrets phase: `sudo` for the firewall helper,
//! the secret store's master password, and every secret the config refers
//! to. Nothing prompts afterwards. Any number of boxes can then start and
//! stop. The CLI opens one session per `up`; the app keeps one while it runs.
//! Dropping the session drops the secrets and removes all firewall rules.

use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};

use super::config::{self, Config};
use super::lifecycle::App;
use crate::firewall::{self, Firewall, Prompt};
use crate::secrets::{Resolver, Secret, SecretStore};

pub struct Session {
    pub config: Config,
    /// The LLM gateway, with the URL already resolved.
    pub llm_base_url: String,
    pub llm_key: Secret,
    /// `None`: no firewall rules (testing without root).
    firewall: Option<Mutex<Firewall>>,
}

impl Session {
    /// The only code that may prompt. `sudo` comes first, then the store.
    pub fn open(cx: &App, store: &mut dyn SecretStore, sudo: Option<Prompt>) -> Result<Self> {
        let config = config::load(cx.home)?;
        let llm = config.llm.as_ref().ok_or_else(|| {
            anyhow!("no LLM gateway configured: add [llm] base_url to {}", config::path(cx.home).display())
        })?;

        let firewall = match sudo {
            Some(prompt) => {
                if !firewall::anchor_configured()? {
                    bail!("{} lacks sandcastle's anchor line; run `sandcastle init`", firewall::PF_CONF);
                }
                Some(Mutex::new(Firewall::start(prompt)?))
            }
            None => None,
        };

        let mut resolver = Resolver::new(store);
        let llm_base_url = resolver.plain_or_reference(&llm.base_url, "the LLM gateway URL")?;
        let llm_key = resolver.secret(&llm.api_key, "the LLM key")?;
        Ok(Session { config, llm_base_url, llm_key, firewall })
    }

    pub fn has_firewall(&self) -> bool {
        self.firewall.is_some()
    }

    /// Confine a box. Until this returns, the box must not do anything.
    pub fn firewall_load(&self, box_name: &str, rules: &[String]) -> Result<()> {
        match &self.firewall {
            Some(firewall) => lock(firewall)?.load(box_name, rules),
            None => Ok(()),
        }
    }

    /// Remove the rules of a box. Call only after the box is gone.
    pub fn firewall_unload(&self, box_name: &str) -> Result<()> {
        match &self.firewall {
            Some(firewall) => lock(firewall)?.unload(box_name),
            None => Ok(()),
        }
    }
}

fn lock(firewall: &Mutex<Firewall>) -> Result<std::sync::MutexGuard<'_, Firewall>> {
    firewall.lock().map_err(|_| anyhow!("the firewall helper is in a broken state"))
}
