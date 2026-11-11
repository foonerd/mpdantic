//! The preferences the pages keep: what the owner knows about the library that no
//! analysis can. Kept in the data folder, which survives an uninstall; held in
//! memory when the folder cannot be written, and the page says so.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::text::{self, Msg};

/// Where Volumio's Glass plugin lives when it is installed: it plays video from the
/// library, so video there is wanted.
pub const GLASS_PLUGIN: &str = "/data/plugins/user_interface/glass";

/// Whether video in the library is wanted: `auto` (in use when the Glass plugin is
/// on the player), `in_use`, or `junk`.
pub const VIDEO_CHOICES: &[&str] = &["auto", "in_use", "junk"];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Prefs {
    #[serde(default = "auto")]
    pub video: String,
}

fn auto() -> String {
    "auto".to_string()
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { video: auto() }
    }
}

pub struct Store {
    file: PathBuf,
    glass: PathBuf,
    pub prefs: Prefs,
    /// Why the file could not be read or written, for the page.
    pub error: Option<Msg>,
}

/// The key of the one refusal the page can cause: a video choice that is not one.
pub const VIDEO_CHOICES_KEY: &str = "core.prefs.video_choices";

impl Store {
    /// The preferences from `<data>/preferences.json`, the defaults when there are
    /// none; `glass` is the folder whose presence says video is in use.
    pub fn open(data: &Path, glass: &Path) -> Store {
        let file = data.join("preferences.json");
        let (prefs, error) = match fs::read_to_string(&file) {
            Ok(text) => match serde_json::from_str::<Prefs>(&text) {
                Ok(p) => (p, None),
                Err(e) => (Prefs::default(), Some(text::at(file.display(), e))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Prefs::default(), None),
            Err(e) => (Prefs::default(), Some(text::at(file.display(), e))),
        };
        let prefs = if VIDEO_CHOICES.contains(&prefs.video.as_str()) {
            prefs
        } else {
            Prefs::default()
        };
        Store {
            file,
            glass: glass.to_path_buf(),
            prefs,
            error,
        }
    }

    /// The preferences kept, written whole and moved into place; held in memory and
    /// the error kept when the folder refuses.
    pub fn save(&mut self, prefs: Prefs) -> Result<(), Msg> {
        if !VIDEO_CHOICES.contains(&prefs.video.as_str()) {
            return Err(msg!(
                "core.prefs.video_choices",
                "video must be one of {choices}",
                choices = VIDEO_CHOICES.join(", ")
            ));
        }
        self.prefs = prefs;
        let result = (|| {
            if let Some(dir) = self.file.parent() {
                fs::create_dir_all(dir)?;
            }
            let part = self.file.with_extension("json.part");
            fs::write(
                &part,
                serde_json::to_string_pretty(&self.prefs).unwrap_or_default() + "\n",
            )?;
            fs::rename(&part, &self.file)
        })();
        match result {
            Ok(()) => {
                self.error = None;
                Ok(())
            }
            Err(e) => {
                let said = text::at(self.file.display(), e);
                self.error = Some(said.clone());
                Err(said)
            }
        }
    }

    pub fn glass_present(&self) -> bool {
        self.glass.is_dir()
    }

    /// Whether video containers are wanted in the library: the owner's word, or
    /// Glass's presence.
    pub fn video_in_use(&self) -> bool {
        match self.prefs.video.as_str() {
            "in_use" => true,
            "junk" => false,
            _ => self.glass_present(),
        }
    }

    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({
            "video": self.prefs.video,
            "video_in_use": self.video_in_use(),
            "glass": self.glass_present(),
            "file": self.file.display().to_string(),
            "error": self.error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpdantic-prefs-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn video_follows_glass_unless_the_owner_says() {
        let dir = temp("auto");
        let glass = dir.join("glass");
        let store = Store::open(&dir.join("data"), &glass);
        assert_eq!(store.prefs, Prefs::default());
        assert!(!store.video_in_use());
        assert!(store.error.is_none());
        fs::create_dir_all(&glass).unwrap();
        assert!(store.video_in_use());
        assert!(store.glass_present());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_choice_is_kept_whole_and_read_back() {
        let dir = temp("save");
        let mut store = Store::open(&dir.join("data"), &dir.join("glass"));
        store
            .save(Prefs {
                video: "in_use".to_string(),
            })
            .unwrap();
        assert!(store.video_in_use());
        assert!(dir.join("data/preferences.json").is_file());
        assert!(!dir.join("data/preferences.json.part").exists());
        let again = Store::open(&dir.join("data"), &dir.join("glass"));
        assert_eq!(again.prefs.video, "in_use");
        assert!(again.video_in_use());
        assert_eq!(again.view()["video_in_use"], true);
        assert_eq!(again.view()["glass"], false);
        assert!(store
            .save(Prefs {
                video: "maybe".to_string()
            })
            .is_err());
        store
            .save(Prefs {
                video: "junk".to_string(),
            })
            .unwrap();
        fs::create_dir_all(dir.join("glass")).unwrap();
        assert!(!store.video_in_use());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_bad_file_gives_the_defaults_and_says_why() {
        let dir = temp("bad");
        fs::write(dir.join("preferences.json"), "not json").unwrap();
        let store = Store::open(&dir, &dir.join("glass"));
        assert_eq!(store.prefs, Prefs::default());
        assert!(store.error.as_deref().unwrap().contains("preferences.json"));
        fs::write(dir.join("preferences.json"), "{ \"video\": \"nonsense\" }").unwrap();
        assert_eq!(Store::open(&dir, &dir.join("glass")).prefs.video, "auto");
        fs::remove_dir_all(&dir).unwrap();
    }
}
