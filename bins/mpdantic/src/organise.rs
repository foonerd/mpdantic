//! Album groups in a dump or mix folder: the same grouping MusicBrainz and the
//! Organise block share, each with the concrete library path those files should
//! move to (`{anchor}/{safe artist}/{safe album}`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use crate::mpd::Decoders;
use crate::names;
use crate::tagfix::{self, FileValues};
use crate::text::Msg;
use crate::walk;

/// One album's files in the folder in hand, and where they should live.
#[derive(Serialize, Clone, Debug)]
pub struct AlbumGroup {
    pub album: String,
    pub album_artist: String,
    pub files: usize,
    /// Where those files should live: `{library anchor}/{artist}/{album}`.
    pub folder: String,
    /// Audio file names in the source folder (leaf names only).
    pub names: Vec<String>,
}

/// Groups under `path` for the page's Organise scan and MB mix guide.
pub fn groups_for(music: &Path, path: &str, decoders: &Decoders) -> Result<Vec<AlbumGroup>, Msg> {
    let dir = if path.is_empty() {
        music.to_path_buf()
    } else {
        music.join(path)
    };
    if !dir.is_dir() {
        return Err(walk::no_such_folder());
    }
    let files = tagfix::folder_values(&dir, path, decoders)?;
    Ok(album_groups_for_folder(path, &files))
}

/// The library share this folder sits under: `INTERNAL`, `NAS/Audio`, `USB/…`.
pub fn library_anchor(folder_path: &str) -> String {
    let parts: Vec<&str> = folder_path.split('/').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => String::new(),
        ["NAS", share, ..] | ["USB", share, ..] => format!("{}/{}", parts[0], share),
        [root, ..] => (*root).to_string(),
    }
}

pub fn album_folder_path(anchor: &str, album_artist: &str, album: &str) -> String {
    let artist = {
        let s = names::safe(album_artist);
        if s.is_empty() {
            "Unknown Artist".into()
        } else {
            s
        }
    };
    let album = {
        let s = names::safe(album);
        if s.is_empty() {
            "Unknown Album".into()
        } else {
            s
        }
    };
    if anchor.is_empty() {
        format!("{artist}/{album}")
    } else {
        format!("{anchor}/{artist}/{album}")
    }
}

/// Groups the folder's files by album tag (empty album is its own group), each
/// with the concrete library path those files should move to.
pub fn album_groups_for_folder(folder_path: &str, files: &[FileValues]) -> Vec<AlbumGroup> {
    let anchor = library_anchor(folder_path);
    album_groups(files)
        .into_iter()
        .map(|mut g| {
            g.folder = album_folder_path(&anchor, &g.album_artist, &g.album);
            g
        })
        .collect()
}

fn first_str<'a>(f: &'a FileValues, key: &str) -> Option<&'a str> {
    f.values
        .get(key)
        .and_then(|v| v.first())
        .map(String::as_str)
}

struct GroupAcc {
    album: String,
    artists: BTreeMap<String, usize>,
    names: Vec<String>,
}

fn album_groups(files: &[FileValues]) -> Vec<AlbumGroup> {
    // key → display album, artist votes, names
    let mut map: BTreeMap<String, GroupAcc> = BTreeMap::new();
    for f in files {
        let album = first_str(f, "album")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let key = album.to_ascii_lowercase();
        let artist = first_str(f, "albumartist")
            .or_else(|| first_str(f, "artist"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let entry = map.entry(key).or_insert_with(|| GroupAcc {
            album: album.to_string(),
            artists: BTreeMap::new(),
            names: Vec::new(),
        });
        if entry.album.is_empty() && !album.is_empty() {
            entry.album = album.to_string();
        }
        if !artist.is_empty() {
            *entry.artists.entry(artist).or_insert(0) += 1;
        }
        entry.names.push(f.name.clone());
    }
    map.into_values()
        .map(|g| {
            let album_artist = g
                .artists
                .into_iter()
                .max_by_key(|(_, n)| *n)
                .map(|(a, _)| a)
                .unwrap_or_default();
            let files = g.names.len();
            AlbumGroup {
                album: g.album,
                album_artist,
                files,
                folder: String::new(),
                names: g.names,
            }
        })
        .collect()
}

/// More than one distinct non-empty album tag (for MusicBrainz map).
pub fn mixed_named_albums(files: &[FileValues]) -> bool {
    album_groups(files)
        .into_iter()
        .filter(|g| !g.album.is_empty())
        .count()
        > 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn file(name: &str, tags: BTreeMap<String, Vec<String>>) -> FileValues {
        FileValues {
            name: name.into(),
            path: name.into(),
            format: "flac".into(),
            writable: true,
            values: tags,
            problem: None,
        }
    }

    #[test]
    fn groups_carry_concrete_paths_and_names() {
        let files = vec![
            file(
                "a.flac",
                BTreeMap::from([
                    ("album".into(), vec!["Hits".into()]),
                    ("albumartist".into(), vec!["Phil Collins".into()]),
                ]),
            ),
            file(
                "b.flac",
                BTreeMap::from([
                    ("album".into(), vec!["But Seriously".into()]),
                    ("albumartist".into(), vec!["Phil Collins".into()]),
                ]),
            ),
            file(
                "c.flac",
                BTreeMap::from([("album".into(), vec!["Hits".into()])]),
            ),
        ];
        let groups = album_groups_for_folder("INTERNAL/Phil Collins/Phil Collins", &files);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].album, "But Seriously");
        assert_eq!(groups[0].files, 1);
        assert_eq!(groups[0].folder, "INTERNAL/Phil Collins/But Seriously");
        assert_eq!(groups[0].names, vec!["b.flac"]);
        assert_eq!(groups[1].album, "Hits");
        assert_eq!(groups[1].files, 2);
        assert_eq!(groups[1].folder, "INTERNAL/Phil Collins/Hits");
        assert_eq!(
            album_folder_path("NAS/Audio", "Albert Hammond Jr.", "Yours To Keep"),
            "NAS/Audio/Albert Hammond Jr/Yours To Keep"
        );
    }
}
