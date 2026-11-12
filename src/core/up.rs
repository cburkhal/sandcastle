//! `sandcastle up`: start a box with its relay and keep both running.
//!
//! Order matters and follows CLAUDE.md section 4:
//! 1. everything that can fail without a box (config, key, gateway reachable),
//! 2. network and volumes,
//! 3. the box, whose entrypoint waits for the relay,
//! 4. the relay's box-side listeners, which can only bind once the box runs,
//! 5. the UI gateway and the one-time login link.
//!
//! Not here yet (build step 3): the Bitwarden secrets phase and the pf rules.
//! Until the pf rules exist, the box can reach other services on this Mac.

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::TcpListener;

use super::config::{self, Config};
use super::lifecycle::{self, App};
use super::state::{self, BoxRecord};
use crate::relay::egress::Egress;
use crate::relay::llm::LlmRelay;
use crate::relay::log::Log;
use crate::relay::ui::UiGateway;
use crate::relay::{self, http};
use crate::runtime::{BoxSpec, BoxState, Mount, Network};
use crate::secrets::Secret;

/// Ports of the relay on the box network's gateway address.
const LLM_PORT: u16 = 4180;
const EGRESS_PORT: u16 = 4181;
/// Where the LLM key comes from until the secrets module exists.
const KEY_VARIABLE: &str = "SANDCASTLE_LLM_KEY";

pub async fn run(cx: &App<'_>, path: &Path, open_browser: bool) -> Result<()> {
    // --- 1. Nothing is created until these all hold. ---
    let config = config::load(cx.home)?;
    let llm_config = config.llm.as_ref().ok_or_else(|| {
        anyhow!("no LLM gateway configured: add [llm] base_url to {}", config::path(cx.home).display())
    })?;
    let key = Secret::new(
        std::env::var(KEY_VARIABLE).map_err(|_| anyhow!("the environment variable {KEY_VARIABLE} is not set"))?,
    );
    // Child processes (`container`, the browser) have no business with the key.
    std::env::remove_var(KEY_VARIABLE);

    let repo = state::repo_root(path)?;
    let name = state::box_name(&repo);
    let image = cx.agent.image();
    let image_digest = cx
        .runtime
        .image_digest(&image.tag)?
        .ok_or_else(|| anyhow!("image {} not found; run `sandcastle init` first", image.tag))?;
    if cx.runtime.inspect(&name)?.state == BoxState::Running {
        bail!("box {name} is already running; stop it with `sandcastle down`");
    }

    let box_dir = state::box_dir(cx.home, &name);
    fs::create_dir_all(box_dir.join("config"))?;
    let log = Log::to_file(&box_dir.join("relay.log"), true)?;
    log.line(&format!("up: starting box {name}"));

    let llm = Arc::new(LlmRelay::new(&llm_config.base_url, &key, log.clone())?);
    drop(key);
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
    lifecycle::prepare_volumes(cx, &record, &image.tag)?;

    let agent_config = cx.agent.config(&format!("http://{gateway}:{LLM_PORT}/v1"), &models);
    fs::write(box_dir.join("config").join(agent_config.name), agent_config.content)?;

    // The UI port is fixed before the box starts so it can be shown early.
    let ui_listener = TcpListener::bind("127.0.0.1:0").await.context("could not listen on 127.0.0.1")?;
    let ui_port = ui_listener.local_addr()?.port();

    // --- 3. The box. From here on, failures must remove it again. ---
    let password = relay::random_token(32)?;
    let spec = box_spec(cx, &config, &record, &image.tag, &box_dir.join("config"), gateway, &password);
    cx.runtime.run(&spec)?;

    let result = serve(cx, &record, gateway, llm, &config, log.clone(), ui_listener, ui_port, &password, open_browser).await;
    log.line("up: stopping");
    let stopped = lifecycle::stop(cx, &name);
    result.and(stopped)
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    cx: &App<'_>,
    record: &BoxRecord,
    gateway: IpAddr,
    llm: Arc<LlmRelay>,
    config: &Config,
    log: Log,
    ui_listener: TcpListener,
    ui_port: u16,
    password: &str,
    open_browser: bool,
) -> Result<()> {
    let name = &record.name;
    let failed = |error: anyhow::Error| {
        let logs = cx.runtime.logs(name, 20).unwrap_or_default();
        error.context(format!("box {name} did not come up. Last output of the box:\n{logs}"))
    };

    // --- 4. The relay's box-side listeners. ---
    let box_ip = box_address(cx, name).await.map_err(failed)?;
    let wait = Duration::from_secs(20);
    let llm_listener = relay::bind_on_gateway(SocketAddr::new(gateway, LLM_PORT), wait).await?;
    let egress_listener = relay::bind_on_gateway(SocketAddr::new(gateway, EGRESS_PORT), wait).await?;
    // Only the box may use them; the host sees the box's own address (spike item 1).
    let egress = Arc::new(Egress::new(&config.egress.allow, Some(box_ip), log.clone()));
    tokio::spawn(egress.serve(egress_listener));
    let handler = llm.clone();
    tokio::spawn(http::serve(llm_listener, Some(box_ip), log.clone(), move |request| handler.clone().handle(request)));

    // --- 5. The agent starts once the relay answers; then the UI gateway. ---
    let agent_port = cx.agent.ui_port();
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
    let login_url = ui.login_url().expect("a new gateway has a login link");
    let handler = ui.clone();
    tokio::spawn(http::serve(ui_listener, None, log.clone(), move |request| handler.clone().handle(request)));

    println!("Box {name} is up for {}", record.repo.display());
    println!("  Open this link to chat (it works once): {login_url}");
    println!("  Allowed hosts: {}", config.egress.allow.join(", "));
    println!("Press Ctrl-C to stop the box.");
    if open_browser {
        // `open` is the macOS way to hand a URL to the default browser.
        let _ = std::process::Command::new("open").arg(&login_url).status();
    }

    let reason = wait_for_stop(cx, name).await;
    println!("\n{reason} The LLM relay forwarded {} requests.", llm.request_count());
    Ok(())
}

/// The hardened box from CLAUDE.md 2.3.
fn box_spec(
    cx: &App<'_>,
    config: &Config,
    record: &BoxRecord,
    image: &str,
    config_dir: &Path,
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
    spec
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
