//! The repair of one playlist, a change of its text alone: the entries MPD cannot
//! find looked for under a base folder by the longest tail of their path and
//! rewritten relative to the playlist's own folder, which is what `load` resolves and
//! what other players read; the byte-order mark removed; the text re-encoded; a PLS
//! given the section and the count MPD requires; and for an m3u8 or an xspf, which
//! Volumio's browser never plays, a `.m3u` written beside it with the same entries.
//! Everything else in the file stays as written.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use encoding_rs::Encoding;
use serde::Serialize;

use crate::classify::{self, Class, Rules};
use crate::cuefix::{self, Repair, ENCODINGS};
use crate::mpd::Decoders;
use crate::playlist;
use crate::plcheck;
use crate::text::Msg;
use crate::walk::{self, Progress};

/// Files the base folder's index holds at most.
const INDEX_LIMIT: usize = 200_000;

/// What the user chose: the repairs by id (every one when not said), the encoding to
/// read the file as, and the folder the missing entries are looked for under.
pub struct Choice<'a> {
    pub repairs: Option<&'a [String]>,
    pub encoding: Option<&'a str>,
    pub base: &'a str,
}

/// One entry of the playlist, as it is and as it would be.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct EntryChange {
    pub line: usize,
    pub before: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// `ok`, `stream`, `rebased`, `not_found`, `not_decoded`.
    pub state: &'static str,
    /// How many files under the base matched as well as the one taken.
    pub candidates: usize,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Fixed {
    pub repairs: Vec<Repair>,
    pub encoding: Option<String>,
    pub encodings: Vec<&'static str>,
    /// The file written, MPD's form: the playlist itself, or the `.m3u` beside it.
    pub target: String,
    pub after: String,
    pub entries: Vec<EntryChange>,
    pub changed: bool,
    pub not_found: usize,
    pub notes: Vec<Msg>,
}

/// The audio files under the base folder, by their lowercased name, each with its
/// library path: one bounded walk, the lines in force respected.
pub(crate) struct Index {
    by_name: HashMap<String, Vec<String>>,
    pub(crate) truncated: bool,
}

impl Index {
    pub(crate) fn build(music: &Path, base: &str, decoders: &Decoders) -> Result<Index, Msg> {
        let start = walk::start(music, base)?;
        let rules = Rules::new();
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        let mut count = 0usize;
        let cancel = AtomicBool::new(false);
        walk::walk(start, &Progress::default(), &cancel, |visit| {
            let mut enter = Vec::new();
            for (i, e) in visit.entries.iter().enumerate() {
                let (class, _) = classify::classify(&rules, &e.name, e.is_dir, decoders);
                if e.is_dir {
                    if class == Class::Folder {
                        enter.push(i);
                    }
                } else if class == Class::Audio {
                    if count >= INDEX_LIMIT {
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                        break;
                    }
                    count += 1;
                    by_name
                        .entry(e.name.to_string_lossy().to_lowercase())
                        .or_default()
                        .push(e.path.clone());
                }
            }
            enter
        });
        Ok(Index {
            by_name,
            truncated: count >= INDEX_LIMIT,
        })
    }

    /// The library path whose tail matches the entry's the longest, and how many
    /// matched as well.
    pub(crate) fn find(&self, parts: &[String]) -> Option<(String, usize)> {
        let name = parts.last()?;
        let candidates = self.by_name.get(name)?;
        let mut best: Option<(&String, usize)> = None;
        let mut ties = 0;
        for path in candidates {
            let tail: Vec<String> = path.rsplit('/').map(|s| s.to_lowercase()).collect();
            // tail[0] is the name; count the folders that agree, innermost first.
            let score = parts
                .iter()
                .rev()
                .skip(1)
                .zip(tail.iter().skip(1))
                .take_while(|(a, b)| a == b)
                .count();
            match best {
                Some((_, s)) if score < s => {}
                Some((p, s)) if score == s => {
                    ties += 1;
                    if path < p {
                        best = Some((path, score));
                    }
                }
                _ => {
                    best = Some((path, score));
                    ties = 0;
                }
            }
        }
        best.map(|(p, _)| (p.clone(), ties))
    }
}

/// `%41` to `A`; what is not percent-encoded stays.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(n) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(n);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The entry as components to match: the scheme and the host of a `file://` off,
/// percent-decoding undone, backslashes turned, a drive letter dropped, dot segments
/// out, everything lowercased.
pub(crate) fn components(uri: &str) -> Vec<String> {
    let mut s = uri.to_string();
    let lower = s.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("file://") {
        // file:///C:/x, file://host/share/x, file:/x.
        let rest_original = &s[s.len() - rest.len()..];
        s = percent_decode(rest_original);
        if !s.starts_with('/') {
            // A host before the path: dropped.
            s = s.find('/').map(|i| s[i..].to_string()).unwrap_or_default();
        }
    }
    s = s.replace('\\', "/");
    let bytes = s.as_bytes();
    if bytes.len() > 1 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        s = s[2..].to_string();
    } else if bytes.len() > 2
        && bytes[0] == b'/'
        && bytes[2] == b':'
        && bytes[1].is_ascii_alphabetic()
    {
        s = s[3..].to_string();
    }
    s.split('/')
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .map(|c| c.to_lowercase())
        .collect()
}

/// The path from the playlist's folder to a library path, `..` where it climbs.
fn relative(from_folder: &str, to_path: &str) -> String {
    let from: Vec<&str> = from_folder.split('/').filter(|s| !s.is_empty()).collect();
    let to: Vec<&str> = to_path.split('/').collect();
    let common = from
        .iter()
        .zip(to.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    parts.join("/")
}

fn folder_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn stem_m3u(path: &str) -> String {
    match path.rsplit_once('.') {
        Some((stem, _)) if !stem.ends_with('/') && !stem.is_empty() => format!("{stem}.m3u"),
        _ => format!("{path}.m3u"),
    }
}

/// The playlist's text as it would be after the repairs chosen, with the repairs it
/// allows. Nothing is written.
pub fn plan(
    music: &Path,
    path: &str,
    bytes: &[u8],
    decoders: &Decoders,
    choice: &Choice,
) -> Result<Fixed, Msg> {
    let chosen = |id: &str| match choice.repairs {
        None => true,
        Some(list) => list.iter().any(|r| r == id),
    };
    let suffix = classify::suffix(path).unwrap_or_default();
    let format = playlist::format_of(&suffix)
        .ok_or_else(|| msg!("core.plfix.not_playlist", "not a playlist"))?;
    let folder = folder_of(path).to_string();
    let mut fixed = Fixed::default();
    // The bytes: a UTF-16 mark, a UTF-8 mark, UTF-8 or not; as the sheets.
    let utf16 = Encoding::for_bom(bytes)
        .filter(|(e, _)| *e == encoding_rs::UTF_16LE || *e == encoding_rs::UTF_16BE);
    let (body, bom) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (rest, true),
        None => (bytes, false),
    };
    let text = match (utf16, std::str::from_utf8(body)) {
        (None, Ok(s)) => s.to_string(),
        (utf16, _) => {
            let (enc, content) = match utf16 {
                Some((enc, mark)) => (enc, &bytes[mark..]),
                None => {
                    let label = choice.encoding.unwrap_or(ENCODINGS[0]);
                    if !ENCODINGS.contains(&label) {
                        return Err(cuefix::not_offered(label));
                    }
                    let enc = Encoding::for_label(label.as_bytes())
                        .ok_or_else(|| cuefix::not_an_encoding(label))?;
                    (enc, body)
                }
            };
            let (decoded, had_errors) = enc.decode_without_bom_handling(content);
            let decoded = decoded.into_owned();
            if had_errors {
                fixed.notes.push(cuefix::replacement_marks(
                    decoded.matches('\u{FFFD}').count(),
                    enc.name(),
                ));
            }
            if utf16.is_none() {
                fixed.encodings = ENCODINGS.to_vec();
            }
            fixed.encoding = Some(enc.name().to_ascii_lowercase());
            fixed.repairs.push(Repair {
                id: "encoding",
                what: cuefix::read_as(enc.name()),
                on: true,
            });
            decoded
        }
    };
    if bom {
        fixed.repairs.push(Repair {
            id: "bom",
            what: cuefix::bom_removed(),
            on: chosen("bom"),
        });
    }
    // The entries as written, from the text without the mark.
    let parsed = playlist::parse_text(&text, &suffix, false, Vec::new());
    // Where each stands, and what the rebase would make of it.
    let mut index: Option<Index> = None;
    let mut rebased = 0usize;
    let mut not_found = 0usize;
    let mut rebase_applies = false;
    for e in &parsed.entries {
        let (state, _) = plcheck::state_of(music, &e.uri, &folder, decoders);
        let mut change = EntryChange {
            line: e.line,
            before: e.uri.clone(),
            after: None,
            state,
            candidates: 0,
        };
        if matches!(state, "missing" | "foreign") {
            rebase_applies = true;
            if chosen("rebase") {
                if index.is_none() {
                    index = Some(Index::build(music, choice.base, decoders)?);
                }
                let parts = components(&e.uri);
                match index.as_ref().and_then(|i| i.find(&parts)) {
                    Some((library_path, ties)) => {
                        let rel = relative(&folder, &library_path);
                        if rel != e.uri {
                            change.after = Some(rel);
                        }
                        change.state = "rebased";
                        change.candidates = ties;
                        rebased += 1;
                    }
                    None => {
                        change.state = "not_found";
                        not_found += 1;
                    }
                }
            }
        }
        fixed.entries.push(change);
    }
    if let Some(i) = &index {
        if i.truncated {
            fixed.notes.push(msg!(
                "core.plfix.index_truncated",
                "the base folder holds more than {limit} audio files; only the first were looked at",
                limit = INDEX_LIMIT
            ));
        }
    }
    if rebase_applies {
        let base = if choice.base.is_empty() {
            msg!("core.plfix.library", "the library")
        } else {
            Msg::raw(choice.base)
        };
        fixed.repairs.push(Repair {
            id: "rebase",
            what: if chosen("rebase") {
                msg!(
                    "core.plfix.rebase_done",
                    "the entries MPD cannot find looked for under {base}: {rebased} found and written relative to the playlist's folder, {not_found} not found",
                    base = base,
                    rebased = rebased,
                    not_found = not_found
                )
            } else {
                msg!("core.plfix.rebase", "the entries MPD cannot find looked for under {base}", base = base)
            },
            on: chosen("rebase"),
        });
    }
    fixed.not_found = not_found;
    // The text rewritten per format, and the file it goes into.
    let lines: Vec<&str> = text.split('\n').collect();
    let after_for = |change: &EntryChange| change.after.clone();
    let to_m3u = matches!(suffix.as_str(), "m3u8" | "xspf");
    if to_m3u {
        fixed.repairs.push(Repair {
            id: "m3u",
            what: msg!(
                "core.plfix.m3u_beside",
                "a .m3u written beside the .{suffix} with the same entries, which Volumio's browser plays",
                suffix = suffix
            ),
            on: chosen("m3u"),
        });
    }
    let write_m3u_beside = to_m3u && chosen("m3u");
    let mut out = String::new();
    match format {
        "xspf" => {
            if write_m3u_beside {
                out.push_str("#EXTM3U\n");
                for (e, change) in parsed.entries.iter().zip(&fixed.entries) {
                    if let Some(name) = &e.name {
                        out.push_str(&format!("#EXTINF:-1,{name}\n"));
                    }
                    out.push_str(&after_for(change).unwrap_or_else(|| e.uri.clone()));
                    out.push('\n');
                }
            } else {
                fixed.notes.push(msg!(
                    "core.plfix.xspf_note",
                    "an XSPF is not rewritten in place; the .m3u beside it carries the entries"
                ));
            }
        }
        "pls" => {
            let mut section = false;
            let mut count = false;
            let mut max_index = 0usize;
            let mut rewritten: Vec<String> = Vec::new();
            for (i, raw) in lines.iter().enumerate() {
                let (line, cr) = match raw.strip_suffix('\r') {
                    Some(l) => (l, "\r"),
                    None => (*raw, ""),
                };
                let trimmed = line.trim_matches(|c: char| c.is_ascii_whitespace());
                if trimmed.eq_ignore_ascii_case("[playlist]") {
                    section = true;
                }
                if trimmed.len() > 16 && trimmed[..15].eq_ignore_ascii_case("NumberOfEntries") {
                    count = true;
                }
                let mut new_line = line.to_string();
                if let Some((name, _)) = trimmed.split_once('=') {
                    let name = name.trim_matches(|c: char| c.is_ascii_whitespace());
                    if name.len() > 4 && name[..4].eq_ignore_ascii_case("File") {
                        if let Ok(n) = name[4..].parse::<usize>() {
                            max_index = max_index.max(n);
                        }
                    }
                }
                if let Some(change) = fixed.entries.iter().find(|c| c.line == i + 1) {
                    if let Some(after) = after_for(change) {
                        if let Some((name, _)) = line.split_once('=') {
                            new_line = format!("{name}={after}");
                        }
                    }
                }
                rewritten.push(format!("{new_line}{cr}"));
            }
            let pls_fix = parsed.rejected || !count;
            if pls_fix {
                fixed.repairs.push(Repair {
                    id: "pls",
                    what: msg!(
                        "core.plfix.pls",
                        "the [playlist] section and the NumberOfEntries line MPD requires"
                    ),
                    on: chosen("pls"),
                });
                if chosen("pls") {
                    if !section {
                        rewritten.insert(0, "[playlist]".to_string());
                    }
                    if !count {
                        let last_blank = rewritten
                            .last()
                            .map(|l| l.trim().is_empty())
                            .unwrap_or(false);
                        let line = format!("NumberOfEntries={max_index}");
                        if last_blank {
                            let n = rewritten.len();
                            rewritten.insert(n - 1, line);
                        } else {
                            rewritten.push(line);
                        }
                    }
                }
            }
            out = rewritten.join("\n");
        }
        _ => {
            let mut rewritten: Vec<String> = Vec::with_capacity(lines.len());
            for (i, raw) in lines.iter().enumerate() {
                let (line, cr) = match raw.strip_suffix('\r') {
                    Some(l) => (l, "\r"),
                    None => (*raw, ""),
                };
                let new_line = fixed
                    .entries
                    .iter()
                    .find(|c| c.line == i + 1)
                    .and_then(after_for)
                    .unwrap_or_else(|| line.to_string());
                rewritten.push(format!("{new_line}{cr}"));
            }
            out = rewritten.join("\n");
        }
    }
    if bom && !chosen("bom") && !write_m3u_beside {
        out.insert(0, '\u{FEFF}');
    }
    fixed.target = if write_m3u_beside {
        stem_m3u(path)
    } else {
        path.to_string()
    };
    fixed.after = out;
    Ok(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plcheck::tests::{decoders, library};
    use std::fs;

    const ALL: Choice = Choice {
        repairs: None,
        encoding: None,
        base: "NAS",
    };

    fn ids(f: &Fixed) -> Vec<(&'static str, bool)> {
        f.repairs.iter().map(|r| (r.id, r.on)).collect()
    }

    #[test]
    fn the_entries_are_found_under_the_base_and_written_relative_to_the_playlist() {
        let root = library("rebase");
        let bytes = fs::read(root.join("NAS/Jazz/mixed.m3u8")).unwrap();
        let f = plan(&root, "NAS/Jazz/mixed.m3u8", &bytes, &decoders(), &ALL).unwrap();
        assert_eq!(ids(&f), [("rebase", true), ("m3u", true)]);
        assert_eq!(f.target, "NAS/Jazz/mixed.m3u");
        let states: Vec<&str> = f.entries.iter().map(|e| e.state).collect();
        assert_eq!(states, ["ok", "rebased", "not_found", "not_decoded"]);
        assert_eq!(f.entries[1].after.as_deref(), Some("Album/02.flac"));
        assert_eq!(f.entries[1].candidates, 0);
        assert_eq!(f.not_found, 1);
        assert_eq!(
            f.after,
            "Album/01.flac\nAlbum/02.flac\nAlbum/03.flac\nAlbum/notes.txt\n"
        );
        assert!(f.repairs[0].what.contains("1 found"));
        // Not as a .m3u beside it: the m3u8 itself.
        let in_place = Choice {
            repairs: Some(&["rebase".to_string()]),
            encoding: None,
            base: "NAS",
        };
        let f = plan(&root, "NAS/Jazz/mixed.m3u8", &bytes, &decoders(), &in_place).unwrap();
        assert_eq!(f.target, "NAS/Jazz/mixed.m3u8");
        assert_eq!(ids(&f), [("rebase", true), ("m3u", false)]);
        // A playlist in a folder above: the entry climbs with "..".
        fs::write(
            root.join("NAS/list.m3u"),
            "/mnt/NAS/Jazz/Album/01.flac\r\n# keep\r\nfile:///C:/Music/Jazz/Album/02.flac\r\n",
        )
        .unwrap();
        let bytes = fs::read(root.join("NAS/list.m3u")).unwrap();
        let f = plan(&root, "NAS/list.m3u", &bytes, &decoders(), &ALL).unwrap();
        assert_eq!(f.target, "NAS/list.m3u");
        assert_eq!(
            f.after,
            "Jazz/Album/01.flac\r\n# keep\r\nJazz/Album/02.flac\r\n"
        );
        assert_eq!(f.entries[0].state, "rebased");
        fs::write(root.join("NAS/Jazz/Album/up.m3u"), "..\\..\\Rock\\x.flac\n").unwrap();
        fs::create_dir_all(root.join("NAS/Rock")).unwrap();
        fs::write(root.join("NAS/Rock/x.flac"), b"x").unwrap();
        let bytes = fs::read(root.join("NAS/Jazz/Album/up.m3u")).unwrap();
        let f = plan(&root, "NAS/Jazz/Album/up.m3u", &bytes, &decoders(), &ALL).unwrap();
        assert_eq!(f.after, "../../Rock/x.flac\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_pls_gets_its_count_and_an_xspf_a_m3u_beside_it() {
        let root = library("pls");
        let bytes = fs::read(root.join("NAS/Jazz/bad.pls")).unwrap();
        let f = plan(&root, "NAS/Jazz/bad.pls", &bytes, &decoders(), &ALL).unwrap();
        assert_eq!(ids(&f), [("pls", true)]);
        assert_eq!(
            f.after,
            "[playlist]\nFile1=Album/01.flac\nNumberOfEntries=1\n"
        );
        assert_eq!(f.target, "NAS/Jazz/bad.pls");
        let bytes = fs::read(root.join("NAS/Jazz/old.xspf")).unwrap();
        let f = plan(&root, "NAS/Jazz/old.xspf", &bytes, &decoders(), &ALL).unwrap();
        assert_eq!(ids(&f), [("rebase", true), ("m3u", true)]);
        assert_eq!(f.target, "NAS/Jazz/old.m3u");
        assert_eq!(f.after, "#EXTM3U\nAlbum/01.flac\n");
        assert_eq!(f.entries[0].before, "file:///C:/Music/Jazz/Album/01.flac");
        assert_eq!(f.entries[0].after.as_deref(), Some("Album/01.flac"));
        // Marked and Latin-1 playlists, as the sheets.
        fs::write(
            root.join("NAS/Jazz/m.m3u"),
            b"\xEF\xBB\xBF#EXTM3U\nAlbum/01.flac\n",
        )
        .unwrap();
        let bytes = fs::read(root.join("NAS/Jazz/m.m3u")).unwrap();
        let f = plan(&root, "NAS/Jazz/m.m3u", &bytes, &decoders(), &ALL).unwrap();
        assert_eq!(ids(&f), [("bom", true)]);
        assert_eq!(f.after, "#EXTM3U\nAlbum/01.flac\n");
        fs::write(root.join("NAS/Jazz/l.m3u"), b"Caf\xe9/01.flac\n").unwrap();
        let bytes = fs::read(root.join("NAS/Jazz/l.m3u")).unwrap();
        let f = plan(&root, "NAS/Jazz/l.m3u", &bytes, &decoders(), &ALL).unwrap();
        assert_eq!(f.repairs[0].id, "encoding");
        assert_eq!(f.encoding.as_deref(), Some("windows-1252"));
        // Read as Windows-1252 the entry is "Cafe/01.flac" with its accent, which is
        // nowhere; the rebase finds the file by its name under the base.
        assert_eq!(f.entries[0].before, "Caf\u{e9}/01.flac");
        assert_eq!(f.entries[0].state, "rebased");
        assert_eq!(f.after, "Album/01.flac\n");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn components_and_relative_paths_are_made_as_the_rebase_needs_them() {
        assert_eq!(
            components("D:\\Music\\Jazz\\01.flac"),
            ["music", "jazz", "01.flac"]
        );
        assert_eq!(
            components("file:///C:/Music/Jazz/01.flac"),
            ["music", "jazz", "01.flac"]
        );
        assert_eq!(
            components("file://nas/share/Jazz/01%20two.flac"),
            ["share", "jazz", "01 two.flac"]
        );
        assert_eq!(components("/mnt/NAS/x.flac"), ["mnt", "nas", "x.flac"]);
        assert_eq!(components("../a/./b.flac"), ["a", "b.flac"]);
        assert_eq!(
            relative("NAS/Jazz", "NAS/Jazz/Album/01.flac"),
            "Album/01.flac"
        );
        assert_eq!(
            relative("NAS/Jazz/Album", "NAS/Rock/x.flac"),
            "../../Rock/x.flac"
        );
        assert_eq!(relative("", "NAS/x.flac"), "NAS/x.flac");
        assert_eq!(percent_decode("a%20b%zz"), "a b%zz");
        assert_eq!(stem_m3u("NAS/a.b/list.m3u8"), "NAS/a.b/list.m3u");
        assert_eq!(stem_m3u("NAS/list"), "NAS/list.m3u");
    }
}
