//! The core of MPDantic: one daemon, started by the plugin as its own systemd
//! unit, that audits and repairs the library and serves the management
//! interface. At this version it serves the pages, the library one folder at a
//! time, the `.mpdignore` analysis, the CUE check and the tag check of a folder as
//! jobs, and its status.
//!
//! `mpdantic [--run DIR] [--settings FILE] [--music DIR] [--mpd ADDRESS] [--data DIR] [--glass DIR] [--version]`
//!
//! `DIR` under `--run` holds `status.json` (`/run/mpdantic` unless given);
//! `--settings` is the file the plugin writes, with the port and the player's language
//! (`/data/configuration/system_controller/mpdantic/settings.json` unless given,
//! the default port and English when the file is not there); `--music` is MPD's music
//! directory (`/var/lib/mpd/music` unless given); `--mpd` is where MPD listens,
//! a socket path or host:port (`/run/mpd/socket` unless given); `--data` is the
//! folder the preferences are kept in (`/data/INTERNAL/mpdantic` unless given);
//! `--glass` is the Glass plugin's folder, whose presence says video in the
//! library is wanted (`/data/plugins/user_interface/glass` unless given);
//! `--networkfs` is Volumio's configuration of the shares, read when a write on a
//! share needs its credentials
//! (`/data/configuration/system_controller/networkfs/config.json` unless given);
//! `--volumio-playlists` is the folder Volumio keeps its own playlists in
//! (`/data/playlist` unless given).

// First, for its `msg!` macro, which the modules after it use.
#[macro_use]
mod text;

mod analyse;
mod classify;
mod cue;
mod cuecheck;
mod cuefix;
mod findings;
mod folders;
mod ignore;
mod jobs;
mod library;
mod mb;
mod mount;
mod mpd;
mod names;
mod organise;
mod playlist;
mod plcheck;
mod plfix;
mod plimport;
mod prefs;
mod report;
mod serve;
mod status;
mod tagcheck;
mod tagfix;
mod tags;
mod undo;
mod vlists;
mod walk;
mod write;

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use jobs::Jobs;
use library::Library;
use status::Status;

/// The version the workspace carries; `--version` and the status say it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The pages' port when the settings say nothing.
const DEFAULT_PORT: u16 = 6218;

/// What the command line said.
struct Args {
    run: PathBuf,
    settings: PathBuf,
    music: PathBuf,
    mpd: mpd::Address,
    data: PathBuf,
    glass: PathBuf,
    networkfs: PathBuf,
    volumio_playlists: PathBuf,
}

fn usage() -> ! {
    eprintln!("mpdantic [--run DIR] [--settings FILE] [--music DIR] [--mpd ADDRESS] [--data DIR] [--glass DIR] [--networkfs FILE] [--volumio-playlists DIR] [--version]");
    process::exit(64);
}

fn args() -> Args {
    let mut out = Args {
        run: PathBuf::from("/run/mpdantic"),
        settings: PathBuf::from("/data/configuration/system_controller/mpdantic/settings.json"),
        music: PathBuf::from("/var/lib/mpd/music"),
        mpd: mpd::Address::parse("/run/mpd/socket"),
        data: PathBuf::from("/data/INTERNAL/mpdantic"),
        glass: PathBuf::from(prefs::GLASS_PLUGIN),
        networkfs: PathBuf::from("/data/configuration/system_controller/networkfs/config.json"),
        volumio_playlists: PathBuf::from("/data/playlist"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--run" => out.run = it.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--settings" => out.settings = it.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--music" => out.music = it.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--data" => out.data = it.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--glass" => out.glass = it.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--networkfs" => {
                out.networkfs = it.next().map(PathBuf::from).unwrap_or_else(|| usage())
            }
            "--volumio-playlists" => {
                out.volumio_playlists = it.next().map(PathBuf::from).unwrap_or_else(|| usage())
            }
            "--mpd" => {
                out.mpd = it
                    .next()
                    .map(|s| mpd::Address::parse(&s))
                    .unwrap_or_else(|| usage())
            }
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
        jobs: Arc::new(Jobs::new()),
        mpd: args.mpd.clone(),
        decoders: Mutex::new(None),
        prefs: Mutex::new(prefs::Store::open(&args.data, &args.glass)),
        networkfs: args.networkfs.clone(),
        volumio_playlists: args.volumio_playlists.clone(),
        // The shell's plan for the output buffer, written beside the settings.
        buffer_plan: args.settings.with_file_name("buffer.json"),
        settings: args.settings.clone(),
        mb_cache: args.data.join("mb-cache"),
        measuring: std::sync::atomic::AtomicBool::new(false),
        album: Mutex::new(None),
        mounter: mount::Mounter::new(&args.run, mount::sudo_runner()),
        journal: undo::Journal::new(&args.data.join("undo")),
        findings: findings::Store::new(&args.data),
        previews: Mutex::new(std::collections::HashMap::new()),
        next_preview: std::sync::atomic::AtomicU64::new(0),
    });
    // The pages. A port that cannot be taken is said in the status, and the core stays
    // up so the plugin's button can say it too; the unit is not restarted in a loop.
    let _server = match serve::start(port, shared) {
        Ok(server) => {
            status.lock().unwrap_or_else(|e| e.into_inner()).port = Some(server.port);
            eprintln!(
                "mpdantic {VERSION}: the pages on port {}, the library at {}, MPD at {}",
                server.port,
                args.music.display(),
                args.mpd
            );
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
            port_from(&temp_file("settings.json", Some("{ \"port\": 6219 }\n"))),
            Ok(6219)
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
