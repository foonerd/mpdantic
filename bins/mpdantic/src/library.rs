//! The library as MPD sees it. The music directory's entries are the roots (on
//! Volumio, `/var/lib/mpd/music` holds links to `/mnt/NAS`, `/mnt/USB` and
//! `/mnt/INTERNAL`), and a path under it is in MPD's own form, `NAS/Jazz/...`. One
//! directory is read at a time, on request, in pages: never the whole tree.
//!
//! Lexical checks alone are not enough: a symlink under the library that points
//! outside would still join cleanly. After join, the resolved path must sit under
//! the music directory itself or under one of the three Volumio roots (NAS, USB,
//! INTERNAL); any other escape is refused.

use serde::Serialize;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Entries in one reply unless asked otherwise, and the most that can be asked.
pub const LIMIT_DEFAULT: usize = 500;
pub const LIMIT_MAX: usize = 2000;

/// First-segment names Volumio links out of the music directory to `/mnt/...`.
/// Only these may resolve outside the music directory's own real path.
const VOLUMIO_ROOTS: &[&str] = &["NAS", "USB", "INTERNAL"];

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

    /// The absolute path of an MPD path under `music`, after resolving links.
    /// Returns the checked MPD path and the path under `music` (not the
    /// canonical target), so Volumio's NAS view and the write mount stay as they
    /// are. Refuses a path whose resolved target leaves the music directory or a
    /// Volumio root.
    pub fn resolve(music: &Path, path: &str) -> Result<(String, PathBuf), ListError> {
        let path = Library::check(path).ok_or(ListError::BadPath)?;
        let joined = if path.is_empty() {
            music.to_path_buf()
        } else {
            music.join(&path)
        };
        let resolved = resolve_existing(&joined).map_err(|e| match e.kind() {
            ErrorKind::NotFound => ListError::NotFound,
            ErrorKind::NotADirectory => ListError::NotADirectory,
            _ => ListError::Io(e.to_string()),
        })?;
        if !is_confined(music, &path, &resolved) {
            return Err(ListError::BadPath);
        }
        Ok((path, joined))
    }

    /// One page of the directory at `path`.
    pub fn list(&self, path: &str, offset: usize, limit: usize) -> Result<Listing, ListError> {
        let (path, dir) = Library::resolve(&self.music, path)?;
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

/// Canonical form of `path`, or of the longest existing prefix with the missing
/// tail components appended. A write may name a file that is not there yet.
fn resolve_existing(path: &Path) -> std::io::Result<PathBuf> {
    if let Ok(resolved) = fs::canonicalize(path) {
        return Ok(resolved);
    }
    let mut suffix: Vec<OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match fs::canonicalize(&cur) {
            Ok(resolved) => {
                let mut out = resolved;
                for part in suffix.iter().rev() {
                    out.push(part);
                }
                return Ok(out);
            }
            Err(e) => match cur.file_name() {
                Some(name) if cur.parent().is_some() => {
                    suffix.push(name.to_os_string());
                    cur.pop();
                }
                _ => return Err(e),
            },
        }
    }
}

fn is_under(child: &Path, root: &Path) -> bool {
    child == root || child.starts_with(root)
}

/// The resolved path stays under the music directory, or under the real path of
/// NAS / USB / INTERNAL when that first segment is one of Volumio's roots.
fn is_confined(music: &Path, rel: &str, resolved: &Path) -> bool {
    let Ok(music_real) = fs::canonicalize(music) else {
        return false;
    };
    if is_under(resolved, &music_real) {
        return true;
    }
    let Some(first) = rel.split('/').next().filter(|c| !c.is_empty()) else {
        return false;
    };
    if !VOLUMIO_ROOTS.contains(&first) {
        return false;
    }
    let Ok(root_real) = fs::canonicalize(music.join(first)) else {
        return false;
    };
    is_under(resolved, &root_real)
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

    #[test]
    fn a_symlink_that_leaves_the_library_is_refused_and_a_volumio_root_is_not() {
        let root =
            std::env::temp_dir().join(format!("mpdantic-lib-confine-{}", std::process::id()));
        let outside =
            std::env::temp_dir().join(format!("mpdantic-lib-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(root.join("INTERNAL/Jazz")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"no").unwrap();
        // A planted link at the music root, and one under INTERNAL.
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("INTERNAL/out")).unwrap();
        // Volumio's shape: NAS is a link to a folder that is not under music.
        let nas_real =
            std::env::temp_dir().join(format!("mpdantic-lib-nas-{}", std::process::id()));
        let _ = fs::remove_dir_all(&nas_real);
        fs::create_dir_all(nas_real.join("Album")).unwrap();
        fs::write(nas_real.join("Album/a.flac"), b"x").unwrap();
        std::os::unix::fs::symlink(&nas_real, root.join("NAS")).unwrap();

        assert_eq!(
            Library::resolve(&root, "escape/secret.txt").unwrap_err(),
            ListError::BadPath
        );
        assert_eq!(
            Library::resolve(&root, "INTERNAL/out/secret.txt").unwrap_err(),
            ListError::BadPath
        );
        assert_eq!(
            Library::resolve(&root, "INTERNAL/Jazz").unwrap().0,
            "INTERNAL/Jazz"
        );
        let (path, joined) = Library::resolve(&root, "NAS/Album/a.flac").unwrap();
        assert_eq!(path, "NAS/Album/a.flac");
        assert_eq!(joined, root.join("NAS/Album/a.flac"));
        // A file not there yet under a confined folder is still resolved.
        assert_eq!(
            Library::resolve(&root, "INTERNAL/Jazz/new.flac").unwrap().0,
            "INTERNAL/Jazz/new.flac"
        );

        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
        fs::remove_dir_all(&nas_real).unwrap();
    }
}
