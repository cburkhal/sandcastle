//! Egress proxy: the box's only way to reach anything but the LLM.
//!
//! It understands exactly one thing: `CONNECT host:443` to a host on the
//! allowlist. Everything else is refused and logged. A CONNECT tunnel is
//! opaque, so the proxy controls *where* the box can talk to, not *what* it
//! says there (accepted risk, CLAUDE.md section 3).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use super::log::Log;

const MAX_HEAD: usize = 8 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Egress {
    /// Lowercase host names the box may reach on port 443.
    allow: Vec<String>,
    /// Connections from any other address are dropped.
    only_peer: Option<IpAddr>,
    /// The only port a tunnel may go to. Always 443, except in tests.
    port: u16,
    /// Tests tunnel to a local server; in real use private targets are refused.
    allow_private_targets: bool,
    log: Log,
}

/// What the client asked for, parsed from the request head.
#[derive(Debug, PartialEq)]
enum Ask {
    Connect { host: String, port: u16 },
    /// Any other method, with the target for the log.
    Other { method: String, target: String },
}

impl Egress {
    pub fn new(allow: &[String], only_peer: Option<IpAddr>, log: Log) -> Self {
        Egress {
            allow: allow.iter().map(|host| host.to_ascii_lowercase()).collect(),
            only_peer,
            port: 443,
            allow_private_targets: false,
            log,
        }
    }

    /// Exact host match, port 443 only. No wildcards and no suffix matching:
    /// `evil-github.com` and `github.com.evil.example` must not pass.
    fn allowed(&self, host: &str, port: u16) -> bool {
        let host = host.to_ascii_lowercase();
        port == self.port && self.allow.iter().any(|allowed| *allowed == host)
    }

    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            if self.only_peer.is_some_and(|allowed| allowed != peer.ip()) {
                self.log.line(&format!("egress: refused connection from {}: not the box", peer.ip()));
                continue;
            }
            let this = self.clone();
            tokio::spawn(async move { this.handle(stream).await });
        }
    }

    async fn handle(&self, mut client: TcpStream) {
        let Some((head, rest)) = read_head(&mut client).await else {
            return;
        };
        let (host, port) = match parse(&head) {
            Some(Ask::Connect { host, port }) => (host, port),
            Some(Ask::Other { method, target }) => {
                self.log.refused(&format!("egress: refused {method} {target}: only CONNECT is supported"));
                reply(&mut client, "405 Method Not Allowed", "sandcastle: only CONNECT is supported").await;
                return;
            }
            None => {
                reply(&mut client, "400 Bad Request", "sandcastle: malformed request").await;
                return;
            }
        };
        if !self.allowed(&host, port) {
            self.log.refused(&format!("egress: refused {host}:{port}: not on the allowlist"));
            reply(&mut client, "403 Forbidden", "sandcastle: host is not on the egress allowlist").await;
            return;
        }
        let mut upstream = match self.connect(&host, port).await {
            Ok(stream) => stream,
            Err(reason) => {
                self.log.line(&format!("egress: {host}:{port} failed: {reason}"));
                reply(&mut client, "502 Bad Gateway", "sandcastle: could not reach the host").await;
                return;
            }
        };
        self.log.line(&format!("egress: allowed {host}:{port}"));
        if client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").await.is_err() {
            return;
        }
        // Bytes the client sent right after the request head belong to the tunnel.
        if !rest.is_empty() && upstream.write_all(&rest).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    }

    /// Resolve on the host (the box has no DNS) and connect.
    async fn connect(&self, host: &str, port: u16) -> Result<TcpStream, String> {
        let addresses: Vec<SocketAddr> = timeout(IO_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| "name lookup timed out".to_string())?
            .map_err(|_| "name lookup failed".to_string())?
            .collect();
        for address in addresses {
            // An allowlisted name must not become a way into the host or the LAN.
            if !self.allow_private_targets && !is_public(address.ip()) {
                continue;
            }
            if let Ok(Ok(stream)) = timeout(IO_TIMEOUT, TcpStream::connect(address)).await {
                return Ok(stream);
            }
        }
        Err("no reachable public address".to_string())
    }
}

/// Read up to the end of the request head. Returns the head and any bytes
/// that followed it.
async fn read_head(client: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = timeout(IO_TIMEOUT, client.read(&mut chunk)).await.ok()?.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Some((String::from_utf8(buffer).ok()?, rest));
        }
        if buffer.len() > MAX_HEAD {
            return None;
        }
    }
}

fn parse(head: &str) -> Option<Ask> {
    let mut parts = head.lines().next()?.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return None;
    }
    if method != "CONNECT" {
        return Some(Ask::Other { method: method.to_string(), target: target.to_string() });
    }
    let (host, port) = target.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    Some(Ask::Connect { host: host.to_string(), port: port.parse().ok()? })
}

async fn reply(client: &mut TcpStream, status: &str, message: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{message}\n",
        message.len() + 1
    );
    let _ = client.write_all(response.as_bytes()).await;
    let _ = client.shutdown().await;
}

/// Whether an address is on the public internet (not loopback, private,
/// link-local, shared, multicast or unspecified).
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn egress(allow: &[&str]) -> Egress {
        let allow: Vec<String> = allow.iter().map(|h| h.to_string()).collect();
        Egress::new(&allow, None, Log::discard())
    }

    #[test]
    fn allowlist_is_exact_and_443_only() {
        let egress = egress(&["github.com", "Registry.NPMJS.org"]);
        assert!(egress.allowed("github.com", 443));
        assert!(egress.allowed("GitHub.com", 443));
        assert!(egress.allowed("registry.npmjs.org", 443));
        for (host, port) in [
            ("github.com", 80),
            ("github.com", 22),
            ("api.github.com", 443),
            ("evil-github.com", 443),
            ("github.com.evil.example", 443),
            ("github.com.", 443),
            ("", 443),
            ("140.82.121.4", 443),
        ] {
            assert!(!egress.allowed(host, port), "{host}:{port} must be refused");
        }
    }

    #[test]
    fn request_lines_are_parsed_strictly() {
        assert_eq!(
            parse("CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n"),
            Some(Ask::Connect { host: "github.com".into(), port: 443 })
        );
        assert_eq!(
            parse("GET http://example.com/ HTTP/1.1\r\n\r\n"),
            Some(Ask::Other { method: "GET".into(), target: "http://example.com/".into() })
        );
        for bad in ["", "CONNECT\r\n\r\n", "CONNECT github.com HTTP/1.1\r\n\r\n", "CONNECT :443 HTTP/1.1\r\n\r\n",
            "CONNECT github.com:https HTTP/1.1\r\n\r\n", "CONNECT a:443 b HTTP/1.1\r\n\r\n", "CONNECT a:443 SPDY/3\r\n\r\n"]
        {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn private_and_local_addresses_are_not_public() {
        for address in ["127.0.0.1", "10.1.2.3", "192.168.128.1", "172.16.0.1", "169.254.1.1", "100.64.0.1", "0.0.0.0",
            "::1", "fe80::1", "fd00::1", "::ffff:192.168.1.1", "::"]
        {
            assert!(!is_public(address.parse().unwrap()), "{address}");
        }
        for address in ["1.1.1.1", "140.82.121.4", "2606:4700:4700::1111"] {
            assert!(is_public(address.parse().unwrap()), "{address}");
        }
    }

    async fn exchange(proxy: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(proxy).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = vec![0u8; 512];
        let read = timeout(Duration::from_secs(5), stream.read(&mut response)).await.unwrap().unwrap();
        String::from_utf8_lossy(&response[..read]).into_owned()
    }

    async fn start(mut egress: Egress) -> SocketAddr {
        egress.allow_private_targets = true;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(Arc::new(egress).serve(listener));
        address
    }

    #[tokio::test]
    async fn refuses_hosts_off_the_list_and_other_methods() {
        let proxy = start(egress(&["github.com"])).await;
        assert!(exchange(proxy, "CONNECT example.com:443 HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 403"));
        assert!(exchange(proxy, "CONNECT github.com:22 HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 403"));
        assert!(exchange(proxy, "GET http://github.com/ HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 405"));
        assert!(exchange(proxy, "nonsense\r\n\r\n").await.starts_with("HTTP/1.1 400"));
    }

    #[tokio::test]
    async fn refuses_an_allowlisted_name_that_resolves_to_a_private_address() {
        // Without the test override, `localhost` on the list must still not open a tunnel into the host.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = listener.local_addr().unwrap();
        tokio::spawn(Arc::new(egress(&["localhost"])).serve(listener));
        assert!(exchange(proxy, "CONNECT localhost:443 HTTP/1.1\r\n\r\n").await.starts_with("HTTP/1.1 502"));
    }

    #[tokio::test]
    async fn tunnels_to_an_allowed_host() {
        // An echo server stands in for the allowed host.
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut buffer = [0u8; 4];
            stream.read_exact(&mut buffer).await.unwrap();
            stream.write_all(&buffer).await.unwrap();
        });
        let mut egress = egress(&["localhost"]);
        egress.port = echo_port;
        let proxy = start(egress).await;

        let mut stream = TcpStream::connect(proxy).await.unwrap();
        stream.write_all(format!("CONNECT localhost:{echo_port} HTTP/1.1\r\n\r\n").as_bytes()).await.unwrap();
        let mut reply = [0u8; 39];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply[..], b"HTTP/1.1 200 Connection established\r\n\r\n");
        stream.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
    }

    #[tokio::test]
    async fn drops_connections_from_other_peers() {
        let allow = vec!["github.com".to_string()];
        let egress = Egress::new(&allow, Some("192.0.2.7".parse().unwrap()), Log::discard());
        let proxy = start(egress).await;
        let mut stream = TcpStream::connect(proxy).await.unwrap();
        let _ = stream.write_all(b"CONNECT github.com:443 HTTP/1.1\r\n\r\n").await;
        let mut buffer = [0u8; 16];
        // Closed without a single byte of response.
        let read = timeout(Duration::from_secs(5), stream.read(&mut buffer)).await.unwrap().unwrap_or(0);
        assert_eq!(read, 0);
    }
}
