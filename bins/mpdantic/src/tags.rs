//! The tags of a file as written, and what MPD makes of them. The reading is lofty's,
//! by the concrete file type MPD's decoder would take for the suffix, so the keys
//! come as written (a Vorbis comment's name, an ID3 frame id, an APE key, an atom);
//! then MPD's own rules, read from its source at v0.24.6 (`src/tag/`,
//! `src/lib/xiph/`, `src/decoder/plugins/FfmpegMetaData.cxx`), say which of them
//! MPD takes and as what. DSF and DFF carry an ID3v2 tag where their header says;
//! it is read from memory the same way.

use std::fs::File;
use std::io::{Cursor, Read as _, Seek, SeekFrom};
use std::path::Path;

use lofty::aac::AacFile;
use lofty::ape::ApeFile;
use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, Id3v2Version};
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::mp4::{AtomData, AtomIdent, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::musepack::MpcFile;
use lofty::ogg::{OpusFile, SpeexFile, VorbisFile};
use lofty::wavpack::WavPackFile;
use lofty::TextEncoding;
use serde::{Deserialize, Serialize};

use crate::text::Msg;

/// The most of a DSD file's tail read for its tag, as MPD caps it.
const DSD_TAG_MAX: u64 = 4 * 1024 * 1024;

/// Where a tag block came from.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    VorbisComments,
    Id3v2,
    Id3v1,
    Ape,
    Ilst,
    RiffInfo,
    AiffText,
}

/// One item as written: the key the format uses, the text, and for ID3v2 whether the
/// frame was stored as Latin-1.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub key: String,
    pub value: String,
    pub latin1: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct Block {
    pub kind: Kind,
    /// `2.3`, `2.4` for ID3v2; empty otherwise.
    pub version: String,
    pub items: Vec<Item>,
}

/// A file as read: its format as MPD's decoder choice names it, its duration, the
/// tag blocks found, or why it could not be read.
#[derive(Serialize, Debug, Clone)]
pub struct Raw {
    pub format: String,
    pub duration_ms: Option<u64>,
    pub blocks: Vec<Block>,
    pub problem: Option<Msg>,
}

/// The formats read here, by suffix as MPD takes it.
pub fn format_of(suffix: &str) -> Option<&'static str> {
    Some(match suffix {
        "flac" => "flac",
        "mp3" | "mp2" => "mp3",
        "m4a" | "m4b" | "mp4" | "alac" => "mp4",
        "aac" => "aac",
        "ogg" | "oga" => "vorbis",
        "opus" => "opus",
        "spx" => "speex",
        "wv" => "wavpack",
        "ape" => "ape",
        "mpc" => "mpc",
        "wav" => "wav",
        "aif" | "aiff" | "aifc" => "aiff",
        "dsf" => "dsf",
        "dff" => "dff",
        _ => return None,
    })
}

fn raw(format: &str) -> Raw {
    Raw {
        format: format.to_string(),
        duration_ms: None,
        blocks: Vec::new(),
        problem: None,
    }
}

/// The file read for its tags and its duration.
pub fn read(path: &Path, suffix: &str) -> Raw {
    let Some(format) = format_of(suffix) else {
        let mut r = raw(suffix);
        r.problem = Some(msg!("core.tags.not_read_here", "a format not read here"));
        return r;
    };
    let mut out = raw(format);
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            out.problem = Some(Msg::raw(e.to_string()));
            return out;
        }
    };
    let result = match format {
        "dsf" => read_dsf(&mut file, &mut out),
        "dff" => read_dff(&mut file, &mut out),
        _ => read_lofty(format, &mut file, &mut out),
    };
    if let Err(e) = result {
        out.problem = Some(e);
    }
    // A length of zero is a length not known.
    out.duration_ms = out.duration_ms.filter(|d| *d > 0);
    out
}

/// The duration alone, for the CUE check.
pub fn duration_ms(path: &Path, suffix: &str) -> Option<u64> {
    read(path, suffix).duration_ms
}

/// How far into a file lofty looks for a tag or a first frame past junk: a megabyte,
/// where lofty's own default is a kilobyte. Files with a stray header before the
/// first MPEG frame are common (one of 4352 bytes seen), MPD's decoders resync past
/// any amount, and lofty's reader does too, but its check before a write stops at the
/// default and refuses the file; the same reach on both sides keeps read and write
/// agreeing.
pub const MAX_JUNK_BYTES: usize = 1 << 20;

/// lofty's parse options as the core uses them, for reading and before a write.
pub fn parse_options() -> ParseOptions {
    ParseOptions::new().max_junk_bytes(MAX_JUNK_BYTES)
}

fn read_lofty(format: &str, file: &mut File, out: &mut Raw) -> Result<(), Msg> {
    // With the properties first; without them when the audio refuses, so the tags
    // still count.
    let with = parse_options();
    let without = parse_options().read_properties(false);
    match read_lofty_with(format, file, out, with) {
        Ok(()) => Ok(()),
        Err(first) => {
            file.seek(SeekFrom::Start(0))
                .map_err(|e| Msg::raw(e.to_string()))?;
            out.blocks.clear();
            out.duration_ms = None;
            read_lofty_with(format, file, out, without).map_err(|_| first)
        }
    }
}

fn read_lofty_with(
    format: &str,
    file: &mut File,
    out: &mut Raw,
    options: ParseOptions,
) -> Result<(), Msg> {
    let e = |e: lofty::error::FileParseError| Msg::raw(e.to_string());
    match format {
        "flac" => {
            let f = FlacFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            if let Some(t) = f.vorbis_comments() {
                out.blocks.push(vorbis_block(t));
            }
            if let Some(t) = f.id3v2() {
                out.blocks.push(id3v2_block(t));
            }
        }
        "mp3" => {
            let f = MpegFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            if let Some(t) = f.id3v2() {
                out.blocks.push(id3v2_block(t));
            }
            if let Some(t) = f.id3v1() {
                out.blocks.push(id3v1_block(t));
            }
            if let Some(t) = f.ape() {
                out.blocks.push(ape_block(t));
            }
        }
        "mp4" => {
            let f = Mp4File::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            if let Some(t) = f.ilst() {
                out.blocks.push(ilst_block(t));
            }
        }
        "aac" => {
            let f = AacFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            if let Some(t) = f.id3v2() {
                out.blocks.push(id3v2_block(t));
            }
            if let Some(t) = f.id3v1() {
                out.blocks.push(id3v1_block(t));
            }
        }
        "vorbis" => {
            let f = VorbisFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            out.blocks.push(vorbis_block(f.vorbis_comments()));
        }
        "opus" => {
            let f = OpusFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            out.blocks.push(vorbis_block(f.vorbis_comments()));
        }
        "speex" => {
            let f = SpeexFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            out.blocks.push(vorbis_block(f.vorbis_comments()));
        }
        "wavpack" => {
            let f = WavPackFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            if let Some(t) = f.ape() {
                out.blocks.push(ape_block(t));
            }
            if let Some(t) = f.id3v1() {
                out.blocks.push(id3v1_block(t));
            }
        }
        "ape" => {
            let f = ApeFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            if let Some(t) = f.ape() {
                out.blocks.push(ape_block(t));
            }
            if let Some(t) = f.id3v2() {
                out.blocks.push(id3v2_block(t));
            }
            if let Some(t) = f.id3v1() {
                out.blocks.push(id3v1_block(t));
            }
        }
        "mpc" => {
            let f = MpcFile::read_from(file, options).map_err(e)?;
            if let Some(t) = f.ape() {
                out.blocks.push(ape_block(t));
            }
            if let Some(t) = f.id3v2() {
                out.blocks.push(id3v2_block(t));
            }
            if let Some(t) = f.id3v1() {
                out.blocks.push(id3v1_block(t));
            }
        }
        "wav" => {
            let f = WavFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            if let Some(t) = f.id3v2() {
                out.blocks.push(id3v2_block(t));
            }
            if let Some(t) = f.riff_info() {
                let mut items = Vec::new();
                for (k, v) in t {
                    items.push(Item {
                        key: k.to_string(),
                        value: v.to_string(),
                        latin1: false,
                    });
                }
                out.blocks.push(Block {
                    kind: Kind::RiffInfo,
                    version: String::new(),
                    items,
                });
            }
        }
        "aiff" => {
            let f = AiffFile::read_from(file, options).map_err(e)?;
            out.duration_ms = Some(f.properties().duration().as_millis() as u64);
            if let Some(t) = f.id3v2() {
                out.blocks.push(id3v2_block(t));
            }
            if let Some(t) = f.text_chunks() {
                let mut items = Vec::new();
                if let Some(v) = &t.name {
                    items.push(Item {
                        key: "NAME".into(),
                        value: v.clone(),
                        latin1: false,
                    });
                }
                if let Some(v) = &t.author {
                    items.push(Item {
                        key: "AUTH".into(),
                        value: v.clone(),
                        latin1: false,
                    });
                }
                if let Some(v) = &t.copyright {
                    items.push(Item {
                        key: "(c) ".into(),
                        value: v.clone(),
                        latin1: false,
                    });
                }
                for v in t.annotations.iter().flatten() {
                    items.push(Item {
                        key: "ANNO".into(),
                        value: v.clone(),
                        latin1: false,
                    });
                }
                out.blocks.push(Block {
                    kind: Kind::AiffText,
                    version: String::new(),
                    items,
                });
            }
        }
        _ => return Err(msg!("core.tags.not_read_here", "a format not read here")),
    }
    Ok(())
}

fn vorbis_block(t: &lofty::ogg::tag::VorbisComments) -> Block {
    Block {
        kind: Kind::VorbisComments,
        version: String::new(),
        items: t
            .items()
            .map(|(k, v)| Item {
                key: k.to_string(),
                value: v.to_string(),
                latin1: false,
            })
            .collect(),
    }
}

fn id3v2_block(t: &Id3v2Tag) -> Block {
    let mut items = Vec::new();
    for frame in t {
        let id = match frame.id() {
            FrameId::Valid(s) | FrameId::Outdated(s) => s.to_string(),
        };
        match frame {
            Frame::Text(f) => {
                // A v2.4 text frame may hold several values, NUL-separated.
                for value in f.value.split('\0').filter(|v| !v.is_empty()) {
                    items.push(Item {
                        key: id.clone(),
                        value: value.to_string(),
                        latin1: f.encoding == TextEncoding::Latin1,
                    });
                }
            }
            // The v2.4 date frames (TDRC, TDOR and the rest) are timestamps to lofty.
            Frame::Timestamp(f) => items.push(Item {
                key: id.clone(),
                value: f.timestamp.to_string(),
                latin1: f.encoding == TextEncoding::Latin1,
            }),
            Frame::UserText(f) => items.push(Item {
                key: format!("TXXX:{}", f.description),
                value: f.content.to_string(),
                latin1: f.encoding == TextEncoding::Latin1,
            }),
            Frame::Comment(f) => items.push(Item {
                key: "COMM".to_string(),
                value: f.content.to_string(),
                latin1: f.encoding == TextEncoding::Latin1,
            }),
            Frame::UniqueFileIdentifier(f) => items.push(Item {
                key: format!("UFID:{}", f.owner),
                value: String::from_utf8_lossy(&f.identifier).into_owned(),
                latin1: false,
            }),
            _ => {}
        }
    }
    Block {
        kind: Kind::Id3v2,
        version: match t.original_version() {
            Id3v2Version::V2 => "2.2",
            Id3v2Version::V3 => "2.3",
            Id3v2Version::V4 => "2.4",
        }
        .to_string(),
        items,
    }
}

fn id3v1_block(t: &lofty::id3::v1::Id3v1Tag) -> Block {
    let mut items = Vec::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v.filter(|v| !v.is_empty()) {
            items.push(Item {
                key: k.to_string(),
                value: v,
                latin1: true,
            });
        }
    };
    put("title", t.title.clone());
    put("artist", t.artist.clone());
    put("album", t.album.clone());
    put("year", t.year.map(|y| y.to_string()));
    put("track", t.track_number.map(|n| n.to_string()));
    put("comment", t.comment.clone());
    Block {
        kind: Kind::Id3v1,
        version: String::new(),
        items,
    }
}

fn ape_block(t: &lofty::ape::ApeTag) -> Block {
    Block {
        kind: Kind::Ape,
        version: String::new(),
        items: t
            .into_iter()
            .filter_map(|item| {
                item.value().text().map(|v| Item {
                    key: item.key().to_string(),
                    value: v.to_string(),
                    latin1: false,
                })
            })
            .collect(),
    }
}

fn ilst_block(t: &lofty::mp4::Ilst) -> Block {
    let mut items = Vec::new();
    for atom in t {
        let key = match atom.ident() {
            // The iTunes atoms start with the byte A9, which is no UTF-8 on its own:
            // kept as the copyright sign, as the table above names them.
            AtomIdent::Fourcc(b) => b.iter().map(|&c| c as char).collect::<String>(),
            AtomIdent::Freeform { mean, name } => format!("----:{mean}:{name}"),
        };
        for data in atom.data() {
            let value = match data {
                AtomData::UTF8(s) | AtomData::UTF16(s) => s.clone(),
                AtomData::SignedInteger(n) => n.to_string(),
                AtomData::UnsignedInteger(n) => n.to_string(),
                AtomData::Bool(b) => b.to_string(),
                AtomData::Unknown { data, .. } if key == "trkn" || key == "disk" => {
                    // A pair of big-endian 16-bit numbers after two zero bytes: the
                    // number and the total.
                    if data.len() >= 6 {
                        let n = u16::from_be_bytes([data[2], data[3]]);
                        let total = u16::from_be_bytes([data[4], data[5]]);
                        if total > 0 {
                            format!("{n}/{total}")
                        } else {
                            n.to_string()
                        }
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            items.push(Item {
                key: key.clone(),
                value,
                latin1: false,
            });
        }
    }
    Block {
        kind: Kind::Ilst,
        version: String::new(),
        items,
    }
}

/// A DSF file: the `DSD ` header's pointer names where the ID3v2 tag is; the `fmt `
/// chunk gives the duration.
fn read_dsf(file: &mut File, out: &mut Raw) -> Result<(), Msg> {
    let mut head = [0u8; 28];
    file.read_exact(&mut head)
        .map_err(|e| Msg::raw(e.to_string()))?;
    if &head[0..4] != b"DSD " {
        return Err(msg!("core.tags.not_dsf", "not a DSF file"));
    }
    let size = file.metadata().map_err(|e| Msg::raw(e.to_string()))?.len();
    let pmeta = u64::from_le_bytes(head[20..28].try_into().unwrap_or([0; 8]));
    let mut fmt = [0u8; 52];
    if file.read_exact(&mut fmt).is_ok() && &fmt[0..4] == b"fmt " {
        let channels = u32::from_le_bytes(fmt[24..28].try_into().unwrap_or([0; 4]));
        let rate = u32::from_le_bytes(fmt[28..32].try_into().unwrap_or([0; 4]));
        let samples = u64::from_le_bytes(fmt[36..44].try_into().unwrap_or([0; 8]));
        if rate > 0 && channels > 0 {
            out.duration_ms = Some(samples * 1000 / rate as u64);
        }
    }
    if pmeta != 0 && pmeta < size {
        id3_at(file, pmeta, size - pmeta, out)?;
    }
    Ok(())
}

/// A DFF (DSDIFF) file: the `FRM8` form's top-level chunks walked for the off-spec
/// `ID3 ` chunk sacdextract and others add, as MPD walks them.
fn read_dff(file: &mut File, out: &mut Raw) -> Result<(), Msg> {
    let mut head = [0u8; 16];
    file.read_exact(&mut head)
        .map_err(|e| Msg::raw(e.to_string()))?;
    if &head[0..4] != b"FRM8" || &head[12..16] != b"DSD " {
        return Err(msg!("core.tags.not_dff", "not a DSDIFF file"));
    }
    let size = file.metadata().map_err(|e| Msg::raw(e.to_string()))?.len();
    let mut offset = 16u64;
    let mut sample_rate = 0u64;
    let mut channels = 0u64;
    while offset + 12 <= size {
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| Msg::raw(e.to_string()))?;
        let mut header = [0u8; 12];
        if file.read_exact(&mut header).is_err() {
            break;
        }
        let id = &header[0..4];
        let length = u64::from_be_bytes(header[4..12].try_into().unwrap_or([0; 8]));
        let data = offset + 12;
        match id {
            b"PROP" => {
                // The property chunks inside: the sample rate and the channels.
                let mut inner = data + 4;
                while inner + 12 <= data + length {
                    file.seek(SeekFrom::Start(inner))
                        .map_err(|e| Msg::raw(e.to_string()))?;
                    let mut h = [0u8; 12];
                    if file.read_exact(&mut h).is_err() {
                        break;
                    }
                    let l = u64::from_be_bytes(h[4..12].try_into().unwrap_or([0; 8]));
                    if &h[0..4] == b"FS  " {
                        let mut v = [0u8; 4];
                        if file.read_exact(&mut v).is_ok() {
                            sample_rate = u32::from_be_bytes(v) as u64;
                        }
                    } else if &h[0..4] == b"CHNL" {
                        let mut v = [0u8; 2];
                        if file.read_exact(&mut v).is_ok() {
                            channels = u16::from_be_bytes(v) as u64;
                        }
                    }
                    inner += 12 + l + (l & 1);
                }
            }
            b"DSD " => {
                if sample_rate > 0 && channels > 0 {
                    out.duration_ms = Some(length * 8 * 1000 / channels / sample_rate);
                }
            }
            b"ID3 " => {
                id3_at(file, data, length, out)?;
                break;
            }
            _ => {}
        }
        offset = data + length + (length & 1);
    }
    Ok(())
}

/// The ID3v2 tag at an offset, read from memory through lofty's MPEG reader with the
/// properties off: it takes the tag and stops where no frame follows.
fn id3_at(file: &mut File, offset: u64, length: u64, out: &mut Raw) -> Result<(), Msg> {
    if !(10..=DSD_TAG_MAX).contains(&length) {
        return Ok(());
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| Msg::raw(e.to_string()))?;
    let mut bytes = vec![0u8; length as usize];
    file.read_exact(&mut bytes)
        .map_err(|e| Msg::raw(e.to_string()))?;
    if &bytes[0..3] != b"ID3" {
        return Ok(());
    }
    // Zeros after the tag: the reader looks for a frame and finds none.
    bytes.extend_from_slice(&[0u8; 16]);
    let mut cursor = Cursor::new(bytes);
    let f = MpegFile::read_from(&mut cursor, ParseOptions::new().read_properties(false))
        .map_err(|e| Msg::raw(e.to_string()))?;
    if let Some(t) = f.id3v2() {
        out.blocks.push(id3v2_block(t));
    }
    Ok(())
}

// ------------------------------------------------------------------ MPD's view

/// MPD's tag names (`src/tag/Names.cxx`), lowercased, which a Vorbis comment, an APE
/// key or a libavformat key must equal, case-insensitively, to be read.
pub const MPD_NAMES: &[&str] = &[
    "artist",
    "artistsort",
    "album",
    "albumsort",
    "albumartist",
    "albumartistsort",
    "title",
    "titlesort",
    "track",
    "name",
    "genre",
    "mood",
    "date",
    "originaldate",
    "composer",
    "composersort",
    "performer",
    "conductor",
    "work",
    "movement",
    "movementnumber",
    "showmovement",
    "ensemble",
    "location",
    "grouping",
    "comment",
    "disc",
    "label",
    "musicbrainz_artistid",
    "musicbrainz_albumid",
    "musicbrainz_albumartistid",
    "musicbrainz_trackid",
    "musicbrainz_releasetrackid",
    "musicbrainz_workid",
    "musicbrainz_releasegroupid",
];

/// What MPD reads of a file: each tag it takes with its values, and the keys it does
/// not read, as written.
#[derive(Serialize, Debug, Default, Clone)]
pub struct Seen {
    pub artist: Vec<String>,
    pub album_artist: Vec<String>,
    pub album: Vec<String>,
    pub title: Vec<String>,
    pub track: Vec<String>,
    pub disc: Vec<String>,
    pub date: Vec<String>,
    pub genre: Vec<String>,
    pub composer: Vec<String>,
    pub comment: Vec<String>,
    /// Keys MPD does not read, with the block they are in.
    pub ignored: Vec<String>,
    /// An ID3v1 block with nothing else to read.
    pub id3v1_only: bool,
    /// A CUESHEET pair, which MPD's embedded cue plugin reads.
    pub cuesheet: bool,
    /// Values stored as Latin-1 in ID3v2.
    pub latin1: usize,
}

impl Seen {
    fn put(&mut self, name: &str, value: &str) {
        let list = match name {
            "artist" => &mut self.artist,
            "albumartist" => &mut self.album_artist,
            "album" => &mut self.album,
            "title" => &mut self.title,
            "track" => &mut self.track,
            "disc" => &mut self.disc,
            "date" => &mut self.date,
            "genre" => &mut self.genre,
            "composer" => &mut self.composer,
            "comment" => &mut self.comment,
            _ => return,
        };
        list.push(value.to_string());
    }
}

/// MPD's view of the blocks, by the rules of each block's kind.
pub fn view(raw: &Raw) -> Seen {
    let mut seen = Seen::default();
    let has_more_than_v1 = raw.blocks.iter().any(|b| b.kind != Kind::Id3v1);
    for block in &raw.blocks {
        match block.kind {
            Kind::VorbisComments => {
                for item in &block.items {
                    let key = item.key.to_lowercase();
                    if key == "cuesheet" {
                        // Read by MPD's embedded cue plugin, not as a tag.
                        seen.cuesheet = true;
                        continue;
                    }
                    let name = match key.as_str() {
                        "tracknumber" => "track",
                        "discnumber" => "disc",
                        "description" => "comment",
                        "movementname" => "movement",
                        "movement" => "movementnumber",
                        k if MPD_NAMES.contains(&k) => k,
                        _ => {
                            seen.ignored.push(format!("{} (Vorbis comment)", item.key));
                            continue;
                        }
                    };
                    seen.put(name, &item.value);
                }
            }
            Kind::Id3v2 => {
                for item in &block.items {
                    if item.latin1 {
                        seen.latin1 += 1;
                    }
                    let name = match item.key.as_str() {
                        "TPE1" => "artist",
                        "TPE2" => "albumartist",
                        "TALB" => "album",
                        "TIT2" => "title",
                        "TRCK" => "track",
                        "TPOS" => "disc",
                        "TDRC" | "TYER" => "date",
                        "TDOR" | "TORY" => "originaldate",
                        "TCON" => "genre",
                        "TCOM" => "composer",
                        "TPE3" => "conductor",
                        "TPE4" => "performer",
                        "TIT1" => "grouping",
                        "TMOO" => "mood",
                        "TPUB" => "label",
                        "TSOP" => "artistsort",
                        "TSO2" => "albumartistsort",
                        "TSOA" => "albumsort",
                        "TSOT" => "titlesort",
                        "COMM" => "comment",
                        k if k.starts_with("TXXX:") => {
                            let desc = k[5..].to_lowercase();
                            if desc == "cuesheet" {
                                seen.cuesheet = true;
                            }
                            if desc.starts_with("musicbrainz ") {
                                continue;
                            }
                            seen.ignored.push(format!("{} (ID3v2)", item.key));
                            continue;
                        }
                        k if k.starts_with("UFID:") => continue,
                        _ => {
                            seen.ignored.push(format!("{} (ID3v2)", item.key));
                            continue;
                        }
                    };
                    seen.put(name, &item.value);
                }
            }
            Kind::Id3v1 => {
                if has_more_than_v1 {
                    continue;
                }
                seen.id3v1_only = true;
                for item in &block.items {
                    let name = match item.key.as_str() {
                        "year" => "date",
                        k => k,
                    };
                    seen.put(name, &item.value);
                }
            }
            Kind::Ape => {
                for item in &block.items {
                    let key = item.key.to_lowercase();
                    if key == "cuesheet" {
                        seen.cuesheet = true;
                        continue;
                    }
                    let name = match key.as_str() {
                        "album artist" => "albumartist",
                        "year" => "date",
                        k if MPD_NAMES.contains(&k) => k,
                        _ => {
                            seen.ignored.push(format!("{} (APE)", item.key));
                            continue;
                        }
                    };
                    seen.put(name, &item.value);
                }
            }
            Kind::Ilst => {
                // As libavformat names them, then MPD's table and its own names.
                for item in &block.items {
                    let name = match item.key.as_str() {
                        "\u{a9}ART" => "artist",
                        "aART" => "albumartist",
                        "\u{a9}alb" => "album",
                        "\u{a9}nam" => "title",
                        "trkn" => "track",
                        "disk" => "disc",
                        "\u{a9}day" => "date",
                        "\u{a9}gen" | "gnre" => "genre",
                        "\u{a9}wrt" => "composer",
                        "\u{a9}grp" => "grouping",
                        "\u{a9}cmt" => "comment",
                        "soar" => "artistsort",
                        "soaa" => "albumartistsort",
                        "soal" => "albumsort",
                        "sonm" => "titlesort",
                        _ => {
                            seen.ignored.push(format!("{} (MP4 atom)", item.key));
                            continue;
                        }
                    };
                    seen.put(name, &item.value);
                }
            }
            Kind::RiffInfo => {
                for item in &block.items {
                    let name = match item.key.as_str() {
                        "INAM" => "title",
                        "IART" => "artist",
                        "IPRD" => "album",
                        "ICRD" => "date",
                        "ITRK" | "IPRT" => "track",
                        "IGNR" => "genre",
                        "ICMT" => "comment",
                        _ => {
                            seen.ignored.push(format!("{} (RIFF INFO)", item.key));
                            continue;
                        }
                    };
                    seen.put(name, &item.value);
                }
            }
            Kind::AiffText => {
                for item in &block.items {
                    let name = match item.key.as_str() {
                        "NAME" => "title",
                        "AUTH" => "artist",
                        "ANNO" => "comment",
                        _ => {
                            seen.ignored.push(format!("{} (AIFF chunk)", item.key));
                            continue;
                        }
                    };
                    seen.put(name, &item.value);
                }
            }
        }
    }
    seen
}

/// UTF-8 read as Latin-1: an accented letter shows as two characters, the first in
/// the range the lead bytes decode to, the second in the range of a continuation
/// byte.
pub fn looks_mangled(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars
        .windows(2)
        .any(|w| ('\u{c2}'..='\u{f4}').contains(&w[0]) && ('\u{80}'..='\u{bf}').contains(&w[1]))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use lofty::config::WriteOptions;
    use lofty::id3::v2::TextInformationFrame;
    use lofty::tag::TagExt;
    use std::io::Write as _;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mpdantic-tags-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// A FLAC with its STREAMINFO (rate, samples) and one VORBIS_COMMENT block, and
    /// no audio: lofty reads both from the headers.
    pub fn flac_bytes(rate: u32, samples: u64, comments: &[(&str, &str)]) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        let mut info = vec![0u8; 34];
        info[0..2].copy_from_slice(&4096u16.to_be_bytes());
        info[2..4].copy_from_slice(&4096u16.to_be_bytes());
        // 20 bits of rate, 3 bits of channels - 1, 5 bits of bits - 1, 36 bits of samples.
        let packed: u64 =
            ((rate as u64) << 44) | (1u64 << 41) | (15u64 << 36) | (samples & 0xF_FFFF_FFFF);
        info[10..18].copy_from_slice(&packed.to_be_bytes());
        out.push(0x00);
        out.extend_from_slice(&[0, 0, 34]);
        out.extend_from_slice(&info);
        let mut vc = Vec::new();
        let vendor = b"mpdantic test";
        vc.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        vc.extend_from_slice(vendor);
        vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for (k, v) in comments {
            let line = format!("{k}={v}");
            vc.extend_from_slice(&(line.len() as u32).to_le_bytes());
            vc.extend_from_slice(line.as_bytes());
        }
        out.push(0x80 | 4);
        let len = vc.len() as u32;
        out.extend_from_slice(&len.to_be_bytes()[1..4]);
        out.extend_from_slice(&vc);
        out
    }

    /// An ID3v2.4 tag with text frames, as bytes, through lofty's writer.
    pub fn id3v2_bytes(frames: &[(&str, &str)]) -> Vec<u8> {
        let mut tag = Id3v2Tag::new();
        for (id, value) in frames {
            tag.insert(Frame::Text(TextInformationFrame::new(
                FrameId::Valid(std::borrow::Cow::Owned(id.to_string())),
                TextEncoding::UTF8,
                value.to_string(),
            )));
        }
        let mut out = Vec::new();
        tag.dump_to(&mut out, WriteOptions::default()).unwrap();
        out
    }

    /// A DSF file: the header pointing at an ID3v2 tag after an empty data chunk.
    pub fn dsf_bytes(rate: u32, samples: u64, frames: &[(&str, &str)]) -> Vec<u8> {
        let tag = id3v2_bytes(frames);
        let mut fmt = vec![0u8; 52];
        fmt[0..4].copy_from_slice(b"fmt ");
        fmt[4..12].copy_from_slice(&52u64.to_le_bytes());
        fmt[12..16].copy_from_slice(&1u32.to_le_bytes());
        fmt[20..24].copy_from_slice(&2u32.to_le_bytes());
        fmt[24..28].copy_from_slice(&2u32.to_le_bytes());
        fmt[28..32].copy_from_slice(&rate.to_le_bytes());
        fmt[32..36].copy_from_slice(&1u32.to_le_bytes());
        fmt[36..44].copy_from_slice(&samples.to_le_bytes());
        fmt[44..48].copy_from_slice(&4096u32.to_le_bytes());
        let mut data = b"data".to_vec();
        data.extend_from_slice(&12u64.to_le_bytes());
        let pmeta = 28 + fmt.len() + data.len();
        let total = pmeta + tag.len();
        let mut out = b"DSD ".to_vec();
        out.extend_from_slice(&28u64.to_le_bytes());
        out.extend_from_slice(&(total as u64).to_le_bytes());
        out.extend_from_slice(&(pmeta as u64).to_le_bytes());
        out.extend_from_slice(&fmt);
        out.extend_from_slice(&data);
        out.extend_from_slice(&tag);
        out
    }

    #[test]
    fn a_flac_gives_its_comments_as_written_and_its_duration() {
        let path = temp("a.flac");
        File::create(&path)
            .unwrap()
            .write_all(&flac_bytes(
                44100,
                44100 * 30,
                &[
                    ("ARTIST", "Hank Mobley"),
                    ("ALBUM ARTIST", "Hank Mobley"),
                    ("ALBUM", "Soul Station"),
                    ("TITLE", "Remember"),
                    ("TRACKNUMBER", "1"),
                    ("YEAR", "1960"),
                ],
            ))
            .unwrap();
        let r = read(&path, "flac");
        assert_eq!(r.problem, None, "{:?}", r.problem);
        assert_eq!(r.format, "flac");
        assert_eq!(r.duration_ms, Some(30_000));
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].kind, Kind::VorbisComments);
        assert_eq!(r.blocks[0].items[1].key, "ALBUM ARTIST");
        let s = view(&r);
        assert_eq!(s.artist, ["Hank Mobley"]);
        assert!(s.album_artist.is_empty());
        assert_eq!(s.album, ["Soul Station"]);
        assert_eq!(s.track, ["1"]);
        assert!(s.date.is_empty());
        assert_eq!(
            s.ignored,
            ["ALBUM ARTIST (Vorbis comment)", "YEAR (Vorbis comment)"]
        );
        assert!(!s.id3v1_only);
    }

    #[test]
    fn an_mp3_with_only_a_tag_gives_its_frames_and_no_duration() {
        let path = temp("a.mp3");
        File::create(&path)
            .unwrap()
            .write_all(&id3v2_bytes(&[
                ("TPE1", "Artist"),
                ("TPE2", "Album Artist"),
                ("TALB", "Album"),
                ("TIT2", "Title"),
                ("TRCK", "3/12"),
                ("TDRC", "1999"),
                ("TPOS", "1"),
            ]))
            .unwrap();
        let r = read(&path, "mp3");
        assert_eq!(r.problem, None, "{:?}", r.problem);
        assert_eq!(r.duration_ms, None);
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].kind, Kind::Id3v2);
        assert_eq!(r.blocks[0].version, "2.4");
        let s = view(&r);
        assert_eq!(s.album_artist, ["Album Artist"]);
        assert_eq!(s.track, ["3/12"]);
        assert_eq!(s.date, ["1999"]);
        assert_eq!(s.disc, ["1"]);
        assert!(s.ignored.is_empty());
    }

    #[test]
    fn a_dsf_gives_its_tag_from_the_offset_and_its_duration() {
        let path = temp("a.dsf");
        File::create(&path)
            .unwrap()
            .write_all(&dsf_bytes(
                2_822_400,
                2_822_400 * 60,
                &[("TPE1", "Artist"), ("TALB", "Album"), ("TIT2", "Title")],
            ))
            .unwrap();
        let r = read(&path, "dsf");
        assert_eq!(r.problem, None, "{:?}", r.problem);
        assert_eq!(r.duration_ms, Some(60_000));
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(r.blocks[0].kind, Kind::Id3v2);
        let s = view(&r);
        assert_eq!(s.title, ["Title"]);
        assert_eq!(s.album, ["Album"]);
    }

    #[test]
    fn a_format_not_read_here_and_a_file_that_is_not_what_its_suffix_says() {
        let path = temp("x.wma");
        File::create(&path).unwrap().write_all(b"nothing").unwrap();
        assert_eq!(
            read(&path, "wma").problem.map(|m| m.en).as_deref(),
            Some("a format not read here")
        );
        let path = temp("not.flac");
        File::create(&path).unwrap().write_all(b"nothing").unwrap();
        assert!(read(&path, "flac").problem.is_some());
        let path = temp("not.dsf");
        File::create(&path).unwrap().write_all(&[0u8; 100]).unwrap();
        assert_eq!(
            read(&path, "dsf").problem.map(|m| m.en).as_deref(),
            Some("not a DSF file")
        );
    }

    #[test]
    fn mpds_view_applies_each_blocks_rules() {
        let raw = Raw {
            format: "test".into(),
            duration_ms: None,
            blocks: vec![
                Block {
                    kind: Kind::Ape,
                    version: String::new(),
                    items: vec![
                        Item {
                            key: "Album Artist".into(),
                            value: "AA".into(),
                            latin1: false,
                        },
                        Item {
                            key: "Year".into(),
                            value: "2001".into(),
                            latin1: false,
                        },
                        Item {
                            key: "Artist".into(),
                            value: "A".into(),
                            latin1: false,
                        },
                        Item {
                            key: "Cuesheet".into(),
                            value: "FILE".into(),
                            latin1: false,
                        },
                    ],
                },
                Block {
                    kind: Kind::Ilst,
                    version: String::new(),
                    items: vec![
                        Item {
                            key: "aART".into(),
                            value: "AA2".into(),
                            latin1: false,
                        },
                        Item {
                            key: "\u{a9}day".into(),
                            value: "2002".into(),
                            latin1: false,
                        },
                        Item {
                            key: "----:com.apple.iTunes:ALBUMARTIST".into(),
                            value: "x".into(),
                            latin1: false,
                        },
                    ],
                },
                Block {
                    kind: Kind::Id3v2,
                    version: "2.3".into(),
                    items: vec![
                        Item {
                            key: "TYER".into(),
                            value: "2003".into(),
                            latin1: true,
                        },
                        Item {
                            key: "TXXX:ALBUM ARTIST".into(),
                            value: "AA3".into(),
                            latin1: false,
                        },
                        Item {
                            key: "TXXX:MusicBrainz Album Id".into(),
                            value: "id".into(),
                            latin1: false,
                        },
                    ],
                },
                Block {
                    kind: Kind::Id3v1,
                    version: String::new(),
                    items: vec![Item {
                        key: "title".into(),
                        value: "old".into(),
                        latin1: true,
                    }],
                },
            ],
            problem: None,
        };
        let s = view(&raw);
        assert_eq!(s.album_artist, ["AA", "AA2"]);
        assert_eq!(s.date, ["2001", "2002", "2003"]);
        assert_eq!(s.artist, ["A"]);
        assert!(s.cuesheet);
        assert_eq!(s.latin1, 1);
        assert!(!s.id3v1_only);
        assert!(s.title.is_empty());
        assert_eq!(
            s.ignored,
            [
                "----:com.apple.iTunes:ALBUMARTIST (MP4 atom)",
                "TXXX:ALBUM ARTIST (ID3v2)"
            ]
        );
        let only_v1 = Raw {
            format: "mp3".into(),
            duration_ms: None,
            blocks: vec![Block {
                kind: Kind::Id3v1,
                version: String::new(),
                items: vec![
                    Item {
                        key: "title".into(),
                        value: "old".into(),
                        latin1: true,
                    },
                    Item {
                        key: "year".into(),
                        value: "1988".into(),
                        latin1: true,
                    },
                ],
            }],
            problem: None,
        };
        let s = view(&only_v1);
        assert!(s.id3v1_only);
        assert_eq!(s.title, ["old"]);
        assert_eq!(s.date, ["1988"]);
    }

    #[test]
    fn utf8_read_as_latin1_is_told_from_text_that_is_fine() {
        assert!(looks_mangled("Caf\u{c3}\u{a9}"));
        assert!(looks_mangled("Bj\u{c3}\u{b6}rk"));
        assert!(!looks_mangled("Caf\u{e9}"));
        assert!(!looks_mangled("Bj\u{f6}rk"));
        assert!(!looks_mangled("plain"));
    }
}
