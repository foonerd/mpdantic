//! The core of MPDantic: one daemon, started by the plugin as its own systemd
//! unit, that will audit and repair the library and serve the management
//! interface. At this version it writes its status and waits for its unit's
//! stop: what it proves is the pipeline that builds, checks, ships and signs it.
//!
//! `mpdantic [--run DIR] [--version]`
//!
//! `DIR` under `--run` holds `status.json` (`/run/mpdantic` unless given).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::Duration;

/// The version the workspace carries; `--version` and the status say it.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the command line said.
struct Args {
    run: PathBuf,
}

fn usage() -> ! {
    eprintln!("mpdantic [--run DIR] [--version]");
    process::exit(64);
}

fn args() -> Args {
    let mut out = Args {
        run: PathBuf::from("/run/mpdantic"),
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--run" => out.run = it.next().map(PathBuf::from).unwrap_or_else(|| usage()),
            "--version" => {
                println!("mpdantic {VERSION}");
                process::exit(0);
            }
            _ => usage(),
        }
    }
    out
}

/// The status file's text: the version, the process and what the core is doing.
fn status_text(version: &str, pid: u32, state: &str) -> String {
    format!("{{ \"version\": \"{version}\", \"pid\": {pid}, \"state\": \"{state}\" }}\n")
}

/// Written whole and moved into place, so a reader never sees a part.
fn write_status(dir: &Path, text: &str) -> io::Result<()> {
    let part = dir.join("status.json.part");
    fs::write(&part, text)?;
    fs::rename(&part, dir.join("status.json"))
}

fn main() {
    let args = args();
    if let Err(e) = fs::create_dir_all(&args.run)
        .and_then(|_| write_status(&args.run, &status_text(VERSION, process::id(), "idle")))
    {
        eprintln!("mpdantic: {}: {e}", args.run.display());
        process::exit(1);
    }
    eprintln!("mpdantic {VERSION}: idle; status in {}", args.run.display());
    // Nothing to do yet. The unit's stop ends the process with SIGTERM, whose
    // default action is to terminate it.
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_names_the_version_the_process_and_the_state() {
        let text = status_text("0.1.0", 42, "idle");
        assert_eq!(
            text,
            "{ \"version\": \"0.1.0\", \"pid\": 42, \"state\": \"idle\" }\n"
        );
    }

    #[test]
    fn the_status_is_written_whole_and_moved_into_place() {
        let dir = std::env::temp_dir().join(format!("mpdantic-test-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        write_status(&dir, "{}\n").unwrap();
        assert_eq!(fs::read_to_string(dir.join("status.json")).unwrap(), "{}\n");
        assert!(!dir.join("status.json.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
