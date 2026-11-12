//! Folders whose library writes are on disk but MPD has not been told yet.
//! Kept under the data folder so a reload still shows Update library.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::text::{self, Msg};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
struct File {
    #[serde(default)]
    paths: BTreeSet<String>,
}

/// Pending library folders waiting for one MPD `update`.
pub struct Pending {
    file: PathBuf,
    paths: Mutex<BTreeSet<String>>,
}

impl Pending {
    /// The set from `<data>/pending.json`, empty when there is none.
    pub fn open(data: &Path) -> Pending {
        let file = data.join("pending.json");
        let paths = match fs::read_to_string(&file) {
            Ok(text) => match serde_json::from_str::<File>(&text) {
                Ok(f) => f.paths,
                Err(_) => BTreeSet::new(),
            },
            Err(_) => BTreeSet::new(),
        };
        Pending {
            file,
            paths: Mutex::new(paths),
        }
    }

    pub fn list(&self) -> Vec<String> {
        self.paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Record a library folder after a write or undo.
    pub fn add(&self, folder: &str) -> Result<(), Msg> {
        let folder = folder.trim_matches('/').to_string();
        {
            let mut paths = self.paths.lock().unwrap_or_else(|e| e.into_inner());
            paths.insert(folder);
        }
        self.save()
    }

    /// The one path MPD should update to cover every pending folder: the
    /// shallowest common ancestor, or the empty path (whole library) when the
    /// pending folders share no useful parent.
    pub fn flush_path(&self) -> String {
        let paths = self.list();
        common_ancestor(&paths)
    }

    pub fn clear(&self) -> Result<(), Msg> {
        self.paths.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.save()
    }

    fn save(&self) -> Result<(), Msg> {
        let paths = self.paths.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let body = File { paths };
        if let Some(dir) = self.file.parent() {
            fs::create_dir_all(dir).map_err(|e| text::at(dir.display(), e))?;
        }
        let part = self.file.with_extension("json.part");
        fs::write(
            &part,
            serde_json::to_string_pretty(&body).unwrap_or_default() + "\n",
        )
        .map_err(|e| text::at(part.display(), e))?;
        fs::rename(&part, &self.file).map_err(|e| text::at(self.file.display(), e))
    }
}

/// Longest common prefix of path components; empty when there is none useful.
fn common_ancestor(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let split: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| p.split('/').filter(|s| !s.is_empty()).collect())
        .collect();
    let Some(first) = split.first() else {
        return String::new();
    };
    let mut depth = first.len();
    for parts in &split[1..] {
        let shared = first
            .iter()
            .zip(parts.iter())
            .take_while(|(a, b)| a == b)
            .count();
        depth = depth.min(shared);
    }
    if depth == 0 {
        return String::new();
    }
    first[..depth].join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpdantic-pending-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn add_list_clear_and_persist() {
        let dir = temp("persist");
        let p = Pending::open(&dir);
        assert!(p.list().is_empty());
        p.add("NAS/A/Album").unwrap();
        p.add("NAS/A/Other").unwrap();
        assert_eq!(
            p.list(),
            vec!["NAS/A/Album".to_string(), "NAS/A/Other".to_string()]
        );
        let again = Pending::open(&dir);
        assert_eq!(again.list(), p.list());
        again.clear().unwrap();
        assert!(Pending::open(&dir).list().is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn flush_path_is_the_common_ancestor_or_empty() {
        assert_eq!(common_ancestor(&[]), "");
        assert_eq!(
            common_ancestor(&["NAS/QNAP/A/Album".to_string()]),
            "NAS/QNAP/A/Album"
        );
        assert_eq!(
            common_ancestor(&[
                "NAS/QNAP/A/Album One".to_string(),
                "NAS/QNAP/A/Album Two".to_string()
            ]),
            "NAS/QNAP/A"
        );
        assert_eq!(
            common_ancestor(&["NAS/One".to_string(), "INTERNAL/Two".to_string()]),
            ""
        );
        assert_eq!(
            common_ancestor(&["NAS/QNAP/A".to_string(), "NAS/QNAP/A/Album".to_string()]),
            "NAS/QNAP/A"
        );
    }
}
