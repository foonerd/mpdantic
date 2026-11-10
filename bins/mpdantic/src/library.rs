//! The library as MPD sees it. The music directory's entries are the roots (on
//! Volumio, `/var/lib/mpd/music` holds links to `/mnt/NAS`, `/mnt/USB` and
//! `/mnt/INTERNAL`), and a path under it is in MPD's own form, `NAS/Jazz/...`. One
//! directory is read at a time, on request, in pages: never the whole tree.

use serde::Serialize;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Entries in one reply unless asked otherwise, and the most that can be asked.
pub const LIMIT_DEFAULT: usize = 500;
pub const LIMIT_MAX: usize = 2000;

pub struct Library {
    music: PathBuf,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    /// Bytes, for a file; 0 for a folder.
    pub size: u64,
}

/// One page of one directory: folders first, then files, each by name.
#[derive(Serialize, Debug)]
pub struct Listing {
    pub path: String,
    pub entries: Vec<Entry>,
    pub total: usize,
    pub offset: usize,
    pub more: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ListError {
    BadPath,
    NotFound,
    NotADirectory,
    Io(String),
}

impl Library {
    pub fn new(music: PathBuf) -> Library {
        Library { music }
    }

    /// MPD's music directory.
    pub fn music(&self) -> &Path {
        &self.music
    }

    /// A path in MPD's form, checked and trimmed: relative, no empty, `.` or `..`
    /// component, nothing that walks out of the library. Empty is the root.
    pub fn check(path: &str) -> Option<String> {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            return Some(String::new());
        }
        if trimmed.starts_with('/')
            || trimmed.contains('\0')
            || trimmed
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return None;
        }
        Some(trimmed.to_string())
    }

    /// One page of the directory at `path`.
    pub fn list(&self, path: &str, offset: usize, limit: usize) -> Result<Listing, ListError> {
        let path = Library::check(path).ok_or(ListError::BadPath)?;
        let dir = if path.is_empty() {
            self.music.clone()
        } else {
            self.music.join(&path)
        };
        let read = fs::read_dir(&dir).map_err(|e| match e.kind() {
            ErrorKind::NotFound => ListError::NotFound,
            ErrorKind::NotADirectory => ListError::NotADirectory,
            _ => ListError::Io(e.to_string()),
        })?;
        let mut entries: Vec<Entry> = read
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                // metadata follows a link: a linked folder is a folder, as MPD has it.
                match fs::metadata(e.path()) {
                    Ok(m) if m.is_dir() => Entry {
                        name,
                        dir: true,
                        size: 0,
                    },
                    Ok(m) => Entry {
                        name,
                        dir: false,
                        size: m.len(),
                    },
                    Err(_) => Entry {
                        name,
                        dir: false,
                        size: 0,
                    },
                }
            })
            .collect();
        entries.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.name.cmp(&b.name))
        });
        let total = entries.len();
        let limit = limit.clamp(1, LIMIT_MAX);
        let offset = offset.min(total);
        let end = (offset + limit).min(total);
        let page: Vec<Entry> = entries
            .into_iter()
            .skip(offset)
            .take(end - offset)
            .collect();
        Ok(Listing {
            path,
            entries: page,
            total,
            offset,
            more: end < total,
        })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A library of its own for a test: NAS/Jazz with two files and a dot file, USB empty.
    pub fn temp_library(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mpdantic-lib-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("NAS/Jazz")).unwrap();
        fs::create_dir_all(root.join("USB")).unwrap();
        fs::write(root.join("NAS/Jazz/b.flac"), [0u8; 2048]).unwrap();
        fs::write(root.join("NAS/Jazz/A.flac"), [0u8; 1024]).unwrap();
        fs::write(root.join("NAS/Jazz/.mpdignore"), "*.iso\n").unwrap();
        fs::create_dir_all(root.join("NAS/Jazz/Live")).unwrap();
        root
    }

    #[test]
    fn a_path_is_relative_with_no_empty_dot_or_parent_component() {
        assert_eq!(Library::check(""), Some(String::new()));
        assert_eq!(Library::check("/"), Some(String::new()));
        assert_eq!(Library::check("NAS/Jazz/"), Some("NAS/Jazz".to_string()));
        assert_eq!(Library::check("NAS/Jazz"), Some("NAS/Jazz".to_string()));
        assert_eq!(Library::check("/NAS"), None);
        assert_eq!(Library::check("NAS/../etc"), None);
        assert_eq!(Library::check(".."), None);
        assert_eq!(Library::check("./NAS"), None);
        assert_eq!(Library::check("NAS//Jazz"), None);
        assert_eq!(Library::check("NAS/\0"), None);
    }

    #[test]
    fn the_root_lists_the_roots_and_a_folder_lists_folders_first_then_files_by_name() {
        let root = temp_library("list");
        let library = Library::new(root.clone());
        let roots = library.list("", 0, LIMIT_DEFAULT).unwrap();
        assert_eq!(roots.path, "");
        assert_eq!(
            roots
                .entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["NAS", "USB"]
        );
        assert!(roots.entries.iter().all(|e| e.dir));
        let jazz = library.list("NAS/Jazz/", 0, LIMIT_DEFAULT).unwrap();
        assert_eq!(jazz.path, "NAS/Jazz");
        assert_eq!(
            jazz.entries,
            vec![
                Entry {
                    name: "Live".into(),
                    dir: true,
                    size: 0
                },
                Entry {
                    name: ".mpdignore".into(),
                    dir: false,
                    size: 6
                },
                Entry {
                    name: "A.flac".into(),
                    dir: false,
                    size: 1024
                },
                Entry {
                    name: "b.flac".into(),
                    dir: false,
                    size: 2048
                },
            ]
        );
        assert_eq!((jazz.total, jazz.offset, jazz.more), (4, 0, false));
        assert!(library
            .list("USB", 0, LIMIT_DEFAULT)
            .unwrap()
            .entries
            .is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_folder_is_read_in_pages() {
        let root = temp_library("pages");
        let library = Library::new(root.clone());
        let first = library.list("NAS/Jazz", 0, 3).unwrap();
        assert_eq!(first.entries.len(), 3);
        assert_eq!((first.total, first.offset, first.more), (4, 0, true));
        let second = library.list("NAS/Jazz", 3, 3).unwrap();
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.entries[0].name, "b.flac");
        assert_eq!((second.total, second.offset, second.more), (4, 3, false));
        let past = library.list("NAS/Jazz", 10, 3).unwrap();
        assert!(past.entries.is_empty());
        assert_eq!((past.offset, past.more), (4, false));
        assert_eq!(library.list("NAS/Jazz", 0, 0).unwrap().entries.len(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_bad_path_a_missing_folder_and_a_file_are_refused_by_kind() {
        let root = temp_library("errors");
        let library = Library::new(root.clone());
        assert_eq!(library.list("../x", 0, 10).unwrap_err(), ListError::BadPath);
        assert_eq!(
            library.list("NAS/Rock", 0, 10).unwrap_err(),
            ListError::NotFound
        );
        assert_eq!(
            library.list("NAS/Jazz/A.flac", 0, 10).unwrap_err(),
            ListError::NotADirectory
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
