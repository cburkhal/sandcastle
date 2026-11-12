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
    /// Root of the host repo this box belongs to.
    pub repo: PathBuf,
    /// Digest of the image the box was last started from.
    pub image_digest: String,
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

/// Find the root of the Git repo that contains `start`.
pub fn repo_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("no such directory: {}", start.display()))?;
    let mut dir = start.as_path();
    loop {
        if dir.join(".git").exists() {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => bail!("{} is not inside a Git repository", start.display()),
        }
    }
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
        let record = |name: &str| BoxRecord { name: name.into(), repo: "/r".into(), image_digest: "sha256:x".into() };
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
    fn repo_root_walks_up_to_the_git_directory() {
        let base = std::env::temp_dir().join(format!("sandcastle-root-test-{}", std::process::id()));
        fs::create_dir_all(base.join("repo/.git")).unwrap();
        fs::create_dir_all(base.join("repo/src/deep")).unwrap();
        let root = repo_root(&base.join("repo/src/deep")).unwrap();
        assert_eq!(root, base.join("repo").canonicalize().unwrap());
        fs::remove_dir_all(&base).unwrap();
    }
}
