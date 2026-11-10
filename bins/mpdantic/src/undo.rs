//! The undo journal: every batch MPDantic writes, kept outside the library under the
//! data folder, which an uninstall leaves alone. A text file is kept whole, before
//! and after, so the undo is the before written back and the page can show both.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// One file of a batch.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The library path, MPD's form.
    pub path: String,
    /// `write_text`: a text file written whole (created when `before` is none).
    pub op: String,
    pub before: Option<String>,
    pub after: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub id: String,
    pub kind: String,
    /// The folder in hand.
    pub path: String,
    /// Seconds since the epoch, when applied.
    pub when: u64,
    /// What the batch did, in a line.
    pub note: String,
    pub entries: Vec<Entry>,
    pub undone: bool,
}

pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn new(dir: &Path) -> Journal {
        Journal {
            dir: dir.to_path_buf(),
        }
    }

    /// An id from the clock, unique within the folder.
    pub fn new_id(&self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut n = 0;
        loop {
            let id = if n == 0 {
                format!("{now}")
            } else {
                format!("{now}-{n}")
            };
            if !self.dir.join(&id).exists() {
                return id;
            }
            n += 1;
        }
    }

    /// The batch recorded, whole and moved into place.
    pub fn record(&self, batch: &Batch) -> Result<(), String> {
        let dir = self.dir.join(&batch.id);
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let text = serde_json::to_string_pretty(batch).map_err(|e| e.to_string())? + "\n";
        let part = dir.join("manifest.json.part");
        fs::write(&part, text).map_err(|e| format!("{}: {e}", part.display()))?;
        fs::rename(&part, dir.join("manifest.json")).map_err(|e| e.to_string())
    }

    pub fn load(&self, id: &str) -> Option<Batch> {
        if id.is_empty() || id.contains('/') || id.contains("..") {
            return None;
        }
        let text = fs::read_to_string(self.dir.join(id).join("manifest.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Every batch, newest first.
    pub fn list(&self) -> Vec<Batch> {
        let mut ids: Vec<String> = match fs::read_dir(&self.dir) {
            Ok(read) => read
                .flatten()
                .filter(|e| e.path().join("manifest.json").is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        ids.sort();
        ids.reverse();
        ids.iter().filter_map(|id| self.load(id)).collect()
    }

    pub fn mark_undone(&self, id: &str) -> Result<(), String> {
        let mut batch = self.load(id).ok_or_else(|| "no such batch".to_string())?;
        batch.undone = true;
        self.record(&batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpdantic-undo-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_batch_is_recorded_listed_newest_first_and_marked_undone() {
        let dir = temp("journal");
        let j = Journal::new(&dir);
        assert!(j.list().is_empty());
        let a = Batch {
            id: "100".into(),
            kind: "mpdignore".into(),
            path: "NAS/Jazz".into(),
            when: 100,
            note: "one line".into(),
            entries: vec![Entry {
                path: "NAS/Jazz/.mpdignore".into(),
                op: "write_text".into(),
                before: None,
                after: "*.iso\n".into(),
            }],
            undone: false,
        };
        let b = Batch {
            id: "200".into(),
            when: 200,
            ..a.clone()
        };
        j.record(&a).unwrap();
        j.record(&b).unwrap();
        let list = j.list();
        assert_eq!(
            list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
            ["200", "100"]
        );
        assert_eq!(j.load("100").unwrap(), a);
        assert!(j.load("../100").is_none());
        assert!(j.load("").is_none());
        j.mark_undone("100").unwrap();
        assert!(j.load("100").unwrap().undone);
        assert!(j.mark_undone("300").is_err());
        let id = j.new_id();
        assert!(!dir.join(&id).exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
