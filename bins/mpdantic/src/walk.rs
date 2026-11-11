//! The walk every analysis shares: from the folder in hand down, one directory at a
//! time, the names in order, the `.mpdignore` lines in force respected before any
//! stat as MPD's own walk does, a depth cap, a cancel flag checked as it goes, the
//! progress counted for the page. What to do with a directory's entries, and which
//! folders to enter, is the visitor's.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde::Serialize;

use crate::ignore::{read_list, InForce};
use crate::library::Library;
use crate::text::Msg;

/// Folders nested deeper than this are not walked; the report says so.
const MAX_DEPTH: usize = 64;

/// How far the walk has come, for the page while it runs.
#[derive(Default)]
pub struct Progress {
    pub directories: AtomicUsize,
    pub files: AtomicUsize,
}

/// One name in a directory, with what a stat said of it.
pub struct Entry {
    pub name: OsString,
    /// The path in MPD's form.
    pub path: String,
    pub is_dir: bool,
    /// Bytes, for a file; 0 for a folder.
    pub size: u64,
}

/// A name a line in force keeps out: MPD never stats it, nor does the walk.
pub struct Excluded {
    pub path: String,
    pub line: String,
    /// The directory whose list has the line; "the root" for the music directory.
    pub from: Msg,
}

/// The music directory, as the page names it.
pub fn root() -> Msg {
    msg!("core.walk.root", "the root")
}

pub fn not_library_path() -> Msg {
    msg!("core.path.not_library", "not a library path")
}

pub fn no_such_folder() -> Msg {
    msg!("core.path.no_such_folder", "no such folder")
}

/// One directory as the visitor sees it.
pub struct Visit<'a> {
    pub dir: &'a Path,
    pub entries: &'a [Entry],
    pub excluded: &'a [Excluded],
}

/// The `.mpdignore` lines in force at a directory, for a report.
#[derive(Serialize, Debug)]
pub struct InForceLines {
    pub dir: String,
    pub lines: Vec<String>,
    /// `#` lines that look like names: comments to MPD, dead to their writer.
    pub dead: Vec<String>,
}

/// What the walk counted.
#[derive(Serialize, Debug, Default)]
pub struct Walked {
    pub directories: usize,
    pub files: usize,
    pub unreadable: usize,
    pub too_deep: usize,
    pub cancelled: bool,
}

/// The folder in hand checked and found, with the lines in force at it.
pub struct Start {
    pub dir: PathBuf,
    pub path: String,
    pub in_force: InForce,
}

/// The lists in force above and at the folder: the root's, then each step down.
pub fn start(music: &Path, path: &str) -> Result<Start, Msg> {
    let path = Library::check(path).ok_or_else(not_library_path)?;
    let mut in_force = InForce::default();
    if let Some(list) = read_list(music) {
        in_force.push(String::new(), list);
    }
    let mut dir = PathBuf::from(music);
    let mut rel = String::new();
    for component in path.split('/').filter(|c| !c.is_empty()) {
        dir.push(component);
        rel = join(&rel, component);
        if let Some(list) = read_list(&dir) {
            in_force.push(rel.clone(), list);
        }
    }
    if !dir.is_dir() {
        return Err(no_such_folder());
    }
    Ok(Start {
        dir,
        path,
        in_force,
    })
}

impl Start {
    pub fn in_force_lines(&self) -> Vec<InForceLines> {
        self.in_force
            .lines()
            .into_iter()
            .map(|(dir, lines, dead)| InForceLines { dir, lines, dead })
            .collect()
    }
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The walk from the start down. The visitor is given each directory's entries and
/// answers with the indexes of the folders to enter.
pub fn walk<F>(start: Start, progress: &Progress, cancel: &AtomicBool, mut visit: F) -> Walked
where
    F: FnMut(&Visit) -> Vec<usize>,
{
    let mut walked = Walked::default();
    let mut in_force = start.in_force;
    directory(
        &start.dir,
        &start.path,
        0,
        &mut in_force,
        progress,
        cancel,
        &mut visit,
        &mut walked,
    );
    walked.cancelled = cancel.load(Ordering::Relaxed);
    walked
}

#[allow(clippy::too_many_arguments)]
fn directory<F>(
    dir: &Path,
    rel: &str,
    depth: usize,
    in_force: &mut InForce,
    progress: &Progress,
    cancel: &AtomicBool,
    visit: &mut F,
    walked: &mut Walked,
) where
    F: FnMut(&Visit) -> Vec<usize>,
{
    if cancel.load(Ordering::Relaxed) {
        return;
    }
    if depth > MAX_DEPTH {
        walked.too_deep += 1;
        return;
    }
    let Ok(read) = fs::read_dir(dir) else {
        walked.unreadable += 1;
        return;
    };
    walked.directories += 1;
    progress.directories.fetch_add(1, Ordering::Relaxed);
    // One directory's names at a time, in order, so a report reads the same twice.
    let mut names: Vec<OsString> = read.flatten().map(|e| e.file_name()).collect();
    names.sort();
    let mut entries = Vec::new();
    let mut excluded = Vec::new();
    for name in names {
        let path = join(rel, &name.to_string_lossy());
        // The exclude check comes before the stat, as in MPD's walk.
        if let Some((from, line)) = in_force.matches(&name) {
            excluded.push(Excluded {
                path,
                line: line.to_string(),
                from: if from.is_empty() {
                    root()
                } else {
                    Msg::raw(from)
                },
            });
            continue;
        }
        let Ok(meta) = fs::metadata(dir.join(&name)) else {
            walked.unreadable += 1;
            continue;
        };
        if !meta.is_dir() {
            walked.files += 1;
            progress.files.fetch_add(1, Ordering::Relaxed);
        }
        entries.push(Entry {
            name,
            path,
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
        });
    }
    let enter = visit(&Visit {
        dir,
        entries: &entries,
        excluded: &excluded,
    });
    for index in enter {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Some(entry) = entries.get(index).filter(|e| e.is_dir) else {
            continue;
        };
        let child = dir.join(&entry.name);
        let pushed = match read_list(&child) {
            Some(list) => {
                in_force.push(entry.path.clone(), list);
                true
            }
            None => false,
        };
        directory(
            &child,
            &entry.path,
            depth + 1,
            in_force,
            progress,
            cancel,
            visit,
            walked,
        );
        if pushed {
            in_force.pop();
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A small tree: NAS/.mpdignore keeps *.opus out; NAS/Jazz/Live holds a file of
    /// each kind.
    pub fn temp_tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("mpdantic-walk-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("NAS/Jazz/Live")).unwrap();
        fs::create_dir_all(root.join("NAS/Skip")).unwrap();
        fs::write(root.join("NAS/.mpdignore"), "*.opus\nSkip\n#dead\n").unwrap();
        fs::write(root.join("NAS/Jazz/Live/b.flac"), [0u8; 20]).unwrap();
        fs::write(root.join("NAS/Jazz/Live/a.opus"), [0u8; 10]).unwrap();
        fs::write(root.join("NAS/Skip/x.flac"), [0u8; 10]).unwrap();
        root
    }

    #[test]
    fn the_walk_visits_in_order_keeps_excluded_names_out_before_the_stat_and_enters_what_the_visitor_says(
    ) {
        let root = temp_tree("visit");
        let start = start(&root, "NAS/").unwrap();
        assert_eq!(start.path, "NAS");
        let lines = start.in_force_lines();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].dir, "NAS");
        assert_eq!(lines[0].lines, ["*.opus", "Skip"]);
        assert_eq!(lines[0].dead, ["#dead"]);
        let mut seen: Vec<String> = Vec::new();
        let mut excluded: Vec<String> = Vec::new();
        let walked = walk(start, &Progress::default(), &AtomicBool::new(false), |v| {
            seen.push(format!(
                "{}: {}",
                v.dir.strip_prefix(&root).unwrap().to_string_lossy(),
                v.entries
                    .iter()
                    .map(|e| e.path.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            excluded.extend(
                v.excluded
                    .iter()
                    .map(|x| format!("{} ({} at {})", x.path, x.line, x.from)),
            );
            (0..v.entries.len()).collect()
        });
        assert_eq!(
            seen,
            [
                "NAS: NAS/.mpdignore,NAS/Jazz",
                "NAS/Jazz: NAS/Jazz/Live",
                "NAS/Jazz/Live: NAS/Jazz/Live/b.flac",
            ]
        );
        assert_eq!(
            excluded,
            [
                "NAS/Skip (Skip at NAS)",
                "NAS/Jazz/Live/a.opus (*.opus at NAS)"
            ]
        );
        assert_eq!(
            (walked.directories, walked.files, walked.unreadable),
            (3, 2, 0)
        );
        assert!(!walked.cancelled);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_visitor_that_enters_nothing_sees_one_directory_and_a_cancel_sees_none() {
        let root = temp_tree("stop");
        let walked = walk(
            start(&root, "").unwrap(),
            &Progress::default(),
            &AtomicBool::new(false),
            |_| vec![],
        );
        assert_eq!(walked.directories, 1);
        let walked = walk(
            start(&root, "").unwrap(),
            &Progress::default(),
            &AtomicBool::new(true),
            |_| vec![0],
        );
        assert_eq!(walked.directories, 0);
        assert!(walked.cancelled);
        assert_eq!(
            start(&root, "../x").err().as_deref(),
            Some("not a library path")
        );
        assert_eq!(
            start(&root, "NAS/Rock").err().as_deref(),
            Some("no such folder")
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
