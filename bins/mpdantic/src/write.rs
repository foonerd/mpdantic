//! A batch of writes: prepared without writing, shown whole, confirmed once, applied
//! with a check that each file is what the preview read, and journaled for the undo.
//! Library kinds enqueue the folder for a later MPD update (Update library); kinds
//! outside the library skip MPD. Three kinds: `mpdignore`, lines into the
//! `.mpdignore` of the folder in hand; `cue`, a sheet's text repaired or the sheet
//! removed; `tags`, the tags of the folder's audio files edited key by key.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::classify;
use crate::cue::Finding;
use crate::cuecheck::{Beside, MAX_SHEET};
use crate::cuefix::{self, Choice, Repair};
use crate::ignore::Pattern;
use crate::library::Library;
use crate::mount::{self, Mount, Mounter, Share, Where};
use crate::mpd::{self, Decoders};
use crate::names;
use crate::organise;
use crate::pending::Pending;
use crate::playlist;
use crate::plcheck;
use crate::plfix::{self, EntryChange};
use crate::plimport::{self, ImportEntry};
use crate::tagcheck;
use crate::tagfix::{self, FilePlan, KeyChange, Values};
use crate::tags;
use crate::text::{self, Msg, ToArg};
use crate::undo::{self, Batch, Entry, Journal};
use crate::vlists;
use crate::walk;

/// What the page asks for.
pub struct Request {
    pub kind: String,
    /// The folder in hand for `mpdignore` and `tags`; the sheet for `cue`.
    pub path: String,
    pub lines: Vec<String>,
    /// The repairs by id; the kind's default set when not given.
    pub repairs: Option<Vec<String>>,
    pub encoding: Option<String>,
    /// The values every file of the folder gets (`tags`).
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub date: Option<String>,
    /// The folder a playlist's missing entries are looked for under (`playlist`);
    /// the playlist's own folder when not given.
    pub base: Option<String>,
    /// The Volumio playlist's name (`volumio_playlist`); the file's stem when not given.
    pub name: Option<String>,
    /// The fields set by hand, file by file (`tagedit`).
    pub edits: Option<Vec<tagfix::Edit>>,
    /// The pattern the names are made from (`rename`), or the tags read by
    /// (`tagedit` without edits).
    pub pattern: Option<String>,
    /// Destination album folders to move (`organise`): each is a library path
    /// from the scan (`INTERNAL/Artist/Album`).
    pub moves: Vec<String>,
    /// Bit-identical collisions for `organise`: leave, replace, or remove the source.
    pub duplicates: Vec<DuplicatePick>,
}

/// One organise collision the page chose an action for.
#[derive(Clone, Debug)]
pub struct DuplicatePick {
    pub name: String,
    pub to: String,
    /// `leave`, `replace`, or `remove`.
    pub action: String,
}

/// A destination that already holds a bit-identical copy of a file being moved.
#[derive(Serialize, Clone, Debug)]
pub struct Duplicate {
    pub name: String,
    pub to: String,
    /// Chosen or default action: `leave`, `replace`, or `remove`.
    pub action: String,
}

/// One file of a rename: its name, the name it would take, or why it is left.
#[derive(Serialize, Clone, Debug)]
pub struct Rename {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<Msg>,
}

/// A sheet or a playlist of the folder that names a renamed file: as it is and as
/// it would be, or why it is left.
#[derive(Serialize, Clone, Debug)]
pub struct RefChange {
    pub file: String,
    pub lines: usize,
    pub before: String,
    pub after: String,
    pub size: u64,
    pub mtime: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<Msg>,
}

/// The batch as it would be written, for the page to confirm.
#[derive(Serialize, Clone, Debug)]
pub struct Preview {
    pub id: u64,
    pub kind: String,
    /// The folder the files are in, MPD's form: what is mounted, what MPD updates.
    pub path: String,
    /// The file, MPD's form; the folder itself for `tags`.
    pub file: String,
    /// `write_text`, `remove` or `tags`.
    pub op: String,
    pub exists: bool,
    /// The file as it is, as text (bytes that are not UTF-8 shown as Latin-1).
    pub before: String,
    /// The file as it would be; empty when removed, or for `tags`.
    pub after: String,
    /// Whether the write would change anything.
    pub changed: bool,
    /// `local`, or `nas` with the share and the folder on it that would be mounted.
    pub place: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_folder: Option<String>,
    /// The file as read: its size and modification time, 0 when it is not there.
    pub size: u64,
    pub mtime: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    /// The lines of the file that are not in the request (`mpdignore`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub already: Vec<String>,
    /// A line not taken, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<(String, Msg)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repairs: Vec<Repair>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub encodings: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remaining: Vec<Finding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Msg>,
    pub lines_changed: usize,
    /// The folder's files and their changes (`tags`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FilePlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Values>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    /// A playlist's entries as they are and as they would be (`playlist`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<EntryChange>,
    pub not_found: usize,
    /// A library playlist's entries as Volumio's items (`volumio_playlist`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub imported: Vec<ImportEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The library playlist the import comes from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub left_out: usize,
    /// The tags set by hand from the grid, not by the rules (`tagedit`).
    pub edited: bool,
    /// One of Volumio's lists repaired: each item's fate (`volumio_list`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<vlists::ItemChange>,
    pub kept: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_kind: Option<String>,
    /// The folder's files as they are and as they would be named (`rename`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub renames: Vec<Rename>,
    /// The sheets and playlists of the folder that follow the renames.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<RefChange>,
    /// Organise: source files whose destination already holds the same bytes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub duplicates: Vec<Duplicate>,
}

/// What an apply or an undo did.
#[derive(Serialize, Debug)]
pub struct Applied {
    pub batch: String,
    pub file: String,
    pub written: bool,
    /// The files written (`tags`); one otherwise.
    pub count: usize,
    /// MPD's update job when a flush ran; apply/undo leave this unset.
    pub updated: Option<u64>,
    pub update_error: Option<Msg>,
    /// True when the write is on disk but MPD has not been told yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

/// What Update library did.
#[derive(Serialize, Debug)]
pub struct Flushed {
    /// The single path sent to MPD (common ancestor, or empty for the whole library).
    pub path: String,
    /// The pending folders cleared on success.
    pub paths: Vec<String>,
    pub updated: Option<u64>,
    pub update_error: Option<Msg>,
}

fn stat(path: &Path) -> (bool, u64, u64) {
    match fs::metadata(path) {
        Ok(m) => (
            true,
            m.len(),
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        ),
        Err(_) => (false, 0, 0),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn folder_dir(music: &Path, folder: &str) -> PathBuf {
    if folder.is_empty() {
        music.to_path_buf()
    } else {
        music.join(folder)
    }
}

/// An MPD path checked and kept under the library (or a Volumio root) after links.
fn resolve_library(music: &Path, path: &str) -> Result<(String, PathBuf), Msg> {
    Library::resolve(music, path).map_err(|e| match e {
        crate::library::ListError::BadPath => walk::not_library_path(),
        crate::library::ListError::NotFound | crate::library::ListError::NotADirectory => {
            walk::no_such_folder()
        }
        crate::library::ListError::Io(s) => Msg::raw(s),
    })
}

/// Where a batch's folder is written, as the preview says it: local, or on a share
/// at a folder.
struct Placed {
    place: String,
    share: Option<String>,
    share_folder: Option<String>,
}

impl Placed {
    fn local() -> Placed {
        Placed {
            place: "local".to_string(),
            share: None,
            share_folder: None,
        }
    }
}

fn placed(shares: &[Share], folder: &str) -> Result<Placed, Msg> {
    Ok(match mount::place(shares, folder)? {
        Where::Local => Placed::local(),
        Where::Nas { share, folder } => Placed {
            place: "nas".to_string(),
            share: Some(share.name.clone()),
            share_folder: Some(if folder.is_empty() {
                "/".to_string()
            } else {
                folder
            }),
        },
    })
}

fn library_root() -> Msg {
    msg!("core.write.library_root", "the library root")
}

/// Bytes as the page shows them: UTF-8 as it is, anything else read as Latin-1.
fn display(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

fn empty(id: u64, kind: &str, path: String, file: String, op: &str, placed: Placed) -> Preview {
    Preview {
        id,
        kind: kind.to_string(),
        path,
        file,
        op: op.to_string(),
        exists: false,
        before: String::new(),
        after: String::new(),
        changed: false,
        place: placed.place,
        share: placed.share,
        share_folder: placed.share_folder,
        size: 0,
        mtime: 0,
        added: Vec::new(),
        removed: Vec::new(),
        already: Vec::new(),
        refused: Vec::new(),
        repairs: Vec::new(),
        encoding: None,
        encodings: Vec::new(),
        remaining: Vec::new(),
        notes: Vec::new(),
        lines_changed: 0,
        files: Vec::new(),
        values: None,
        album: None,
        album_artist: None,
        date: None,
        entries: Vec::new(),
        not_found: 0,
        imported: Vec::new(),
        name: None,
        from: None,
        left_out: 0,
        edited: false,
        items: Vec::new(),
        kept: 0,
        list_kind: None,
        renames: Vec::new(),
        refs: Vec::new(),
        duplicates: Vec::new(),
    }
}

/// The batch prepared, nothing written.
pub fn prepare(
    music: &Path,
    shares: &[Share],
    decoders: &Decoders,
    volumio_playlists: &Path,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    match req.kind.as_str() {
        "mpdignore" => prepare_lines(music, shares, id, req),
        "cue" => prepare_cue(music, shares, decoders, id, req),
        "tags" => prepare_tags(music, shares, decoders, id, req),
        "tagedit" => prepare_tagedit(music, shares, decoders, id, req),
        "rename" => prepare_rename(music, shares, decoders, id, req),
        "folder" => prepare_folder(music, shares, decoders, id, req),
        "organise" => prepare_organise(music, shares, decoders, id, req),
        "playlist" => prepare_playlist(music, shares, decoders, id, req),
        "volumio_playlist" => prepare_volumio(music, decoders, volumio_playlists, id, req),
        "volumio_list" => prepare_volumio_list(music, decoders, volumio_playlists, id, req),
        _ => Err(msg!(
            "core.write.kind_unknown",
            "the kind is not one the core writes"
        )),
    }
}

fn not_a_playlist() -> Msg {
    msg!("core.write.not_playlist", "not a playlist MPDantic reads")
}

fn no_such_playlist() -> Msg {
    msg!("core.write.no_such_playlist", "no such playlist")
}

fn playlist_too_large(size: u64) -> Msg {
    msg!(
        "core.write.playlist_too_large",
        "the playlist is {size} bytes; not read",
        size = size
    )
}

/// A library playlist imported as one of Volumio's own: the JSON file Volumio reads,
/// under its playlist folder, written whole; outside the library, so no mount and no
/// update of MPD.
fn prepare_volumio(
    music: &Path,
    decoders: &Decoders,
    volumio_playlists: &Path,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    let (path, full) = resolve_library(music, &req.path)?;
    let suffix = classify::suffix(&path).unwrap_or_default();
    if !playlist::SUFFIXES.contains(&suffix.as_str()) {
        return Err(not_a_playlist());
    }
    if !full.is_file() {
        return Err(no_such_playlist());
    }
    let (_, size, _) = stat(&full);
    if size > plcheck::MAX_PLAYLIST {
        return Err(playlist_too_large(size));
    }
    let bytes = fs::read(&full).map_err(|e| text::at(full.display(), e))?;
    let imported = plimport::plan(music, &path, &bytes, decoders, req.name.as_deref())?;
    let folder = volumio_playlists.display().to_string();
    let target = volumio_playlists.join(&imported.name);
    let (exists, tsize, tmtime) = stat(&target);
    let current = if exists {
        fs::read(&target).map_err(|e| text::at(target.display(), e))?
    } else {
        Vec::new()
    };
    let changed = imported.after.as_bytes() != current.as_slice()
        && (imported.songs + imported.streams > 0 || exists);
    let mut p = empty(
        id,
        "volumio_playlist",
        folder,
        target.display().to_string(),
        "write_text",
        Placed::local(),
    );
    p.exists = exists;
    p.before = display(&current);
    p.after = imported.after;
    p.changed = changed;
    p.size = tsize;
    p.mtime = tmtime;
    p.imported = imported.entries;
    p.name = Some(imported.name);
    p.from = Some(path);
    p.left_out = imported.left_out;
    if imported.songs + imported.streams == 0 {
        p.notes.push(msg!(
            "core.write.nothing_to_import",
            "no entry MPD can load: nothing to import"
        ));
    }
    Ok(p)
}

/// `.mpdignore` lines into the folder in hand, against the file as it is.
fn prepare_lines(music: &Path, shares: &[Share], id: u64, req: &Request) -> Result<Preview, Msg> {
    let (path, dir) = resolve_library(music, &req.path)?;
    if !dir.is_dir() {
        return Err(walk::no_such_folder());
    }
    let place = placed(shares, &path)?;
    let file_path = dir.join(".mpdignore");
    let (exists, size, mtime) = stat(&file_path);
    let before = if exists {
        let bytes = fs::read(&file_path).map_err(|e| text::at(file_path.display(), e))?;
        String::from_utf8(bytes)
            .map_err(|_| msg!("core.write.not_utf8", "the file is not UTF-8; not touched"))?
    } else {
        String::new()
    };
    // The file as it is, line by line, and the file as the request wants it whole:
    // a line kept stays as written (a comment too), a line new to the file is
    // checked, a line of the file not in the request is removed.
    let present: Vec<String> = before
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let mut wanted: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    let mut already = Vec::new();
    let mut refused = Vec::new();
    for raw in &req.lines {
        let line = raw.trim();
        if line.is_empty() || wanted.iter().any(|w| w == line) {
            continue;
        }
        if present.iter().any(|p| p == line) {
            already.push(line.to_string());
            wanted.push(line.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            refused.push((
                line.to_string(),
                msg!(
                    "core.write.comment_line",
                    "a line starting with # is a comment to MPD; for a name starting with a hash write [#]{rest}",
                    rest = rest
                ),
            ));
            continue;
        }
        if line.contains('\n') || line.contains('\r') {
            refused.push((
                line.to_string(),
                msg!("core.write.one_line", "one line at a time"),
            ));
            continue;
        }
        if Pattern::new(line).is_none() {
            refused.push((
                line.to_string(),
                msg!("core.write.not_pattern", "not a pattern"),
            ));
            continue;
        }
        added.push(line.to_string());
        wanted.push(line.to_string());
    }
    let removed: Vec<String> = present
        .iter()
        .filter(|p| !wanted.iter().any(|w| w == *p))
        .cloned()
        .collect();
    let after = if wanted.is_empty() {
        String::new()
    } else {
        wanted.join("\n") + "\n"
    };
    let file = if path.is_empty() {
        ".mpdignore".to_string()
    } else {
        format!("{path}/.mpdignore")
    };
    let op = if after.is_empty() && exists {
        "remove"
    } else {
        "write_text"
    };
    let mut p = empty(id, "mpdignore", path, file, op, place);
    p.exists = exists;
    p.before = before;
    p.changed = if op == "remove" {
        true
    } else {
        !after.is_empty() && present != wanted
    };
    p.after = after;
    p.size = size;
    p.mtime = mtime;
    p.added = added;
    p.removed = removed;
    p.already = already;
    p.refused = refused;
    Ok(p)
}

/// A sheet's repairs, or its removal, against the sheet as it is and the files
/// beside it.
fn prepare_cue(
    music: &Path,
    shares: &[Share],
    decoders: &Decoders,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    let (path, full) = resolve_library(music, &req.path)?;
    if classify::suffix(&path).as_deref() != Some(classify::CUE_SUFFIX) {
        return Err(msg!("core.write.not_cue", "not a CUE sheet"));
    }
    if !full.is_file() {
        return Err(msg!("core.write.no_such_sheet", "no such sheet"));
    }
    let (folder, name) = match path.rsplit_once('/') {
        Some((f, n)) => (f.to_string(), n.to_string()),
        None => (String::new(), path.clone()),
    };
    let dir = folder_dir(music, &folder);
    let place = placed(shares, &folder)?;
    let (exists, size, mtime) = stat(&full);
    if size > MAX_SHEET {
        return Err(msg!(
            "core.write.sheet_too_large",
            "the sheet is {size} bytes; not read",
            size = size
        ));
    }
    let bytes = fs::read(&full).map_err(|e| text::at(full.display(), e))?;
    let beside = Beside::read(&dir);
    let choice = Choice {
        repairs: req.repairs.as_deref(),
        encoding: req.encoding.as_deref(),
    };
    let fixed = cuefix::plan(&bytes, &dir, &name, &beside, decoders, &choice)?;
    let op = if fixed.remove { "remove" } else { "write_text" };
    let mut p = empty(id, "cue", folder, path, op, place);
    p.exists = exists;
    p.before = display(&bytes);
    p.after = fixed.after;
    p.changed = fixed.changed;
    p.size = size;
    p.mtime = mtime;
    p.repairs = fixed.repairs;
    p.encoding = fixed.encoding;
    p.encodings = fixed.encodings;
    p.remaining = fixed.remaining;
    p.notes = fixed.notes;
    p.lines_changed = fixed.lines_changed;
    Ok(p)
}

/// The tags of the folder's audio files, each file's changes planned.
fn prepare_tags(
    music: &Path,
    shares: &[Share],
    decoders: &Decoders,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    let (path, dir) = resolve_library(music, &req.path)?;
    if !dir.is_dir() {
        return Err(walk::no_such_folder());
    }
    let place = placed(shares, &path)?;
    let choice = tagfix::Choice {
        repairs: req.repairs.as_deref(),
        album: req
            .album
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        album_artist: req
            .album_artist
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty()),
        date: req.date.as_deref().map(str::trim).filter(|s| !s.is_empty()),
    };
    let planned = tagfix::plan(&dir, &path, decoders, &choice)?;
    let mut p = empty(id, "tags", path.clone(), path, "tags", place);
    p.exists = true;
    p.changed = planned.changed;
    p.repairs = planned.repairs;
    p.files = planned.files;
    p.values = Some(planned.values);
    p.album = choice.album.map(str::to_string);
    p.album_artist = choice.album_artist.map(str::to_string);
    p.date = choice.date.map(str::to_string);
    Ok(p)
}

/// The tags of the folder's files set by hand, file by file: the same tags path as the
/// repairs (the preview per file and key, lofty in place, the audio guard, the journal
/// per key, the undo), the changes from the grid instead of the rules.
fn prepare_tagedit(
    music: &Path,
    shares: &[Share],
    decoders: &Decoders,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    let (path, dir) = resolve_library(music, &req.path)?;
    if !dir.is_dir() {
        return Err(walk::no_such_folder());
    }
    // The edits as the grid gave them, or read from the file names by the pattern:
    // a file that does not fit is said and left.
    let mut refused = Vec::new();
    let edits: Vec<tagfix::Edit> = match (req.edits.as_deref(), req.pattern.as_deref()) {
        (Some(edits), _) if !edits.is_empty() => edits.to_vec(),
        (_, Some(pattern)) => {
            let pattern = names::Pattern::parse(pattern)?;
            let mut edits = Vec::new();
            for file in tagfix::folder_values(&dir, &path, decoders)? {
                let stem = match classify::suffix(&file.name) {
                    Some(s) => &file.name[..file.name.len() - s.len() - 1],
                    None => file.name.as_str(),
                };
                match pattern.capture(stem) {
                    Some(set) if file.writable => edits.push(tagfix::Edit {
                        name: file.name.clone(),
                        set,
                    }),
                    Some(_) => refused.push((
                        file.name.clone(),
                        file.problem.clone().unwrap_or_else(|| {
                            msg!(
                                "core.tagfix.format_not_written",
                                "{format}: not written here",
                                format = file.format.clone()
                            )
                        }),
                    )),
                    None => refused.push((
                        file.name.clone(),
                        msg!("core.names.no_fit", "does not fit the pattern"),
                    )),
                }
            }
            edits
        }
        _ => Vec::new(),
    };
    if edits.is_empty() && refused.is_empty() {
        return Err(msg!("core.write.nothing_edited", "nothing edited"));
    }
    let place = placed(shares, &path)?;
    let planned = tagfix::plan_edits(&dir, &path, decoders, &edits)?;
    let mut p = empty(id, "tags", path.clone(), path, "tags", place);
    p.exists = true;
    p.changed = planned.changed;
    p.files = planned.files;
    p.refused = refused;
    p.edited = true;
    Ok(p)
}

/// The folder's audio files named from their tags by the pattern: each as it is and
/// as it would be, the sheets and playlists of the folder that name them following;
/// refused whole when two files would take one name or a name is taken.
fn prepare_rename(
    music: &Path,
    shares: &[Share],
    decoders: &Decoders,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    let (path, dir) = resolve_library(music, &req.path)?;
    if !dir.is_dir() {
        return Err(walk::no_such_folder());
    }
    let pattern = names::Pattern::parse(req.pattern.as_deref().unwrap_or(""))?;
    let place = placed(shares, &path)?;
    let present: BTreeSet<String> = fs::read_dir(&dir)
        .map_err(|e| text::at(dir.display(), e))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_lowercase())
        .collect();
    let mut renames = Vec::new();
    let mut refused = Vec::new();
    let mut taken: BTreeMap<String, String> = BTreeMap::new();
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for file in tagfix::folder_values(&dir, &path, decoders)? {
        let name = file.name.clone();
        if let Some(problem) = &file.problem {
            renames.push(Rename {
                name,
                to: None,
                note: Some(msg!(
                    "core.tagfix.not_read",
                    "not read: {problem}",
                    problem = problem
                )),
            });
            continue;
        }
        let value = |f: &str| file.values.get(f).and_then(|v| v.first()).cloned();
        let stem = match pattern.render(&value) {
            Ok(stem) => names::safe(&stem),
            Err(field) => {
                renames.push(Rename {
                    name,
                    to: None,
                    note: Some(msg!(
                        "core.names.missing",
                        "no {field}",
                        field = tagcheck::field_name(&field)
                    )),
                });
                continue;
            }
        };
        if stem.is_empty() {
            renames.push(Rename {
                name,
                to: None,
                note: Some(msg!(
                    "core.names.empty_name",
                    "the pattern gives an empty name"
                )),
            });
            continue;
        }
        let to = match classify::suffix(&name) {
            Some(s) => format!("{stem}.{}", &name[name.len() - s.len()..]),
            None => stem,
        };
        if to == name {
            renames.push(Rename {
                name,
                to: None,
                note: Some(msg!("core.names.as_is", "as it is")),
            });
            continue;
        }
        let key = to.to_lowercase();
        if let Some(other) = taken.get(&key) {
            refused.push((
                to.clone(),
                msg!(
                    "core.names.taken",
                    "two files would take this name, {a} and {b}",
                    a = other,
                    b = name
                ),
            ));
        } else if present.contains(&key) && key != name.to_lowercase() {
            refused.push((
                to.clone(),
                msg!("core.names.exists", "a file of this name is there already"),
            ));
        }
        taken.insert(key, name.clone());
        map.insert(name.clone(), to.clone());
        renames.push(Rename {
            name,
            to: Some(to),
            note: None,
        });
    }
    let refs = references(&dir, &path, &map);
    let mut p = empty(id, "rename", path.clone(), path, "rename", place);
    p.exists = true;
    p.changed = !map.is_empty() && refused.is_empty();
    p.renames = renames;
    p.refs = refs;
    p.refused = refused;
    Ok(p)
}

/// The folder in hand renamed under its parent: one leaf → a safe name from tags or
/// a custom string; refused for roots, empty names, collisions, and when unchanged.
fn prepare_folder(
    music: &Path,
    shares: &[Share],
    _decoders: &Decoders,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    let (path, dir) = resolve_library(music, &req.path)?;
    if !dir.is_dir() {
        return Err(walk::no_such_folder());
    }
    if crate::folders::is_root_folder(&path) {
        return Err(msg!("core.folder.root", "a library root is not renamed"));
    }
    let (parent, leaf) = crate::folders::split_leaf(&path).ok_or_else(walk::no_such_folder)?;
    let raw = req.name.as_deref().unwrap_or("").trim();
    if raw.is_empty() {
        return Err(msg!("core.folder.no_name", "no name given for the folder"));
    }
    if raw.contains('\\') || raw.contains("..") {
        return Err(msg!(
            "core.folder.bad_path",
            "a folder path cannot contain .. or a backslash"
        ));
    }
    let slash_count = raw.matches('/').count();
    if slash_count > 1 {
        return Err(msg!(
            "core.folder.one_slash",
            "use at most one slash (Artist/Album)"
        ));
    }
    let (base, from_rel, to_rel) = if slash_count == 1 {
        let (artist_raw, album_raw) = raw.split_once('/').unwrap();
        let artist = names::safe(artist_raw.trim());
        let album = names::safe(album_raw.trim());
        if artist.is_empty() || album.is_empty() {
            return Err(msg!(
                "core.folder.empty_name",
                "the name is empty after it is made safe"
            ));
        }
        let base = crate::folders::layout_base(&parent);
        let to_rel = format!("{artist}/{album}");
        let dest = if base.is_empty() {
            to_rel.clone()
        } else {
            format!("{base}/{to_rel}")
        };
        if dest == path {
            return Err(msg!(
                "core.folder.as_is",
                "the folder already has that name"
            ));
        }
        let from_rel = if base.is_empty() {
            path.clone()
        } else {
            path.strip_prefix(&(base.clone() + "/"))
                .unwrap_or(&path)
                .to_string()
        };
        (base, from_rel, to_rel)
    } else {
        let to = names::safe(raw);
        if to.is_empty() {
            return Err(msg!(
                "core.folder.empty_name",
                "the name is empty after it is made safe"
            ));
        }
        if to == leaf {
            return Err(msg!(
                "core.folder.as_is",
                "the folder already has that name"
            ));
        }
        (parent.clone(), leaf.clone(), to)
    };
    let base_dir = folder_dir(music, &base);
    let dest_disk = base_dir.join(&to_rel);
    let mut refused = Vec::new();
    if dest_disk.exists() {
        refused.push((
            to_rel.clone(),
            msg!("core.names.exists", "a file of this name is there already"),
        ));
    } else if slash_count == 0 {
        // Sibling leaf rename: also refuse a case-insensitive clash with another sibling.
        let present: BTreeSet<String> = fs::read_dir(&base_dir)
            .map_err(|e| text::at(base_dir.display(), e))?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_lowercase())
            .collect();
        let key = to_rel.to_lowercase();
        if present.contains(&key) && key != leaf.to_lowercase() {
            refused.push((
                to_rel.clone(),
                msg!("core.names.exists", "a file of this name is there already"),
            ));
        }
    }
    let place = placed(shares, &base)?;
    let mut p = empty(id, "folder", base.clone(), path.clone(), "rename", place);
    p.exists = true;
    p.changed = refused.is_empty();
    p.renames = vec![Rename {
        name: from_rel,
        to: Some(to_rel),
        note: None,
    }];
    p.refused = refused;
    Ok(p)
}

/// Selected album groups moved out of the dump folder into Artist/Album paths.
fn prepare_organise(
    music: &Path,
    shares: &[Share],
    decoders: &Decoders,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    let (path, dir) = resolve_library(music, &req.path)?;
    if !dir.is_dir() {
        return Err(walk::no_such_folder());
    }
    if req.moves.is_empty() {
        return Err(msg!(
            "core.organise.none_selected",
            "no album chosen to move"
        ));
    }
    let want: BTreeSet<String> = req.moves.iter().cloned().collect();
    let groups = organise::groups_for(music, &path, decoders)?;
    let place = placed(shares, &path)?;
    let mut renames = Vec::new();
    let mut refused = Vec::new();
    let mut duplicates = Vec::new();
    let mut taken: BTreeMap<String, String> = BTreeMap::new();
    let pick_for = |name: &str, to: &str| -> String {
        req.duplicates
            .iter()
            .find(|d| d.name == name && d.to == to)
            .map(|d| d.action.as_str())
            .unwrap_or("leave")
            .to_string()
    };
    for g in &groups {
        if !want.contains(&g.folder) {
            continue;
        }
        if g.album.is_empty() {
            refused.push((
                g.folder.clone(),
                msg!(
                    "core.organise.no_album",
                    "no album tag; tag the files or skip this group"
                ),
            ));
            continue;
        }
        if g.folder == path || g.folder.starts_with(&(path.clone() + "/")) {
            refused.push((
                g.folder.clone(),
                msg!(
                    "core.organise.same_folder",
                    "the destination is this folder or under it"
                ),
            ));
            continue;
        }
        let dest_dir = music.join(&g.folder);
        let present: BTreeSet<String> = if dest_dir.is_dir() {
            fs::read_dir(&dest_dir)
                .map_err(|e| text::at(dest_dir.display(), e))?
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_lowercase())
                .collect()
        } else {
            BTreeSet::new()
        };
        for name in &g.names {
            let to = walk::join(&g.folder, name);
            let key = to.to_lowercase();
            if let Some(other) = taken.get(&key) {
                refused.push((
                    to.clone(),
                    msg!(
                        "core.names.taken",
                        "two files would take this name, {a} and {b}",
                        a = other,
                        b = name
                    ),
                ));
                continue;
            }
            taken.insert(key, name.clone());
            if present.contains(&name.to_lowercase()) {
                let src = dir.join(name);
                let dst = dest_dir.join(name);
                if files_bit_identical(&src, &dst)? {
                    let action = pick_for(name, &to);
                    duplicates.push(Duplicate {
                        name: name.clone(),
                        to: to.clone(),
                        action: action.clone(),
                    });
                    match action.as_str() {
                        "replace" => renames.push(Rename {
                            name: name.clone(),
                            to: Some(to),
                            note: Some(msg!(
                                "core.organise.replace",
                                "identical file there; this copy replaces it"
                            )),
                        }),
                        "remove" => renames.push(Rename {
                            name: name.clone(),
                            to: None,
                            note: Some(msg!(
                                "core.organise.remove_dup",
                                "identical file there; this copy is removed"
                            )),
                        }),
                        _ => {
                            // leave: source stays, destination stays
                        }
                    }
                } else {
                    refused.push((
                        to.clone(),
                        msg!("core.names.exists", "a file of this name is there already"),
                    ));
                }
                continue;
            }
            renames.push(Rename {
                name: name.clone(),
                to: Some(to),
                note: None,
            });
        }
    }
    let unknown: Vec<_> = want
        .iter()
        .filter(|f| !groups.iter().any(|g| g.folder == **f))
        .cloned()
        .collect();
    for folder in unknown {
        refused.push((
            folder,
            msg!(
                "core.organise.unknown_group",
                "that album is not in this folder any more; scan again"
            ),
        ));
    }
    let mut p = empty(id, "organise", path.clone(), path, "rename", place);
    p.exists = true;
    p.changed = !renames.is_empty() && refused.is_empty();
    p.renames = renames;
    p.refused = refused;
    p.duplicates = duplicates;
    Ok(p)
}

/// Whole-file equality for organise collisions (size, then bytes).
fn files_bit_identical(a: &Path, b: &Path) -> Result<bool, Msg> {
    use std::io::Read;
    let ma = fs::metadata(a).map_err(|e| text::at(a.display(), e))?;
    let mb = fs::metadata(b).map_err(|e| text::at(b.display(), e))?;
    if ma.len() != mb.len() {
        return Ok(false);
    }
    let mut fa = fs::File::open(a).map_err(|e| text::at(a.display(), e))?;
    let mut fb = fs::File::open(b).map_err(|e| text::at(b.display(), e))?;
    let mut ba = [0u8; 65536];
    let mut bb = [0u8; 65536];
    loop {
        let na = fa.read(&mut ba).map_err(|e| text::at(a.display(), e))?;
        let nb = fb.read(&mut bb).map_err(|e| text::at(b.display(), e))?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

/// The sheets and playlists of the folder that name a renamed file: a sheet's FILE
/// lines and an m3u, m3u8 or pls entry that is the name or `./` and the name,
/// rewritten when the file is UTF-8; otherwise, and for an xspf, said and left.
fn references(dir: &Path, folder: &str, map: &BTreeMap<String, String>) -> Vec<RefChange> {
    let mut out = Vec::new();
    if map.is_empty() {
        return out;
    }
    let Ok(read) = fs::read_dir(dir) else {
        return out;
    };
    let mut names: Vec<String> = read
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        let suffix = classify::suffix(&name).unwrap_or_default();
        if !matches!(suffix.as_str(), "cue" | "m3u" | "m3u8" | "pls" | "xspf") {
            continue;
        }
        let file = dir.join(&name);
        let Ok(bytes) = fs::read(&file) else { continue };
        let names_it = map
            .keys()
            .any(|old| bytes.windows(old.len()).any(|w| w == old.as_bytes()));
        if !names_it {
            continue;
        }
        let (_, size, mtime) = stat(&file);
        let library = walk::join(folder, &name);
        let left = |note: Msg| RefChange {
            file: library.clone(),
            lines: 0,
            before: display(&bytes),
            after: display(&bytes),
            size,
            mtime,
            note: Some(note),
        };
        if suffix == "xspf" {
            out.push(left(msg!(
                "core.names.xspf",
                "an xspf is not rewritten; the Playlists tab repairs it"
            )));
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            out.push(left(msg!(
                "core.names.not_utf8",
                "not UTF-8; its line naming the file is not changed"
            )));
            continue;
        };
        let rewritten = if suffix == "cue" {
            cuefix::rename_files(text, map)
        } else {
            rename_entries(text, suffix == "pls", map)
        };
        if let Some((after, lines)) = rewritten {
            out.push(RefChange {
                file: library,
                lines,
                before: text.to_string(),
                after,
                size,
                mtime,
                note: None,
            });
        }
    }
    out
}

/// A playlist's entries that are a renamed file's name, or `./` and the name,
/// rewritten; a pls entry is a `FileN=` line. The text as it would be and the lines
/// changed; none when no entry names one.
fn rename_entries(
    text: &str,
    pls: bool,
    map: &BTreeMap<String, String>,
) -> Option<(String, usize)> {
    let mut out = String::with_capacity(text.len());
    let mut changed = 0;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let body = line.trim_end_matches(['\n', '\r']);
        let ending = &line[body.len()..];
        let bom = if i == 0 && body.starts_with('\u{FEFF}') {
            "\u{FEFF}"
        } else {
            ""
        };
        let body = &body[bom.len()..];
        let (lead, entry) = if pls {
            match body.find('=') {
                Some(eq) if body[..eq].to_ascii_lowercase().starts_with("file") => {
                    (&body[..eq + 1], &body[eq + 1..])
                }
                _ => ("", ""),
            }
        } else {
            ("", body)
        };
        let dotted = entry.strip_prefix("./");
        let old = dotted.unwrap_or(entry);
        match map.get(old.trim()) {
            Some(to) if !entry.is_empty() => {
                out.push_str(bom);
                out.push_str(lead);
                if dotted.is_some() {
                    out.push_str("./");
                }
                out.push_str(to);
                out.push_str(ending);
                changed += 1;
            }
            _ => out.push_str(line),
        }
    }
    (changed > 0).then_some((out, changed))
}

/// One of Volumio's own lists repaired: the file under Volumio's data folder written
/// whole, outside the library (no mount, no update of MPD), the journal keeping it
/// as it was. The list is named by its id, never by a path.
fn prepare_volumio_list(
    music: &Path,
    decoders: &Decoders,
    volumio_playlists: &Path,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    let places = vlists::Places::beside(volumio_playlists);
    let list_id = req.name.as_deref().unwrap_or("");
    let no_such_list = || msg!("core.write.no_such_list", "no such list");
    let list = vlists::by_id(&places, list_id).ok_or_else(no_such_list)?;
    let (exists, size, mtime) = stat(&list.path);
    if !exists {
        return Err(no_such_list());
    }
    if size > vlists::MAX_LIST {
        return Err(msg!(
            "core.write.list_too_large",
            "the list is {size} bytes; not read",
            size = size
        ));
    }
    let bytes = fs::read(&list.path).map_err(|e| text::at(list.path.display(), e))?;
    let fixed = vlists::plan(music, decoders, &list, &bytes, req.repairs.as_deref())?;
    let folder = list
        .path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut p = empty(
        id,
        "volumio_list",
        folder,
        list.path.display().to_string(),
        "write_text",
        Placed::local(),
    );
    p.exists = true;
    p.before = display(&bytes);
    p.after = fixed.after;
    p.changed = fixed.changed && p.after.as_bytes() != bytes.as_slice();
    p.size = size;
    p.mtime = mtime;
    p.repairs = fixed
        .repairs
        .into_iter()
        .map(|r| Repair {
            id: r.id,
            what: r.what,
            on: r.on,
        })
        .collect();
    p.items = fixed.changes;
    p.kept = fixed.kept;
    p.notes = fixed.notes;
    p.name = Some(list.name);
    p.list_kind = Some(list.kind);
    Ok(p)
}

/// A playlist's repair: the entries MPD cannot find looked for under the base
/// folder and written relative to the playlist's folder, the mark, the encoding, a
/// PLS's count, a `.m3u` beside an m3u8 or an xspf.
fn prepare_playlist(
    music: &Path,
    shares: &[Share],
    decoders: &Decoders,
    id: u64,
    req: &Request,
) -> Result<Preview, Msg> {
    let (path, full) = resolve_library(music, &req.path)?;
    let suffix = classify::suffix(&path).unwrap_or_default();
    if !playlist::SUFFIXES.contains(&suffix.as_str()) {
        return Err(not_a_playlist());
    }
    if !full.is_file() {
        return Err(no_such_playlist());
    }
    let folder = path
        .rsplit_once('/')
        .map(|(f, _)| f.to_string())
        .unwrap_or_default();
    let place = placed(shares, &folder)?;
    let (exists, size, mtime) = stat(&full);
    if size > plcheck::MAX_PLAYLIST {
        return Err(playlist_too_large(size));
    }
    let bytes = fs::read(&full).map_err(|e| text::at(full.display(), e))?;
    // Discard: remove the library playlist file; same write-model remove as a sheet.
    if matches!(req.repairs.as_deref(), Some([r]) if r == "remove") {
        let mut p = empty(id, "playlist", folder, path, "remove", place);
        p.exists = exists;
        p.before = display(&bytes);
        p.after = String::new();
        p.changed = true;
        p.size = size;
        p.mtime = mtime;
        return Ok(p);
    }
    let base = match &req.base {
        Some(b) => {
            resolve_library(music, b)
                .map_err(|_| {
                    msg!(
                        "core.write.base_not_library",
                        "the base is not a library path"
                    )
                })?
                .0
        }
        None => folder.clone(),
    };
    let (_, base_dir) = resolve_library(music, &base)
        .map_err(|_| msg!("core.write.no_such_base", "no such base folder"))?;
    if !base_dir.is_dir() {
        return Err(msg!("core.write.no_such_base", "no such base folder"));
    }
    let choice = plfix::Choice {
        repairs: req.repairs.as_deref(),
        encoding: req.encoding.as_deref(),
        base: &base,
    };
    let fixed = plfix::plan(music, &path, &bytes, decoders, &choice)?;
    let target_full = music.join(&fixed.target);
    let (exists, tsize, tmtime) = stat(&target_full);
    let current = if exists {
        fs::read(&target_full).map_err(|e| text::at(target_full.display(), e))?
    } else {
        Vec::new()
    };
    let changed = if exists {
        fixed.after.as_bytes() != current.as_slice()
    } else {
        !fixed.after.is_empty()
    };
    let mut p = empty(
        id,
        "playlist",
        folder,
        fixed.target.clone(),
        "write_text",
        place,
    );
    p.exists = exists;
    p.before = display(&current);
    p.after = fixed.after;
    p.changed = changed;
    p.size = tsize;
    p.mtime = tmtime;
    p.repairs = fixed.repairs;
    p.encoding = fixed.encoding;
    p.encodings = fixed.encodings;
    p.notes = fixed.notes;
    p.entries = fixed.entries;
    p.not_found = fixed.not_found;
    Ok(p)
}

/// Whether the batch's file is outside the library: a Volumio playlist, under
/// Volumio's own folder, whose paths are absolute.
fn outside(kind: &str) -> bool {
    matches!(kind, "volumio_playlist" | "volumio_list")
}

/// The batch's file on disk: the library path joined to the music directory, or the
/// absolute path as it is for a file outside the library.
fn file_on_disk(music: &Path, kind: &str, file: &str) -> PathBuf {
    if outside(kind) {
        PathBuf::from(file)
    } else {
        music.join(file)
    }
}

/// Where the batch's folder is written: the library folder (see `writable`), or the
/// absolute folder of a file outside the library, made when it is not there.
fn target(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    kind: &str,
    path: &str,
) -> Result<(PathBuf, Option<Mount>), Msg> {
    if !outside(kind) {
        return writable(music, shares, mounter, path);
    }
    let dir = PathBuf::from(path);
    fs::create_dir_all(&dir).map_err(|e| text::at(path, e))?;
    mount::write_test(&dir).map_err(|e| cannot_write(Msg::raw(path), e))?;
    Ok((dir, None))
}

fn cannot_write(place: Msg, error: String) -> Msg {
    msg!(
        "core.write.cannot_write",
        "{path}: cannot write there: {error}",
        path = place,
        error = error
    )
}

/// Where the folder is written: the folder itself for a local root, the mount's
/// target for a share, with the mount to take down after.
fn writable(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    path: &str,
) -> Result<(PathBuf, Option<Mount>), Msg> {
    match mount::place(shares, path)? {
        Where::Local => {
            let dir = folder_dir(music, path);
            mount::write_test(&dir).map_err(|e| {
                cannot_write(
                    if path.is_empty() {
                        library_root()
                    } else {
                        Msg::raw(path)
                    },
                    e,
                )
            })?;
            Ok((dir, None))
        }
        Where::Nas { share, folder } => {
            let m = mounter.mount(share, &folder)?;
            Ok((m.target.clone(), Some(m)))
        }
    }
}

/// A file written whole and moved into place, as its owner's and with its mode: the
/// library's files are mostly another user's (Volumio's Samba share takes guest
/// writes, so music copied over the network is nobody's), and the core writes them
/// through the capabilities its unit grants, leaving each file that user's. A new file
/// takes its folder's owner and the folder's mode without the search bits.
fn write_whole(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), Msg> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let part = dir.join(format!("{name}.part"));
    let file = dir.join(name);
    let owner = match fs::metadata(&file).or_else(|_| fs::metadata(dir)) {
        Ok(m) if m.is_file() => Some((m.mode() & 0o7777, m.uid(), m.gid())),
        Ok(m) => Some((m.mode() & 0o666, m.uid(), m.gid())),
        Err(_) => None,
    };
    fs::write(&part, bytes).map_err(|e| text::at(part.display(), e))?;
    if let Some((mode, uid, gid)) = owner {
        let kept = fs::set_permissions(&part, fs::Permissions::from_mode(mode))
            .and_then(|_| std::os::unix::fs::chown(&part, Some(uid), Some(gid)));
        if let Err(e) = kept {
            // Said, not fatal: the content is what the write is for, and the core run
            // by hand has no capability to give a file to another user.
            eprintln!(
                "mpdantic: {}: the owner or the mode not kept: {e}",
                file.display()
            );
        }
    }
    fs::rename(&part, &file).map_err(|e| {
        let _ = fs::remove_file(&part);
        text::at(file.display(), e)
    })
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// What the batch did, in a line; the repairs by their ids.
fn note_for(preview: &Preview, written: usize) -> Msg {
    let on = || {
        preview
            .repairs
            .iter()
            .filter(|r| r.on)
            .map(|r| r.id.to_string())
            .collect::<Vec<String>>()
    };
    match preview.kind.as_str() {
        "cue" if preview.op == "remove" => {
            msg!(
                "core.write.note_sheet_removed",
                "sheet {file} removed",
                file = preview.file
            )
        }
        "playlist" if preview.op == "remove" => {
            msg!(
                "core.write.note_playlist_removed",
                "playlist {file} discarded",
                file = preview.file
            )
        }
        "cue" => msg!(
            "core.write.note_sheet_repaired",
            "sheet {file} repaired: {repairs}",
            file = preview.file,
            repairs = on()
        ),
        "playlist" => msg!(
            "core.write.note_playlist",
            "playlist {file} written: {repairs}",
            file = preview.file,
            repairs = on()
        ),
        "volumio_playlist" => msg!(
            "core.write.note_volumio_playlist",
            "Volumio playlist \"{name}\" written from {from}",
            name = preview.name.as_deref().unwrap_or(""),
            from = preview.from.as_deref().unwrap_or("")
        ),
        "volumio_list" => msg!(
            "core.write.note_volumio_list",
            "Volumio's {kind} {name} repaired: {repairs}",
            kind = preview.list_kind.as_deref().unwrap_or("list"),
            name = preview.name.as_deref().unwrap_or(""),
            repairs = on()
        ),
        "tags" => msg!(
            "core.write.note_tags",
            "tags of {n} {n|file|files} in {folder}: {what}",
            n = written,
            folder = if preview.path.is_empty() {
                library_root()
            } else {
                Msg::raw(preview.path.clone())
            },
            what = if preview.edited {
                msg!("core.write.note_edited", "edited file by file").to_arg()
            } else {
                on().to_arg()
            }
        ),
        _ if preview.op == "remove" => msg!(
            "core.write.note_removed",
            "{file} removed",
            file = preview.file
        ),
        _ => msg!(
            "core.write.note_lines",
            "{added} {added|line|lines} added, {removed} removed in {file}",
            added = preview.added.len(),
            removed = preview.removed.len(),
            file = preview.file
        ),
    }
}

/// Record a library folder for a later Update library press.
fn defer_update(pending: &Pending, folder: &str) -> (Option<u64>, Option<Msg>, bool) {
    match pending.add(folder) {
        Ok(()) => (None, None, true),
        Err(e) => (None, Some(e), false),
    }
}

fn applied(
    batch: String,
    file: String,
    count: usize,
    updated: Option<u64>,
    update_error: Option<Msg>,
    pending: bool,
) -> Applied {
    Applied {
        batch,
        file,
        written: true,
        count,
        updated,
        update_error,
        pending,
    }
}

/// One MPD `update` covering every pending folder; clears the set on success.
pub fn flush(pending: &Pending, mpd_address: &mpd::Address) -> Result<Flushed, Msg> {
    let paths = pending.list();
    if paths.is_empty() {
        return Err(msg!(
            "core.library.nothing_pending",
            "nothing pending to update"
        ));
    }
    let path = crate::pending::flush_path_of(&paths);
    match mpd::update(mpd_address, &path) {
        Ok(n) => {
            pending.remove_all(&paths)?;
            Ok(Flushed {
                path,
                paths,
                updated: Some(n),
                update_error: None,
            })
        }
        Err(e) => Ok(Flushed {
            path,
            paths,
            updated: None,
            update_error: Some(e),
        }),
    }
}

/// The preview applied: the file checked to be what the preview read, the folder
/// made writable, the file written or removed, the batch journaled with the file's
/// bytes as they were; library folders enqueue a later MPD update.
pub fn apply(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    pending: &Pending,
    preview: &Preview,
) -> Result<Applied, Msg> {
    if !preview.changed {
        return Err(msg!("core.write.nothing_to_write", "nothing to write"));
    }
    if preview.kind == "tags" {
        return apply_tags(music, shares, mounter, journal, pending, preview);
    }
    if preview.kind == "rename" || preview.kind == "folder" {
        return apply_rename(music, shares, mounter, journal, pending, preview);
    }
    if preview.kind == "organise" {
        return apply_organise(music, shares, mounter, journal, pending, preview);
    }
    let ro = file_on_disk(music, &preview.kind, &preview.file);
    let (exists, size, mtime) = stat(&ro);
    if exists != preview.exists || size != preview.size || mtime != preview.mtime {
        return Err(msg!(
            "core.write.changed_since_preview",
            "the file changed since the preview; preview it again"
        ));
    }
    let before_bytes = if exists {
        Some(fs::read(&ro).map_err(|e| text::at(&preview.file, e))?)
    } else {
        None
    };
    let name = file_name(&preview.file);
    let (dir, mount) = target(music, shares, mounter, &preview.kind, &preview.path)?;
    let result = if preview.op == "remove" {
        fs::remove_file(dir.join(name)).map_err(|e| text::at(&preview.file, e))
    } else {
        write_whole(&dir, name, preview.after.as_bytes())
    };
    let journaled = result.and_then(|_| {
        let id = journal.new_id();
        let kept = match &before_bytes {
            Some(bytes) => Some(journal.keep(&id, "0.before", bytes)?),
            None => None,
        };
        let batch = Batch {
            id,
            kind: preview.kind.clone(),
            path: preview.path.clone(),
            when: now(),
            note: note_for(preview, 1),
            entries: vec![Entry {
                path: preview.file.clone(),
                op: preview.op.clone(),
                before: if preview.exists {
                    Some(preview.before.clone())
                } else {
                    None
                },
                before_bytes: kept,
                after: preview.after.clone(),
            }],
            undone: false,
        };
        journal.record(&batch).map(|_| batch.id)
    });
    if let Some(mut m) = mount {
        mounter.unmount(&mut m);
    }
    let batch = journaled?;
    // A write outside the library is nothing to MPD.
    let (updated, update_error, is_pending) = if outside(&preview.kind) {
        (None, None, false)
    } else {
        defer_update(pending, &preview.path)
    };
    Ok(applied(
        batch,
        preview.file.clone(),
        1,
        updated,
        update_error,
        is_pending,
    ))
}

/// The folder's files written one by one through lofty: each checked to be what the
/// preview read; a file that fails stops the batch there, with the files before it
/// written and journaled.
fn apply_tags(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    pending: &Pending,
    preview: &Preview,
) -> Result<Applied, Msg> {
    let todo: Vec<&FilePlan> = preview
        .files
        .iter()
        .filter(|f| !f.changes.is_empty())
        .collect();
    for fp in &todo {
        let (exists, size, mtime) = stat(&music.join(&fp.path));
        if !exists || size != fp.size || mtime != fp.mtime {
            return Err(msg!(
                "core.write.changed_file",
                "{path} changed since the preview; preview it again",
                path = fp.path
            ));
        }
    }
    let (dir, mount) = writable(music, shares, mounter, &preview.path)?;
    let mut entries = Vec::new();
    let mut failure = None;
    for fp in &todo {
        match tagfix::apply_file(&dir.join(&fp.name), &fp.format, &fp.changes) {
            Ok(()) => {
                let text = serde_json::to_string(&fp.changes).unwrap_or_default();
                entries.push(Entry {
                    path: fp.path.clone(),
                    op: "tags".to_string(),
                    before: Some(text.clone()),
                    before_bytes: None,
                    after: text,
                });
            }
            Err(e) => {
                failure = Some(text::at(&fp.path, e));
                break;
            }
        }
    }
    if let Some(mut m) = mount {
        mounter.unmount(&mut m);
    }
    let written = entries.len();
    let batch = if written > 0 {
        let batch = Batch {
            id: journal.new_id(),
            kind: "tags".to_string(),
            path: preview.path.clone(),
            when: now(),
            note: note_for(preview, written),
            entries,
            undone: false,
        };
        journal.record(&batch)?;
        Some(batch.id)
    } else {
        None
    };
    if let Some(e) = failure {
        return Err(match &batch {
            Some(id) => msg!(
                "core.write.tags_partial",
                "{error}; the {n} {n|file|files} before it written and journaled as batch {batch}",
                error = e,
                n = written,
                batch = id
            ),
            None => e,
        });
    }
    let (updated, update_error, is_pending) = defer_update(pending, &preview.path);
    Ok(applied(
        batch.unwrap_or_default(),
        preview.path.clone(),
        written,
        updated,
        update_error,
        is_pending,
    ))
}

/// The folder's files renamed one by one, then the sheets and playlists that name
/// them written: every source still there and every target still free as the
/// preview saw them, every reference unchanged since; a case-only change goes
/// through a temporary name, since a share does not tell the cases apart; a
/// failure stops the batch there, with what was done journaled.
fn apply_rename(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    pending: &Pending,
    preview: &Preview,
) -> Result<Applied, Msg> {
    let todo: Vec<(&str, &str)> = preview
        .renames
        .iter()
        .filter_map(|r| r.to.as_deref().map(|to| (r.name.as_str(), to)))
        .collect();
    let folder = folder_dir(music, &preview.path);
    for (from, to) in &todo {
        if !folder.join(from).exists() {
            return Err(msg!(
                "core.write.source_gone",
                "{name}: not there any more; preview again",
                name = from
            ));
        }
        if !from.eq_ignore_ascii_case(to) && folder.join(to).exists() {
            return Err(msg!(
                "core.write.target_taken",
                "{name}: a file of this name is there since the preview; preview again",
                name = to
            ));
        }
    }
    for r in preview.refs.iter().filter(|r| r.note.is_none()) {
        let (exists, size, mtime) = stat(&music.join(&r.file));
        if !exists || size != r.size || mtime != r.mtime {
            return Err(msg!(
                "core.write.changed_file",
                "{path} changed since the preview; preview it again",
                path = r.file
            ));
        }
    }
    let id = journal.new_id();
    let (dir, mount) = writable(music, shares, mounter, &preview.path)?;
    let mut entries = Vec::new();
    let mut failure = None;
    for (from, to) in &todo {
        let dest = dir.join(to);
        if let Some(parent) = dest.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                failure = Some(text::at(parent.display(), e));
                break;
            }
        }
        match rename_in(&dir, from, to) {
            Ok(()) => entries.push(Entry {
                path: walk::join(&preview.path, from),
                op: "rename".to_string(),
                before: None,
                before_bytes: None,
                after: walk::join(&preview.path, to),
            }),
            Err(e) => {
                failure = Some(text::at(walk::join(&preview.path, from), e));
                break;
            }
        }
    }
    let renamed = entries.len();
    if failure.is_none() {
        for r in preview.refs.iter().filter(|r| r.note.is_none()) {
            let name = file_name(&r.file);
            let kept = journal.keep(
                &id,
                &format!("{}.before", entries.len()),
                r.before.as_bytes(),
            );
            let written =
                kept.and_then(|kept| write_whole(&dir, name, r.after.as_bytes()).map(|_| kept));
            match written {
                Ok(kept) => entries.push(Entry {
                    path: r.file.clone(),
                    op: "write_text".to_string(),
                    before: Some(r.before.clone()),
                    before_bytes: Some(kept),
                    after: r.after.clone(),
                }),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
    }
    if let Some(mut m) = mount {
        mounter.unmount(&mut m);
    }
    let batch = if entries.is_empty() {
        None
    } else {
        let note = if preview.kind == "folder" {
            msg!(
                "core.write.note_folder",
                "folder renamed to {name} under {folder}",
                name = preview
                    .renames
                    .first()
                    .and_then(|r| r.to.clone())
                    .unwrap_or_default(),
                folder = if preview.path.is_empty() {
                    library_root()
                } else {
                    Msg::raw(preview.path.clone())
                }
            )
        } else {
            msg!(
                "core.write.note_rename",
                "{n} {n|file|files} renamed in {folder}",
                n = renamed,
                folder = if preview.path.is_empty() {
                    library_root()
                } else {
                    Msg::raw(preview.path.clone())
                }
            )
        };
        let batch = Batch {
            id: id.clone(),
            kind: preview.kind.clone(),
            path: preview.path.clone(),
            when: now(),
            note,
            entries,
            undone: false,
        };
        journal.record(&batch)?;
        Some(batch.id)
    };
    if let Some(e) = failure {
        return Err(match &batch {
            Some(id) => msg!(
                "core.write.rename_partial",
                "{error}; the {n} {n|file|files} before it renamed and journaled as batch {batch}",
                error = e,
                n = renamed,
                batch = id
            ),
            None => e,
        });
    }
    let (updated, update_error, is_pending) = defer_update(pending, &preview.path);
    Ok(applied(
        batch.unwrap_or_default(),
        preview.path.clone(),
        renamed,
        updated,
        update_error,
        is_pending,
    ))
}

/// Selected files moved into Artist/Album folders under the library anchor.
fn apply_organise(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    pending: &Pending,
    preview: &Preview,
) -> Result<Applied, Msg> {
    let moves: Vec<(&str, &str)> = preview
        .renames
        .iter()
        .filter_map(|r| r.to.as_deref().map(|to| (r.name.as_str(), to)))
        .collect();
    let removes: Vec<&str> = preview
        .renames
        .iter()
        .filter(|r| r.to.is_none())
        .map(|r| r.name.as_str())
        .collect();
    let src_dir = folder_dir(music, &preview.path);
    for from in removes.iter().copied().chain(moves.iter().map(|(f, _)| *f)) {
        if !src_dir.join(from).exists() {
            return Err(msg!(
                "core.write.source_gone",
                "{name}: not there any more; preview again",
                name = from
            ));
        }
    }
    for (from, to) in &moves {
        let dest = music.join(to);
        if dest.exists() {
            let src = src_dir.join(from);
            // Replace of a bit-identical copy is allowed; anything else is stale.
            if !files_bit_identical(&src, &dest)? {
                return Err(msg!(
                    "core.write.target_taken",
                    "{name}: a file of this name is there since the preview; preview again",
                    name = to
                ));
            }
        }
    }
    let anchor = organise::library_anchor(&preview.path);
    let mount_path = if anchor.is_empty() {
        preview.path.clone()
    } else {
        anchor.clone()
    };
    let id = journal.new_id();
    let (root, mount) = writable(music, shares, mounter, &mount_path)?;
    let disk = |library: &str| -> PathBuf {
        if mount_path.is_empty() {
            return if library.is_empty() {
                root.clone()
            } else {
                root.join(library)
            };
        }
        if library == mount_path {
            return root.clone();
        }
        let prefix = mount_path.clone() + "/";
        match library.strip_prefix(&prefix) {
            Some(rest) => root.join(rest),
            None => root.join(library),
        }
    };
    let mut entries = Vec::new();
    let mut failure = None;
    for from in &removes {
        let src_lib = walk::join(&preview.path, from);
        let src = disk(&src_lib);
        let bytes = match fs::read(&src) {
            Ok(b) => b,
            Err(e) => {
                failure = Some(text::at(src.display(), e));
                break;
            }
        };
        let kept = match journal.keep(&id, &format!("remove-{}", from.replace('/', "_")), &bytes) {
            Ok(n) => Some(n),
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        match fs::remove_file(&src) {
            Ok(()) => entries.push(Entry {
                path: src_lib,
                op: "remove".to_string(),
                before: None,
                before_bytes: kept,
                after: String::new(),
            }),
            Err(e) => {
                failure = Some(text::at(src.display(), e));
                break;
            }
        }
    }
    for (from, to) in &moves {
        let src = disk(&walk::join(&preview.path, from));
        let dst = disk(to);
        if let Some(parent) = dst.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                failure = Some(text::at(parent.display(), e));
                break;
            }
        }
        // Replace: keep the destination's bytes so undo can put Hits/ back after
        // the rename is reversed (entries undo in reverse order).
        if dst.exists() {
            let dest_bytes = match fs::read(&dst) {
                Ok(b) => b,
                Err(e) => {
                    failure = Some(text::at(dst.display(), e));
                    break;
                }
            };
            let kept = match journal.keep(
                &id,
                &format!("replace-{}", to.replace('/', "_")),
                &dest_bytes,
            ) {
                Ok(n) => Some(n),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            };
            if let Err(e) = fs::remove_file(&dst) {
                failure = Some(text::at(dst.display(), e));
                break;
            }
            entries.push(Entry {
                path: (*to).to_string(),
                op: "remove".to_string(),
                before: None,
                before_bytes: kept,
                after: String::new(),
            });
        }
        match fs::rename(&src, &dst) {
            Ok(()) => entries.push(Entry {
                path: walk::join(&preview.path, from),
                op: "rename".to_string(),
                before: None,
                before_bytes: None,
                after: (*to).to_string(),
            }),
            Err(e) => {
                failure = Some(text::at(src.display(), e));
                break;
            }
        }
    }
    let moved = entries.len();
    if let Some(mut m) = mount {
        mounter.unmount(&mut m);
    }
    let batch = if entries.is_empty() {
        None
    } else {
        let batch = Batch {
            id: id.clone(),
            kind: preview.kind.clone(),
            path: preview.path.clone(),
            when: now(),
            note: msg!(
                "core.write.note_organise",
                "{n} {n|file|files} moved out of {folder}",
                n = moved,
                folder = if preview.path.is_empty() {
                    library_root()
                } else {
                    Msg::raw(preview.path.clone())
                }
            ),
            entries,
            undone: false,
        };
        journal.record(&batch)?;
        Some(batch.id)
    };
    if let Some(e) = failure {
        return Err(match &batch {
            Some(id) => msg!(
                "core.write.rename_partial",
                "{error}; the {n} {n|file|files} before it renamed and journaled as batch {batch}",
                error = e,
                n = moved,
                batch = id
            ),
            None => e,
        });
    }
    let update_path = if anchor.is_empty() {
        preview.path.clone()
    } else {
        anchor
    };
    let (updated, update_error, is_pending) = defer_update(pending, &update_path);
    Ok(applied(
        batch.unwrap_or_default(),
        preview.path.clone(),
        moved,
        updated,
        update_error,
        is_pending,
    ))
}

/// A file renamed within its folder; a change of case alone goes through a
/// temporary name, since a share does not tell the cases apart.
fn rename_in(dir: &Path, from: &str, to: &str) -> std::io::Result<()> {
    if from != to && from.eq_ignore_ascii_case(to) {
        let via = format!("{to}.mpdantic-renaming");
        fs::rename(dir.join(from), dir.join(&via))?;
        return fs::rename(dir.join(&via), dir.join(to));
    }
    fs::rename(dir.join(from), dir.join(to))
}

/// One step of an undo, checked before anything moves.
enum Step {
    /// A text file written back, or removed where the batch made it.
    Write {
        name: String,
        bytes: Option<Vec<u8>>,
    },
    /// A file renamed back.
    Rename { from: String, to: String },
    /// A file moved back across folders (organise); library paths.
    Move { from_path: String, to_path: String },
    /// A source removed for a bit-identical organise collision; restore by library path.
    Restore {
        library_path: String,
        bytes: Vec<u8>,
    },
}

fn parent_library(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

fn organise_disk(root: &Path, anchor: &str, library: &str) -> PathBuf {
    if anchor.is_empty() {
        return if library.is_empty() {
            root.to_path_buf()
        } else {
            root.join(library)
        };
    }
    if library == anchor {
        return root.to_path_buf();
    }
    let prefix = anchor.to_string() + "/";
    match library.strip_prefix(&prefix) {
        Some(rest) => root.join(rest),
        None => root.join(library),
    }
}

/// A batch undone: every entry checked first, in reverse, to be what the batch
/// left (a text file holds what was written, or nothing where one was removed; a
/// renamed file is at its new name and nothing is at its old), then each put back,
/// the before written byte for byte, the file removed, the name given back (the
/// tags set back key by key); the batch marked, MPD told.
pub fn undo(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    pending: &Pending,
    id: &str,
) -> Result<Applied, Msg> {
    let batch = journal.load(id).ok_or_else(undo::no_such_batch)?;
    if batch.undone {
        return Err(msg!("core.write.already_undone", "already undone"));
    }
    if batch.kind == "tags" {
        return undo_tags(music, shares, mounter, journal, pending, &batch);
    }
    let Some(first) = batch.entries.first() else {
        return Err(msg!("core.write.empty_batch", "an empty batch"));
    };
    let mut steps = Vec::new();
    for entry in batch.entries.iter().rev() {
        if entry.op == "rename" {
            let src_parent = parent_library(&entry.path);
            let dst_parent = parent_library(&entry.after);
            let from = file_name(&entry.path).to_string();
            let to = file_name(&entry.after).to_string();
            // Organise and folder layout journal full library paths; a layout
            // under an artist that already held the leaf keeps the same parent,
            // so leaf Rename under batch.path (the layout base) would miss.
            // Folder leaf renames use Move too (after → path); same result.
            if src_parent != dst_parent || batch.kind == "organise" || batch.kind == "folder" {
                let at = folder_dir(music, &dst_parent).join(&to);
                let back = folder_dir(music, &src_parent).join(&from);
                if !at.exists() {
                    return Err(msg!(
                        "core.write.not_at",
                        "{path}: not at {after} any more; not undone",
                        path = entry.path,
                        after = entry.after
                    ));
                }
                if back.exists() && !from.eq_ignore_ascii_case(&to) {
                    return Err(msg!(
                        "core.write.name_taken_since",
                        "{path}: a file is there again at the old name; not undone",
                        path = entry.path
                    ));
                }
                steps.push(Step::Move {
                    from_path: entry.after.clone(),
                    to_path: entry.path.clone(),
                });
                continue;
            }
            let folder = folder_dir(music, &batch.path);
            if !folder.join(&to).exists() {
                return Err(msg!(
                    "core.write.not_at",
                    "{path}: not at {after} any more; not undone",
                    path = entry.path,
                    after = entry.after
                ));
            }
            if !from.eq_ignore_ascii_case(&to) && folder.join(&from).exists() {
                return Err(msg!(
                    "core.write.name_taken_since",
                    "{path}: a file is there again at the old name; not undone",
                    path = entry.path
                ));
            }
            steps.push(Step::Rename { from: to, to: from });
            continue;
        }
        if entry.op == "remove" && batch.kind == "organise" {
            let disk = music.join(&entry.path);
            // Replace journals remove-of-dest then rename-onto-dest. Entries undo
            // in reverse, so a Move that vacates this path is already in `steps`
            // and the dest will be empty when Restore runs.
            let vacated = steps.iter().any(|s| {
                matches!(
                    s,
                    Step::Move { from_path, .. } if from_path == &entry.path
                )
            });
            if disk.exists() && !vacated {
                return Err(msg!(
                    "core.write.file_back",
                    "a file is there again where the batch removed one; not undone"
                ));
            }
            let bytes = match &entry.before_bytes {
                Some(name) => journal.kept(&batch.id, name)?,
                None => {
                    return Err(msg!(
                        "core.write.changed_since_batch",
                        "the file changed since the batch; not undone"
                    ));
                }
            };
            steps.push(Step::Restore {
                library_path: entry.path.clone(),
                bytes,
            });
            continue;
        }
        let current = fs::read(file_on_disk(music, &batch.kind, &entry.path)).ok();
        if entry.op == "remove" {
            if current.is_some() {
                return Err(msg!(
                    "core.write.file_back",
                    "a file is there again where the batch removed one; not undone"
                ));
            }
        } else if current.as_deref() != Some(entry.after.as_bytes()) {
            return Err(msg!(
                "core.write.changed_since_batch",
                "the file changed since the batch; not undone"
            ));
        }
        let bytes: Option<Vec<u8>> = match &entry.before_bytes {
            Some(name) => Some(journal.kept(&batch.id, name)?),
            None => entry.before.as_ref().map(|s| s.as_bytes().to_vec()),
        };
        steps.push(Step::Write {
            name: file_name(&entry.path).to_string(),
            bytes,
        });
    }
    let undo_mount_path = if batch.kind == "organise" {
        let a = organise::library_anchor(&batch.path);
        if a.is_empty() {
            batch.path.clone()
        } else {
            a
        }
    } else {
        batch.path.clone()
    };
    let (dir, mount) = target(music, shares, mounter, &batch.kind, &undo_mount_path)?;
    let mut done = 0;
    let mut failure = None;
    for step in &steps {
        let result = match step {
            Step::Write {
                name,
                bytes: Some(bytes),
            } => write_whole(&dir, name, bytes),
            Step::Write { name, bytes: None } => fs::remove_file(dir.join(name))
                .map_err(|e| text::at(walk::join(&batch.path, name), e)),
            Step::Rename { from, to } => {
                rename_in(&dir, from, to).map_err(|e| text::at(walk::join(&batch.path, from), e))
            }
            Step::Restore {
                library_path,
                bytes,
            } => {
                let disk = organise_disk(&dir, &undo_mount_path, library_path);
                if let Some(parent) = disk.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                let name = file_name(library_path);
                let parent = disk.parent().unwrap_or(&dir);
                write_whole(parent, name, bytes)
            }
            Step::Move { from_path, to_path } => {
                let from = music.join(from_path);
                // When a share is mounted, music.join may be read-only; prefer paths under dir.
                let from_disk = organise_disk(&dir, &undo_mount_path, from_path);
                let to_disk = organise_disk(&dir, &undo_mount_path, to_path);
                let src = if from_disk.exists() { from_disk } else { from };
                if let Some(parent) = to_disk.parent() {
                    let _ = fs::create_dir_all(parent);
                }
                match fs::rename(&src, &to_disk) {
                    Ok(()) => {
                        // Folder layout undo: drop an empty Artist/ folder left behind.
                        if batch.kind == "folder" {
                            if let Some(parent) = Path::new(from_path).parent() {
                                let p = organise_disk(
                                    &dir,
                                    &undo_mount_path,
                                    &parent.to_string_lossy(),
                                );
                                let _ = fs::remove_dir(p);
                            }
                        }
                        Ok(())
                    }
                    Err(e) => Err(text::at(src.display(), e)),
                }
            }
        };
        match result {
            Ok(()) => done += 1,
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    if let Some(mut m) = mount {
        mounter.unmount(&mut m);
    }
    if let Some(e) = failure {
        if done == 0 {
            return Err(e);
        }
        return Err(msg!(
            "core.write.undo_partial",
            "{error}; {n} {n|file|files} put back before it, the undo can be run again",
            error = e,
            n = done
        ));
    }
    journal.mark_undone(id)?;
    let (updated, update_error, is_pending) = if outside(&batch.kind) {
        (None, None, false)
    } else {
        defer_update(pending, &batch.path)
    };
    Ok(applied(
        id.to_string(),
        first.path.clone(),
        done,
        updated,
        update_error,
        is_pending,
    ))
}

/// Values the same as the batch wrote them: as they are, or as the same number,
/// since a writer keeps a number as a number and a track written "01" reads back
/// "1".
fn same_values(a: &[String], b: &[String]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x == y
                || matches!(
                    (x.trim().parse::<u64>(), y.trim().parse::<u64>()),
                    (Ok(p), Ok(q)) if p == q
                )
        })
}

/// The tags set back: each key checked to hold what the batch wrote (or what it
/// held before, so an undo cut short runs again), then the change the other way.
fn undo_tags(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    pending: &Pending,
    batch: &Batch,
) -> Result<Applied, Msg> {
    let mut todo: Vec<(String, String, Vec<KeyChange>)> = Vec::new();
    for entry in &batch.entries {
        let changes: Vec<KeyChange> =
            serde_json::from_str(&entry.after).map_err(|e| text::at(&entry.path, e))?;
        let suffix = classify::suffix(&entry.path).unwrap_or_default();
        let format = tags::format_of(&suffix).unwrap_or("").to_string();
        let raw = tags::read(&music.join(&entry.path), &suffix);
        if let Some(p) = &raw.problem {
            return Err(msg!(
                "core.write.not_undone",
                "{path}: {problem}; not undone",
                path = entry.path,
                problem = p
            ));
        }
        let mut reverse = Vec::new();
        for c in changes {
            let current = tagfix::current_values(&raw, &c.kind, &c.key);
            if same_values(&current, &c.after) {
                reverse.push(KeyChange {
                    kind: c.kind,
                    key: c.key,
                    before: c.after,
                    after: c.before,
                });
            } else if !same_values(&current, &c.before) {
                return Err(msg!(
                    "core.write.changed_key",
                    "{path}: {key} changed since the batch; not undone",
                    path = entry.path,
                    key = c.key
                ));
            }
        }
        if !reverse.is_empty() {
            todo.push((entry.path.clone(), format, reverse));
        }
    }
    let (dir, mount) = writable(music, shares, mounter, &batch.path)?;
    let mut put_back = 0;
    let mut failure = None;
    for (path, format, reverse) in &todo {
        match tagfix::apply_file(&dir.join(file_name(path)), format, reverse) {
            Ok(()) => put_back += 1,
            Err(e) => {
                failure = Some(text::at(path, e));
                break;
            }
        }
    }
    if let Some(mut m) = mount {
        mounter.unmount(&mut m);
    }
    if let Some(e) = failure {
        return Err(msg!(
            "core.write.undo_partial",
            "{error}; {n} {n|file|files} put back before it, the undo can be run again",
            error = e,
            n = put_back
        ));
    }
    journal.mark_undone(&batch.id)?;
    let (updated, update_error, is_pending) = defer_update(pending, &batch.path);
    Ok(applied(
        batch.id.clone(),
        batch.path.clone(),
        put_back,
        updated,
        update_error,
        is_pending,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::tests::sample_config;
    use crate::mpd::tests::{decoders_of, fake_mpd};
    use crate::pending::Pending;
    use crate::tagfix::tests::{faulty_flac, real_flac};
    use std::sync::{Arc, Mutex};

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpdantic-write-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("music/INTERNAL/Jazz")).unwrap();
        fs::create_dir_all(dir.join("music/NAS/multimedia/Dune")).unwrap();
        fs::create_dir_all(dir.join("run")).unwrap();
        dir
    }

    fn decoders() -> Decoders {
        decoders_of(&[("flac", &["flac"]), ("mad", &["mp3"])])
    }

    fn request(path: &str, lines: &[&str]) -> Request {
        Request {
            kind: "mpdignore".into(),
            path: path.into(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
            repairs: None,
            encoding: None,
            album: None,
            album_artist: None,
            date: None,
            base: None,
            name: None,
            edits: None,
            pattern: None,
            moves: Vec::new(),
            duplicates: Vec::new(),
        }
    }

    fn sheet(path: &str, repairs: Option<&[&str]>) -> Request {
        Request {
            kind: "cue".into(),
            path: path.into(),
            lines: Vec::new(),
            repairs: repairs.map(|r| r.iter().map(|s| s.to_string()).collect()),
            encoding: None,
            album: None,
            album_artist: None,
            date: None,
            base: None,
            name: None,
            edits: None,
            pattern: None,
            moves: Vec::new(),
            duplicates: Vec::new(),
        }
    }

    fn folder_tags(path: &str, repairs: Option<&[&str]>, album: Option<&str>) -> Request {
        Request {
            kind: "tags".into(),
            path: path.into(),
            lines: Vec::new(),
            repairs: repairs.map(|r| r.iter().map(|s| s.to_string()).collect()),
            encoding: None,
            album: album.map(str::to_string),
            album_artist: None,
            date: None,
            base: None,
            name: None,
            edits: None,
            pattern: None,
            moves: Vec::new(),
            duplicates: Vec::new(),
        }
    }

    fn local_mounter(dir: &Path) -> Mounter {
        Mounter::new(
            &dir.join("run"),
            Box::new(|_| panic!("no mount for a local root")),
        )
    }

    #[test]
    fn a_preview_says_what_would_be_written_and_refuses_what_mpd_would_not_read() {
        let dir = temp("preview");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        fs::write(music.join("INTERNAL/Jazz/.mpdignore"), "*.iso\n#recycle").unwrap();
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            1,
            &request(
                "INTERNAL/Jazz",
                &[
                    "*.iso",
                    " *.mov ",
                    "#recycle",
                    "[#]recycle",
                    "",
                    "*.mov",
                    "bad\0",
                ],
            ),
        )
        .unwrap();
        assert_eq!(p.file, "INTERNAL/Jazz/.mpdignore");
        assert!(p.exists);
        assert_eq!(p.before, "*.iso\n#recycle");
        assert_eq!(p.added, ["*.mov", "[#]recycle"]);
        assert_eq!(
            p.already,
            ["*.iso", "#recycle"],
            "a line of the file stays, a comment too"
        );
        assert!(p.removed.is_empty());
        assert_eq!(p.refused.len(), 1, "{:?}", p.refused);
        assert_eq!(p.refused[0].0, "bad\0");
        assert_eq!(
            p.after, "*.iso\n*.mov\n#recycle\n[#]recycle\n",
            "in the order of the field, the file edited whole"
        );
        assert_eq!(p.place, "local");
        assert!(p.changed);
        assert!(p.size > 0);
        // The file edited whole: a line left out is removed; everything left out
        // removes the file; a new comment line is refused with the line that works.
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            7,
            &request("INTERNAL/Jazz", &["*.mov", "#snapshot"]),
        )
        .unwrap();
        assert_eq!(p.removed, ["*.iso", "#recycle"]);
        assert_eq!(p.added, ["*.mov"]);
        assert_eq!(p.refused[0].0, "#snapshot");
        assert!(p.refused[0].1.contains("[#]snapshot"));
        assert_eq!(p.after, "*.mov\n");
        assert_eq!(p.op, "write_text");
        assert!(p.changed);
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            8,
            &request("INTERNAL/Jazz", &["*.iso", "#recycle"]),
        )
        .unwrap();
        assert!(!p.changed, "the file as it is");
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            9,
            &request("INTERNAL/Jazz", &[]),
        )
        .unwrap();
        assert_eq!(p.op, "remove");
        assert!(p.changed);
        assert_eq!(p.removed, ["*.iso", "#recycle"]);
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            2,
            &request("NAS/multimedia/Dune", &["*.srt"]),
        )
        .unwrap();
        assert!(!p.exists);
        assert_eq!(p.after, "*.srt\n");
        assert_eq!(p.place, "nas");
        assert_eq!(p.share.as_deref(), Some("multimedia"));
        assert_eq!(p.share_folder.as_deref(), Some("Dune"));
        assert!(prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            3,
            &request("../x", &["a"])
        )
        .is_err());
        assert!(prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            3,
            &request("INTERNAL/Rock", &["a"])
        )
        .is_err());
        let mut odd = request("INTERNAL", &[]);
        odd.kind = "nothing".into();
        assert!(prepare(&music, &shares, &d, &dir.join("playlists"), 3, &odd).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_local_batch_is_written_journaled_updated_and_undone() {
        let dir = temp("local");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            1,
            &request("INTERNAL/Jazz", &["*.iso", "[#]recycle"]),
        )
        .unwrap();
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert!(a.written);
        assert!(a.pending);
        assert_eq!(a.updated, None);
        assert_eq!(
            fs::read_to_string(music.join("INTERNAL/Jazz/.mpdignore")).unwrap(),
            "*.iso\n[#]recycle\n"
        );
        assert!(!music.join("INTERNAL/Jazz/.mpdignore.part").exists());
        let list = journal.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, a.batch);
        assert_eq!(
            list[0].note,
            "2 lines added, 0 removed in INTERNAL/Jazz/.mpdignore"
        );
        assert_eq!(list[0].entries[0].before, None);
        assert_eq!(list[0].entries[0].before_bytes, None);
        // The same preview again: the file moved on.
        assert!(apply(&music, &shares, &mounter, &journal, &pending, &p)
            .unwrap_err()
            .contains("changed since"));
        // A second batch on top (the file whole, a line added), then both undone in
        // order.
        let p2 = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            2,
            &request("INTERNAL/Jazz", &["*.iso", "[#]recycle", "*.mov"]),
        )
        .unwrap();
        let a2 = apply(&music, &shares, &mounter, &journal, &pending, &p2).unwrap();
        assert_eq!(
            fs::read_to_string(music.join("INTERNAL/Jazz/.mpdignore")).unwrap(),
            "*.iso\n[#]recycle\n*.mov\n"
        );
        assert_eq!(
            journal.load(&a2.batch).unwrap().entries[0]
                .before_bytes
                .as_deref(),
            Some("0.before")
        );
        // The first cannot be undone while the second sits on it.
        assert!(
            undo(&music, &shares, &mounter, &journal, &pending, &a.batch)
                .unwrap_err()
                .contains("changed since")
        );
        let u2 = undo(&music, &shares, &mounter, &journal, &pending, &a2.batch).unwrap();
        assert!(u2.pending);
        assert_eq!(u2.updated, None);
        assert_eq!(
            fs::read_to_string(music.join("INTERNAL/Jazz/.mpdignore")).unwrap(),
            "*.iso\n[#]recycle\n"
        );
        let u1 = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert!(u1.written);
        assert!(!music.join("INTERNAL/Jazz/.mpdignore").exists());
        assert!(journal.load(&a.batch).unwrap().undone);
        assert!(
            undo(&music, &shares, &mounter, &journal, &pending, &a.batch)
                .unwrap_err()
                .contains("already")
        );
        let empty = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            3,
            &request("INTERNAL/Jazz", &[""]),
        )
        .unwrap();
        assert!(apply(&music, &shares, &mounter, &journal, &pending, &empty)
            .unwrap_err()
            .contains("nothing"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_batch_on_a_share_mounts_writes_through_the_mount_and_unmounts() {
        let dir = temp("nas");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        // The runner makes the mount's target the share's folder itself, so a write
        // through the mount lands where the read-only view reads it; the unmount
        // leaves the empty folder a real one leaves.
        let view = music.join("NAS/multimedia/Dune");
        let mounter = Mounter::new(
            &dir.join("run"),
            Box::new(move |words: &[String]| {
                recorded.lock().unwrap().push(words.to_vec());
                if words[0] == "/bin/mount" {
                    let target = PathBuf::from(&words[4]);
                    fs::remove_dir(&target).unwrap();
                    std::os::unix::fs::symlink(&view, &target).unwrap();
                } else {
                    let target = PathBuf::from(&words[1]);
                    fs::remove_file(&target).unwrap();
                    fs::create_dir(&target).unwrap();
                }
                Ok(())
            }),
        );
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            1,
            &request("NAS/multimedia/Dune", &["*.srt", "*.txt"]),
        )
        .unwrap();
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert!(a.written);
        assert_eq!(
            fs::read_to_string(music.join("NAS/multimedia/Dune/.mpdignore")).unwrap(),
            "*.srt\n*.txt\n"
        );
        let words = calls.lock().unwrap().clone();
        assert_eq!(words.len(), 2);
        assert_eq!(
            words[0][3],
            "//192.168.30.1/multimedia/broadcast/Video/Movies/Dune"
        );
        assert_eq!(words[1][0], "/bin/umount");
        assert!(!dir.join("run/mounts/k1/Dune").exists());
        let u = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert!(u.written);
        assert!(!music.join("NAS/multimedia/Dune/.mpdignore").exists());
        assert_eq!(calls.lock().unwrap().len(), 4);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sheet_is_repaired_journaled_byte_for_byte_and_undone_exactly() {
        let dir = temp("cue");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let original: &[u8] = b"\xEF\xBB\xBFTITLE \"Caf\xe9\"\r\nFILE \"album.flac\" BINARY\r\nTRACK 01 AUDIO\r\nINDEX 01 00:00:00\r\n";
        fs::write(music.join("INTERNAL/Jazz/album.flac"), b"x").unwrap();
        fs::write(music.join("INTERNAL/Jazz/album.cue"), original).unwrap();
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            1,
            &sheet("INTERNAL/Jazz/album.cue", None),
        )
        .unwrap();
        assert_eq!(p.kind, "cue");
        assert_eq!(p.path, "INTERNAL/Jazz");
        assert_eq!(p.file, "INTERNAL/Jazz/album.cue");
        assert_eq!(p.op, "write_text");
        assert!(p.exists);
        assert!(p.changed);
        assert_eq!(
            p.repairs.iter().map(|r| r.id).collect::<Vec<_>>(),
            ["encoding", "bom", "type"]
        );
        assert_eq!(p.encoding.as_deref(), Some("windows-1252"));
        assert!(p
            .before
            .starts_with("\u{ef}\u{bb}\u{bf}TITLE \"Caf\u{e9}\""));
        assert_eq!(
            p.after,
            "TITLE \"Caf\u{e9}\"\r\nFILE \"album.flac\" WAVE\r\nTRACK 01 AUDIO\r\nINDEX 01 00:00:00\r\n"
        );
        assert_eq!(p.lines_changed, 1);
        assert!(p.remaining.is_empty(), "{:?}", p.remaining);
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert!(a.pending);
        assert_eq!(a.updated, None);
        assert_eq!(
            fs::read(music.join("INTERNAL/Jazz/album.cue")).unwrap(),
            p.after.as_bytes()
        );
        let batch = journal.load(&a.batch).unwrap();
        assert_eq!(
            batch.note,
            "sheet INTERNAL/Jazz/album.cue repaired: encoding, bom, type"
        );
        assert_eq!(batch.entries[0].op, "write_text");
        assert_eq!(
            journal.kept(&a.batch, "0.before").unwrap(),
            original,
            "the bytes as they were, Latin-1 and mark included"
        );
        // Nothing left to repair now; the undo brings the bytes back exactly.
        let again = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            2,
            &sheet("INTERNAL/Jazz/album.cue", None),
        )
        .unwrap();
        assert!(!again.changed);
        assert!(again.repairs.is_empty());
        assert!(apply(&music, &shares, &mounter, &journal, &pending, &again)
            .unwrap_err()
            .contains("nothing"));
        let u = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert!(u.written);
        assert_eq!(
            fs::read(music.join("INTERNAL/Jazz/album.cue")).unwrap(),
            original
        );
        // Not a sheet, not there, not a path.
        assert!(prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            3,
            &sheet("INTERNAL/Jazz/album.flac", None)
        )
        .is_err());
        assert!(prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            3,
            &sheet("INTERNAL/Jazz/gone.cue", None)
        )
        .is_err());
        assert!(prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            3,
            &sheet("../album.cue", None)
        )
        .is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sheet_removed_comes_back_with_the_undo_unless_something_took_its_place() {
        let dir = temp("remove");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let text = b"FILE \"image.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n";
        fs::write(music.join("INTERNAL/Jazz/image.flac"), b"x").unwrap();
        fs::write(music.join("INTERNAL/Jazz/a.cue"), text).unwrap();
        fs::write(music.join("INTERNAL/Jazz/b.cue"), text).unwrap();
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            1,
            &sheet("INTERNAL/Jazz/b.cue", None),
        )
        .unwrap();
        assert_eq!(p.op, "write_text");
        assert!(!p.changed);
        assert_eq!(
            p.repairs.iter().map(|r| (r.id, r.on)).collect::<Vec<_>>(),
            [("remove", false)]
        );
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            2,
            &sheet("INTERNAL/Jazz/b.cue", Some(&["remove"])),
        )
        .unwrap();
        assert_eq!(p.op, "remove");
        assert!(p.changed);
        assert!(p.after.is_empty());
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert!(!music.join("INTERNAL/Jazz/b.cue").exists());
        assert_eq!(
            journal.load(&a.batch).unwrap().note,
            "sheet INTERNAL/Jazz/b.cue removed"
        );
        assert_eq!(journal.load(&a.batch).unwrap().entries[0].op, "remove");
        // Something else put a file there: the undo does not write over it.
        fs::write(music.join("INTERNAL/Jazz/b.cue"), b"new").unwrap();
        assert!(
            undo(&music, &shares, &mounter, &journal, &pending, &a.batch)
                .unwrap_err()
                .contains("there again")
        );
        fs::remove_file(music.join("INTERNAL/Jazz/b.cue")).unwrap();
        let u = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert!(u.written);
        assert_eq!(fs::read(music.join("INTERNAL/Jazz/b.cue")).unwrap(), text);
        assert!(journal.load(&a.batch).unwrap().undone);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_library_playlist_discarded_comes_back_with_the_undo() {
        let dir = temp("playlist-discard");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let text = b"#EXTM3U\n01.flac\n";
        fs::create_dir_all(music.join("INTERNAL/Jazz")).unwrap();
        fs::write(music.join("INTERNAL/Jazz/mix.m3u"), text).unwrap();
        let req = Request {
            kind: "playlist".into(),
            path: "INTERNAL/Jazz/mix.m3u".into(),
            lines: Vec::new(),
            repairs: Some(vec!["remove".into()]),
            encoding: None,
            album: None,
            album_artist: None,
            date: None,
            base: None,
            name: None,
            edits: None,
            pattern: None,
            moves: Vec::new(),
            duplicates: Vec::new(),
        };
        let p = prepare(&music, &shares, &d, &dir.join("playlists"), 1, &req).unwrap();
        assert_eq!(p.kind, "playlist");
        assert_eq!(p.op, "remove");
        assert!(p.changed);
        assert!(p.after.is_empty());
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert!(!music.join("INTERNAL/Jazz/mix.m3u").exists());
        assert_eq!(
            journal.load(&a.batch).unwrap().note,
            "playlist INTERNAL/Jazz/mix.m3u discarded"
        );
        let u = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert!(u.written);
        assert_eq!(fs::read(music.join("INTERNAL/Jazz/mix.m3u")).unwrap(), text);
        assert!(prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            2,
            &Request {
                kind: "playlist".into(),
                path: "INTERNAL/Jazz/not-a-playlist.txt".into(),
                lines: Vec::new(),
                repairs: Some(vec!["remove".into()]),
                encoding: None,
                album: None,
                album_artist: None,
                date: None,
                base: None,
                name: None,
                edits: None,
                pattern: None,
                moves: Vec::new(),
                duplicates: Vec::new(),
            },
        )
        .is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_whole_write_keeps_the_files_mode_and_a_new_file_takes_its_folders() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("whole");
        let folder = dir.join("folder");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("a.txt"), "one\n").unwrap();
        fs::set_permissions(folder.join("a.txt"), fs::Permissions::from_mode(0o600)).unwrap();
        write_whole(&folder, "a.txt", b"two\n").unwrap();
        assert_eq!(fs::read_to_string(folder.join("a.txt")).unwrap(), "two\n");
        assert_eq!(
            fs::metadata(folder.join("a.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600,
            "the file's own mode kept"
        );
        assert!(!folder.join("a.txt.part").exists());
        // A new file: the folder's mode without the search bits.
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o775)).unwrap();
        write_whole(&folder, "b.txt", b"new\n").unwrap();
        assert_eq!(
            fs::metadata(folder.join("b.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o664
        );
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        write_whole(&folder, "c.txt", b"new\n").unwrap();
        assert_eq!(
            fs::metadata(folder.join("c.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o644
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_library_playlist_is_imported_as_volumios_and_taken_back() {
        let dir = temp("volumio");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        // Volumio's folder is made by the write when it is not there.
        let playlists = dir.join("playlists");
        fs::write(
            music.join("INTERNAL/Jazz/01.flac"),
            real_flac(&[("TITLE", "One"), ("ARTIST", "Ann"), ("ALBUM", "Live")]),
        )
        .unwrap();
        fs::write(music.join("INTERNAL/Jazz/mix.m3u"), "01.flac\ngone.flac\n").unwrap();
        let mut req = Request {
            kind: "volumio_playlist".into(),
            path: "INTERNAL/Jazz/mix.m3u".into(),
            lines: Vec::new(),
            repairs: None,
            encoding: None,
            album: None,
            album_artist: None,
            date: None,
            base: None,
            name: None,
            edits: None,
            pattern: None,
            moves: Vec::new(),
            duplicates: Vec::new(),
        };
        let p = prepare(&music, &shares, &d, &playlists, 1, &req).unwrap();
        assert_eq!(p.kind, "volumio_playlist");
        assert_eq!(p.name.as_deref(), Some("mix"));
        assert_eq!(p.from.as_deref(), Some("INTERNAL/Jazz/mix.m3u"));
        assert_eq!(p.path, playlists.display().to_string());
        assert_eq!(p.file, playlists.join("mix").display().to_string());
        assert!(!p.exists);
        assert!(p.changed);
        assert_eq!(p.imported.len(), 2);
        assert_eq!(p.imported[0].state, "song");
        assert_eq!(p.imported[0].title.as_deref(), Some("One"));
        assert_eq!(p.imported[1].state, "left_out");
        assert_eq!(p.left_out, 1);
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert_eq!(a.updated, None, "nothing for MPD to update");
        assert!(!a.pending, "Volumio playlists stay off the pending set");
        assert_eq!(a.update_error, None);
        assert!(pending.list().is_empty());
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(playlists.join("mix")).unwrap()).unwrap();
        assert_eq!(written[0]["uri"], "mnt/INTERNAL/Jazz/01.flac");
        assert_eq!(written[0]["service"], "mpd");
        let batch = journal.load(&a.batch).unwrap();
        assert_eq!(
            batch.note,
            "Volumio playlist \"mix\" written from INTERNAL/Jazz/mix.m3u"
        );
        // The same name again is a change of that file, with the old one journaled.
        req.name = Some("mix".into());
        let again = prepare(&music, &shares, &d, &playlists, 2, &req).unwrap();
        assert!(again.exists);
        assert!(!again.changed, "the same items again");
        req.name = Some("Mix two".into());
        let other = prepare(&music, &shares, &d, &playlists, 3, &req).unwrap();
        assert_eq!(other.file, playlists.join("Mix two").display().to_string());
        req.name = Some("a/b".into());
        assert!(prepare(&music, &shares, &d, &playlists, 4, &req).is_err());
        let u = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert!(u.written);
        assert_eq!(u.updated, None);
        assert!(!playlists.join("mix").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn by_pattern(kind: &str, pattern: &str) -> Request {
        Request {
            kind: kind.to_string(),
            path: "INTERNAL/Jazz".to_string(),
            lines: Vec::new(),
            repairs: None,
            encoding: None,
            album: None,
            album_artist: None,
            date: None,
            base: None,
            name: None,
            edits: None,
            pattern: Some(pattern.to_string()),
            moves: Vec::new(),
            duplicates: Vec::new(),
        }
    }

    #[test]
    fn the_files_of_a_folder_are_named_from_their_tags_with_the_sheet_and_playlists_following_and_given_back(
    ) {
        let dir = temp("rename");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let jazz = music.join("INTERNAL/Jazz");
        let flac = |title: Option<&str>, track: Option<&str>| {
            let mut tags: Vec<(&str, &str)> = vec![("ARTIST", "A")];
            if let Some(t) = title {
                tags.push(("TITLE", t));
            }
            if let Some(n) = track {
                tags.push(("TRACKNUMBER", n));
            }
            real_flac(&tags)
        };
        fs::write(
            jazz.join("a.flac"),
            flac(Some("One: Alpha/Beta"), Some("1")),
        )
        .unwrap();
        fs::write(jazz.join("b.flac"), flac(Some("Two"), Some("2/9"))).unwrap();
        fs::write(jazz.join("c.flac"), flac(None, Some("3"))).unwrap();
        fs::write(jazz.join("02 Two.flac"), flac(Some("Two"), Some("2"))).unwrap();
        let sheet = "FILE \"a.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\nFILE \"zzz.flac\" WAVE\n";
        fs::write(jazz.join("album.cue"), sheet).unwrap();
        fs::write(
            jazz.join("list.m3u"),
            "#EXTM3U\n./a.flac\nb.flac\nother.flac\n",
        )
        .unwrap();
        fs::write(
            jazz.join("list.pls"),
            "[playlist]\nFile1=b.flac\nNumberOfEntries=1\n",
        )
        .unwrap();
        let pl = dir.join("playlists");
        // A pattern that cannot work is refused before anything is read.
        let e = prepare(&music, &shares, &d, &pl, 1, &by_pattern("rename", "{size}")).unwrap_err();
        assert_eq!(e.k, "core.names.bad_field");
        // A name that is there already: refused whole, nothing moves.
        let p = prepare(
            &music,
            &shares,
            &d,
            &pl,
            1,
            &by_pattern("rename", "{track} {title}"),
        )
        .unwrap();
        assert!(!p.changed);
        assert!(p
            .refused
            .iter()
            .any(|(n, why)| n == "02 Two.flac" && why.k == "core.names.exists"));
        fs::remove_file(jazz.join("02 Two.flac")).unwrap();
        let p = prepare(
            &music,
            &shares,
            &d,
            &pl,
            2,
            &by_pattern("rename", "{track} {title}"),
        )
        .unwrap();
        assert!(p.changed);
        assert_eq!(
            (p.kind.as_str(), p.op.as_str(), p.path.as_str()),
            ("rename", "rename", "INTERNAL/Jazz")
        );
        let by = |n: &str| p.renames.iter().find(|r| r.name == n).unwrap();
        assert_eq!(by("a.flac").to.as_deref(), Some("01 One_ Alpha_Beta.flac"));
        assert_eq!(by("b.flac").to.as_deref(), Some("02 Two.flac"));
        assert!(by("c.flac").to.is_none());
        assert_eq!(by("c.flac").note.as_ref().unwrap().k, "core.names.missing");
        assert_eq!(by("c.flac").note.as_ref().unwrap().en, "no title");
        // The sheet and the playlists follow; an entry naming nothing renamed stays.
        assert_eq!(p.refs.len(), 3, "{:?}", p.refs);
        let r = |f: &str| {
            p.refs
                .iter()
                .find(|r| r.file == format!("INTERNAL/Jazz/{f}"))
                .unwrap()
        };
        assert_eq!(
            r("album.cue").after,
            "FILE \"01 One_ Alpha_Beta.flac\" WAVE\n  TRACK 01 AUDIO\n    INDEX 01 00:00:00\nFILE \"zzz.flac\" WAVE\n"
        );
        assert_eq!(r("album.cue").lines, 1);
        assert_eq!(
            r("list.m3u").after,
            "#EXTM3U\n./01 One_ Alpha_Beta.flac\n02 Two.flac\nother.flac\n"
        );
        assert_eq!(r("list.m3u").lines, 2);
        assert_eq!(
            r("list.pls").after,
            "[playlist]\nFile1=02 Two.flac\nNumberOfEntries=1\n"
        );
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert_eq!(a.count, 2);
        assert!(jazz.join("01 One_ Alpha_Beta.flac").is_file());
        assert!(jazz.join("02 Two.flac").is_file());
        assert!(!jazz.join("a.flac").exists() && !jazz.join("b.flac").exists());
        assert!(jazz.join("c.flac").is_file());
        assert_eq!(
            fs::read_to_string(jazz.join("list.pls")).unwrap(),
            "[playlist]\nFile1=02 Two.flac\nNumberOfEntries=1\n"
        );
        let batch = journal
            .list()
            .into_iter()
            .find(|b| b.kind == "rename")
            .unwrap();
        assert_eq!(batch.entries.len(), 5);
        assert_eq!(batch.entries[0].op, "rename");
        assert_eq!(batch.entries[0].path, "INTERNAL/Jazz/a.flac");
        assert_eq!(
            batch.entries[0].after,
            "INTERNAL/Jazz/01 One_ Alpha_Beta.flac"
        );
        assert_eq!(batch.entries[2].op, "write_text");
        assert_eq!(batch.note.en, "2 files renamed in INTERNAL/Jazz");
        // The same preview applied again: its sources are gone.
        let e = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap_err();
        assert_eq!(e.k, "core.write.source_gone");
        // The undo: the playlists and the sheet written back, the names given back.
        let u = undo(&music, &shares, &mounter, &journal, &pending, &batch.id).unwrap();
        assert_eq!(u.count, 5);
        assert!(jazz.join("a.flac").is_file() && jazz.join("b.flac").is_file());
        assert!(!jazz.join("02 Two.flac").exists());
        assert_eq!(
            fs::read_to_string(jazz.join("list.m3u")).unwrap(),
            "#EXTM3U\n./a.flac\nb.flac\nother.flac\n"
        );
        assert_eq!(fs::read_to_string(jazz.join("album.cue")).unwrap(), sheet);
        assert!(journal.load(&batch.id).unwrap().undone);
        // A change of case alone goes through a temporary name.
        fs::write(jazz.join("Same.flac"), b"x").unwrap();
        rename_in(&jazz, "Same.flac", "same.flac").unwrap();
        assert!(fs::read_dir(&jazz)
            .unwrap()
            .flatten()
            .any(|e| e.file_name() == "same.flac"));
        fs::remove_file(jazz.join("same.flac")).unwrap();
        // Tags from the names by the same pattern: a file that does not fit is said.
        let p = prepare(
            &music,
            &shares,
            &d,
            &pl,
            3,
            &by_pattern("tagedit", "{track} {title}"),
        )
        .unwrap();
        assert!(!p.changed);
        assert_eq!(p.refused.len(), 3);
        assert!(p
            .refused
            .iter()
            .all(|(_, why)| why.k == "core.names.no_fit"));
        fs::rename(jazz.join("b.flac"), jazz.join("07 Seven.flac")).unwrap();
        let p = prepare(
            &music,
            &shares,
            &d,
            &pl,
            4,
            &by_pattern("tagedit", "{track} {title}"),
        )
        .unwrap();
        assert!(p.changed);
        assert_eq!(p.refused.len(), 2);
        let seven = p.files.iter().find(|f| f.name == "07 Seven.flac").unwrap();
        assert_eq!(seven.changes.len(), 2, "{:?}", seven.changes);
        assert!(seven
            .changes
            .iter()
            .any(|c| c.key == "TITLE" && c.after == vec!["Seven".to_string()]));
        assert!(seven
            .changes
            .iter()
            .any(|c| c.key == "TRACKNUMBER" && c.after == vec!["07".to_string()]));
        // Written, then set back: the track written "07" reads back as the number, and
        // the undo takes it for what the batch wrote.
        apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        let written = tags::view(&tags::read(&jazz.join("07 Seven.flac"), "flac"));
        assert_eq!(written.title, vec!["Seven".to_string()]);
        let batch = journal
            .list()
            .into_iter()
            .find(|b| b.kind == "tags" && !b.undone)
            .unwrap();
        let u = undo(&music, &shares, &mounter, &journal, &pending, &batch.id).unwrap();
        assert_eq!(u.count, 1);
        let back = tags::view(&tags::read(&jazz.join("07 Seven.flac"), "flac"));
        assert_eq!(back.title, vec!["Two".to_string()]);
        // The writer keeps the number as a number: "2/9" set back reads as 2.
        assert!(back.track[0].starts_with('2'), "{:?}", back.track);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_is_renamed_under_its_parent_and_given_back() {
        let dir = temp("folder-rename");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let pl = dir.join("playlists");
        let leaf = music.join("INTERNAL/Peter Hollens/MR0XF1~N");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(
            leaf.join("01.flac"),
            real_flac(&[
                ("ALBUM", "Misty Mountains: Songs"),
                ("ALBUMARTIST", "Peter Hollens"),
                ("TITLE", "I See Fire"),
            ]),
        )
        .unwrap();
        let root = prepare(
            &music,
            &shares,
            &d,
            &pl,
            1,
            &Request {
                kind: "folder".into(),
                path: "INTERNAL".into(),
                lines: Vec::new(),
                repairs: None,
                encoding: None,
                album: None,
                album_artist: None,
                date: None,
                base: None,
                name: Some("Nope".into()),
                edits: None,
                pattern: None,
                moves: Vec::new(),
                duplicates: Vec::new(),
            },
        )
        .unwrap_err();
        assert_eq!(root.k, "core.folder.root");
        let p = prepare(
            &music,
            &shares,
            &d,
            &pl,
            2,
            &Request {
                kind: "folder".into(),
                path: "INTERNAL/Peter Hollens/MR0XF1~N".into(),
                lines: Vec::new(),
                repairs: None,
                encoding: None,
                album: None,
                album_artist: None,
                date: None,
                base: None,
                name: Some("Misty Mountains: Songs".into()),
                edits: None,
                pattern: None,
                moves: Vec::new(),
                duplicates: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(p.kind, "folder");
        assert_eq!(p.path, "INTERNAL/Peter Hollens");
        assert!(p.changed);
        assert_eq!(p.renames[0].name, "MR0XF1~N");
        assert_eq!(p.renames[0].to.as_deref(), Some("Misty Mountains_ Songs"));
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert_eq!(a.count, 1);
        assert!(music
            .join("INTERNAL/Peter Hollens/Misty Mountains_ Songs")
            .is_dir());
        assert!(!leaf.exists());
        let batch = journal
            .list()
            .into_iter()
            .find(|b| b.kind == "folder")
            .unwrap();
        undo(&music, &shares, &mounter, &journal, &pending, &batch.id).unwrap();
        assert!(leaf.is_dir());
        assert!(!music
            .join("INTERNAL/Peter Hollens/Misty Mountains_ Songs")
            .exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_moves_to_artist_album_under_the_grandparent_and_undo_clears_an_empty_artist() {
        let dir = temp("folder-layout");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let pl = dir.join("playlists");
        let leaf = music.join("INTERNAL/Misc/MR0XF1~N");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join("01.flac"), real_flac(&[("TITLE", "One")])).unwrap();
        let p = prepare(
            &music,
            &shares,
            &d,
            &pl,
            1,
            &Request {
                kind: "folder".into(),
                path: "INTERNAL/Misc/MR0XF1~N".into(),
                lines: Vec::new(),
                repairs: None,
                encoding: None,
                album: None,
                album_artist: None,
                date: None,
                base: None,
                name: Some("Peter Hollens/Misty Mountains: Songs".into()),
                edits: None,
                pattern: None,
                moves: Vec::new(),
                duplicates: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(p.path, "INTERNAL");
        assert_eq!(p.renames[0].name, "Misc/MR0XF1~N");
        assert_eq!(
            p.renames[0].to.as_deref(),
            Some("Peter Hollens/Misty Mountains_ Songs")
        );
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert_eq!(a.count, 1);
        assert!(music
            .join("INTERNAL/Peter Hollens/Misty Mountains_ Songs/01.flac")
            .is_file());
        assert!(!leaf.exists());
        let batch = journal
            .list()
            .into_iter()
            .find(|b| b.kind == "folder" && !b.undone)
            .unwrap();
        undo(&music, &shares, &mounter, &journal, &pending, &batch.id).unwrap();
        assert!(leaf.is_dir());
        assert!(!music.join("INTERNAL/Peter Hollens").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_layout_under_an_artist_that_already_holds_it_undoes() {
        // Hollens case: leaf already under Peter Hollens; layout only renames the
        // album segment. Same parent, batch path is the grandparent — undo must
        // still find the folder (not look for the album leaf under INTERNAL).
        let dir = temp("folder-layout-same-parent");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let pl = dir.join("playlists");
        let leaf = music.join("INTERNAL/Peter Hollens/MR0XF1~N");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join("01.flac"), real_flac(&[("TITLE", "One")])).unwrap();
        let p = prepare(
            &music,
            &shares,
            &d,
            &pl,
            1,
            &Request {
                kind: "folder".into(),
                path: "INTERNAL/Peter Hollens/MR0XF1~N".into(),
                lines: Vec::new(),
                repairs: None,
                encoding: None,
                album: None,
                album_artist: None,
                date: None,
                base: None,
                name: Some("Peter Hollens/Misty Mountains: Songs".into()),
                edits: None,
                pattern: None,
                moves: Vec::new(),
                duplicates: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(p.path, "INTERNAL");
        assert_eq!(p.renames[0].name, "Peter Hollens/MR0XF1~N");
        assert_eq!(
            p.renames[0].to.as_deref(),
            Some("Peter Hollens/Misty Mountains_ Songs")
        );
        apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert!(music
            .join("INTERNAL/Peter Hollens/Misty Mountains_ Songs/01.flac")
            .is_file());
        assert!(!leaf.exists());
        let batch = journal
            .list()
            .into_iter()
            .find(|b| b.kind == "folder" && !b.undone)
            .unwrap();
        undo(&music, &shares, &mounter, &journal, &pending, &batch.id).unwrap();
        assert!(leaf.is_dir());
        assert!(music.join("INTERNAL/Peter Hollens").is_dir());
        assert!(!music
            .join("INTERNAL/Peter Hollens/Misty Mountains_ Songs")
            .exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_tags_of_a_folder_are_written_file_by_file_journaled_and_set_back() {
        let dir = temp("tags");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        fs::write(music.join("INTERNAL/Jazz/a.flac"), faulty_flac()).unwrap();
        fs::write(
            music.join("INTERNAL/Jazz/b.flac"),
            real_flac(&[("ALBUM", "Two"), ("ARTIST", "A"), ("TITLE", "B")]),
        )
        .unwrap();
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            1,
            &folder_tags("INTERNAL/Jazz", None, Some("One")),
        )
        .unwrap();
        assert_eq!(p.kind, "tags");
        assert_eq!(p.op, "tags");
        assert_eq!(p.path, "INTERNAL/Jazz");
        assert_eq!(p.file, "INTERNAL/Jazz");
        assert!(p.changed);
        assert_eq!(p.files.len(), 2);
        assert_eq!(p.album.as_deref(), Some("One"));
        // Unify was not in the repairs: the value is echoed, not applied.
        assert!(p.repairs.iter().any(|r| r.id == "unify" && !r.on));
        assert!(p.files[1].changes.is_empty());
        let p = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            2,
            &folder_tags(
                "INTERNAL/Jazz",
                Some(&["albumartist", "date", "text", "twice", "unify"]),
                Some("One"),
            ),
        )
        .unwrap();
        assert_eq!(p.files[1].changes.len(), 1);
        assert_eq!(p.files[1].changes[0].key, "ALBUM");
        assert_eq!(p.files[1].changes[0].after, ["One"]);
        let values = p.values.as_ref().unwrap();
        assert_eq!(
            values.album,
            [("One".to_string(), 1), ("Two".to_string(), 1)]
        );
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert_eq!(a.count, 2);
        assert!(a.pending);
        assert_eq!(a.updated, None);
        assert_eq!(a.file, "INTERNAL/Jazz");
        let batch = journal.load(&a.batch).unwrap();
        assert_eq!(batch.kind, "tags");
        assert_eq!(batch.entries.len(), 2);
        assert_eq!(batch.entries[0].op, "tags");
        assert_eq!(
            batch.note,
            "tags of 2 files in INTERNAL/Jazz: albumartist, date, text, twice, unify"
        );
        let seen = tags::view(&tags::read(&music.join("INTERNAL/Jazz/b.flac"), "flac"));
        assert_eq!(seen.album, ["One"]);
        let seen = tags::view(&tags::read(&music.join("INTERNAL/Jazz/a.flac"), "flac"));
        assert_eq!(seen.album_artist, ["The Band"]);
        assert_eq!(seen.title, ["Caf\u{e9} Society"]);
        // The same preview again: the files moved on.
        assert!(apply(&music, &shares, &mounter, &journal, &pending, &p)
            .unwrap_err()
            .contains("changed since"));
        // Nothing left: the folder reads clean.
        let again = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            3,
            &folder_tags("INTERNAL/Jazz", None, None),
        )
        .unwrap();
        assert!(!again.changed, "{:?}", again.files);
        // One file has an album artist now and the other falls back to its artist,
        // so the folder still disagrees on that: unify stays on offer, nothing else.
        assert!(
            again.repairs.iter().all(|r| r.id == "unify" && !r.on),
            "{:?}",
            again.repairs
        );
        // A file changed since the batch stops the undo; set back, it goes through.
        fs::write(
            music.join("INTERNAL/Jazz/b.flac"),
            real_flac(&[("ALBUM", "Three"), ("ARTIST", "A"), ("TITLE", "B")]),
        )
        .unwrap();
        let err = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap_err();
        assert!(err.contains("b.flac: ALBUM changed since"), "{err}");
        fs::write(
            music.join("INTERNAL/Jazz/b.flac"),
            real_flac(&[("ALBUM", "One"), ("ARTIST", "A"), ("TITLE", "B")]),
        )
        .unwrap();
        let u = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert_eq!(u.count, 2);
        assert!(journal.load(&a.batch).unwrap().undone);
        let seen = tags::view(&tags::read(&music.join("INTERNAL/Jazz/b.flac"), "flac"));
        assert_eq!(seen.album, ["Two"]);
        let seen = tags::view(&tags::read(&music.join("INTERNAL/Jazz/a.flac"), "flac"));
        assert!(seen.album_artist.is_empty());
        assert_eq!(seen.title, ["Caf\u{c3}\u{a9} Society"]);
        assert_eq!(seen.artist, ["A", "B"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn organise_moves_selected_albums_and_undo_restores_the_dump() {
        let dir = temp("organise");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let pl = dir.join("playlists");
        let dump = music.join("INTERNAL/Phil Collins/Phil Collins");
        fs::create_dir_all(&dump).unwrap();
        fs::write(
            dump.join("h1.flac"),
            real_flac(&[
                ("ALBUM", "Hits"),
                ("ALBUMARTIST", "Phil Collins"),
                ("TITLE", "One"),
            ]),
        )
        .unwrap();
        fs::write(
            dump.join("s1.flac"),
            real_flac(&[
                ("ALBUM", "But Seriously"),
                ("ALBUMARTIST", "Phil Collins"),
                ("TITLE", "Two"),
            ]),
        )
        .unwrap();
        fs::write(
            dump.join("n1.flac"),
            real_flac(&[
                ("ALBUM", "No Jacket Required"),
                ("ALBUMARTIST", "Phil Collins"),
                ("TITLE", "Three"),
            ]),
        )
        .unwrap();
        let none = prepare(
            &music,
            &shares,
            &d,
            &pl,
            1,
            &Request {
                kind: "organise".into(),
                path: "INTERNAL/Phil Collins/Phil Collins".into(),
                lines: Vec::new(),
                repairs: None,
                encoding: None,
                album: None,
                album_artist: None,
                date: None,
                base: None,
                name: None,
                edits: None,
                pattern: None,
                moves: Vec::new(),
                duplicates: Vec::new(),
            },
        )
        .unwrap_err();
        assert_eq!(none.k, "core.organise.none_selected");
        let p = prepare(
            &music,
            &shares,
            &d,
            &pl,
            2,
            &Request {
                kind: "organise".into(),
                path: "INTERNAL/Phil Collins/Phil Collins".into(),
                lines: Vec::new(),
                repairs: None,
                encoding: None,
                album: None,
                album_artist: None,
                date: None,
                base: None,
                name: None,
                edits: None,
                pattern: None,
                moves: vec![
                    "INTERNAL/Phil Collins/Hits".into(),
                    "INTERNAL/Phil Collins/But Seriously".into(),
                ],
                duplicates: Vec::new(),
            },
        )
        .unwrap();
        assert!(p.changed);
        assert_eq!(p.renames.len(), 2);
        assert!(p
            .renames
            .iter()
            .any(|r| r.to.as_deref() == Some("INTERNAL/Phil Collins/Hits/h1.flac")));
        assert!(p
            .renames
            .iter()
            .any(|r| r.to.as_deref() == Some("INTERNAL/Phil Collins/But Seriously/s1.flac")));
        let a = apply(&music, &shares, &mounter, &journal, &pending, &p).unwrap();
        assert_eq!(a.count, 2);
        assert!(music.join("INTERNAL/Phil Collins/Hits/h1.flac").is_file());
        assert!(music
            .join("INTERNAL/Phil Collins/But Seriously/s1.flac")
            .is_file());
        assert!(dump.join("n1.flac").is_file());
        assert!(!dump.join("h1.flac").exists());
        let u = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert_eq!(u.count, 2);
        assert!(dump.join("h1.flac").is_file());
        assert!(dump.join("s1.flac").is_file());
        assert!(dump.join("n1.flac").is_file());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn organise_bit_identical_collision_offers_leave_replace_remove() {
        let dir = temp("organise-dup");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let _address = fake_mpd("OK MPD 0.24.6\n");
        let pl = dir.join("playlists");
        let dump = music.join("INTERNAL/Phil Collins/Phil Collins");
        let hits = music.join("INTERNAL/Phil Collins/Hits");
        fs::create_dir_all(&dump).unwrap();
        fs::create_dir_all(&hits).unwrap();
        let dump_bytes = real_flac(&[
            ("ALBUM", "Hits"),
            ("ALBUMARTIST", "Phil Collins"),
            ("TITLE", "One"),
        ]);
        let hits_bytes = real_flac(&[
            ("ALBUM", "Hits"),
            ("ALBUMARTIST", "Phil Collins"),
            ("TITLE", "One (Hits master)"),
        ]);
        fs::write(dump.join("h1.flac"), &dump_bytes).unwrap();
        fs::write(hits.join("h1.flac"), &dump_bytes).unwrap();
        let req = |dups: Vec<DuplicatePick>| Request {
            kind: "organise".into(),
            path: "INTERNAL/Phil Collins/Phil Collins".into(),
            lines: Vec::new(),
            repairs: None,
            encoding: None,
            album: None,
            album_artist: None,
            date: None,
            base: None,
            name: None,
            edits: None,
            pattern: None,
            moves: vec!["INTERNAL/Phil Collins/Hits".into()],
            duplicates: dups,
        };
        let leave = prepare(&music, &shares, &d, &pl, 1, &req(Vec::new())).unwrap();
        assert!(!leave.changed);
        assert!(leave.renames.is_empty());
        assert_eq!(leave.duplicates.len(), 1);
        assert_eq!(leave.duplicates[0].action, "leave");
        assert!(leave.refused.is_empty());
        let remove = prepare(
            &music,
            &shares,
            &d,
            &pl,
            2,
            &req(vec![DuplicatePick {
                name: "h1.flac".into(),
                to: "INTERNAL/Phil Collins/Hits/h1.flac".into(),
                action: "remove".into(),
            }]),
        )
        .unwrap();
        assert!(remove.changed);
        assert_eq!(remove.renames.len(), 1);
        assert!(remove.renames[0].to.is_none());
        let a = apply(&music, &shares, &mounter, &journal, &pending, &remove).unwrap();
        assert_eq!(a.count, 1);
        assert!(!dump.join("h1.flac").exists());
        assert!(hits.join("h1.flac").is_file());
        let u = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert_eq!(u.count, 1);
        assert!(dump.join("h1.flac").is_file());
        assert!(hits.join("h1.flac").is_file());
        // Replace: destination bytes are journaled before the rename; undo puts
        // Hits/ back (and the dump file) even when the copies were identical.
        fs::write(dump.join("h1.flac"), &dump_bytes).unwrap();
        fs::write(hits.join("h1.flac"), &hits_bytes).unwrap();
        // Collision offers only for bit-identical; force matching bytes for prepare.
        fs::write(hits.join("h1.flac"), &dump_bytes).unwrap();
        let replace = prepare(
            &music,
            &shares,
            &d,
            &pl,
            3,
            &req(vec![DuplicatePick {
                name: "h1.flac".into(),
                to: "INTERNAL/Phil Collins/Hits/h1.flac".into(),
                action: "replace".into(),
            }]),
        )
        .unwrap();
        assert!(replace.changed);
        assert_eq!(
            replace.renames[0].to.as_deref(),
            Some("INTERNAL/Phil Collins/Hits/h1.flac")
        );
        let a = apply(&music, &shares, &mounter, &journal, &pending, &replace).unwrap();
        assert!(a.count >= 1);
        assert!(!dump.join("h1.flac").exists());
        assert_eq!(fs::read(hits.join("h1.flac")).unwrap(), dump_bytes);
        let batch = journal
            .list()
            .into_iter()
            .find(|b| b.id == a.batch)
            .expect("batch");
        assert!(
            batch.entries.iter().any(|e| {
                e.op == "remove"
                    && e.path == "INTERNAL/Phil Collins/Hits/h1.flac"
                    && e.before_bytes.is_some()
            }),
            "{:?}",
            batch.entries
        );
        let u = undo(&music, &shares, &mounter, &journal, &pending, &a.batch).unwrap();
        assert!(u.count >= 1);
        assert_eq!(fs::read(dump.join("h1.flac")).unwrap(), dump_bytes);
        assert_eq!(fs::read(hits.join("h1.flac")).unwrap(), dump_bytes);
        let _ = hits_bytes;
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn deferred_writes_flush_once_across_folders() {
        let dir = temp("flush");
        let music = dir.join("music");
        fs::create_dir_all(music.join("INTERNAL/A")).unwrap();
        fs::create_dir_all(music.join("INTERNAL/B")).unwrap();
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let d = decoders();
        let mounter = local_mounter(&dir);
        let journal = Journal::new(&dir.join("undo"));
        let pending = Pending::open(&dir);
        let address = fake_mpd("OK MPD 0.24.6\n");
        let p1 = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            1,
            &request("INTERNAL/A", &["*.iso"]),
        )
        .unwrap();
        let p2 = prepare(
            &music,
            &shares,
            &d,
            &dir.join("playlists"),
            2,
            &request("INTERNAL/B", &["*.iso"]),
        )
        .unwrap();
        let a1 = apply(&music, &shares, &mounter, &journal, &pending, &p1).unwrap();
        let a2 = apply(&music, &shares, &mounter, &journal, &pending, &p2).unwrap();
        assert!(a1.pending && a2.pending);
        assert_eq!(
            pending.list(),
            vec!["INTERNAL/A".to_string(), "INTERNAL/B".to_string()]
        );
        assert_eq!(crate::pending::flush_path_of(&pending.list()), "INTERNAL");
        let f = flush(&pending, &address).unwrap();
        assert_eq!(f.updated, Some(7));
        assert_eq!(f.path, "INTERNAL");
        assert!(pending.list().is_empty());
        assert_eq!(
            flush(&pending, &address).unwrap_err().k,
            "core.library.nothing_pending"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
