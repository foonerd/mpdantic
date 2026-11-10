//! The `.mpdignore` analysis of one folder and everything under it, read-only: what
//! MPD's walk would meet there by class, the lines already in force, and the lines
//! that would keep the junk out. One directory at a time; counts and a handful of
//! examples, never a list of every file.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde::Serialize;

use crate::classify::{self, Class, Rules};
use crate::ignore::{read_list, InForce};
use crate::library::Library;
use crate::mpd::Decoders;

/// Examples kept per class and per suggestion.
const EXAMPLES: usize = 5;
/// Distinct suffixes counted under Other before the rest are lumped together.
const OTHER_SUFFIXES: usize = 200;
/// Suffixes of Other shown, the most frequent first.
const OTHER_SHOWN: usize = 20;
/// Folders nested deeper than this are not walked; the report says so.
const MAX_DEPTH: usize = 64;

/// How far the walk has come, for the page while it runs.
#[derive(Default)]
pub struct Progress {
    pub directories: AtomicUsize,
    pub files: AtomicUsize,
}

#[derive(Serialize, Default, Debug)]
pub struct Count {
    pub count: usize,
    pub bytes: u64,
    pub examples: Vec<String>,
}

impl Count {
    fn add(&mut self, path: &str, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
        if self.examples.len() < EXAMPLES {
            self.examples.push(path.to_string());
        }
    }
}

/// A line that would keep a class of junk out, at the folder analysed.
#[derive(Serialize, Debug)]
pub struct Suggestion {
    pub pattern: String,
    pub class: Class,
    pub count: usize,
    pub bytes: u64,
    pub examples: Vec<String>,
    pub note: &'static str,
}

#[derive(Serialize, Debug)]
pub struct InForceLines {
    pub dir: String,
    pub lines: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct DecoderNote {
    /// `mpd` when the running MPD answered, `assumed` when it could not be asked.
    pub source: String,
    pub count: usize,
    pub detail: String,
}

#[derive(Serialize, Debug)]
pub struct Report {
    pub path: String,
    pub directories: usize,
    pub files: usize,
    pub unreadable: usize,
    pub too_deep: usize,
    /// Names a line in force already keeps out.
    pub excluded: Count,
    pub classes: BTreeMap<Class, Count>,
    pub audio_suffixes: BTreeMap<String, usize>,
    pub other_suffixes: Vec<(String, usize)>,
    pub in_force: Vec<InForceLines>,
    pub suggestions: Vec<Suggestion>,
    pub decoders: DecoderNote,
    pub cancelled: bool,
}

struct Walk<'a> {
    rules: Rules,
    decoders: &'a Decoders,
    progress: &'a Progress,
    cancel: &'a AtomicBool,
    in_force: InForce,
    report: Report,
    other_suffixes: BTreeMap<String, usize>,
    other_lumped: usize,
    suggestions: BTreeMap<String, Suggestion>,
}

/// The analysis of `path` (MPD's form) under the music directory.
pub fn analyse(
    music: &Path,
    path: &str,
    decoders: &Decoders,
    source: DecoderNote,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<Report, String> {
    let path = Library::check(path).ok_or_else(|| "not a library path".to_string())?;
    // The lists in force above and at the folder: the root's, then each step down.
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
        return Err("no such folder".to_string());
    }
    let mut walk = Walk {
        rules: Rules::new(),
        decoders,
        progress,
        cancel,
        report: Report {
            path: path.clone(),
            directories: 0,
            files: 0,
            unreadable: 0,
            too_deep: 0,
            excluded: Count::default(),
            classes: BTreeMap::new(),
            audio_suffixes: BTreeMap::new(),
            other_suffixes: Vec::new(),
            in_force: in_force
                .lines()
                .into_iter()
                .map(|(dir, lines)| InForceLines { dir, lines })
                .collect(),
            suggestions: Vec::new(),
            decoders: source,
            cancelled: false,
        },
        in_force,
        other_suffixes: BTreeMap::new(),
        other_lumped: 0,
        suggestions: BTreeMap::new(),
    };
    walk.directory(&dir, &path, 0);
    let mut report = walk.report;
    report.cancelled = cancel.load(Ordering::Relaxed);
    let mut other: Vec<(String, usize)> = walk.other_suffixes.into_iter().collect();
    other.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    other.truncate(OTHER_SHOWN);
    if walk.other_lumped > 0 {
        other.push(("(other suffixes)".to_string(), walk.other_lumped));
    }
    report.other_suffixes = other;
    let mut suggestions: Vec<Suggestion> = walk.suggestions.into_values().collect();
    suggestions.sort_by(|a, b| a.class.cmp(&b.class).then_with(|| b.count.cmp(&a.count)));
    report.suggestions = suggestions;
    Ok(report)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

impl Walk<'_> {
    fn directory(&mut self, dir: &Path, rel: &str, depth: usize) {
        if self.cancel.load(Ordering::Relaxed) {
            return;
        }
        if depth > MAX_DEPTH {
            self.report.too_deep += 1;
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            self.report.unreadable += 1;
            return;
        };
        self.report.directories += 1;
        self.progress.directories.fetch_add(1, Ordering::Relaxed);
        // One directory's names at a time, in order, so a report reads the same twice.
        let mut entries: Vec<fs::DirEntry> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            let name = entry.file_name();
            let path = join(rel, &name.to_string_lossy());
            // The exclude check comes before the stat, as in MPD's walk.
            if let Some((from, line)) = self.in_force.matches(&name) {
                let from = if from.is_empty() { "the root" } else { from };
                self.report
                    .excluded
                    .add(&format!("{path} ({line} at {from})"), 0);
                continue;
            }
            let Ok(meta) = fs::metadata(entry.path()) else {
                self.report.unreadable += 1;
                continue;
            };
            let (class, pattern) =
                classify::classify(&self.rules, &name, meta.is_dir(), &self.decoders.suffixes);
            if class == Class::Folder {
                let child = entry.path();
                let pushed = match read_list(&child) {
                    Some(list) => {
                        self.in_force.push(path.clone(), list);
                        true
                    }
                    None => false,
                };
                self.directory(&child, &path, depth + 1);
                if pushed {
                    self.in_force.pop();
                }
                continue;
            }
            let bytes = if meta.is_dir() { 0 } else { meta.len() };
            if !meta.is_dir() {
                self.report.files += 1;
                self.progress.files.fetch_add(1, Ordering::Relaxed);
            }
            self.report
                .classes
                .entry(class)
                .or_default()
                .add(&path, bytes);
            match class {
                Class::Audio => {
                    if let Some(s) = classify::suffix(&name.to_string_lossy()) {
                        *self.report.audio_suffixes.entry(s).or_insert(0) += 1;
                    }
                }
                Class::Other => {
                    let s = classify::suffix(&name.to_string_lossy())
                        .unwrap_or_else(|| "(none)".to_string());
                    if self.other_suffixes.len() < OTHER_SUFFIXES
                        || self.other_suffixes.contains_key(&s)
                    {
                        *self.other_suffixes.entry(s).or_insert(0) += 1;
                    } else {
                        self.other_lumped += 1;
                    }
                }
                _ => {}
            }
            if let Some(pattern) = pattern {
                let suggestion =
                    self.suggestions
                        .entry(pattern.clone())
                        .or_insert_with(|| Suggestion {
                            pattern,
                            class,
                            count: 0,
                            bytes: 0,
                            examples: Vec::new(),
                            note: class.note(),
                        });
                suggestion.count += 1;
                suggestion.bytes += bytes;
                if suggestion.examples.len() < EXAMPLES {
                    suggestion.examples.push(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpd;

    /// A library with the reporter's kinds of trouble in it.
    fn library(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("mpdantic-analyse-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mk = |p: &str| fs::create_dir_all(root.join(p)).unwrap();
        let put = |p: &str, n: usize| fs::write(root.join(p), vec![0u8; n]).unwrap();
        mk("NAS/Pop/Genesis/_SACD");
        mk("NAS/Pop/#recycle/deep");
        mk("NAS/Pop/@eaDir");
        mk("NAS/Jazz/Live");
        put("NAS/Pop/Genesis/01 Track.flac", 100);
        put("NAS/Pop/Genesis/02 Track.FLAC", 100);
        put("NAS/Pop/Genesis/._01 Track.flac", 4);
        put("NAS/Pop/Genesis/clip.mov", 500);
        put("NAS/Pop/Genesis/cover.jpg", 50);
        put("NAS/Pop/Genesis/album.cue", 10);
        put("NAS/Pop/Genesis/album.m3u", 10);
        put("NAS/Pop/Genesis/notes.txt", 10);
        put("NAS/Pop/Genesis/_SACD/album.iso", 1000);
        put("NAS/Pop/#recycle/deep/old.flac", 100);
        put("NAS/Jazz/Live/03.mp3", 100);
        put("NAS/Jazz/Live/skip.iso", 100);
        fs::write(root.join("NAS/.mpdignore"), "# Jazz is clean\n*.opus\n").unwrap();
        fs::write(root.join("NAS/Jazz/.mpdignore"), "skip.*\n").unwrap();
        put("NAS/Jazz/Live/x.opus", 100);
        root
    }

    fn decoders() -> Decoders {
        let mut d = mpd::assumed();
        d.suffixes.insert("mov".to_string());
        d
    }

    fn note() -> DecoderNote {
        DecoderNote {
            source: "test".to_string(),
            count: 0,
            detail: String::new(),
        }
    }

    #[test]
    fn the_walk_counts_every_class_and_suggests_the_lines_that_keep_the_junk_out() {
        let root = library("report");
        let progress = Progress::default();
        let cancel = AtomicBool::new(false);
        let r = analyse(&root, "NAS/Pop", &decoders(), note(), &progress, &cancel).unwrap();
        assert_eq!(r.path, "NAS/Pop");
        // Pop, Genesis, _SACD; not #recycle or @eaDir, which are not walked into.
        assert_eq!(r.directories, 3);
        assert_eq!(r.files, 9);
        assert_eq!(progress.directories.load(Ordering::Relaxed), 3);
        let count = |c: Class| r.classes.get(&c).map(|x| x.count).unwrap_or(0);
        assert_eq!(count(Class::Audio), 2);
        assert_eq!(count(Class::Sidecar), 1);
        assert_eq!(count(Class::Video), 1);
        assert_eq!(count(Class::Image), 1);
        assert_eq!(count(Class::Cue), 1);
        assert_eq!(count(Class::Playlist), 1);
        assert_eq!(count(Class::Other), 1);
        assert_eq!(count(Class::Archive), 1);
        assert_eq!(count(Class::SystemFolder), 2);
        assert_eq!(r.audio_suffixes.get("flac"), Some(&2));
        assert_eq!(r.other_suffixes, vec![("txt".to_string(), 1)]);
        assert_eq!(
            r.classes[&Class::Archive].examples,
            ["NAS/Pop/Genesis/_SACD/album.iso"]
        );
        let patterns: Vec<(&str, usize)> = r
            .suggestions
            .iter()
            .map(|s| (s.pattern.as_str(), s.count))
            .collect();
        assert_eq!(
            patterns,
            [
                ("@eaDir", 1),
                ("[#]recycle", 1),
                ("._*", 1),
                ("*.iso", 1),
                ("*.mov", 1)
            ]
        );
        assert_eq!(r.suggestions[3].bytes, 1000);
        assert_eq!(r.suggestions[4].note, Class::Video.note());
        // The root's list is in force at NAS/Pop: NAS/.mpdignore.
        assert_eq!(r.in_force.len(), 1);
        assert_eq!(r.in_force[0].dir, "NAS");
        assert_eq!(r.in_force[0].lines, ["*.opus"]);
        assert_eq!(r.excluded.count, 0);
        assert!(!r.cancelled);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_line_in_force_keeps_a_name_out_before_it_is_looked_at() {
        let root = library("excluded");
        let r = analyse(
            &root,
            "NAS/Jazz",
            &decoders(),
            note(),
            &Progress::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(r.excluded.count, 2);
        assert_eq!(
            r.excluded.examples,
            [
                "NAS/Jazz/Live/skip.iso (skip.* at NAS/Jazz)",
                "NAS/Jazz/Live/x.opus (*.opus at NAS)"
            ]
        );
        assert!(r.suggestions.is_empty());
        assert_eq!(r.in_force.len(), 2);
        assert_eq!(r.in_force[1].dir, "NAS/Jazz");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_bad_path_and_a_missing_folder_are_refused_and_a_cancel_stops_the_walk() {
        let root = library("errors");
        let d = decoders();
        assert_eq!(
            analyse(
                &root,
                "../x",
                &d,
                note(),
                &Progress::default(),
                &AtomicBool::new(false)
            )
            .unwrap_err(),
            "not a library path"
        );
        assert_eq!(
            analyse(
                &root,
                "NAS/Rock",
                &d,
                note(),
                &Progress::default(),
                &AtomicBool::new(false)
            )
            .unwrap_err(),
            "no such folder"
        );
        let r = analyse(
            &root,
            "NAS",
            &d,
            note(),
            &Progress::default(),
            &AtomicBool::new(true),
        )
        .unwrap();
        assert!(r.cancelled);
        assert_eq!(r.directories, 0);
        fs::remove_dir_all(&root).unwrap();
    }
}
