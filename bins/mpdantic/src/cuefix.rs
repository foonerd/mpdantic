//! The repairs of a CUE sheet, each a change of the sheet's text alone, offered from
//! what MPD would make of the sheet and chosen by the user: the byte-order mark
//! removed, the text re-encoded to UTF-8, a `FILE` line's type keyword, name and
//! quotes rewritten to the file that is beside the sheet; or the sheet removed where
//! two sheets, or a sheet and its split tracks, put the album in the library twice.
//! Everything else stays as written: the line endings, the comments, the indentation,
//! the lines MPD ignores.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use encoding_rs::Encoding;
use serde::Serialize;

use crate::classify;
use crate::cue::{self, Finding, FILE_TYPES};
use crate::cuecheck::{Beside, MAX_SHEET};
use crate::mpd::Decoders;
use crate::text::Msg;

/// The encodings a sheet that is not UTF-8 is offered to be read as, the first the
/// default. Every one a WHATWG label encoding_rs knows.
pub const ENCODINGS: &[&str] = &[
    "windows-1252",
    "windows-1250",
    "windows-1251",
    "iso-8859-2",
    "iso-8859-15",
    "macintosh",
    "koi8-r",
    "shift_jis",
    "euc-jp",
    "gbk",
    "big5",
    "euc-kr",
];

/// One repair the sheet allows, and whether it is in the batch.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Repair {
    /// `bom`, `encoding`, `type`, `name`, `quote`, `remove`.
    pub id: &'static str,
    pub what: Msg,
    pub on: bool,
}

/// The sheet as it would be after the repairs chosen.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Fixed {
    pub repairs: Vec<Repair>,
    /// The encoding the text was read as, when it was not UTF-8.
    pub encoding: Option<String>,
    /// The encodings on offer, when the user can choose one.
    pub encodings: Vec<&'static str>,
    /// The text to write; empty when the sheet is removed.
    pub after: String,
    pub remove: bool,
    pub lines_changed: usize,
    /// What MPD's parser would still find in the text after.
    pub remaining: Vec<Finding>,
    pub notes: Vec<Msg>,
    /// Whether anything would be written at all.
    pub changed: bool,
}

/// The encodings' repairs and notes, the same for a sheet and a playlist.
pub fn not_offered(label: &str) -> Msg {
    msg!(
        "core.encoding.not_offered",
        "{label} is not an encoding offered here",
        label = label
    )
}

pub fn not_an_encoding(label: &str) -> Msg {
    msg!(
        "core.encoding.not_encoding",
        "{label}: not an encoding",
        label = label
    )
}

pub fn replacement_marks(n: usize, encoding: &str) -> Msg {
    msg!(
        "core.encoding.replacement_marks",
        "{n} characters could not be read as {encoding}; each shows as a replacement mark",
        n = n,
        encoding = encoding
    )
}

pub fn read_as(encoding: &str) -> Msg {
    msg!(
        "core.encoding.read_as",
        "the text read as {encoding} and written as UTF-8",
        encoding = encoding
    )
}

pub fn bom_removed() -> Msg {
    msg!(
        "core.encoding.bom_removed",
        "the byte-order mark removed, so MPD sees the first line"
    )
}

/// What the user chose: the repairs by id (every one but `remove` when not said),
/// and the encoding to read the sheet as.
pub struct Choice<'a> {
    pub repairs: Option<&'a [String]>,
    pub encoding: Option<&'a str>,
}

/// A `FILE` line as written: the indentation, the name as a human reads it, the
/// type, whether the name was quoted, anything after the type.
struct FileLine {
    indent: String,
    name: String,
    kind: Option<String>,
    quoted: bool,
    rest: String,
}

/// A line's `FILE` command, as MPD's parser finds it (the keyword, then a quoted
/// value or words). An unquoted name takes every word before a type-looking last
/// word, which is what the writer meant and MPD does not see.
fn file_line(line: &str) -> Option<FileLine> {
    let trimmed = line.trim_start();
    let indent = line[..line.len() - trimmed.len()].to_string();
    let rest = trimmed.strip_prefix("FILE")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    if let Some(after_quote) = rest.strip_prefix('"') {
        let (name, tail) = match after_quote.find('"') {
            Some(end) => (&after_quote[..end], &after_quote[end + 1..]),
            None => (after_quote, ""),
        };
        let tail = tail.trim_start();
        let kind = tail.split_whitespace().next().map(str::to_string);
        let rest = kind
            .as_ref()
            .map(|k| tail[k.len()..].trim_end().to_string())
            .unwrap_or_default();
        return Some(FileLine {
            indent,
            name: name.to_string(),
            kind,
            quoted: true,
            rest,
        });
    }
    let rest = rest.trim_end();
    let words: Vec<&str> = rest.split_whitespace().collect();
    let last = *words.last()?;
    let looks_like_type = words.len() >= 2
        && !last.contains('.')
        && last
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if looks_like_type {
        Some(FileLine {
            indent,
            name: rest[..rest.len() - last.len()].trim_end().to_string(),
            kind: Some(last.to_string()),
            quoted: false,
            rest: String::new(),
        })
    } else {
        Some(FileLine {
            indent,
            name: rest.to_string(),
            kind: None,
            quoted: false,
            rest: String::new(),
        })
    }
}

/// The type keyword for a name: `MP3` and `AIFF` where the suffix says so, `WAVE`
/// for everything else, which MPD takes and other tools write and read.
fn keyword_for(name: &str) -> &'static str {
    match classify::suffix(name).as_deref() {
        Some("mp3") => "MP3",
        Some("aif") | Some("aiff") | Some("aifc") => "AIFF",
        _ => "WAVE",
    }
}

/// The name beside the sheet a `FILE` name should read as, when there is one: the
/// name in its own case, with its own suffix, or the last part of a foreign path
/// found beside the sheet.
fn target_for(name: &str, dir: &Path, beside: &Beside, decoders: &Decoders) -> Option<String> {
    // For `case` and `suffix` the name found is carried as it is.
    match beside.locate(name, dir, decoders) {
        ("case", found) | ("suffix", found) => found.map(|m| m.en),
        ("foreign", _) => {
            let base = name.rsplit(['/', '\\']).next().unwrap_or("");
            if base.is_empty() || base == name {
                return None;
            }
            match beside.locate(base, dir, decoders) {
                ("ok", _) | ("not_decoded", _) => Some(base.to_string()),
                ("case", found) | ("suffix", found) => found.map(|m| m.en),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether the sheet doubles the album: another sheet beside it names one of its
/// files (C11), or it names one file and three or more audio files beside it belong
/// to no sheet (C12).
fn doubled(
    dir: &Path,
    sheet_name: &str,
    this: &cue::Sheet,
    beside: &Beside,
    decoders: &Decoders,
) -> (bool, bool) {
    let mine: Vec<&str> = this
        .files
        .iter()
        .filter(|f| f.accepted && beside.locate(&f.name, dir, decoders).0 == "ok")
        .map(|f| f.name.as_str())
        .collect();
    let mut referenced: HashSet<String> = mine.iter().map(|s| s.to_string()).collect();
    let mut twice = false;
    for name in beside.names() {
        if name == sheet_name || classify::suffix(name).as_deref() != Some(classify::CUE_SUFFIX) {
            continue;
        }
        let Ok(bytes) = fs::read(dir.join(name)) else {
            continue;
        };
        if bytes.len() as u64 > MAX_SHEET {
            continue;
        }
        for f in cue::parse(&bytes).files.iter().filter(|f| f.accepted) {
            if mine.contains(&f.name.as_str()) {
                twice = true;
            }
            referenced.insert(f.name.clone());
        }
    }
    let unreferenced = beside
        .audio(decoders)
        .iter()
        .filter(|n| !referenced.contains(**n))
        .count();
    (twice, mine.len() == 1 && unreferenced >= 3)
}

/// The sheet's text as it would be after the repairs chosen, with the repairs it
/// allows. Nothing is written.
pub fn plan(
    bytes: &[u8],
    dir: &Path,
    sheet_name: &str,
    beside: &Beside,
    decoders: &Decoders,
    choice: &Choice,
) -> Result<Fixed, Msg> {
    let chosen = |id: &str| match choice.repairs {
        None => id != "remove",
        Some(list) => list.iter().any(|r| r == id),
    };
    let mut fixed = Fixed::default();
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
                        return Err(not_offered(label));
                    }
                    let enc = Encoding::for_label(label.as_bytes())
                        .ok_or_else(|| not_an_encoding(label))?;
                    (enc, body)
                }
            };
            let (decoded, had_errors) = enc.decode_without_bom_handling(content);
            let decoded = decoded.into_owned();
            if had_errors {
                fixed.notes.push(replacement_marks(
                    decoded.matches('\u{FFFD}').count(),
                    enc.name(),
                ));
            }
            if utf16.is_none() {
                fixed.encodings = ENCODINGS.to_vec();
            }
            fixed.encoding = Some(enc.name().to_ascii_lowercase());
            // Forced: the sheet is written as UTF-8 with any change.
            fixed.repairs.push(Repair {
                id: "encoding",
                what: read_as(enc.name()),
                on: true,
            });
            decoded
        }
    };
    if bom {
        fixed.repairs.push(Repair {
            id: "bom",
            what: bom_removed(),
            on: chosen("bom"),
        });
    }
    // The FILE lines, each against what is beside the sheet.
    let mut types: Vec<Msg> = Vec::new();
    let mut names: Vec<Msg> = Vec::new();
    let mut quotes: Vec<Msg> = Vec::new();
    let mut out = String::with_capacity(text.len() + 64);
    let mut lines_changed = 0;
    for (i, raw) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let (line, cr) = match raw.strip_suffix('\r') {
            Some(l) => (l, "\r"),
            None => (raw, ""),
        };
        let Some(f) = file_line(line) else {
            out.push_str(raw);
            continue;
        };
        let line_no = i + 1;
        let mut name = f.name.clone();
        let mut kind = f.kind.clone();
        let mut quoted = f.quoted;
        if let Some(target) = target_for(&f.name, dir, beside, decoders) {
            names.push(msg!(
                "core.cuefix.name_change",
                "line {line}: \"{from}\" to \"{to}\"",
                line = line_no,
                from = f.name,
                to = target
            ));
            if chosen("name") {
                name = target;
            }
        }
        let bad_type = f
            .kind
            .as_deref()
            .map(|k| !FILE_TYPES.contains(&k))
            .unwrap_or(true);
        if bad_type {
            let keyword = keyword_for(&name);
            types.push(match &f.kind {
                Some(k) => msg!(
                    "core.cuefix.type_change",
                    "line {line}: {from} to {to}",
                    line = line_no,
                    from = k,
                    to = keyword
                ),
                None => msg!(
                    "core.cuefix.type_added",
                    "line {line}: no type, {keyword} added",
                    line = line_no,
                    keyword = keyword
                ),
            });
            if chosen("type") {
                kind = Some(keyword.to_string());
            }
        }
        if !f.quoted && f.name.contains(char::is_whitespace) {
            quotes.push(msg!(
                "core.cuefix.quote_line",
                "line {line}: \"{name}\"",
                line = line_no,
                name = f.name
            ));
            if chosen("quote") {
                quoted = true;
            }
        }
        if name != f.name || kind != f.kind || quoted != f.quoted {
            // A rewritten line is always quoted; that is never wrong.
            out.push_str(&f.indent);
            out.push_str("FILE \"");
            out.push_str(&name);
            out.push('"');
            if let Some(k) = &kind {
                out.push(' ');
                out.push_str(k);
            }
            out.push_str(&f.rest);
            lines_changed += 1;
        } else {
            out.push_str(line);
        }
        out.push_str(cr);
    }
    if !types.is_empty() {
        fixed.repairs.push(Repair {
            id: "type",
            what: msg!(
                "core.cuefix.type",
                "the FILE type MPD takes: {changes}",
                changes = types
            ),
            on: chosen("type"),
        });
    }
    if !names.is_empty() {
        fixed.repairs.push(Repair {
            id: "name",
            what: msg!(
                "core.cuefix.name",
                "the FILE name as the file beside the sheet: {changes}",
                changes = names
            ),
            on: chosen("name"),
        });
    }
    if !quotes.is_empty() {
        fixed.repairs.push(Repair {
            id: "quote",
            what: msg!(
                "core.cuefix.quote",
                "the FILE name quoted, so MPD takes all of it: {changes}",
                changes = quotes
            ),
            on: chosen("quote"),
        });
    }
    let parsed = cue::parse(bytes);
    let (twice, split) = doubled(dir, sheet_name, &parsed, beside, decoders);
    if twice || split {
        let why = if twice {
            msg!(
                "core.cuefix.remove_twice",
                "another sheet beside it names the same file, so MPD indexes the tracks twice"
            )
        } else {
            msg!(
                "core.cuefix.remove_split",
                "it names one file and the album's tracks are beside it as files too"
            )
        };
        fixed.repairs.push(Repair {
            id: "remove",
            what: msg!(
                "core.cuefix.remove",
                "the sheet removed: {why}; the undo puts it back",
                why = why
            ),
            on: chosen("remove"),
        });
    }
    if bom && !chosen("bom") {
        out.insert(0, '\u{FEFF}');
    }
    fixed.remove = fixed.repairs.iter().any(|r| r.id == "remove" && r.on);
    if fixed.remove {
        fixed.changed = true;
    } else {
        fixed.changed = out.as_bytes() != bytes;
        fixed.lines_changed = lines_changed;
        fixed.remaining = cue::parse(out.as_bytes()).findings;
        fixed.after = out;
    }
    Ok(fixed)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::mpd;
    use std::path::PathBuf;

    fn folder(name: &str, files: &[(&str, &[u8])]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpdantic-cuefix-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (n, b) in files {
            fs::write(dir.join(n), b).unwrap();
        }
        dir
    }

    fn decoders() -> Decoders {
        mpd::tests::decoders_of(&[("flac", &["flac"]), ("mad", &["mp3"])])
    }

    fn ids(f: &Fixed) -> Vec<(&'static str, bool)> {
        f.repairs.iter().map(|r| (r.id, r.on)).collect()
    }

    const ALL: Choice = Choice {
        repairs: None,
        encoding: None,
    };

    #[test]
    fn the_file_lines_are_rewritten_to_what_is_beside_the_sheet_and_nothing_else_moves() {
        let sheet = b"\xEF\xBB\xBFFILE \"disc one.flac\" BINARY\r\n  TRACK 01 AUDIO\r\n    TITLE \"A \\\"quoted\\\" title\"\r\n    INDEX 01 00:00:00\r\nFILE D:\\Music\\Album.flac WAVE\r\nFILE Disc Two.flac\r\n  TRACK 02 AUDIO\r\n    INDEX 01 00:00:00\r\nFILE \"album.wav\" WAVE\r\nFILE \"album.flac\" WAVE extra\r\n";
        let dir = folder(
            "lines",
            &[
                ("Disc One.flac", b"x"),
                ("Disc Two.flac", b"x"),
                ("album.flac", b"x"),
                ("album.cue", b""),
                ("x.cue", sheet),
            ],
        );
        let beside = Beside::read(&dir);
        let f = plan(sheet, &dir, "x.cue", &beside, &decoders(), &ALL).unwrap();
        assert_eq!(
            ids(&f),
            [
                ("bom", true),
                ("type", true),
                ("name", true),
                ("quote", true)
            ]
        );
        assert_eq!(
            f.after,
            "FILE \"Disc One.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    TITLE \"A \\\"quoted\\\" title\"\r\n    INDEX 01 00:00:00\r\nFILE \"album.flac\" WAVE\r\nFILE \"Disc Two.flac\" WAVE\r\n  TRACK 02 AUDIO\r\n    INDEX 01 00:00:00\r\nFILE \"album.flac\" WAVE\r\nFILE \"album.flac\" WAVE extra\r\n"
        );
        assert_eq!(f.lines_changed, 4);
        assert!(f.changed);
        assert!(!f.remove);
        assert!(f.encoding.is_none());
        // What the text still gives MPD: several FILE entries, and that is all.
        assert!(
            f.remaining.iter().all(|r| r.code == "C10"),
            "{:?}",
            f.remaining
        );
        // One repair only: the type; the name stays, the mark stays, a rewritten line
        // is quoted all the same.
        let only_type = Choice {
            repairs: Some(&["type".to_string()]),
            encoding: None,
        };
        let f = plan(sheet, &dir, "x.cue", &beside, &decoders(), &only_type).unwrap();
        assert_eq!(
            ids(&f),
            [
                ("bom", false),
                ("type", true),
                ("name", false),
                ("quote", false)
            ]
        );
        assert!(f
            .after
            .starts_with("\u{FEFF}FILE \"disc one.flac\" WAVE\r\n"));
        assert!(f.after.contains("\r\nFILE D:\\Music\\Album.flac WAVE\r\n"));
        assert!(f
            .after
            .contains("\r\nFILE \"Disc Two.flac\" WAVE\r\n  TRACK 02"));
        assert_eq!(f.lines_changed, 2);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sheet_that_is_not_utf8_is_read_as_the_encoding_chosen_and_always_re_encoded() {
        let sheet =
            b"TITLE \"Ma\xb3y\"\nFILE \"album.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n";
        let dir = folder("latin", &[("album.flac", b"x"), ("x.cue", sheet)]);
        let beside = Beside::read(&dir);
        let f = plan(sheet, &dir, "x.cue", &beside, &decoders(), &ALL).unwrap();
        assert_eq!(ids(&f), [("encoding", true)]);
        assert_eq!(f.encoding.as_deref(), Some("windows-1252"));
        assert_eq!(f.encodings, ENCODINGS);
        assert!(f.after.starts_with("TITLE \"Ma\u{b3}y\"\n"));
        assert!(f.changed);
        assert!(f.remaining.is_empty(), "{:?}", f.remaining);
        let polish = Choice {
            repairs: Some(&["type".to_string()]),
            encoding: Some("windows-1250"),
        };
        let f = plan(sheet, &dir, "x.cue", &beside, &decoders(), &polish).unwrap();
        assert_eq!(ids(&f), [("encoding", true)]);
        assert!(f.after.starts_with("TITLE \"Ma\u{142}y\"\n"));
        let bad = Choice {
            repairs: None,
            encoding: Some("utf-7"),
        };
        assert!(plan(sheet, &dir, "x.cue", &beside, &decoders(), &bad).is_err());
        // Bytes the encoding cannot read (a single-byte code page reads every byte;
        // a multi-byte one does not): said, shown as a mark, written all the same.
        let broken = b"TITLE \"\x81\"\nFILE \"album.flac\" WAVE\n";
        let japanese = Choice {
            repairs: None,
            encoding: Some("shift_jis"),
        };
        let f = plan(broken, &dir, "x.cue", &beside, &decoders(), &japanese).unwrap();
        assert_eq!(f.notes.len(), 1, "{:?}", f.notes);
        assert!(f.after.contains('\u{FFFD}'));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_utf16_sheet_is_read_by_its_mark_with_no_choice_to_make() {
        let text = "FILE \"album.flac\" WAVE\r\nTRACK 01 AUDIO\r\nINDEX 01 00:00:00\r\n";
        let mut sheet = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            sheet.extend_from_slice(&unit.to_le_bytes());
        }
        let dir = folder("utf16", &[("album.flac", b"x"), ("x.cue", &sheet)]);
        let beside = Beside::read(&dir);
        let f = plan(&sheet, &dir, "x.cue", &beside, &decoders(), &ALL).unwrap();
        assert_eq!(ids(&f), [("encoding", true)]);
        assert_eq!(f.encoding.as_deref(), Some("utf-16le"));
        assert!(f.encodings.is_empty());
        assert_eq!(f.after, text);
        assert!(f.remaining.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_clean_sheet_has_nothing_to_repair() {
        let sheet = b"REM COMMENT \"FILE not a command here\"\nFILE \"album.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nFILE album.flac WAVE\nFILES \"x\" WAVE\n";
        let dir = folder("clean", &[("album.flac", b"x"), ("x.cue", sheet)]);
        let beside = Beside::read(&dir);
        let f = plan(sheet, &dir, "x.cue", &beside, &decoders(), &ALL).unwrap();
        assert!(f.repairs.is_empty(), "{:?}", f.repairs);
        assert!(!f.changed);
        assert_eq!(f.after.as_bytes(), sheet);
        assert_eq!(f.lines_changed, 0);
        // A name nothing beside the sheet answers to is left alone.
        let gone = b"FILE \"nothing.flac\" WAVE\nFILE \"C:\\x\\nothing.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n";
        let f = plan(gone, &dir, "x.cue", &beside, &decoders(), &ALL).unwrap();
        assert!(f.repairs.is_empty(), "{:?}", f.repairs);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sheet_that_doubles_the_album_may_be_removed_but_never_by_default() {
        let sheet = b"FILE \"image.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n";
        let dir = folder(
            "twice",
            &[
                ("image.flac", b"x"),
                ("a.cue", sheet),
                ("b.cue", sheet),
                ("big.cue", &vec![b'x'; (MAX_SHEET + 1) as usize]),
            ],
        );
        let beside = Beside::read(&dir);
        let f = plan(sheet, &dir, "a.cue", &beside, &decoders(), &ALL).unwrap();
        assert_eq!(ids(&f), [("remove", false)]);
        assert!(f.repairs[0].what.contains("another sheet"));
        assert!(!f.changed);
        let remove = Choice {
            repairs: Some(&["remove".to_string()]),
            encoding: None,
        };
        let f = plan(sheet, &dir, "a.cue", &beside, &decoders(), &remove).unwrap();
        assert!(f.remove);
        assert!(f.changed);
        assert!(f.after.is_empty());
        assert!(f.remaining.is_empty());
        // Alone with the album's tracks beside it as files.
        fs::remove_file(dir.join("b.cue")).unwrap();
        for n in ["01.flac", "02.flac", "03.flac"] {
            fs::write(dir.join(n), b"x").unwrap();
        }
        let beside = Beside::read(&dir);
        let f = plan(sheet, &dir, "a.cue", &beside, &decoders(), &ALL).unwrap();
        assert_eq!(ids(&f), [("remove", false)]);
        assert!(f.repairs[0].what.contains("tracks are beside it"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
