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
    /// The host repo's `.git` directory; `None` for a folder that is not a repo.
    source: Option<PathBuf>,
    name: String,
    image_tag: String,
    image_digest: String,
}

/// Check everything that needs neither a password nor a box.
pub fn plan(cx: &App, path: &Path) -> Result<Plan> {
    let repo = state::box_root(path)?;
    let source = if state::is_repo(&repo) { Some(git::source_dir(&repo)?) } else { None };
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
    /// The agent's API in the box, and the password only sandcastle knows.
    agent_address: SocketAddr,
    password: String,
}

impl RunningBox {
    /// A one-time link that logs the browser in and lands in a chat. Each
    /// call replaces the last unused link.
    pub fn login_url(&self, cx: &App) -> Result<String> {
        let authorization = format!("Basic {}", http::base64(format!("{}:{}", cx.agent.api_user(), self.password).as_bytes()));
        let api = |method: &str, path: &str, body: &str| agent_api(self.agent_address, &authorization, method, path, body);
        let landing = cx.agent.landing_path(&format!("http://127.0.0.1:{}", self.ui_port), &api);
        self.ui.new_login_url(&landing)
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

    // The chat's port: the one from the last start if it is still free, so
    // the browser sees the same site again. Fixed before the box starts.
    let last_port = state::load(cx.home, &name)?.and_then(|record| record.ui_port);
    let ui_listener = match last_port {
        Some(port) => TcpListener::bind(("127.0.0.1", port)).await.ok(),
        None => None,
    };
    let ui_listener = match ui_listener {
        Some(listener) => listener,
        None => TcpListener::bind("127.0.0.1:0").await.context("could not listen on 127.0.0.1")?,
    };
    let ui_port = ui_listener.local_addr()?.port();

    // --- 2. Network and volumes. ---
    let record = BoxRecord { name: name.clone(), repo, image_digest, ui_port: Some(ui_port) };
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

    // --- 3. The box. From here on, failures must remove it again. ---
    let password = relay::random_token(32)?;
    let spec =
        box_spec(cx, &session.config, &record, &image_tag, &box_dir.join("config"), source.as_deref(), gateway, &password);
    cx.runtime.run(&spec)?;

    let mut tasks = Vec::new();
    let served = serve(
        cx, session, &record, gateway, &network.subnet, llm.clone(), log.clone(), ui_listener, ui_port, &password,
        source.is_some(), &mut tasks,
    )
    .await;
    match served {
        Ok((ui, agent_address)) => {
            Ok(RunningBox { name, repo: record.repo, ui_port, ui, llm, log, tasks, agent_address, password })
        }
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
    has_source: bool,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<(Arc<UiGateway>, SocketAddr)> {
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
    git::seed(cx, name, has_source).map_err(failed)?;

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

    let agent_address = SocketAddr::new(box_ip, agent_port);
    let ui = Arc::new(UiGateway::new(
        ui_port,
        agent_address,
        cx.agent.api_user(),
        password,
        relay::random_token(32)?,
        relay::random_token(32)?,
        log.clone(),
    ));
    let handler = ui.clone();
    tasks.push(tokio::spawn(http::serve(ui_listener, None, log, move |request| handler.clone().handle(request))));
    Ok((ui, agent_address))
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

    let login_url = running.login_url(cx)?;
    println!("Box {} is up for {}", running.name, running.repo.display());
    println!("  Open this link to chat (it works once): {login_url}");
    println!("  Allowed hosts: {}", session.config.egress.allow.join(", "));
    if state::is_repo(&running.repo) {
        println!("  The agent works on its own clone. `sandcastle fetch` (in another terminal) brings its commits here.");
    } else {
        println!("  This folder is not a Git repository: the box starts with an empty workspace, and nothing");
        println!("  from the folder is in it. To get the agent's commits later, run `git init` here, then `sandcastle fetch`.");
    }
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
    source: Option<&Path>,
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
    ];
    if let Some(source) = source {
        // Only `.git`, not the working tree: untracked files such as `.env` stay on the host.
        spec.mounts.push(Mount::Bind { source: source.to_path_buf(), target: git::SOURCE_MOUNT.into(), readonly: true });
    }
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

/// One authenticated request to the agent's API in the box; returns the
/// response body. Small and blocking: it runs a few times per login link.
fn agent_api(address: SocketAddr, authorization: &str, method: &str, path: &str, body: &str) -> Result<String> {
    let timeout = Duration::from_secs(5);
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: {authorization}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    parse_api_response(&response)
}

/// Body of an HTTP/1.1 response with status 200, chunked or not.
fn parse_api_response(response: &[u8]) -> Result<String> {
    let split = response.windows(4).position(|window| window == b"\r\n\r\n");
    let split = split.ok_or_else(|| anyhow!("the agent sent no HTTP response"))?;
    let head = String::from_utf8_lossy(&response[..split]).to_ascii_lowercase();
    let mut rest = &response[split + 4..];
    if head.split(' ').nth(1) != Some("200") {
        bail!("the agent answered {}", head.lines().next().unwrap_or_default());
    }
    if !head.contains("transfer-encoding: chunked") {
        return Ok(String::from_utf8_lossy(rest).into_owned());
    }
    // Chunked: a hex length line, that many bytes, CRLF, until a zero length.
    let mut body = Vec::new();
    while let Some(line_end) = rest.windows(2).position(|window| window == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..line_end]).trim(), 16)
            .context("bad chunk size from the agent")?;
        let start = line_end + 2;
        if size == 0 || rest.len() < start + size {
            break;
        }
        body.extend_from_slice(&rest[start..start + size]);
        rest = rest.get(start + size + 2..).unwrap_or_default();
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_responses_are_read_plain_and_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_api_response(plain).unwrap(), "{}");
        // The second chunk starts in the middle of a two-byte character.
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"\xc3\r\n3\r\n\xa4\"}\r\n0\r\n\r\n";
        assert_eq!(parse_api_response(chunked).unwrap(), "{\"\u{e4}\"}");
        assert!(parse_api_response(b"HTTP/1.1 401 Unauthorized\r\n\r\n").is_err());
        assert!(parse_api_response(b"garbage").is_err());
    }
}
