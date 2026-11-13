//! Sandbox runtime module: everything that creates, runs and removes boxes.
//!
//! The core only talks to [`SandboxRuntime`]. [`BoxSpec`] uses OCI runtime
//! terms (image, mounts, env, capabilities), not flags of one tool, so another
//! runtime can be added later without touching the core.
//!

pub mod apple_container;

use std::path::{Path, PathBuf};

use anyhow::Result;

/// How a box is attached to a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    /// No network at all.
    None,
    /// A named network created with [`SandboxRuntime::network_create`].
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mount {
    /// A named volume managed by the runtime.
    Volume { name: String, target: String },
    /// A host directory.
    Bind {
        source: PathBuf,
        target: String,
        readonly: bool,
    },
    /// An in-memory filesystem.
    Tmpfs { target: String },
}

/// Description of one box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxSpec {
    pub name: String,
    pub image: String,
    /// Process to run; empty means the image's default.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    pub mounts: Vec<Mount>,
    pub network: Network,
    /// Whether the runtime may configure a resolver in the box.
    pub dns: bool,
    pub readonly_rootfs: bool,
    /// Capabilities to drop ("ALL" drops everything) and to add back.
    pub cap_drop: Vec<String>,
    pub cap_add: Vec<String>,
    /// `name` or `uid[:gid]`; `None` keeps the image's user.
    pub user: Option<String>,
    pub cpus: Option<u32>,
    /// Memory limit with unit suffix, for example `4G`.
    pub memory: Option<String>,
}

impl BoxSpec {
    /// A spec with nothing granted: no network, no mounts, image defaults.
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        BoxSpec {
            name: name.into(),
            image: image.into(),
            command: Vec::new(),
            env: Vec::new(),
            mounts: Vec::new(),
            network: Network::None,
            dns: false,
            readonly_rootfs: false,
            cap_drop: Vec::new(),
            cap_add: Vec::new(),
            user: None,
            cpus: None,
            memory: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInfo {
    /// Host-side address of the network; the relay binds here.
    pub gateway: String,
    /// Subnet in CIDR notation.
    pub subnet: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoxState {
    Running,
    Stopped,
    /// No box with that name exists.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxStatus {
    pub state: BoxState,
    /// Address of the box on its network, without prefix length.
    pub ipv4: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

pub trait SandboxRuntime: Send + Sync {
    /// Make the runtime usable, starting its background service if needed.
    fn ensure_started(&self) -> Result<()>;

    /// Build an image from a Containerfile.
    fn image_build(&self, tag: &str, containerfile: &Path, context: &Path) -> Result<()>;
    /// Digest of a local image, or `None` if it does not exist.
    fn image_digest(&self, tag: &str) -> Result<Option<String>>;

    /// Create a host-only network. Returns `false` if it already existed.
    fn network_create(&self, name: &str) -> Result<bool>;
    fn network_inspect(&self, name: &str) -> Result<Option<NetworkInfo>>;
    /// Remove a network; a missing network is not an error.
    fn network_remove(&self, name: &str) -> Result<()>;

    /// Create a named volume. Returns `false` if it already existed.
    fn volume_create(&self, name: &str) -> Result<bool>;
    /// Remove a volume; a missing volume is not an error.
    fn volume_remove(&self, name: &str) -> Result<()>;

    /// Start a box in the background.
    fn run(&self, spec: &BoxSpec) -> Result<()>;
    /// Run a box to completion and remove it afterwards.
    fn run_once(&self, spec: &BoxSpec) -> Result<ExecOutput>;
    /// Run a command in a running box. `stdin` is forwarded if given.
    fn exec(&self, name: &str, command: &[String], stdin: Option<&[u8]>) -> Result<ExecOutput>;
    /// The command line that runs `command` in a box with stdin and stdout
    /// attached. For programs that must start it themselves, such as Git's
    /// `ext` transport.
    fn exec_argv(&self, name: &str, command: &[&str]) -> Vec<String>;
    /// Remove a box, stopping it first; a missing box is not an error.
    fn remove(&self, name: &str) -> Result<()>;
    fn inspect(&self, name: &str) -> Result<BoxStatus>;
    /// Last lines of the box's output, for error messages.
    fn logs(&self, name: &str, lines: usize) -> Result<String>;
}
