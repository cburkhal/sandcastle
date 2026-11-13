//! Per-box state under `~/Library/Application Support/sandcastle/`.
//!
//! One JSON file per box. It records what sandcastle created, so `ls`, `down`
//! and `reset` work from any directory. No secrets are stored here.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoxRecord {
    /// Name of the box; network and volumes derive from it.
    pub name: String,
    /// The folder this box belongs to: a host repo's root, or a plain folder.
    pub repo: PathBuf,
    /// Digest of the image the box was last started from.
    pub image_digest: String,
    /// Port of the chat on `127.0.0.1`, kept across starts so the browser
    /// sees the same site again and the web UI keeps what it remembers.
    #[serde(default)]
    pub ui_port: Option<u16>,
    /// What happens when the agent asks to publish a branch (`git push`).
    /// Lives here, on the host: a repo must not widen its own access.
    #[serde(default)]
    pub push: PushMode,
}

/// What `git push` in the box leads to. The box never pushes itself: the Mac
/// fetches the branch and pushes it with the owner's own Git credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushMode {
    /// Refused. The owner takes commits with `sandcastle fetch`.
    Off,
    /// Queued until the owner approves or denies it.
    #[default]
    Ask,
    /// Published at once, except changes to CI configuration, which are queued.
    Always,
}

impl PushMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "off" => Some(PushMode::Off),
            "ask" => Some(PushMode::Ask),
            "always" => Some(PushMode::Always),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            PushMode::Off => "off",
            PushMode::Ask => "ask",
            PushMode::Always => "always",
        }
    }
}

impl BoxRecord {
    pub fn network(&self) -> String {
        self.name.clone()
    }
    pub fn home_volume(&self) -> String {
        format!("{}-home", self.name)
    }
    pub fn workspace_volume(&self) -> String {
        format!("{}-workspace", self.name)
    }
}

pub fn dir(home: &Path) -> PathBuf {
    home.join("Library/Application Support/sandcastle")
}

/// Directory for a box's files that are not state: relay log, agent config.
pub fn box_dir(home: &Path, name: &str) -> PathBuf {
    dir(home).join("boxes").join(name)
}

fn record_path(home: &Path, name: &str) -> PathBuf {
    dir(home).join("boxes").join(format!("{name}.json"))
}

pub fn save(home: &Path, record: &BoxRecord) -> Result<()> {
    let path = record_path(home, &record.name);
    fs::create_dir_all(path.parent().expect("record path has a parent"))?;
    fs::write(&path, serde_json::to_string_pretty(record)?)?;
    Ok(())
}

pub fn load(home: &Path, name: &str) -> Result<Option<BoxRecord>> {
    let path = record_path(home, name);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let record = serde_json::from_str(&text).with_context(|| format!("corrupt state file {}", path.display()))?;
    Ok(Some(record))
}

pub fn remove(home: &Path, name: &str) -> Result<()> {
    let path = record_path(home, name);
    if path.exists() {
        fs::remove_file(path)?;
    }
    let files = box_dir(home, name);
    if files.exists() {
        fs::remove_dir_all(files)?;
    }
    Ok(())
}

pub fn list(home: &Path) -> Result<Vec<BoxRecord>> {
    let boxes = dir(home).join("boxes");
    if !boxes.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(boxes)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let text = fs::read_to_string(&path)?;
            records.push(
                serde_json::from_str(&text).with_context(|| format!("corrupt state file {}", path.display()))?,
            );
        }
    }
    records.sort_by(|a: &BoxRecord, b: &BoxRecord| a.name.cmp(&b.name));
    Ok(records)
}

/// The folder a box belongs to: the root of the Git repo that contains
/// `start`, or `start` itself when it is not inside a repo. A box does not
/// need a host repo; without one it starts with an empty workspace.
pub fn box_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("no such directory: {}", start.display()))?;
    if !start.is_dir() {
        bail!("{} is not a directory", start.display());
    }
    let mut dir = start.as_path();
    loop {
        if is_repo(dir) {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return Ok(start),
        }
    }
}

/// Whether `root` is the top of a Git checkout.
pub fn is_repo(root: &Path) -> bool {
    root.join(".git").exists()
}

/// Box name for a repo: readable, unique per path, valid as a container,
/// network and volume name.
pub fn box_name(repo: &Path) -> String {
    let base = repo.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mut slug = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = &slug[..slug.len().min(24)];
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "repo" } else { slug };

    // FNV-1a over the full path keeps two repos with the same folder name apart.
    // Written out here because the hash must stay the same across releases.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in repo.to_string_lossy().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("sc-{slug}-{:08x}", hash as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_names_are_stable_readable_and_distinct() {
        let a = box_name(Path::new("/Users/me/projects/My Repo_2"));
        assert_eq!(a, box_name(Path::new("/Users/me/projects/My Repo_2")));
        assert!(a.starts_with("sc-my-repo-2-"), "{a}");
        assert_ne!(a, box_name(Path::new("/Users/me/other/My Repo_2")));
        assert!(a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'));
    }

    #[test]
    fn box_name_handles_odd_folder_names() {
        assert!(box_name(Path::new("/")).starts_with("sc-repo-"));
        assert!(box_name(Path::new("/x/---")).starts_with("sc-repo-"));
        let long = box_name(Path::new("/x/a-very-long-repository-name-that-keeps-going"));
        assert!(long.len() <= 3 + 24 + 1 + 8, "{long}");
    }

    #[test]
    fn records_round_trip_and_list_sorted() {
        let home = std::env::temp_dir().join(format!("sandcastle-state-test-{}", std::process::id()));
        let record = |name: &str| BoxRecord {
            name: name.into(),
            repo: "/r".into(),
            image_digest: "sha256:x".into(),
            ui_port: None,
            push: PushMode::Ask,
        };
        save(&home, &record("sc-b")).unwrap();
        save(&home, &record("sc-a")).unwrap();
        assert_eq!(load(&home, "sc-a").unwrap(), Some(record("sc-a")));
        assert_eq!(load(&home, "sc-missing").unwrap(), None);
        let names: Vec<String> = list(&home).unwrap().into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["sc-a", "sc-b"]);
        remove(&home, "sc-a").unwrap();
        assert_eq!(list(&home).unwrap().len(), 1);
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn box_root_walks_up_to_the_git_directory_or_stays() {
        let base = std::env::temp_dir().join(format!("sandcastle-root-test-{}", std::process::id()));
        fs::create_dir_all(base.join("repo/.git")).unwrap();
        fs::create_dir_all(base.join("repo/src/deep")).unwrap();
        let root = box_root(&base.join("repo/src/deep")).unwrap();
        assert_eq!(root, base.join("repo").canonicalize().unwrap());
        assert!(is_repo(&root));
        // A folder outside any repo is its own root.
        fs::create_dir_all(base.join("plain/sub")).unwrap();
        let plain = box_root(&base.join("plain/sub")).unwrap();
        assert_eq!(plain, base.join("plain/sub").canonicalize().unwrap());
        assert!(!is_repo(&plain));
        fs::remove_dir_all(&base).unwrap();
    }
}
