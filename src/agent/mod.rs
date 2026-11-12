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
    /// Port of the agent's web UI and API inside the box.
    fn ui_port(&self) -> u16;
    /// Path that answers 200 without credentials once the agent is up.
    fn health_path(&self) -> &str;
    /// Process and environment. `password` protects the agent's API; only the
    /// relay knows it.
    fn launch(&self, password: &str) -> Launch;
}
