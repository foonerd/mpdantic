//! Folder name suggestions from the folder's audio tags: album, albumartist (else
//! artist), for the leaf rename of the folder in hand. The safe form is `names::safe`.
//! Mixed album or artist tags offer no chip for that field (custom stays available).

use std::collections::BTreeMap;

use serde::Serialize;

use crate::names;
use crate::tagfix::FileValues;
use crate::text::Msg;

/// What the page shows before a folder rename preview.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Suggest {
    /// The folder as MPD sees it.
    pub path: String,
    /// The leaf name as it is on disk.
    pub leaf: String,
    /// Parent path (empty at a top-level folder under the music root).
    pub parent: String,
    /// Safe album when every non-empty album tag agrees and it differs from the leaf.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Safe albumartist (else artist) when tags agree and it differs from the leaf.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub albumartist: Option<String>,
    /// Why a chip is missing, when useful.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Msg>,
}

/// A library root, or a NAS share root: not renamed.
pub fn is_root_folder(path: &str) -> bool {
    if path.is_empty() {
        return true;
    }
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    matches!(
        parts.as_slice(),
        ["INTERNAL"] | ["USB"] | ["NAS"] | ["NAS", _]
    )
}

/// Parent path and leaf of an MPD folder path; None when the path is empty.
pub fn split_leaf(path: &str) -> Option<(String, String)> {
    if path.is_empty() {
        return None;
    }
    match path.rfind('/') {
        Some(i) => Some((path[..i].to_string(), path[i + 1..].to_string())),
        None => Some((String::new(), path.to_string())),
    }
}

/// Suggestions from the folder's audio tags for renaming its leaf.
pub fn suggest(path: &str, files: &[FileValues]) -> Result<Suggest, Msg> {
    let (parent, leaf) = split_leaf(path).ok_or_else(crate::walk::no_such_folder)?;
    if is_root_folder(path) {
        return Err(msg!("core.folder.root", "a library root is not renamed"));
    }
    let mut notes = Vec::new();
    let album = match agreed_field(files, "album") {
        FieldAgree::One(raw) => {
            let safe = names::safe(&raw);
            if safe.is_empty() {
                notes.push(msg!(
                    "core.folder.empty_safe",
                    "the album tag is empty after it is made safe"
                ));
                None
            } else if safe == leaf {
                None
            } else {
                Some(safe)
            }
        }
        FieldAgree::Mixed => {
            notes.push(msg!(
                "core.folder.mixed_album",
                "the folder's files carry more than one album tag"
            ));
            None
        }
        FieldAgree::None => None,
    };
    let albumartist = match agreed_albumartist(files) {
        FieldAgree::One(raw) => {
            let safe = names::safe(&raw);
            if safe.is_empty() {
                notes.push(msg!(
                    "core.folder.empty_safe_artist",
                    "the album artist tag is empty after it is made safe"
                ));
                None
            } else if safe == leaf {
                None
            } else {
                Some(safe)
            }
        }
        FieldAgree::Mixed => {
            notes.push(msg!(
                "core.folder.mixed_artist",
                "the folder's files carry more than one album artist"
            ));
            None
        }
        FieldAgree::None => None,
    };
    Ok(Suggest {
        path: path.to_string(),
        leaf,
        parent,
        album,
        albumartist,
        notes,
    })
}

enum FieldAgree {
    None,
    One(String),
    Mixed,
}

fn agreed_field(files: &[FileValues], field: &str) -> FieldAgree {
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    for f in files {
        let Some(raw) = f
            .values
            .get(field)
            .and_then(|v| v.first())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let key = raw.to_ascii_lowercase();
        keys.entry(key).or_insert_with(|| raw.to_string());
        if keys.len() > 1 {
            return FieldAgree::Mixed;
        }
    }
    match keys.into_values().next() {
        Some(v) => FieldAgree::One(v),
        None => FieldAgree::None,
    }
}

/// Albumartist when present and agreed; else artist when agreed.
fn agreed_albumartist(files: &[FileValues]) -> FieldAgree {
    match agreed_field(files, "albumartist") {
        FieldAgree::None => agreed_field(files, "artist"),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn file(album: Option<&str>, albumartist: Option<&str>, artist: Option<&str>) -> FileValues {
        let mut values = BTreeMap::new();
        if let Some(a) = album {
            values.insert("album".into(), vec![a.into()]);
        }
        if let Some(a) = albumartist {
            values.insert("albumartist".into(), vec![a.into()]);
        }
        if let Some(a) = artist {
            values.insert("artist".into(), vec![a.into()]);
        }
        FileValues {
            name: "01.mp3".into(),
            path: "INTERNAL/Peter Hollens/MR0XF1~N/01.mp3".into(),
            format: "mp3".into(),
            writable: true,
            values,
            problem: None,
        }
    }

    #[test]
    fn hollens_dos_leaf_offers_the_album() {
        let files = vec![
            file(
                Some("Misty Mountains: Songs Inspired by The Hobbit and Lord of the Rings"),
                Some("Peter Hollens"),
                Some("Peter Hollens feat. X"),
            ),
            file(
                Some("Misty Mountains: Songs Inspired by The Hobbit and Lord of the Rings"),
                Some("Peter Hollens"),
                Some("Peter Hollens"),
            ),
        ];
        let s = suggest("INTERNAL/Peter Hollens/MR0XF1~N", &files).unwrap();
        assert_eq!(s.leaf, "MR0XF1~N");
        assert_eq!(s.parent, "INTERNAL/Peter Hollens");
        assert_eq!(
            s.album.as_deref(),
            Some("Misty Mountains_ Songs Inspired by The Hobbit and Lord of the Rings")
        );
        assert_eq!(s.albumartist.as_deref(), Some("Peter Hollens"));
        assert!(is_root_folder("INTERNAL"));
        assert!(is_root_folder("NAS/multimedia"));
        assert!(!is_root_folder("INTERNAL/Peter Hollens/MR0XF1~N"));
    }

    #[test]
    fn mixed_album_tags_offer_no_album_chip() {
        let files = vec![
            file(Some("Hits"), Some("Phil Collins"), None),
            file(Some("But Seriously"), Some("Phil Collins"), None),
        ];
        let s = suggest("INTERNAL/Phil Collins/Dump", &files).unwrap();
        assert!(s.album.is_none());
        assert_eq!(s.albumartist.as_deref(), Some("Phil Collins"));
        assert!(s.notes.iter().any(|n| n.k == "core.folder.mixed_album"));
    }

    #[test]
    fn a_root_folder_refuses() {
        let err = suggest("INTERNAL", &[]).unwrap_err();
        assert_eq!(err.k, "core.folder.root");
    }
}
