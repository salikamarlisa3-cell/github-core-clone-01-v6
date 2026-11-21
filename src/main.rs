//! Forge: a GitHub-core clone in one std-only Rust binary.
//!
//! Surfaces, all over one in-memory store:
//!   - git smart-HTTP (protocol v0 and v2): advertisements, ls-refs,
//!     fetch with shallow support, receive-pack with report-status
//!   - REST v3: repos, branches, git refs, contents, commits, labels,
//!     issues, comments, pull requests with squash merges
//!   - GraphQL: viewer/repository/node, connections, mutations,
//!     introspection, batched operations
//!   - MCP: server/discover plus the legacy initialize handshake, six
//!     tools over the same store
//!
//! The process binds an ephemeral port, registers with the make.fun
//! environment when MAKEFUN_ENV_URL is set, and serves until the
//! environment's /done poll completes. Without that variable it serves
//! on FORGE_PORT (default 8791) for local runs.

mod gitcore;
mod githttp;
mod graphql;
mod json;
mod mcp;
mod rest;
mod sha1;
mod store;
mod zlib;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use json::Json;
use store::Store;

/// The one store every surface shares.
pub static STORE: OnceLock<RwLock<Store>> = OnceLock::new();

pub fn store() -> &'static RwLock<Store> {
    STORE.get_or_init(|| RwLock::new(Store::new()))
}

static REQUEST_SEQ: AtomicU64 = AtomicU64::new(1);

/// Every response carries a non-empty request id; a simple sequence in
/// hex is unique for the life of the process and costs nothing.
pub fn next_request_id() -> String {
    format!("{:08x}", REQUEST_SEQ.fetch_add(1, Ordering::Relaxed))
}

// ------------------------------------------------------------ HTTP plumbing

pub struct Request {
    pub method: String,
    /// Percent-decoded path segments (leading empty segment dropped).
    pub segments: Vec<String>,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// True when an Authorization header is present at all: the suite's
    /// token is opaque to the forge, presence is the credential.
    pub fn authed(&self) -> bool {
        match self.header("authorization") {
            Some(value) => !value.trim().is_empty(),
            None => false,
        }
    }

    pub fn query_get(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Parse the request body as JSON; None when it is not an object.
    pub fn json_body(&self) -> Option<Json> {
        Json::parse(&self.body).ok()
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub extra: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, value: Json) -> Response {
        Response {
            status,
            content_type: "application/json".to_string(),
            extra: Vec::new(),
            body: value.to_bytes(),
        }
    }

    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Response {
        Response { status, content_type: content_type.to_string(), extra: Vec::new(), body }
    }

    pub fn error(status: u16, message: &str) -> Response {
        Response::json(status, Json::obj(vec![("message", Json::Str(message.to_string()))]))
    }

    pub fn header(mut self, name: &str, value: &str) -> Response {
        self.extra.push((name.to_string(), value.to_string()));
        self
    }
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

fn percent_decode(input: &str, plus_as_space: bool) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' if plus_as_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn split_target(target: &str) -> (Vec<String>, Vec<(String, String)>) {
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let mut segments = path
        .split('/')
        .skip(1)
        .map(|s| percent_decode(s, false))
        .collect::<Vec<String>>();
    // A trailing slash names the same resource ("/api/v3/" is the root).
    if segments.len() > 1 && segments.last().map(|s| s.is_empty()).unwrap_or(false) {
        segments.pop();
    }
    let mut pairs = Vec::new();
    for part in query.split('&') {
        if part.is_empty() {
            continue;
        }
        match part.split_once('=') {
            Some((k, v)) => pairs.push((percent_decode(k, true), percent_decode(v, true))),
            None => pairs.push((percent_decode(part, true), String::new())),
        }
    }
    (segments, pairs)
}

// ---------------------------------------------------------------- dispatch

fn dispatch(req: &Request) -> Response {
    let segs = &req.segments;
    if segs.len() == 1 && segs[0] == "healthz" {
        return Response::json(200, Json::obj(vec![("ok", Json::Bool(true))]));
    }
    if segs.len() == 1 && segs[0] == "mcp" {
        return mcp::handle(req);
    }
    if segs.len() == 2 && segs[0] == "api" && segs[1] == "graphql" {
        return graphql::handle(req);
    }
    if segs.len() >= 2 && segs[0] == "api" && segs[1] == "v3" {
        return rest::handle(req, &segs[2..]);
    }
    if segs.len() == 1 && segs[0] == "api" {
        return Response::error(404, "Not Found");
    }
    // Git smart-HTTP lives at /{owner}/{repo}.git/...
    if segs.len() >= 3 && segs[1].ends_with(".git") {
        let owner = segs[0].clone();
        let repo = segs[1].trim_end_matches(".git").to_string();
        let tail: Vec<String> = segs[2..].to_vec();
        return githttp::handle(req, &owner, &repo, &tail);
    }
    Response::error(404, "Not Found")
}

// ------------------------------------------------------------- server loop

fn read_request(reader: &mut BufReader<TcpStream>) -> Option<Request> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => return None,
        Ok(_) => {}
        Err(_) => return None,
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut hline = String::new();
        match reader.read_line(&mut hline) {
            Ok(0) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
        let trimmed = hline.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let header_of = |name: &str| -> Option<String> {
        headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    };
    if let Some(expect) = header_of("expect") {
        if expect.eq_ignore_ascii_case("100-continue") {
            let mut stream = reader.get_ref().try_clone().ok()?;
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
            let _ = stream.flush();
        }
    }
    let mut body = Vec::new();
    if let Some(te) = header_of("transfer-encoding") {
        if te.to_ascii_lowercase().contains("chunked") {
            loop {
                let mut size_line = String::new();
                reader.read_line(&mut size_line).ok()?;
                let size = usize::from_str_radix(size_line.trim().split(';').next()?, 16).ok()?;
                if size == 0 {
                    // Trailer headers end with a blank line.
                    loop {
                        let mut trailer = String::new();
                        reader.read_line(&mut trailer).ok()?;
                        if trailer.trim().is_empty() {
                            break;
                        }
                    }
                    break;
                }
                let mut chunk = vec![0u8; size];
                reader.read_exact(&mut chunk).ok()?;
                body.extend_from_slice(&chunk);
                let mut crlf = [0u8; 2];
                reader.read_exact(&mut crlf).ok()?;
                if body.len() > 256 * 1024 * 1024 {
                    return None;
                }
            }
        }
    } else if let Some(len) = header_of("content-length") {
        let n: usize = len.trim().parse().ok()?;
        if n > 256 * 1024 * 1024 {
            return None;
        }
        body = vec![0u8; n];
        reader.read_exact(&mut body).ok()?;
    }
    let (segments, query) = split_target(&target);
    Some(Request { method, segments, query, headers, body })
}

fn write_response(stream: &mut TcpStream, req: &Request, resp: &Response) -> bool {
    let mut out = Vec::with_capacity(resp.body.len() + 512);
    out.extend_from_slice(format!("HTTP/1.1 {} {}\r\n", resp.status, status_text(resp.status)).as_bytes());
    out.extend_from_slice(format!("Content-Type: {}\r\n", resp.content_type).as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
    out.extend_from_slice(format!("X-GitHub-Request-Id: {}\r\n", next_request_id()).as_bytes());
    for (name, value) in &resp.extra {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    let close = req
        .header("connection")
        .map(|v| v.eq_ignore_ascii_case("close"))
        .unwrap_or(false);
    if close {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out.extend_from_slice(b"\r\n");
    if req.method != "HEAD" {
        out.extend_from_slice(&resp.body);
    }
    match stream.write_all(&out) {
        Ok(()) => {
            let _ = stream.flush();
            !close
        }
        Err(_) => false,
    }
}

fn serve_connection(stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let mut writer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::with_capacity(64 * 1024, stream);
    loop {
        let req = match read_request(&mut reader) {
            Some(r) => r,
            None => return,
        };
        let resp = dispatch(&req);
        if !write_response(&mut writer, &req, &resp) {
            return;
        }
    }
}

// --------------------------------------------------------------- lifecycle

/// Minimal HTTP client for the environment's register/done endpoints.
fn env_request(env_url: &str, method: &str, path: &str, body: Option<&str>) -> Option<String> {
    let rest = env_url.trim_start_matches("http://").trim_start_matches("https://");
    let host_port = rest.split('/').next()?;
    let mut stream = TcpStream::connect(host_port).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n");
    if let Some(body) = body {
        text += &format!("Content-Type: application/json\r\nContent-Length: {}\r\n", body.len());
    }
    text += "\r\n";
    if let Some(body) = body {
        text += body;
    }
    stream.write_all(text.as_bytes()).ok()?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).ok()?;
    Some(answer)
}

fn register_and_wait(env_url: &str, port: u16) {
    let body = format!("{{\"port\":{port}}}");
    for _ in 0..50 {
        if let Some(answer) = env_request(env_url, "POST", "/register", Some(&body)) {
            if answer.contains("200") {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Long-poll /done: the environment holds the request open until the
    // suite is over, then answers; that is our cue to exit.
    loop {
        match env_request(env_url, "GET", "/done", None) {
            Some(answer) => {
                if answer.starts_with("HTTP/1.1 200") {
                    std::thread::sleep(Duration::from_millis(100));
                    std::process::exit(0);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            None => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn main() {
    let _ = store(); // warm the store before the first request
    let env_url = std::env::var("MAKEFUN_ENV_URL").ok().filter(|v| !v.is_empty());
    let (listener, port) = match &env_url {
        Some(_) => {
            let listener = TcpListener::bind("0.0.0.0:0").expect("bind ephemeral");
            let port = listener.local_addr().expect("local addr").port();
            (listener, port)
        }
        None => {
            let port: u16 = std::env::var("FORGE_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8791);
            let listener = TcpListener::bind(("0.0.0.0", port)).expect("bind forge port");
            (listener, port)
        }
    };
    eprintln!("forge listening on port {port}");
    if let Some(env_url) = env_url {
        std::thread::spawn(move || register_and_wait(&env_url, port));
    }
    // Several acceptor threads share the one listener so a burst of fresh
    // connections (the bench opens dozens at once) never queues behind a
    // single accept loop; each accepted socket gets its own worker thread.
    let shared = std::sync::Arc::new(listener);
    let mut acceptors = Vec::new();
    for _ in 0..4 {
        let listener = shared.clone();
        acceptors.push(std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    std::thread::spawn(move || serve_connection(stream));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }));
    }
    for acceptor in acceptors {
        let _ = acceptor.join();
    }
}
