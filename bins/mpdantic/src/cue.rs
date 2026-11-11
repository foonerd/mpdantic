//! A CUE sheet read the way MPD reads it (`src/playlist/cue/CueParser.cxx` at
//! v0.24.6): the bytes first (a byte-order mark, the encoding), then line by line
//! with a carriage return stripped, each line's first word the command, a value
//! quoted up to the next quote or the first word; six commands taken, the rest
//! ignored; `FILE` gated to WAVE, FLAC, MP3 and AIFF, `TRACK` to AUDIO; a track's
//! first `INDEX` its start, `INDEX 00` then `01` giving the pregap to the previous
//! track. The findings are what MPD makes of the sheet, in MPD's terms.

use serde::Serialize;

use crate::text::Msg;

/// What MPD takes as a `FILE` type; anything else drops the file's tracks.
pub const FILE_TYPES: &[&str] = &["WAVE", "FLAC", "MP3", "AIFF"];

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub code: &'static str,
    pub line: usize,
    pub text: Msg,
}

/// A `FILE` line as written.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub name: String,
    pub kind: String,
    /// Whether MPD takes the type, so the tracks under it count.
    pub accepted: bool,
    pub line: usize,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub number: String,
    pub title: Option<String>,
    pub performer: Option<String>,
    /// The `FILE` the track is in, as written; empty when none came before it.
    pub file: String,
    pub start_ms: Option<u32>,
    pub end_ms: Option<u32>,
    pub line: usize,
}

#[derive(Serialize, Debug)]
pub struct Sheet {
    /// `utf-8`, `utf-8-bom`, `latin-1` (not UTF-8, read as Latin-1), `utf-16`.
    pub encoding: String,
    pub album: Option<String>,
    pub performer: Option<String>,
    pub files: Vec<FileRef>,
    pub tracks: Vec<Track>,
    pub findings: Vec<Finding>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum State {
    Header,
    File,
    Track,
    IgnoreFile,
    IgnoreTrack,
}

/// The first word, as `cue_next_token`: leading whitespace skipped, up to whitespace.
fn next_token<'a>(src: &mut &'a str) -> Option<&'a str> {
    *src = src.trim_start();
    if src.is_empty() {
        return None;
    }
    let end = src.find(char::is_whitespace).unwrap_or(src.len());
    let (word, rest) = src.split_at(end);
    *src = rest;
    Some(word)
}

/// A value, as `cue_next_value`: quoted up to the next quote (an unterminated quote
/// takes the rest), else the first word. Also whether it was quoted.
fn next_value<'a>(src: &mut &'a str) -> Option<(&'a str, bool)> {
    *src = src.trim_start();
    if src.is_empty() {
        return None;
    }
    if let Some(rest) = src.strip_prefix('"') {
        match rest.find('"') {
            Some(end) => {
                *src = &rest[end + 1..];
                Some((&rest[..end], true))
            }
            None => {
                *src = "";
                Some((rest, true))
            }
        }
    } else {
        next_token(src).map(|w| (w, false))
    }
}

/// `mm:ss:ff` to milliseconds, as `cue_parse_position`: digits, a colon, digits, a
/// colon, digits and nothing after; frames at 75 a second, not checked.
pub fn position_ms(text: &str) -> Option<u32> {
    let mut parts = text.split(':');
    let minutes: u32 = parts.next()?.parse().ok()?;
    let seconds: u32 = parts.next()?.parse().ok()?;
    let frames: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(minutes * 60_000 + seconds * 1000 + frames * 1000 / 75)
}

/// The bytes as text: the encoding judged, the byte-order mark noted, Latin-1 read
/// as such when the bytes are not UTF-8.
fn decode(bytes: &[u8]) -> (String, String, Vec<Finding>) {
    let mut findings = Vec::new();
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        findings.push(Finding {
            code: "C07",
            line: 1,
            text: msg!(
                "core.cue.utf16",
                "The sheet is UTF-16; MPD reads it as bytes and makes nothing of it."
            ),
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
                code: "C07",
                line: 1,
                text: msg!("core.cue.not_utf8", "The sheet is not UTF-8 (read here as Latin-1); MPD replaces what does not decode, so the names show mangled."),
            });
            ("latin-1", body.iter().map(|&b| b as char).collect())
        }
    };
    (encoding.to_string(), text, findings)
}

/// The sheet parsed as MPD parses it, with the findings the text alone gives.
pub fn parse(bytes: &[u8]) -> Sheet {
    let (encoding, text, mut findings) = decode(bytes);
    let bom = encoding == "utf-8-bom";
    let mut sheet = Sheet {
        encoding,
        album: None,
        performer: None,
        files: Vec::new(),
        tracks: Vec::new(),
        findings: Vec::new(),
    };
    let mut state = State::Header;
    let mut filename = String::new();
    let mut ignore_index = false;
    let mut current: Option<Track> = None;
    let mut track_count = 0usize;
    for (i, raw) in text.split('\n').enumerate() {
        let number = i + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let mut src = line;
        let Some(command) = next_token(&mut src) else {
            continue;
        };
        if number == 1 && bom {
            // The mark is part of the first word to MPD, so the command is not seen.
            let first = command;
            findings.push(Finding {
                code: "C06",
                line: 1,
                text: if first.eq_ignore_ascii_case("FILE") {
                    msg!("core.cue.bom_file", "A byte-order mark starts the sheet, so MPD does not see the first line ({first} ...); that line is the FILE, so every track is lost", first = first)
                } else {
                    msg!("core.cue.bom", "A byte-order mark starts the sheet, so MPD does not see the first line ({first} ...)", first = first)
                },
            });
            continue;
        }
        match command {
            "REM" => {}
            "PERFORMER" => {
                if let Some((value, _)) = next_value(&mut src) {
                    match state {
                        State::Header => sheet.performer = Some(value.to_string()),
                        State::Track => {
                            if let Some(t) = current.as_mut() {
                                t.performer = Some(value.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            "TITLE" => {
                if let Some((value, _)) = next_value(&mut src) {
                    match state {
                        State::Header => sheet.album = Some(value.to_string()),
                        State::Track => {
                            if let Some(t) = current.as_mut() {
                                t.title = Some(value.to_string());
                            }
                        }
                        _ => {}
                    }
                }
            }
            "FILE" => {
                commit(&mut sheet, &mut current);
                let Some((name, quoted)) = next_value(&mut src) else {
                    continue;
                };
                let Some(kind) = next_token(&mut src) else {
                    findings.push(Finding {
                        code: "C01",
                        line: number,
                        text: msg!("core.cue.file_no_type", "FILE \"{name}\" has no type, so MPD ignores the line and the tracks after it belong to the file before, or to none.", name = name),
                    });
                    continue;
                };
                if !quoted && next_token(&mut src).is_some() {
                    findings.push(Finding {
                        code: "C05",
                        line: number,
                        text: msg!("core.cue.file_not_quoted", "The FILE name is not quoted and has a space in it: MPD takes \"{name}\" as the name and \"{kind}\" as the type.", name = name, kind = kind),
                    });
                }
                let accepted = FILE_TYPES.contains(&kind);
                if !accepted {
                    findings.push(Finding {
                        code: "C01",
                        line: number,
                        text: msg!("core.cue.file_type", "FILE type {kind} is not WAVE, FLAC, MP3 or AIFF: MPD drops every track under this FILE.", kind = kind),
                    });
                }
                sheet.files.push(FileRef {
                    name: name.to_string(),
                    kind: kind.to_string(),
                    accepted,
                    line: number,
                });
                state = if accepted {
                    State::File
                } else {
                    State::IgnoreFile
                };
                filename = name.to_string();
            }
            _ if state == State::IgnoreFile => {}
            "TRACK" => {
                commit(&mut sheet, &mut current);
                let Some(nr) = next_token(&mut src) else {
                    continue;
                };
                let Some(kind) = next_token(&mut src) else {
                    continue;
                };
                if kind != "AUDIO" {
                    state = State::IgnoreTrack;
                    continue;
                }
                state = State::Track;
                ignore_index = false;
                track_count += 1;
                if filename.is_empty() {
                    findings.push(Finding {
                        code: "C09",
                        line: number,
                        text: msg!("core.cue.track_before_file", "TRACK {nr} comes before any FILE MPD takes: it has no file and is purged.", nr = nr),
                    });
                }
                current = Some(Track {
                    number: nr.to_string(),
                    title: None,
                    performer: None,
                    file: filename.clone(),
                    start_ms: None,
                    end_ms: None,
                    line: number,
                });
            }
            _ if state == State::IgnoreTrack => {}
            "INDEX" if state == State::Track => {
                if ignore_index {
                    continue;
                }
                let Some(nr) = next_token(&mut src) else {
                    continue;
                };
                let Some(position) = next_token(&mut src) else {
                    continue;
                };
                let Some(ms) = position_ms(position) else {
                    findings.push(Finding {
                        code: "C08",
                        line: number,
                        text: msg!(
                            "core.cue.index_not_time",
                            "INDEX {nr} {position} is not mm:ss:ff; MPD ignores it.",
                            nr = nr,
                            position = position
                        ),
                    });
                    continue;
                };
                if let Some(frames) = position
                    .rsplit(':')
                    .next()
                    .and_then(|f| f.parse::<u32>().ok())
                {
                    if frames > 74 {
                        findings.push(Finding {
                            code: "C08",
                            line: number,
                            text: msg!("core.cue.index_frames", "INDEX {nr} {position}: {frames} frames in a second of 75; MPD takes it as written.", nr = nr, position = position, frames = frames),
                        });
                    }
                }
                if let Some(previous) = sheet.tracks.last_mut() {
                    if previous.file == filename {
                        if previous.start_ms.unwrap_or(0) < ms {
                            previous.end_ms = Some(ms);
                        } else {
                            findings.push(Finding {
                                code: "C08",
                                line: number,
                                text: msg!("core.cue.index_not_after", "INDEX {nr} {position} is not after the previous track's start; MPD gives that track no end here.", nr = nr, position = position),
                            });
                        }
                    }
                }
                if let Some(t) = current.as_mut() {
                    t.start_ms = Some(ms);
                }
                if nr != "00" || sheet.tracks.is_empty() {
                    ignore_index = true;
                }
            }
            _ => {}
        }
    }
    commit(&mut sheet, &mut current);
    for t in &sheet.tracks {
        if t.start_ms.is_none() {
            findings.push(Finding {
                code: "C08",
                line: t.line,
                text: msg!(
                    "core.cue.track_no_index",
                    "TRACK {nr} has no INDEX MPD can read: it starts at zero.",
                    nr = t.number
                ),
            });
        }
    }
    if track_count == 0 {
        findings.push(Finding {
            code: "C09",
            line: 0,
            text: msg!(
                "core.cue.no_track",
                "No AUDIO track in the sheet: MPD makes nothing of it."
            ),
        });
    }
    let accepted = sheet.files.iter().filter(|f| f.accepted).count();
    if accepted > 1 {
        findings.push(Finding {
            code: "C10",
            line: 0,
            text: msg!("core.cue.several_files", "{n} FILE entries: fine for MPD; Volumio's own browse shows only the first file's tracks.", n = accepted),
        });
    }
    // By line; what concerns the whole sheet (no line) last.
    findings.sort_by_key(|f| (f.line == 0, f.line, f.code));
    sheet.findings = findings;
    sheet
}

/// The track in hand finished, as `CueParser::Commit`.
fn commit(sheet: &mut Sheet, current: &mut Option<Track>) {
    if let Some(track) = current.take() {
        sheet.tracks.push(track);
    }
}

/// A `FILE` name that cannot be beside the sheet on a player: absolute, a URL, or a
/// Windows path; what kind it is, for a sentence.
pub fn foreign_path(name: &str) -> Option<Msg> {
    if name.starts_with('/') {
        return Some(msg!("core.cue.foreign_absolute", "an absolute path"));
    }
    if name.contains("://") {
        return Some(msg!("core.cue.foreign_url", "a URL"));
    }
    if name.contains('\\')
        || (name.len() > 1
            && name.as_bytes()[1] == b':'
            && name.as_bytes()[0].is_ascii_alphabetic())
    {
        return Some(msg!("core.cue.foreign_windows", "a Windows path"));
    }
    if name.split('/').any(|c| c == "..") {
        return Some(msg!(
            "core.cue.foreign_above",
            "a path above the sheet's folder"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(sheet: &Sheet) -> Vec<&'static str> {
        sheet.findings.iter().map(|f| f.code).collect()
    }

    #[test]
    fn a_clean_sheet_gives_its_tracks_their_files_starts_and_ends() {
        let text = "REM GENRE Jazz\r\nPERFORMER \"Hank Mobley\"\r\nTITLE \"Soul Station\"\r\nFILE \"Soul Station.flac\" WAVE\r\n  TRACK 01 AUDIO\r\n    TITLE \"Remember\"\r\n    INDEX 01 00:00:00\r\n  TRACK 02 AUDIO\r\n    TITLE \"This I Dig of You\"\r\n    PERFORMER \"Hank Mobley Quartet\"\r\n    INDEX 00 05:58:00\r\n    INDEX 01 06:00:00\r\n  TRACK 03 AUDIO\r\n    TITLE \"Dig Dis\"\r\n    INDEX 01 12:30:37\r\n";
        let s = parse(text.as_bytes());
        assert_eq!(s.encoding, "utf-8");
        assert_eq!(s.album.as_deref(), Some("Soul Station"));
        assert_eq!(s.performer.as_deref(), Some("Hank Mobley"));
        assert_eq!(s.files.len(), 1);
        assert!(s.files[0].accepted);
        assert_eq!(s.tracks.len(), 3);
        assert_eq!(s.tracks[0].title.as_deref(), Some("Remember"));
        assert_eq!(s.tracks[0].start_ms, Some(0));
        // The pregap goes to the previous track: its end is INDEX 01 of the next.
        assert_eq!(s.tracks[0].end_ms, Some(360_000));
        assert_eq!(s.tracks[1].start_ms, Some(360_000));
        assert_eq!(
            s.tracks[1].performer.as_deref(),
            Some("Hank Mobley Quartet")
        );
        assert_eq!(
            s.tracks[2].start_ms,
            Some(12 * 60_000 + 30_000 + 37 * 1000 / 75)
        );
        assert_eq!(s.tracks[2].end_ms, None);
        assert!(s.findings.is_empty(), "{:?}", s.findings);
    }

    #[test]
    fn a_file_type_mpd_does_not_take_drops_its_tracks() {
        let s = parse(b"FILE \"album.bin\" BINARY\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nFILE \"album.flac\" FLAC\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n");
        assert_eq!(codes(&s), ["C01"]);
        assert!(!s.files[0].accepted);
        assert!(s.files[1].accepted);
        assert_eq!(s.tracks.len(), 1);
        assert_eq!(s.tracks[0].number, "02");
        let s = parse(b"FILE \"album.flac\"\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n");
        assert_eq!(codes(&s), ["C01", "C09"]);
        assert_eq!(s.tracks[0].file, "");
    }

    #[test]
    fn an_unquoted_name_with_a_space_loses_its_tail() {
        let s = parse(b"FILE Soul Station.flac WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n");
        // The type MPD sees is "Station.flac", so the file and its track are dropped.
        assert_eq!(codes(&s), ["C01", "C05", "C09"]);
        assert_eq!(s.files[0].name, "Soul");
        assert_eq!(s.files[0].kind, "Station.flac");
        let s = parse(b"FILE album.flac WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n");
        assert!(s.findings.is_empty());
        assert_eq!(s.files[0].name, "album.flac");
    }

    #[test]
    fn a_byte_order_mark_hides_the_first_line() {
        let s = parse(b"\xEF\xBB\xBFREM GENRE Jazz\nFILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n");
        assert_eq!(s.encoding, "utf-8-bom");
        assert_eq!(codes(&s), ["C06"]);
        assert_eq!(s.tracks.len(), 1);
        let s = parse(b"\xEF\xBB\xBFFILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n");
        assert_eq!(codes(&s), ["C06", "C09"]);
        assert!(s.findings[0].text.contains("every track is lost"));
    }

    #[test]
    fn an_encoding_that_is_not_utf8_is_said() {
        let s =
            parse(b"TITLE \"Caf\xE9\"\nFILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n");
        assert_eq!(s.encoding, "latin-1");
        assert_eq!(codes(&s), ["C07"]);
        assert_eq!(s.album.as_deref(), Some("Caf\u{e9}"));
        let s = parse(b"\xFF\xFEF\0I\0L\0E\0");
        assert_eq!(s.encoding, "utf-16");
        assert_eq!(codes(&s), ["C07", "C09"]);
    }

    #[test]
    fn indexes_are_checked_as_mpd_reads_them() {
        let s = parse(b"FILE \"a.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nTRACK 02 AUDIO\nINDEX 01 bad\nTRACK 03 AUDIO\nINDEX 01 00:00:80\nTRACK 04 AUDIO\nINDEX 01 00:00:10\nTRACK 05 AUDIO\n");
        let c = codes(&s);
        assert_eq!(c, ["C08", "C08", "C08", "C08", "C08"], "{:?}", s.findings);
        // By line: track 02 (no index it can read), its bad index, the 80 frames, the
        // index not after the previous, track 05 with no index.
        assert!(s.findings[0].text.contains("TRACK 02 has no INDEX"));
        assert!(s.findings[1].text.contains("not mm:ss:ff"));
        assert!(s.findings[2].text.contains("80 frames"));
        assert!(s.findings[3].text.contains("not after the previous"));
        assert!(s.findings[4].text.contains("TRACK 05 has no INDEX"));
        assert_eq!(s.tracks[1].start_ms, None);
        assert_eq!(s.tracks.len(), 5);
    }

    #[test]
    fn no_track_and_several_files_are_said() {
        let s = parse(b"REM nothing\n");
        assert_eq!(codes(&s), ["C09"]);
        let s = parse(b"FILE \"1.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\nFILE \"2.flac\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\nTRACK 03 DATA\nINDEX 01 00:00:00\n");
        assert_eq!(codes(&s), ["C10"]);
        assert_eq!(s.tracks.len(), 2);
        assert_eq!(s.tracks[1].file, "2.flac");
    }

    #[test]
    fn a_position_is_mm_ss_ff_and_nothing_else() {
        assert_eq!(position_ms("00:00:00"), Some(0));
        assert_eq!(position_ms("01:02:03"), Some(62_000 + 40));
        assert_eq!(position_ms("1:2:3"), Some(62_000 + 40));
        assert_eq!(position_ms("00:00"), None);
        assert_eq!(position_ms("00:00:00:00"), None);
        assert_eq!(position_ms("a:b:c"), None);
    }

    #[test]
    fn a_foreign_file_name_is_named_for_what_it_is() {
        assert_eq!(foreign_path("album.flac"), None);
        assert_eq!(foreign_path("disc1/album.flac"), None);
        assert_eq!(
            foreign_path("/mnt/NAS/album.flac").as_deref(),
            Some("an absolute path")
        );
        assert_eq!(
            foreign_path("http://x/album.flac").as_deref(),
            Some("a URL")
        );
        assert_eq!(
            foreign_path("D:\\Music\\album.flac").as_deref(),
            Some("a Windows path")
        );
        assert_eq!(
            foreign_path("..\\album.flac").as_deref(),
            Some("a Windows path")
        );
        assert_eq!(
            foreign_path("../album.flac").as_deref(),
            Some("a path above the sheet's folder")
        );
    }
}
