//! Small HTTP helpers shared by the LLM relay and the UI gateway.

use std::convert::Infallible;
use std::future::Future;
use std::net::IpAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderName, CONTENT_TYPE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use super::log::Log;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = UnsyncBoxBody<Bytes, BoxError>;

pub fn empty() -> Body {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed_unsync()
}

pub fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into()).map_err(|never| match never {}).boxed_unsync()
}

/// Pass a body through as it arrives, without buffering it.
pub fn streaming(body: Incoming) -> Body {
    body.map_err(|error| Box::new(error) as BoxError).boxed_unsync()
}

pub fn text(status: StatusCode, message: &str) -> Response<Body> {
    let mut response = Response::new(full(format!("{message}\n")));
    *response.status_mut() = status;
    response.headers_mut().insert(CONTENT_TYPE, "text/plain; charset=utf-8".parse().expect("static header"));
    response
}

/// Headers that describe one connection and must not be forwarded (RFC 9110).
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Remove hop-by-hop headers. `upgrade` goes too unless `keep_upgrade` is
/// set, which is the case when a protocol switch (WebSocket) is passed on.
pub fn strip_hop_by_hop(headers: &mut HeaderMap, keep_upgrade: bool) {
    for name in HOP_BY_HOP {
        if keep_upgrade && name == "connection" {
            continue;
        }
        headers.remove(name);
    }
    if !keep_upgrade {
        headers.remove(HeaderName::from_static("upgrade"));
    }
}

/// Accept HTTP/1.1 connections forever and answer each request with `handler`.
/// If `only_peer` is set, connections from any other address are dropped.
pub async fn serve<F, Fut>(listener: TcpListener, only_peer: Option<IpAddr>, log: Log, handler: F)
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<Body>> + Send + 'static,
{
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => {
                // For example "too many open files": wait instead of spinning.
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        if only_peer.is_some_and(|allowed| allowed != peer.ip()) {
            log.line(&format!("refused connection from {}: not the box", peer.ip()));
            continue;
        }
        let handler = handler.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let handler = handler.clone();
                async move { Ok::<_, Infallible>(handler(request).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

/// Compare two strings without stopping at the first difference, so the time
/// taken does not reveal how much of a token was right.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Standard base64 with padding; only needed for one Basic Auth header.
pub fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"opencode:pw"), "b3BlbmNvZGU6cHc=");
    }

    #[test]
    fn constant_time_eq_compares_whole_strings() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
    }

    #[test]
    fn hop_by_hop_headers_are_removed() {
        let mut headers = HeaderMap::new();
        for (name, value) in [("connection", "upgrade"), ("upgrade", "websocket"), ("te", "trailers"), ("accept", "*/*")] {
            headers.insert(HeaderName::from_static(name), value.parse().unwrap());
        }
        let mut kept = headers.clone();
        strip_hop_by_hop(&mut headers, false);
        assert_eq!(headers.len(), 1);
        strip_hop_by_hop(&mut kept, true);
        assert!(kept.contains_key("connection") && kept.contains_key("upgrade") && !kept.contains_key("te"));
    }
}
