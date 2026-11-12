//! Agent module: what runs inside the box.
//!
//! The core only talks to [`Agent`]. This interface is the leakiest of the
//! three modules: swapping the agent is real work, but it stays in here.

pub mod opencode;

/// How to build the agent's image.
pub struct ImageSource {
    /// Local tag for the built image.
    pub tag: String,
    /// Contents of the Containerfile (OCI image build file).
    pub containerfile: &'static str,
}

/// The agent's configuration file, written on the host and mounted read-only.
pub struct ConfigFile {
    pub name: &'static str,
    pub content: String,
}

/// How to start the agent in a box.
pub struct Launch {
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub trait Agent {
    fn image(&self) -> ImageSource;
    /// Numeric user the agent runs as; volumes are handed to this user.
    fn uid(&self) -> u32;
    /// Home directory of that user; gets its own volume.
    fn home(&self) -> &str;
    /// Directory that holds the box's clone of the repo; gets its own volume.
    fn workspace(&self) -> &str;
    /// Directory in the box where the configuration is mounted read-only.
    fn config_dir(&self) -> &str;
    /// Configuration that points the agent at an OpenAI-compatible endpoint
    /// (the relay) and lists the models it may use.
    fn config(&self, llm_base_url: &str, models: &[String]) -> ConfigFile;
    /// Port of the agent's web UI and API inside the box.
    fn ui_port(&self) -> u16;
    /// Path that answers 200 without credentials once the agent is up.
    fn health_path(&self) -> &str;
    /// User name for the agent's API (HTTP Basic Auth).
    fn api_user(&self) -> &str;
    /// Process and environment. `password` protects the agent's API; only the
    /// relay knows it. The agent must not start before `wait_for` (host, port)
    /// accepts connections: the box runs before the relay can listen.
    fn launch(&self, password: &str, wait_for: (&str, u16)) -> Launch;
}
