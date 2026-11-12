//! OpenCode as the agent. Behaviour checked against 2.2.7 in the spike:
//! `opencode serve` provides the web UI and the API, `/` answers 200 without
//! credentials, `/api/*` needs Basic Auth (user `opencode`) with
//! `OPENCODE_SERVER_PASSWORD`, and the config file is found through
//! `OPENCODE_CONFIG`.

use serde_json::{json, Map, Value};

use super::{Agent, ConfigFile, ImageSource, Launch};

/// Must match `OPENCODE_VERSION` in the Containerfile.
const VERSION: &str = "2.2.7";
const UI_PORT: u16 = 4096;
const CONFIG_DIR: &str = "/etc/sandcastle";
const CONFIG_NAME: &str = "opencode.json";

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

    fn config_dir(&self) -> &str {
        CONFIG_DIR
    }

    fn config(&self, llm_base_url: &str, models: &[String]) -> ConfigFile {
        let models: Map<String, Value> = models.iter().map(|id| (id.clone(), json!({}))).collect();
        let config = json!({
            "$schema": "https://opencode.ai/config.json",
            // Full autonomy: safety comes from the box, not from permission prompts.
            "permission": { "*": "allow" },
            "provider": {
                "relay": {
                    "npm": "@ai-sdk/openai-compatible",
                    "name": "LLM gateway",
                    // The key is a placeholder; the relay on the host adds the real one.
                    "options": { "baseURL": llm_base_url, "apiKey": "sandcastle-placeholder" },
                    "models": models,
                }
            }
        });
        ConfigFile {
            name: CONFIG_NAME,
            content: serde_json::to_string_pretty(&config).expect("JSON values serialize"),
        }
    }

    fn ui_port(&self) -> u16 {
        UI_PORT
    }

    fn health_path(&self) -> &str {
        "/"
    }

    fn api_user(&self) -> &str {
        "opencode"
    }

    fn launch(&self, password: &str, wait_for: (&str, u16)) -> Launch {
        let (host, port) = wait_for;
        // bash's /dev/tcp needs no extra tool in the image. Host and port come
        // from sandcastle (an address and a number), never from the repo.
        let script = format!(
            "until (exec 3<>/dev/tcp/{host}/{port}) 2>/dev/null; do sleep 0.2; done; \
             exec opencode serve --hostname 0.0.0.0 --port {UI_PORT}"
        );
        Launch {
            command: vec!["bash".into(), "-c".into(), script],
            env: vec![
                ("OPENCODE_SERVER_PASSWORD".to_string(), password.to_string()),
                ("OPENCODE_CONFIG".to_string(), format!("{CONFIG_DIR}/{CONFIG_NAME}")),
            ],
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
        let launch = OpenCode.launch("s3cret", ("192.168.128.1", 4180));
        assert!(!launch.command.iter().any(|arg| arg.contains("s3cret")));
        assert!(launch.env.contains(&("OPENCODE_SERVER_PASSWORD".into(), "s3cret".into())));
    }

    #[test]
    fn agent_waits_for_the_relay_before_it_starts() {
        let launch = OpenCode.launch("pw", ("192.168.128.1", 4180));
        let script = &launch.command[2];
        let wait = script.find("/dev/tcp/192.168.128.1/4180").unwrap();
        assert!(wait < script.find("opencode serve").unwrap());
    }

    #[test]
    fn config_points_at_the_relay_and_holds_no_real_key() {
        let file = OpenCode.config("http://192.168.128.1:4180/v1", &["a/one".into(), "b/two".into()]);
        let json: Value = serde_json::from_str(&file.content).unwrap();
        let provider = &json["provider"]["relay"];
        assert_eq!(provider["options"]["baseURL"], "http://192.168.128.1:4180/v1");
        assert_eq!(provider["options"]["apiKey"], "sandcastle-placeholder");
        assert!(provider["models"]["a/one"].is_object() && provider["models"]["b/two"].is_object());
        assert_eq!(json["permission"]["*"], "allow");
    }
}
