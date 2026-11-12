//! Secrets from Bitwarden through its CLI, `bw`.
//!
//! This is the only file that may invoke `bw`. References look like
//! `bitwarden://<item name or id>/<field>` with field `password`, `username`,
//! `uri` (the website field) or `notes`.
//!
//! Login and unlock are separate in Bitwarden: login downloads the encrypted
//! vault and lasts until `bw logout`; unlock decrypts it with the master
//! password and yields a session key, which is kept in this process only.

use std::io::Read;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use super::{Secret, SecretStore};

const BIN: &str = "bw";
/// Keychain entries for a Bitwarden personal API key (see README.md).
const KEYCHAIN_CLIENT_ID: &str = "bitwarden-client-id";
const KEYCHAIN_CLIENT_SECRET: &str = "bitwarden-client-secret";

pub struct Bitwarden {
    session: Option<Secret>,
}

impl Bitwarden {
    pub fn new() -> Self {
        Bitwarden { session: None }
    }

    fn status(&self) -> Result<String> {
        let output = Command::new(BIN)
            .arg("status")
            .stderr(Stdio::null())
            .output()
            .with_context(|| format!("could not start `{BIN}`; is the Bitwarden CLI installed?"))?;
        parse_status(&String::from_utf8_lossy(&output.stdout))
    }

    /// Log in with the API key from the Keychain if there is one, otherwise
    /// interactively.
    fn login(&self) -> Result<()> {
        let id = keychain(KEYCHAIN_CLIENT_ID);
        let secret = keychain(KEYCHAIN_CLIENT_SECRET);
        let status = if let (Some(id), Some(secret)) = (id, secret) {
            println!("Bitwarden: logging in with the API key from the macOS Keychain.");
            Command::new(BIN)
                .args(["login", "--apikey"])
                .env("BW_CLIENTID", id.expose())
                .env("BW_CLIENTSECRET", secret.expose())
                .stdout(Stdio::null())
                .status()?
        } else {
            println!("\n>> Bitwarden login (email, MASTER password, and a second factor if you use one)");
            println!("   Needed once: this Mac's Bitwarden CLI is not logged in.");
            Command::new(BIN).arg("login").stdout(Stdio::null()).status()?
        };
        if !status.success() {
            bail!("Bitwarden login failed");
        }
        Ok(())
    }
}

/// Read a generic password from the user's Keychain, if present.
fn keychain(service: &str) -> Option<Secret> {
    let user = std::env::var("USER").ok()?;
    let output = Command::new("security")
        .args(["find-generic-password", "-a", &user, "-s", service, "-w"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && !value.is_empty()).then(|| Secret::new(value))
}

fn parse_status(json: &str) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(json).context("unexpected `bw status` output")?;
    value["status"].as_str().map(str::to_string).ok_or_else(|| anyhow!("`bw status` reports no status"))
}

/// Split `<item>/<field>` and check the field.
fn parse_path(path: &str) -> Result<(&str, &str)> {
    let (item, field) = path
        .rsplit_once('/')
        .ok_or_else(|| anyhow!("a Bitwarden reference looks like bitwarden://<item>/password"))?;
    if item.is_empty() {
        bail!("the Bitwarden reference names no item");
    }
    if !["password", "username", "uri", "notes"].contains(&field) {
        bail!("unknown Bitwarden field \"{field}\" (supported: password, username, uri, notes)");
    }
    Ok((item, field))
}

impl SecretStore for Bitwarden {
    fn scheme(&self) -> &'static str {
        "bitwarden"
    }

    fn unlock(&mut self, reason: &str) -> Result<()> {
        if self.status()? == "unauthenticated" {
            self.login()?;
        }
        // The prompt itself comes from `bw` and only says "Master password".
        println!("\n>> Bitwarden MASTER password (unlocks your vault for this run only)");
        println!("   Needed to: {reason}");
        let mut child = Command::new(BIN)
            .args(["unlock", "--raw"])
            .stdin(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut session = String::new();
        child.stdout.take().expect("stdout was piped").read_to_string(&mut session)?;
        if !child.wait()?.success() || session.trim().is_empty() {
            bail!("Bitwarden unlock failed");
        }
        // `bw` leaves the cursor on the prompt line.
        println!();
        self.session = Some(Secret::new(session.trim().to_string()));
        Ok(())
    }

    fn resolve(&self, path: &str) -> Result<Secret> {
        let (item, field) = parse_path(path)?;
        let session = self.session.as_ref().ok_or_else(|| anyhow!("the Bitwarden vault is locked"))?;
        let output = Command::new(BIN)
            .args(["get", field, item])
            // By environment, not by argument: arguments show up in `ps`.
            .env("BW_SESSION", session.expose())
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            // `bw` answers "Not found." or "More than one result was found."; no secret in there.
            bail!("Bitwarden item \"{item}\": {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        let value = String::from_utf8(output.stdout).context("the Bitwarden value is not text")?.trim().to_string();
        if value.is_empty() {
            bail!("Bitwarden item \"{item}\" has an empty {field}");
        }
        Ok(Secret::new(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_read_from_json() {
        assert_eq!(parse_status(r#"{"serverUrl":null,"status":"locked"}"#).unwrap(), "locked");
        assert!(parse_status("not json").is_err());
        assert!(parse_status("{}").is_err());
    }

    #[test]
    fn references_split_into_item_and_field() {
        assert_eq!(parse_path("vshn-llm-router/password").unwrap(), ("vshn-llm-router", "password"));
        assert_eq!(parse_path("folder/item name/uri").unwrap(), ("folder/item name", "uri"));
        assert!(parse_path("item").is_err());
        assert!(parse_path("/password").is_err());
        assert!(parse_path("item/totp").is_err());
    }

    #[test]
    fn a_locked_store_resolves_nothing() {
        assert!(Bitwarden::new().resolve("item/password").is_err());
    }
}
