//! The tag edits of one folder's audio files, where MPD's own rules make the edit
//! obvious: the album artist and the date written where MPD reads them, beside the
//! key it does not; text that is UTF-8 read as Latin-1 decoded back; a tag given
//! twice kept as its first value; an ID3v2 tag written from ID3v1 fields; and the
//! Album, AlbumArtist and Date every file of a split folder gets. Every change is a
//! key in one tag block set to a list of values, applied through lofty, which writes
//! every format it reads; DSF and DFF, read by this plugin's own reader, have no
//! writer here and are left alone.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::path::Path;
use std::str::FromStr;
use std::time::UNIX_EPOCH;

use lofty::aac::AacFile;
use lofty::ape::{ApeFile, ApeItem, ApeTag};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame, TimestampFrame};
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::musepack::MpcFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OpusFile, SpeexFile, VorbisFile};
use lofty::tag::items::Timestamp;
use lofty::tag::ItemValue;
use lofty::wavpack::WavPackFile;
use lofty::TextEncoding;
use serde::{Deserialize, Serialize};

use crate::classify::{self, Class, Rules};
use crate::cuefix::Repair;
use crate::mpd::Decoders;
use crate::tags::{self, Block, Kind, Raw};

/// One key of one tag block set to a list of values; the undo is the same change
/// the other way round.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct KeyChange {
    pub kind: Kind,
    pub key: String,
    pub before: Vec<String>,
    pub after: Vec<String>,
}

/// One file of the folder: its changes, or why it has none.
#[derive(Serialize, Clone, Debug)]
pub struct FilePlan {
    /// The library path, MPD's form.
    pub path: String,
    pub name: String,
    pub format: String,
    pub changes: Vec<KeyChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// As read, for the check at apply.
    pub size: u64,
    pub mtime: u64,
}

/// The distinct values the folder's files carry, with their counts, most common
/// first: what the unify selects offer.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Values {
    pub album: Vec<(String, usize)>,
    pub album_artist: Vec<(String, usize)>,
    pub date: Vec<(String, usize)>,
}

/// What the user chose: the repairs by id (every one but `unify` when not said), and
/// the values every file gets.
pub struct Choice<'a> {
    pub repairs: Option<&'a [String]>,
    pub album: Option<&'a str>,
    pub album_artist: Option<&'a str>,
    pub date: Option<&'a str>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Planned {
    pub repairs: Vec<Repair>,
    pub values: Values,
    pub files: Vec<FilePlan>,
    pub changed: bool,
}

const REPAIR_IDS: [&str; 6] = ["albumartist", "date", "text", "twice", "id3v2", "unify"];

/// Whether lofty writes the format.
fn writable_format(format: &str) -> bool {
    !matches!(format, "dsf" | "dff")
}

fn writable_kind(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::VorbisComments | Kind::Id3v2 | Kind::Ape | Kind::Ilst
    )
}

/// Whether two keys name the same item in a block of the kind: Vorbis comments and
/// APE keys are case-insensitive, frames and atoms are not.
fn same_key(kind: &Kind, a: &str, b: &str) -> bool {
    match kind {
        Kind::VorbisComments | Kind::Ape => a.eq_ignore_ascii_case(b),
        _ => a == b,
    }
}

/// The key a block of the kind writes a field under: `album`, `albumartist`, `date`.
fn field_key(kind: &Kind, field: &str) -> Option<&'static str> {
    Some(match (kind, field) {
        (Kind::VorbisComments, "album") => "ALBUM",
        (Kind::VorbisComments, "albumartist") => "ALBUMARTIST",
        (Kind::VorbisComments, "date") => "DATE",
        (Kind::Id3v2, "album") => "TALB",
        (Kind::Id3v2, "albumartist") => "TPE2",
        (Kind::Id3v2, "date") => "TDRC",
        (Kind::Ape, "album") => "Album",
        (Kind::Ape, "albumartist") => "Album Artist",
        (Kind::Ape, "date") => "Year",
        (Kind::Ilst, "album") => "\u{a9}alb",
        (Kind::Ilst, "albumartist") => "aART",
        (Kind::Ilst, "date") => "\u{a9}day",
        _ => return None,
    })
}

/// What MPD makes of a key in a block of the kind, for the fields the edits touch.
fn mpd_field(kind: &Kind, key: &str) -> Option<&'static str> {
    let lower = key.to_lowercase();
    Some(match kind {
        Kind::VorbisComments => match lower.as_str() {
            "title" => "title",
            "artist" => "artist",
            "albumartist" => "albumartist",
            "album" => "album",
            "date" => "date",
            "track" | "tracknumber" => "track",
            _ => return None,
        },
        Kind::Id3v2 => match key {
            "TIT2" => "title",
            "TPE1" => "artist",
            "TPE2" => "albumartist",
            "TALB" => "album",
            "TDRC" | "TYER" => "date",
            "TRCK" => "track",
            _ => return None,
        },
        Kind::Ape => match lower.as_str() {
            "title" => "title",
            "artist" => "artist",
            "album artist" | "albumartist" => "albumartist",
            "album" => "album",
            "year" | "date" => "date",
            "track" => "track",
            _ => return None,
        },
        Kind::Ilst => match key {
            "\u{a9}nam" => "title",
            "\u{a9}ART" => "artist",
            "aART" => "albumartist",
            "\u{a9}alb" => "album",
            "\u{a9}day" => "date",
            "trkn" => "track",
            _ => return None,
        },
        _ => return None,
    })
}

fn values_of(block: &Block, key: &str) -> Vec<String> {
    block
        .items
        .iter()
        .filter(|i| same_key(&block.kind, &i.key, key))
        .map(|i| i.value.clone())
        .collect()
}

/// The keys of a block, each once, in the order first seen.
fn keys_of(block: &Block) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for item in &block.items {
        if !keys.iter().any(|k| same_key(&block.kind, k, &item.key)) {
            keys.push(item.key.clone());
        }
    }
    keys
}

/// The album artist written under a key MPD does not read, in a block of the kind.
fn ignored_album_artist(block: &Block) -> Option<String> {
    block
        .items
        .iter()
        .find(|i| {
            let lower = i.key.to_lowercase().replace('_', " ");
            match block.kind {
                Kind::VorbisComments => lower == "album artist",
                Kind::Id3v2 => lower == "txxx:album artist" || lower == "txxx:albumartist",
                Kind::Ilst => lower.ends_with(":album artist") || lower.ends_with(":albumartist"),
                _ => false,
            }
        })
        .map(|i| i.value.clone())
        .filter(|v| !v.trim().is_empty())
}

/// A year written under a key MPD does not read.
fn ignored_year(block: &Block) -> Option<String> {
    block
        .items
        .iter()
        .find(|i| {
            let lower = i.key.to_lowercase();
            match block.kind {
                Kind::VorbisComments => lower == "year",
                Kind::Id3v2 => lower == "txxx:year",
                _ => false,
            }
        })
        .map(|i| i.value.clone())
        .filter(|v| !v.trim().is_empty())
}

/// UTF-8 that was read as Latin-1, read again as UTF-8: the characters taken as the
/// bytes they came from. None when the text is not of that shape, or reads the same.
pub fn unmangle(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    for c in text.chars() {
        let n = c as u32;
        if n > 0xFF {
            return None;
        }
        bytes.push(n as u8);
    }
    let out = String::from_utf8(bytes).ok()?;
    if out == text {
        None
    } else {
        Some(out)
    }
}

fn stat(path: &Path) -> (u64, u64) {
    match fs::metadata(path) {
        Ok(m) => (
            m.len(),
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        ),
        Err(_) => (0, 0),
    }
}

fn counted(values: BTreeMap<String, usize>) -> Vec<(String, usize)> {
    let mut v: Vec<(String, usize)> = values.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

fn first(values: &[String]) -> Option<&str> {
    values.first().map(|v| v.trim()).filter(|v| !v.is_empty())
}

/// The folder's audio files planned: what each repair would change, what is chosen,
/// the values on offer. Nothing is written.
pub fn plan(
    dir: &Path,
    folder: &str,
    decoders: &Decoders,
    choice: &Choice,
) -> Result<Planned, String> {
    let chosen = |id: &str| match choice.repairs {
        None => id != "unify",
        Some(list) => list.iter().any(|r| r == id),
    };
    let wanted = [
        ("album", choice.album),
        ("albumartist", choice.album_artist),
        ("date", choice.date),
    ];
    let unify_on = chosen("unify") && wanted.iter().any(|(_, v)| v.is_some());
    let rules = Rules::new();
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            classify::classify(&rules, std::ffi::OsStr::new(n), false, decoders).0 == Class::Audio
        })
        .collect();
    names.sort();
    let mut planned = Planned::default();
    let mut applicable: BTreeMap<&str, usize> = BTreeMap::new();
    let mut albums: BTreeMap<String, usize> = BTreeMap::new();
    let mut album_artists: BTreeMap<String, usize> = BTreeMap::new();
    let mut dates: BTreeMap<String, usize> = BTreeMap::new();
    for name in names {
        let path = dir.join(&name);
        let suffix = classify::suffix(&name).unwrap_or_default();
        let raw = tags::read(&path, &suffix);
        let (size, mtime) = stat(&path);
        let mut fp = FilePlan {
            path: if folder.is_empty() {
                name.clone()
            } else {
                format!("{folder}/{name}")
            },
            name: name.clone(),
            format: raw.format.clone(),
            changes: Vec::new(),
            note: None,
            size,
            mtime,
        };
        if let Some(p) = &raw.problem {
            fp.note = Some(format!("not read: {p}"));
            planned.files.push(fp);
            continue;
        }
        let seen = tags::view(&raw);
        *albums
            .entry(first(&seen.album).unwrap_or("(none)").to_string())
            .or_insert(0) += 1;
        *album_artists
            .entry(
                first(&seen.album_artist)
                    .or_else(|| first(&seen.artist))
                    .unwrap_or("(none)")
                    .to_string(),
            )
            .or_insert(0) += 1;
        *dates
            .entry(first(&seen.date).unwrap_or("(none)").to_string())
            .or_insert(0) += 1;
        if !writable_format(&raw.format) {
            fp.note =
                Some("not written here: no writer for DSF and DFF in this version".to_string());
            planned.files.push(fp);
            continue;
        }
        let mut changes: Vec<KeyChange> = Vec::new();
        // A change on a key already planned supersedes it.
        let put = |changes: &mut Vec<KeyChange>, c: KeyChange| {
            changes.retain(|x| !(x.kind == c.kind && same_key(&c.kind, &x.key, &c.key)));
            changes.push(c);
        };
        // The block the edits go into: the first MPD reads and lofty writes; for a
        // file with ID3v1 alone, a new ID3v2 tag when that repair is on.
        let mut primary: Option<Block> =
            raw.blocks.iter().find(|b| writable_kind(&b.kind)).cloned();
        if primary.is_none() && seen.id3v1_only {
            let v1 = raw.blocks.iter().find(|b| b.kind == Kind::Id3v1);
            let mut fresh = Block {
                kind: Kind::Id3v2,
                version: "2.4".to_string(),
                items: Vec::new(),
            };
            let mut proposed = Vec::new();
            for item in v1.map(|b| b.items.as_slice()).unwrap_or(&[]) {
                let key = match item.key.as_str() {
                    "title" => "TIT2",
                    "artist" => "TPE1",
                    "album" => "TALB",
                    "year" => "TDRC",
                    "track" => "TRCK",
                    "genre" => "TCON",
                    _ => continue,
                };
                if item.value.trim().is_empty() {
                    continue;
                }
                proposed.push(KeyChange {
                    kind: Kind::Id3v2,
                    key: key.to_string(),
                    before: Vec::new(),
                    after: vec![item.value.clone()],
                });
            }
            if !proposed.is_empty() {
                *applicable.entry("id3v2").or_insert(0) += 1;
                if chosen("id3v2") {
                    for c in proposed {
                        fresh.items.push(tags::Item {
                            key: c.key.clone(),
                            value: c.after[0].clone(),
                            latin1: false,
                        });
                        put(&mut changes, c);
                    }
                    primary = Some(fresh);
                }
            }
        }
        if let Some(block) = &primary {
            let kind = block.kind.clone();
            if seen.album_artist.is_empty() {
                if let (Some(value), Some(key)) =
                    (ignored_album_artist(block), field_key(&kind, "albumartist"))
                {
                    *applicable.entry("albumartist").or_insert(0) += 1;
                    if chosen("albumartist") {
                        put(
                            &mut changes,
                            KeyChange {
                                kind: kind.clone(),
                                key: key.to_string(),
                                before: values_of(block, key),
                                after: vec![value],
                            },
                        );
                    }
                }
            }
            if seen.date.is_empty() {
                if let (Some(value), Some(key)) = (ignored_year(block), field_key(&kind, "date")) {
                    *applicable.entry("date").or_insert(0) += 1;
                    if chosen("date") {
                        put(
                            &mut changes,
                            KeyChange {
                                kind: kind.clone(),
                                key: key.to_string(),
                                before: values_of(block, key),
                                after: vec![value],
                            },
                        );
                    }
                }
            }
            let mut text_hit = false;
            let mut twice_hit = false;
            for key in keys_of(block) {
                let Some(field) = mpd_field(&kind, &key) else {
                    continue;
                };
                let current = values_of(block, &key);
                if matches!(field, "title" | "artist" | "albumartist" | "album") {
                    let decoded: Vec<String> = current
                        .iter()
                        .map(|v| {
                            if tags::looks_mangled(v) {
                                unmangle(v).unwrap_or_else(|| v.clone())
                            } else {
                                v.clone()
                            }
                        })
                        .collect();
                    if decoded != current {
                        text_hit = true;
                        if chosen("text") {
                            put(
                                &mut changes,
                                KeyChange {
                                    kind: kind.clone(),
                                    key: key.clone(),
                                    before: current.clone(),
                                    after: decoded,
                                },
                            );
                        }
                    }
                }
                let mut distinct: Vec<&str> = current.iter().map(|v| v.trim()).collect();
                distinct.sort();
                distinct.dedup();
                if distinct.len() > 1 {
                    twice_hit = true;
                    if chosen("twice") {
                        // The first as written, decoded if that repair is on too.
                        let keep = changes
                            .iter()
                            .find(|c| same_key(&kind, &c.key, &key))
                            .and_then(|c| c.after.first().cloned())
                            .unwrap_or_else(|| current[0].clone());
                        put(
                            &mut changes,
                            KeyChange {
                                kind: kind.clone(),
                                key: key.clone(),
                                before: current.clone(),
                                after: vec![keep],
                            },
                        );
                    }
                }
            }
            if text_hit {
                *applicable.entry("text").or_insert(0) += 1;
            }
            if twice_hit {
                *applicable.entry("twice").or_insert(0) += 1;
            }
            if unify_on {
                for (field, value) in wanted {
                    let (Some(value), Some(key)) = (value, field_key(&kind, field)) else {
                        continue;
                    };
                    let current = changes
                        .iter()
                        .find(|c| same_key(&kind, &c.key, key))
                        .map(|c| c.after.clone())
                        .unwrap_or_else(|| values_of(block, key));
                    if current != [value.to_string()] {
                        put(
                            &mut changes,
                            KeyChange {
                                kind: kind.clone(),
                                key: key.to_string(),
                                before: values_of(block, key),
                                after: vec![value.to_string()],
                            },
                        );
                    }
                }
            }
        } else if fp.note.is_none() && !seen.id3v1_only {
            fp.note = Some("no tag block MPD reads and lofty writes".to_string());
        }
        if changes.is_empty() && fp.note.is_none() {
            fp.note = Some("nothing to change".to_string());
        }
        fp.changes = changes;
        planned.files.push(fp);
    }
    planned.values = Values {
        album: counted(albums),
        album_artist: counted(album_artists),
        date: counted(dates),
    };
    let split = planned.values.album.len() > 1
        || planned.values.album_artist.len() > 1
        || planned.values.date.len() > 1;
    if split {
        applicable.insert("unify", planned.files.len());
    }
    let unified: Vec<String> = wanted
        .iter()
        .filter_map(|(f, v)| v.map(|v| format!("{f} \"{v}\"")))
        .collect();
    for id in REPAIR_IDS {
        let Some(n) = applicable.get(id) else {
            continue;
        };
        let files = |n: usize| {
            if n == 1 {
                "1 file".to_string()
            } else {
                format!("{n} files")
            }
        };
        let what = match id {
            "albumartist" => format!(
                "the album artist written where MPD reads it, beside the key it does not ({})",
                files(*n)
            ),
            "date" => format!(
                "the date written where MPD reads it, beside YEAR ({})",
                files(*n)
            ),
            "text" => format!(
                "text that is UTF-8 read as Latin-1 decoded back ({})",
                files(*n)
            ),
            "twice" => format!("a tag given twice kept as its first value ({})", files(*n)),
            "id3v2" => format!("an ID3v2 tag written from the ID3v1 fields ({})", files(*n)),
            _ => {
                if unified.is_empty() {
                    "the files disagree; pick the Album, Album artist or Date every file gets"
                        .to_string()
                } else {
                    format!("every file gets {}", unified.join(", "))
                }
            }
        };
        planned.repairs.push(Repair {
            id,
            what,
            on: if id == "unify" { unify_on } else { chosen(id) },
        });
    }
    planned.changed = planned.files.iter().any(|f| !f.changes.is_empty());
    Ok(planned)
}

// ------------------------------------------------------------------ the writes

fn set_vorbis(tag: &mut VorbisComments, key: &str, values: &[String]) {
    let _ = tag.remove(key).count();
    for v in values {
        tag.push(key.to_string(), v.clone());
    }
}

fn set_id3v2(tag: &mut Id3v2Tag, key: &str, values: &[String]) {
    if let Some(desc) = key.strip_prefix("TXXX:") {
        while tag.remove_user_text(desc).is_some() {}
        for v in values {
            tag.insert_user_text(desc.to_string(), v.clone());
        }
        return;
    }
    let id = FrameId::Valid(Cow::Owned(key.to_string()));
    let _ = tag.remove(&id).count();
    if values.is_empty() {
        return;
    }
    let text = values.join("\0");
    let frame = if matches!(key, "TDRC" | "TDOR" | "TDEN" | "TDRL" | "TDTG") {
        match Timestamp::from_str(values[0].trim()) {
            Ok(ts) => Frame::Timestamp(TimestampFrame::new(id, TextEncoding::UTF8, ts)),
            Err(_) => Frame::Text(TextInformationFrame::new(id, TextEncoding::UTF8, text)),
        }
    } else {
        Frame::Text(TextInformationFrame::new(id, TextEncoding::UTF8, text))
    };
    tag.insert(frame);
}

fn set_ape(tag: &mut ApeTag, key: &str, values: &[String]) -> Result<(), String> {
    tag.remove(key);
    if values.is_empty() {
        return Ok(());
    }
    let item = ApeItem::new(key.to_string(), ItemValue::Text(values.join("\0")))
        .map_err(|e| format!("{key}: {e}"))?;
    tag.insert(item);
    Ok(())
}

fn atom_ident(key: &str) -> Result<AtomIdent<'static>, String> {
    if let Some(rest) = key.strip_prefix("----:") {
        let (mean, name) = rest
            .split_once(':')
            .ok_or_else(|| format!("{key}: not an atom"))?;
        return Ok(AtomIdent::Freeform {
            mean: Cow::Owned(mean.to_string()),
            name: Cow::Owned(name.to_string()),
        });
    }
    let mut bytes = Vec::with_capacity(4);
    for c in key.chars() {
        let n = c as u32;
        if n > 0xFF {
            return Err(format!("{key}: not an atom"));
        }
        bytes.push(n as u8);
    }
    let four: [u8; 4] = bytes
        .try_into()
        .map_err(|_| format!("{key}: not an atom"))?;
    Ok(AtomIdent::Fourcc(four))
}

fn set_ilst(tag: &mut Ilst, key: &str, values: &[String]) -> Result<(), String> {
    let ident = atom_ident(key)?;
    let _ = tag.remove(&ident).count();
    let Some(first) = values.first() else {
        return Ok(());
    };
    let mut atom = Atom::new(ident, AtomData::UTF8(first.clone()));
    for v in &values[1..] {
        atom.push_data(AtomData::UTF8(v.clone()));
    }
    tag.insert(atom);
    Ok(())
}

fn save<T: AudioFile>(file: &mut File, f: &T) -> Result<(), String> {
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    f.save_to(file, WriteOptions::default())
        .map_err(|e| e.to_string())
}

fn not_here(kind: &Kind, format: &str) -> String {
    format!("a {kind:?} block is not written in a {format} file")
}

/// The bytes of the file after its leading tags: the audio, which a tag write must
/// leave as it is. For FLAC the bytes after the metadata blocks (an ID3v2 tag before
/// the marker skipped); for the formats that carry ID3v2 at the front, the bytes
/// after that tag; none for the formats whose tag sits at the end, which lofty
/// rewrites whole in memory.
pub fn audio_after_tags(path: &Path, format: &str) -> Result<Option<u64>, String> {
    use std::io::Read as _;
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let len = file.metadata().map_err(|e| e.to_string())?.len();
    let mut head = [0u8; 10];
    let got = file.read(&mut head).map_err(|e| e.to_string())?;
    let mut pos: u64 = 0;
    if got >= 10 && &head[0..3] == b"ID3" {
        // A synchsafe size, the header, and a footer when the flag says so.
        let size = ((head[6] as u64) << 21)
            | ((head[7] as u64) << 14)
            | ((head[8] as u64) << 7)
            | (head[9] as u64);
        pos = 10 + size + if head[5] & 0x10 != 0 { 10 } else { 0 };
    }
    match format {
        "flac" => {
            file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
            let mut marker = [0u8; 4];
            file.read_exact(&mut marker).map_err(|e| e.to_string())?;
            if &marker != b"fLaC" {
                return Err("not a FLAC stream".to_string());
            }
            pos += 4;
            loop {
                let mut header = [0u8; 4];
                file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
                file.read_exact(&mut header)
                    .map_err(|_| "the metadata runs past the end of the file".to_string())?;
                let block_len =
                    ((header[1] as u64) << 16) | ((header[2] as u64) << 8) | header[3] as u64;
                pos += 4 + block_len;
                if header[0] & 0x80 != 0 {
                    break;
                }
            }
            Ok(Some(len.saturating_sub(pos)))
        }
        "mp3" | "aac" | "ape" | "mpc" => Ok(Some(len.saturating_sub(pos))),
        _ => Ok(None),
    }
}

/// The changes applied to the file through lofty and the file saved, with the audio
/// checked to be there before and the same length after. A file with no audio after
/// its tags is not written: lofty's writer truncates such a file when the tag shrinks
/// (its `replace_range` fast path, lofty 0.25.5), and there is nothing in it to keep.
pub fn apply_file(path: &Path, format: &str, changes: &[KeyChange]) -> Result<(), String> {
    let before = audio_after_tags(path, format)?;
    if before == Some(0) {
        return Err("no audio after the tags; not written".to_string());
    }
    write_changes(path, format, changes)?;
    let after = audio_after_tags(path, format)?;
    if before != after {
        return Err(format!(
            "the audio is not the same length after the write ({} bytes, then {}); the file needs a look",
            before.unwrap_or(0),
            after.unwrap_or(0)
        ));
    }
    Ok(())
}

fn write_changes(path: &Path, format: &str, changes: &[KeyChange]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    let opts = ParseOptions::new().read_properties(false);
    let e = |e: lofty::error::FileParseError| e.to_string();
    macro_rules! with_id3v2 {
        ($f:expr, $c:expr) => {{
            if $f.id3v2().is_none() {
                $f.set_id3v2(Id3v2Tag::new());
            }
            set_id3v2($f.id3v2_mut().expect("set above"), &$c.key, &$c.after);
        }};
    }
    macro_rules! with_ape {
        ($f:expr, $c:expr) => {{
            if $f.ape().is_none() {
                $f.set_ape(ApeTag::new());
            }
            set_ape($f.ape_mut().expect("set above"), &$c.key, &$c.after)?;
        }};
    }
    match format {
        "flac" => {
            let mut f = FlacFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::VorbisComments => {
                        if f.vorbis_comments().is_none() {
                            f.set_vorbis_comments(VorbisComments::new());
                        }
                        set_vorbis(
                            f.vorbis_comments_mut().expect("set above"),
                            &c.key,
                            &c.after,
                        );
                    }
                    Kind::Id3v2 => with_id3v2!(f, c),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "vorbis" => {
            let mut f = VorbisFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::VorbisComments => set_vorbis(f.vorbis_comments_mut(), &c.key, &c.after),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "opus" => {
            let mut f = OpusFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::VorbisComments => set_vorbis(f.vorbis_comments_mut(), &c.key, &c.after),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "speex" => {
            let mut f = SpeexFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::VorbisComments => set_vorbis(f.vorbis_comments_mut(), &c.key, &c.after),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "mp3" => {
            let mut f = MpegFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::Id3v2 => with_id3v2!(f, c),
                    Kind::Ape => with_ape!(f, c),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "aac" => {
            let mut f = AacFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::Id3v2 => with_id3v2!(f, c),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "wav" => {
            let mut f = WavFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::Id3v2 => with_id3v2!(f, c),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "aiff" => {
            let mut f = AiffFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::Id3v2 => with_id3v2!(f, c),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "mp4" => {
            let mut f = Mp4File::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::Ilst => {
                        if f.ilst().is_none() {
                            f.set_ilst(Ilst::new());
                        }
                        set_ilst(f.ilst_mut().expect("set above"), &c.key, &c.after)?;
                    }
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "wavpack" => {
            let mut f = WavPackFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::Ape => with_ape!(f, c),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "ape" => {
            let mut f = ApeFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::Ape => with_ape!(f, c),
                    Kind::Id3v2 => with_id3v2!(f, c),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        "mpc" => {
            let mut f = MpcFile::read_from(&mut file, opts).map_err(e)?;
            for c in changes {
                match c.kind {
                    Kind::Ape => with_ape!(f, c),
                    Kind::Id3v2 => with_id3v2!(f, c),
                    _ => return Err(not_here(&c.kind, format)),
                }
            }
            save(&mut file, &f)
        }
        other => Err(format!("{other}: not written here")),
    }
}

/// The values a key holds now in the file's block of the kind, for the undo's check.
pub fn current_values(raw: &Raw, kind: &Kind, key: &str) -> Vec<String> {
    raw.blocks
        .iter()
        .find(|b| b.kind == *kind)
        .map(|b| values_of(b, key))
        .unwrap_or_default()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::mpd;
    use crate::tags::tests::{dsf_bytes, flac_bytes};
    use lofty::tag::TagExt;
    use std::path::PathBuf;

    pub fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpdantic-tagfix-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn decoders() -> Decoders {
        mpd::tests::decoders_of(&[("flac", &["flac"]), ("mad", &["mp3"]), ("dsf", &["dsf"])])
    }

    const ALL: Choice = Choice {
        repairs: None,
        album: None,
        album_artist: None,
        date: None,
    };

    fn ids(p: &Planned) -> Vec<(&'static str, bool)> {
        p.repairs.iter().map(|r| (r.id, r.on)).collect()
    }

    /// One MPEG-1 layer III frame, 128 kbit/s at 44.1 kHz: 417 bytes, the header and
    /// silence; what lofty's writer looks for after the tag.
    fn mpeg_frame() -> Vec<u8> {
        let mut frame = vec![0u8; 417];
        frame[0..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        frame
    }

    const SILENCE: &[u8] = include_bytes!("../tests/silence.flac");
    static MADE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// A real FLAC (ffmpeg's silence, frames and all) carrying the comments: what
    /// lofty writes and reads back any number of times, where the bare metadata of
    /// `flac_bytes` does not survive a second write.
    pub fn real_flac(comments: &[(&str, &str)]) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!(
            "mpdantic-real-{}-{}.flac",
            std::process::id(),
            MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::write(&path, SILENCE).unwrap();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut f =
            FlacFile::read_from(&mut file, ParseOptions::new().read_properties(false)).unwrap();
        let mut vc = VorbisComments::new();
        for (k, v) in comments {
            vc.push(k.to_string(), v.to_string());
        }
        f.set_vorbis_comments(vc);
        file.seek(SeekFrom::Start(0)).unwrap();
        f.save_to(&mut file, WriteOptions::default()).unwrap();
        drop(file);
        let bytes = fs::read(&path).unwrap();
        let _ = fs::remove_file(&path);
        bytes
    }

    /// A FLAC with every obvious fault: the album artist and the year under keys MPD
    /// does not read, a mangled title, the artist twice.
    pub fn faulty_flac() -> Vec<u8> {
        real_flac(&[
            ("TITLE", "Caf\u{c3}\u{a9} Society"),
            ("ARTIST", "A"),
            ("ARTIST", "B"),
            ("ALBUM ARTIST", "The Band"),
            ("ALBUM", "One"),
            ("YEAR", "1999"),
            ("TRACKNUMBER", "1"),
        ])
    }

    #[test]
    fn the_folder_is_planned_written_and_put_back() {
        let dir = folder("plan");
        fs::write(dir.join("a.flac"), faulty_flac()).unwrap();
        fs::write(
            dir.join("b.flac"),
            flac_bytes(
                44100,
                44100,
                &[
                    ("TITLE", "Two"),
                    ("ARTIST", "A"),
                    ("ALBUMARTIST", "The Band"),
                    ("ALBUM", "Two"),
                    ("DATE", "1999"),
                ],
            ),
        )
        .unwrap();
        fs::write(
            dir.join("c.dsf"),
            dsf_bytes(2822400, 2822400, &[("TALB", "Three")]),
        )
        .unwrap();
        fs::write(dir.join("cover.jpg"), b"x").unwrap();
        let p = plan(&dir, "INTERNAL/Jazz", &decoders(), &ALL).unwrap();
        assert_eq!(
            ids(&p),
            [
                ("albumartist", true),
                ("date", true),
                ("text", true),
                ("twice", true),
                ("unify", false)
            ]
        );
        assert_eq!(p.files.len(), 3);
        let a = &p.files[0];
        assert_eq!(a.path, "INTERNAL/Jazz/a.flac");
        assert_eq!(a.format, "flac");
        let keys: Vec<&str> = a.changes.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys,
            ["ALBUMARTIST", "DATE", "TITLE", "ARTIST"],
            "{:?}",
            a.changes
        );
        assert_eq!(a.changes[0].before, Vec::<String>::new());
        assert_eq!(a.changes[0].after, ["The Band"]);
        assert_eq!(a.changes[1].after, ["1999"]);
        assert_eq!(a.changes[2].before, ["Caf\u{c3}\u{a9} Society"]);
        assert_eq!(a.changes[2].after, ["Caf\u{e9} Society"]);
        assert_eq!(a.changes[3].before, ["A", "B"]);
        assert_eq!(a.changes[3].after, ["A"]);
        assert_eq!(p.files[1].note.as_deref(), Some("nothing to change"));
        assert!(p.files[2]
            .note
            .as_deref()
            .unwrap()
            .starts_with("not written here"));
        assert_eq!(
            p.values.album,
            [
                ("One".to_string(), 1),
                ("Three".to_string(), 1),
                ("Two".to_string(), 1)
            ]
        );
        assert_eq!(
            p.values.date,
            [("(none)".to_string(), 2), ("1999".to_string(), 1)]
        );
        assert!(p.changed);
        // Written, read back as MPD would read it, then put back the other way round.
        apply_file(&dir.join("a.flac"), "flac", &a.changes).unwrap();
        let raw = tags::read(&dir.join("a.flac"), "flac");
        let seen = tags::view(&raw);
        assert_eq!(seen.album_artist, ["The Band"]);
        assert_eq!(seen.date, ["1999"]);
        assert_eq!(seen.title, ["Caf\u{e9} Society"]);
        assert_eq!(seen.artist, ["A"]);
        assert_eq!(seen.album, ["One"]);
        assert_eq!(
            current_values(&raw, &Kind::VorbisComments, "album artist"),
            ["The Band"],
            "the old key stays"
        );
        let reverse: Vec<KeyChange> = a
            .changes
            .iter()
            .map(|c| KeyChange {
                kind: c.kind.clone(),
                key: c.key.clone(),
                before: c.after.clone(),
                after: c.before.clone(),
            })
            .collect();
        apply_file(&dir.join("a.flac"), "flac", &reverse).unwrap();
        let raw = tags::read(&dir.join("a.flac"), "flac");
        assert!(raw.problem.is_none(), "{:?}", raw.problem);
        let seen = tags::view(&raw);
        assert!(seen.album_artist.is_empty());
        assert!(seen.date.is_empty());
        assert_eq!(seen.title, ["Caf\u{c3}\u{a9} Society"], "{:?}", raw.blocks);
        assert_eq!(seen.artist, ["A", "B"]);
        // Nothing left once repaired: planned again, the same folder says so.
        apply_file(&dir.join("a.flac"), "flac", &a.changes).unwrap();
        let again = plan(&dir, "INTERNAL/Jazz", &decoders(), &ALL).unwrap();
        assert_eq!(ids(&again), [("unify", false)]);
        assert!(!again.changed);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_values_chosen_go_on_every_file_and_a_choice_of_repairs_is_kept() {
        let dir = folder("unify");
        fs::write(dir.join("a.flac"), faulty_flac()).unwrap();
        fs::write(
            dir.join("b.flac"),
            flac_bytes(44100, 44100, &[("ALBUM", "Two"), ("ALBUMARTIST", "Them")]),
        )
        .unwrap();
        let choice = Choice {
            repairs: Some(&["twice".to_string(), "unify".to_string()]),
            album: Some("One"),
            album_artist: Some("The Band"),
            date: None,
        };
        let p = plan(&dir, "", &decoders(), &choice).unwrap();
        assert_eq!(
            ids(&p),
            [
                ("albumartist", false),
                ("date", false),
                ("text", false),
                ("twice", true),
                ("unify", true)
            ]
        );
        assert!(p.repairs[4].what.contains("album \"One\""));
        let a = &p.files[0];
        let keys: Vec<&str> = a.changes.iter().map(|c| c.key.as_str()).collect();
        // The artist kept once; the album already One; the album artist set where
        // MPD reads it (the value was under the key it does not).
        assert_eq!(keys, ["ARTIST", "ALBUMARTIST"], "{:?}", a.changes);
        assert_eq!(a.changes[1].before, Vec::<String>::new());
        assert_eq!(a.changes[1].after, ["The Band"]);
        let b = &p.files[1];
        let keys: Vec<&str> = b.changes.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["ALBUM", "ALBUMARTIST"]);
        assert_eq!(b.changes[0].before, ["Two"]);
        assert_eq!(b.changes[1].before, ["Them"]);
        assert_eq!(a.path, "a.flac");
        // Unify alone with nothing chosen changes nothing and says what to pick.
        let none = Choice {
            repairs: Some(&["unify".to_string()]),
            album: None,
            album_artist: None,
            date: None,
        };
        let p = plan(&dir, "", &decoders(), &none).unwrap();
        assert!(!p.changed);
        assert!(p
            .repairs
            .iter()
            .any(|r| r.id == "unify" && !r.on && r.what.contains("pick")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_mp3_gets_its_frames_set_and_an_id3v1_only_file_gets_a_tag() {
        let dir = folder("mp3");
        // ID3v2 with the album artist and the year under TXXX, and a mangled title.
        let mut tag = Id3v2Tag::new();
        for (id, value) in [("TIT2", "Caf\u{c3}\u{a9}"), ("TPE1", "A"), ("TALB", "One")] {
            tag.insert(Frame::Text(TextInformationFrame::new(
                FrameId::Valid(Cow::Owned(id.to_string())),
                TextEncoding::UTF8,
                value.to_string(),
            )));
        }
        tag.insert_user_text("ALBUM ARTIST".to_string(), "The Band".to_string());
        tag.insert_user_text("YEAR".to_string(), "1999".to_string());
        let mut bytes = Vec::new();
        tag.dump_to(&mut bytes, WriteOptions::default()).unwrap();
        bytes.extend_from_slice(&mpeg_frame());
        fs::write(dir.join("a.mp3"), &bytes).unwrap();
        let p = plan(&dir, "", &decoders(), &ALL).unwrap();
        let a = &p.files[0];
        let keys: Vec<&str> = a.changes.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["TPE2", "TDRC", "TIT2"], "{:?}", a.changes);
        apply_file(&dir.join("a.mp3"), "mp3", &a.changes).unwrap();
        let raw = tags::read(&dir.join("a.mp3"), "mp3");
        let seen = tags::view(&raw);
        assert_eq!(seen.album_artist, ["The Band"]);
        assert_eq!(seen.date, ["1999"]);
        assert_eq!(seen.title, ["Caf\u{e9}"]);
        assert_eq!(
            current_values(&raw, &Kind::Id3v2, "TXXX:ALBUM ARTIST"),
            ["The Band"]
        );
        // ID3v1 alone: thirty bytes a field.
        let mut v1 = vec![0u8; 128];
        v1[0..3].copy_from_slice(b"TAG");
        v1[3..3 + 5].copy_from_slice(b"Title");
        v1[33..33 + 6].copy_from_slice(b"Artist");
        v1[63..63 + 5].copy_from_slice(b"Album");
        v1[93..97].copy_from_slice(b"1999");
        v1[125] = 0;
        v1[126] = 7;
        v1[127] = 255;
        let mut only = mpeg_frame();
        only.extend_from_slice(&v1);
        fs::write(dir.join("b.mp3"), &only).unwrap();
        let p = plan(&dir, "", &decoders(), &ALL).unwrap();
        let b = &p.files[1];
        assert_eq!(b.path, "b.mp3");
        let keys: Vec<&str> = b.changes.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(
            keys,
            ["TIT2", "TPE1", "TALB", "TDRC", "TRCK"],
            "{:?}",
            b.changes
        );
        assert!(p.repairs.iter().any(|r| r.id == "id3v2" && r.on));
        apply_file(&dir.join("b.mp3"), "mp3", &b.changes).unwrap();
        let seen = tags::view(&tags::read(&dir.join("b.mp3"), "mp3"));
        assert!(!seen.id3v1_only);
        assert_eq!(seen.title, ["Title"]);
        assert_eq!(seen.date, ["1999"]);
        assert_eq!(seen.track, ["7"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_with_no_audio_after_its_tags_is_not_written_and_a_real_one_keeps_its_audio() {
        let dir = folder("audio");
        // The bare metadata the reader tests build: no audio after it. lofty writes it
        // once and truncates it on a shrink; here it is refused before either.
        fs::write(
            dir.join("bare.flac"),
            flac_bytes(44100, 44100, &[("TITLE", "One")]),
        )
        .unwrap();
        assert_eq!(
            audio_after_tags(&dir.join("bare.flac"), "flac").unwrap(),
            Some(0)
        );
        let change = vec![KeyChange {
            kind: Kind::VorbisComments,
            key: "TITLE".into(),
            before: vec!["One".into()],
            after: vec!["T".into()],
        }];
        let err = apply_file(&dir.join("bare.flac"), "flac", &change).unwrap_err();
        assert!(err.contains("no audio after the tags"), "{err}");
        assert_eq!(
            fs::read(dir.join("bare.flac")).unwrap(),
            flac_bytes(44100, 44100, &[("TITLE", "One")]),
            "untouched"
        );
        // A real file: the audio measured, the same length after a grow and a shrink.
        fs::write(dir.join("real.flac"), real_flac(&[("TITLE", "One")])).unwrap();
        let audio = audio_after_tags(&dir.join("real.flac"), "flac").unwrap();
        assert!(audio.unwrap() > 0);
        let grow = vec![KeyChange {
            kind: Kind::VorbisComments,
            key: "TITLE".into(),
            before: vec!["One".into()],
            after: vec!["A rather long title to make the block grow".into()],
        }];
        apply_file(&dir.join("real.flac"), "flac", &grow).unwrap();
        apply_file(&dir.join("real.flac"), "flac", &change).unwrap();
        assert_eq!(
            audio_after_tags(&dir.join("real.flac"), "flac").unwrap(),
            audio
        );
        // An MP3: the frame after the tag is the audio.
        let mut mp3 = Vec::new();
        let mut tag = Id3v2Tag::new();
        tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid(Cow::Owned("TIT2".to_string())),
            TextEncoding::UTF8,
            "One".to_string(),
        )));
        tag.dump_to(&mut mp3, WriteOptions::default()).unwrap();
        mp3.extend_from_slice(&mpeg_frame());
        fs::write(dir.join("a.mp3"), &mp3).unwrap();
        assert_eq!(
            audio_after_tags(&dir.join("a.mp3"), "mp3").unwrap(),
            Some(417)
        );
        let mp3_change = vec![KeyChange {
            kind: Kind::Id3v2,
            key: "TIT2".into(),
            before: vec!["One".into()],
            after: vec!["A rather long title to make the tag grow".into()],
        }];
        apply_file(&dir.join("a.mp3"), "mp3", &mp3_change).unwrap();
        assert_eq!(
            audio_after_tags(&dir.join("a.mp3"), "mp3").unwrap(),
            Some(417)
        );
        // Formats whose tag sits at the end are not measured.
        assert_eq!(
            audio_after_tags(&dir.join("a.mp3"), "wavpack").unwrap(),
            None
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mangled_text_is_decoded_only_where_it_reads_cleanly() {
        assert_eq!(unmangle("Caf\u{c3}\u{a9}").as_deref(), Some("Caf\u{e9}"));
        assert_eq!(unmangle("Caf\u{e9}"), None, "already right");
        assert_eq!(unmangle("plain"), None);
        assert_eq!(unmangle("\u{c3}"), None, "a lead byte alone is not UTF-8");
        assert_eq!(unmangle("\u{4e2d}"), None, "beyond Latin-1");
    }
}
