//! The sandcastle app: a window on top of the same library the CLI uses.
//!
//! The app is one session (see `sandcastle::core::session`): it asks for the
//! Bitwarden master password and for `sudo` once, when the user unlocks it,
//! and then starts and stops boxes without asking again. Locking or quitting
//! stops every box, drops the secrets and removes the firewall rules.
//!
//! The chat itself stays in the browser. The window never loads remote
//! content, and its only powers are the commands below.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sandcastle::agent::opencode::OpenCode;
use sandcastle::core::lifecycle::{self, App};
use sandcastle::core::session::{self, Session, Step};
use sandcastle::core::up::{self, RunningBox};
use sandcastle::core::state::PushMode;
use sandcastle::core::{config, git, push, state};
use sandcastle::firewall::{self, Prompt};
use sandcastle::runtime::apple_container::AppleContainer;
use sandcastle::runtime::BoxState;
use sandcastle::secrets::bitwarden::Bitwarden;
use sandcastle::secrets::Secret;
use serde::Serialize;
use tokio::sync::Mutex;

static RUNTIME: AppleContainer = AppleContainer;
static AGENT: OpenCode = OpenCode;

/// Shown when Touch ID is not set up for sudo or was cancelled (`sudo -A`).
/// Fixed text only.
const ASKPASS: &str = r#"#!/bin/sh
exec /usr/bin/osascript \
  -e 'display dialog "sandcastle needs your macOS login password (sudo) to load firewall rules. A box may reach only the sandcastle relay on this Mac, and only this Mac may reach the box." default answer "" with hidden answer with title "sandcastle" with icon caution' \
  -e 'text returned of result'
"#;

struct Inner {
    home: PathBuf,
    /// From the `--no-firewall` flag: no pf rules, for testing without root.
    firewall: bool,
    session: Mutex<Option<Arc<Session>>>,
    boxes: Mutex<HashMap<String, RunningBox>>,
    /// What a running command is waiting for, shown in the window.
    progress: std::sync::Mutex<Progress>,
    /// Things that happened without a click, shown once.
    notices: std::sync::Mutex<Vec<String>>,
    /// Refused pushes (box, branch) the owner was already told about.
    told_refused: std::sync::Mutex<HashSet<(String, String)>>,
}

impl Inner {
    fn cx(&self) -> App<'_> {
        App { home: &self.home, runtime: &RUNTIME, agent: &AGENT }
    }

    fn set_progress(&self, step: u8, text: &str) {
        if let Ok(mut progress) = self.progress.lock() {
            *progress = Progress { step, text: text.to_string() };
        }
    }

    fn notice(&self, notice: String) {
        if let Ok(mut notices) = self.notices.lock() {
            notices.push(notice);
        }
    }

    /// What is missing before a session can open, each with what to do.
    /// Cheap checks only: this runs on every refresh of a locked window.
    fn setup_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        match config::load(&self.home) {
            Err(error) => problems.push(format!("{error:#}. Run `sandcastle init` in a terminal.")),
            Ok(config) if config.llm.is_none() => problems.push(format!(
                "No LLM gateway is configured. Add an [llm] section to {} (see README.md).",
                config::path(&self.home).display()
            )),
            Ok(_) => {}
        }
        if self.firewall && !firewall::anchor_configured().unwrap_or(false) {
            problems.push(format!(
                "{} has no sandcastle anchor line (a macOS update may have removed it). Run `sandcastle init` in a terminal.",
                firewall::PF_CONF
            ));
        }
        problems
    }
}

/// What a running command is waiting for. `step` numbers the unlock steps
/// the window lists (0: none of them).
#[derive(Clone, Default, Serialize)]
struct Progress {
    step: u8,
    text: String,
}

const UNLOCK_RUNTIME: u8 = 1;
const UNLOCK_SUDO: u8 = 2;
const UNLOCK_SECRETS: u8 = 3;

type Shared<'a> = tauri::State<'a, Arc<Inner>>;

/// Errors cross to the window as text. Core errors never contain secrets.
fn text(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[derive(Serialize)]
struct BoxView {
    name: String,
    repo: String,
    running: bool,
    /// "off", "ask" or "always": what `git push` in the box leads to.
    push_mode: &'static str,
    /// Branches the agent asked to publish and the owner has not decided on.
    requests: Vec<push::Request>,
}

#[derive(Serialize)]
struct Status {
    version: &'static str,
    unlocked: bool,
    firewall: bool,
    needs_password: bool,
    boxes: Vec<BoxView>,
    /// While locked: what must be fixed before unlocking can work.
    problems: Vec<String>,
    /// Things that happened without a click since the last call.
    notices: Vec<String>,
}

#[tauri::command]
async fn status(inner: Shared<'_>) -> Result<Status, String> {
    let cx = inner.cx();
    // Lock order everywhere: session first, then boxes.
    let session = inner.session.lock().await;
    let mut boxes = inner.boxes.lock().await;
    let unlocked = session.is_some();
    let problems = if unlocked { Vec::new() } else { inner.setup_problems() };
    // With a broken config the unlock form is not shown at all.
    let needs_password = session::needs_store_password(&cx, &Bitwarden::new()).unwrap_or(true);

    // A box that stopped by itself still has a relay and firewall rules.
    if let Some(session) = session.as_ref() {
        let mut gone = Vec::new();
        for name in boxes.keys() {
            match cx.runtime.inspect(name) {
                Ok(found) if found.state != BoxState::Running => gone.push(name.clone()),
                Ok(_) => {}
                // One box that cannot be inspected must not hide the others.
                Err(error) => inner.notice(format!("Could not check box {name}: {error:#}")),
            }
        }
        for name in gone {
            if let Some(running) = boxes.remove(&name) {
                let folder = running.repo.display().to_string();
                match running.stop(&cx, session) {
                    Ok(()) => inner.notice(format!("The box for {folder} stopped by itself. Its relay and firewall rules were removed.")),
                    Err(error) => inner.notice(format!("The box for {folder} stopped by itself, and cleaning up after it failed: {error:#}")),
                }
            }
        }
    }

    let records = state::list(&inner.home).map_err(text)?;
    // A push refused by the "Never" setting: the agent has work to fetch.
    for record in records.iter().filter(|record| boxes.contains_key(&record.name)) {
        for branch in push::refused(&inner.home, &record.name) {
            let first = inner.told_refused.lock().map(|mut told| told.insert((record.name.clone(), branch.clone())));
            if first.unwrap_or(false) {
                inner.notice(format!(
                    "The agent tried to push {branch} in the box for {}. Pushing is set to Never for this box; take its commits with Fetch.",
                    record.repo.display()
                ));
            }
        }
    }
    let views = records
        .into_iter()
        .map(|record| BoxView {
            running: boxes.contains_key(&record.name),
            push_mode: record.push.name(),
            // A broken request file must not hide the box.
            requests: push::pending(&inner.home, &record.name).unwrap_or_default(),
            name: record.name,
            repo: record.repo.display().to_string(),
        })
        .collect();
    let notices = inner.notices.lock().map(|mut notices| std::mem::take(&mut *notices)).unwrap_or_default();
    Ok(Status { version: env!("CARGO_PKG_VERSION"), unlocked, firewall: inner.firewall, needs_password, boxes: views, problems, notices })
}

/// What the running command is waiting for. Takes no lock a command holds.
#[tauri::command]
fn progress(inner: Shared<'_>) -> Progress {
    inner.progress.lock().map(|progress| progress.clone()).unwrap_or_default()
}

/// The secrets phase: `sudo` (Touch ID or a password dialog), then the vault.
#[tauri::command]
async fn unlock(inner: Shared<'_>, password: Option<String>) -> Result<(), String> {
    let mut slot = inner.session.lock().await;
    if slot.is_some() {
        return Ok(());
    }
    let inner = Arc::clone(&inner);
    let worker = Arc::clone(&inner);
    let session = tauri::async_runtime::spawn_blocking(move || -> anyhow::Result<Session> {
        let inner = worker;
        let cx = inner.cx();
        inner.set_progress(UNLOCK_RUNTIME, "Starting Apple container.");
        cx.runtime.ensure_started()?;
        let mut store = match password {
            Some(password) if !password.is_empty() => Bitwarden::with_password(Secret::new(password)),
            _ => Bitwarden::new(),
        };
        let askpass = state::dir(&inner.home).join("askpass.sh");
        let prompt = if inner.firewall {
            std::fs::create_dir_all(state::dir(&inner.home))?;
            std::fs::write(&askpass, ASKPASS)?;
            std::fs::set_permissions(&askpass, std::fs::Permissions::from_mode(0o700))?;
            Some(Prompt::Askpass(&askpass))
        } else {
            None
        };
        Session::open(&cx, &mut store, prompt, &|step| {
            let number = match step {
                Step::Sudo => UNLOCK_SUDO,
                Step::Secrets => UNLOCK_SECRETS,
            };
            inner.set_progress(number, step.text())
        })
    })
    .await;
    inner.set_progress(0, "");
    let session = session.map_err(|error| error.to_string())?.map_err(text)?;
    *slot = Some(Arc::new(session));
    Ok(())
}

async fn current_session(inner: &Inner) -> Result<Arc<Session>, String> {
    inner.session.lock().await.clone().ok_or_else(|| "sandcastle is locked; unlock it first".to_string())
}

/// Start the box for the repo that contains `path`.
#[tauri::command]
async fn start_box(inner: Shared<'_>, path: String) -> Result<(), String> {
    let session = current_session(&inner).await?;
    let cx = inner.cx();
    let plan = up::plan(&cx, Path::new(&path)).map_err(text)?;
    let running = up::start(&cx, &session, plan).await.map_err(text)?;
    // A new run of the box starts with a clean slate of refused pushes.
    if let Ok(mut told) = inner.told_refused.lock() {
        told.retain(|(name, _)| name != &running.name);
    }
    inner.boxes.lock().await.insert(running.name.clone(), running);
    Ok(())
}

#[tauri::command]
async fn stop_box(inner: Shared<'_>, name: String) -> Result<(), String> {
    let session = current_session(&inner).await?;
    let running = inner.boxes.lock().await.remove(&name).ok_or("this box is not running")?;
    running.stop(&inner.cx(), &session).map_err(text)
}

/// Delete a box: its clone, sessions and caches (both volumes), its network
/// and its state. A running box is stopped first. The host folder is never
/// touched.
#[tauri::command]
async fn delete_box(inner: Shared<'_>, name: String) -> Result<(), String> {
    let cx = inner.cx();
    // Lock order everywhere: session first, then boxes.
    let session = inner.session.lock().await.clone();
    let running = inner.boxes.lock().await.remove(&name);
    if let (Some(running), Some(session)) = (running, session) {
        running.stop(&cx, &session).map_err(text)?;
    }
    let record = state::load(&inner.home, &name).map_err(text)?.ok_or("this box no longer exists")?;
    lifecycle::delete(&cx, &record).map_err(text)
}

/// Open the chat in the default browser, through a fresh one-time link.
#[tauri::command]
async fn open_chat(inner: Shared<'_>, name: String) -> Result<(), String> {
    let boxes = inner.boxes.lock().await;
    let running = boxes.get(&name).ok_or("this box is not running")?;
    let url = running.login_url(&inner.cx()).map_err(text)?;
    // `open` is the macOS way to hand a URL to the default browser.
    let opened = std::process::Command::new("open").arg(&url).status().map_err(|error| error.to_string())?;
    if !opened.success() {
        return Err("could not open the browser".into());
    }
    Ok(())
}

/// Bring the box's commits into the host repo. Returns its branches.
#[tauri::command]
async fn fetch(inner: Shared<'_>, name: String) -> Result<Vec<String>, String> {
    let repo = inner.boxes.lock().await.get(&name).ok_or("this box is not running")?.repo.clone();
    git::fetch_refs(&inner.cx(), &repo).map_err(text)
}

/// Set what `git push` in a box leads to. Applies at once, also while it runs.
#[tauri::command]
async fn set_push_mode(inner: Shared<'_>, name: String, mode: String) -> Result<(), String> {
    let mode = PushMode::parse(&mode).ok_or("unknown push mode")?;
    push::set_mode(&inner.home, &name, mode).map_err(text)
}

async fn running_repo(inner: &Inner, name: &str) -> Result<PathBuf, String> {
    Ok(inner.boxes.lock().await.get(name).ok_or("this box is not running")?.repo.clone())
}

/// Fetch from the box and describe what publishing `branch` would add.
#[tauri::command]
async fn review_push(inner: Shared<'_>, name: String, branch: String) -> Result<push::Summary, String> {
    let repo = running_repo(&inner, &name).await?;
    push::review(&inner.cx(), &repo, &branch).map_err(text)
}

/// Publish `branch` from this Mac as sandcastle/<branch>. Returns what the
/// remote said, which often has a link to open a pull request.
#[tauri::command]
async fn approve_push(inner: Shared<'_>, name: String, branch: String) -> Result<String, String> {
    let repo = running_repo(&inner, &name).await?;
    let published = push::approve(&inner.cx(), &repo, &branch).map_err(text)?;
    Ok(format!("Published {branch} as {} on {}.\n{}", published.branch, published.remote, published.output).trim().to_string())
}

/// Drop the agent's request without publishing.
#[tauri::command]
async fn deny_push(inner: Shared<'_>, name: String, branch: String) -> Result<(), String> {
    push::dismiss(&inner.home, &name, &branch).map_err(text)
}

/// Stop every box, drop the secrets and remove the firewall rules.
#[tauri::command]
async fn lock(inner: Shared<'_>) -> Result<(), String> {
    let failures = lock_all(&inner).await;
    if failures.is_empty() {
        Ok(())
    } else {
        // The session is gone all the same; a box that still runs is then
        // blocked completely by the firewall helper (it fails closed).
        Err(format!("Locked, but not every box could be stopped. {}", failures.join(" ")))
    }
}

/// Returns what could not be stopped.
async fn lock_all(inner: &Inner) -> Vec<String> {
    let mut failures = Vec::new();
    let mut slot = inner.session.lock().await;
    if let Some(session) = slot.as_ref() {
        for (name, running) in inner.boxes.lock().await.drain() {
            if let Err(error) = running.stop(&inner.cx(), session) {
                failures.push(format!("Box {name}: {error:#}."));
            }
        }
    }
    // Dropping the session ends the root helper, which removes its rules.
    *slot = None;
    failures
}

/// The search path for `container`, `bw` and what they start themselves.
/// An app started from Finder gets only the system folders, without
/// Homebrew's and without `/usr/local/bin`, where Apple `container` lives.
fn search_path(current: &str) -> String {
    let mut folders: Vec<&str> = current.split(':').filter(|folder| !folder.is_empty()).collect();
    for folder in ["/opt/homebrew/bin", "/usr/local/bin"] {
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    folders.join(":")
}

fn main() {
    // Before anything else runs: no other thread reads the environment yet.
    let path = search_path(&std::env::var("PATH").unwrap_or_default());
    std::env::set_var("PATH", path);
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    let firewall = !std::env::args().any(|argument| argument == "--no-firewall");
    let inner = Arc::new(Inner { home, firewall, 
        session: Mutex::new(None),
        boxes: Mutex::new(HashMap::new()),
        progress: Default::default(),
        notices: Default::default(),
        told_refused: Default::default(),
    });

    let on_exit = Arc::clone(&inner);
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(inner)
        .invoke_handler(tauri::generate_handler![status, progress, unlock, start_box, stop_box, delete_box, open_chat, set_push_mode, review_push, approve_push, deny_push, fetch, lock])
        .build(tauri::generate_context!())
        .expect("the app could not start")
        .run(move |_, event| {
            if let tauri::RunEvent::Exit = event {
                let _ = tauri::async_runtime::block_on(lock_all(&on_exit));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::search_path;

    #[test]
    fn search_path_gains_the_missing_folders_once() {
        assert_eq!(search_path("/usr/bin:/bin"), "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin");
        assert_eq!(search_path("/usr/local/bin:/usr/bin"), "/usr/local/bin:/usr/bin:/opt/homebrew/bin");
        assert_eq!(search_path(""), "/opt/homebrew/bin:/usr/local/bin");
    }
}
