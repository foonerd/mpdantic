//! A batch of writes: prepared without writing, shown whole, confirmed once, applied
//! with a check that each file is what the preview read, journaled for the undo, and
//! followed by MPD's update of the folder. Three kinds: `mpdignore`, lines into the
//! `.mpdignore` of the folder in hand; `cue`, a sheet's text repaired or the sheet
//! removed; `tags`, the tags of the folder's audio files edited key by key.

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
use crate::playlist;
use crate::plcheck;
use crate::plfix::{self, EntryChange};
use crate::tagfix::{self, FilePlan, KeyChange, Values};
use crate::tags;
use crate::undo::{Batch, Entry, Journal};

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
    /// `local`, or the share and the folder that would be mounted.
    pub place: String,
    /// The file as read: its size and modification time, 0 when it is not there.
    pub size: u64,
    pub mtime: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub already: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repairs: Vec<Repair>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub encodings: Vec<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remaining: Vec<Finding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
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
}

/// What an apply or an undo did.
#[derive(Serialize, Debug)]
pub struct Applied {
    pub batch: String,
    pub file: String,
    pub written: bool,
    /// The files written (`tags`); one otherwise.
    pub count: usize,
    /// MPD's update job for the folder, or why there is none.
    pub updated: Option<u64>,
    pub update_error: Option<String>,
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

fn place_text(shares: &[Share], folder: &str) -> Result<String, String> {
    Ok(match mount::place(shares, folder)? {
        Where::Local => "local".to_string(),
        Where::Nas { share, folder } => format!(
            "the share {}, folder {}",
            share.name,
            if folder.is_empty() { "/" } else { &folder }
        ),
    })
}

/// Bytes as the page shows them: UTF-8 as it is, anything else read as Latin-1.
fn display(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

fn empty(id: u64, kind: &str, path: String, file: String, op: &str, place: String) -> Preview {
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
        place,
        size: 0,
        mtime: 0,
        added: Vec::new(),
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
    }
}

/// The batch prepared, nothing written.
pub fn prepare(
    music: &Path,
    shares: &[Share],
    decoders: &Decoders,
    id: u64,
    req: &Request,
) -> Result<Preview, String> {
    match req.kind.as_str() {
        "mpdignore" => prepare_lines(music, shares, id, req),
        "cue" => prepare_cue(music, shares, decoders, id, req),
        "tags" => prepare_tags(music, shares, decoders, id, req),
        "playlist" => prepare_playlist(music, shares, decoders, id, req),
        _ => Err("the kind is not one the core writes".to_string()),
    }
}

/// `.mpdignore` lines into the folder in hand, against the file as it is.
fn prepare_lines(
    music: &Path,
    shares: &[Share],
    id: u64,
    req: &Request,
) -> Result<Preview, String> {
    let path = Library::check(&req.path).ok_or_else(|| "not a library path".to_string())?;
    let dir = folder_dir(music, &path);
    if !dir.is_dir() {
        return Err("no such folder".to_string());
    }
    let place = place_text(shares, &path)?;
    let file_path = dir.join(".mpdignore");
    let (exists, size, mtime) = stat(&file_path);
    let before = if exists {
        let bytes = fs::read(&file_path).map_err(|e| format!("{}: {e}", file_path.display()))?;
        String::from_utf8(bytes).map_err(|_| "the file is not UTF-8; not touched".to_string())?
    } else {
        String::new()
    };
    let present: Vec<String> = before
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let mut added: Vec<String> = Vec::new();
    let mut already = Vec::new();
    let mut refused = Vec::new();
    for raw in &req.lines {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            refused.push((
                line.to_string(),
                format!(
                    "a line starting with # is a comment to MPD; for a name starting with a hash write [#]{rest}"
                ),
            ));
            continue;
        }
        if line.contains('\n') || line.contains('\r') {
            refused.push((line.to_string(), "one line at a time".to_string()));
            continue;
        }
        if Pattern::new(line).is_none() {
            refused.push((line.to_string(), "not a pattern".to_string()));
            continue;
        }
        if present.iter().any(|p| p == line) || added.iter().any(|a| a == line) {
            already.push(line.to_string());
            continue;
        }
        added.push(line.to_string());
    }
    let mut after = before.clone();
    if !after.is_empty() && !after.ends_with('\n') {
        after.push('\n');
    }
    for line in &added {
        after.push_str(line);
        after.push('\n');
    }
    let file = if path.is_empty() {
        ".mpdignore".to_string()
    } else {
        format!("{path}/.mpdignore")
    };
    let mut p = empty(id, "mpdignore", path, file, "write_text", place);
    p.exists = exists;
    p.before = before;
    p.after = after;
    p.changed = !added.is_empty();
    p.size = size;
    p.mtime = mtime;
    p.added = added;
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
) -> Result<Preview, String> {
    let path = Library::check(&req.path).ok_or_else(|| "not a library path".to_string())?;
    if classify::suffix(&path).as_deref() != Some(classify::CUE_SUFFIX) {
        return Err("not a CUE sheet".to_string());
    }
    let full = music.join(&path);
    if !full.is_file() {
        return Err("no such sheet".to_string());
    }
    let (folder, name) = match path.rsplit_once('/') {
        Some((f, n)) => (f.to_string(), n.to_string()),
        None => (String::new(), path.clone()),
    };
    let dir = folder_dir(music, &folder);
    let place = place_text(shares, &folder)?;
    let (exists, size, mtime) = stat(&full);
    if size > MAX_SHEET {
        return Err(format!("the sheet is {size} bytes; not read"));
    }
    let bytes = fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
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
) -> Result<Preview, String> {
    let path = Library::check(&req.path).ok_or_else(|| "not a library path".to_string())?;
    let dir = folder_dir(music, &path);
    if !dir.is_dir() {
        return Err("no such folder".to_string());
    }
    let place = place_text(shares, &path)?;
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

/// A playlist's repair: the entries MPD cannot find looked for under the base
/// folder and written relative to the playlist's folder, the mark, the encoding, a
/// PLS's count, a `.m3u` beside an m3u8 or an xspf.
fn prepare_playlist(
    music: &Path,
    shares: &[Share],
    decoders: &Decoders,
    id: u64,
    req: &Request,
) -> Result<Preview, String> {
    let path = Library::check(&req.path).ok_or_else(|| "not a library path".to_string())?;
    let suffix = classify::suffix(&path).unwrap_or_default();
    if !playlist::SUFFIXES.contains(&suffix.as_str()) {
        return Err("not a playlist MPDantic reads".to_string());
    }
    let full = music.join(&path);
    if !full.is_file() {
        return Err("no such playlist".to_string());
    }
    let folder = path
        .rsplit_once('/')
        .map(|(f, _)| f.to_string())
        .unwrap_or_default();
    let place = place_text(shares, &folder)?;
    let (_, size, _) = stat(&full);
    if size > plcheck::MAX_PLAYLIST {
        return Err(format!("the playlist is {size} bytes; not read"));
    }
    let bytes = fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
    let base = match &req.base {
        Some(b) => Library::check(b).ok_or_else(|| "the base is not a library path".to_string())?,
        None => folder.clone(),
    };
    if !folder_dir(music, &base).is_dir() {
        return Err("no such base folder".to_string());
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
        fs::read(&target_full).map_err(|e| format!("{}: {e}", target_full.display()))?
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

/// Where the folder is written: the folder itself for a local root, the mount's
/// target for a share, with the mount to take down after.
fn writable(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    path: &str,
) -> Result<(PathBuf, Option<Mount>), String> {
    match mount::place(shares, path)? {
        Where::Local => {
            let dir = folder_dir(music, path);
            mount::write_test(&dir).map_err(|e| {
                format!(
                    "{}: cannot write there: {e}",
                    if path.is_empty() {
                        "the library root"
                    } else {
                        path
                    }
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

/// A file written whole and moved into place.
fn write_whole(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let part = dir.join(format!("{name}.part"));
    let file = dir.join(name);
    fs::write(&part, bytes).map_err(|e| format!("{}: {e}", part.display()))?;
    fs::rename(&part, &file).map_err(|e| {
        let _ = fs::remove_file(&part);
        format!("{}: {e}", file.display())
    })
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn files_word(n: usize) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{n} files")
    }
}

/// What the batch did, in a line.
fn note_for(preview: &Preview, written: usize) -> String {
    let on = || {
        preview
            .repairs
            .iter()
            .filter(|r| r.on)
            .map(|r| r.id)
            .collect::<Vec<_>>()
            .join(", ")
    };
    match preview.kind.as_str() {
        "cue" if preview.op == "remove" => format!("sheet {} removed", preview.file),
        "cue" => format!("sheet {} repaired: {}", preview.file, on()),
        "playlist" => format!("playlist {} written: {}", preview.file, on()),
        "tags" => format!(
            "tags of {} in {}: {}",
            files_word(written),
            if preview.path.is_empty() {
                "the library root"
            } else {
                &preview.path
            },
            on()
        ),
        _ => format!(
            "{} {} into {}",
            preview.added.len(),
            if preview.added.len() == 1 {
                "line"
            } else {
                "lines"
            },
            preview.file
        ),
    }
}

fn update_after(mpd_address: &mpd::Address, folder: &str) -> (Option<u64>, Option<String>) {
    match mpd::update(mpd_address, folder) {
        Ok(n) => (Some(n), None),
        Err(e) => (None, Some(e)),
    }
}

/// The preview applied: the file checked to be what the preview read, the folder
/// made writable, the file written or removed, the batch journaled with the file's
/// bytes as they were, MPD told.
pub fn apply(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    mpd_address: &mpd::Address,
    preview: &Preview,
) -> Result<Applied, String> {
    if !preview.changed {
        return Err("nothing to write".to_string());
    }
    if preview.kind == "tags" {
        return apply_tags(music, shares, mounter, journal, mpd_address, preview);
    }
    let ro = music.join(&preview.file);
    let (exists, size, mtime) = stat(&ro);
    if exists != preview.exists || size != preview.size || mtime != preview.mtime {
        return Err("the file changed since the preview; preview it again".to_string());
    }
    let before_bytes = if exists {
        Some(fs::read(&ro).map_err(|e| format!("{}: {e}", preview.file))?)
    } else {
        None
    };
    let name = file_name(&preview.file);
    let (dir, mount) = writable(music, shares, mounter, &preview.path)?;
    let result = if preview.op == "remove" {
        fs::remove_file(dir.join(name)).map_err(|e| format!("{}: {e}", preview.file))
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
    let (updated, update_error) = update_after(mpd_address, &preview.path);
    Ok(Applied {
        batch,
        file: preview.file.clone(),
        written: true,
        count: 1,
        updated,
        update_error,
    })
}

/// The folder's files written one by one through lofty: each checked to be what the
/// preview read; a file that fails stops the batch there, with the files before it
/// written and journaled.
fn apply_tags(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    mpd_address: &mpd::Address,
    preview: &Preview,
) -> Result<Applied, String> {
    let todo: Vec<&FilePlan> = preview
        .files
        .iter()
        .filter(|f| !f.changes.is_empty())
        .collect();
    for fp in &todo {
        let (exists, size, mtime) = stat(&music.join(&fp.path));
        if !exists || size != fp.size || mtime != fp.mtime {
            return Err(format!(
                "{} changed since the preview; preview it again",
                fp.path
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
                failure = Some(format!("{}: {e}", fp.path));
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
            Some(id) => format!(
                "{e}; the {} before it written and journaled as batch {id}",
                files_word(written)
            ),
            None => e,
        });
    }
    let (updated, update_error) = update_after(mpd_address, &preview.path);
    Ok(Applied {
        batch: batch.unwrap_or_default(),
        file: preview.path.clone(),
        written: true,
        count: written,
        updated,
        update_error,
    })
}

/// A batch undone: each file checked to be what the batch left, the before written
/// back byte for byte or the file removed (or the tags set back key by key), the
/// batch marked, MPD told.
pub fn undo(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    mpd_address: &mpd::Address,
    id: &str,
) -> Result<Applied, String> {
    let batch = journal
        .load(id)
        .ok_or_else(|| "no such batch".to_string())?;
    if batch.undone {
        return Err("already undone".to_string());
    }
    if batch.kind == "tags" {
        return undo_tags(music, shares, mounter, journal, mpd_address, &batch);
    }
    let Some(entry) = batch.entries.first() else {
        return Err("an empty batch".to_string());
    };
    let current = fs::read(music.join(&entry.path)).ok();
    if entry.op == "remove" {
        if current.is_some() {
            return Err(
                "a file is there again where the batch removed one; not undone".to_string(),
            );
        }
    } else if current.as_deref() != Some(entry.after.as_bytes()) {
        return Err("the file changed since the batch; not undone".to_string());
    }
    let before: Option<Vec<u8>> = match &entry.before_bytes {
        Some(name) => Some(journal.kept(&batch.id, name)?),
        None => entry.before.as_ref().map(|s| s.as_bytes().to_vec()),
    };
    let (dir, mount) = writable(music, shares, mounter, &batch.path)?;
    let name = file_name(&entry.path);
    let result = match before {
        Some(bytes) => write_whole(&dir, name, &bytes),
        None => fs::remove_file(dir.join(name)).map_err(|e| format!("{}: {e}", entry.path)),
    };
    if let Some(mut m) = mount {
        mounter.unmount(&mut m);
    }
    result?;
    journal.mark_undone(id)?;
    let (updated, update_error) = update_after(mpd_address, &batch.path);
    Ok(Applied {
        batch: id.to_string(),
        file: entry.path.clone(),
        written: true,
        count: 1,
        updated,
        update_error,
    })
}

/// The tags set back: each key checked to hold what the batch wrote (or what it
/// held before, so an undo cut short runs again), then the change the other way.
fn undo_tags(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    mpd_address: &mpd::Address,
    batch: &Batch,
) -> Result<Applied, String> {
    let mut todo: Vec<(String, String, Vec<KeyChange>)> = Vec::new();
    for entry in &batch.entries {
        let changes: Vec<KeyChange> =
            serde_json::from_str(&entry.after).map_err(|e| format!("{}: {e}", entry.path))?;
        let suffix = classify::suffix(&entry.path).unwrap_or_default();
        let format = tags::format_of(&suffix).unwrap_or("").to_string();
        let raw = tags::read(&music.join(&entry.path), &suffix);
        if let Some(p) = &raw.problem {
            return Err(format!("{}: {p}; not undone", entry.path));
        }
        let mut reverse = Vec::new();
        for c in changes {
            let current = tagfix::current_values(&raw, &c.kind, &c.key);
            if current == c.after {
                reverse.push(KeyChange {
                    kind: c.kind,
                    key: c.key,
                    before: c.after,
                    after: c.before,
                });
            } else if current != c.before {
                return Err(format!(
                    "{}: {} changed since the batch; not undone",
                    entry.path, c.key
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
                failure = Some(format!("{path}: {e}"));
                break;
            }
        }
    }
    if let Some(mut m) = mount {
        mounter.unmount(&mut m);
    }
    if let Some(e) = failure {
        return Err(format!(
            "{e}; {} put back before it, the undo can be run again",
            files_word(put_back)
        ));
    }
    journal.mark_undone(&batch.id)?;
    let (updated, update_error) = update_after(mpd_address, &batch.path);
    Ok(Applied {
        batch: batch.id.clone(),
        file: batch.path.clone(),
        written: true,
        count: put_back,
        updated,
        update_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::tests::sample_config;
    use crate::mpd::tests::{decoders_of, fake_mpd};
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
        assert_eq!(p.already, ["*.iso", "*.mov"]);
        assert_eq!(p.refused.len(), 2);
        assert!(p.refused[0].1.contains("[#]recycle"));
        assert_eq!(p.after, "*.iso\n#recycle\n*.mov\n[#]recycle\n");
        assert_eq!(p.place, "local");
        assert!(p.changed);
        assert!(p.size > 0);
        let p = prepare(
            &music,
            &shares,
            &d,
            2,
            &request("NAS/multimedia/Dune", &["*.srt"]),
        )
        .unwrap();
        assert!(!p.exists);
        assert_eq!(p.after, "*.srt\n");
        assert_eq!(p.place, "the share multimedia, folder Dune");
        assert!(prepare(&music, &shares, &d, 3, &request("../x", &["a"])).is_err());
        assert!(prepare(&music, &shares, &d, 3, &request("INTERNAL/Rock", &["a"])).is_err());
        let mut odd = request("INTERNAL", &[]);
        odd.kind = "nothing".into();
        assert!(prepare(&music, &shares, &d, 3, &odd).is_err());
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
        let address = fake_mpd("OK MPD 0.24.6\n");
        let p = prepare(
            &music,
            &shares,
            &d,
            1,
            &request("INTERNAL/Jazz", &["*.iso", "[#]recycle"]),
        )
        .unwrap();
        let a = apply(&music, &shares, &mounter, &journal, &address, &p).unwrap();
        assert!(a.written);
        assert_eq!(a.updated, Some(7));
        assert_eq!(
            fs::read_to_string(music.join("INTERNAL/Jazz/.mpdignore")).unwrap(),
            "*.iso\n[#]recycle\n"
        );
        assert!(!music.join("INTERNAL/Jazz/.mpdignore.part").exists());
        let list = journal.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, a.batch);
        assert_eq!(list[0].note, "2 lines into INTERNAL/Jazz/.mpdignore");
        assert_eq!(list[0].entries[0].before, None);
        assert_eq!(list[0].entries[0].before_bytes, None);
        // The same preview again: the file moved on.
        assert!(apply(&music, &shares, &mounter, &journal, &address, &p)
            .unwrap_err()
            .contains("changed since"));
        // A second batch on top, then both undone in order.
        let p2 = prepare(
            &music,
            &shares,
            &d,
            2,
            &request("INTERNAL/Jazz", &["*.mov"]),
        )
        .unwrap();
        let a2 = apply(&music, &shares, &mounter, &journal, &address, &p2).unwrap();
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
            undo(&music, &shares, &mounter, &journal, &address, &a.batch)
                .unwrap_err()
                .contains("changed since")
        );
        let u2 = undo(&music, &shares, &mounter, &journal, &address, &a2.batch).unwrap();
        assert_eq!(u2.updated, Some(7));
        assert_eq!(
            fs::read_to_string(music.join("INTERNAL/Jazz/.mpdignore")).unwrap(),
            "*.iso\n[#]recycle\n"
        );
        let u1 = undo(&music, &shares, &mounter, &journal, &address, &a.batch).unwrap();
        assert!(u1.written);
        assert!(!music.join("INTERNAL/Jazz/.mpdignore").exists());
        assert!(journal.load(&a.batch).unwrap().undone);
        assert!(
            undo(&music, &shares, &mounter, &journal, &address, &a.batch)
                .unwrap_err()
                .contains("already")
        );
        let empty = prepare(&music, &shares, &d, 3, &request("INTERNAL/Jazz", &[""])).unwrap();
        assert!(apply(&music, &shares, &mounter, &journal, &address, &empty)
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
        let address = fake_mpd("OK MPD 0.24.6\n");
        let p = prepare(
            &music,
            &shares,
            &d,
            1,
            &request("NAS/multimedia/Dune", &["*.srt", "*.txt"]),
        )
        .unwrap();
        let a = apply(&music, &shares, &mounter, &journal, &address, &p).unwrap();
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
        let u = undo(&music, &shares, &mounter, &journal, &address, &a.batch).unwrap();
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
        let address = fake_mpd("OK MPD 0.24.6\n");
        let original: &[u8] = b"\xEF\xBB\xBFTITLE \"Caf\xe9\"\r\nFILE \"album.flac\" BINARY\r\nTRACK 01 AUDIO\r\nINDEX 01 00:00:00\r\n";
        fs::write(music.join("INTERNAL/Jazz/album.flac"), b"x").unwrap();
        fs::write(music.join("INTERNAL/Jazz/album.cue"), original).unwrap();
        let p = prepare(
            &music,
            &shares,
            &d,
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
        let a = apply(&music, &shares, &mounter, &journal, &address, &p).unwrap();
        assert_eq!(a.updated, Some(7));
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
            2,
            &sheet("INTERNAL/Jazz/album.cue", None),
        )
        .unwrap();
        assert!(!again.changed);
        assert!(again.repairs.is_empty());
        assert!(apply(&music, &shares, &mounter, &journal, &address, &again)
            .unwrap_err()
            .contains("nothing"));
        let u = undo(&music, &shares, &mounter, &journal, &address, &a.batch).unwrap();
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
            3,
            &sheet("INTERNAL/Jazz/album.flac", None)
        )
        .is_err());
        assert!(prepare(
            &music,
            &shares,
            &d,
            3,
            &sheet("INTERNAL/Jazz/gone.cue", None)
        )
        .is_err());
        assert!(prepare(&music, &shares, &d, 3, &sheet("../album.cue", None)).is_err());
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
        let address = fake_mpd("OK MPD 0.24.6\n");
        let text = b"FILE \"image.flac\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n";
        fs::write(music.join("INTERNAL/Jazz/image.flac"), b"x").unwrap();
        fs::write(music.join("INTERNAL/Jazz/a.cue"), text).unwrap();
        fs::write(music.join("INTERNAL/Jazz/b.cue"), text).unwrap();
        let p = prepare(&music, &shares, &d, 1, &sheet("INTERNAL/Jazz/b.cue", None)).unwrap();
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
            2,
            &sheet("INTERNAL/Jazz/b.cue", Some(&["remove"])),
        )
        .unwrap();
        assert_eq!(p.op, "remove");
        assert!(p.changed);
        assert!(p.after.is_empty());
        let a = apply(&music, &shares, &mounter, &journal, &address, &p).unwrap();
        assert!(!music.join("INTERNAL/Jazz/b.cue").exists());
        assert_eq!(
            journal.load(&a.batch).unwrap().note,
            "sheet INTERNAL/Jazz/b.cue removed"
        );
        assert_eq!(journal.load(&a.batch).unwrap().entries[0].op, "remove");
        // Something else put a file there: the undo does not write over it.
        fs::write(music.join("INTERNAL/Jazz/b.cue"), b"new").unwrap();
        assert!(
            undo(&music, &shares, &mounter, &journal, &address, &a.batch)
                .unwrap_err()
                .contains("there again")
        );
        fs::remove_file(music.join("INTERNAL/Jazz/b.cue")).unwrap();
        let u = undo(&music, &shares, &mounter, &journal, &address, &a.batch).unwrap();
        assert!(u.written);
        assert_eq!(fs::read(music.join("INTERNAL/Jazz/b.cue")).unwrap(), text);
        assert!(journal.load(&a.batch).unwrap().undone);
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
        let address = fake_mpd("OK MPD 0.24.6\n");
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
        let a = apply(&music, &shares, &mounter, &journal, &address, &p).unwrap();
        assert_eq!(a.count, 2);
        assert_eq!(a.updated, Some(7));
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
        assert!(apply(&music, &shares, &mounter, &journal, &address, &p)
            .unwrap_err()
            .contains("changed since"));
        // Nothing left: the folder reads clean.
        let again = prepare(
            &music,
            &shares,
            &d,
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
        let err = undo(&music, &shares, &mounter, &journal, &address, &a.batch).unwrap_err();
        assert!(err.contains("b.flac: ALBUM changed since"), "{err}");
        fs::write(
            music.join("INTERNAL/Jazz/b.flac"),
            real_flac(&[("ALBUM", "One"), ("ARTIST", "A"), ("TITLE", "B")]),
        )
        .unwrap();
        let u = undo(&music, &shares, &mounter, &journal, &address, &a.batch).unwrap();
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
}
