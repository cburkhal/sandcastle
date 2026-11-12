//! Sandbox runtime backed by Apple's `container` CLI.
//!
//! This is the only file that may invoke `container`. Flags change between
//! releases; everything here was checked against 1.7.1 in the spike
//! (spike/NOTES.md).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use super::{BoxSpec, BoxState, BoxStatus, ExecOutput, Mount, Network, NetworkInfo, SandboxRuntime};

const BIN: &str = "container";

pub struct AppleContainer;

impl AppleContainer {
    pub fn new() -> Self {
        AppleContainer
    }
}

/// Run `container` with the given arguments and capture its output.
fn call(args: &[String], stdin: Option<&[u8]>) -> Result<Output> {
    let mut command = Command::new(BIN);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    let mut child = command
        .spawn()
        .with_context(|| format!("could not start `{BIN}`; is Apple container installed?"))?;
    if let Some(data) = stdin {
        // Dropping the handle closes the pipe so the child sees end of input.
        child.stdin.take().expect("stdin was piped").write_all(data)?;
    }
    Ok(child.wait_with_output()?)
}

/// Like [`call`], but a non-zero exit is an error that carries stderr.
fn call_ok(args: &[String]) -> Result<String> {
    let output = call(args, None)?;
    if !output.status.success() {
        bail!(
            "`{BIN} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Names printed by `container <kind> list --quiet`.
fn list_names(kind: &str) -> Result<Vec<String>> {
    let out = call_ok(&strings(&[kind, "list", "--quiet"]))?;
    Ok(out.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
}

/// Arguments for `container run`, without the leading `run` and mode flags.
fn run_args(spec: &BoxSpec) -> Vec<String> {
    let mut args = vec!["--name".to_string(), spec.name.clone()];
    args.push("--network".into());
    args.push(match &spec.network {
        Network::None => "none".into(),
        Network::Named(name) => name.clone(),
    });
    if !spec.dns {
        args.push("--no-dns".into());
    }
    if spec.readonly_rootfs {
        args.push("--read-only".into());
    }
    for cap in &spec.cap_drop {
        args.push("--cap-drop".into());
        args.push(cap.clone());
    }
    for cap in &spec.cap_add {
        args.push("--cap-add".into());
        args.push(cap.clone());
    }
    if let Some(user) = &spec.user {
        args.push("--user".into());
        args.push(user.clone());
    }
    if let Some(cpus) = spec.cpus {
        args.push("--cpus".into());
        args.push(cpus.to_string());
    }
    if let Some(memory) = &spec.memory {
        args.push("--memory".into());
        args.push(memory.clone());
    }
    for mount in &spec.mounts {
        match mount {
            Mount::Volume { name, target } => {
                args.push("--volume".into());
                args.push(format!("{name}:{target}"));
            }
            Mount::Bind { source, target, readonly } => {
                let mut value = format!("type=bind,source={},target={target}", source.display());
                if *readonly {
                    value.push_str(",readonly");
                }
                args.push("--mount".into());
                args.push(value);
            }
            Mount::Tmpfs { target } => {
                args.push("--tmpfs".into());
                args.push(target.clone());
            }
        }
    }
    for (key, value) in &spec.env {
        args.push("--env".into());
        args.push(format!("{key}={value}"));
    }
    args.push(spec.image.clone());
    args.extend(spec.command.iter().cloned());
    args
}

fn exec_output(output: Output) -> ExecOutput {
    ExecOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// Parse the output of `container inspect <name>`.
fn parse_inspect(json: &str) -> Result<BoxStatus> {
    let value: Value = serde_json::from_str(json).context("unexpected `container inspect` output")?;
    let Some(entry) = value.get(0) else {
        return Ok(BoxStatus { state: BoxState::Missing, ipv4: None });
    };
    let status = &entry["status"];
    let state = match status["state"].as_str() {
        Some("running") => BoxState::Running,
        _ => BoxState::Stopped,
    };
    let ipv4 = status["networks"][0]["ipv4Address"]
        .as_str()
        .map(|address| address.split('/').next().unwrap_or(address).to_string());
    Ok(BoxStatus { state, ipv4 })
}

/// Parse the output of `container network inspect <name>`.
fn parse_network(json: &str) -> Result<NetworkInfo> {
    let value: Value = serde_json::from_str(json).context("unexpected `container network inspect` output")?;
    let status = &value[0]["status"];
    let field = |name: &str| {
        status[name]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| anyhow!("`container network inspect` has no status.{name}"))
    };
    Ok(NetworkInfo { gateway: field("ipv4Gateway")?, subnet: field("ipv4Subnet")? })
}

impl SandboxRuntime for AppleContainer {
    fn image_build(&self, tag: &str, containerfile: &Path, context: &Path) -> Result<()> {
        // Build output is long and useful, so it goes straight to the terminal.
        let status = Command::new(BIN)
            .args(["build", "--tag", tag, "--file"])
            .arg(containerfile)
            .arg(context)
            .status()
            .with_context(|| format!("could not start `{BIN}`; is Apple container installed?"))?;
        if !status.success() {
            bail!("image build failed");
        }
        Ok(())
    }

    fn image_digest(&self, tag: &str) -> Result<Option<String>> {
        let output = call(&strings(&["image", "inspect", tag]), None)?;
        if !output.status.success() {
            return Ok(None);
        }
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        Ok(value[0]["configuration"]["descriptor"]["digest"].as_str().map(str::to_string))
    }

    fn network_create(&self, name: &str) -> Result<bool> {
        if list_names("network")?.iter().any(|n| n == name) {
            return Ok(false);
        }
        call_ok(&strings(&["network", "create", "--internal", name]))?;
        Ok(true)
    }

    fn network_inspect(&self, name: &str) -> Result<Option<NetworkInfo>> {
        if !list_names("network")?.iter().any(|n| n == name) {
            return Ok(None);
        }
        let out = call_ok(&strings(&["network", "inspect", name]))?;
        parse_network(&out).map(Some)
    }

    fn network_remove(&self, name: &str) -> Result<()> {
        if list_names("network")?.iter().any(|n| n == name) {
            call_ok(&strings(&["network", "delete", name]))?;
        }
        Ok(())
    }

    fn volume_create(&self, name: &str) -> Result<bool> {
        if list_names("volume")?.iter().any(|n| n == name) {
            return Ok(false);
        }
        call_ok(&strings(&["volume", "create", name]))?;
        Ok(true)
    }

    fn volume_remove(&self, name: &str) -> Result<()> {
        if list_names("volume")?.iter().any(|n| n == name) {
            call_ok(&strings(&["volume", "delete", name]))?;
        }
        Ok(())
    }

    fn run(&self, spec: &BoxSpec) -> Result<()> {
        let mut args = strings(&["run", "--detach"]);
        args.extend(run_args(spec));
        call_ok(&args)?;
        Ok(())
    }

    fn run_once(&self, spec: &BoxSpec) -> Result<ExecOutput> {
        let mut args = strings(&["run", "--rm"]);
        args.extend(run_args(spec));
        Ok(exec_output(call(&args, None)?))
    }

    fn exec(&self, name: &str, command: &[String], stdin: Option<&[u8]>) -> Result<ExecOutput> {
        let mut args = strings(&["exec"]);
        if stdin.is_some() {
            // Without -i the runtime does not forward stdin (spike item 3).
            args.push("--interactive".into());
        }
        args.push(name.to_string());
        args.extend(command.iter().cloned());
        Ok(exec_output(call(&args, stdin)?))
    }

    fn stop(&self, name: &str) -> Result<()> {
        if self.inspect(name)?.state == BoxState::Running {
            call_ok(&strings(&["stop", name]))?;
        }
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<()> {
        if self.inspect(name)?.state != BoxState::Missing {
            call_ok(&strings(&["delete", "--force", name]))?;
        }
        Ok(())
    }

    fn inspect(&self, name: &str) -> Result<BoxStatus> {
        let output = call(&strings(&["inspect", name]), None)?;
        if !output.status.success() {
            // `container inspect` exits non-zero with "container not found".
            return Ok(BoxStatus { state: BoxState::Missing, ipv4: None });
        }
        parse_inspect(&String::from_utf8_lossy(&output.stdout))
    }

    fn logs(&self, name: &str, lines: usize) -> Result<String> {
        let output = call(&strings(&["logs", name]), None)?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let all: Vec<&str> = text.lines().collect();
        Ok(all[all.len().saturating_sub(lines)..].join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_args_cover_the_hardening_flags() {
        let mut spec = BoxSpec::new("sc-demo", "image:tag");
        spec.network = Network::Named("sc-demo".into());
        spec.readonly_rootfs = true;
        spec.cap_drop = vec!["ALL".into()];
        spec.user = Some("1001".into());
        spec.cpus = Some(2);
        spec.memory = Some("4G".into());
        spec.mounts = vec![
            Mount::Volume { name: "sc-demo-home".into(), target: "/home/agent".into() },
            Mount::Bind { source: "/repo/.git".into(), target: "/source.git".into(), readonly: true },
            Mount::Tmpfs { target: "/tmp".into() },
        ];
        spec.env = vec![("A".into(), "b c".into())];
        spec.command = vec!["opencode".into(), "serve".into()];

        let args = run_args(&spec).join(" ");
        assert_eq!(
            args,
            "--name sc-demo --network sc-demo --no-dns --read-only --cap-drop ALL --user 1001 \
             --cpus 2 --memory 4G --volume sc-demo-home:/home/agent \
             --mount type=bind,source=/repo/.git,target=/source.git,readonly --tmpfs /tmp \
             --env A=b c image:tag opencode serve"
        );
    }

    #[test]
    fn default_spec_has_no_network_and_no_dns() {
        let args = run_args(&BoxSpec::new("x", "img"));
        assert_eq!(args, ["--name", "x", "--network", "none", "--no-dns", "img"]);
    }

    #[test]
    fn inspect_reads_state_and_strips_the_prefix_length() {
        let json = r#"[{"status":{"state":"running","networks":[{"ipv4Address":"192.168.128.2/24"}]}}]"#;
        let status = parse_inspect(json).unwrap();
        assert_eq!(status.state, BoxState::Running);
        assert_eq!(status.ipv4.as_deref(), Some("192.168.128.2"));
    }

    #[test]
    fn inspect_of_a_stopped_box_has_no_address() {
        let status = parse_inspect(r#"[{"status":{"state":"stopped","networks":[]}}]"#).unwrap();
        assert_eq!(status, BoxStatus { state: BoxState::Stopped, ipv4: None });
    }

    #[test]
    fn empty_inspect_means_missing() {
        assert_eq!(parse_inspect("[]").unwrap().state, BoxState::Missing);
    }

    #[test]
    fn network_inspect_reads_gateway_and_subnet() {
        let json = r#"[{"status":{"ipv4Gateway":"192.168.128.1","ipv4Subnet":"192.168.128.0/24"}}]"#;
        let info = parse_network(json).unwrap();
        assert_eq!(info.gateway, "192.168.128.1");
        assert_eq!(info.subnet, "192.168.128.0/24");
    }
}
