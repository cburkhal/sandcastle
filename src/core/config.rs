//! `~/.config/sandcastle/config.toml`: secret *references* only, never values.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Written by `sandcastle init` when no config exists yet.
pub const DEFAULT_CONFIG: &str = r#"# sandcastle configuration. Secret references only, never secret values.

[agent]
kind = "opencode"

[runtime]
kind = "apple-container"

# The OpenAI-compatible gateway the agent's model requests go to. Fill this in.
# The key is not stored here: until the Bitwarden integration exists it is
# read from the environment variable SANDCASTLE_LLM_KEY.
# [llm]
# base_url = "https://<litellm-host>"

# Hosts the box may open HTTPS connections to, through the relay. Exact names.
[egress]
allow = ["registry.npmjs.org", "github.com", "codeload.github.com"]

# Limits for each box.
[limits]
cpus = 4
memory = "4G"
"#;

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub agent: Kind,
    pub runtime: Kind,
    pub llm: Option<Llm>,
    #[serde(default)]
    pub egress: Egress,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Llm {
    pub base_url: String,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Egress {
    pub allow: Vec<String>,
}

impl Default for Egress {
    fn default() -> Self {
        Egress { allow: ["registry.npmjs.org", "github.com", "codeload.github.com"].map(String::from).to_vec() }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Kind {
    pub kind: String,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub cpus: u32,
    pub memory: String,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { cpus: 4, memory: "4G".into() }
    }
}

pub fn path(home: &Path) -> PathBuf {
    home.join(".config/sandcastle/config.toml")
}

pub fn parse(text: &str) -> Result<Config> {
    let config: Config = toml::from_str(text)?;
    // One implementation per module exists so far; name anything else clearly.
    if config.agent.kind != "opencode" {
        bail!("unknown agent kind \"{}\" (supported: \"opencode\")", config.agent.kind);
    }
    if config.runtime.kind != "apple-container" {
        bail!("unknown runtime kind \"{}\" (supported: \"apple-container\")", config.runtime.kind);
    }
    Ok(config)
}

pub fn load(home: &Path) -> Result<Config> {
    let path = path(home);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("no config at {}; run `sandcastle init` first", path.display()))?;
    parse(&text).with_context(|| format!("invalid config {}", path.display()))
}

/// Write the default config unless one exists. Returns `true` if it was written.
pub fn write_default(home: &Path) -> Result<bool> {
    let path = path(home);
    if path.exists() {
        return Ok(false);
    }
    fs::create_dir_all(path.parent().expect("config path has a parent"))?;
    fs::write(&path, DEFAULT_CONFIG)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let config = parse(DEFAULT_CONFIG).unwrap();
        assert_eq!(config.agent.kind, "opencode");
        assert_eq!(config.limits, Limits::default());
        assert_eq!(config.egress, Egress::default());
        assert_eq!(config.llm, None, "the gateway URL is for the user to fill in");
    }

    #[test]
    fn llm_and_egress_are_read() {
        let config = parse(
            "[agent]\nkind = \"opencode\"\n[runtime]\nkind = \"apple-container\"\n\
             [llm]\nbase_url = \"https://gw.example/api\"\n[egress]\nallow = [\"example.org\"]\n",
        )
        .unwrap();
        assert_eq!(config.llm.unwrap().base_url, "https://gw.example/api");
        assert_eq!(config.egress.allow, ["example.org"]);
    }

    #[test]
    fn limits_are_optional() {
        let config = parse("[agent]\nkind = \"opencode\"\n[runtime]\nkind = \"apple-container\"\n").unwrap();
        assert_eq!(config.limits.cpus, 4);
    }

    #[test]
    fn unknown_kinds_and_keys_are_rejected() {
        assert!(parse("[agent]\nkind = \"other\"\n[runtime]\nkind = \"apple-container\"\n").is_err());
        assert!(parse("[agent]\nkind = \"opencode\"\n[runtime]\nkind = \"docker\"\n").is_err());
        assert!(parse("[agent]\nkind = \"opencode\"\ntypo = 1\n[runtime]\nkind = \"apple-container\"\n").is_err());
    }
}
