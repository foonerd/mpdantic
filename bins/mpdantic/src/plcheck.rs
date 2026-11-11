//! The playlist check of one folder and everything under it, read-only: every m3u,
//! m3u8, pls and xspf read as MPD reads it, each entry resolved as `load` would, the
//! file it names looked for, what MPD and Volumio would make of it said per playlist,
//! and the totals.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use serde::Serialize;

use crate::analyse::DecoderNote;
use crate::classify::{self, Class, Rules};
use crate::cue::Finding;
use crate::mpd::Decoders;
use crate::playlist::{self, Place};
use crate::walk::{self, InForceLines, Progress, Visit};

/// A playlist larger than this is not read; a finding says so.
pub const MAX_PLAYLIST: u64 = 1024 * 1024;
/// Playlists listed in the report; the totals count every one.
const LISTED: usize = 500;
/// Entries listed per playlist.
const ENTRIES_LISTED: usize = 300;

/// One entry as `load` would take it.
#[derive(Serialize, Debug)]
pub struct EntryState {
    pub line: usize,
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `ok`, `missing`, `not_decoded`, `foreign`, `stream`.
    pub state: &'static str,
    /// The library path MPD looks up, for a relative entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct PlaylistReport {
    pub path: String,
    pub format: String,
    pub encoding: String,
    pub entries: usize,
    pub found: usize,
    pub streams: usize,
    pub findings: Vec<Finding>,
    pub entry_list: Vec<EntryState>,
    pub entries_truncated: bool,
}

#[derive(Serialize, Debug)]
pub struct Report {
    pub path: String,
    pub directories: usize,
    pub files: usize,
    pub unreadable: usize,
    pub too_deep: usize,
    pub playlists: usize,
    pub clean: usize,
    pub with_findings: usize,
    /// `asx` and `rss` files, MPD's too, not read here.
    pub not_read: usize,
    pub by_code: BTreeMap<String, usize>,
    pub codes: BTreeMap<String, &'static str>,
    pub list: Vec<PlaylistReport>,
    pub list_truncated: bool,
    pub in_force: Vec<InForceLines>,
    pub decoders: DecoderNote,
    pub cancelled: bool,
}

/// What each code means, for the page.
pub fn code_text(code: &str) -> &'static str {
    match code {
        "P01" => "an entry not found where MPD looks",
        "P02" => "a foreign path",
        "P03" => "an entry no decoder claims",
        "P04" => "byte-order mark",
        "P05" => "not UTF-8",
        "P06" => "a PLS MPD rejects",
        "P07" => "m3u8 or xspf, which Volumio never plays",
        "P08" => "no entry",
        "P09" => "not read",
        _ => "",
    }
}

/// The playlist check of `path` (MPD's form) under the music directory.
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
        playlists: 0,
        clean: 0,
        with_findings: 0,
        not_read: 0,
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
                if class == Class::Folder {
                    enter.push(index);
                }
            }
        }
        directory(music, visit, decoders, &mut report);
        enter
    });
    report.directories = walked.directories;
    report.files = walked.files;
    report.unreadable += walked.unreadable;
    report.too_deep = walked.too_deep;
    report.cancelled = walked.cancelled;
    report.codes = report
        .by_code
        .keys()
        .map(|code| (code.clone(), code_text(code)))
        .collect();
    // Playlists with findings first, then by path.
    report.list.sort_by(|a, b| {
        a.findings
            .is_empty()
            .cmp(&b.findings.is_empty())
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(report)
}

/// The state of one entry: where `load` would find it, if anywhere.
pub fn state_of(
    music: &Path,
    uri: &str,
    folder: &str,
    decoders: &Decoders,
) -> (&'static str, Option<String>) {
    match playlist::resolve(uri, folder) {
        (Place::Relative, Some(resolved)) => {
            if music.join(&resolved).is_file() {
                let claimed = classify::suffix(&resolved)
                    .map(|s| decoders.suffixes.contains(&s))
                    .unwrap_or(false);
                (if claimed { "ok" } else { "not_decoded" }, Some(resolved))
            } else {
                ("missing", Some(resolved))
            }
        }
        (Place::Stream, _) => ("stream", None),
        _ => ("foreign", None),
    }
}

fn folder_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

/// The playlists of one directory.
fn directory(music: &Path, visit: &Visit, decoders: &Decoders, report: &mut Report) {
    for entry in visit.entries.iter().filter(|e| !e.is_dir) {
        let name = entry.name.to_string_lossy();
        let Some(suffix) = classify::suffix(&name) else {
            continue;
        };
        if suffix == "asx" || suffix == "rss" {
            report.not_read += 1;
            continue;
        }
        if !playlist::SUFFIXES.contains(&suffix.as_str()) {
            continue;
        }
        let path = entry.path.clone();
        let folder = folder_of(&path).to_string();
        let mut findings: Vec<Finding> = Vec::new();
        let (format, encoding, entries, parsed_findings, rejected) = if entry.size > MAX_PLAYLIST {
            (
                String::new(),
                String::new(),
                Vec::new(),
                vec![Finding {
                    code: "P09",
                    line: 0,
                    text: format!("The playlist is {} bytes; not read.", entry.size),
                }],
                true,
            )
        } else {
            match fs::read(visit.dir.join(&entry.name)) {
                Ok(bytes) => {
                    let p = playlist::parse(&bytes, &suffix);
                    (p.format, p.encoding, p.entries, p.findings, p.rejected)
                }
                Err(e) => {
                    report.unreadable += 1;
                    (
                        String::new(),
                        String::new(),
                        Vec::new(),
                        vec![Finding {
                            code: "P09",
                            line: 0,
                            text: format!("The playlist could not be read: {e}"),
                        }],
                        true,
                    )
                }
            }
        };
        findings.extend(parsed_findings);
        let mut states = Vec::new();
        let mut counts: BTreeMap<&'static str, (usize, String)> = BTreeMap::new();
        let mut found = 0;
        let mut streams = 0;
        for e in &entries {
            let (state, resolved) = state_of(music, &e.uri, &folder, decoders);
            match state {
                "ok" => found += 1,
                "stream" => streams += 1,
                other => {
                    let slot = counts.entry(other).or_insert((0, e.uri.clone()));
                    slot.0 += 1;
                }
            }
            states.push(EntryState {
                line: e.line,
                uri: e.uri.clone(),
                name: e.name.clone(),
                state,
                resolved,
            });
        }
        for (state, (n, first)) in &counts {
            let (code, text) = match *state {
                "missing" => ("P01", format!("{n} {} not found where MPD looks (joined to the playlist's folder), the first \"{first}\": skipped at load without a word.", if *n == 1 { "entry" } else { "entries" })),
                "foreign" => ("P02", format!("{n} {} a foreign path (absolute, a drive letter, backslashes or file://), the first \"{first}\": nothing on this player.", if *n == 1 { "entry is" } else { "entries are" })),
                _ => ("P03", format!("{n} {} a file no decoder of this MPD claims, the first \"{first}\": not in the database, skipped.", if *n == 1 { "entry names" } else { "entries name" })),
            };
            findings.push(Finding {
                code,
                line: 0,
                text,
            });
        }
        if !rejected && (suffix == "m3u8" || suffix == "xspf") {
            findings.push(Finding {
                code: "P07",
                line: 0,
                text: format!("Volumio's browser plays .cue, .pls and .m3u only; a .{suffix} is never sent to MPD, which could play it."),
            });
        }
        findings.sort_by_key(|f| (f.line == 0, f.line, f.code));
        report.playlists += 1;
        if findings.is_empty() {
            report.clean += 1;
        } else {
            report.with_findings += 1;
        }
        for f in &findings {
            *report.by_code.entry(f.code.to_string()).or_insert(0) += 1;
        }
        if report.list.len() < LISTED {
            let total = states.len();
            states.truncate(ENTRIES_LISTED);
            report.list.push(PlaylistReport {
                path,
                format,
                encoding,
                entries: total,
                found,
                streams,
                findings,
                entry_list: states,
                entries_truncated: total > ENTRIES_LISTED,
            });
        } else {
            report.list_truncated = true;
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::mpd;
    use std::path::PathBuf;

    pub fn library(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("mpdantic-plcheck-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mk = |p: &str| fs::create_dir_all(root.join(p)).unwrap();
        let put = |p: &str, b: &[u8]| fs::write(root.join(p), b).unwrap();
        mk("NAS/Jazz/Album");
        put("NAS/Jazz/Album/01.flac", b"x");
        put("NAS/Jazz/Album/02.flac", b"x");
        put("NAS/Jazz/Album/notes.txt", b"x");
        put(
            "NAS/Jazz/good.m3u",
            b"#EXTM3U\n#EXTINF:1,One\nAlbum/01.flac\nhttp://radio.example/live\n",
        );
        put(
            "NAS/Jazz/mixed.m3u8",
            b"Album/01.flac\nD:\\Music\\Jazz\\Album\\02.flac\nAlbum/03.flac\nAlbum/notes.txt\n",
        );
        put("NAS/Jazz/bad.pls", b"[playlist]\nFile1=Album/01.flac\n");
        put(
            "NAS/Jazz/old.xspf",
            b"<playlist><trackList><track><location>file:///C:/Music/Jazz/Album/01.flac</location></track></trackList></playlist>",
        );
        put("NAS/Jazz/feed.rss", b"<rss/>");
        put("NAS/Jazz/empty.m3u", b"# nothing\n");
        root
    }

    pub fn decoders() -> Decoders {
        mpd::tests::decoders_of(&[("flac", &["flac"]), ("mad", &["mp3"])])
    }

    fn note() -> DecoderNote {
        DecoderNote {
            source: "test".to_string(),
            count: 0,
            detail: crate::text::Msg::raw(""),
        }
    }

    #[test]
    fn every_playlist_is_read_and_its_entries_placed() {
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
        assert_eq!(r.playlists, 5);
        assert_eq!(r.not_read, 1);
        assert_eq!(r.clean, 1);
        assert_eq!(r.with_findings, 4);
        let by = |p: &str| r.list.iter().find(|s| s.path == p).unwrap();
        let good = by("NAS/Jazz/good.m3u");
        assert_eq!(good.format, "extm3u");
        assert_eq!(good.entries, 2);
        assert_eq!(good.found, 1);
        assert_eq!(good.streams, 1);
        assert!(good.findings.is_empty(), "{:?}", good.findings);
        assert_eq!(good.entry_list[0].state, "ok");
        assert_eq!(
            good.entry_list[0].resolved.as_deref(),
            Some("NAS/Jazz/Album/01.flac")
        );
        assert_eq!(good.entry_list[0].name.as_deref(), Some("One"));
        assert_eq!(good.entry_list[1].state, "stream");
        let mixed = by("NAS/Jazz/mixed.m3u8");
        let states: Vec<&str> = mixed.entry_list.iter().map(|e| e.state).collect();
        assert_eq!(states, ["ok", "foreign", "missing", "not_decoded"]);
        let codes: Vec<&str> = mixed.findings.iter().map(|f| f.code).collect();
        assert_eq!(codes, ["P01", "P02", "P03", "P07"]);
        assert!(mixed.findings[3].text.contains(".m3u8"));
        let bad = by("NAS/Jazz/bad.pls");
        assert_eq!(
            bad.findings.iter().map(|f| f.code).collect::<Vec<_>>(),
            ["P06"]
        );
        assert_eq!(bad.entries, 0);
        let old = by("NAS/Jazz/old.xspf");
        assert_eq!(
            old.findings.iter().map(|f| f.code).collect::<Vec<_>>(),
            ["P02", "P07"]
        );
        assert_eq!(by("NAS/Jazz/empty.m3u").findings[0].code, "P08");
        assert_eq!(r.by_code.get("P07"), Some(&2));
        assert_eq!(r.codes.get("P06"), Some(&"a PLS MPD rejects"));
        assert!(!r.list[0].findings.is_empty(), "findings first");
        fs::remove_dir_all(&root).unwrap();
    }
}
