//! OpenCode as the agent. Behaviour checked against 2.2.7 in the spike:
//! `opencode serve` provides the web UI and the API, `/` answers 200 without
//! credentials, `/api/*` needs Basic Auth with `OPENCODE_SERVER_PASSWORD`.

use super::{Agent, ImageSource, Launch};

/// Must match `OPENCODE_VERSION` in the Containerfile.
const VERSION: &str = "2.2.7";
const UI_PORT: u16 = 4096;

pub struct OpenCode;

impl Agent for OpenCode {
    fn image(&self) -> ImageSource {
        ImageSource {
            tag: format!("sandcastle-opencode:{VERSION}"),
            containerfile: include_str!("Containerfile"),
        }
    }

    fn uid(&self) -> u32 {
        1001
    }

    fn home(&self) -> &str {
        "/home/agent"
    }

    fn workspace(&self) -> &str {
        "/workspace"
    }

    fn ui_port(&self) -> u16 {
        UI_PORT
    }

    fn health_path(&self) -> &str {
        "/"
    }

    fn launch(&self, password: &str) -> Launch {
        Launch {
            command: ["opencode", "serve", "--hostname", "0.0.0.0", "--port", &UI_PORT.to_string()]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            env: vec![("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string())],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containerfile_pins_the_same_version_as_the_tag() {
        let image = OpenCode.image();
        assert!(image.containerfile.contains(&format!("ARG OPENCODE_VERSION={VERSION}")));
        assert!(image.tag.ends_with(VERSION));
    }

    #[test]
    fn password_is_passed_by_environment_not_by_argument() {
        let launch = OpenCode.launch("s3cret");
        assert!(!launch.command.iter().any(|arg| arg.contains("s3cret")));
        assert!(launch.env.contains(&("OPENCODE_SERVER_PASSWORD".into(), "s3cret".into())));
    }
}
