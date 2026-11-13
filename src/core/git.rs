//! Git between the host repo and the box: clone-and-fetch (CLAUDE.md 2.5).
//!
//! The box never gets write access to the host repo. It clones from a
//! read-only mount of the host's `.git` directory, and the host pulls the
//! box's commits out with `git fetch`, running Git's server side inside the
//! box. Nothing the agent puts into its own `.git` (hooks, config keys) can
//! therefore run on the Mac. Verified in spike item 7.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::lifecycle::App;
use super::state;
use crate::runtime::BoxState;

/// Where the host's `.git` directory is mounted in the box, read-only.
pub const SOURCE_MOUNT: &str = "/source.git";
/// Remote-tracking namespace for the box's branches in the host repo.
const REMOTE: &str = "sandcastle";

/// The host repo's `.git` directory, after checking that mounting it into the
/// box gives nothing away.
pub fn source_dir(repo: &Path) -> Result<PathBuf> {
    let git_dir = repo.join(".git");
    if !git_dir.is_dir() {
        // Linked worktrees and submodules have a `.git` file that points elsewhere.
        bail!("{} is a linked worktree or submodule; run sandcastle in the main checkout", repo.display());
    }
    let config = fs::read_to_string(git_dir.join("config")).unwrap_or_default();
    if let Some(reason) = config_leak(&config) {
        bail!(
            "{}/config {reason}. The box could read it through the source mount; \
             move the credential to a credential helper first",
            git_dir.display()
        );
    }
    Ok(git_dir)
}

/// Why a `.git/config` must not be shown to the box, if there is a reason.
fn config_leak(config: &str) -> Option<&'static str> {
    for line in config.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        if key == "extraheader" {
            return Some("sets an HTTP extra header, which usually carries a token");
        }
        if url_has_credentials(value) {
            return Some("has a URL with a user name or token in it");
        }
    }
    None
}

/// `https://user:token@host/...` and `https://token@host/...`. SSH URLs such
/// as `ssh://git@host/...` carry a user name but no secret.
fn url_has_credentials(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once("://") else { return false };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    rest.split('/').next().is_some_and(|authority| authority.contains('@'))
}

/// Make sure the box has its clone, and bring it up to date with the host.
/// Runs inside the box before the agent starts. Without a host repo
/// (`has_source` false) the box gets an empty folder to start in; the agent
/// can create or clone a repository there.
pub fn seed(cx: &App, box_name: &str, has_source: bool) -> Result<()> {
    let repo_dir = cx.agent.repo_dir();
    let run = |args: &[&str]| -> Result<bool> {
        let command: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        Ok(cx.runtime.exec(box_name, &command, None)?.success)
    };
    let must = |args: &[&str], what: &str| -> Result<()> {
        let command: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let output = cx.runtime.exec(box_name, &command, None)?;
        if !output.success {
            bail!("{what} failed in the box: {}", output.stderr.trim());
        }
        Ok(())
    };

    if !run(&["git", "config", "--global", "user.name"])? {
        must(&["git", "config", "--global", "user.name", "sandcastle agent"], "git config user.name")?;
        must(&["git", "config", "--global", "user.email", "agent@sandcastle.invalid"], "git config user.email")?;
    }
    if !has_source {
        return must(&["mkdir", "-p", &repo_dir], "creating the workspace folder");
    }

    // The mount shows up as owned by root; Git refuses it without this (spike item 7).
    // `--replace-all` keeps the setting single across restarts.
    must(
        &["git", "config", "--global", "--replace-all", "safe.directory", SOURCE_MOUNT],
        "git config safe.directory",
    )?;

    if run(&["test", "-d", &format!("{repo_dir}/.git")])? {
        // New host commits become visible as origin/*; the agent's branches are left alone.
        if !run(&["git", "-C", &repo_dir, "fetch", "--quiet", "origin"])? {
            eprintln!("Note: the box could not fetch new host commits; its clone is unchanged.");
        }
        return Ok(());
    }
    println!("First start for this repo: cloning it into the box.");
    must(&["git", "clone", "--quiet", SOURCE_MOUNT, &repo_dir], "git clone")
}

/// `sandcastle fetch`: bring the box's commits into the host repo under
/// `refs/remotes/sandcastle/*`. Nothing is checked out or merged.
pub fn fetch(cx: &App, path: &Path) -> Result<()> {
    let listing = fetch_refs(cx, path)?;
    if listing.is_empty() {
        println!("The box has no branches yet.");
        return Ok(());
    }
    println!("Branches from the box, now in this repo:");
    for line in &listing {
        println!("  {line}");
    }
    println!("Nothing was checked out or merged. Treat these like a pull request from a stranger:");
    println!("  git log -p HEAD..{REMOTE}/<branch>     review");
    println!("  git merge {REMOTE}/<branch>            take the changes");
    Ok(())
}

/// The fetch itself. Returns one line per branch of the box:
/// `sandcastle/<branch>  <commit>  <subject>`.
pub fn fetch_refs(cx: &App, path: &Path) -> Result<Vec<String>> {
    let repo = state::box_root(path)?;
    let name = state::box_name(&repo);
    if !state::is_repo(&repo) {
        bail!(
            "{} is not a Git repository, so there is nowhere to fetch into.              Run `git init` there first, then fetch again",
            repo.display()
        );
    }
    if cx.runtime.inspect(&name)?.state != BoxState::Running {
        // The workspace volume can only be read through the running box.
        bail!("box {name} is not running; start it with `sandcastle up`, then fetch from another terminal");
    }

    let repo_dir = cx.agent.repo_dir();
    let has_repo = cx.runtime.exec(&name, &["test".into(), "-d".into(), format!("{repo_dir}/.git")], None)?.success;
    if !has_repo {
        bail!("the agent has no Git repository in {repo_dir} yet, so there is nothing to fetch");
    }

    // Git runs this command and speaks its protocol over stdin/stdout, so the
    // server side (`upload-pack`) runs in the box and the host only reads a pack.
    let server = cx.runtime.exec_argv(&name, &["git", "upload-pack", &cx.agent.repo_dir()]);
    if server.iter().any(|word| word.contains(' ') || word.contains('%')) {
        bail!("cannot build the fetch command: an argument contains a space");
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(&repo)
        // The `ext` transport is off by default; it is enabled for this one command only.
        .args(["-c", "protocol.ext.allow=always", "fetch", "--prune"])
        .arg(format!("ext::{}", server.join(" ")))
        .arg(format!("+refs/heads/*:refs/remotes/{REMOTE}/*"))
        .status()
        .context("could not run git")?;
    if !status.success() {
        bail!("git fetch from the box failed");
    }

    let branches = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["for-each-ref", "--format=%(refname:short)  %(objectname:short)  %(subject)"])
        .arg(format!("refs/remotes/{REMOTE}/"))
        .output()?;
    Ok(String::from_utf8_lossy(&branches.stdout).lines().map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_in_urls_are_detected() {
        for url in ["https://user:tok@github.com/o/r.git", "https://ghp_abc@github.com/o/r.git", "HTTP://u:p@host/x"] {
            assert!(url_has_credentials(url), "{url}");
        }
        for url in [
            "https://github.com/o/r.git",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r.git",
            "/local/path",
            "https://host/path@with-at",
            "",
        ] {
            assert!(!url_has_credentials(url), "{url}");
        }
    }

    #[test]
    fn leaky_configs_are_refused() {
        assert!(config_leak("[remote \"origin\"]\n\turl = https://github.com/o/r.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n").is_none());
        assert!(config_leak("[remote \"origin\"]\n\turl = https://x:y@github.com/o/r.git\n").is_some());
        assert!(config_leak("[remote \"origin\"]\n\tpushurl = https://tok@github.com/o/r.git\n").is_some());
        assert!(config_leak("[http \"https://github.com/\"]\n\textraheader = AUTHORIZATION: basic abc\n").is_some());
    }

    #[test]
    fn worktrees_are_refused_and_plain_repos_accepted() {
        let base = std::env::temp_dir().join(format!("sandcastle-git-test-{}", std::process::id()));
        fs::create_dir_all(base.join("plain/.git")).unwrap();
        fs::create_dir_all(base.join("worktree")).unwrap();
        fs::write(base.join("worktree/.git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(source_dir(&base.join("plain")).unwrap(), base.join("plain/.git"));
        assert!(source_dir(&base.join("worktree")).is_err());
        fs::write(base.join("plain/.git/config"), "[remote \"o\"]\n\turl = https://a:b@h/r\n").unwrap();
        assert!(source_dir(&base.join("plain")).is_err());
        fs::remove_dir_all(&base).unwrap();
    }
}
