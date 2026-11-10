//! The status the core keeps: written to `status.json` in the run folder for the
//! plugin's status button, and served at `/api/status` for the page.

use std::fs;
use std::io;
use std::path::Path;
use std::process;
use std::time::Instant;

pub struct Status {
    /// `idle`, or `error` with `error` saying why.
    pub state: String,
    pub error: Option<String>,
    /// The port the pages are served on, once they are.
    pub port: Option<u16>,
    started: Instant,
}

impl Status {
    pub fn new() -> Status {
        Status {
            state: "idle".to_string(),
            error: None,
            port: None,
            started: Instant::now(),
        }
    }

    /// Something the core could not do; the page and the button say it.
    pub fn failed(&mut self, what: String) {
        self.state = "error".to_string();
        self.error = Some(what);
    }

    /// The status as JSON, one line.
    pub fn json(&self) -> String {
        serde_json::json!({
            "version": crate::VERSION,
            "pid": process::id(),
            "state": self.state,
            "error": self.error,
            "port": self.port,
            "uptime": self.started.elapsed().as_secs(),
        })
        .to_string()
            + "\n"
    }

    /// Written whole and moved into place, so a reader never sees a part.
    pub fn write(&self, dir: &Path) -> io::Result<()> {
        let part = dir.join("status.json.part");
        fs::write(&part, self.json())?;
        fs::rename(&part, dir.join("status.json"))
    }
}

impl Default for Status {
    fn default() -> Self {
        Status::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_names_the_version_the_process_the_state_and_the_port() {
        let mut status = Status::new();
        status.port = Some(8030);
        let value: serde_json::Value = serde_json::from_str(&status.json()).unwrap();
        assert_eq!(value["version"], crate::VERSION);
        assert_eq!(value["pid"], process::id());
        assert_eq!(value["state"], "idle");
        assert_eq!(value["error"], serde_json::Value::Null);
        assert_eq!(value["port"], 8030);
        assert!(value["uptime"].is_u64());
        status.failed("port 8030: in use".to_string());
        let value: serde_json::Value = serde_json::from_str(&status.json()).unwrap();
        assert_eq!(value["state"], "error");
        assert_eq!(value["error"], "port 8030: in use");
    }

    #[test]
    fn the_status_is_written_whole_and_moved_into_place() {
        let dir = std::env::temp_dir().join(format!("mpdantic-status-{}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        Status::new().write(&dir).unwrap();
        let text = fs::read_to_string(dir.join("status.json")).unwrap();
        assert!(text.ends_with('\n'));
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
        assert!(!dir.join("status.json.part").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
