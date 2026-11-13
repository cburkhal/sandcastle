//! Requests from the box to sandcastle itself. One exists: "publish this
//! branch" (`POST /push`, the branch name as the body), sent by the Git
//! helper in the box when the agent runs `git push`.
//!
//! The relay only receives the request and hands the name on; what happens
//! then is decided on the host (`core::push`). Nothing from the box but
//! that one short name gets through.

use std::sync::Arc;

use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};

use super::http::{text, Body};
use super::log::Log;

/// Longest request body that is read; a branch name is far shorter.
const MAX_BODY: usize = 512;

pub struct Control {
    /// Decides on a push request: the status and the text for the agent.
    on_push: Box<dyn Fn(&str) -> (StatusCode, String) + Send + Sync>,
    log: Log,
}

impl Control {
    pub fn new(on_push: impl Fn(&str) -> (StatusCode, String) + Send + Sync + 'static, log: Log) -> Self {
        Control { on_push: Box::new(on_push), log }
    }

    /// Answer one request from the box.
    pub async fn handle(self: Arc<Self>, request: Request<Incoming>) -> Response<Body> {
        if request.method() != Method::POST || request.uri().path() != "/push" {
            self.log.refused(&format!("control: refused {} {}", request.method(), request.uri().path()));
            return text(StatusCode::NOT_FOUND, "sandcastle: unknown request");
        }
        let body = match Limited::new(request.into_body(), MAX_BODY).collect().await {
            Ok(body) => body.to_bytes(),
            Err(_) => {
                self.log.refused("control: refused a push request: body too long or broken");
                return text(StatusCode::BAD_REQUEST, "sandcastle: malformed request");
            }
        };
        let Ok(branch) = String::from_utf8(body.to_vec()) else {
            return text(StatusCode::BAD_REQUEST, "sandcastle: malformed request");
        };
        // Publishing fetches and pushes: seconds of blocking work.
        let this = self.clone();
        let decided = tokio::task::spawn_blocking(move || (this.on_push)(branch.trim())).await;
        match decided {
            Ok((status, message)) => text(status, &message),
            Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "sandcastle: internal error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;
    use crate::relay::http;

    async fn ask(request: String) -> (String, Vec<String>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let control = Arc::new(Control::new(
            move |branch| {
                record.lock().unwrap().push(branch.to_string());
                (StatusCode::ACCEPTED, "queued".to_string())
            },
            Log::discard(),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(http::serve(listener, None, Log::discard(), move |request| control.clone().handle(request)));
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let seen = seen.lock().unwrap().clone();
        (response, seen)
    }

    fn post(path: &str, body: &str) -> String {
        format!("POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
    }

    #[tokio::test]
    async fn a_push_request_hands_on_the_branch_name_only() {
        let (response, seen) = ask(post("/push", "fix-login\n")).await;
        assert!(response.starts_with("HTTP/1.1 202"), "{response}");
        assert!(response.ends_with("queued\n"), "{response}");
        assert_eq!(seen, ["fix-login"]);
    }

    #[tokio::test]
    async fn other_requests_and_long_bodies_are_refused() {
        let (response, seen) = ask(post("/other", "x")).await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        assert!(seen.is_empty());
        let (response, seen) = ask("GET /push HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n".to_string()).await;
        assert!(response.starts_with("HTTP/1.1 404"), "{response}");
        assert!(seen.is_empty());
        let (response, seen) = ask(post("/push", &"x".repeat(MAX_BODY + 1))).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        assert!(seen.is_empty());
    }
}
