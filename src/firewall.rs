//! Host firewall: pf rules that keep a box away from everything on this Mac
//! except the relay, and keep the LAN away from the box (CLAUDE.md 2.6).
//!
//! Why it is needed: an "internal" network isolates the box from the internet
//! and the LAN, but not from the Mac. Without rules the box reaches any host
//! service that listens on all addresses, and because the Mac forwards IP, a
//! LAN device with a route could reach the box (spike items 1, 4 and 9).
//!
//! Root is needed twice: once in `init` to add an anchor line to
//! `/etc/pf.conf`, and once per session (one `up`, or one run of the app). A
//! small root helper is started during the secrets phase, so the password
//! prompt comes first. The helper loads and removes the rules of each box
//! when it is told to, and removes all of them when its input closes, which
//! also happens if sandcastle crashes.

use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

pub const PF_CONF: &str = "/etc/pf.conf";
const ANCHOR_LINE: &str = r#"anchor "sandcastle/*""#;

/// Whether `/etc/pf.conf` has sandcastle's anchor line. Needs no root.
pub fn anchor_configured() -> Result<bool> {
    let text = std::fs::read_to_string(PF_CONF).with_context(|| format!("could not read {PF_CONF}"))?;
    Ok(has_anchor_line(&text))
}

fn has_anchor_line(pf_conf: &str) -> bool {
    pf_conf.lines().any(|line| line.trim() == ANCHOR_LINE)
}

fn sudo(reason: &str, why: &str) -> Command {
    println!("\n>> macOS login password or Touch ID (sudo, admin rights on this Mac)");
    println!("   Needed to: {reason}");
    println!("   Why: {why}");
    println!("   (No prompt appears if sudo was used in this terminal a moment ago.)");
    let mut command = Command::new("sudo");
    command.args(["-p", "   macOS password for %u: "]);
    command
}

/// Add the anchor line to `/etc/pf.conf` and load it. Keeps a backup and
/// restores it if pf rejects the new file.
pub fn install_anchor() -> Result<()> {
    if anchor_configured()? {
        return Ok(());
    }
    // The file is checked with `pfctl -n` before it replaces the live rules.
    let script = format!(
        r#"set -e
cp {PF_CONF} {PF_CONF}.sandcastle-backup
printf '\n# sandcastle: rules for each running box are loaded below this anchor.\n%s\n' '{ANCHOR_LINE}' >> {PF_CONF}
if ! pfctl -nf {PF_CONF} 2>/dev/null; then
  cp {PF_CONF}.sandcastle-backup {PF_CONF}
  echo "pf rejected the new {PF_CONF}; the old file was restored" >&2
  exit 1
fi
pfctl -f {PF_CONF} 2>/dev/null"#
    );
    let status = sudo(
        &format!("add one line to {PF_CONF} and reload it (backup: {PF_CONF}.sandcastle-backup)"),
        "the line gives sandcastle a place for per-box rules; it allows or blocks nothing by itself",
    )
        .args(["/bin/sh", "-c", &script])
        .status()?;
    if !status.success() {
        bail!("could not add the anchor line to {PF_CONF}");
    }
    println!("Added `{ANCHOR_LINE}` to {PF_CONF}.");
    Ok(())
}

/// The rules for one box, by bridge interface so that IPv6 is covered too.
/// Verified in spike item 9.
pub fn rules(interface: &str, subnet: &str, gateway: IpAddr, relay_ports: &[u16], agent_port: u16) -> Vec<String> {
    let ports: Vec<String> = relay_ports.iter().map(u16::to_string).collect();
    vec![
        // The box may talk to the relay, and to nothing else on this Mac.
        format!("pass in quick on {interface} inet proto tcp from {subnet} to {gateway} port {{ {} }}", ports.join(", ")),
        // Only the Mac itself may talk to the agent; forwarded LAN traffic may not.
        format!("pass out quick on {interface} inet proto tcp from {gateway} to {subnet} port {agent_port} keep state"),
        format!("block return in quick on {interface} all"),
        format!("block return out quick on {interface} all"),
    ]
}

/// Name of the host interface that carries `gateway`, from `ifconfig` output.
pub fn interface_for(gateway: IpAddr) -> Result<String> {
    let output = Command::new("ifconfig").output().context("could not run ifconfig")?;
    parse_interface(&String::from_utf8_lossy(&output.stdout), gateway)
        .ok_or_else(|| anyhow!("no network interface on this Mac has the address {gateway}"))
}

fn parse_interface(ifconfig: &str, gateway: IpAddr) -> Option<String> {
    let wanted = gateway.to_string();
    let mut current = None;
    for line in ifconfig.lines() {
        if !line.starts_with(char::is_whitespace) {
            current = line.split(':').next();
        } else {
            let mut words = line.split_whitespace();
            if words.next() == Some("inet") && words.next() == Some(wanted.as_str()) {
                return current.map(str::to_string);
            }
        }
    }
    None
}

/// The root helper for a session: one `sudo`, any number of boxes. See the
/// module documentation.
pub struct Firewall {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

/// Runs as root. Line protocol on stdin:
/// - `load <box> <interface> <count>` followed by that many rules; answers `loaded` or `failed`,
/// - `unload <box>`; answers `unloaded`,
/// - `blocked`; answers the boxes left blocked by an earlier helper, then `end`.
///
/// It answers `ready` first. Signals are ignored so that Ctrl-C in the
/// terminal cannot kill it before the cleanup. Box and interface names are
/// checked here too, because they become part of pf anchor names and rules.
///
/// Fail closed: when stdin closes while a box still has rules (sandcastle
/// crashed, or a box could not be stopped), the box may still be running. Its
/// rules are then replaced by "block everything on its interface", pf stays
/// enabled, and the box name and the pf token are written to `$1` (a
/// root-only directory) so that a later helper can clean up: `unload` or a
/// new `load` for that box removes its entry, and once no entry is left the
/// kept tokens are released.
const HELPER: &str = r#"trap '' INT TERM HUP
blocked="$1/sandcastle-pf-blocked"
tokens="$1/sandcastle-pf-tokens"
if pfctl -sr 2>/dev/null </dev/null | grep -q '^anchor "sandcastle/\*"'; then echo ready; else echo no-anchor; exit 0; fi
token=$(pfctl -E 2>&1 </dev/null | awk '/Token/{print $NF}')
boxes=""
valid() { case "$1" in ''|*[!a-z0-9-]*) return 1;; esac; return 0; }
forget() {
  [ -f "$blocked" ] || return 0
  grep -vx "$1" "$blocked" >"$blocked.new"; mv "$blocked.new" "$blocked"
  [ -s "$blocked" ] && return 0
  rm -f "$blocked"
  [ -f "$tokens" ] || return 0
  while IFS= read -r old; do pfctl -X "$old" >/dev/null 2>&1 </dev/null; done <"$tokens"
  rm -f "$tokens"
}
while IFS=' ' read -r command box interface count; do
  case "$command" in
    load)
      rules=""; i=0
      while [ "$i" -lt "${count:-0}" ]; do IFS= read -r rule || break; rules="$rules$rule
"; i=$((i+1)); done
      if valid "$box" && valid "$interface" && printf '%s' "$rules" | pfctl -a "sandcastle/$box" -f - 2>/dev/null; then
        boxes="$boxes $box:$interface"; forget "$box"; echo loaded
      else echo failed; fi;;
    unload)
      if valid "$box"; then
        pfctl -a "sandcastle/$box" -F all >/dev/null 2>&1 </dev/null
        kept=""; for entry in $boxes; do [ "${entry%%:*}" = "$box" ] || kept="$kept $entry"; done; boxes="$kept"
        forget "$box"
      fi
      echo unloaded;;
    blocked)
      [ -f "$blocked" ] && cat "$blocked"
      echo end;;
  esac
done
left=""
for entry in $boxes; do
  box="${entry%%:*}"; interface="${entry#*:}"
  printf 'block return in quick on %s all\nblock return out quick on %s all\n' "$interface" "$interface" \
    | pfctl -a "sandcastle/$box" -f - 2>/dev/null
  echo "$box" >>"$blocked"; left=yes
done
if [ -n "$left" ]; then [ -n "$token" ] && echo "$token" >>"$tokens"
elif [ -n "$token" ]; then pfctl -X "$token" >/dev/null 2>&1 </dev/null; fi
exit 0"#;

/// Where the helper keeps what must survive it. Root-only, cleared at boot,
/// like pf's own state.
const HELPER_STATE_DIR: &str = "/var/run";

/// How `sudo` asks for the password.
pub enum Prompt<'a> {
    /// On the terminal sandcastle runs in.
    Terminal,
    /// Through this program (`sudo -A`), for apps without a terminal.
    Askpass(&'a std::path::Path),
}

impl Firewall {
    /// Start the helper. This is where the sudo prompt appears.
    pub fn start(prompt: Prompt) -> Result<Self> {
        let mut command = sudo(
            "load firewall (pf) rules for each box, and remove them when it stops",
            "a box may reach only sandcastle's relay on this Mac, and only this Mac may reach the box",
        );
        if let Prompt::Askpass(program) = prompt {
            command.arg("-A").env("SUDO_ASKPASS", program);
        }
        command.args(["/bin/sh", "-c", HELPER, "sandcastle-firewall", HELPER_STATE_DIR]);
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("could not start sudo")?;
        let input = child.stdin.take();
        let mut output = BufReader::new(child.stdout.take().expect("stdout was piped"));
        let mut line = String::new();
        output.read_line(&mut line)?;
        match line.trim() {
            "ready" => Ok(Firewall { child, input, output }),
            "no-anchor" => bail!(
                "pf does not have sandcastle's anchor loaded. Run `sandcastle init` (a macOS update may have reset {PF_CONF})"
            ),
            _ => bail!("sudo did not start the firewall helper (wrong password, or no admin rights?)"),
        }
    }

    fn command(&mut self, lines: &[String]) -> Result<String> {
        let input = self.input.as_mut().ok_or_else(|| anyhow!("the firewall helper is gone"))?;
        for line in lines {
            writeln!(input, "{line}")?;
        }
        input.flush()?;
        let mut answer = String::new();
        self.output.read_line(&mut answer)?;
        Ok(answer.trim().to_string())
    }

    /// Load the rules for a box. Until this returns, the box is not confined.
    /// `interface` is the box's bridge; the helper blocks it if sandcastle
    /// goes away without unloading.
    pub fn load(&mut self, box_name: &str, interface: &str, rules: &[String]) -> Result<()> {
        let mut lines = vec![format!("load {box_name} {interface} {}", rules.len())];
        lines.extend(rules.iter().cloned());
        if self.command(&lines)? != "loaded" {
            bail!("pf rejected the firewall rules for box {box_name}");
        }
        Ok(())
    }

    /// Boxes an earlier helper left blocked because sandcastle went away
    /// while they had rules.
    pub fn blocked(&mut self) -> Result<Vec<String>> {
        let mut names = vec![self.command(&["blocked".to_string()])?];
        while names.last().is_some_and(|line| line != "end") {
            let mut line = String::new();
            if self.output.read_line(&mut line)? == 0 {
                bail!("the firewall helper is gone");
            }
            names.push(line.trim().to_string());
        }
        names.pop();
        Ok(names)
    }

    /// Remove the rules of a box that has stopped. Never call this for a box
    /// that may still be running.
    pub fn unload(&mut self, box_name: &str) -> Result<()> {
        if self.command(&[format!("unload {box_name}")])? != "unloaded" {
            bail!("the firewall helper did not remove the rules for box {box_name}");
        }
        Ok(())
    }
}

impl Drop for Firewall {
    /// Closing the helper's input ends it. Boxes that still have rules are
    /// blocked completely (see [`HELPER`]); wait for that.
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_line_is_recognised_exactly() {
        assert!(has_anchor_line("anchor \"com.apple/*\"\n  anchor \"sandcastle/*\"  \n"));
        assert!(!has_anchor_line("anchor \"com.apple/*\"\n# anchor \"sandcastle/*\"\n"));
        assert!(!has_anchor_line(""));
    }

    #[test]
    fn rules_match_the_set_verified_in_the_spike() {
        let rules = rules("bridge100", "192.168.128.0/24", "192.168.128.1".parse().unwrap(), &[4180, 4181], 4096);
        assert_eq!(
            rules,
            [
                "pass in quick on bridge100 inet proto tcp from 192.168.128.0/24 to 192.168.128.1 port { 4180, 4181 }",
                "pass out quick on bridge100 inet proto tcp from 192.168.128.1 to 192.168.128.0/24 port 4096 keep state",
                "block return in quick on bridge100 all",
                "block return out quick on bridge100 all",
            ]
        );
    }

    /// A helper with a fake `pfctl` that records its calls in `<dir>/calls`.
    fn fake_helper(dir: &std::path::Path) -> Firewall {
        std::fs::create_dir_all(dir).unwrap();
        let fake = format!(
            "#!/bin/sh\ncase \"$1\" in -sr) echo 'anchor \"sandcastle/*\" all';; -E) echo 'Token : 42';; \
             -a) if [ \"$3\" = -f ]; then echo \"load $2: $(cat | tr '\\n' '|')\" >>{0}; else echo \"flush $2\" >>{0}; fi;; \
             -X) echo \"release $2\" >>{0};; esac\n",
            dir.join("calls").display()
        );
        std::fs::write(dir.join("pfctl"), fake).unwrap();
        assert!(Command::new("chmod").arg("+x").arg(dir.join("pfctl")).status().unwrap().success());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", HELPER, "sandcastle-firewall"]).arg(dir);
        command.env("PATH", format!("{}:/usr/bin:/bin", dir.display()));
        Firewall::spawn(command).unwrap()
    }

    fn calls(dir: &std::path::Path) -> Vec<String> {
        let text = std::fs::read_to_string(dir.join("calls")).unwrap_or_default();
        std::fs::remove_file(dir.join("calls")).ok();
        text.lines().map(str::to_string).collect()
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sandcastle-fw-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn helper_loads_and_unloads_and_releases_pf_when_nothing_is_left() {
        let dir = test_dir("orderly");
        let mut firewall = fake_helper(&dir);
        firewall.load("sc-a-1", "bridge100", &["rule one".into(), "rule two".into()]).unwrap();
        firewall.load("sc-b-2", "bridge101", &["rule three".into()]).unwrap();
        // Names that could leave the anchor namespace or the rule are refused,
        // and the helper stays in step with the protocol.
        assert!(firewall.load("../evil", "bridge100", &["rule four".into()]).is_err());
        assert!(firewall.load("sc-c-3", "all; pass", &["rule five".into()]).is_err());
        firewall.unload("sc-a-1").unwrap();
        firewall.unload("sc-b-2").unwrap();
        assert!(firewall.blocked().unwrap().is_empty());
        drop(firewall);

        assert_eq!(
            calls(&dir),
            [
                "load sandcastle/sc-a-1: rule one|rule two|",
                "load sandcastle/sc-b-2: rule three|",
                "flush sandcastle/sc-a-1",
                "flush sandcastle/sc-b-2",
                "release 42",
            ]
        );
        assert!(!dir.join("sandcastle-pf-blocked").exists() && !dir.join("sandcastle-pf-tokens").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn helper_fails_closed_and_a_later_helper_cleans_up() {
        let dir = test_dir("crash");
        let mut firewall = fake_helper(&dir);
        firewall.load("sc-a-1", "bridge100", &["rule one".into()]).unwrap();
        firewall.load("sc-b-2", "bridge101", &["rule two".into()]).unwrap();
        firewall.unload("sc-b-2").unwrap();
        // sandcastle goes away while sc-a-1 still has rules.
        drop(firewall);
        assert_eq!(
            calls(&dir),
            [
                "load sandcastle/sc-a-1: rule one|",
                "load sandcastle/sc-b-2: rule two|",
                "flush sandcastle/sc-b-2",
                "load sandcastle/sc-a-1: block return in quick on bridge100 all|block return out quick on bridge100 all|",
                // No "release": pf must stay enabled for the block rules.
            ]
        );

        // The next session sees the blocked box and removes its rules; only
        // then is the kept token released, and its own at the end.
        let mut firewall = fake_helper(&dir);
        assert_eq!(firewall.blocked().unwrap(), ["sc-a-1"]);
        firewall.unload("sc-a-1").unwrap();
        assert!(firewall.blocked().unwrap().is_empty());
        drop(firewall);
        assert_eq!(calls(&dir), ["flush sandcastle/sc-a-1", "release 42", "release 42"]);
        assert!(!dir.join("sandcastle-pf-blocked").exists() && !dir.join("sandcastle-pf-tokens").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn interface_is_found_by_its_address() {
        let ifconfig = "lo0: flags=8049<UP,LOOPBACK,RUNNING,MULTICAST> mtu 16384\n\
            \tinet 127.0.0.1 netmask 0xff000000\n\
            en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\
            \tinet 192.168.1.248 netmask 0xffffff00 broadcast 192.168.1.255\n\
            bridge100: flags=8a63<UP,BROADCAST,SMART,RUNNING,ALLMULTI,SIMPLEX,MULTICAST> mtu 1500\n\
            \tinet6 fe80::50f2:65ff:feae:8b64%bridge100 prefixlen 64 scopeid 0x1a\n\
            \tinet 192.168.128.1 netmask 0xffffff00 broadcast 192.168.128.255\n";
        assert_eq!(parse_interface(ifconfig, "192.168.128.1".parse().unwrap()).as_deref(), Some("bridge100"));
        assert_eq!(parse_interface(ifconfig, "192.168.1.248".parse().unwrap()).as_deref(), Some("en0"));
        assert_eq!(parse_interface(ifconfig, "192.168.128.10".parse().unwrap()), None);
    }
}
