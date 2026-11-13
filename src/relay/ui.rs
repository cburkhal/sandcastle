//! UI gateway: the only way from the browser to the agent's web UI.
//!
//! It listens on `127.0.0.1` and lets a request through only if all of these
//! hold:
//! - the `Host` header is exactly `127.0.0.1:<port>` (blocks DNS rebinding),
//! - an `Origin` header, if present, is this gateway (blocks other websites),
//! - the request carries the session cookie.
//!
//! The cookie is set by a one-time login link that `sandcastle up` opens.
//! Requests that pass get the agent's Basic Auth password added on the way to
//! the box; the browser never sees it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use hyper::body::Incoming;
use hyper::header::{HeaderValue, AUTHORIZATION, CACHE_CONTROL, COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE, UPGRADE};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use super::http::{base64, constant_time_eq, empty, streaming, strip_hop_by_hop, text, Body, BoxError};
use super::log::Log;

const LOGIN_PATH: &str = "/__sandcastle/login/";
const COOKIE_NAME: &str = "sandcastle_session";

pub struct UiGateway {
    /// `127.0.0.1:<port>`: the only accepted `Host`.
    host: String,
    /// `http://127.0.0.1:<port>`: the only accepted `Origin`.
    origin: String,
    /// The agent's UI in the box.
    target: SocketAddr,
    authorization: HeaderValue,
    /// One-time login token; `None` once it has been used.
    login_token: Mutex<Option<String>>,
    session: String,
    log: Log,
}

impl UiGateway {
    /// `login_token` and `session` must be long random strings.
    pub fn new(
        port: u16,
        target: SocketAddr,
        agent_user: &str,
        agent_password: &str,
        login_token: String,
        session: String,
        log: Log,
    ) -> Self {
        let basic = base64(format!("{agent_user}:{agent_password}").as_bytes());
        let mut authorization = HeaderValue::from_str(&format!("Basic {basic}")).expect("base64 is a valid header value");
        authorization.set_sensitive(true);
        UiGateway {
            host: format!("127.0.0.1:{port}"),
            origin: format!("http://127.0.0.1:{port}"),
            target,
            authorization,
            login_token: Mutex::new(Some(login_token)),
            session,
            log,
        }
    }

    /// The link that logs the browser in. It works once.
    pub fn login_url(&self) -> Option<String> {
        let token = self.login_token.lock().ok()?;
        token.as_ref().map(|token| format!("{}{LOGIN_PATH}{token}", self.origin))
    }

    /// A fresh one-time link; any unused earlier link stops working. For a
    /// browser that lost the session cookie.
    pub fn new_login_url(&self) -> anyhow::Result<String> {
        let token = super::random_token(32)?;
        let url = format!("{}{LOGIN_PATH}{token}", self.origin);
        *self.login_token.lock().map_err(|_| anyhow::anyhow!("the login state is broken"))? = Some(token);
        Ok(url)
    }

    pub async fn handle(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        if let Err(reason) = self.check_headers(&request) {
            self.log.refused(&format!("ui: refused {} {}: {reason}", request.method(), request.uri().path()));
            return text(StatusCode::FORBIDDEN, "sandcastle: request refused");
        }
        if let Some(token) = request.uri().path().strip_prefix(LOGIN_PATH) {
            return self.login(token);
        }
        if !self.has_session(&request) {
            return text(
                StatusCode::UNAUTHORIZED,
                "sandcastle: not logged in. Open the link that `sandcastle up` printed.",
            );
        }
        match self.forward(request).await {
            Ok(response) => response,
            Err(error) => {
                self.log.line(&format!("ui: forwarding to the box failed: {error}"));
                text(StatusCode::BAD_GATEWAY, "sandcastle: the agent in the box did not answer")
            }
        }
    }

    fn check_headers<B>(&self, request: &Request<B>) -> Result<(), &'static str> {
        let host = request.headers().get(HOST).and_then(|value| value.to_str().ok());
        if host != Some(self.host.as_str()) {
            return Err("unexpected Host header");
        }
        // Browsers attach Origin to cross-site requests and to all non-GET ones.
        if let Some(origin) = request.headers().get(ORIGIN) {
            if origin.to_str().ok() != Some(self.origin.as_str()) {
                return Err("foreign Origin header");
            }
        }
        Ok(())
    }

    fn login(&self, token: &str) -> Response<Body> {
        let mut expected = match self.login_token.lock() {
            Ok(guard) => guard,
            Err(_) => return text(StatusCode::INTERNAL_SERVER_ERROR, "sandcastle: internal error"),
        };
        let matches = expected.as_deref().is_some_and(|expected| constant_time_eq(expected, token));
        if !matches {
            self.log.refused("ui: refused login: wrong or used link");
            return text(StatusCode::FORBIDDEN, "sandcastle: this login link is wrong or was already used");
        }
        *expected = None;
        self.log.line("ui: browser logged in");

        let cookie = format!("{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Strict", self.session);
        let mut response = Response::new(empty());
        *response.status_mut() = StatusCode::SEE_OTHER;
        let headers = response.headers_mut();
        headers.insert(LOCATION, HeaderValue::from_static("/"));
        headers.insert(SET_COOKIE, HeaderValue::from_str(&cookie).expect("session is hex"));
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }

    fn has_session<B>(&self, request: &Request<B>) -> bool {
        cookies(request).any(|(name, value)| name == COOKIE_NAME && constant_time_eq(value, &self.session))
    }

    async fn forward(&self, mut request: Request<Incoming>) -> Result<Response<Body>, BoxError> {
        // A WebSocket (or other protocol switch) must be handed over on both sides.
        let client_upgrade = request.headers().contains_key(UPGRADE).then(|| hyper::upgrade::on(&mut request));

        let headers = request.headers_mut();
        strip_hop_by_hop(headers, client_upgrade.is_some());
        // The session cookie is the gateway's business; the agent never sees it.
        let other_cookies: Vec<String> = headers
            .get_all(COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(';'))
            .map(str::trim)
            .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some(COOKIE_NAME))
            .map(str::to_string)
            .collect();
        headers.remove(COOKIE);
        if !other_cookies.is_empty() {
            headers.insert(COOKIE, HeaderValue::from_str(&other_cookies.join("; "))?);
        }
        // Replaces anything the browser sent.
        headers.insert(AUTHORIZATION, self.authorization.clone());

        let stream = TcpStream::connect(self.target).await?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(async move {
            let _ = connection.with_upgrades().await;
        });
        let mut response = sender.send_request(request).await?;

        if response.status() == StatusCode::SWITCHING_PROTOCOLS {
            if let Some(client_upgrade) = client_upgrade {
                let box_upgrade = hyper::upgrade::on(&mut response);
                tokio::spawn(async move {
                    if let (Ok(client), Ok(agent)) = (client_upgrade.await, box_upgrade.await) {
                        let _ = tokio::io::copy_bidirectional(&mut TokioIo::new(client), &mut TokioIo::new(agent)).await;
                    }
                });
            }
            let (parts, _) = response.into_parts();
            return Ok(Response::from_parts(parts, empty()));
        }

        let (mut parts, body) = response.into_parts();
        strip_hop_by_hop(&mut parts.headers, false);
        // Streamed as it arrives: the UI relies on server-sent events.
        Ok(Response::from_parts(parts, streaming(body)))
    }
}

/// All `name=value` pairs from the request's Cookie headers.
fn cookies<B>(request: &Request<B>) -> impl Iterator<Item = (&str, &str)> {
    request
        .headers()
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::relay::http::{full, serve};

    const TOKEN: &str = "one-time-token";
    const SESSION: &str = "session-value";

    struct Setup {
        gateway: SocketAddr,
        ui: Arc<UiGateway>,
        /// Requests as the fake agent saw them.
        seen: Arc<StdMutex<Vec<String>>>,
    }

    async fn setup() -> Setup {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let agent_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let agent = agent_listener.local_addr().unwrap();
        let record = seen.clone();
        tokio::spawn(serve(agent_listener, None, Log::discard(), move |request: Request<Incoming>| {
            let record = record.clone();
            async move {
                let header = |name: &str| {
                    request.headers().get(name).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default()
                };
                let line = format!(
                    "{} {} auth={} cookie={} host={}",
                    request.method(),
                    request.uri(),
                    header("authorization"),
                    header("cookie"),
                    header("host")
                );
                let body = request.into_body().collect().await.unwrap().to_bytes();
                record.lock().unwrap().push(format!("{line} body={}", String::from_utf8_lossy(&body)));
                Response::new(full("agent says hi"))
            }
        }));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = listener.local_addr().unwrap();
        let ui = Arc::new(UiGateway::new(
            gateway.port(),
            agent,
            "opencode",
            "pw",
            TOKEN.into(),
            SESSION.into(),
            Log::discard(),
        ));
        let handler = ui.clone();
        tokio::spawn(serve(listener, None, Log::discard(), move |request| handler.clone().handle(request)));
        Setup { gateway, ui, seen }
    }

    async fn raw(address: SocketAddr, request: String) -> String {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    fn get(setup: &Setup, path: &str, extra: &str) -> String {
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{extra}Connection: close\r\n\r\n", setup.gateway.port())
    }

    #[tokio::test]
    async fn without_a_session_nothing_reaches_the_agent() {
        let s = setup().await;
        let response = raw(s.gateway, get(&s, "/api/session", "")).await;
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        let response = raw(s.gateway, get(&s, "/", "Cookie: sandcastle_session=guess\r\n")).await;
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
        assert!(s.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn login_link_works_exactly_once() {
        let s = setup().await;
        let url = s.ui.login_url().unwrap();
        assert!(url.ends_with(&format!("{LOGIN_PATH}{TOKEN}")));

        let wrong = raw(s.gateway, get(&s, &format!("{LOGIN_PATH}nope"), "")).await;
        assert!(wrong.starts_with("HTTP/1.1 403"), "{wrong}");

        let first = raw(s.gateway, get(&s, &format!("{LOGIN_PATH}{TOKEN}"), "")).await;
        assert!(first.starts_with("HTTP/1.1 303"), "{first}");
        assert!(first.contains(&format!("set-cookie: {COOKIE_NAME}={SESSION}; Path=/; HttpOnly; SameSite=Strict")));
        assert!(first.contains("location: /"));

        let second = raw(s.gateway, get(&s, &format!("{LOGIN_PATH}{TOKEN}"), "")).await;
        assert!(second.starts_with("HTTP/1.1 403"), "{second}");
        assert!(s.ui.login_url().is_none());
        assert!(s.seen.lock().unwrap().is_empty(), "the login path is never forwarded");
    }

    #[tokio::test]
    async fn with_a_session_the_request_is_forwarded_with_the_agent_password() {
        let s = setup().await;
        let request = format!(
            "POST /api/x?y=1 HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\n\
             Cookie: theme=dark; {COOKIE_NAME}={SESSION}\r\nAuthorization: Bearer from-browser\r\n\
             Content-Length: 4\r\nConnection: close\r\n\r\nbody",
            port = s.gateway.port()
        );
        let response = raw(s.gateway, request).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("agent says hi"));
        assert!(!response.contains("b3BlbmNvZGU6cHc="), "the agent password must not reach the browser");

        let seen = s.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("POST /api/x?y=1 "), "{}", seen[0]);
        assert!(seen[0].contains("auth=Basic b3BlbmNvZGU6cHc= "), "{}", seen[0]);
        assert!(seen[0].contains("cookie=theme=dark "), "session cookie must be stripped: {}", seen[0]);
        assert!(seen[0].ends_with("body=body"));
    }

    #[tokio::test]
    async fn foreign_host_and_origin_are_refused_even_with_a_session() {
        let s = setup().await;
        let cookie = format!("Cookie: {COOKIE_NAME}={SESSION}\r\n");
        let port = s.gateway.port();
        for request in [
            // DNS rebinding: the browser thinks it talks to attacker.example.
            format!("GET / HTTP/1.1\r\nHost: attacker.example:{port}\r\n{cookie}Connection: close\r\n\r\n"),
            format!("GET / HTTP/1.1\r\nHost: localhost:{port}\r\n{cookie}Connection: close\r\n\r\n"),
            format!("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n{cookie}Connection: close\r\n\r\n"),
            // Another website driving the agent from the same browser.
            format!("POST /api/x HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\n{cookie}Content-Length: 0\r\nConnection: close\r\n\r\n"),
            format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: null\r\n{cookie}Connection: close\r\n\r\n"),
            format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:1\r\n{cookie}Connection: close\r\n\r\n"),
            // The login link is protected the same way.
            format!("GET {LOGIN_PATH}{TOKEN} HTTP/1.1\r\nHost: attacker.example\r\nConnection: close\r\n\r\n"),
        ] {
            let response = raw(s.gateway, request.clone()).await;
            assert!(response.starts_with("HTTP/1.1 403"), "{request}\n{response}");
        }
        assert!(s.seen.lock().unwrap().is_empty());
        assert!(s.ui.login_url().is_some(), "a refused request must not consume the login link");
    }

    #[tokio::test]
    async fn protocol_upgrades_are_tunnelled() {
        // A fake agent that accepts an upgrade and then echoes bytes.
        let agent_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let agent = agent_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = agent_listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).await.unwrap();
                head.push(byte[0]);
            }
            assert!(String::from_utf8_lossy(&head).to_lowercase().contains("upgrade: websocket"));
            stream
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n")
                .await
                .unwrap();
            let mut buffer = [0u8; 4];
            stream.read_exact(&mut buffer).await.unwrap();
            stream.write_all(&buffer).await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = listener.local_addr().unwrap();
        let ui = Arc::new(UiGateway::new(gateway.port(), agent, "opencode", "pw", TOKEN.into(), SESSION.into(), Log::discard()));
        tokio::spawn(serve(listener, None, Log::discard(), move |request| ui.clone().handle(request)));

        let mut stream = TcpStream::connect(gateway).await.unwrap();
        let request = format!(
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nCookie: {COOKIE_NAME}={SESSION}\r\n\r\n",
            gateway.port()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));
        stream.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        stream.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
    }
}
