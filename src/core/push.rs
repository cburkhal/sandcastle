//! Publishing the agent's branches: `git push` in the box, done by the Mac.
//!
//! The box has no Git credentials and never talks to the owner's remote.
//! When the agent runs `git push`, a helper in the box asks the relay to
//! publish the branch. Depending on the box's [`PushMode`] the request is
//! refused, queued for the owner, or carried out at once. Carrying it out
//! means: the Mac fetches the branch from the box (as `sandcastle fetch`
//! does) and pushes it with the owner's own Git setup to
//! `sandcastle/<branch>` on the owner's remote. The agent chooses nothing
//! but the name of one of its branches; it can never touch another branch
//! of the remote.
//!
//! Known limit: once a branch is on the remote, CI that runs on pushes runs
//! the agent's code with the secrets those workflows have. `always` mode
//! therefore queues every push that changes CI configuration, but it cannot
//! tell what an existing workflow will execute.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::git::{self, REMOTE};
use super::lifecycle::App;
use super::state::{self, PushMode};
use crate::relay::log::Log;
use crate::runtime::BoxState;

/// Paths whose change makes a push wait for the owner even in `always` mode.
const CI_PATHS: [&str; 10] = [
    ".github",
    ".gitlab-ci.yml",
    ".gitlab",
    ".circleci",
    ".forgejo",
    ".gitea",
    ".woodpecker",
    ".woodpecker.yml",
    "Jenkinsfile",
    "azure-pipelines.yml",
];

/// A branch the agent asked to publish and the owner has not decided on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub branch: String,
    /// Seconds since 1970.
    pub requested_at: u64,
    /// Why it waits, when the mode alone would have published it.
    #[serde(default)]
    pub reason: Option<String>,
}

/// What the agent is told. The text goes to the agent, so it holds nothing
/// from the host beyond the branch name the agent sent.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Published(String),
    Queued(String),
    Refused(String),
    Failed(String),
}

/// What the owner looks at before approving.
#[derive(Debug, Serialize)]
pub struct Summary {
    pub branch: String,
    /// Commits that are on no branch of the host repo yet, newest first.
    pub commits: Vec<String>,
    /// `git diff --stat` of those commits.
    pub stat: String,
    pub touches_ci: bool,
}

/// A branch name the agent may ask for. Stricter than Git: it ends up in
/// command lines and in refs on the owner's remote.
pub fn valid_branch(name: &str) -> bool {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    !name.is_empty()
        && name.len() <= 200
        && name.chars().all(plain)
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("/.")
        && !name.ends_with(".lock")
        && !name.contains(".lock/")
}

fn requests_path(home: &Path, box_name: &str) -> PathBuf {
    state::box_dir(home, box_name).join("push-requests.json")
}

/// Requests that wait for the owner, oldest first.
pub fn pending(home: &Path, box_name: &str) -> Result<Vec<Request>> {
    let path = requests_path(home, box_name);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path)?;
    serde_json::from_str(&text).with_context(|| format!("corrupt file {}", path.display()))
}

fn store(home: &Path, box_name: &str, requests: &[Request]) -> Result<()> {
    let path = requests_path(home, box_name);
    fs::create_dir_all(path.parent().expect("the path has a parent"))?;
    // Written whole and renamed: another sandcastle process may read it.
    let partial = path.with_extension("json.partial");
    fs::write(&partial, serde_json::to_string_pretty(requests)?)?;
    fs::rename(partial, path)?;
    Ok(())
}

fn enqueue(home: &Path, box_name: &str, branch: &str, reason: Option<String>) -> Result<()> {
    let mut requests = pending(home, box_name)?;
    requests.retain(|request| request.branch != branch);
    let requested_at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    requests.push(Request { branch: branch.to_string(), requested_at, reason });
    store(home, box_name, &requests)
}

/// Forget a request: the owner denied it, or it was carried out.
pub fn dismiss(home: &Path, box_name: &str, branch: &str) -> Result<()> {
    let mut requests = pending(home, box_name)?;
    let before = requests.len();
    requests.retain(|request| request.branch != branch);
    if requests.len() != before {
        store(home, box_name, &requests)?;
    }
    Ok(())
}

/// The mode of a box. Read from the record each time, so a change applies to
/// a running box at once, also when another sandcastle process made it.
pub fn mode(home: &Path, box_name: &str) -> Result<PushMode> {
    Ok(state::load(home, box_name)?.map(|record| record.push).unwrap_or_default())
}

pub fn set_mode(home: &Path, box_name: &str, mode: PushMode) -> Result<()> {
    let mut record = state::load(home, box_name)?.ok_or_else(|| anyhow!("no box named {box_name} exists"))?;
    record.push = mode;
    state::save(home, &record)
}

/// Handles the agent's requests for one running box. Holds plain data only,
/// so the relay can call it from its own tasks.
pub struct Gate {
    home: PathBuf,
    box_name: String,
    repo: PathBuf,
    /// See [`git::upload_pack_command`].
    server: Vec<String>,
    log: Log,
}

impl Gate {
    pub fn new(cx: &App, box_name: &str, repo: &Path, log: Log) -> Self {
        Gate {
            home: cx.home.to_path_buf(),
            box_name: box_name.to_string(),
            repo: repo.to_path_buf(),
            server: git::upload_pack_command(cx, box_name),
            log,
        }
    }

    /// The agent asks to publish `branch`. Blocking: in `always` mode this
    /// fetches and pushes.
    pub fn request(&self, branch: &str) -> Answer {
        if !valid_branch(branch) {
            self.log.refused("push: refused a request with an unusable branch name");
            return Answer::Refused(
                "This is not a branch name sandcastle can publish. Use letters, digits, '.', '_', '-' and '/'.".into(),
            );
        }
        match self.decide(branch) {
            Ok(answer) => answer,
            Err(error) => {
                // The reason may name host paths or the remote; it stays on the host.
                self.log.line(&format!("push: publishing {branch} failed: {error:#}"));
                Answer::Failed(format!(
                    "Publishing {branch} failed on the owner's Mac. The owner sees the reason; tell them. Do not retry."
                ))
            }
        }
    }

    fn decide(&self, branch: &str) -> Result<Answer> {
        let wait = |reason: Option<String>| -> Result<Answer> {
            enqueue(&self.home, &self.box_name, branch, reason.clone())?;
            self.log.line(&format!(
                "push: the agent asks to publish branch {branch}{}. Review and approve: sandcastle push {branch}",
                reason.map(|reason| format!(" ({reason})")).unwrap_or_default()
            ));
            Ok(Answer::Queued(format!(
                "Not published yet: the owner was asked to publish {branch} as {REMOTE}/{branch} and decides on their Mac. \
                 Do not retry; tell the user that branch {branch} waits for their approval."
            )))
        };
        match mode(&self.home, &self.box_name)? {
            PushMode::Off => {
                self.log.refused(&format!("push: refused branch {branch}: publishing is off for this box"));
                Ok(Answer::Refused(
                    "Publishing is off for this box. Commit your work; the owner takes it with `sandcastle fetch`. Do not retry."
                        .into(),
                ))
            }
            _ if !state::is_repo(&self.repo) => Ok(Answer::Refused(
                "The owner's folder is not a Git repository, so there is nowhere to publish from. Tell the user.".into(),
            )),
            PushMode::Ask => wait(None),
            PushMode::Always => {
                git::fetch_with(&self.repo, &self.server)?;
                if summarize(&self.repo, branch)?.touches_ci {
                    return wait(Some("it changes CI configuration".into()));
                }
                let published = push_to_remote(&self.repo, branch)?;
                dismiss(&self.home, &self.box_name, branch)?;
                self.log.line(&format!("push: published {branch} as {} on {}", published.branch, published.remote));
                Ok(Answer::Published(format!("Published as {} on the owner's remote.", published.branch)))
            }
        }
    }
}

/// Where a branch went.
pub struct Published {
    pub remote: String,
    /// Name of the branch on the remote.
    pub branch: String,
    /// What the remote said, for the owner: it often has a link to open a
    /// pull request.
    pub output: String,
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        // Never wait for a password: the push runs without a terminal.
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("could not run git")?;
    if !output.status.success() {
        bail!("git {} failed: {}", args.first().copied().unwrap_or_default(), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn tracking_ref(branch: &str) -> String {
    format!("refs/remotes/{REMOTE}/{branch}")
}

/// What `branch` would publish, from what was last fetched: the commits that
/// are on no branch of the host repo and no remote-tracking branch yet.
fn summarize(repo: &Path, branch: &str) -> Result<Summary> {
    let tip = tracking_ref(branch);
    if git(repo, &["rev-parse", "--verify", "--quiet", &format!("{tip}^{{commit}}")]).is_err() {
        bail!("the box has no branch named {branch}");
    }
    // Everything but the box's own branches counts as "already known".
    let exclude = format!("--exclude={REMOTE}/*");
    let range = [tip.as_str(), "--not", "--branches", exclude.as_str(), "--remotes"];

    let mut log = vec!["log", "--format=%h %s"];
    log.extend(range);
    let commits: Vec<String> = git(repo, &log)?.lines().map(str::to_string).collect();

    let mut touched = vec!["log", "--format=", "--name-only"];
    touched.extend(range);
    touched.push("--");
    touched.extend(CI_PATHS);
    let touches_ci = !git(repo, &touched)?.trim().is_empty();

    // The oldest new commit's parent is where the agent started.
    let mut oldest = vec!["rev-list", "--reverse"];
    oldest.extend(range);
    let stat = match git(repo, &oldest)?.lines().next() {
        Some(first) => {
            let base = format!("{first}^");
            let has_parent = git(repo, &["rev-parse", "--verify", "--quiet", &base]).is_ok();
            // Git's empty tree, for a branch that starts without a parent.
            let from = if has_parent { base.as_str() } else { "4b825dc642cb6eb9a060e54bf8d69288fbee4904" };
            git(repo, &["diff", "--stat", "--no-ext-diff", "--no-textconv", from, &tip])?
        }
        None => String::new(),
    };
    Ok(Summary { branch: branch.to_string(), commits, stat: stat.trim_end().to_string(), touches_ci })
}

/// The remote a branch is published to: `origin`, or the only one.
fn remote(repo: &Path) -> Result<String> {
    let listing = git(repo, &["remote"])?;
    let remotes: Vec<&str> = listing.lines().filter(|name| *name != REMOTE).collect();
    match remotes.as_slice() {
        [] => bail!("{} has no Git remote to publish to", repo.display()),
        [only] => Ok(only.to_string()),
        many if many.contains(&"origin") => Ok("origin".to_string()),
        _ => bail!("{} has several Git remotes and none is named origin", repo.display()),
    }
}

/// Push what was fetched for `branch` to `sandcastle/<branch>` on the remote,
/// with the owner's own Git setup. Never forced: the agent cannot rewrite
/// what is already published.
fn push_to_remote(repo: &Path, branch: &str) -> Result<Published> {
    let remote = remote(repo)?;
    let published = format!("{REMOTE}/{branch}");
    let refspec = format!("{}:refs/heads/{published}", tracking_ref(branch));
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["push", "--porcelain", &remote, &refspec])
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("could not run git")?;
    let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        bail!("git push to {remote} failed: {said}");
    }
    Ok(Published { remote, branch: published, output: said })
}

fn running_box(cx: &App, path: &Path) -> Result<(PathBuf, String)> {
    let repo = state::box_root(path)?;
    let name = state::box_name(&repo);
    if !state::is_repo(&repo) {
        bail!("{} is not a Git repository, so there is nothing to publish from", repo.display());
    }
    if cx.runtime.inspect(&name)?.state != BoxState::Running {
        bail!("box {name} is not running; its branches can only be read while it runs");
    }
    Ok((repo, name))
}

/// Fetch from the box and describe what publishing `branch` would add.
pub fn review(cx: &App, path: &Path, branch: &str) -> Result<Summary> {
    if !valid_branch(branch) {
        bail!("\"{branch}\" is not a branch name sandcastle can publish");
    }
    let (repo, name) = running_box(cx, path)?;
    git::fetch_with(&repo, &git::upload_pack_command(cx, &name))?;
    summarize(&repo, branch)
}

/// The owner publishes `branch`, with or without a request from the agent.
pub fn approve(cx: &App, path: &Path, branch: &str) -> Result<Published> {
    if !valid_branch(branch) {
        bail!("\"{branch}\" is not a branch name sandcastle can publish");
    }
    let (repo, name) = running_box(cx, path)?;
    git::fetch_with(&repo, &git::upload_pack_command(cx, &name))?;
    summarize(&repo, branch)?;
    let published = push_to_remote(&repo, branch)?;
    dismiss(cx.home, &name, branch)?;
    Ok(published)
}

/// Instructions every chat in the box starts with. They do not depend on the
/// push mode: `git push` itself answers with what happened.
pub fn instructions(has_source: bool, allowed_hosts: &[String]) -> String {
    let git = if has_source {
        format!(
            "## Git\n\n\
             - Your working directory is your own clone of the owner's repository. The remote `origin` is a read-only \
             copy of the owner's repository on their Mac: `git fetch origin` brings in their newest commits.\n\
             - The owner sees only what you commit. Uncommitted changes are invisible to them. Work on a branch with \
             a descriptive name and commit when a piece of work is done.\n\
             - To hand work over, run `git push origin <branch>`. This does not push from here: it asks the owner's \
             Mac to publish your branch as `{REMOTE}/<branch>` on the owner's remote. The answer says what happened: \
             published, waiting for the owner's approval, or publishing is off for this box. In the last two cases \
             do not retry; tell the user which branch is ready. The owner can also take your commits without a push.\n\
             - You cannot change `main` or any other branch of the owner's remote, and you cannot open pull requests. \
             After a successful push, tell the user the branch name so they can open one.\n"
        )
    } else {
        "## Git\n\n\
         - The owner's folder is not a Git repository, so your working directory started empty. You can create a \
         repository with `git init` or clone a public one.\n\
         - The owner sees only what you commit, once their folder is a Git repository too. Tell the user when work \
         is committed.\n"
            .to_string()
    };
    format!(
        "# You are working inside a sandcastle box\n\n\
         This is an isolated virtual machine on the owner's Mac. Inside it you may run any command without asking.\n\n\
         ## Limits\n\n\
         - There are no credentials here: no Git credentials, no SSH keys, no API keys. Do not ask for them and do \
         not try to configure them.\n\
         - The network reaches only these hosts: {}. Every other host fails. Do not retry or look for a way around \
         it; tell the user which host you needed, and they can allow it.\n\
         - Docker is not available.\n\n\
         {git}",
        allowed_hosts.join(", ")
    )
}

/// The Git remote helper that turns `git push` in the box into a request to
/// the relay at `host:port`. Bash only: `/dev/tcp` needs no extra tool.
pub fn helper_script(host: &str, port: u16) -> String {
    HELPER.replace("@HOST@", host).replace("@PORT@", &port.to_string())
}

/// Name under which Git finds the helper for `sandcastle::` URLs.
pub const HELPER_NAME: &str = "git-remote-sandcastle";

const HELPER: &str = r#"#!/bin/bash
# Installed by sandcastle. `git push` to a sandcastle:: URL asks the owner's
# Mac to publish the branch; this box has no credentials and pushes nothing.
ask() {
  local branch="$1" response head
  if ! exec 3<>/dev/tcp/@HOST@/@PORT@; then
    status=0 message="sandcastle on the owner's Mac did not answer."
    return
  fi
  printf 'POST /push HTTP/1.1\r\nHost: @HOST@:@PORT@\r\nContent-Type: text/plain\r\nContent-Length: %s\r\nConnection: close\r\n\r\n%s' \
    "${#branch}" "$branch" >&3
  response="$(cat <&3)"
  exec 3<&- 3>&-
  head="${response%%$'\r'*}"
  status="${head#* }"
  status="${status%% *}"
  message="${response#*$'\r\n\r\n'}"
}

while read -r line; do
  case "$line" in
    capabilities) printf 'push\n\n' ;;
    list*) printf '\n' ;;
    push\ *)
      specs=("${line#push }")
      while read -r line && [ -n "$line" ]; do specs+=("${line#push }"); done
      for spec in "${specs[@]}"; do
        spec="${spec#+}"
        src="${spec%%:*}"
        dst="${spec#*:}"
        case "$src" in
          refs/heads/*)
            ask "${src#refs/heads/}"
            echo "sandcastle: $message" >&2
            case "$status" in
              200) echo "ok $dst" ;;
              202) echo "error $dst waiting for the owner's approval" ;;
              403) echo "error $dst refused" ;;
              *) echo "error $dst failed on the owner's Mac" ;;
            esac ;;
          *)
            echo "sandcastle: only branches can be published, and nothing can be deleted. Use: git push origin <branch>" >&2
            echo "error $dst not a branch" ;;
        esac
      done
      printf '\n' ;;
    '') exit 0 ;;
  esac
done
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_plain() {
        for good in ["fix-login", "feature/add_x", "v1.2", "a"] {
            assert!(valid_branch(good), "{good}");
        }
        let long = "x".repeat(201);
        for bad in [
            "", "-force", "/abs", "a//b", "a..b", ".hidden", "a/.b", "trail/", "x.lock", "x.lock/y", "sp ace", "semi;colon",
            "tilde~1", "caret^", "colon:main", "star*", "back\\slash", "new\nline", "ümlaut", long.as_str(),
        ] {
            assert!(!valid_branch(bad), "{bad:?}");
        }
    }

    fn home(test: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("sandcastle-push-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        home
    }

    #[test]
    fn requests_queue_once_per_branch_and_can_be_dismissed() {
        let home = home("queue");
        assert!(pending(&home, "sc-a").unwrap().is_empty());
        enqueue(&home, "sc-a", "one", None).unwrap();
        enqueue(&home, "sc-a", "two", Some("it changes CI configuration".into())).unwrap();
        enqueue(&home, "sc-a", "one", None).unwrap();
        let branches: Vec<String> = pending(&home, "sc-a").unwrap().into_iter().map(|r| r.branch).collect();
        assert_eq!(branches, ["two", "one"]);
        dismiss(&home, "sc-a", "two").unwrap();
        dismiss(&home, "sc-a", "missing").unwrap();
        assert_eq!(pending(&home, "sc-a").unwrap().len(), 1);
        fs::remove_dir_all(home).unwrap();
    }

    fn sh(dir: &Path, script: &str) {
        let status = Command::new("/bin/sh")
            .args(["-ec", script])
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "{script}");
    }

    /// A host repo with a bare remote, and a branch "fetched from the box".
    fn repo_with_agent_branch(test: &str, agent_commits: &str) -> (PathBuf, PathBuf) {
        let root = home(test);
        fs::create_dir_all(&root).unwrap();
        sh(&root, "git init -q --bare remote.git && git init -q -b main host");
        let host = root.join("host");
        sh(&host, "echo a > a && git add a && git commit -qm base && git remote add origin ../remote.git && git push -q origin main");
        sh(&host, &format!("git checkout -q -b work && {agent_commits} && git update-ref refs/remotes/sandcastle/work HEAD && git checkout -q main && git branch -qD work"));
        (root, host)
    }

    #[test]
    fn summary_lists_only_new_commits_and_push_goes_under_the_prefix() {
        let (root, host) = repo_with_agent_branch("publish", "echo b > b && git add b && git commit -qm 'add b'");
        let summary = summarize(&host, "work").unwrap();
        assert_eq!(summary.commits.len(), 1);
        assert!(summary.commits[0].ends_with("add b"));
        assert!(summary.stat.contains("1 file changed"), "{}", summary.stat);
        assert!(!summary.touches_ci);
        assert!(summarize(&host, "missing").is_err());

        let published = push_to_remote(&host, "work").unwrap();
        assert_eq!((published.remote.as_str(), published.branch.as_str()), ("origin", "sandcastle/work"));
        let refs = git(&root.join("remote.git"), &["for-each-ref", "--format=%(refname)"]).unwrap();
        assert_eq!(refs.lines().collect::<Vec<_>>(), ["refs/heads/main", "refs/heads/sandcastle/work"]);
        // Published commits are known from then on.
        assert!(summarize(&host, "work").unwrap().commits.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_rewritten_branch_is_not_forced_onto_the_remote() {
        let (root, host) = repo_with_agent_branch("noforce", "echo b > b && git add b && git commit -qm 'add b'");
        push_to_remote(&host, "work").unwrap();
        sh(&host, "git checkout -q -b other main && echo c > c && git add c && git commit -qm rewritten && git update-ref refs/remotes/sandcastle/work HEAD && git checkout -q main");
        assert!(push_to_remote(&host, "work").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ci_changes_are_noticed() {
        let (root, host) = repo_with_agent_branch(
            "ci",
            "mkdir -p .github/workflows && echo x > .github/workflows/ci.yml && git add .github && git commit -qm 'ci' && echo b > b && git add b && git commit -qm 'add b'",
        );
        let summary = summarize(&host, "work").unwrap();
        assert_eq!(summary.commits.len(), 2);
        assert!(summary.touches_ci);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn instructions_explain_push_only_with_a_host_repo() {
        let with = instructions(true, &["registry.npmjs.org".into(), "github.com".into()]);
        assert!(with.contains("git push origin <branch>") && with.contains("registry.npmjs.org, github.com"));
        assert!(!instructions(false, &[]).contains("git push"));
    }

    #[test]
    fn helper_has_the_relay_address_filled_in() {
        let script = helper_script("192.168.128.1", 4182);
        assert!(script.contains("/dev/tcp/192.168.128.1/4182"));
        assert!(!script.contains("@HOST@") && !script.contains("@PORT@"));
    }
}
