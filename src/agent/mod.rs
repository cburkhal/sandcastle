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

pub trait Agent: Send + Sync {
    fn image(&self) -> ImageSource;
    /// Numeric user the agent runs as; volumes are handed to this user.
    fn uid(&self) -> u32;
    /// Home directory of that user; gets its own volume.
    fn home(&self) -> &str;
    /// Directory that holds the box's clone of the repo; gets its own volume.
    fn workspace(&self) -> &str;
    /// The box's clone of the repo; the agent works here.
    fn repo_dir(&self) -> String {
        format!("{}/repo", self.workspace())
    }
    /// Directory in the box where the configuration is mounted read-only.
    fn config_dir(&self) -> &str;
    /// Files for [`Agent::config_dir`]: configuration that points the agent
    /// at an OpenAI-compatible endpoint (the relay), lists the models it may
    /// use, and makes every chat start with `instructions` (Markdown, about
    /// the box itself). The agent cannot change them.
    fn config(&self, llm_base_url: &str, models: &[String], instructions: &str) -> Vec<ConfigFile>;
    /// Port of the agent's web UI and API inside the box.
    fn ui_port(&self) -> u16;
    /// Path that answers 200 without credentials once the agent is up.
    fn health_path(&self) -> &str;
    /// Where the browser should go right after login, as a path of the
    /// agent's UI, so the user lands in a chat and not on an empty start
    /// page. `origin` is the address the browser uses. `api(method, path,
    /// body)` sends an authenticated request to the agent's API and returns
    /// the response body. Any failure must fall back to `/`.
    fn landing_path(&self, _origin: &str, _api: &dyn Fn(&str, &str, &str) -> anyhow::Result<String>) -> String {
        "/".to_string()
    }
    /// User name for the agent's API (HTTP Basic Auth).
    fn api_user(&self) -> &str;
    /// Process and environment. `password` protects the agent's API; only the
    /// relay knows it. The agent must not start before `wait_for` (host, port)
    /// accepts connections: the box runs before the relay can listen. By then
    /// the clone in [`Agent::repo_dir`] exists, and the agent starts in it.
    /// Programs in the `bin` folder of [`Agent::config_dir`] must be on the
    /// agent's `PATH`: sandcastle puts its Git helper there.
    fn launch(&self, password: &str, wait_for: (&str, u16)) -> Launch;
}
