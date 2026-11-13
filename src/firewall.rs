//! Host firewall: pf rules that keep a box away from everything on this Mac
//! except the relay, and keep the LAN away from the box (CLAUDE.md 2.6).
//!
//! Why it is needed: an "internal" network isolates the box from the internet
//! and the LAN, but not from the Mac. Without rules the box reaches any host
//! service that listens on all addresses, and because the Mac forwards IP, a
//! LAN device with a route could reach the box (spike items 1, 4 and 9).
//!
//! Root is needed twice: once in `init` to add an anchor line to
//! `/etc/pf.conf`, and once per `up`. For `up`, a small root helper is started
//! during the secrets phase, so the password prompt comes first. The helper
//! loads the rules when it is told to and removes them when its input closes,
//! which also happens if sandcastle crashes.

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

/// The root helper for one box. See the module documentation.
pub struct Firewall {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

/// Runs as root. Line protocol on stdin: a rule count, then that many rules.
/// It answers `ready` first, then `loaded` or `failed`, and cleans up when
/// stdin closes. Signals are ignored so that Ctrl-C in the terminal cannot
/// kill it before the cleanup.
const HELPER: &str = r#"trap '' INT TERM HUP
anchor="$1"
if pfctl -sr 2>/dev/null | grep -q '^anchor "sandcastle/\*"'; then echo ready; else echo no-anchor; exit 0; fi
IFS= read -r count || exit 0
token=$(pfctl -E 2>&1 | awk '/Token/{print $NF}')
i=0
while [ "$i" -lt "$count" ]; do IFS= read -r rule || break; printf '%s\n' "$rule"; i=$((i+1)); done \
  | pfctl -a "$anchor" -f - 2>/dev/null && echo loaded || echo failed
cat >/dev/null
pfctl -a "$anchor" -F all >/dev/null 2>&1
[ -n "$token" ] && pfctl -X "$token" >/dev/null 2>&1
exit 0"#;

impl Firewall {
    /// Start the helper. This is where the sudo prompt appears.
    pub fn start(box_name: &str) -> Result<Self> {
        let mut child = sudo(
            "load firewall (pf) rules for this box, and remove them when it stops",
            "the box may reach only sandcastle's relay on this Mac, and only this Mac may reach the box",
        )
            .args(["/bin/sh", "-c", HELPER, "sandcastle-firewall"])
            .arg(format!("sandcastle/{box_name}"))
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

    /// Load the rules for the box. Until this returns, the box is not confined.
    pub fn load(&mut self, rules: &[String]) -> Result<()> {
        let input = self.input.as_mut().ok_or_else(|| anyhow!("the firewall helper is gone"))?;
        writeln!(input, "{}", rules.len())?;
        for rule in rules {
            writeln!(input, "{rule}")?;
        }
        input.flush()?;
        let mut line = String::new();
        self.output.read_line(&mut line)?;
        if line.trim() != "loaded" {
            bail!("pf rejected the firewall rules for the box");
        }
        Ok(())
    }
}

impl Drop for Firewall {
    /// Closing the helper's input makes it remove the rules; wait for that.
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
