//! sandcastle: one OpenCode instance per Git repo, each in its own VM.

mod agent;
mod core;
mod relay;
mod runtime;
mod secrets;

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};

use crate::agent::opencode::OpenCode;
use crate::core::lifecycle::{self, App};
use crate::runtime::apple_container::AppleContainer;

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
        /// Print the login link instead of opening it in the browser
        #[arg(long)]
        no_browser: bool,
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
        Command::Up { path, no_browser } => up(&cx, &path, !no_browser),
        Command::Down { path } => lifecycle::down(&cx, &path),
        Command::Ls => lifecycle::list(&cx),
        Command::Reset { path, yes } => lifecycle::reset(&cx, &path, yes),
    }
}

/// `up` runs in the foreground: the box and its relay live as long as this
/// process.
fn up(cx: &App, path: &std::path::Path, open_browser: bool) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(crate::core::up::run(cx, path, open_browser))
}
