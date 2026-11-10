//! The CUE check of one folder and everything under it, read-only: every sheet read
//! as MPD reads it, its files looked for beside it, what MPD would make of it said
//! in MPD's terms, sheet by sheet, and the totals.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde::Serialize;

use crate::analyse::DecoderNote;
use crate::classify;
use crate::cue::{self, Finding, Track};
use crate::mpd::Decoders;
use crate::walk::{self, InForceLines, Progress, Visit};

/// A sheet larger than this is not read; a finding says so.
pub const MAX_SHEET: u64 = 1024 * 1024;
/// Sheets listed in the report; the totals count every one.
const SHEETS_LISTED: usize = 500;
/// Tracks listed per sheet.
const TRACKS_LISTED: usize = 200;

/// A `FILE` of a sheet, as found beside it.
#[derive(Serialize, Debug)]
pub struct FileState {
    pub name: String,
    pub kind: String,
    pub accepted: bool,
    /// `ok`, `missing`, `case` (found in another case), `suffix` (found with another
    /// suffix), `not_decoded` (there, but no decoder claims it), `foreign`.
    pub state: &'static str,
    pub found: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct SheetReport {
    pub path: String,
    pub encoding: String,
    pub album: Option<String>,
    pub tracks: usize,
    pub files: Vec<FileState>,
    pub findings: Vec<Finding>,
    pub track_list: Vec<Track>,
    pub tracks_truncated: bool,
}

#[derive(Serialize, Debug)]
pub struct Report {
    pub path: String,
    pub directories: usize,
    pub files: usize,
    pub unreadable: usize,
    pub too_deep: usize,
    pub sheets: usize,
    pub clean: usize,
    pub with_findings: usize,
    pub by_code: BTreeMap<String, usize>,
    /// What each code in `by_code` means.
    pub codes: BTreeMap<String, &'static str>,
    pub list: Vec<SheetReport>,
    pub list_truncated: bool,
    pub in_force: Vec<InForceLines>,
    pub decoders: DecoderNote,
    pub cancelled: bool,
}

/// What each code means, for the page.
pub fn code_text(code: &str) -> &'static str {
    match code {
        "C01" => "FILE type MPD does not take",
        "C02" => "FILE not beside the sheet",
        "C03" => "FILE there but no decoder claims it",
        "C04" => "FILE is a foreign path",
        "C05" => "FILE name not quoted",
        "C06" => "byte-order mark",
        "C07" => "not UTF-8",
        "C08" => "an INDEX MPD cannot use",
        "C09" => "no track, or a track with no file",
        "C10" => "several FILE entries",
        "C11" => "two sheets name the same FILE",
        "C12" => "a sheet beside split tracks",
        "C13" => "sheet too large to read",
        "C14" => "an INDEX beyond the file's length",
        _ => "",
    }
}

/// The file names of one directory, for a sheet's `FILE` names to be looked up:
/// exact, in another case, with another suffix.
pub struct Beside {
    names: Vec<String>,
    by_lower: HashMap<String, String>,
    by_stem: HashMap<String, Vec<String>>,
}

impl Beside {
    pub fn new(names: Vec<String>) -> Beside {
        let by_lower = names
            .iter()
            .map(|n| (n.to_lowercase(), n.clone()))
            .collect();
        let by_stem =
            names
                .iter()
                .fold(HashMap::new(), |mut m: HashMap<String, Vec<String>>, n| {
                    let stem = n
                        .rsplit_once('.')
                        .map(|(s, _)| s)
                        .unwrap_or(n)
                        .to_lowercase();
                    m.entry(stem).or_default().push(n.clone());
                    m
                });
        Beside {
            names,
            by_lower,
            by_stem,
        }
    }

    /// The names of the files in `dir`, the folders left out.
    pub fn read(dir: &Path) -> Beside {
        let names = fs::read_dir(dir)
            .map(|read| {
                read.flatten()
                    .filter(|e| e.path().is_file())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        Beside::new(names)
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The names a decoder of this MPD claims.
    pub fn audio(&self, decoders: &Decoders) -> Vec<&str> {
        self.names
            .iter()
            .filter(|n| {
                classify::suffix(n)
                    .map(|s| decoders.suffixes.contains(&s))
                    .unwrap_or(false)
            })
            .map(String::as_str)
            .collect()
    }

    /// Where a `FILE` name stands beside the sheet: `ok`; `not_decoded` (there, but
    /// no decoder claims it); `case` and `suffix` (there under the name said); `foreign`
    /// (what kind of path); `missing`.
    pub fn locate(
        &self,
        name: &str,
        dir: &Path,
        decoders: &Decoders,
    ) -> (&'static str, Option<String>) {
        let claimed = |n: &str| {
            classify::suffix(n)
                .map(|s| decoders.suffixes.contains(&s))
                .unwrap_or(false)
        };
        if let Some(what) = cue::foreign_path(name) {
            return ("foreign", Some(what.to_string()));
        }
        if self.names.iter().any(|n| n == name) || dir.join(name).is_file() {
            return if claimed(name) {
                ("ok", None)
            } else {
                ("not_decoded", None)
            };
        }
        if let Some(n) = self.by_lower.get(&name.to_lowercase()) {
            return ("case", Some(n.clone()));
        }
        // The same stem with another suffix: an audio file first, never a sheet.
        if let Some(candidates) = name
            .rsplit_once('.')
            .map(|(s, _)| s.to_lowercase())
            .and_then(|stem| self.by_stem.get(&stem))
        {
            let found = candidates.iter().find(|n| claimed(n)).or_else(|| {
                candidates
                    .iter()
                    .find(|n| classify::suffix(n).as_deref() != Some(classify::CUE_SUFFIX))
            });
            if let Some(n) = found {
                return ("suffix", Some(n.clone()));
            }
        }
        ("missing", None)
    }
}

/// The CUE check of `path` (MPD's form) under the music directory.
pub fn check(
    music: &Path,
    path: &str,
    decoders: &Decoders,
    source: DecoderNote,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<Report, String> {
    let start = walk::start(music, path)?;
    let rules = classify::Rules::new();
    let mut report = Report {
        path: start.path.clone(),
        directories: 0,
        files: 0,
        unreadable: 0,
        too_deep: 0,
        sheets: 0,
        clean: 0,
        with_findings: 0,
        by_code: BTreeMap::new(),
        codes: BTreeMap::new(),
        list: Vec::new(),
        list_truncated: false,
        in_force: start.in_force_lines(),
        decoders: source,
        cancelled: false,
    };
    let walked = walk::walk(start, progress, cancel, |visit| {
        let mut enter = Vec::new();
        for (index, entry) in visit.entries.iter().enumerate() {
            if entry.is_dir {
                let (class, _) = classify::classify(&rules, &entry.name, true, decoders);
                if class == classify::Class::Folder {
                    enter.push(index);
                }
            }
        }
        directory(visit, decoders, &mut report);
        enter
    });
    report.directories = walked.directories;
    report.files = walked.files;
    report.unreadable = walked.unreadable;
    report.too_deep = walked.too_deep;
    report.cancelled = walked.cancelled;
    report.codes = report
        .by_code
        .keys()
        .map(|code| (code.clone(), code_text(code)))
        .collect();
    // Sheets with findings first, then by path.
    report.list.sort_by(|a, b| {
        a.findings
            .is_empty()
            .cmp(&b.findings.is_empty())
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(report)
}

/// The sheets of one directory, each against the names beside it.
fn directory(visit: &Visit, decoders: &Decoders, report: &mut Report) {
    // The names beside the sheets: exact, by lowercase, by stem.
    let beside = Beside::new(
        visit
            .entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.name.to_string_lossy().into_owned())
            .collect(),
    );
    let audio = beside.audio(decoders);
    let sheets: Vec<&walk::Entry> = visit
        .entries
        .iter()
        .filter(|e| {
            !e.is_dir
                && classify::suffix(&e.name.to_string_lossy()).as_deref()
                    == Some(classify::CUE_SUFFIX)
        })
        .collect();
    if sheets.is_empty() {
        return;
    }
    // Which FILE names each sheet takes, for C11 and C12 across the directory.
    let mut referenced: HashMap<String, Vec<String>> = HashMap::new();
    let mut parsed: Vec<(String, cue::Sheet, Vec<FileState>)> = Vec::new();
    for entry in sheets {
        let path = entry.path.clone();
        if entry.size > MAX_SHEET {
            parsed.push((
                path,
                cue::Sheet {
                    encoding: String::new(),
                    album: None,
                    performer: None,
                    files: Vec::new(),
                    tracks: Vec::new(),
                    findings: vec![Finding {
                        code: "C13",
                        line: 0,
                        text: format!("The sheet is {} bytes; not read.", entry.size),
                    }],
                },
                Vec::new(),
            ));
            continue;
        }
        let bytes = match fs::read(visit.dir.join(&entry.name)) {
            Ok(b) => b,
            Err(e) => {
                report.unreadable += 1;
                parsed.push((
                    path,
                    cue::Sheet {
                        encoding: String::new(),
                        album: None,
                        performer: None,
                        files: Vec::new(),
                        tracks: Vec::new(),
                        findings: vec![Finding {
                            code: "C13",
                            line: 0,
                            text: format!("The sheet could not be read: {e}"),
                        }],
                    },
                    Vec::new(),
                ));
                continue;
            }
        };
        let sheet = cue::parse(&bytes);
        let mut files = Vec::new();
        for f in &sheet.files {
            let (state, found) = beside.locate(&f.name, visit.dir, decoders);
            if f.accepted {
                referenced
                    .entry(f.name.clone())
                    .or_default()
                    .push(path.clone());
            }
            files.push(FileState {
                name: f.name.clone(),
                kind: f.kind.clone(),
                accepted: f.accepted,
                state,
                found,
            });
        }
        parsed.push((path, sheet, files));
    }
    for (path, mut sheet, files) in parsed {
        for f in &files {
            if !f.accepted {
                continue;
            }
            let finding = match (f.state, &f.found) {
                ("missing", _) => Some(("C02", format!("FILE \"{}\" is not beside the sheet: MPD adds the tracks and purges them at the end of every update.", f.name))),
                ("case", Some(n)) => Some(("C02", format!("FILE \"{}\" is not beside the sheet; \"{n}\" is, in another case: MPD looks the name up exactly and purges the tracks.", f.name))),
                ("suffix", Some(n)) => Some(("C02", format!("FILE \"{}\" is not beside the sheet; \"{n}\" is: MPD purges the tracks.", f.name))),
                ("not_decoded", _) => Some(("C03", format!("FILE \"{}\" is there but no decoder of this MPD claims its suffix: MPD purges the tracks.", f.name))),
                ("foreign", Some(what)) => Some(("C04", format!("FILE \"{}\" is {what}: nothing beside the sheet on this player.", f.name))),
                _ => None,
            };
            if let Some((code, text)) = finding {
                sheet.findings.push(Finding {
                    code,
                    line: 0,
                    text,
                });
            }
            if let Some(others) = referenced.get(&f.name) {
                if others.len() > 1 && f.state == "ok" {
                    let names: Vec<&str> = others
                        .iter()
                        .filter(|p| **p != path)
                        .map(|p| p.rsplit('/').next().unwrap_or(p))
                        .collect();
                    sheet.findings.push(Finding {
                        code: "C11",
                        line: 0,
                        text: format!(
                            "\"{}\" is also named by {}: MPD indexes its tracks twice.",
                            f.name,
                            names.join(", ")
                        ),
                    });
                }
            }
        }
        // The last track's start against the file's length, where the file can be read.
        for f in files.iter().filter(|f| f.accepted && f.state == "ok") {
            let last_start = sheet
                .tracks
                .iter()
                .filter(|t| t.file == f.name)
                .filter_map(|t| t.start_ms)
                .max();
            let (Some(start), Some(suffix)) = (last_start, classify::suffix(&f.name)) else {
                continue;
            };
            if let Some(length) = crate::tags::duration_ms(&visit.dir.join(&f.name), &suffix) {
                if length > 0 && u64::from(start) > length {
                    sheet.findings.push(Finding {
                        code: "C14",
                        line: 0,
                        text: format!(
                            "The last INDEX in \"{}\" is at {}:{:02}, past the file's length of {}:{:02}: MPD makes a track it cannot play.",
                            f.name,
                            start / 60_000,
                            (start / 1000) % 60,
                            length / 60_000,
                            (length / 1000) % 60
                        ),
                    });
                }
            }
        }
        // One file in the sheet, and several audio files beside it that no sheet names.
        let accepted: Vec<&FileState> = files
            .iter()
            .filter(|f| f.accepted && f.state == "ok")
            .collect();
        if accepted.len() == 1 {
            let unreferenced = audio
                .iter()
                .filter(|n| !referenced.contains_key(**n))
                .count();
            if unreferenced >= 3 {
                sheet.findings.push(Finding {
                    code: "C12",
                    line: 0,
                    text: format!("The sheet names one file and {unreferenced} audio files beside it belong to no sheet: the album may be here twice, as an image and as tracks."),
                });
            }
        }
        sheet.findings.sort_by_key(|f| (f.code, f.line));
        report.sheets += 1;
        if sheet.findings.is_empty() {
            report.clean += 1;
        } else {
            report.with_findings += 1;
        }
        for f in &sheet.findings {
            *report.by_code.entry(f.code.to_string()).or_insert(0) += 1;
        }
        if report.list.len() < SHEETS_LISTED {
            let tracks = sheet.tracks.len();
            let tracks_truncated = tracks > TRACKS_LISTED;
            let mut track_list = sheet.tracks;
            track_list.truncate(TRACKS_LISTED);
            report.list.push(SheetReport {
                path,
                encoding: sheet.encoding,
                album: sheet.album,
                tracks,
                files,
                findings: sheet.findings,
                track_list,
                tracks_truncated,
            });
        } else {
            report.list_truncated = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpd;
    use std::path::PathBuf;

    fn library(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mpdantic-cue-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mk = |p: &str| fs::create_dir_all(root.join(p)).unwrap();
        let put = |p: &str, b: &[u8]| fs::write(root.join(p), b).unwrap();
        mk("NAS/Clean");
        // A real FLAC header of four minutes, so the sheet's indexes are within it.
        put(
            "NAS/Clean/album.flac",
            &crate::tags::tests::flac_bytes(44100, 44100 * 240, &[]),
        );
        put("NAS/Clean/album.cue", b"TITLE \"A\"\nFILE \"album.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 03:00:00\n");
        mk("NAS/Short");
        put(
            "NAS/Short/album.flac",
            &crate::tags::tests::flac_bytes(44100, 44100 * 100, &[]),
        );
        put("NAS/Short/album.cue", b"FILE \"album.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 03:00:00\n");
        mk("NAS/Box/1");
        put("NAS/Box/1/Disc One.flac", b"x");
        put(
            "NAS/Box/1/disc1.cue",
            b"FILE \"disc one.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        put(
            "NAS/Box/1/disc1-wav.cue",
            b"FILE \"Disc One.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        put(
            "NAS/Box/1/gone.cue",
            b"FILE \"nothing.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        put(
            "NAS/Box/1/win.cue",
            b"FILE \"D:\\Music\\Disc One.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        put(
            "NAS/Box/1/ape.cue",
            b"FILE \"disc.ape\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        put("NAS/Box/1/disc.ape", b"x");
        mk("NAS/Twice");
        put("NAS/Twice/image.flac", b"x");
        put(
            "NAS/Twice/a.cue",
            b"FILE \"image.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        put(
            "NAS/Twice/b.cue",
            b"FILE \"image.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        );
        put("NAS/Twice/01.flac", b"x");
        put("NAS/Twice/02.flac", b"x");
        put("NAS/Twice/03.flac", b"x");
        mk("NAS/Skipped");
        put("NAS/Skipped/x.cue", b"nothing\n");
        fs::write(root.join("NAS/.mpdignore"), "Skipped\n").unwrap();
        root
    }

    fn decoders() -> Decoders {
        mpd::tests::decoders_of(&[("flac", &["flac"]), ("mad", &["mp3"])])
    }

    fn note() -> DecoderNote {
        DecoderNote {
            source: "test".to_string(),
            count: 0,
            detail: String::new(),
        }
    }

    #[test]
    fn every_sheet_is_read_and_its_files_looked_for_beside_it() {
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
        assert_eq!(r.sheets, 9);
        assert_eq!(r.clean, 1);
        assert_eq!(r.with_findings, 8);
        // NAS, Clean, Box, Box/1, Short, Twice; Skipped is kept out by the line in force.
        assert_eq!(r.directories, 6);
        let short = r
            .list
            .iter()
            .find(|s| s.path == "NAS/Short/album.cue")
            .unwrap();
        assert_eq!(short.findings[0].code, "C14");
        assert!(
            short.findings[0].text.contains("3:00"),
            "{}",
            short.findings[0].text
        );
        assert!(short.findings[0].text.contains("1:40"));
        let by = |p: &str| r.list.iter().find(|s| s.path == p).unwrap();
        assert!(
            by("NAS/Clean/album.cue").findings.is_empty(),
            "{:?}",
            by("NAS/Clean/album.cue").findings
        );
        assert_eq!(by("NAS/Clean/album.cue").tracks, 2);
        assert_eq!(
            by("NAS/Clean/album.cue").track_list[0].end_ms,
            Some(180_000)
        );
        let case = by("NAS/Box/1/disc1.cue");
        assert_eq!(case.files[0].state, "case");
        assert_eq!(case.files[0].found.as_deref(), Some("Disc One.flac"));
        assert_eq!(case.findings[0].code, "C02");
        let suffix = by("NAS/Box/1/disc1-wav.cue");
        assert_eq!(suffix.files[0].state, "suffix");
        assert_eq!(suffix.files[0].found.as_deref(), Some("Disc One.flac"));
        assert_eq!(by("NAS/Box/1/gone.cue").files[0].state, "missing");
        assert_eq!(by("NAS/Box/1/win.cue").findings[0].code, "C04");
        assert_eq!(by("NAS/Box/1/ape.cue").findings[0].code, "C03");
        let twice = by("NAS/Twice/a.cue");
        let codes: Vec<&str> = twice.findings.iter().map(|f| f.code).collect();
        assert_eq!(codes, ["C11", "C12"]);
        assert!(twice.findings[0].text.contains("b.cue"));
        // The sheets with findings come first; the skipped folder's sheet is never read.
        assert!(!r.list[0].findings.is_empty());
        assert!(r.list.iter().all(|s| !s.path.starts_with("NAS/Skipped")));
        assert_eq!(r.by_code.get("C02"), Some(&3));
        assert_eq!(r.by_code.get("C11"), Some(&2));
        assert_eq!(r.codes.get("C11"), Some(&"two sheets name the same FILE"));
        assert_eq!(r.in_force[0].lines, ["Skipped"]);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_folder_without_sheets_reports_none() {
        let root = library("none");
        let r = check(
            &root,
            "NAS/Twice/..",
            &decoders(),
            note(),
            &Progress::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(r.unwrap_err(), "not a library path");
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
        assert_eq!(r.sheets, 0);
        assert!(r.list.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }
}
