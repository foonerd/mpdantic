//! The pages and the API, served from the core's memory on a port of the player, for
//! a browser anywhere on the network: an HTTP/1.0 server on the standard library,
//! one thread per connection with a cap on how many at once, every response closed
//! when written. The page is one file built into the binary; it asks the API for the
//! status and for one directory's listing at a time.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;

use crate::library::{self, Library, ListError};
use crate::status::Status;

/// The page, whole.
const PAGE: &str = include_str!("../assets/index.html");
/// Connections answered at once; the rest wait in the listen queue, and past that are
/// told to come back.
const AT_ONCE: usize = 8;
/// A request head longer than this is dropped.
const MAX_HEAD: usize = 16384;

/// What the routes read.
pub struct Shared {
    pub status: Arc<Mutex<Status>>,
    pub library: Library,
}

/// The server, alive while it is held.
pub struct Server {
    pub port: u16,
    _listener: TcpListener,
    _acceptor: JoinHandle<()>,
}

/// The listener on every interface at `port` (0 for any free one, as the tests
/// ask), and the thread that answers.
pub fn start(port: u16, shared: Arc<Shared>) -> Result<Server, String> {
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("port {port}: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let acceptor = listener.try_clone().map_err(|e| e.to_string())?;
    let open = Arc::new(AtomicUsize::new(0));
    let thread = std::thread::Builder::new()
        .name("serve".into())
        .spawn(move || {
            for stream in acceptor.incoming() {
                // An accept that fails is one connection lost, not the server.
                let Ok(mut stream) = stream else { continue };
                if open.load(Ordering::Relaxed) >= AT_ONCE {
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                    let _ = stream.write_all(
                        b"HTTP/1.0 503 Service Unavailable\r\nRetry-After: 1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                    );
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                open.fetch_add(1, Ordering::Relaxed);
                let counter = open.clone();
                let shared = shared.clone();
                let spawned = std::thread::Builder::new()
                    .name("request".into())
                    .spawn(move || {
                        serve(stream, &shared);
                        counter.fetch_sub(1, Ordering::Relaxed);
                    });
                if spawned.is_err() {
                    open.fetch_sub(1, Ordering::Relaxed);
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Server {
        port,
        _listener: listener,
        _acceptor: thread,
    })
}

/// One request answered and the connection closed.
fn serve(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let Some(head) = read_head(&mut stream) else {
        return;
    };
    let request_line = head.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let head_only = method == "HEAD";
    if method != "GET" && !head_only {
        respond(
            &mut stream,
            405,
            "application/json",
            &error("method not allowed"),
            head_only,
        );
        return;
    }
    match path {
        "/" | "/index.html" => {
            respond(
                &mut stream,
                200,
                "text/html; charset=utf-8",
                PAGE,
                head_only,
            );
        }
        "/api/status" => {
            let body = shared
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .json();
            respond(&mut stream, 200, "application/json", &body, head_only);
        }
        "/api/list" => {
            let (code, body) = list(&shared.library, query);
            respond(&mut stream, code, "application/json", &body, head_only);
        }
        _ => respond(
            &mut stream,
            404,
            "application/json",
            &error("not found"),
            head_only,
        ),
    }
}

/// The request head, up to the blank line; none when the client sends nothing
/// usable.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return Some(String::from_utf8_lossy(&buf[..i]).into_owned());
        }
        if let Some(i) = buf.windows(2).position(|w| w == b"\n\n") {
            return Some(String::from_utf8_lossy(&buf[..i]).into_owned());
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
    }
}

/// `/api/list?path=P&offset=N&limit=N`: one page of one directory.
fn list(library: &Library, query: &str) -> (u16, String) {
    let mut path = String::new();
    let mut offset = 0usize;
    let mut limit = library::LIMIT_DEFAULT;
    for (key, value) in pairs(query) {
        match key.as_str() {
            "path" => path = value,
            "offset" => offset = value.parse().unwrap_or(0),
            "limit" => limit = value.parse().unwrap_or(library::LIMIT_DEFAULT),
            _ => {}
        }
    }
    match library.list(&path, offset, limit) {
        Ok(listing) => (200, json(&listing)),
        Err(ListError::BadPath) => (400, error("not a library path")),
        Err(ListError::NotFound) => (404, error("no such folder")),
        Err(ListError::NotADirectory) => (400, error("not a folder")),
        Err(ListError::Io(e)) => (500, error(&e)),
    }
}

/// The query's pairs, percent-decoded.
fn pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

fn error(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string() + "\n"
}

fn respond(stream: &mut TcpStream, code: u16, content_type: &str, body: &str, head_only: bool) {
    let head = format!(
        "HTTP/1.0 {code} {}\r\nServer: mpdantic\r\nContent-Type: {content_type}\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        reason(code),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    if !head_only {
        let _ = stream.write_all(body.as_bytes());
    }
    let _ = stream.shutdown(Shutdown::Both);
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::temp_library;
    use std::path::PathBuf;

    fn server(root: PathBuf) -> Server {
        let shared = Arc::new(Shared {
            status: Arc::new(Mutex::new(Status::new())),
            library: Library::new(root),
        });
        start(0, shared).unwrap()
    }

    fn talk(port: u16, request: &str) -> String {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        out
    }

    fn body(response: &str) -> &str {
        response.split("\r\n\r\n").nth(1).unwrap_or("")
    }

    #[test]
    fn the_page_is_served_at_the_root_and_a_head_has_no_body() {
        let root = temp_library("serve-page");
        let server = server(root.clone());
        let out = talk(server.port, "GET / HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 200 OK"), "{out}");
        assert!(out.contains("Content-Type: text/html; charset=utf-8"));
        assert!(body(&out).contains("<title>MPDantic</title>"));
        let out = talk(server.port, "HEAD / HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 200 OK"), "{out}");
        assert_eq!(body(&out), "");
        assert!(out.contains(&format!("Content-Length: {}", PAGE.len())));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_status_is_served_as_json() {
        let root = temp_library("serve-status");
        let server = server(root.clone());
        let out = talk(server.port, "GET /api/status HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 200 OK"), "{out}");
        let value: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(value["state"], "idle");
        assert_eq!(value["version"], crate::VERSION);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_listing_is_served_one_folder_at_a_time_in_pages() {
        let root = temp_library("serve-list");
        let server = server(root.clone());
        let out = talk(server.port, "GET /api/list HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 200 OK"), "{out}");
        let roots: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(roots["path"], "");
        assert_eq!(roots["entries"][0]["name"], "NAS");
        assert_eq!(roots["entries"][1]["name"], "USB");
        let out = talk(
            server.port,
            "GET /api/list?path=NAS%2FJazz&offset=1&limit=2 HTTP/1.0\r\n\r\n",
        );
        let page: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(page["path"], "NAS/Jazz");
        assert_eq!(page["entries"][0]["name"], ".mpdignore");
        assert_eq!(page["entries"][1]["name"], "A.flac");
        assert_eq!(page["entries"][1]["size"], 1024);
        assert_eq!(page["total"], 4);
        assert_eq!(page["offset"], 1);
        assert_eq!(page["more"], true);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_bad_path_a_missing_folder_and_a_file_are_refused() {
        let root = temp_library("serve-errors");
        let server = server(root.clone());
        let out = talk(server.port, "GET /api/list?path=..%2Fx HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 400"), "{out}");
        assert!(body(&out).contains("not a library path"));
        let out = talk(server.port, "GET /api/list?path=NAS/Rock HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 404"), "{out}");
        let out = talk(
            server.port,
            "GET /api/list?path=NAS/Jazz/A.flac HTTP/1.0\r\n\r\n",
        );
        assert!(out.starts_with("HTTP/1.0 400"), "{out}");
        assert!(body(&out).contains("not a folder"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn an_unknown_route_is_not_found_and_a_post_is_not_allowed() {
        let root = temp_library("serve-routes");
        let server = server(root.clone());
        let out = talk(server.port, "GET /nothing HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 404"), "{out}");
        let out = talk(
            server.port,
            "POST /api/list HTTP/1.0\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(out.starts_with("HTTP/1.0 405"), "{out}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_query_is_percent_decoded() {
        assert_eq!(
            pairs("path=NAS%2FJazz%20Live&offset=3&flag"),
            vec![
                ("path".to_string(), "NAS/Jazz Live".to_string()),
                ("offset".to_string(), "3".to_string()),
                ("flag".to_string(), String::new()),
            ]
        );
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }
}
