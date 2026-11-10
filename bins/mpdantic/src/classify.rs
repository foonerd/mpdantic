//! What a name in the library is to MPD's update walk (`src/db/update/Walk.cxx` at
//! v0.24.6): a song when a decoder claims its suffix, else an archive it opens and
//! walks, else a playlist; and what it is to a library's owner: a folder the NAS
//! writes for itself, a sidecar a desktop leaves, a video the ffmpeg decoder happens
//! to claim, an image Volumio shows as folder art.

use std::collections::BTreeSet;
use std::ffi::OsStr;

use serde::Serialize;

use crate::ignore::Pattern;

/// What MPD opens as an archive and walks inside.
pub const ARCHIVE_SUFFIXES: &[&str] = &["iso", "zip", "bz2"];
/// What MPD's playlist plugins take, besides CUE.
pub const PLAYLIST_SUFFIXES: &[&str] = &["m3u", "m3u8", "pls", "xspf", "asx", "rss"];
pub const CUE_SUFFIX: &str = "cue";
/// Suffixes the ffmpeg decoder claims that are video containers in practice.
pub const VIDEO_SUFFIXES: &[&str] = &[
    "mp4", "m4v", "mkv", "avi", "mov", "flv", "wmv", "webm", "ts", "mts", "m2ts", "mpg", "mpeg",
    "vob", "swf", "3gp",
];
/// Folder art and the rest: never suggested.
pub const IMAGE_SUFFIXES: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff"];
/// Folders a NAS or a desktop writes for itself, as patterns on the folder's name.
pub const SYSTEM_FOLDERS: &[&str] = &[
    // A line starting with # is a comment to MPD: Synology's bin is matched with the
    // hash in a bracket, which fnmatch reads as the character itself.
    "[#]recycle",
    "@eaDir",
    "@Recently-Snapshot",
    ".Trash*",
    ".Trashes",
    ".TemporaryItems",
    ".Spotlight-V100",
    ".fseventsd",
    ".AppleDouble",
    "lost+found",
    "System Volume Information",
    "$RECYCLE.BIN",
];
/// Files a desktop leaves beside the music, as patterns on the file's name.
pub const SIDECARS: &[&str] = &["._*", ".DS_Store", "Thumbs.db", "desktop.ini", "*.lnk"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    SystemFolder,
    Sidecar,
    Archive,
    Video,
    Audio,
    Cue,
    Playlist,
    Image,
    Other,
    Folder,
}

impl Class {
    /// What the page says of the class, and why a pattern is suggested.
    pub fn note(self) -> &'static str {
        match self {
            Class::SystemFolder => "A folder the NAS or a desktop writes for itself; MPD walks it.",
            Class::Sidecar => "A file a desktop leaves beside the music; MPD tries it by its suffix.",
            Class::Archive => "MPD opens archives and walks inside; an SACD ISO is not ISO9660 and fails.",
            Class::Video => "MPD's ffmpeg decoder claims this suffix; usually video. Audio-only MP4 is normally .m4a.",
            Class::Audio => "A decoder claims it: a song to MPD.",
            Class::Cue => "A CUE sheet: MPD's cue playlist plugin reads it.",
            Class::Playlist => "A playlist: MPD's playlist plugins read it.",
            Class::Image => "Folder art and pictures; never suggested, Volumio shows them.",
            Class::Other => "Nothing claims it: one stat and MPD moves on.",
            Class::Folder => "A folder MPD walks into.",
        }
    }
}

/// The name patterns, compiled once.
pub struct Rules {
    system: Vec<Pattern>,
    sidecar: Vec<Pattern>,
}

impl Rules {
    pub fn new() -> Rules {
        Rules {
            system: SYSTEM_FOLDERS
                .iter()
                .filter_map(|p| Pattern::new(p))
                .collect(),
            sidecar: SIDECARS.iter().filter_map(|p| Pattern::new(p)).collect(),
        }
    }
}

impl Default for Rules {
    fn default() -> Self {
        Rules::new()
    }
}

/// The suffix as MPD takes it: what follows the last dot, lowercased; none without a
/// dot or with nothing after it.
pub fn suffix(name: &str) -> Option<String> {
    let (_, after) = name.rsplit_once('.')?;
    if after.is_empty() {
        return None;
    }
    Some(after.to_lowercase())
}

/// The class of a name, and the pattern that would exclude it when one is suggested.
pub fn classify(
    rules: &Rules,
    name: &OsStr,
    is_dir: bool,
    decoders: &BTreeSet<String>,
) -> (Class, Option<String>) {
    if is_dir {
        return match rules.system.iter().find(|p| p.matches(name)) {
            Some(p) => (Class::SystemFolder, Some(p.text().to_string())),
            None => (Class::Folder, None),
        };
    }
    if let Some(p) = rules.sidecar.iter().find(|p| p.matches(name)) {
        return (Class::Sidecar, Some(p.text().to_string()));
    }
    let text = name.to_string_lossy();
    let Some(s) = suffix(&text) else {
        return (Class::Other, None);
    };
    let s = s.as_str();
    if decoders.contains(s) {
        return if VIDEO_SUFFIXES.contains(&s) {
            (Class::Video, Some(format!("*.{s}")))
        } else {
            (Class::Audio, None)
        };
    }
    if ARCHIVE_SUFFIXES.contains(&s) {
        return (Class::Archive, Some(format!("*.{s}")));
    }
    if s == CUE_SUFFIX {
        return (Class::Cue, None);
    }
    if PLAYLIST_SUFFIXES.contains(&s) {
        return (Class::Playlist, None);
    }
    if IMAGE_SUFFIXES.contains(&s) {
        return (Class::Image, None);
    }
    (Class::Other, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoders() -> BTreeSet<String> {
        ["flac", "mp3", "mp4", "mkv", "mov", "dsf"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn class(name: &str, is_dir: bool) -> (Class, Option<String>) {
        classify(&Rules::new(), OsStr::new(name), is_dir, &decoders())
    }

    #[test]
    fn the_suffix_is_what_follows_the_last_dot_lowercased() {
        assert_eq!(suffix("Track.FLAC"), Some("flac".to_string()));
        assert_eq!(suffix(".mpdignore"), Some("mpdignore".to_string()));
        assert_eq!(suffix("a.b.c"), Some("c".to_string()));
        assert_eq!(suffix("noext"), None);
        assert_eq!(suffix("dot."), None);
    }

    #[test]
    fn folders_are_walked_unless_a_nas_or_a_desktop_made_them() {
        assert_eq!(class("Jazz", true), (Class::Folder, None));
        assert_eq!(
            class("#recycle", true),
            (Class::SystemFolder, Some("[#]recycle".to_string()))
        );
        assert_eq!(
            class("@eaDir", true),
            (Class::SystemFolder, Some("@eaDir".to_string()))
        );
        assert_eq!(
            class(".Trash-1000", true),
            (Class::SystemFolder, Some(".Trash*".to_string()))
        );
        assert_eq!(
            class("System Volume Information", true).0,
            Class::SystemFolder
        );
        assert_eq!(class("#recycle", false), (Class::Other, None));
    }

    #[test]
    fn files_fall_into_their_class_in_mpds_order() {
        assert_eq!(
            class("._01 Track.flac", false),
            (Class::Sidecar, Some("._*".to_string()))
        );
        assert_eq!(
            class(".DS_Store", false),
            (Class::Sidecar, Some(".DS_Store".to_string()))
        );
        assert_eq!(class("01 Track.flac", false), (Class::Audio, None));
        assert_eq!(class("01 Track.FLAC", false), (Class::Audio, None));
        assert_eq!(
            class("clip.mkv", false),
            (Class::Video, Some("*.mkv".to_string()))
        );
        assert_eq!(
            class("clip.MOV", false),
            (Class::Video, Some("*.mov".to_string()))
        );
        assert_eq!(
            class("album.iso", false),
            (Class::Archive, Some("*.iso".to_string()))
        );
        assert_eq!(class("album.cue", false), (Class::Cue, None));
        assert_eq!(class("album.m3u8", false), (Class::Playlist, None));
        assert_eq!(class("cover.jpg", false), (Class::Image, None));
        assert_eq!(class("album.txt", false), (Class::Other, None));
        assert_eq!(class("README", false), (Class::Other, None));
        // A video suffix no decoder claims is nothing to MPD.
        assert_eq!(class("clip.avi", false), (Class::Other, None));
    }
}
