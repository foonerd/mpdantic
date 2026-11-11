//! The `.mpdignore` analysis of one folder and everything under it, read-only: what
//! MPD's walk would meet there by class, the lines already in force, and the lines
//! that would keep the junk out. Counts and a handful of examples, never a list of
//! every file.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde::Serialize;

use crate::classify::{self, Class, Rules};
use crate::mpd::Decoders;
use crate::text::Msg;
use crate::walk::{self, InForceLines, Progress, Walked};

/// Examples kept per class and per suggestion.
const EXAMPLES: usize = 5;
/// Distinct suffixes counted under Other before the rest are lumped together.
const OTHER_SUFFIXES: usize = 200;
/// Suffixes of Other shown, the most frequent first.
const OTHER_SHOWN: usize = 20;

/// A count with a handful of examples: paths as they are, or a sentence about one.
#[derive(Serialize, Default, Debug)]
pub struct Count {
    pub count: usize,
    pub bytes: u64,
    pub examples: Vec<Msg>,
}

impl Count {
    pub(crate) fn add(&mut self, path: &str, bytes: u64) {
        self.add_example(Msg::raw(path), bytes);
    }

    pub(crate) fn add_example(&mut self, example: Msg, bytes: u64) {
        self.count += 1;
        self.bytes += bytes;
        if self.examples.len() < EXAMPLES {
            self.examples.push(example);
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
    pub note: Msg,
}

#[derive(Serialize, Debug)]
pub struct DecoderNote {
    /// `mpd` when the running MPD answered, `assumed` when it could not be asked.
    pub source: String,
    pub count: usize,
    /// What MPD said of itself, or why it could not be asked.
    pub detail: Msg,
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
    /// What ffmpeg alone claims, by suffix.
    pub ffmpeg_suffixes: BTreeMap<String, usize>,
    pub other_suffixes: Vec<(String, usize)>,
    /// Up to two names per other suffix, so an offer explains itself.
    pub other_examples: BTreeMap<String, Vec<String>>,
    pub in_force: Vec<InForceLines>,
    pub suggestions: Vec<Suggestion>,
    pub decoders: DecoderNote,
    /// Video containers are wanted here (Glass plays them, or the owner said so):
    /// counted, never suggested.
    pub video_in_use: bool,
    pub cancelled: bool,
}

/// The analysis of `path` (MPD's form) under the music directory.
/// What follows the last dot of a name, as written; none without a dot or with
/// nothing after it.
fn suffix_as_written(name: &std::ffi::OsStr) -> Option<String> {
    let text = name.to_string_lossy();
    let (_, after) = text.rsplit_once('.')?;
    if after.is_empty() {
        None
    } else {
        Some(after.to_string())
    }
}

pub fn analyse(
    music: &Path,
    path: &str,
    decoders: &Decoders,
    source: DecoderNote,
    video_in_use: bool,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<Report, Msg> {
    let start = walk::start(music, path)?;
    let rules = Rules::new();
    let mut report = Report {
        path: start.path.clone(),
        directories: 0,
        files: 0,
        unreadable: 0,
        too_deep: 0,
        excluded: Count::default(),
        classes: BTreeMap::new(),
        audio_suffixes: BTreeMap::new(),
        ffmpeg_suffixes: BTreeMap::new(),
        other_suffixes: Vec::new(),
        other_examples: BTreeMap::new(),
        in_force: start.in_force_lines(),
        suggestions: Vec::new(),
        decoders: source,
        video_in_use,
        cancelled: false,
    };
    let mut other_suffixes: BTreeMap<String, usize> = BTreeMap::new();
    let mut other_lumped = 0usize;
    let mut suggestions: BTreeMap<String, Suggestion> = BTreeMap::new();
    let walked: Walked = walk::walk(start, progress, cancel, |visit| {
        for x in visit.excluded {
            report.excluded.add_example(
                msg!(
                    "core.analyse.excluded",
                    "{path} ({line} at {from})",
                    path = x.path,
                    line = x.line,
                    from = x.from
                ),
                0,
            );
        }
        let mut enter = Vec::new();
        for (index, entry) in visit.entries.iter().enumerate() {
            let (class, mut pattern) =
                classify::classify(&rules, &entry.name, entry.is_dir, decoders);
            if class == Class::Folder {
                enter.push(index);
                continue;
            }
            // Video wanted here (Glass plays it, or the owner said so): counted, never
            // suggested.
            if class == Class::Video && video_in_use {
                pattern = None;
            }
            report
                .classes
                .entry(class)
                .or_default()
                .add(&entry.path, entry.size);
            let suffix = classify::suffix(&entry.name.to_string_lossy());
            match class {
                Class::Audio => {
                    if let Some(s) = suffix {
                        *report.audio_suffixes.entry(s).or_insert(0) += 1;
                    }
                }
                Class::FfmpegOnly => {
                    if let Some(s) = suffix {
                        *report.ffmpeg_suffixes.entry(s).or_insert(0) += 1;
                    }
                }
                Class::Other => {
                    // As written, case kept: the line offered must match the name as
                    // MPD matches it, and fnmatch is case-sensitive.
                    let s = suffix_as_written(&entry.name).unwrap_or_else(|| "(none)".to_string());
                    if other_suffixes.len() < OTHER_SUFFIXES || other_suffixes.contains_key(&s) {
                        let examples = report.other_examples.entry(s.clone()).or_default();
                        if examples.len() < 2 {
                            examples.push(entry.path.clone());
                        }
                        *other_suffixes.entry(s).or_insert(0) += 1;
                    } else {
                        other_lumped += 1;
                    }
                }
                _ => {}
            }
            if let Some(pattern) = pattern {
                let suggestion = suggestions
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
                suggestion.bytes += entry.size;
                if suggestion.examples.len() < EXAMPLES {
                    suggestion.examples.push(entry.path.clone());
                }
            }
        }
        enter
    });
    report.directories = walked.directories;
    report.files = walked.files;
    report.unreadable = walked.unreadable;
    report.too_deep = walked.too_deep;
    report.cancelled = walked.cancelled;
    let mut other: Vec<(String, usize)> = other_suffixes.into_iter().collect();
    other.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    other.truncate(OTHER_SHOWN);
    if other_lumped > 0 {
        other.push(("(other suffixes)".to_string(), other_lumped));
    }
    report.other_suffixes = other;
    let mut suggestions: Vec<Suggestion> = suggestions.into_values().collect();
    suggestions.sort_by(|a, b| a.class.cmp(&b.class).then_with(|| b.count.cmp(&a.count)));
    report.suggestions = suggestions;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpd;
    use std::fs;
    use std::path::PathBuf;

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
        fs::write(
            root.join("NAS/.mpdignore"),
            "# Jazz is clean\n*.opus\n#recycle\n",
        )
        .unwrap();
        fs::write(root.join("NAS/Jazz/.mpdignore"), "skip.*\n").unwrap();
        put("NAS/Jazz/Live/x.opus", 100);
        root
    }

    /// MPD's own flac and mp3, ffmpeg on mov (video), m4a (music) and txt (neither).
    fn decoders() -> Decoders {
        mpd::tests::decoders_of(&[
            ("flac", &["flac"]),
            ("mad", &["mp3"]),
            ("ffmpeg", &["mov", "m4a", "txt"]),
        ])
    }

    fn note() -> DecoderNote {
        DecoderNote {
            source: "test".to_string(),
            count: 0,
            detail: Msg::raw(""),
        }
    }

    #[test]
    fn the_walk_counts_every_class_and_suggests_the_lines_that_keep_the_junk_out() {
        let root = library("report");
        let progress = Progress::default();
        let cancel = AtomicBool::new(false);
        let r = analyse(
            &root,
            "NAS/Pop",
            &decoders(),
            note(),
            false,
            &progress,
            &cancel,
        )
        .unwrap();
        assert_eq!(r.path, "NAS/Pop");
        assert!(!r.video_in_use);
        // Pop, Genesis, _SACD; not #recycle or @eaDir, which are not walked into.
        assert_eq!(r.directories, 3);
        assert_eq!(r.files, 9);
        assert_eq!(
            progress
                .directories
                .load(std::sync::atomic::Ordering::Relaxed),
            3
        );
        let count = |c: Class| r.classes.get(&c).map(|x| x.count).unwrap_or(0);
        assert_eq!(count(Class::Audio), 2);
        assert_eq!(count(Class::Sidecar), 1);
        assert_eq!(count(Class::Video), 1);
        assert_eq!(count(Class::Image), 1);
        assert_eq!(count(Class::Cue), 1);
        assert_eq!(count(Class::Playlist), 1);
        assert_eq!(count(Class::Other), 0);
        assert_eq!(count(Class::FfmpegOnly), 1);
        assert_eq!(count(Class::Archive), 1);
        assert_eq!(count(Class::SystemFolder), 2);
        assert_eq!(r.audio_suffixes.get("flac"), Some(&2));
        assert_eq!(r.ffmpeg_suffixes.get("txt"), Some(&1));
        assert!(r.other_suffixes.is_empty());
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
                ("*.mov", 1),
                ("*.txt", 1)
            ]
        );
        assert_eq!(r.suggestions[3].bytes, 1000);
        assert_eq!(r.suggestions[4].note, Class::Video.note());
        assert_eq!(r.suggestions[5].note, Class::FfmpegOnly.note());
        // The root's list is in force at NAS/Pop: NAS/.mpdignore, with its dead line.
        assert_eq!(r.in_force.len(), 1);
        assert_eq!(r.in_force[0].dir, "NAS");
        assert_eq!(r.in_force[0].lines, ["*.opus"]);
        assert_eq!(r.in_force[0].dead, ["#recycle"]);
        assert_eq!(r.excluded.count, 0);
        assert!(!r.cancelled);
        // With video in use, the clip is counted and not suggested.
        let r = analyse(
            &root,
            "NAS/Pop",
            &decoders(),
            note(),
            true,
            &Progress::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(r.video_in_use);
        assert_eq!(r.classes[&Class::Video].count, 1);
        assert!(r.suggestions.iter().all(|s| s.pattern != "*.mov"));
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
            false,
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
                false,
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
                false,
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
            false,
            &Progress::default(),
            &AtomicBool::new(true),
        )
        .unwrap();
        assert!(r.cancelled);
        assert_eq!(r.directories, 0);
        fs::remove_dir_all(&root).unwrap();
    }
}
