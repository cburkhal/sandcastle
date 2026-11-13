//! Starting a box with its relay, and `sandcastle up` on top of that.
//!
//! Order matters and follows CLAUDE.md section 4:
//! 1. everything that can fail without a box (repo, image) and the session,
//!    which is the only part that may ask for a password,
//! 2. network and volumes,
//! 3. the box, whose entrypoint waits for the relay,
//! 4. firewall rules and the relay's box-side listeners, which need the
//!    running box,
//! 5. the UI gateway and the one-time login link.

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::config::Config;
use super::git;
use super::lifecycle::{self, App};
use super::session::Session;
use super::state::{self, BoxRecord};
use crate::firewall::{self, Prompt};
use crate::relay::egress::Egress;
use crate::relay::llm::LlmRelay;
use crate::relay::log::Log;
use crate::relay::ui::UiGateway;
use crate::relay::{self, http};
use crate::runtime::{BoxSpec, BoxState, Mount, Network};
use crate::secrets::SecretStore;

/// Ports of the relay on the box network's gateway address.
const LLM_PORT: u16 = 4180;
const EGRESS_PORT: u16 = 4181;

pub struct Options {
    pub open_browser: bool,
    /// Load pf rules for the box. Off only for testing without root.
    pub firewall: bool,
}

/// What is known about a box before anything is created or asked.
pub struct Plan {
    repo: PathBuf,
    source: PathBuf,
    name: String,
    image_tag: String,
    image_digest: String,
}

/// Check everything that needs neither a password nor a box.
pub fn plan(cx: &App, path: &Path) -> Result<Plan> {
    let repo = state::repo_root(path)?;
    let source = git::source_dir(&repo)?;
    let name = state::box_name(&repo);
    let image = cx.agent.image();
    let image_digest = cx
        .runtime
        .image_digest(&image.tag)?
        .ok_or_else(|| anyhow!("image {} not found; run `sandcastle init` first", image.tag))?;
    if cx.runtime.inspect(&name)?.state == BoxState::Running {
        bail!("box {name} is already running; stop it with `sandcastle down`");
    }
    Ok(Plan { repo, source, name, image_tag: image.tag, image_digest })
}

/// A box that is up, with its relay. Stop it with [`RunningBox::stop`].
pub struct RunningBox {
    pub name: String,
    pub repo: PathBuf,
    pub ui_port: u16,
    ui: Arc<UiGateway>,
    llm: Arc<LlmRelay>,
    log: Log,
    tasks: Vec<JoinHandle<()>>,
}

impl RunningBox {
    /// A one-time link that logs the browser in. Each call replaces the last
    /// unused link.
    pub fn login_url(&self) -> Result<String> {
        self.ui.new_login_url()
    }

    pub fn llm_requests(&self) -> u64 {
        self.llm.request_count()
    }

    /// Stop the relay and the box, then remove the box's firewall rules.
    pub fn stop(self, cx: &App, session: &Session) -> Result<()> {
        self.log.line("up: stopping");
        for task in &self.tasks {
            task.abort();
        }
        // The rules go only once the box is gone. If it could not be
        // stopped, they stay and the helper blocks the box when it ends.
        lifecycle::stop(cx, &self.name)?;
        session.firewall_unload(&self.name)
    }
}

/// Start the box for `plan` in `session`. Prompts for nothing.
pub async fn start(cx: &App<'_>, session: &Session, plan: Plan) -> Result<RunningBox> {
    let Plan { repo, source, name, image_tag, image_digest } = plan;
    let box_dir = state::box_dir(cx.home, &name);
    fs::create_dir_all(box_dir.join("config"))?;
    let log = Log::to_file(&box_dir.join("relay.log"), true)?;
    log.line(&format!("up: starting box {name}"));

    let llm = Arc::new(LlmRelay::new(&session.llm_base_url, &session.llm_key, log.clone())?);
    let models = llm.models().await?;
    println!("LLM gateway reachable, {} models.", models.len());

    // --- 2. Network and volumes. ---
    let record = BoxRecord { name: name.clone(), repo, image_digest };
    state::save(cx.home, &record)?;
    // A stopped leftover from a crashed run would block the name.
    cx.runtime.remove(&name)?;
    cx.runtime.network_create(&record.network())?;
    let network = cx
        .runtime
        .network_inspect(&record.network())?
        .ok_or_else(|| anyhow!("network {} vanished right after it was created", record.network()))?;
    let gateway: IpAddr = network.gateway.parse().context("the runtime reported an invalid gateway address")?;
    lifecycle::prepare_volumes(cx, &record, &image_tag)?;

    let agent_config = cx.agent.config(&format!("http://{gateway}:{LLM_PORT}/v1"), &models);
    fs::write(box_dir.join("config").join(agent_config.name), agent_config.content)?;

    // The UI port is fixed before the box starts so it can be shown early.
    let ui_listener = TcpListener::bind("127.0.0.1:0").await.context("could not listen on 127.0.0.1")?;
    let ui_port = ui_listener.local_addr()?.port();

    // --- 3. The box. From here on, failures must remove it again. ---
    let password = relay::random_token(32)?;
    let spec =
        box_spec(cx, &session.config, &record, &image_tag, &box_dir.join("config"), &source, gateway, &password);
    cx.runtime.run(&spec)?;

    let mut tasks = Vec::new();
    let served = serve(
        cx, session, &record, gateway, &network.subnet, llm.clone(), log.clone(), ui_listener, ui_port, &password,
        &mut tasks,
    )
    .await;
    match served {
        Ok(ui) => Ok(RunningBox { name, repo: record.repo, ui_port, ui, llm, log, tasks }),
        Err(error) => {
            for task in &tasks {
                task.abort();
            }
            if lifecycle::stop(cx, &name).is_ok() {
                let _ = session.firewall_unload(&name);
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    cx: &App<'_>,
    session: &Session,
    record: &BoxRecord,
    gateway: IpAddr,
    subnet: &str,
    llm: Arc<LlmRelay>,
    log: Log,
    ui_listener: TcpListener,
    ui_port: u16,
    password: &str,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<Arc<UiGateway>> {
    let name = &record.name;
    let failed = |error: anyhow::Error| {
        let logs = cx.runtime.logs(name, 20).unwrap_or_default();
        error.context(format!("box {name} did not come up. Last output of the box:\n{logs}"))
    };

    // --- 4. Firewall rules, then the relay's box-side listeners. ---
    // The agent waits for the relay, so it starts only after the rules are in place.
    let box_ip = box_address(cx, name).await.map_err(failed)?;
    let wait = Duration::from_secs(20);
    let agent_port = cx.agent.ui_port();
    if session.has_firewall() {
        let interface = host_interface(gateway, wait).await?;
        let rules = firewall::rules(&interface, subnet, gateway, &[LLM_PORT, EGRESS_PORT], agent_port);
        session.firewall_load(name, &interface, &rules)?;
        log.line(&format!("up: firewall rules loaded on {interface}"));
    }
    // The box's clone must exist before the agent starts in it.
    git::seed(cx, name).map_err(failed)?;

    let llm_listener = relay::bind_on_gateway(SocketAddr::new(gateway, LLM_PORT), wait).await?;
    let egress_listener = relay::bind_on_gateway(SocketAddr::new(gateway, EGRESS_PORT), wait).await?;
    // Only the box may use them; the host sees the box's own address (spike item 1).
    let egress = Arc::new(Egress::new(&session.config.egress.allow, Some(box_ip), log.clone()));
    tasks.push(tokio::spawn(egress.serve(egress_listener)));
    let handler = llm.clone();
    tasks.push(tokio::spawn(http::serve(llm_listener, Some(box_ip), log.clone(), move |request| {
        handler.clone().handle(request)
    })));

    // --- 5. The agent starts once the relay answers; then the UI gateway. ---
    wait_until_healthy(cx, name, box_ip, agent_port).await.map_err(failed)?;

    let ui = Arc::new(UiGateway::new(
        ui_port,
        SocketAddr::new(box_ip, agent_port),
        cx.agent.api_user(),
        password,
        relay::random_token(32)?,
        relay::random_token(32)?,
        log.clone(),
    ));
    let handler = ui.clone();
    tasks.push(tokio::spawn(http::serve(ui_listener, None, log, move |request| handler.clone().handle(request))));
    Ok(ui)
}

/// `sandcastle up`: one session, one box, until Ctrl-C.
pub async fn run(cx: &App<'_>, store: &mut dyn SecretStore, path: &Path, options: Options) -> Result<()> {
    let plan = plan(cx, path)?;
    if !options.firewall {
        eprintln!("WARNING: --no-firewall: the box can reach other services on this Mac, and");
        eprintln!("         devices on your network may be able to reach the box.");
    }
    let session = Session::open(cx, store, options.firewall.then_some(Prompt::Terminal))?;
    let running = start(cx, &session, plan).await?;

    let login_url = running.login_url()?;
    println!("Box {} is up for {}", running.name, running.repo.display());
    println!("  Open this link to chat (it works once): {login_url}");
    println!("  Allowed hosts: {}", session.config.egress.allow.join(", "));
    println!("  The agent works on its own clone. `sandcastle fetch` (in another terminal) brings its commits here.");
    println!("Press Ctrl-C to stop the box.");
    if options.open_browser {
        // `open` is the macOS way to hand a URL to the default browser.
        let _ = std::process::Command::new("open").arg(&login_url).status();
    }

    let reason = wait_for_stop(cx, &running.name).await;
    println!("\n{reason} The LLM relay forwarded {} requests.", running.llm_requests());
    running.stop(cx, &session)
}

/// The hardened box from CLAUDE.md 2.3.
fn box_spec(
    cx: &App<'_>,
    config: &Config,
    record: &BoxRecord,
    image: &str,
    config_dir: &Path,
    source: &Path,
    gateway: IpAddr,
    password: &str,
) -> BoxSpec {
    let agent = cx.agent;
    let gateway = gateway.to_string();
    let launch = agent.launch(password, (&gateway, LLM_PORT));
    let proxy = format!("http://{gateway}:{EGRESS_PORT}");

    let mut spec = BoxSpec::new(&record.name, image);
    spec.command = launch.command;
    spec.env = launch.env;
    // All spellings are needed, and Node's fetch only obeys them with the last one (spike item 6).
    for variable in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        spec.env.push((variable.into(), proxy.clone()));
    }
    // The LLM relay is reached directly, not through the proxy.
    spec.env.push(("NO_PROXY".into(), gateway.clone()));
    spec.env.push(("no_proxy".into(), gateway));
    spec.env.push(("NODE_USE_ENV_PROXY".into(), "1".into()));

    spec.network = Network::Named(record.network());
    spec.dns = false;
    spec.readonly_rootfs = true;
    spec.cap_drop = vec!["ALL".into()];
    spec.user = Some(agent.uid().to_string());
    spec.cpus = Some(config.limits.cpus);
    spec.memory = Some(config.limits.memory.clone());
    spec.mounts = vec![
        Mount::Tmpfs { target: "/tmp".into() },
        Mount::Volume { name: record.home_volume(), target: agent.home().into() },
        Mount::Volume { name: record.workspace_volume(), target: agent.workspace().into() },
        Mount::Bind { source: config_dir.to_path_buf(), target: agent.config_dir().into(), readonly: true },
        // Only `.git`, not the working tree: untracked files such as `.env` stay on the host.
        Mount::Bind { source: source.to_path_buf(), target: git::SOURCE_MOUNT.into(), readonly: true },
    ];
    spec
}

/// The host interface of the box network. It appears shortly after the box starts.
async fn host_interface(gateway: IpAddr, wait: Duration) -> Result<String> {
    let deadline = Instant::now() + wait;
    loop {
        match firewall::interface_for(gateway) {
            Ok(interface) => return Ok(interface),
            Err(error) if Instant::now() > deadline => return Err(error),
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

/// The box's address on its network, once the runtime has assigned one.
async fn box_address(cx: &App<'_>, name: &str) -> Result<IpAddr> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = cx.runtime.inspect(name)?;
        if status.state != BoxState::Running {
            bail!("the box stopped right after it started");
        }
        if let Some(address) = status.ipv4 {
            return address.parse().context("the runtime reported an invalid box address");
        }
        if Instant::now() > deadline {
            bail!("the box got no network address within 20 seconds");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_until_healthy(cx: &App<'_>, name: &str, address: IpAddr, port: u16) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if cx.runtime.inspect(name)?.state != BoxState::Running {
            bail!("the box stopped while the agent was starting");
        }
        let path = cx.agent.health_path().to_string();
        let status = tokio::task::spawn_blocking(move || http_status(address, port, &path)).await?;
        if status == Some(200) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("the agent did not answer on port {port} within 60 seconds");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Status code of a plain HTTP GET, or `None` if the request failed.
fn http_status(address: IpAddr, port: u16, path: &str) -> Option<u16> {
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&SocketAddr::new(address, port), timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: {address}:{port}\r\n\r\n").ok()?;
    let mut head = [0u8; 16];
    stream.read_exact(&mut head).ok()?;
    // "HTTP/1.1 200 ..."
    std::str::from_utf8(&head).ok()?.split(' ').nth(1)?.parse().ok()
}

/// Returns when the user presses Ctrl-C or the box stops by itself.
async fn wait_for_stop(cx: &App<'_>, name: &str) -> &'static str {
    let mut check = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return "Stopping.",
            _ = check.tick() => {
                // An inspect error is not proof that the box is gone; keep waiting.
                if matches!(cx.runtime.inspect(name), Ok(status) if status.state != BoxState::Running) {
                    return "The box stopped by itself.";
                }
            }
        }
    }
}
