//! sandcastle: one OpenCode instance per Git repo, each in its own VM.

mod agent;
mod core;
mod runtime;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};

use crate::agent::opencode::OpenCode;
use crate::core::lifecycle::{self, App};
use crate::runtime::apple_container::AppleContainer;
use crate::runtime::{BoxState, SandboxRuntime};

#[derive(Parser)]
#[command(name = "sandcastle", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write the default config and build the box image
    Init,
    /// Start the box for a repo and keep it running until Ctrl-C
    Up {
        /// A directory inside the repo
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Stop the box for a repo; its volumes are kept
    Down {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List boxes
    Ls,
    /// Delete the box for a repo with its network and volumes
    Reset {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
    },
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?);
    // One implementation per module so far; the config names them (`kind`).
    let runtime = AppleContainer::new();
    let agent = OpenCode;
    let cx = App { home: &home, runtime: &runtime, agent: &agent };

    match cli.command {
        Command::Init => lifecycle::init(&cx),
        Command::Up { path } => up(&cx, &path),
        Command::Down { path } => lifecycle::down(&cx, &path),
        Command::Ls => lifecycle::list(&cx),
        Command::Reset { path, yes } => lifecycle::reset(&cx, &path, yes),
    }
}

/// `up` runs in the foreground: the box lives as long as this process.
fn up(cx: &App, path: &std::path::Path) -> Result<()> {
    let running = lifecycle::start(cx, path)?;
    let name = &running.record.name;
    println!("Box {name} is up for {}", running.record.repo.display());
    println!("  agent: http://{}:{} (host only)", running.address, running.port);
    println!("  There is no relay yet: the agent has no LLM and no network access.");
    println!("Press Ctrl-C to stop the box.");

    let reason = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(wait_for_stop(cx.runtime, name));
    println!("\n{reason}");
    lifecycle::stop(cx, name)
}

/// Returns when the user presses Ctrl-C or the box stops by itself.
async fn wait_for_stop(runtime: &dyn SandboxRuntime, name: &str) -> &'static str {
    let mut check = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return "Stopping.",
            _ = check.tick() => {
                // An inspect error is not proof that the box is gone; keep waiting.
                if matches!(runtime.inspect(name), Ok(status) if status.state != BoxState::Running) {
                    return "The box stopped by itself.";
                }
            }
        }
    }
}
