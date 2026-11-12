//! LLM relay: the box talks plain HTTP with a dummy key; the relay swaps in
//! the real key and forwards over HTTPS.
//!
//! The key exists only in this process. It is attached to outgoing requests as
//! a header marked sensitive and is never written to the log or to a response.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::{HeaderValue, AUTHORIZATION, HOST};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

use super::http::{empty, streaming, strip_hop_by_hop, text, Body};
use super::log::Log;
use crate::secrets::Secret;

pub struct LlmRelay {
    /// Upstream base URL without a trailing slash, for example `https://host/api`.
    base: String,
    authorization: HeaderValue,
    client: Client<HttpsConnector<HttpConnector>, Body>,
    requests: AtomicU64,
    log: Log,
}

impl LlmRelay {
    pub fn new(base_url: &str, key: &Secret, log: Log) -> Result<Self> {
        let base = base_url.trim_end_matches('/').to_string();
        let uri: Uri = base.parse().with_context(|| format!("invalid LLM base URL {base}"))?;
        // Plain HTTP would send the key in the clear. It is only accepted for a
        // gateway on this Mac itself, where nothing leaves the machine.
        let local = matches!(uri.host(), Some("127.0.0.1" | "localhost" | "[::1]"));
        match uri.scheme_str() {
            Some("https") => {}
            Some("http") if local => {}
            _ => bail!("the LLM base URL must start with https:// (http:// only for 127.0.0.1)"),
        }
        if key.expose().is_empty() {
            bail!("the LLM key is empty");
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {}", key.expose()))
            .map_err(|_| anyhow!("the LLM key contains characters that are not allowed in a header"))?;
        authorization.set_sensitive(true);

        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .context("could not load the system's trusted certificates")?
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new()).build(connector);
        Ok(LlmRelay { base, authorization, client, requests: AtomicU64::new(0), log })
    }

    /// Number of requests forwarded so far.
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Answer one request from the box.
    pub async fn handle(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        let path = request.uri().path_and_query().map(|pq| pq.as_str().to_string()).unwrap_or_default();
        // The box only needs the OpenAI-compatible API; nothing else upstream is its business.
        if !path.starts_with("/v1/") {
            self.log.refused(&format!("llm: refused {} {path}: not under /v1/", request.method()));
            return text(StatusCode::NOT_FOUND, "sandcastle: only /v1/ is relayed");
        }
        let method = request.method().clone();
        let (mut parts, body) = request.into_parts();
        strip_hop_by_hop(&mut parts.headers, false);
        parts.headers.remove(HOST);
        // Whatever the box sent as a key is dropped, not forwarded.
        parts.headers.remove(AUTHORIZATION);

        let upstream = match self.upstream_request(&method, &path, parts.headers, streaming(body)) {
            Ok(upstream) => upstream,
            Err(_) => return text(StatusCode::BAD_REQUEST, "sandcastle: malformed request"),
        };
        self.requests.fetch_add(1, Ordering::Relaxed);
        match self.client.request(upstream).await {
            Ok(response) => {
                self.log.line(&format!("llm: {method} {path} -> {}", response.status().as_u16()));
                let (mut parts, body) = response.into_parts();
                strip_hop_by_hop(&mut parts.headers, false);
                // The body is passed on chunk by chunk, so streamed answers stay streamed.
                Response::from_parts(parts, streaming(body))
            }
            Err(error) => {
                // The error names the kind of failure (connect, TLS, ...), never a header.
                self.log.line(&format!("llm: {method} {path} failed: {error}"));
                text(StatusCode::BAD_GATEWAY, "sandcastle: the LLM gateway could not be reached")
            }
        }
    }

    fn upstream_request(
        &self,
        method: &Method,
        path: &str,
        headers: hyper::HeaderMap,
        body: Body,
    ) -> Result<Request<Body>> {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("{}{path}", self.base).parse::<Uri>()?)
            .body(body)?;
        *request.headers_mut() = headers;
        request.headers_mut().insert(AUTHORIZATION, self.authorization.clone());
        Ok(request)
    }

    /// Model ids the gateway offers for this key (`GET /v1/models`).
    pub async fn models(&self) -> Result<Vec<String>> {
        let request = self.upstream_request(&Method::GET, "/v1/models", hyper::HeaderMap::new(), empty())?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|error| anyhow!("could not reach the LLM gateway: {error}"))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            bail!("the LLM gateway rejected the key (HTTP {})", status.as_u16());
        }
        if !status.is_success() {
            bail!("the LLM gateway answered HTTP {} to /v1/models", status.as_u16());
        }
        let body = response.into_body().collect().await.map_err(|error| anyhow!("reading /v1/models: {error}"))?;
        let json: serde_json::Value =
            serde_json::from_slice(&body.to_bytes()).context("the LLM gateway's /v1/models answer is not JSON")?;
        let models: Vec<String> = json["data"]
            .as_array()
            .map(|list| list.iter().filter_map(|model| model["id"].as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if models.is_empty() {
            bail!("the LLM gateway offers no models for this key");
        }
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::relay::http::{full, serve};

    const KEY: &str = "sk-real-key-0123456789";

    /// A fake gateway that records the requests it gets.
    async fn fake_gateway(seen: Arc<Mutex<Vec<String>>>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, None, Log::discard(), move |request: Request<Incoming>| {
            let seen = seen.clone();
            async move {
                let auth = request.headers().get(AUTHORIZATION).map(|v| v.to_str().unwrap().to_string());
                let line = format!("{} {} auth={auth:?}", request.method(), request.uri());
                let body = request.into_body().collect().await.unwrap().to_bytes();
                seen.lock().unwrap().push(format!("{line} body={}", String::from_utf8_lossy(&body)));
                if line.contains("/v1/models") {
                    Response::new(full(r#"{"data":[{"id":"a/one"},{"id":"b/two"}]}"#))
                } else {
                    Response::new(full("data: pong\n\n"))
                }
            }
        }));
        address
    }

    async fn relay_for(base: String) -> (Arc<LlmRelay>, SocketAddr) {
        let relay = Arc::new(LlmRelay::new(&base, &Secret::new(KEY.into()), Log::discard()).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handler = relay.clone();
        tokio::spawn(serve(listener, None, Log::discard(), move |request| handler.clone().handle(request)));
        (relay, address)
    }

    async fn raw(address: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn swaps_the_key_and_keeps_path_prefix_and_body() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gateway = fake_gateway(seen.clone()).await;
        let (relay, address) = relay_for(format!("http://{gateway}/api/")).await;

        let response = raw(
            address,
            "POST /v1/chat/completions?x=1 HTTP/1.1\r\nHost: relay\r\nAuthorization: Bearer dummy\r\n\
             Content-Length: 2\r\nConnection: close\r\n\r\nhi",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("data: pong"));
        assert!(!response.contains(KEY));

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("POST /api/v1/chat/completions?x=1 "), "{}", seen[0]);
        assert!(seen[0].contains(&format!("Bearer {KEY}")), "real key must reach the gateway");
        assert!(!seen[0].contains("dummy"), "the box's key must not be forwarded");
        assert!(seen[0].ends_with("body=hi"));
        assert_eq!(relay.request_count(), 1);
    }

    #[tokio::test]
    async fn refuses_paths_outside_v1() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gateway = fake_gateway(seen.clone()).await;
        let (_, address) = relay_for(format!("http://{gateway}")).await;
        for path in ["/", "/key/info", "/v1", "/../v1/models"] {
            let response = raw(address, &format!("GET {path} HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n")).await;
            assert!(response.starts_with("HTTP/1.1 404"), "{path}: {response}");
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn gateway_failure_does_not_leak_the_key() {
        // Nothing listens on this port.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let (relay, address) = relay_for(format!("http://{dead}")).await;
        let response = raw(address, "GET /v1/models HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n").await;
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(!response.contains(KEY));
        let error = relay.models().await.unwrap_err();
        assert!(!format!("{error:#}").contains(KEY));
    }

    #[tokio::test]
    async fn lists_models() {
        let gateway = fake_gateway(Arc::new(Mutex::new(Vec::new()))).await;
        let (relay, _) = relay_for(format!("http://{gateway}")).await;
        assert_eq!(relay.models().await.unwrap(), ["a/one", "b/two"]);
    }

    #[test]
    fn rejects_bad_configuration() {
        let key = Secret::new(KEY.into());
        assert!(LlmRelay::new("ftp://host", &key, Log::discard()).is_err());
        assert!(LlmRelay::new("http://gateway.example", &key, Log::discard()).is_err());
        assert!(LlmRelay::new("http://127.0.0.1:4000", &key, Log::discard()).is_ok());
        assert!(LlmRelay::new("host/api", &key, Log::discard()).is_err());
        assert!(LlmRelay::new("https://host", &Secret::new(String::new()), Log::discard()).is_err());
        assert!(LlmRelay::new("https://host", &Secret::new("line\nbreak".into()), Log::discard()).is_err());
    }
}
