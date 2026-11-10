//! The core of MPDantic: one daemon, started by the plugin as its own systemd
//! unit, that audits and repairs the library and serves the management
//! interface. At this version it serves the pages, the library one folder at a
//! time, and its status.
//!
//! `mpdantic [--run DIR] [--settings FILE] [--music DIR] [--version]`
//!
//! `DIR` under `--run` holds `status.json` (`/run/mpdantic` unless given);
//! `--settings` is the file the plugin writes, with the port
//! (`/data/configuration/system_controller/mpdantic/settings.json` unless given,
//! the default port when the file is not there); `--music` is MPD's music
//! directory (`/var/lib/mpd/music` unless given).

mod library;
mod serve;
mod status;

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use library::Library;
use status::Status;

/// The version the workspace carries; `--version` and the status say it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The pages' port when the settings say nothing.
const DEFAULT_PORT: u16 = 8030;

/// What the command line said.
struct Args {
    run: PathBuf,
    settings: PathBuf,
    music: PathBuf,
}

fn usage() -> ! {
    eprintln!("mpdantic [--run DIR] [--settings FILE] [--music DIR] [--version]");
    process::exit(64);
}

fn args() -> Args {
    let mut out = Args {
        run: PathBuf::from("/run/mpdantic"),
        settings: PathBuf::from("/data/configuration/system_controller/mpdantic/settings.json"),
        music: PathBuf::from("/var/lib/mpd/music"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--run" => out.run = it.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--settings" => out.settings = it.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--music" => out.music = it.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--version" => {
                println!("mpdantic {VERSION}");
                process::exit(0);
            }
            _ => usage(),
        }
    }
    out
}

/// The port from the plugin's settings file: the default when the file is not there
/// (the core run by hand), an error when it is there and says something else.
fn port_from(settings: &Path) -> Result<u16, String> {
    let text = match fs::read_to_string(settings) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(DEFAULT_PORT),
        Err(e) => return Err(format!("{}: {e}", settings.display())),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", settings.display()))?;
    match value.get("port").and_then(|p| p.as_u64()) {
        Some(p) if (1024..=65535).contains(&p) => Ok(p as u16),
        _ => Err(format!(
            "{}: no port from 1024 to 65535 in it",
            settings.display()
        )),
    }
}

fn main() {
    let args = args();
    if let Err(e) = fs::create_dir_all(&args.run) {
        eprintln!("mpdantic: {}: {e}", args.run.display());
        process::exit(1);
    }
    let status = Arc::new(Mutex::new(Status::new()));
    let write = |status: &Arc<Mutex<Status>>| {
        let result = status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(&args.run);
        if let Err(e) = result {
            eprintln!("mpdantic: {}: {e}", args.run.display());
        }
    };
    let port = match port_from(&args.settings) {
        Ok(port) => port,
        Err(e) => {
            eprintln!("mpdantic: {e}; the pages on port {DEFAULT_PORT}");
            DEFAULT_PORT
        }
    };
    let shared = Arc::new(serve::Shared {
        status: status.clone(),
        library: Library::new(args.music.clone()),
    });
    // The pages. A port that cannot be taken is said in the status, and the core stays
    // up so the plugin's button can say it too; the unit is not restarted in a loop.
    let _server = match serve::start(port, shared) {
        Ok(server) => {
            status.lock().unwrap_or_else(|e| e.into_inner()).port = Some(server.port);
            eprintln!("mpdantic {VERSION}: the pages on port {}", server.port);
            Some(server)
        }
        Err(e) => {
            eprintln!("mpdantic {VERSION}: {e}");
            status.lock().unwrap_or_else(|e| e.into_inner()).failed(e);
            None
        }
    };
    write(&status);
    // The status file kept fresh for the plugin's button. The unit's stop ends the
    // process with SIGTERM, whose default action is to terminate it.
    loop {
        thread::sleep(Duration::from_secs(30));
        write(&status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, text: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpdantic-main-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        let _ = fs::remove_file(&file);
        if let Some(text) = text {
            fs::write(&file, text).unwrap();
        }
        file
    }

    #[test]
    fn the_port_comes_from_the_settings_or_is_the_default_when_there_are_none() {
        assert_eq!(
            port_from(&temp_file("settings.json", Some("{ \"port\": 8031 }\n"))),
            Ok(8031)
        );
        assert_eq!(
            port_from(&temp_file("missing.json", None)),
            Ok(DEFAULT_PORT)
        );
        assert!(port_from(&temp_file("low.json", Some("{ \"port\": 80 }"))).is_err());
        assert!(port_from(&temp_file("none.json", Some("{}"))).is_err());
        assert!(port_from(&temp_file("bad.json", Some("not json"))).is_err());
    }
}
