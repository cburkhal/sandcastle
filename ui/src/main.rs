//! The sandcastle app: a window on top of the same library the CLI uses.
//!
//! The app is one session (see `sandcastle::core::session`): it asks for the
//! Bitwarden master password and for `sudo` once, when the user unlocks it,
//! and then starts and stops boxes without asking again. Locking or quitting
//! stops every box, drops the secrets and removes the firewall rules.
//!
//! The chat itself stays in the browser. The window never loads remote
//! content, and its only powers are the commands below.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sandcastle::agent::opencode::OpenCode;
use sandcastle::core::lifecycle::App;
use sandcastle::core::session::{self, Session};
use sandcastle::core::up::{self, RunningBox};
use sandcastle::core::{git, state};
use sandcastle::firewall::Prompt;
use sandcastle::runtime::apple_container::AppleContainer;
use sandcastle::runtime::BoxState;
use sandcastle::secrets::bitwarden::Bitwarden;
use sandcastle::secrets::Secret;
use serde::Serialize;
use tokio::sync::Mutex;

static RUNTIME: AppleContainer = AppleContainer;
static AGENT: OpenCode = OpenCode;

/// Shown by `sudo -A` when Touch ID is not set up for sudo. Fixed text only.
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
}

impl Inner {
    fn cx(&self) -> App<'_> {
        App { home: &self.home, runtime: &RUNTIME, agent: &AGENT }
    }
}

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
}

#[derive(Serialize)]
struct Status {
    unlocked: bool,
    firewall: bool,
    needs_password: bool,
    boxes: Vec<BoxView>,
}

#[tauri::command]
async fn status(inner: Shared<'_>) -> Result<Status, String> {
    let cx = inner.cx();
    let needs_password = session::needs_store_password(&cx, &Bitwarden::new()).map_err(text)?;

    // Lock order everywhere: session first, then boxes.
    let session = inner.session.lock().await;
    let mut boxes = inner.boxes.lock().await;
    // A box that stopped by itself still has a relay and firewall rules.
    if let Some(session) = session.as_ref() {
        let mut gone = Vec::new();
        for name in boxes.keys() {
            if cx.runtime.inspect(name).map_err(text)?.state != BoxState::Running {
                gone.push(name.clone());
            }
        }
        for name in gone {
            if let Some(running) = boxes.remove(&name) {
                let _ = running.stop(&cx, session);
            }
        }
    }
    let unlocked = session.is_some();

    let views = state::list(&inner.home)
        .map_err(text)?
        .into_iter()
        .map(|record| BoxView {
            running: boxes.contains_key(&record.name),
            name: record.name,
            repo: record.repo.display().to_string(),
        })
        .collect();
    Ok(Status { unlocked, firewall: inner.firewall, needs_password, boxes: views })
}

/// The secrets phase: `sudo` (Touch ID or a password dialog), then the vault.
#[tauri::command]
async fn unlock(inner: Shared<'_>, password: Option<String>) -> Result<(), String> {
    let mut slot = inner.session.lock().await;
    if slot.is_some() {
        return Ok(());
    }
    let inner = Arc::clone(&inner);
    let session = tauri::async_runtime::spawn_blocking(move || -> anyhow::Result<Session> {
        let cx = inner.cx();
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
        Session::open(&cx, &mut store, prompt)
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(text)?;
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
    inner.boxes.lock().await.insert(running.name.clone(), running);
    Ok(())
}

#[tauri::command]
async fn stop_box(inner: Shared<'_>, name: String) -> Result<(), String> {
    let session = current_session(&inner).await?;
    let running = inner.boxes.lock().await.remove(&name).ok_or("this box is not running")?;
    running.stop(&inner.cx(), &session).map_err(text)
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

/// Stop every box, drop the secrets and remove the firewall rules.
#[tauri::command]
async fn lock(inner: Shared<'_>) -> Result<(), String> {
    lock_all(&inner).await;
    Ok(())
}

async fn lock_all(inner: &Inner) {
    let mut slot = inner.session.lock().await;
    if let Some(session) = slot.as_ref() {
        for (_, running) in inner.boxes.lock().await.drain() {
            let _ = running.stop(&inner.cx(), session);
        }
    }
    // Dropping the session ends the root helper, which removes its rules.
    *slot = None;
}

fn main() {
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set"));
    let firewall = !std::env::args().any(|argument| argument == "--no-firewall");
    let inner = Arc::new(Inner { home, firewall, session: Mutex::new(None), boxes: Mutex::new(HashMap::new()) });

    let on_exit = Arc::clone(&inner);
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(inner)
        .invoke_handler(tauri::generate_handler![status, unlock, start_box, stop_box, open_chat, fetch, lock])
        .build(tauri::generate_context!())
        .expect("the app could not start")
        .run(move |_, event| {
            if let tauri::RunEvent::Exit = event {
                tauri::async_runtime::block_on(lock_all(&on_exit));
            }
        });
}
