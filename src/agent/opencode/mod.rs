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
            // Only the gateway's models: without this OpenCode also offers its
            // built-in providers, which the box cannot reach anyway.
            "enabled_providers": ["relay"],
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

    /// The newest session, or a new one if the box has none. The web UI's
    /// start page is empty until the user finds the small "+"; a session URL
    /// opens the chat directly. URL form checked against 2.2.7:
    /// `/server/<base64url of the origin, unpadded>/session/<id>`.
    fn landing_path(&self, origin: &str, api: &dyn Fn(&str, &str, &str) -> anyhow::Result<String>) -> String {
        let id_of = |body: anyhow::Result<String>, pointer: &str| -> Option<String> {
            let value: Value = serde_json::from_str(&body.ok()?).ok()?;
            let id = value.pointer(pointer)?.as_str()?;
            let plain = !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            plain.then(|| id.to_string())
        };
        let session = id_of(api("GET", "/api/session?limit=1", ""), "/data/0/id")
            .or_else(|| id_of(api("POST", "/api/session", "{}"), "/data/id"));
        match session {
            Some(id) => {
                let server = crate::relay::http::base64(origin.as_bytes())
                    .trim_end_matches('=')
                    .replace('+', "-")
                    .replace('/', "_");
                format!("/server/{server}/session/{id}")
            }
            None => "/".to_string(),
        }
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
             cd {repo} && exec opencode serve --hostname 0.0.0.0 --port {UI_PORT}",
            repo = self.repo_dir()
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
        let cd = script.find("cd /workspace/repo").unwrap();
        assert!(wait < cd && cd < script.find("opencode serve").unwrap());
    }

    #[test]
    fn landing_is_the_newest_session_or_a_new_one_or_the_root() {
        let origin = "http://127.0.0.1:57702";
        let existing = |method: &str, _: &str, _: &str| -> anyhow::Result<String> {
            assert_eq!(method, "GET");
            Ok(r#"{"data":[{"id":"ses_abc"}]}"#.into())
        };
        assert_eq!(OpenCode.landing_path(origin, &existing), "/server/aHR0cDovLzEyNy4wLjAuMTo1NzcwMg/session/ses_abc");
        let none_yet = |method: &str, _: &str, _: &str| -> anyhow::Result<String> {
            Ok(if method == "GET" { r#"{"data":[]}"# } else { r#"{"data":{"id":"ses_new"}}"# }.into())
        };
        assert!(OpenCode.landing_path(origin, &none_yet).ends_with("/session/ses_new"));
        let broken = |_: &str, _: &str, _: &str| -> anyhow::Result<String> { Ok(r#"{"data":{"id":"../x"}}"#.into()) };
        assert_eq!(OpenCode.landing_path(origin, &broken), "/");
        let down = |_: &str, _: &str, _: &str| -> anyhow::Result<String> { anyhow::bail!("no answer") };
        assert_eq!(OpenCode.landing_path(origin, &down), "/");
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
        assert_eq!(json["enabled_providers"], json!(["relay"]));
    }
}
