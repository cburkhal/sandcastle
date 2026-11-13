//! The host-side relay: the box's only exit and the browser's only entrance.
//! Security core; it stays one piece and is not a replaceable module.
//!
//! - [`control`]: the box's requests to sandcastle itself (publish a branch).
//! - [`egress`]: CONNECT proxy with a host allowlist.
//! - [`llm`]: plain HTTP in, real key swapped in, HTTPS out.
//! - [`ui`]: browser access to the agent's UI behind a one-time link, a
//!   cookie, and Host and Origin checks.

pub mod control;
pub mod egress;
pub mod http;
pub mod llm;
pub mod log;
pub mod ui;

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::net::TcpListener;

/// Bind a listener on the box network's gateway address. That address only
/// exists on the host once a box is running (spike item 2), so "cannot assign
/// requested address" is retried for a while.
pub async fn bind_on_gateway(address: SocketAddr, wait: Duration) -> Result<TcpListener> {
    let deadline = Instant::now() + wait;
    loop {
        match TcpListener::bind(address).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == ErrorKind::AddrNotAvailable && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(error) if error.kind() == ErrorKind::AddrNotAvailable => {
                bail!("the box network's address {} did not appear on this Mac", address.ip())
            }
            Err(error) => return Err(error).with_context(|| format!("could not listen on {address}")),
        }
    }
}

/// A random hex string of `bytes` bytes from the system's random source.
pub fn random_token(bytes: usize) -> Result<String> {
    use std::io::Read;
    let mut buffer = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(buffer.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_tokens_are_long_and_differ() {
        let (a, b) = (random_token(32).unwrap(), random_token(32).unwrap());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn binding_a_foreign_address_gives_up_with_a_clear_error() {
        // 192.0.2.0/24 is reserved for documentation; no interface has it.
        let error = bind_on_gateway("192.0.2.1:4180".parse().unwrap(), Duration::from_millis(300)).await.unwrap_err();
        assert!(error.to_string().contains("did not appear"), "{error}");
    }
}
