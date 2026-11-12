//! Box lifecycle: init, up, down, ls, reset.
//!
//! Build step 1: boxes start hardened and isolated, but there is no relay yet,
//! so the agent has no way out (no LLM, no package registry) and the UI is
//! reachable only on the box's address on the host-only network.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use super::config::{self, Config};
use super::state::{self, BoxRecord};
use crate::agent::Agent;
use crate::runtime::{BoxSpec, BoxState, Mount, Network, SandboxRuntime};

pub struct App<'a> {
    pub home: &'a Path,
    pub runtime: &'a dyn SandboxRuntime,
    pub agent: &'a dyn Agent,
}

/// `sandcastle init`: write the default config and build the image.
pub fn init(cx: &App) -> Result<()> {
    if config::write_default(cx.home)? {
        println!("Wrote {}", config::path(cx.home).display());
    }
    config::load(cx.home)?;

    let image = cx.agent.image();
    let build_dir = state::dir(cx.home).join("build");
    fs::create_dir_all(&build_dir)?;
    let containerfile = build_dir.join("Containerfile");
    fs::write(&containerfile, image.containerfile)?;

    println!("Building image {} ...", image.tag);
    cx.runtime.image_build(&image.tag, &containerfile, &build_dir)?;
    let digest = cx
        .runtime
        .image_digest(&image.tag)?
        .ok_or_else(|| anyhow!("image {} is missing after the build", image.tag))?;
    println!("Image {} ready ({digest})", image.tag);
    Ok(())
}

/// A box that `up` has started and must clean up again.
pub struct RunningBox {
    pub record: BoxRecord,
    pub address: String,
    pub port: u16,
}

/// `sandcastle up`, first half: create what is missing and start the box.
pub fn start(cx: &App, path: &Path) -> Result<RunningBox> {
    let config = config::load(cx.home)?;
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
    // A stopped leftover from a crashed run would block the name.
    cx.runtime.remove(&name)?;

    let record = BoxRecord { name: name.clone(), repo, image_digest };
    state::save(cx.home, &record)?;

    cx.runtime.network_create(&record.network())?;
    for (volume, target) in [(record.home_volume(), cx.agent.home()), (record.workspace_volume(), cx.agent.workspace())] {
        if cx.runtime.volume_create(&volume)? {
            hand_volume_to_agent(cx, &image.tag, &volume).with_context(|| format!("preparing volume for {target}"))?;
        }
    }

    cx.runtime.run(&box_spec(cx.agent, &config, &record, &image.tag, &random_password()?))?;

    let port = cx.agent.ui_port();
    match wait_until_healthy(cx, &name, port) {
        Ok(address) => Ok(RunningBox { record, address, port }),
        Err(error) => {
            let logs = cx.runtime.logs(&name, 20).unwrap_or_default();
            let _ = cx.runtime.remove(&name);
            Err(error.context(format!("box {name} did not come up. Last output:\n{logs}")))
        }
    }
}

/// The hardened box from CLAUDE.md 2.3.
fn box_spec(agent: &dyn Agent, config: &Config, record: &BoxRecord, image: &str, password: &str) -> BoxSpec {
    let launch = agent.launch(password);
    let mut spec = BoxSpec::new(&record.name, image);
    spec.command = launch.command;
    spec.env = launch.env;
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
    ];
    spec
}

/// New volumes belong to root, so the agent's user could not write them
/// (spike item 5). A one-off root box with no network and only CAP_CHOWN
/// fixes the owner. It must run before the real box: a volume attaches to
/// one running box only (spike item 8).
fn hand_volume_to_agent(cx: &App, image: &str, volume: &str) -> Result<()> {
    let uid = cx.agent.uid();
    let mut spec = BoxSpec::new(format!("{volume}-init"), image);
    spec.user = Some("0".into());
    spec.cap_drop = vec!["ALL".into()];
    spec.cap_add = vec!["CAP_CHOWN".into()];
    spec.mounts = vec![Mount::Volume { name: volume.into(), target: "/volume".into() }];
    spec.command = vec!["chown".into(), format!("{uid}:{uid}"), "/volume".into()];
    let output = cx.runtime.run_once(&spec)?;
    if !output.success {
        bail!("could not change the owner of volume {volume}: {}", output.stderr.trim());
    }
    Ok(())
}

/// The agent's API password for this run. Step 1 has no relay to hold it, so
/// it is generated and then forgotten: nothing can use the API yet.
fn random_password() -> Result<String> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn wait_until_healthy(cx: &App, name: &str, port: u16) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let status = cx.runtime.inspect(name)?;
        if status.state != BoxState::Running {
            bail!("the box stopped right after it started");
        }
        if let Some(address) = status.ipv4 {
            if http_status(&address, port, cx.agent.health_path()) == Some(200) {
                return Ok(address);
            }
        }
        if Instant::now() > deadline {
            bail!("the agent did not answer on port {port} within 60 seconds");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Status code of a plain HTTP GET, or `None` if the request failed.
fn http_status(address: &str, port: u16, path: &str) -> Option<u16> {
    let socket: SocketAddr = format!("{address}:{port}").parse().ok()?;
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&socket, timeout).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    write!(stream, "GET {path} HTTP/1.0\r\nHost: {address}:{port}\r\n\r\n").ok()?;
    let mut head = [0u8; 16];
    stream.read_exact(&mut head).ok()?;
    // "HTTP/1.1 200 ..."
    std::str::from_utf8(&head).ok()?.split(' ').nth(1)?.parse().ok()
}

/// `sandcastle down`: stop and remove the box; network and volumes stay.
pub fn down(cx: &App, path: &Path) -> Result<()> {
    let name = state::box_name(&state::repo_root(path)?);
    stop(cx, &name)
}

pub fn stop(cx: &App, name: &str) -> Result<()> {
    if cx.runtime.inspect(name)?.state == BoxState::Missing {
        println!("Box {name} is not running.");
        return Ok(());
    }
    cx.runtime.remove(name)?;
    println!("Stopped box {name}. Its volumes are kept; `sandcastle reset` deletes them.");
    Ok(())
}

/// `sandcastle ls`.
pub fn list(cx: &App) -> Result<()> {
    let records = state::list(cx.home)?;
    if records.is_empty() {
        println!("No boxes. Start one with `sandcastle up` inside a Git repository.");
        return Ok(());
    }
    println!("{:<40} {:<9} {:<16} REPO", "BOX", "STATUS", "ADDRESS");
    for record in records {
        let status = cx.runtime.inspect(&record.name)?;
        let state = match status.state {
            BoxState::Running => "running",
            BoxState::Stopped | BoxState::Missing => "stopped",
        };
        println!(
            "{:<40} {:<9} {:<16} {}",
            record.name,
            state,
            status.ipv4.unwrap_or_else(|| "-".into()),
            record.repo.display()
        );
    }
    Ok(())
}

/// `sandcastle reset`: delete the box, its network, both volumes and its
/// state. The host repo is not touched.
pub fn reset(cx: &App, path: &Path, assume_yes: bool) -> Result<()> {
    let repo = state::repo_root(path)?;
    let name = state::box_name(&repo);
    let Some(record) = state::load(cx.home, &name)? else {
        println!("No box exists for {}.", repo.display());
        return Ok(());
    };

    if !assume_yes {
        println!("This deletes box {name} with its home and workspace volumes.");
        println!("Everything the agent did in the box is lost, including commits not yet fetched.");
        print!("Type the box name to confirm: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != name {
            bail!("not confirmed; nothing was deleted");
        }
    }

    cx.runtime.remove(&name)?;
    cx.runtime.volume_remove(&record.home_volume())?;
    cx.runtime.volume_remove(&record.workspace_volume())?;
    cx.runtime.network_remove(&record.network())?;
    state::remove(cx.home, &name)?;
    println!("Deleted box {name}, its network and its volumes.");
    Ok(())
}
