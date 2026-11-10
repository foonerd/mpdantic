//! The pages and the API, served from the core's memory on a port of the player, for
//! a browser anywhere on the network: an HTTP/1.0 server on the standard library,
//! one thread per connection with a cap on how many at once, every response closed
//! when written. The page is one file built into the binary; it asks the API for the
//! status, for one directory's listing at a time, and for the jobs it starts.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;

use crate::analyse::{self, DecoderNote};
use crate::cuecheck;
use crate::jobs::Jobs;
use crate::library::{self, Library, ListError};
use crate::mount;
use crate::mpd::{self, Address, Decoders};
use crate::prefs;
use crate::status::Status;
use crate::tagcheck;
use crate::undo;
use crate::write;

/// The page, whole.
const PAGE: &str = include_str!("../assets/index.html");
/// Connections answered at once; the rest wait in the listen queue, and past that are
/// told to come back.
const AT_ONCE: usize = 8;
/// A request head longer than this is dropped; a body longer than this refused.
const MAX_HEAD: usize = 16384;
const MAX_BODY: usize = 65536;

/// What the routes read and start.
pub struct Shared {
    pub status: Arc<Mutex<Status>>,
    pub library: Library,
    pub jobs: Arc<Jobs>,
    pub mpd: Address,
    /// The decoders last read, with where they came from.
    pub decoders: Mutex<Option<(DecoderNote, Decoders)>>,
    /// What the owner knows about the library.
    pub prefs: Mutex<prefs::Store>,
    /// Volumio's shares, read when a write needs them.
    pub networkfs: std::path::PathBuf,
    pub mounter: mount::Mounter,
    pub journal: undo::Journal,
    /// The previews prepared and not yet applied, by id.
    pub previews: Mutex<std::collections::HashMap<u64, write::Preview>>,
    pub next_preview: std::sync::atomic::AtomicU64,
}

impl Shared {
    /// The running MPD's decoders, or the assumed set when it cannot be asked; kept
    /// for the page.
    pub fn decoders(&self) -> (DecoderNote, Decoders) {
        let (note, decoders) = match mpd::decoders(&self.mpd) {
            Ok(d) => (
                DecoderNote {
                    source: "mpd".to_string(),
                    count: d.suffixes.len(),
                    detail: format!(
                        "MPD protocol {}, {} plugins, {} suffixes only ffmpeg claims",
                        d.protocol,
                        d.plugins.len(),
                        d.ffmpeg_only.len()
                    ),
                },
                d,
            ),
            Err(e) => {
                let d = mpd::assumed();
                (
                    DecoderNote {
                        source: "assumed".to_string(),
                        count: d.suffixes.len(),
                        detail: format!("MPD could not be asked: {e}"),
                    },
                    d,
                )
            }
        };
        let kept = (
            DecoderNote {
                source: note.source.clone(),
                count: note.count,
                detail: note.detail.clone(),
            },
            decoders.clone(),
        );
        *self.decoders.lock().unwrap_or_else(|e| e.into_inner()) = Some(kept);
        (note, decoders)
    }
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

/// A request as read: the method, the path, the query, the body.
struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

/// One request answered and the connection closed.
fn serve(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let request = match read_request(&mut stream) {
        Ok(r) => r,
        Err(code) => {
            respond(
                &mut stream,
                code,
                "application/json",
                &error(reason(code)),
                false,
            );
            return;
        }
    };
    let head_only = request.method == "HEAD";
    let (code, content_type, body) = route(shared, &request);
    respond(&mut stream, code, content_type, &body, head_only);
}

/// The routes: what each answers, as (code, content type, body).
fn route(shared: &Shared, r: &Request) -> (u16, &'static str, String) {
    let json = "application/json";
    let get = r.method == "GET" || r.method == "HEAD";
    let job_id = r
        .path
        .strip_prefix("/api/jobs/")
        .and_then(|s| s.parse::<u64>().ok());
    match (r.method.as_str(), r.path.as_str()) {
        (_, "/") | (_, "/index.html") if get => (200, "text/html; charset=utf-8", PAGE.to_string()),
        (_, "/api/status") if get => {
            let body = shared
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .json();
            (200, json, body)
        }
        (_, "/api/list") if get => {
            let (code, body) = list(&shared.library, &r.query);
            (code, json, body)
        }
        (_, "/api/decoders") if get => {
            let (note, decoders) = shared.decoders();
            let body = serde_json::json!({
                "source": note.source, "detail": note.detail, "protocol": decoders.protocol,
                "plugins": decoders.plugins.iter().map(|(name, suffixes)| serde_json::json!({ "name": name, "suffixes": suffixes })).collect::<Vec<_>>(),
                "suffixes": decoders.suffixes, "ffmpeg_only": decoders.ffmpeg_only,
            })
            .to_string()
                + "\n";
            (200, json, body)
        }
        ("POST", "/api/jobs") => {
            let (code, body) = start_job(shared, &r.body);
            (code, json, body)
        }
        ("POST", "/api/batches") => {
            let (code, body) = prepare_batch(shared, &r.body);
            (code, json, body)
        }
        (_, _) if get && r.path.starts_with("/api/batches/") => {
            let id: Option<u64> = r.path["/api/batches/".len()..].parse().ok();
            let previews = shared.previews.lock().unwrap_or_else(|e| e.into_inner());
            match id.and_then(|id| previews.get(&id)) {
                Some(p) => (200, json, json_of(p)),
                None => (404, json, error("no such preview")),
            }
        }
        ("POST", _) if r.path.starts_with("/api/batches/") && r.path.ends_with("/apply") => {
            let id: Option<u64> = r.path["/api/batches/".len()..r.path.len() - "/apply".len()]
                .parse()
                .ok();
            let (code, body) = apply_batch(shared, id);
            (code, json, body)
        }
        (_, "/api/undo") if get => {
            let list: Vec<serde_json::Value> = shared
                .journal
                .list()
                .iter()
                .map(|b| {
                    serde_json::json!({
                        "id": b.id, "kind": b.kind, "path": b.path, "when": b.when,
                        "note": b.note, "undone": b.undone,
                        "files": b.entries.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            (200, json, serde_json::Value::Array(list).to_string() + "\n")
        }
        ("POST", _) if r.path.starts_with("/api/undo/") => {
            let id = r.path["/api/undo/".len()..].to_string();
            let (code, body) = undo_batch(shared, &id);
            (code, json, body)
        }
        (_, "/api/preferences") if get => {
            let body = shared
                .prefs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .view()
                .to_string()
                + "\n";
            (200, json, body)
        }
        ("PUT", "/api/preferences") => {
            let Ok(wanted) = serde_json::from_slice::<prefs::Prefs>(&r.body) else {
                return (400, json, error("the body is not the preferences"));
            };
            let mut store = shared.prefs.lock().unwrap_or_else(|e| e.into_inner());
            match store.save(wanted) {
                Ok(()) => (200, json, store.view().to_string() + "\n"),
                Err(e) if e.starts_with("video must be") => (400, json, error(&e)),
                // Kept in memory; the view says the file refused.
                Err(_) => (200, json, store.view().to_string() + "\n"),
            }
        }
        (_, "/api/jobs") if get => match shared.jobs.current() {
            Some(job) => (200, json, job.to_string() + "\n"),
            None => (404, json, error("no job yet")),
        },
        (_, _) if job_id.is_some() && get => match shared.jobs.get(job_id.unwrap_or(0)) {
            Some(job) => (200, json, job.to_string() + "\n"),
            None => (404, json, error("no such job")),
        },
        ("DELETE", _) if job_id.is_some() => {
            if shared.jobs.cancel(job_id.unwrap_or(0)) {
                (204, json, String::new())
            } else {
                (404, json, error("no such running job"))
            }
        }
        (_, "/")
        | (_, "/index.html")
        | (_, "/api/status")
        | (_, "/api/preferences")
        | (_, "/api/list")
        | (_, "/api/decoders") => (405, json, error("method not allowed")),
        _ => (404, json, error("not found")),
    }
}

/// `POST /api/jobs` with `{ "kind": "ignore", "path": "NAS/Jazz" }`.
fn start_job(shared: &Shared, body: &[u8]) -> (u16, String) {
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (400, error("the body is not JSON"));
    };
    let kind = request["kind"].as_str().unwrap_or("").to_string();
    let path = request["path"].as_str().unwrap_or("").to_string();
    if kind != "ignore" && kind != "cue" && kind != "tags" {
        return (400, error("the kind is not one the core knows"));
    }
    if Library::check(&path).is_none() {
        return (400, error("not a library path"));
    }
    let music = shared.library.music().to_path_buf();
    let (note, decoders) = shared.decoders();
    let job_path = path.clone();
    let job_kind = kind.clone();
    let video_in_use = shared
        .prefs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .video_in_use();
    let started = shared.jobs.start(&kind, &path, move |progress, cancel| {
        if job_kind == "cue" {
            cuecheck::check(&music, &job_path, &decoders, note, progress, cancel)
                .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
        } else if job_kind == "tags" {
            tagcheck::check(&music, &job_path, &decoders, note, progress, cancel)
                .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
        } else {
            analyse::analyse(
                &music,
                &job_path,
                &decoders,
                note,
                video_in_use,
                progress,
                cancel,
            )
            .and_then(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
        }
    });
    match started {
        Ok(id) => (202, serde_json::json!({ "id": id }).to_string() + "\n"),
        Err(_) => (409, error("a job is running; wait for it or cancel it")),
    }
}

fn json_of<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()) + "\n"
}

/// `POST /api/batches` with `{ "kind": "mpdignore", "path": "NAS/Jazz", "lines": [...] }`:
/// the preview, nothing written.
fn prepare_batch(shared: &Shared, body: &[u8]) -> (u16, String) {
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(body) else {
        return (400, error("the body is not JSON"));
    };
    let req = write::Request {
        kind: request["kind"].as_str().unwrap_or("").to_string(),
        path: request["path"].as_str().unwrap_or("").to_string(),
        lines: request["lines"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let shares = match mount::shares(&shared.networkfs) {
        Ok(s) => s,
        Err(e) => {
            // No configuration is no shares: local roots still write.
            if shared.networkfs.exists() {
                return (500, error(&e));
            }
            Vec::new()
        }
    };
    let id = shared.next_preview.fetch_add(1, Ordering::Relaxed) + 1;
    match write::prepare(shared.library.music(), &shares, id, &req) {
        Ok(preview) => {
            let body = json_of(&preview);
            shared
                .previews
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, preview);
            (200, body)
        }
        Err(e) => (400, error(&e)),
    }
}

/// `POST /api/batches/<id>/apply`: the preview written, journaled, MPD told.
fn apply_batch(shared: &Shared, id: Option<u64>) -> (u16, String) {
    let preview = {
        let previews = shared.previews.lock().unwrap_or_else(|e| e.into_inner());
        match id.and_then(|id| previews.get(&id).cloned()) {
            Some(p) => p,
            None => return (404, error("no such preview")),
        }
    };
    let shares = mount::shares(&shared.networkfs).unwrap_or_default();
    match write::apply(
        shared.library.music(),
        &shares,
        &shared.mounter,
        &shared.journal,
        &shared.mpd,
        &preview,
    ) {
        Ok(applied) => {
            shared
                .previews
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&preview.id);
            (200, json_of(&applied))
        }
        Err(e) if e.contains("changed since") => (409, error(&e)),
        Err(e) if e.contains("nothing to write") => (400, error(&e)),
        Err(e) => (500, error(&e)),
    }
}

/// `POST /api/undo/<id>`: the batch undone, MPD told.
fn undo_batch(shared: &Shared, id: &str) -> (u16, String) {
    let shares = mount::shares(&shared.networkfs).unwrap_or_default();
    match write::undo(
        shared.library.music(),
        &shares,
        &shared.mounter,
        &shared.journal,
        &shared.mpd,
        id,
    ) {
        Ok(applied) => (200, json_of(&applied)),
        Err(e) if e.contains("no such") => (404, error(&e)),
        Err(e) if e.contains("changed since") || e.contains("already") => (409, error(&e)),
        Err(e) => (500, error(&e)),
    }
}

/// The request head, up to the blank line, and the body its Content-Length names;
/// the status code when it cannot be read.
fn read_request(stream: &mut TcpStream) -> Result<Request, u16> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (head, body_start) = loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(400),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break (String::from_utf8_lossy(&buf[..i]).into_owned(), i + 4);
        }
        if let Some(i) = buf.windows(2).position(|w| w == b"\n\n") {
            break (String::from_utf8_lossy(&buf[..i]).into_owned(), i + 2);
        }
        if buf.len() > MAX_HEAD {
            return Err(431);
        }
    };
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let content_length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY {
        return Err(413);
    }
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(400),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    body.truncate(content_length);
    Ok(Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
        body,
    })
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
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::tests::temp_library;
    use crate::mpd::tests::fake_mpd;
    use std::path::PathBuf;

    fn make_server(root: PathBuf, mpd: Address) -> Server {
        let data = root.join(".mpdantic-data");
        let glass = root.join(".no-glass");
        // Under the data folder, which is made when first written to, so a listing of
        // the root sees nothing of it.
        let run = data.join("run");
        let shared = Arc::new(Shared {
            status: Arc::new(Mutex::new(Status::new())),
            library: Library::new(root.clone()),
            jobs: Arc::new(Jobs::new()),
            mpd,
            decoders: Mutex::new(None),
            prefs: Mutex::new(prefs::Store::open(&data, &glass)),
            networkfs: root.join(".no-networkfs.json"),
            mounter: mount::Mounter::new(&run, Box::new(|_| Err("no mount in a test".to_string()))),
            journal: undo::Journal::new(&data.join("undo")),
            previews: Mutex::new(std::collections::HashMap::new()),
            next_preview: std::sync::atomic::AtomicU64::new(0),
        });
        start(0, shared).unwrap()
    }

    #[test]
    fn a_batch_is_previewed_applied_listed_and_undone_over_the_api() {
        let root = temp_library("serve-batch");
        let server = make_server(root.clone(), fake_mpd("OK MPD 0.24.6\n"));
        // A folder under NAS with no share of that name in Volumio's configuration (none
        // at all here) is refused, not written.
        let req = "{\"kind\":\"mpdignore\",\"path\":\"NAS/Jazz\",\"lines\":[\"*.iso\"]}";
        let out = talk(
            server.port,
            &format!(
                "POST /api/batches HTTP/1.0\r\nContent-Length: {}\r\n\r\n{req}",
                req.len()
            ),
        );
        assert!(out.starts_with("HTTP/1.0 400"), "{out}");
        assert!(body(&out).contains("no share named Jazz"), "{out}");
        let req = "{\"kind\":\"mpdignore\",\"path\":\"USB\",\"lines\":[\"*.iso\",\"#bad\"]}";
        let out = talk(
            server.port,
            &format!(
                "POST /api/batches HTTP/1.0\r\nContent-Length: {}\r\n\r\n{req}",
                req.len()
            ),
        );
        assert!(out.starts_with("HTTP/1.0 200"), "{out}");
        let preview: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        let id = preview["id"].as_u64().unwrap();
        assert_eq!(preview["file"], "USB/.mpdignore");
        assert_eq!(preview["exists"], false);
        assert_eq!(preview["added"][0], "*.iso");
        assert_eq!(preview["refused"][0][0], "#bad");
        assert_eq!(preview["place"], "local");
        let out = talk(
            server.port,
            &format!("GET /api/batches/{id} HTTP/1.0\r\n\r\n"),
        );
        assert!(out.starts_with("HTTP/1.0 200"), "{out}");
        let out = talk(
            server.port,
            &format!("POST /api/batches/{id}/apply HTTP/1.0\r\nContent-Length: 0\r\n\r\n"),
        );
        assert!(out.starts_with("HTTP/1.0 200"), "{out}");
        let applied: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(applied["updated"], 7);
        let batch = applied["batch"].as_str().unwrap().to_string();
        assert_eq!(
            std::fs::read_to_string(root.join("USB/.mpdignore")).unwrap(),
            "*.iso\n"
        );
        let out = talk(
            server.port,
            &format!("POST /api/batches/{id}/apply HTTP/1.0\r\nContent-Length: 0\r\n\r\n"),
        );
        assert!(out.starts_with("HTTP/1.0 404"), "{out}");
        let out = talk(server.port, "GET /api/undo HTTP/1.0\r\n\r\n");
        let list: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(list[0]["id"], batch);
        assert_eq!(list[0]["undone"], false);
        let out = talk(
            server.port,
            &format!("POST /api/undo/{batch} HTTP/1.0\r\nContent-Length: 0\r\n\r\n"),
        );
        assert!(out.starts_with("HTTP/1.0 200"), "{out}");
        assert!(!root.join("USB/.mpdignore").exists());
        let out = talk(
            server.port,
            &format!("POST /api/undo/{batch} HTTP/1.0\r\nContent-Length: 0\r\n\r\n"),
        );
        assert!(out.starts_with("HTTP/1.0 409"), "{out}");
        let out = talk(
            server.port,
            "POST /api/undo/nothing HTTP/1.0\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(out.starts_with("HTTP/1.0 404"), "{out}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_preferences_are_read_kept_and_refused_when_wrong() {
        let root = temp_library("serve-prefs");
        let server = make_server(root.clone(), no_mpd());
        let out = talk(server.port, "GET /api/preferences HTTP/1.0\r\n\r\n");
        let value: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(value["video"], "auto");
        assert_eq!(value["video_in_use"], false);
        assert_eq!(value["glass"], false);
        let put = "{\"video\":\"in_use\"}";
        let out = talk(
            server.port,
            &format!(
                "PUT /api/preferences HTTP/1.0\r\nContent-Length: {}\r\n\r\n{put}",
                put.len()
            ),
        );
        assert!(out.starts_with("HTTP/1.0 200"), "{out}");
        let value: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(value["video_in_use"], true);
        assert!(root.join(".mpdantic-data/preferences.json").is_file());
        let bad = "{\"video\":\"maybe\"}";
        let out = talk(
            server.port,
            &format!(
                "PUT /api/preferences HTTP/1.0\r\nContent-Length: {}\r\n\r\n{bad}",
                bad.len()
            ),
        );
        assert!(out.starts_with("HTTP/1.0 400"), "{out}");
        let out = talk(
            server.port,
            "POST /api/preferences HTTP/1.0\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(out.starts_with("HTTP/1.0 405"), "{out}");
        std::fs::remove_dir_all(&root).unwrap();
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

    fn no_mpd() -> Address {
        Address::Tcp("127.0.0.1:1".to_string())
    }

    #[test]
    fn the_page_is_served_at_the_root_and_a_head_has_no_body() {
        let root = temp_library("serve-page");
        let server = make_server(root.clone(), no_mpd());
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
        let server = make_server(root.clone(), no_mpd());
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
        let server = make_server(root.clone(), no_mpd());
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
        let server = make_server(root.clone(), no_mpd());
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
    fn an_unknown_route_is_not_found_and_a_wrong_method_is_not_allowed() {
        let root = temp_library("serve-routes");
        let server = make_server(root.clone(), no_mpd());
        let out = talk(server.port, "GET /nothing HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 404"), "{out}");
        let out = talk(
            server.port,
            "POST /api/list HTTP/1.0\r\nContent-Length: 0\r\n\r\n",
        );
        assert!(out.starts_with("HTTP/1.0 405"), "{out}");
        let out = talk(server.port, "GET /api/jobs/7 HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 404"), "{out}");
        let out = talk(server.port, "GET /api/jobs HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 404"), "{out}");
        let out = talk(server.port, "DELETE /api/jobs/7 HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 404"), "{out}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_decoders_come_from_mpd_or_are_assumed() {
        let root = temp_library("serve-decoders");
        let server = make_server(root.clone(), fake_mpd("OK MPD 0.24.6\n"));
        let out = talk(server.port, "GET /api/decoders HTTP/1.0\r\n\r\n");
        let value: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(value["source"], "mpd");
        assert_eq!(value["suffixes"][0], "flac");
        assert_eq!(value["ffmpeg_only"][0], "mkv");
        assert_eq!(value["plugins"][0]["name"], "mad");
        assert!(value["detail"]
            .as_str()
            .unwrap()
            .starts_with("MPD protocol 0.24.6"));
        let server = make_server(root.clone(), no_mpd());
        let out = talk(server.port, "GET /api/decoders HTTP/1.0\r\n\r\n");
        let value: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(value["source"], "assumed");
        assert!(value["detail"]
            .as_str()
            .unwrap()
            .contains("could not be asked"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_job_is_started_followed_and_its_report_read() {
        let root = temp_library("serve-job");
        let server = make_server(root.clone(), fake_mpd("OK MPD 0.24.6\n"));
        let request = "{\"kind\":\"ignore\",\"path\":\"NAS\"}";
        let out = talk(
            server.port,
            &format!(
                "POST /api/jobs HTTP/1.0\r\nContent-Length: {}\r\n\r\n{request}",
                request.len()
            ),
        );
        assert!(out.starts_with("HTTP/1.0 202"), "{out}");
        let id = serde_json::from_str::<serde_json::Value>(body(&out)).unwrap()["id"]
            .as_u64()
            .unwrap();
        let mut job = serde_json::Value::Null;
        for _ in 0..200 {
            let out = talk(server.port, &format!("GET /api/jobs/{id} HTTP/1.0\r\n\r\n"));
            job = serde_json::from_str(body(&out)).unwrap();
            if job["state"] != "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(job["state"], "done", "{job}");
        assert_eq!(job["result"]["path"], "NAS");
        assert_eq!(job["result"]["decoders"]["source"], "mpd");
        assert_eq!(job["result"]["classes"]["audio"]["count"], 2);
        // The current job, for a page that opens after the job was started.
        let out = talk(server.port, "GET /api/jobs HTTP/1.0\r\n\r\n");
        assert!(out.starts_with("HTTP/1.0 200"), "{out}");
        let current: serde_json::Value = serde_json::from_str(body(&out)).unwrap();
        assert_eq!(current["id"], id);
        // A bad body, a bad path, a kind the core does not know.
        let out = talk(
            server.port,
            "POST /api/jobs HTTP/1.0\r\nContent-Length: 3\r\n\r\nabc",
        );
        assert!(out.starts_with("HTTP/1.0 400"), "{out}");
        let bad = "{\"kind\":\"ignore\",\"path\":\"../x\"}";
        let out = talk(
            server.port,
            &format!(
                "POST /api/jobs HTTP/1.0\r\nContent-Length: {}\r\n\r\n{bad}",
                bad.len()
            ),
        );
        assert!(out.starts_with("HTTP/1.0 400"), "{out}");
        let bad = "{\"kind\":\"nothing\",\"path\":\"NAS\"}";
        let out = talk(
            server.port,
            &format!(
                "POST /api/jobs HTTP/1.0\r\nContent-Length: {}\r\n\r\n{bad}",
                bad.len()
            ),
        );
        assert!(out.starts_with("HTTP/1.0 400"), "{out}");
        // The CUE check is the other kind.
        let cue = "{\"kind\":\"cue\",\"path\":\"NAS\"}";
        let out = talk(
            server.port,
            &format!(
                "POST /api/jobs HTTP/1.0\r\nContent-Length: {}\r\n\r\n{cue}",
                cue.len()
            ),
        );
        assert!(out.starts_with("HTTP/1.0 202"), "{out}");
        let id = serde_json::from_str::<serde_json::Value>(body(&out)).unwrap()["id"]
            .as_u64()
            .unwrap();
        let mut job = serde_json::Value::Null;
        for _ in 0..200 {
            let out = talk(server.port, &format!("GET /api/jobs/{id} HTTP/1.0\r\n\r\n"));
            job = serde_json::from_str(body(&out)).unwrap();
            if job["state"] != "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(job["state"], "done", "{job}");
        assert_eq!(job["kind"], "cue");
        assert_eq!(job["result"]["sheets"], 0);
        // The tag check is the third kind.
        let tags = "{\"kind\":\"tags\",\"path\":\"NAS\"}";
        let out = talk(
            server.port,
            &format!(
                "POST /api/jobs HTTP/1.0\r\nContent-Length: {}\r\n\r\n{tags}",
                tags.len()
            ),
        );
        assert!(out.starts_with("HTTP/1.0 202"), "{out}");
        let id = serde_json::from_str::<serde_json::Value>(body(&out)).unwrap()["id"]
            .as_u64()
            .unwrap();
        let mut job = serde_json::Value::Null;
        for _ in 0..200 {
            let out = talk(server.port, &format!("GET /api/jobs/{id} HTTP/1.0\r\n\r\n"));
            job = serde_json::from_str(body(&out)).unwrap();
            if job["state"] != "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(job["state"], "done", "{job}");
        assert_eq!(job["kind"], "tags");
        // The sample library's flac files are not real: read, not readable.
        assert_eq!(job["result"]["read"], 2);
        assert_eq!(job["result"]["findings"]["T12"]["count"], 2);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_body_too_large_is_refused() {
        let root = temp_library("serve-large");
        let server = make_server(root.clone(), no_mpd());
        let out = talk(
            server.port,
            "POST /api/jobs HTTP/1.0\r\nContent-Length: 999999\r\n\r\n",
        );
        assert!(out.starts_with("HTTP/1.0 413"), "{out}");
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
