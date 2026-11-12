//! sandcastle: one OpenCode instance per Git repo, each in its own VM.

mod agent;
mod core;
mod firewall;
mod relay;
mod runtime;
mod secrets;

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};

use crate::agent::opencode::OpenCode;
use crate::core::lifecycle::{self, App};
use crate::core::up::Options;
use crate::runtime::apple_container::AppleContainer;
use crate::secrets::bitwarden::Bitwarden;

#[derive(Parser)]
#[command(name = "sandcastle", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write the default config, add the pf anchor line and build the box image
    Init {
        /// Do not touch /etc/pf.conf (boxes then need `up --no-firewall`)
        #[arg(long)]
        no_firewall: bool,
    },
    /// Check that everything `up` relies on is in place
    Doctor,
    /// Start the box for a repo and keep it running until Ctrl-C
    Up {
        /// A directory inside the repo
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Print the login link instead of opening it in the browser
        #[arg(long)]
        no_browser: bool,
        /// Start without pf rules. Needs no root, but the box can then reach
        /// other services on this Mac. For testing only.
        #[arg(long)]
        no_firewall: bool,
    },
    /// Stop the box for a repo; its volumes are kept
    Down {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List boxes
    Ls,
    /// Bring the box's commits into this repo as sandcastle/<branch>
    Fetch {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
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
        Command::Init { no_firewall } => lifecycle::init(&cx, !no_firewall),
        Command::Doctor => lifecycle::doctor(&cx),
        Command::Up { path, no_browser, no_firewall } => {
            up(&cx, &path, Options { open_browser: !no_browser, firewall: !no_firewall })
        }
        Command::Down { path } => lifecycle::down(&cx, &path),
        Command::Ls => lifecycle::list(&cx),
        Command::Fetch { path } => crate::core::git::fetch(&cx, &path),
        Command::Reset { path, yes } => lifecycle::reset(&cx, &path, yes),
    }
}

/// `up` runs in the foreground: the box and its relay live as long as this
/// process.
fn up(cx: &App, path: &std::path::Path, options: Options) -> Result<()> {
    // One secret store exists so far; the config names it (`[secrets] kind`).
    let mut store = Bitwarden::new();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(crate::core::up::run(cx, &mut store, path, options))
}
