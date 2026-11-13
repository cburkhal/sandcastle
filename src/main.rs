//! sandcastle: one OpenCode instance per Git repo, each in its own VM.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};

use sandcastle::agent::opencode::OpenCode;
use sandcastle::core::git::REMOTE;
use sandcastle::core::lifecycle::{self, App};
use sandcastle::core::state::{self, PushMode};
use sandcastle::core::{push};
use sandcastle::core::up::Options;
use sandcastle::runtime::apple_container::AppleContainer;
use sandcastle::secrets::bitwarden::Bitwarden;

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
    /// Publish a branch of the box: fetch it and push it from this Mac as
    /// sandcastle/<branch>. Without a branch: list what the agent asked for
    Push {
        /// The agent's branch
        branch: Option<String>,
        /// Show what the branch would publish and do nothing else
        #[arg(long, requires = "branch", conflicts_with = "deny")]
        review: bool,
        /// Drop the agent's request for this branch without publishing
        #[arg(long, requires = "branch")]
        deny: bool,
        /// A directory inside the repo
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Show or set what `git push` in the box leads to
    PushMode {
        /// off: refused. ask: you approve each push (default). always:
        /// published at once, except changes to CI configuration
        mode: Option<String>,
        /// A directory inside the repo
        #[arg(long, default_value = ".")]
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
    // Every command talks to the runtime, and its service is down after a reboot.
    cx.runtime.ensure_started()?;

    match cli.command {
        Command::Init { no_firewall } => lifecycle::init(&cx, !no_firewall),
        Command::Doctor => lifecycle::doctor(&cx),
        Command::Up { path, no_browser, no_firewall } => {
            up(&cx, &path, Options { open_browser: !no_browser, firewall: !no_firewall })
        }
        Command::Down { path } => lifecycle::down(&cx, &path),
        Command::Ls => lifecycle::list(&cx),
        Command::Fetch { path } => sandcastle::core::git::fetch(&cx, &path),
        Command::Push { branch, review, deny, path } => push(&cx, &path, branch.as_deref(), review, deny),
        Command::PushMode { mode, path } => push_mode(&cx, &path, mode.as_deref()),
        Command::Reset { path, yes } => lifecycle::reset(&cx, &path, yes),
    }
}

fn print_summary(summary: &push::Summary) {
    if summary.commits.is_empty() {
        println!("Branch {}: nothing new; every commit is already in this repo or on its remote.", summary.branch);
        return;
    }
    println!("Branch {} would publish {} commit(s) as {}/{}:", summary.branch, summary.commits.len(), REMOTE, summary.branch);
    for commit in &summary.commits {
        println!("  {commit}");
    }
    println!("{}", summary.stat);
    if summary.touches_ci {
        println!("WARNING: these commits change CI configuration. CI may run them with its secrets once published.");
    }
    println!("Read the changes: git log -p {REMOTE}/{} --not --branches --remotes", summary.branch);
}

/// `sandcastle push`: the owner's side of `git push` in the box.
fn push(cx: &App, path: &std::path::Path, branch: Option<&str>, review: bool, deny: bool) -> Result<()> {
    let name = state::box_name(&state::box_root(path)?);
    let Some(branch) = branch else {
        let requests = push::pending(cx.home, &name)?;
        println!("Push mode of this box: {}", push::mode(cx.home, &name)?.name());
        if requests.is_empty() {
            println!("The agent has not asked to publish anything.");
            return Ok(());
        }
        println!("The agent asks to publish:");
        for request in requests {
            let reason = request.reason.map(|reason| format!("  ({reason})")).unwrap_or_default();
            println!("  {}{reason}", request.branch);
        }
        println!("Look first:  sandcastle push <branch> --review");
        println!("Publish:     sandcastle push <branch>");
        println!("Drop:        sandcastle push <branch> --deny");
        return Ok(());
    };
    if deny {
        push::dismiss(cx.home, &name, branch)?;
        println!("Dropped the request for {branch}. Nothing was published.");
        return Ok(());
    }
    if review {
        print_summary(&push::review(cx, path, branch)?);
        return Ok(());
    }
    let published = push::approve(cx, path, branch)?;
    println!("Published {branch} as {} on {}.", published.branch, published.remote);
    if !published.output.is_empty() {
        println!("{}", published.output);
    }
    Ok(())
}

fn push_mode(cx: &App, path: &std::path::Path, mode: Option<&str>) -> Result<()> {
    let name = state::box_name(&state::box_root(path)?);
    let Some(mode) = mode else {
        println!("{}", push::mode(cx.home, &name)?.name());
        return Ok(());
    };
    let mode = PushMode::parse(mode).ok_or_else(|| anyhow!("unknown mode \"{mode}\" (off, ask, always)"))?;
    push::set_mode(cx.home, &name, mode)?;
    match mode {
        PushMode::Off => println!("`git push` in the box is refused. Take commits with `sandcastle fetch`."),
        PushMode::Ask => println!("`git push` in the box waits for you: `sandcastle push` lists the requests."),
        PushMode::Always => {
            println!("`git push` in the box is published at once as {REMOTE}/<branch>, without review.");
            println!("WARNING: CI that runs on pushes then runs the agent's code with the secrets its workflows have.");
            println!("Pushes that change CI configuration still wait for you.");
        }
    }
    Ok(())
}

/// `up` runs in the foreground: the box and its relay live as long as this
/// process.
fn up(cx: &App, path: &std::path::Path, options: Options) -> Result<()> {
    // One secret store exists so far; the config names it (`[secrets] kind`).
    let mut store = Bitwarden::new();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(sandcastle::core::up::run(cx, &mut store, path, options))
}
