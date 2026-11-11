//! A playlist file read the way MPD reads it (`src/playlist/plugins/*.cxx` and
//! `PlaylistSong.cxx` at v0.24.6): the bytes first (a byte-order mark, the encoding),
//! then the format's own rules; and each entry resolved as `load` would resolve it,
//! joined to the playlist's folder with the dot segments squashed, or kept as it is
//! when it is absolute or has a scheme. The findings are MPD's behaviour, in MPD's
//! terms, with Volumio's own limit (`addPlay` loads `.cue`, `.pls` and `.m3u` only).

use serde::Serialize;

use crate::cue::Finding;

/// The suffixes read here; `asx` and `rss` are MPD's too, counted and not read.
pub const SUFFIXES: &[&str] = &["m3u", "m3u8", "pls", "xspf"];

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The line the entry is on (1-based); for XSPF the track's number.
    pub line: usize,
    /// As MPD takes it: the line stripped, the PLS `FileN` value, the XSPF location.
    pub uri: String,
    /// The EXTINF name, the PLS title or the XSPF title, when there is one.
    pub name: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
pub struct Playlist {
    /// `m3u`, `extm3u`, `pls` or `xspf`, as MPD's plugins take the file.
    pub format: String,
    /// `utf-8`, `utf-8-bom`, `latin-1` (not UTF-8, read as Latin-1), `utf-16`.
    pub encoding: String,
    pub entries: Vec<Entry>,
    pub findings: Vec<Finding>,
    /// A PLS MPD rejects whole.
    pub rejected: bool,
}

/// The bytes as text: the encoding judged, the mark noted, Latin-1 read as such
/// when the bytes are not UTF-8. The findings are the playlist codes.
pub fn decode(bytes: &[u8]) -> (String, String, Vec<Finding>) {
    let mut findings = Vec::new();
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        findings.push(Finding {
            code: "P05",
            line: 1,
            text: msg!("core.playlist.utf16", "The playlist is UTF-16; MPD reads it as bytes and no entry matches a name on disk."),
        });
        let text: String = bytes.iter().map(|&b| b as char).collect();
        return ("utf-16".to_string(), text, findings);
    }
    let (body, bom) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (rest, true),
        None => (bytes, false),
    };
    let (encoding, text) = match std::str::from_utf8(body) {
        Ok(s) => (if bom { "utf-8-bom" } else { "utf-8" }, s.to_string()),
        Err(_) => {
            findings.push(Finding {
                code: "P05",
                line: 1,
                text: msg!("core.playlist.not_utf8", "The playlist is not UTF-8 (read here as Latin-1); MPD takes the bytes as written, so an entry with an accented letter matches no name on disk."),
            });
            ("latin-1", body.iter().map(|&b| b as char).collect())
        }
    };
    (encoding.to_string(), text, findings)
}

/// The format MPD's plugins take a suffix as, before the file is read.
pub fn format_of(suffix: &str) -> Option<&'static str> {
    match suffix {
        "m3u" | "m3u8" => Some("m3u"),
        "pls" => Some("pls"),
        "xspf" => Some("xspf"),
        _ => None,
    }
}

/// MPD's `Strip`: ASCII whitespace off both ends.
fn strip(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_ascii_whitespace())
}

/// The playlist parsed as MPD parses it, the findings the text alone gives. `bom`
/// says the mark was there, which MPD's plugins see as part of the first line.
pub fn parse_text(text: &str, suffix: &str, bom: bool, mut findings: Vec<Finding>) -> Playlist {
    let format = format_of(suffix).unwrap_or("m3u");
    let mut out = Playlist {
        format: format.to_string(),
        encoding: String::new(),
        entries: Vec::new(),
        findings: Vec::new(),
        rejected: false,
    };
    match format {
        "pls" => parse_pls(text, &mut out, &mut findings),
        "xspf" => parse_xspf(text, &mut out),
        _ => parse_m3u(text, bom, &mut out, &mut findings),
    }
    if out.entries.is_empty() && !out.rejected {
        findings.push(Finding {
            code: "P08",
            line: 0,
            text: msg!(
                "core.playlist.no_entry",
                "No entry in the playlist: nothing to play."
            ),
        });
    }
    findings.sort_by_key(|f| (f.line == 0, f.line, f.code));
    out.findings = findings;
    out
}

/// The whole file: decoded, then parsed.
pub fn parse(bytes: &[u8], suffix: &str) -> Playlist {
    let (encoding, text, findings) = decode(bytes);
    let bom = encoding == "utf-8-bom";
    let mut out = parse_text(&text, suffix, bom, findings);
    out.encoding = encoding;
    out
}

/// M3U as MPD's two plugins read it: ExtM3U when the first line, right-stripped, is
/// exactly `#EXTM3U` (a mark in front of it defeats that), else plain.
fn parse_m3u(text: &str, bom: bool, out: &mut Playlist, findings: &mut Vec<Finding>) {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let first = lines
        .first()
        .map(|l| l.trim_end_matches(|c: char| c.is_ascii_whitespace()));
    let extm3u = !bom && first == Some("#EXTM3U");
    if bom {
        let line = strip(first.unwrap_or(""));
        findings.push(Finding {
            code: "P04",
            line: 1,
            text: if first == Some("#EXTM3U") {
                msg!("core.playlist.bom_extm3u", "A byte-order mark starts the playlist, so MPD takes the first line ({first}) as an entry and the EXTINF names are lost", first = line)
            } else {
                msg!("core.playlist.bom", "A byte-order mark starts the playlist, so MPD takes the first line ({first}) as an entry", first = line)
            },
        });
    }
    if extm3u {
        out.format = "extm3u".to_string();
    }
    let mut pending: Option<String> = None;
    for (i, raw) in lines.iter().enumerate() {
        let number = i + 1;
        let line = strip(raw);
        if number == 1 && bom {
            // The mark is part of the line to MPD: not a comment, an entry; the
            // decoded text has the mark's bytes off, so it goes back in front.
            out.entries.push(Entry {
                line: number,
                uri: format!("\u{FEFF}{raw}"),
                name: None,
            });
            continue;
        }
        if extm3u {
            if let Some(rest) = line.strip_prefix("#EXTINF:") {
                // The duration up to the comma, the name after it; a line without
                // the comma gives nothing.
                pending = rest.split_once(',').map(|(_, name)| {
                    name.trim_start_matches(|c: char| c.is_ascii_whitespace())
                        .to_string()
                });
                continue;
            }
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        out.entries.push(Entry {
            line: number,
            uri: line.to_string(),
            name: pending.take().filter(|n| !n.is_empty()),
        });
    }
}

/// PLS as MPD reads it: the `[playlist]` section, `NumberOfEntries` required,
/// `FileN`, `TitleN`.
fn parse_pls(text: &str, out: &mut Playlist, findings: &mut Vec<Finding>) {
    let mut in_section = false;
    let mut n_entries: Option<usize> = None;
    let mut files: Vec<(usize, usize, String)> = Vec::new(); // (index, line, value)
    let mut titles: Vec<(usize, String)> = Vec::new();
    let mut section_line = 0;
    for (i, raw) in text.split('\n').enumerate() {
        let number = i + 1;
        let line = strip(raw.strip_suffix('\r').unwrap_or(raw));
        if !in_section {
            if line.eq_ignore_ascii_case("[playlist]") {
                in_section = true;
                section_line = number;
            }
            continue;
        }
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            break;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = strip(name);
        let value = strip(value);
        if name.eq_ignore_ascii_case("NumberOfEntries") {
            n_entries = value.parse().ok();
        } else if let Some(n) = prefixed_index(name, "File") {
            files.push((n, number, value.to_string()));
        } else if let Some(n) = prefixed_index(name, "Title") {
            titles.push((n, value.to_string()));
        }
    }
    if !in_section {
        out.rejected = true;
        findings.push(Finding {
            code: "P06",
            line: 0,
            text: msg!(
                "core.playlist.no_section",
                "No [playlist] section: MPD rejects the file."
            ),
        });
        return;
    }
    let _ = section_line;
    match n_entries {
        None => {
            out.rejected = true;
            findings.push(Finding {
                code: "P06",
                line: 0,
                text: msg!(
                    "core.playlist.no_count",
                    "No NumberOfEntries line: MPD rejects the whole file."
                ),
            });
        }
        Some(0) => {}
        Some(n) => {
            files.sort_by_key(|f| f.0);
            for (index, line, value) in files {
                if index == 0 || index > n {
                    continue;
                }
                if value.is_empty() {
                    continue;
                }
                let name = titles
                    .iter()
                    .find(|(t, _)| *t == index)
                    .map(|(_, v)| v.clone())
                    .filter(|v| !v.is_empty());
                out.entries.push(Entry {
                    line,
                    uri: value,
                    name,
                });
            }
        }
    }
}

/// `File3` to 3, case-insensitively on the word.
fn prefixed_index(name: &str, word: &str) -> Option<usize> {
    if name.len() <= word.len() || !name[..word.len()].eq_ignore_ascii_case(word) {
        return None;
    }
    name[word.len()..].parse().ok()
}

/// The five XML entities expat decodes in text.
fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The text of the first `<tag>...</tag>` in `s`, and where it ends.
fn element<'a>(s: &'a str, tag: &str) -> Option<(&'a str, usize)> {
    let open = format!("<{tag}");
    let start = s.find(&open)?;
    let after = &s[start + open.len()..];
    // The start tag may carry attributes; its end is the next '>'.
    let close_of_open = after.find('>')?;
    let body_start = start + open.len() + close_of_open + 1;
    let close = format!("</{tag}>");
    let body_end = s[body_start..].find(&close)? + body_start;
    Some((&s[body_start..body_end], body_end + close.len()))
}

/// XSPF as MPD reads it: every `track` under `trackList`, its `location` the entry,
/// its `title` the name. A small scan, not an XML parser: the elements as written,
/// the five entities decoded.
fn parse_xspf(text: &str, out: &mut Playlist) {
    let Some((list, _)) = element(text, "trackList") else {
        return;
    };
    let mut rest = list;
    let mut number = 0;
    while let Some((track, end)) = element(rest, "track") {
        number += 1;
        if let Some((location, _)) = element(track, "location") {
            let uri = unescape(strip(location));
            if !uri.is_empty() {
                out.entries.push(Entry {
                    line: number,
                    uri,
                    name: element(track, "title")
                        .map(|(t, _)| unescape(strip(t)))
                        .filter(|t| !t.is_empty()),
                });
            }
        }
        rest = &rest[end..];
    }
}

// --------------------------------------------------------------- resolution

/// Where an entry stands to MPD's `load`.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// Relative: joined to the playlist's folder; the library path it names.
    Relative,
    /// `http://` and the like: a stream, fine for MPD.
    Stream,
    /// `file://`: not a song of the database.
    FileUri,
    /// Starts with `/`: kept as it is, not a song of the database.
    Absolute,
    /// A drive letter or backslashes: relative to MPD on Linux, dangles.
    Windows,
}

/// MPD's `uri_has_scheme`: letters, digits, `+`, `-` or `.` up to `://`.
pub fn has_scheme(uri: &str) -> bool {
    match uri.find("://") {
        Some(at) if at > 0 => uri[..at]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.'),
        _ => false,
    }
}

/// MPD's `uri_squash_dot_segments`: `.` dropped, `..` eating the segment before it.
pub fn squash(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skips = 0usize;
    for seg in path.split('/').rev() {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            skips += 1;
            continue;
        }
        if skips > 0 {
            skips -= 1;
            continue;
        }
        out.push(seg);
    }
    out.reverse();
    out.join("/")
}

/// An entry as `load` takes it: its place, and the library path MPD looks up when
/// it is relative.
pub fn resolve(uri: &str, playlist_folder: &str) -> (Place, Option<String>) {
    if uri.starts_with('/') {
        return (Place::Absolute, None);
    }
    if has_scheme(uri) {
        let lower = uri.to_ascii_lowercase();
        return if lower.starts_with("file://") {
            (Place::FileUri, None)
        } else {
            (Place::Stream, None)
        };
    }
    let bytes = uri.as_bytes();
    if uri.contains('\\') || (bytes.len() > 1 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
    {
        return (Place::Windows, None);
    }
    let joined = if playlist_folder.is_empty() {
        uri.to_string()
    } else {
        format!("{playlist_folder}/{uri}")
    };
    (Place::Relative, Some(squash(&joined)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uris(p: &Playlist) -> Vec<&str> {
        p.entries.iter().map(|e| e.uri.as_str()).collect()
    }

    fn codes(p: &Playlist) -> Vec<&'static str> {
        p.findings.iter().map(|f| f.code).collect()
    }

    #[test]
    fn an_m3u_gives_its_lines_and_an_extm3u_its_names() {
        let plain = b"# a comment\r\n\r\n  Artist/Album/01.flac  \r\nhttp://radio.example/stream\r\nD:\\Music\\x.mp3\n";
        let p = parse(plain, "m3u");
        assert_eq!(p.format, "m3u");
        assert_eq!(p.encoding, "utf-8");
        assert_eq!(
            uris(&p),
            [
                "Artist/Album/01.flac",
                "http://radio.example/stream",
                "D:\\Music\\x.mp3"
            ]
        );
        assert_eq!(p.entries[0].line, 3);
        assert!(p.entries.iter().all(|e| e.name.is_none()));
        assert!(p.findings.is_empty(), "{:?}", p.findings);
        let ext = b"#EXTM3U\n#EXTINF:123, The Song\n01.flac\n#EXTINF:bad\n02.flac\n#EXTINF:-1,\n03.flac\n";
        let p = parse(ext, "m3u8");
        assert_eq!(p.format, "extm3u");
        assert_eq!(uris(&p), ["01.flac", "02.flac", "03.flac"]);
        assert_eq!(p.entries[0].name.as_deref(), Some("The Song"));
        assert_eq!(p.entries[1].name, None, "a malformed EXTINF gives nothing");
        assert_eq!(p.entries[2].name, None, "an empty name is none");
        // The mark: the first line is an entry to MPD, and ExtM3U is not seen.
        let marked = b"\xEF\xBB\xBF#EXTM3U\n#EXTINF:1,Name\n01.flac\n";
        let p = parse(marked, "m3u");
        assert_eq!(p.format, "m3u");
        assert_eq!(p.encoding, "utf-8-bom");
        assert_eq!(uris(&p), ["\u{FEFF}#EXTM3U", "01.flac"]);
        assert_eq!(codes(&p), ["P04"]);
        assert!(p.findings[0].text.contains("EXTINF names are lost"));
        // Empty and Latin-1.
        let p = parse(b"# nothing\n", "m3u");
        assert_eq!(codes(&p), ["P08"]);
        let p = parse(b"Caf\xe9/01.flac\n", "m3u");
        assert_eq!(p.encoding, "latin-1");
        assert_eq!(codes(&p), ["P05"]);
        assert_eq!(uris(&p), ["Caf\u{e9}/01.flac"]);
    }

    #[test]
    fn a_pls_needs_its_section_and_its_count() {
        let good = b"[PLAYLIST]\r\n; a comment\r\nFile1=Artist/01.flac\r\nTitle1=One\r\nLength1=200\r\nFile2=Artist/02.flac\r\nNumberOfEntries=2\r\nVersion=2\r\n";
        let p = parse(good, "pls");
        assert_eq!(p.format, "pls");
        assert_eq!(uris(&p), ["Artist/01.flac", "Artist/02.flac"]);
        assert_eq!(p.entries[0].name.as_deref(), Some("One"));
        assert_eq!(p.entries[0].line, 3);
        assert!(!p.rejected);
        assert!(p.findings.is_empty(), "{:?}", p.findings);
        let no_count = b"[playlist]\nFile1=01.flac\n";
        let p = parse(no_count, "pls");
        assert!(p.rejected);
        assert_eq!(codes(&p), ["P06"]);
        assert!(p.entries.is_empty());
        let no_section = b"File1=01.flac\nNumberOfEntries=1\n";
        let p = parse(no_section, "pls");
        assert!(p.rejected);
        assert!(p.findings[0].text.contains("[playlist]"));
        // Entries past the count are dropped, as MPD drops them; another section ends it.
        let over =
            b"[playlist]\nNumberOfEntries=1\nFile1=a.flac\nFile2=b.flac\n[other]\nFile3=c.flac\n";
        assert_eq!(uris(&parse(over, "pls")), ["a.flac"]);
    }

    #[test]
    fn an_xspf_gives_its_locations_and_titles() {
        let x = br#"<?xml version="1.0" encoding="UTF-8"?>
<playlist version="1" xmlns="http://xspf.org/ns/0/">
  <title>Mix</title>
  <trackList>
    <track><location>file:///C:/Music/Artist/01.flac</location><title>One &amp; Two</title></track>
    <track><title>no location</title></track>
    <track>
      <location>Artist/02.flac</location>
    </track>
  </trackList>
</playlist>"#;
        let p = parse(x, "xspf");
        assert_eq!(p.format, "xspf");
        assert_eq!(
            uris(&p),
            ["file:///C:/Music/Artist/01.flac", "Artist/02.flac"]
        );
        assert_eq!(p.entries[0].name.as_deref(), Some("One & Two"));
        assert_eq!(p.entries[0].line, 1);
        assert_eq!(p.entries[1].line, 3);
        assert!(p.findings.is_empty());
        let p = parse(b"<playlist></playlist>", "xspf");
        assert_eq!(codes(&p), ["P08"]);
    }

    #[test]
    fn an_entry_is_placed_as_load_places_it() {
        assert_eq!(
            resolve("Album/01.flac", "NAS/Jazz"),
            (Place::Relative, Some("NAS/Jazz/Album/01.flac".to_string()))
        );
        assert_eq!(
            resolve("../Rock/01.flac", "NAS/Jazz"),
            (Place::Relative, Some("NAS/Rock/01.flac".to_string()))
        );
        assert_eq!(
            resolve("./a/../b.flac", ""),
            (Place::Relative, Some("b.flac".to_string()))
        );
        assert_eq!(resolve("/mnt/NAS/x.flac", "NAS"), (Place::Absolute, None));
        assert_eq!(resolve("D:\\Music\\x.flac", "NAS"), (Place::Windows, None));
        assert_eq!(resolve("Music\\x.flac", "NAS"), (Place::Windows, None));
        assert_eq!(
            resolve("file:///home/x.flac", "NAS"),
            (Place::FileUri, None)
        );
        assert_eq!(resolve("https://a.b/c", "NAS"), (Place::Stream, None));
        assert!(has_scheme("http://x"));
        assert!(!has_scheme("://x"));
        assert!(!has_scheme("a b://x"));
        assert_eq!(squash("a/b/../../c/./d"), "c/d");
        assert_eq!(squash("../../x"), "x");
    }
}
