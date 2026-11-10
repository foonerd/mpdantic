//! The tag check of one folder and everything under it, read-only: every audio file
//! read for its tags, what MPD makes of them per file, and per folder whether the
//! Albums view would show the folder as one album or several. Counts and a handful
//! of examples; nothing per file kept for the whole walk.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde::Serialize;

use crate::analyse::{Count, DecoderNote};
use crate::classify::{self, Class, Rules};
use crate::mpd::Decoders;
use crate::tags::{self, Seen};
use crate::walk::{self, InForceLines, Progress, Visit};

/// Folders with a split listed; the rest counted.
const SPLITS_LISTED: usize = 300;
/// Distinct values listed per split.
const VALUES_LISTED: usize = 8;

/// A folder whose files would show as more than one album, with the values that
/// split it.
#[derive(Serialize, Debug)]
pub struct Split {
    pub dir: String,
    pub files: usize,
    pub albums: Vec<(String, usize)>,
    pub album_artists: Vec<(String, usize)>,
    pub dates: Vec<(String, usize)>,
}

#[derive(Serialize, Debug)]
pub struct Report {
    pub path: String,
    pub directories: usize,
    pub files: usize,
    pub unreadable: usize,
    pub too_deep: usize,
    /// Audio files read for their tags.
    pub read: usize,
    pub by_format: BTreeMap<String, usize>,
    pub findings: BTreeMap<String, Count>,
    pub codes: BTreeMap<String, &'static str>,
    pub splits: Vec<Split>,
    pub splits_truncated: bool,
    pub in_force: Vec<InForceLines>,
    pub decoders: DecoderNote,
    pub cancelled: bool,
}

/// What each code means, for the page.
pub fn code_text(code: &str) -> &'static str {
    match code {
        "T01" => "no title, artist or album as MPD reads them",
        "T02" => "an album artist where MPD does not read it",
        "T03" => "a date where MPD does not read it, or not a year first",
        "T04" => "a track number missing or not a number",
        "T05" => "a disc number missing where the folder looks like a disc",
        "T06" => "a folder that shows as several albums",
        "T07" => "UTF-8 read as Latin-1",
        "T08" => "ID3v1 only",
        "T09" => "a tag given twice",
        "T10" => "a placeholder or an empty value",
        "T11" => "an embedded cue sheet beside an external one",
        "T12" => "not read",
        _ => "",
    }
}

const PLACEHOLDERS: &[&str] = &[
    "unknown",
    "unknown artist",
    "unknown album",
    "untitled",
    "no title",
    "no artist",
    "no album",
    "track",
    "various",
    "various artists",
];

/// The tag check of `path` (MPD's form) under the music directory.
pub fn check(
    music: &Path,
    path: &str,
    decoders: &Decoders,
    source: DecoderNote,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<Report, String> {
    let start = walk::start(music, path)?;
    let rules = Rules::new();
    let mut report = Report {
        path: start.path.clone(),
        directories: 0,
        files: 0,
        unreadable: 0,
        too_deep: 0,
        read: 0,
        by_format: BTreeMap::new(),
        findings: BTreeMap::new(),
        codes: BTreeMap::new(),
        splits: Vec::new(),
        splits_truncated: false,
        in_force: start.in_force_lines(),
        decoders: source,
        cancelled: false,
    };
    let walked = walk::walk(start, progress, cancel, |visit| {
        let mut enter = Vec::new();
        for (index, entry) in visit.entries.iter().enumerate() {
            if entry.is_dir {
                let (class, _) = classify::classify(&rules, &entry.name, true, decoders);
                if class == Class::Folder {
                    enter.push(index);
                }
            }
        }
        directory(visit, &rules, decoders, &mut report, cancel);
        enter
    });
    report.directories = walked.directories;
    report.files = walked.files;
    report.unreadable += walked.unreadable;
    report.too_deep = walked.too_deep;
    report.cancelled = walked.cancelled;
    report.codes = report
        .findings
        .keys()
        .map(|code| (code.clone(), code_text(code)))
        .collect();
    Ok(report)
}

fn add(report: &mut Report, code: &str, example: String) {
    report
        .findings
        .entry(code.to_string())
        .or_default()
        .add(&example, 0);
}

/// The first value, trimmed.
fn first(values: &[String]) -> Option<&str> {
    values.first().map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn counted(values: BTreeMap<String, usize>) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = values.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(VALUES_LISTED);
    v
}

/// The audio files of one directory, each read, then the folder as a whole.
fn directory(
    visit: &Visit,
    rules: &Rules,
    decoders: &Decoders,
    report: &mut Report,
    cancel: &AtomicBool,
) {
    let has_cue = visit.entries.iter().any(|e| {
        !e.is_dir && classify::suffix(&e.name.to_string_lossy()).as_deref() == Some("cue")
    });
    let dir_name = visit
        .dir
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let looks_like_disc = {
        let n = dir_name.trim();
        (n.starts_with("cd") || n.starts_with("disc") || n.starts_with("disk"))
            && n.chars()
                .next_back()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
    };
    let mut audio_files = 0usize;
    let mut albums: BTreeMap<String, usize> = BTreeMap::new();
    let mut album_artists: BTreeMap<String, usize> = BTreeMap::new();
    let mut dates: BTreeMap<String, usize> = BTreeMap::new();
    let mut with_disc = 0usize;
    let mut without_disc = 0usize;
    for entry in visit.entries.iter().filter(|e| !e.is_dir) {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let (class, _) = classify::classify(rules, &entry.name, false, decoders);
        if class != Class::Audio {
            continue;
        }
        let suffix = classify::suffix(&entry.name.to_string_lossy()).unwrap_or_default();
        let raw = tags::read(&visit.dir.join(&entry.name), &suffix);
        report.read += 1;
        *report.by_format.entry(raw.format.clone()).or_insert(0) += 1;
        if let Some(problem) = &raw.problem {
            add(report, "T12", format!("{}: {problem}", entry.path));
            continue;
        }
        audio_files += 1;
        let seen = tags::view(&raw);
        file_findings(report, &entry.path, &raw, &seen, has_cue);
        let album = first(&seen.album).unwrap_or("(none)").to_string();
        let album_artist = first(&seen.album_artist)
            .or_else(|| first(&seen.artist))
            .unwrap_or("(none)")
            .to_string();
        *albums.entry(album).or_insert(0) += 1;
        *album_artists.entry(album_artist).or_insert(0) += 1;
        if let Some(d) = first(&seen.date) {
            *dates.entry(d.chars().take(4).collect()).or_insert(0) += 1;
        }
        if first(&seen.disc).is_some() {
            with_disc += 1;
        } else {
            without_disc += 1;
        }
    }
    if audio_files == 0 {
        return;
    }
    if looks_like_disc && with_disc == 0 {
        add(
            report,
            "T05",
            format!(
                "{}: {without_disc} files, no disc number",
                visit.entries[0]
                    .path
                    .rsplit_once('/')
                    .map(|(d, _)| d)
                    .unwrap_or("")
            ),
        );
    } else if with_disc > 0 && without_disc > 0 {
        add(
            report,
            "T05",
            format!(
                "{}: {with_disc} files with a disc number, {without_disc} without",
                dir_of(visit)
            ),
        );
    }
    if albums.len() > 1 || album_artists.len() > 1 {
        add(
            report,
            "T06",
            format!(
                "{}: {} albums, {} album artists in {audio_files} files",
                dir_of(visit),
                albums.len(),
                album_artists.len()
            ),
        );
        if report.splits.len() < SPLITS_LISTED {
            report.splits.push(Split {
                dir: dir_of(visit),
                files: audio_files,
                albums: counted(albums),
                album_artists: counted(album_artists),
                dates: counted(dates),
            });
        } else {
            report.splits_truncated = true;
        }
    }
}

fn dir_of(visit: &Visit) -> String {
    visit
        .entries
        .first()
        .and_then(|e| e.path.rsplit_once('/').map(|(d, _)| d.to_string()))
        .unwrap_or_default()
}

/// The findings of one file, from MPD's view of it.
fn file_findings(report: &mut Report, path: &str, raw: &tags::Raw, seen: &Seen, has_cue: bool) {
    let missing: Vec<&str> = [
        ("title", first(&seen.title).is_none()),
        ("artist", first(&seen.artist).is_none()),
        ("album", first(&seen.album).is_none()),
    ]
    .iter()
    .filter(|(_, m)| *m)
    .map(|(n, _)| *n)
    .collect();
    if !missing.is_empty() {
        add(report, "T01", format!("{path}: no {}", missing.join(", ")));
    }
    let ignored_album_artist = seen.ignored.iter().find(|k| {
        k.to_lowercase().contains("album artist") || k.to_lowercase().contains("albumartist")
    });
    if seen.album_artist.is_empty() {
        if let Some(k) = ignored_album_artist {
            add(report, "T02", format!("{path}: {k}"));
        }
    }
    let ignored_date = seen.ignored.iter().find(|k| {
        k.to_lowercase().starts_with("year ") || k.to_lowercase().starts_with("txxx:year")
    });
    match first(&seen.date) {
        None => {
            if let Some(k) = ignored_date {
                add(report, "T03", format!("{path}: {k}"));
            }
        }
        Some(d) => {
            if d.len() < 4 || !d[..4].chars().all(|c| c.is_ascii_digit()) {
                add(report, "T03", format!("{path}: date \"{d}\""));
            }
        }
    }
    match first(&seen.track) {
        None => add(report, "T04", format!("{path}: no track number")),
        Some(t) => {
            let n = t.split('/').next().unwrap_or("").trim();
            if n.is_empty() || n.chars().any(|c| !c.is_ascii_digit()) {
                add(report, "T04", format!("{path}: track \"{t}\""));
            } else if n.chars().all(|c| c == '0') {
                add(report, "T04", format!("{path}: track 0"));
            }
        }
    }
    let texts: Vec<&String> = seen
        .title
        .iter()
        .chain(&seen.artist)
        .chain(&seen.album_artist)
        .chain(&seen.album)
        .collect();
    if let Some(t) = texts.iter().find(|t| tags::looks_mangled(t)) {
        add(report, "T07", format!("{path}: \"{t}\""));
    }
    if seen.id3v1_only {
        add(report, "T08", path.to_string());
    }
    for (name, values) in [
        ("title", &seen.title),
        ("artist", &seen.artist),
        ("album artist", &seen.album_artist),
        ("album", &seen.album),
        ("date", &seen.date),
        ("track", &seen.track),
    ] {
        let mut distinct: Vec<&str> = values.iter().map(|v| v.trim()).collect();
        distinct.sort();
        distinct.dedup();
        if distinct.len() > 1 {
            add(
                report,
                "T09",
                format!("{path}: {name} given {} times", values.len()),
            );
            break;
        }
    }
    let placeholder = |v: &String| {
        let t = v.trim().to_lowercase();
        t.is_empty()
            || PLACEHOLDERS.contains(&t.as_str())
            || (t.starts_with("track ") && t[6..].chars().all(|c| c.is_ascii_digit()))
    };
    if let Some(v) = seen
        .title
        .iter()
        .chain(&seen.artist)
        .chain(&seen.album)
        .find(|v| placeholder(v))
    {
        add(report, "T10", format!("{path}: \"{}\"", v.trim()));
    }
    if seen.cuesheet && has_cue {
        add(report, "T11", path.to_string());
    }
    let _ = raw;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpd;
    use crate::tags::tests::{flac_bytes, id3v2_bytes};
    use std::fs;
    use std::io::Write as _;
    use std::path::PathBuf;

    fn library(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("mpdantic-tagcheck-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mk = |p: &str| fs::create_dir_all(root.join(p)).unwrap();
        let put = |p: &str, b: &[u8]| {
            fs::File::create(root.join(p))
                .unwrap()
                .write_all(b)
                .unwrap()
        };
        mk("NAS/Clean");
        put(
            "NAS/Clean/01.flac",
            &flac_bytes(
                44100,
                44100,
                &[
                    ("ARTIST", "A"),
                    ("ALBUMARTIST", "A"),
                    ("ALBUM", "X"),
                    ("TITLE", "One"),
                    ("TRACKNUMBER", "1"),
                    ("DATE", "2001"),
                ],
            ),
        );
        put(
            "NAS/Clean/02.flac",
            &flac_bytes(
                44100,
                44100,
                &[
                    ("ARTIST", "A"),
                    ("ALBUMARTIST", "A"),
                    ("ALBUM", "X"),
                    ("TITLE", "Two"),
                    ("TRACKNUMBER", "2"),
                    ("DATE", "2001"),
                ],
            ),
        );
        mk("NAS/Comp");
        put(
            "NAS/Comp/01.flac",
            &flac_bytes(
                44100,
                44100,
                &[
                    ("ARTIST", "P"),
                    ("ALBUM ARTIST", "Various"),
                    ("ALBUM", "Hits"),
                    ("TITLE", "One"),
                    ("TRACKNUMBER", "1"),
                    ("YEAR", "1999"),
                ],
            ),
        );
        put(
            "NAS/Comp/02.flac",
            &flac_bytes(
                44100,
                44100,
                &[
                    ("ARTIST", "Q"),
                    ("ALBUM ARTIST", "Various"),
                    ("ALBUM", "Hits"),
                    ("TITLE", "Two"),
                    ("TRACKNUMBER", "2"),
                    ("YEAR", "1999"),
                ],
            ),
        );
        put(
            "NAS/Comp/03.mp3",
            &id3v2_bytes(&[
                ("TPE1", "R"),
                ("TALB", "Hits"),
                ("TIT2", "Caf\u{c3}\u{a9}"),
                ("TRCK", "x"),
            ]),
        );
        put(
            "NAS/Comp/04.flac",
            &flac_bytes(
                44100,
                44100,
                &[
                    ("ARTIST", "S"),
                    ("ALBUM", "hits"),
                    ("TITLE", "Unknown"),
                    ("CUESHEET", "FILE"),
                ],
            ),
        );
        put(
            "NAS/Comp/04.cue",
            b"FILE \"04.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        mk("NAS/Box/CD1");
        put(
            "NAS/Box/CD1/01.flac",
            &flac_bytes(
                44100,
                44100,
                &[
                    ("ARTIST", "A"),
                    ("ALBUM", "Box"),
                    ("TITLE", "One"),
                    ("TRACKNUMBER", "1"),
                ],
            ),
        );
        put("NAS/Box/CD1/bad.flac", b"not a flac");
        put("NAS/Box/CD1/x.wma", b"nothing");
        root
    }

    fn decoders() -> Decoders {
        mpd::tests::decoders_of(&[("flac", &["flac"]), ("mad", &["mp3"]), ("ffmpeg", &["wma"])])
    }

    fn note() -> DecoderNote {
        DecoderNote {
            source: "test".to_string(),
            count: 0,
            detail: String::new(),
        }
    }

    #[test]
    fn every_audio_file_is_read_and_the_folders_judged() {
        let root = library("check");
        let r = check(
            &root,
            "NAS",
            &decoders(),
            note(),
            &Progress::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(r.directories, 5);
        // Two in Clean, three in Comp, two in CD1 as flac; the mp3; the wma.
        assert_eq!(r.read, 9, "{:?}", r.by_format);
        assert_eq!(r.by_format.get("flac"), Some(&7));
        assert_eq!(r.by_format.get("mp3"), Some(&1));
        let c = |code: &str| r.findings.get(code).map(|x| x.count).unwrap_or(0);
        let ex = |code: &str| {
            r.findings
                .get(code)
                .map(|x| x.examples.clone())
                .unwrap_or_default()
        };
        // T12: the wma, not read here; the broken flac.
        assert_eq!(c("T12"), 2, "{:?}", ex("T12"));
        // T02: the two compilation tracks with ALBUM ARTIST where MPD does not read it.
        assert_eq!(c("T02"), 2, "{:?}", ex("T02"));
        assert!(ex("T02")[0].contains("ALBUM ARTIST (Vorbis comment)"));
        // T03: YEAR on the same two.
        assert_eq!(c("T03"), 2, "{:?}", ex("T03"));
        // T04: the mp3's track "x", 04.flac with no track, the box disc's fine.
        assert_eq!(c("T04"), 2, "{:?}", ex("T04"));
        // T07: the mangled title.
        assert_eq!(c("T07"), 1, "{:?}", ex("T07"));
        assert!(ex("T07")[0].contains("03.mp3"));
        // T10: "Unknown".
        assert_eq!(c("T10"), 1, "{:?}", ex("T10"));
        // T11: 04.flac with a CUESHEET comment beside 04.cue.
        assert_eq!(c("T11"), 1, "{:?}", ex("T11"));
        // T05: the CD1 folder with no disc numbers.
        assert_eq!(c("T05"), 1, "{:?}", ex("T05"));
        // T06: Comp splits (Hits and hits; P, Q, R, S as album artists); Clean does not.
        assert_eq!(c("T06"), 1, "{:?}", ex("T06"));
        assert_eq!(r.splits.len(), 1);
        assert_eq!(r.splits[0].dir, "NAS/Comp");
        assert_eq!(r.splits[0].files, 4);
        assert_eq!(r.splits[0].albums.len(), 2);
        assert_eq!(r.splits[0].album_artists.len(), 4);
        assert_eq!(
            r.codes.get("T06"),
            Some(&"a folder that shows as several albums")
        );
        assert_eq!(c("T01"), 0, "{:?}", ex("T01"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_folder_without_audio_reports_nothing() {
        let root = library("none");
        fs::create_dir_all(root.join("USB")).unwrap();
        let r = check(
            &root,
            "USB",
            &decoders(),
            note(),
            &Progress::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(r.read, 0);
        assert!(r.findings.is_empty());
        assert!(r.splits.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }
}
