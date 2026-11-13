//! Box lifecycle apart from `up`: init, down, ls, reset.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use super::config;
use super::state::{self, BoxRecord};
use crate::agent::Agent;
use crate::firewall;
use crate::runtime::{BoxSpec, BoxState, Mount, SandboxRuntime};

/// What every command needs: where the user's files are and the two modules.
pub struct App<'a> {
    pub home: &'a Path,
    pub runtime: &'a dyn SandboxRuntime,
    pub agent: &'a dyn Agent,
}

/// `sandcastle init`: write the default config, prepare pf and build the image.
pub fn init(cx: &App, with_firewall: bool) -> Result<()> {
    if config::write_default(cx.home)? {
        println!("Wrote {}", config::path(cx.home).display());
    }
    config::load(cx.home)?;
    if with_firewall {
        firewall::install_anchor()?;
    }

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

/// `sandcastle doctor`: check what `up` relies on. Prompts for nothing.
pub fn doctor(cx: &App) -> Result<()> {
    let mut problems = 0;
    let mut check = |ok: bool, good: String, bad: String| {
        if ok {
            println!("ok       {good}");
        } else {
            println!("PROBLEM  {bad}");
            problems += 1;
        }
    };

    let config = config::load(cx.home);
    check(
        config.is_ok(),
        format!("config {}", config::path(cx.home).display()),
        config.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_default(),
    );
    if let Ok(config) = &config {
        check(
            config.llm.is_some(),
            "LLM gateway configured".into(),
            "no [llm] section in the config: `up` cannot start".into(),
        );
    }

    let image = cx.agent.image();
    match cx.runtime.image_digest(&image.tag) {
        Ok(digest) => check(
            digest.is_some(),
            format!("box image {}", image.tag),
            format!("box image {} is not built; run `sandcastle init`", image.tag),
        ),
        Err(error) => check(false, String::new(), format!("the sandbox runtime does not work: {error:#}")),
    }

    // A macOS update may replace /etc/pf.conf and drop the line.
    let anchor = firewall::anchor_configured().unwrap_or(false);
    check(
        anchor,
        format!("pf anchor line in {}", firewall::PF_CONF),
        format!("{} has no sandcastle anchor line; run `sandcastle init`", firewall::PF_CONF),
    );

    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    println!("Everything `sandcastle up` relies on is in place.");
    Ok(())
}

/// Create the box's home and workspace volumes if they are missing.
pub fn prepare_volumes(cx: &App, record: &BoxRecord, image: &str) -> Result<()> {
    for (volume, target) in [(record.home_volume(), cx.agent.home()), (record.workspace_volume(), cx.agent.workspace())] {
        if cx.runtime.volume_create(&volume)? {
            hand_volume_to_agent(cx, image, &volume).with_context(|| format!("preparing the volume for {target}"))?;
        }
    }
    Ok(())
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

/// `sandcastle down`: stop and remove the box; network and volumes stay.
pub fn down(cx: &App, path: &Path) -> Result<()> {
    let name = state::box_name(&state::box_root(path)?);
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
    let repo = state::box_root(path)?;
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

    delete(cx, &record)?;
    println!("Deleted box {name}, its network and its volumes.");
    Ok(())
}

/// Delete a box with its network, both volumes and its state, without
/// asking. Everything the agent did in the box is lost. The host folder is
/// not touched.
pub fn delete(cx: &App, record: &BoxRecord) -> Result<()> {
    cx.runtime.remove(&record.name)?;
    cx.runtime.volume_remove(&record.home_volume())?;
    cx.runtime.volume_remove(&record.workspace_volume())?;
    cx.runtime.network_remove(&record.network())?;
    state::remove(cx.home, &record.name)
}
