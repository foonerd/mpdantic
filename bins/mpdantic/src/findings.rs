//! The findings of the last check of each kind, kept whole under the data folder:
//! written as the walk goes, so the memory does not grow with the library; read by
//! folder, since every repair is a folder's; patched when a folder is checked again
//! after a write; marked with the dismissals the owner made, which outlive the
//! checks; and written out as text in a language.
//!
//! A kind has three files under `findings/`: `<kind>.jsonl`, a row per line, made
//! whole by a rename when the check ends; `<kind>.json`, what the check said of
//! itself; `<kind>.updates.jsonl`, a record per folder checked again, appended, the
//! latest record for a folder replacing the base's rows for it when the store is
//! read, folded into the base when there are more records than folders, so a session
//! of repairs does not rewrite the whole list on the player's flash at every one.
//! `dismissed.json` beside them holds the dismissals of every kind.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::text::{self, Msg};

/// The kinds of check whose findings are kept.
pub const KINDS: [&str; 3] = ["cue", "tags", "playlists"];

/// Folders listed per page unless asked otherwise, and the most asked for.
pub const FOLDERS_DEFAULT: usize = 50;
pub const FOLDERS_MAX: usize = 500;
/// Findings listed per page in the flat file table (one code chip on).
pub const ROWS_DEFAULT: usize = 20;
pub const ROWS_MAX: usize = 500;

/// One finding as the store keeps it: the code, the folder it is repaired in, the
/// file or sheet or playlist it is about (the folder itself for a code raised per
/// folder), the line for a sheet's or a playlist's, and the sentence.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Row {
    pub code: String,
    pub folder: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub line: usize,
    pub text: Msg,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A row as the page reads it: the row and whether the owner dismissed it.
#[derive(Clone, Debug, Serialize)]
pub struct RowOut {
    #[serde(flatten)]
    pub row: Row,
    pub dismissed: bool,
}

/// Where a check tells each finding as it goes.
pub trait Sink {
    fn found(&mut self, code: &str, folder: &str, path: &str, line: usize, text: &Msg);
}

/// Collects the rows in memory: one folder checked again, and the tests.
#[derive(Default)]
pub struct Collector {
    pub rows: Vec<Row>,
}

impl Sink for Collector {
    fn found(&mut self, code: &str, folder: &str, path: &str, line: usize, text: &Msg) {
        self.rows.push(Row {
            code: code.to_string(),
            folder: folder.to_string(),
            path: path.to_string(),
            line,
            text: text.clone(),
        });
    }
}

/// What a kind's store says of itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub kind: String,
    /// The folder checked, empty for the library.
    pub path: String,
    pub when: u64,
    pub cancelled: bool,
    pub directories: usize,
    pub files: usize,
    /// What the check counted: the sheets, the playlists, the audio files read.
    pub counted: usize,
    pub rows: usize,
    pub folders: usize,
    pub by_code: BTreeMap<String, usize>,
    /// What each code means, in English, for a page without the key.
    pub codes: BTreeMap<String, String>,
}

/// What a check says of itself when it is done, for the meta.
pub struct Summary {
    pub path: String,
    pub cancelled: bool,
    pub directories: usize,
    pub files: usize,
    pub counted: usize,
    pub codes: BTreeMap<String, String>,
}

/// A kind's store read: the meta kept current, and the rows with the overlay
/// applied, in order of folder, path, line and code.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub meta: Meta,
    pub rows: Vec<Row>,
    /// Records in the overlay file, for the folding.
    updates: usize,
}

/// A record of the overlay: a folder's rows as they are after a check of it.
#[derive(Serialize, Deserialize)]
struct Update {
    folder: String,
    when: u64,
    rows: Vec<Row>,
}

/// A dismissal: this code on this path is not a finding, said the owner.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dismissal {
    pub kind: String,
    pub code: String,
    pub path: String,
    pub when: u64,
}

/// What the page asks of the list: the codes (none for all) and whether the
/// dismissed rows are wanted too.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub codes: Vec<String>,
    pub dismissed: bool,
}

impl Filter {
    fn takes(&self, row: &Row, dismissed: bool) -> bool {
        (self.codes.is_empty() || self.codes.iter().any(|c| c == &row.code))
            && (self.dismissed || !dismissed)
    }
}

/// A kind's store as the page sees it whole.
#[derive(Clone, Debug, Serialize)]
pub struct Overview {
    #[serde(flatten)]
    pub meta: Meta,
    pub open: usize,
    pub dismissed: usize,
    /// Folders with an open finding.
    pub open_folders: usize,
    /// Per code, the open and the dismissed.
    pub by_code_open: BTreeMap<String, usize>,
    pub by_code_dismissed: BTreeMap<String, usize>,
}

/// One folder of the list.
#[derive(Clone, Debug, Serialize)]
pub struct FolderSummary {
    pub folder: String,
    pub open: usize,
    pub dismissed: usize,
    /// The open rows per code.
    pub codes: BTreeMap<String, usize>,
}

/// A page of folders.
#[derive(Clone, Debug, Serialize)]
pub struct FoldersPage {
    pub total: usize,
    pub offset: usize,
    pub folders: Vec<FolderSummary>,
}

/// A page of findings across folders, for the flat table under a code chip.
#[derive(Clone, Debug, Serialize)]
pub struct RowsPage {
    pub total: usize,
    pub offset: usize,
    pub rows: Vec<RowOut>,
}

/// The store: the folder under the data folder, and what is loaded of it. Cloned
/// into a job, it is the same store.
#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
    loaded: Arc<Mutex<HashMap<String, Loaded>>>,
}

/// Writes a kind's store as the walk goes, to a temporary file that `finish` makes
/// the store; dropped unfinished, the temporary file goes and the store is as it was.
pub struct Writer {
    store: Store,
    kind: String,
    temp: PathBuf,
    out: Option<BufWriter<File>>,
    rows: usize,
    folders: BTreeSet<String>,
    by_code: BTreeMap<String, usize>,
    error: Option<Msg>,
}

impl Sink for Writer {
    fn found(&mut self, code: &str, folder: &str, path: &str, line: usize, text: &Msg) {
        if self.error.is_some() {
            return;
        }
        let row = Row {
            code: code.to_string(),
            folder: folder.to_string(),
            path: path.to_string(),
            line,
            text: text.clone(),
        };
        let written = serde_json::to_string(&row)
            .map_err(|e| Msg::raw(e.to_string()))
            .and_then(|json| match self.out.as_mut() {
                Some(out) => out
                    .write_all(json.as_bytes())
                    .and_then(|_| out.write_all(b"\n"))
                    .map_err(|e| text::at(self.temp.display(), e)),
                None => Ok(()),
            });
        match written {
            Ok(()) => {
                self.rows += 1;
                if !self.folders.contains(folder) {
                    self.folders.insert(folder.to_string());
                }
                *self.by_code.entry(code.to_string()).or_insert(0) += 1;
            }
            Err(e) => self.error = Some(e),
        }
    }
}

impl Writer {
    /// The store made whole: the rows flushed and renamed into place, the meta
    /// written beside them, the overlay of the store before gone, what was loaded
    /// of it dropped.
    pub fn finish(mut self, summary: Summary) -> Result<Meta, Msg> {
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        if let Some(mut out) = self.out.take() {
            out.flush().map_err(|e| text::at(self.temp.display(), e))?;
        }
        let base = self.store.base_file(&self.kind);
        fs::rename(&self.temp, &base).map_err(|e| text::at(base.display(), e))?;
        let _ = fs::remove_file(self.store.updates_file(&self.kind));
        // Every code written has its English beside it, whatever the summary said.
        let mut codes = summary.codes;
        for code in self.by_code.keys() {
            if !codes.contains_key(code) {
                codes.insert(code.clone(), code_text(&self.kind, code).to_string());
            }
        }
        let meta = Meta {
            kind: self.kind.clone(),
            path: summary.path,
            when: now(),
            cancelled: summary.cancelled,
            directories: summary.directories,
            files: summary.files,
            counted: summary.counted,
            rows: self.rows,
            folders: self.folders.len(),
            by_code: std::mem::take(&mut self.by_code),
            codes,
        };
        write_json(&self.store.meta_file(&self.kind), &meta)?;
        self.store.lock().remove(&self.kind);
        Ok(meta)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.out.is_some() {
            let _ = fs::remove_file(&self.temp);
        }
    }
}

impl Store {
    pub fn new(data: &Path) -> Store {
        Store {
            dir: data.join("findings"),
            loaded: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn base_file(&self, kind: &str) -> PathBuf {
        self.dir.join(format!("{kind}.jsonl"))
    }

    fn meta_file(&self, kind: &str) -> PathBuf {
        self.dir.join(format!("{kind}.json"))
    }

    fn updates_file(&self, kind: &str) -> PathBuf {
        self.dir.join(format!("{kind}.updates.jsonl"))
    }

    fn dismissed_file(&self) -> PathBuf {
        self.dir.join("dismissed.json")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Loaded>> {
        self.loaded.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A writer for a check of the kind that starts now.
    pub fn begin(&self, kind: &str) -> Result<Writer, Msg> {
        fs::create_dir_all(&self.dir).map_err(|e| text::at(self.dir.display(), e))?;
        let temp = self.dir.join(format!("{kind}.jsonl.tmp"));
        let file = File::create(&temp).map_err(|e| text::at(temp.display(), e))?;
        Ok(Writer {
            store: self.clone(),
            kind: kind.to_string(),
            temp,
            out: Some(BufWriter::new(file)),
            rows: 0,
            folders: BTreeSet::new(),
            by_code: BTreeMap::new(),
            error: None,
        })
    }

    /// The kind's store read from its files; none without a check of the kind.
    fn read(&self, kind: &str) -> Option<Loaded> {
        let meta: Meta = read_json(&self.meta_file(kind))?;
        let mut rows = read_rows(&self.base_file(kind));
        let mut updates = 0;
        for update in read_updates(&self.updates_file(kind)) {
            rows.retain(|r| r.folder != update.folder);
            rows.extend(update.rows);
            updates += 1;
        }
        let mut loaded = Loaded {
            meta,
            rows,
            updates,
        };
        loaded.settle();
        Some(loaded)
    }

    /// The store of the kind loaded, then `f` on it under the lock.
    fn with<T>(&self, kind: &str, f: impl FnOnce(&mut Loaded) -> T) -> Option<T> {
        let mut map = self.lock();
        if !map.contains_key(kind) {
            let loaded = self.read(kind)?;
            map.insert(kind.to_string(), loaded);
        }
        map.get_mut(kind).map(f)
    }

    /// The dismissals of a kind, as (code, path).
    fn dismissed(&self, kind: &str) -> BTreeSet<(String, String)> {
        self.dismissals()
            .into_iter()
            .filter(|d| d.kind == kind)
            .map(|d| (d.code, d.path))
            .collect()
    }

    pub fn dismissals(&self) -> Vec<Dismissal> {
        read_json(&self.dismissed_file()).unwrap_or_default()
    }

    /// The kind's store as the page sees it whole; none without a check.
    pub fn overview(&self, kind: &str) -> Option<Overview> {
        let dismissed = self.dismissed(kind);
        self.with(kind, |loaded| {
            let mut open = 0;
            let mut gone = 0;
            let mut by_code_open = BTreeMap::new();
            let mut by_code_dismissed = BTreeMap::new();
            let mut open_folders = BTreeSet::new();
            for row in &loaded.rows {
                if dismissed.contains(&(row.code.clone(), row.path.clone())) {
                    gone += 1;
                    *by_code_dismissed.entry(row.code.clone()).or_insert(0) += 1;
                } else {
                    open += 1;
                    *by_code_open.entry(row.code.clone()).or_insert(0) += 1;
                    open_folders.insert(row.folder.as_str());
                }
            }
            Overview {
                meta: loaded.meta.clone(),
                open,
                dismissed: gone,
                open_folders: open_folders.len(),
                by_code_open,
                by_code_dismissed,
            }
        })
    }

    /// Every kind's overview, none where no check was made.
    pub fn overviews(&self) -> BTreeMap<String, Option<Overview>> {
        KINDS
            .iter()
            .map(|k| (k.to_string(), self.overview(k)))
            .collect()
    }

    /// The folders with rows the filter takes, by path, a page of them.
    pub fn folders(
        &self,
        kind: &str,
        filter: &Filter,
        offset: usize,
        limit: usize,
    ) -> Option<FoldersPage> {
        let dismissed = self.dismissed(kind);
        self.with(kind, |loaded| {
            let mut summaries: BTreeMap<&str, FolderSummary> = BTreeMap::new();
            for row in &loaded.rows {
                let gone = dismissed.contains(&(row.code.clone(), row.path.clone()));
                if !filter.takes(row, gone) {
                    continue;
                }
                let s = summaries
                    .entry(row.folder.as_str())
                    .or_insert_with(|| FolderSummary {
                        folder: row.folder.clone(),
                        open: 0,
                        dismissed: 0,
                        codes: BTreeMap::new(),
                    });
                if gone {
                    s.dismissed += 1;
                } else {
                    s.open += 1;
                    *s.codes.entry(row.code.clone()).or_insert(0) += 1;
                }
            }
            let total = summaries.len();
            let folders = summaries
                .into_values()
                .skip(offset)
                .take(limit.clamp(1, FOLDERS_MAX))
                .collect();
            FoldersPage {
                total,
                offset,
                folders,
            }
        })
    }

    /// One folder's rows, every one, with the dismissals marked.
    pub fn rows(&self, kind: &str, folder: &str) -> Option<Vec<RowOut>> {
        let dismissed = self.dismissed(kind);
        self.with(kind, |loaded| rows_of(loaded, folder, &dismissed))
    }

    /// Findings the filter takes, by path, a page of them: the flat table under a
    /// code chip, twenty at a time unless asked otherwise.
    pub fn rows_page(
        &self,
        kind: &str,
        filter: &Filter,
        offset: usize,
        limit: usize,
    ) -> Option<RowsPage> {
        let dismissed = self.dismissed(kind);
        self.with(kind, |loaded| {
            let matched: Vec<RowOut> = loaded
                .rows
                .iter()
                .filter_map(|row| {
                    let gone = dismissed.contains(&(row.code.clone(), row.path.clone()));
                    if !filter.takes(row, gone) {
                        return None;
                    }
                    Some(RowOut {
                        row: row.clone(),
                        dismissed: gone,
                    })
                })
                .collect();
            let total = matched.len();
            let rows = matched
                .into_iter()
                .skip(offset)
                .take(limit.clamp(1, ROWS_MAX))
                .collect();
            RowsPage {
                total,
                offset,
                rows,
            }
        })
    }

    /// The folders with an open finding, by path: what a re-check of everything takes.
    pub fn open_folders(&self, kind: &str) -> Vec<String> {
        let dismissed = self.dismissed(kind);
        self.with(kind, |loaded| {
            let mut out = BTreeSet::new();
            for row in &loaded.rows {
                if !dismissed.contains(&(row.code.clone(), row.path.clone())) {
                    out.insert(row.folder.clone());
                }
            }
            out.into_iter().collect()
        })
        .unwrap_or_default()
    }

    /// A code on a path dismissed, or taken back; the path's folder's rows after.
    pub fn dismiss(
        &self,
        kind: &str,
        code: &str,
        path: &str,
        on: bool,
    ) -> Result<Vec<RowOut>, Msg> {
        let folder = self
            .with(kind, |loaded| {
                loaded
                    .rows
                    .iter()
                    .find(|r| r.code == code && r.path == path)
                    .map(|r| r.folder.clone())
            })
            .flatten()
            .ok_or_else(no_such_finding)?;
        self.write_dismissals(kind, &[(code.to_string(), path.to_string())], on)?;
        Ok(self.rows(kind, &folder).unwrap_or_default())
    }

    /// Many findings dismissed or restored in one write; how many were found and
    /// changed. Unknown pairs are skipped, not an error: a page of ticks may race a
    /// re-check.
    pub fn dismiss_many(
        &self,
        kind: &str,
        items: &[(String, String)],
        on: bool,
    ) -> Result<usize, Msg> {
        if items.is_empty() {
            return Ok(0);
        }
        let known: BTreeSet<(String, String)> = self
            .with(kind, |loaded| {
                loaded
                    .rows
                    .iter()
                    .map(|r| (r.code.clone(), r.path.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let pairs: Vec<(String, String)> = items
            .iter()
            .filter(|p| known.contains(&(p.0.clone(), p.1.clone())))
            .cloned()
            .collect();
        if pairs.is_empty() {
            return Ok(0);
        }
        self.write_dismissals(kind, &pairs, on)?;
        Ok(pairs.len())
    }

    /// Every finding matching the filter's codes dismissed (`on`) or restored; how
    /// many changed. The filter's `dismissed` flag is not used: dismiss takes the
    /// open ones, restore the dismissed ones.
    pub fn dismiss_filter(&self, kind: &str, filter: &Filter, on: bool) -> Result<usize, Msg> {
        let dismissed = self.dismissed(kind);
        let pairs: Vec<(String, String)> = self
            .with(kind, |loaded| {
                loaded
                    .rows
                    .iter()
                    .filter_map(|row| {
                        if !filter.codes.is_empty() && !filter.codes.iter().any(|c| c == &row.code)
                        {
                            return None;
                        }
                        let gone = dismissed.contains(&(row.code.clone(), row.path.clone()));
                        // Dismiss open ones; restore dismissed ones.
                        if on == gone {
                            return None;
                        }
                        Some((row.code.clone(), row.path.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let n = pairs.len();
        if n == 0 {
            return Ok(0);
        }
        self.write_dismissals(kind, &pairs, on)?;
        Ok(n)
    }

    fn write_dismissals(
        &self,
        kind: &str,
        pairs: &[(String, String)],
        on: bool,
    ) -> Result<(), Msg> {
        let mut all = self.dismissals();
        for (code, path) in pairs {
            all.retain(|d| !(d.kind == kind && d.code == *code && d.path == *path));
            if on {
                all.push(Dismissal {
                    kind: kind.to_string(),
                    code: code.clone(),
                    path: path.clone(),
                    when: now(),
                });
            }
        }
        fs::create_dir_all(&self.dir).map_err(|e| text::at(self.dir.display(), e))?;
        write_json(&self.dismissed_file(), &all)
    }

    /// A folder's rows replaced by those of a check of it just made: the overlay
    /// takes the record, the meta follows, and the overlay is folded into the base
    /// when it has grown past the base's folders. The folder's rows after.
    pub fn patch(&self, kind: &str, folder: &str, rows: Vec<Row>) -> Result<Vec<RowOut>, Msg> {
        let dismissed = self.dismissed(kind);
        let update = Update {
            folder: folder.to_string(),
            when: now(),
            rows,
        };
        let updates_file = self.updates_file(kind);
        let fold = self
            .with(kind, |loaded| {
                loaded.rows.retain(|r| r.folder != folder);
                loaded.rows.extend(update.rows.iter().cloned());
                loaded.updates += 1;
                loaded.settle();
                loaded.updates > loaded.meta.folders.max(FOLD_AT)
            })
            .ok_or_else(no_store)?;
        let line = serde_json::to_string(&update).map_err(|e| Msg::raw(e.to_string()))?;
        let mut out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&updates_file)
            .map_err(|e| text::at(updates_file.display(), e))?;
        out.write_all(line.as_bytes())
            .and_then(|_| out.write_all(b"\n"))
            .map_err(|e| text::at(updates_file.display(), e))?;
        if fold {
            self.fold(kind)?;
        }
        let meta = self.with(kind, |loaded| loaded.meta.clone());
        if let Some(meta) = meta {
            write_json(&self.meta_file(kind), &meta)?;
        }
        Ok(self
            .with(kind, |loaded| rows_of(loaded, folder, &dismissed))
            .unwrap_or_default())
    }

    /// The overlay folded into the base: the rows as loaded written whole in place
    /// of the base, the overlay gone.
    fn fold(&self, kind: &str) -> Result<(), Msg> {
        let temp = self.dir.join(format!("{kind}.jsonl.fold"));
        let base = self.base_file(kind);
        let done = self.with(kind, |loaded| -> Result<(), Msg> {
            let file = File::create(&temp).map_err(|e| text::at(temp.display(), e))?;
            let mut out = BufWriter::new(file);
            for row in &loaded.rows {
                let json = serde_json::to_string(row).map_err(|e| Msg::raw(e.to_string()))?;
                out.write_all(json.as_bytes())
                    .and_then(|_| out.write_all(b"\n"))
                    .map_err(|e| text::at(temp.display(), e))?;
            }
            out.flush().map_err(|e| text::at(temp.display(), e))?;
            fs::rename(&temp, &base).map_err(|e| text::at(base.display(), e))?;
            let _ = fs::remove_file(self.updates_file(kind));
            loaded.updates = 0;
            Ok(())
        });
        done.unwrap_or(Ok(()))
    }

    /// The list as text in a language: a header that says the kind, the path, the
    /// time and the counts, then a line per row, the code, the path, the line and
    /// the sentence separated by tabs, and `dismissed` after a dismissed row.
    pub fn export(&self, kind: &str, lang: &str, filter: &Filter) -> Option<String> {
        let dismissed = self.dismissed(kind);
        let renderer = text::Renderer::new(lang);
        self.with(kind, |loaded| {
            let mut lines = Vec::new();
            let mut open = 0;
            let mut gone = 0;
            for row in &loaded.rows {
                let d = dismissed.contains(&(row.code.clone(), row.path.clone()));
                if d {
                    gone += 1;
                } else {
                    open += 1;
                }
                if !filter.takes(row, d) {
                    continue;
                }
                let sentence = renderer.render(&row.text).replace(['\t', '\n', '\r'], " ");
                lines.push(format!(
                    "{}\t{}\t{}\t{}{}",
                    row.code,
                    row.path,
                    row.line,
                    sentence,
                    if d { "\tdismissed" } else { "" }
                ));
            }
            let mut out = format!(
                "# mpdantic {} {} findings under {} checked {}{}: {} open, {} dismissed\n# code\tpath\tline\ttext\n",
                crate::VERSION,
                kind,
                if loaded.meta.path.is_empty() {
                    "the library"
                } else {
                    &loaded.meta.path
                },
                iso(loaded.meta.when),
                if loaded.meta.cancelled {
                    " (cancelled, the list is partial)"
                } else {
                    ""
                },
                open,
                gone
            );
            for line in lines {
                out.push_str(&line);
                out.push('\n');
            }
            out
        })
    }
}

/// Records of the overlay beyond which it is folded, when the base has fewer folders.
const FOLD_AT: usize = 50;

impl Loaded {
    /// The rows in their order and the meta's counts as the rows are.
    fn settle(&mut self) {
        self.rows.sort_by(|a, b| {
            (&a.folder, &a.path, a.line, &a.code).cmp(&(&b.folder, &b.path, b.line, &b.code))
        });
        self.meta.rows = self.rows.len();
        let mut folders = BTreeSet::new();
        let mut by_code = BTreeMap::new();
        for row in &self.rows {
            folders.insert(row.folder.as_str());
            *by_code.entry(row.code.clone()).or_insert(0) += 1;
            if !self.meta.codes.contains_key(&row.code) {
                self.meta.codes.insert(
                    row.code.clone(),
                    code_text(&self.meta.kind, &row.code).to_string(),
                );
            }
        }
        self.meta.folders = folders.len();
        self.meta.by_code = by_code;
    }
}

fn rows_of(loaded: &Loaded, folder: &str, dismissed: &BTreeSet<(String, String)>) -> Vec<RowOut> {
    loaded
        .rows
        .iter()
        .filter(|r| r.folder == folder)
        .map(|r| RowOut {
            row: r.clone(),
            dismissed: dismissed.contains(&(r.code.clone(), r.path.clone())),
        })
        .collect()
}

impl Store {
    /// Whether a check of the kind was made: its store is loaded or on disk.
    pub fn has(&self, kind: &str) -> bool {
        self.lock().contains_key(kind) || self.meta_file(kind).is_file()
    }
}

/// A report's code table as the meta keeps it.
pub fn codes_of(codes: &BTreeMap<String, &'static str>) -> BTreeMap<String, String> {
    codes
        .iter()
        .map(|(k, v)| (k.clone(), (*v).to_string()))
        .collect()
}

/// What a code means, by the kind's own table.
pub fn code_text(kind: &str, code: &str) -> &'static str {
    match kind {
        "cue" => crate::cuecheck::code_text(code),
        "playlists" => crate::plcheck::code_text(code),
        _ => crate::tagcheck::code_text(code),
    }
}

pub fn no_store() -> Msg {
    msg!("core.findings.no_store", "no check of that kind was made")
}

pub fn no_such_finding() -> Msg {
    msg!("core.findings.no_such_finding", "no such finding")
}

pub fn kind_unknown() -> Msg {
    msg!(
        "core.findings.kind_unknown",
        "not a kind whose findings are kept"
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A time as `2026-11-11T12:14:33Z`, from the seconds since the epoch.
pub fn iso(when: u64) -> String {
    let days = (when / 86_400) as i64;
    let rest = when % 86_400;
    // Civil date from days since 1970-01-01, after Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Written whole beside its place, then moved into it.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Msg> {
    let temp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(value).map_err(|e| Msg::raw(e.to_string()))?;
    fs::write(&temp, text.as_bytes()).map_err(|e| text::at(temp.display(), e))?;
    fs::rename(&temp, path).map_err(|e| text::at(path.display(), e))
}

/// The rows of a file, a line each; a line that is not a row (the end of a file cut
/// short) is passed over.
fn read_rows(path: &Path) -> Vec<Row> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

fn read_updates(path: &Path) -> Vec<Update> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_data(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpdantic-findings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn no_track(path: &str) -> Msg {
        msg!(
            "core.tagcheck.no_track",
            "{path}: no track number",
            path = path
        )
    }

    fn summary(path: &str) -> Summary {
        Summary {
            path: path.to_string(),
            cancelled: false,
            directories: 3,
            files: 10,
            counted: 6,
            codes: [(
                "T04".to_string(),
                "a track number missing or not a number".to_string(),
            )]
            .into_iter()
            .collect(),
        }
    }

    fn filled(store: &Store) -> Meta {
        let mut w = store.begin("tags").unwrap();
        w.found("T04", "a", "a/x.flac", 0, &no_track("a/x.flac"));
        w.found("T04", "a", "a/y.flac", 0, &no_track("a/y.flac"));
        w.found("T06", "a", "a", 0, &msg!("core.tagcheck.split", "{dir}: {albums} {albums|album|albums}, {artists} album {artists|artist|artists} in {files} {files|file|files}", dir = "a", albums = 2, artists = 1, files = 2));
        w.found("T04", "b/c", "b/c/z.flac", 0, &no_track("b/c/z.flac"));
        w.finish(summary("")).unwrap()
    }

    #[test]
    fn a_store_is_written_as_the_walk_goes_and_read_by_folder_from_memory_or_the_files() {
        let data = temp_data("write");
        let store = Store::new(&data);
        assert!(store.overview("tags").is_none());
        let meta = filled(&store);
        assert_eq!(meta.rows, 4);
        assert_eq!(meta.folders, 2);
        assert_eq!(meta.by_code["T04"], 3);
        assert_eq!(meta.codes["T06"], "a folder that shows as several albums");
        assert!(data.join("findings/tags.jsonl").is_file());
        assert!(!data.join("findings/tags.jsonl.tmp").exists());
        // Read whole, and read again by a store that knows nothing yet: the files.
        for store in [store.clone(), Store::new(&data)] {
            let o = store.overview("tags").unwrap();
            assert_eq!((o.open, o.dismissed, o.open_folders), (4, 0, 2));
            assert_eq!(o.by_code_open["T06"], 1);
            let page = store.folders("tags", &Filter::default(), 0, 50).unwrap();
            assert_eq!(page.total, 2);
            assert_eq!(page.folders[0].folder, "a");
            assert_eq!(page.folders[0].open, 3);
            assert_eq!(page.folders[0].codes["T04"], 2);
            assert_eq!(page.folders[1].folder, "b/c");
            let only_t06 = Filter {
                codes: vec!["T06".to_string()],
                dismissed: false,
            };
            let page = store.folders("tags", &only_t06, 0, 50).unwrap();
            assert_eq!(page.total, 1);
            assert_eq!(page.folders[0].open, 1);
            let only_t04 = Filter {
                codes: vec!["T04".to_string()],
                dismissed: false,
            };
            let files = store.rows_page("tags", &only_t04, 0, 2).unwrap();
            assert_eq!(files.total, 3);
            assert_eq!(files.rows.len(), 2);
            assert_eq!(files.rows[0].row.path, "a/x.flac");
            let next = store.rows_page("tags", &only_t04, 2, 2).unwrap();
            assert_eq!(next.rows.len(), 1);
            assert_eq!(next.rows[0].row.path, "b/c/z.flac");
            let page = store.folders("tags", &Filter::default(), 1, 1).unwrap();
            assert_eq!(page.folders.len(), 1);
            assert_eq!(page.folders[0].folder, "b/c");
            let rows = store.rows("tags", "a").unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0].row.path, "a");
            assert_eq!(rows[0].row.code, "T06");
            assert_eq!(rows[1].row.path, "a/x.flac");
            assert!(rows.iter().all(|r| !r.dismissed));
            assert_eq!(
                store.open_folders("tags"),
                vec!["a".to_string(), "b/c".to_string()]
            );
        }
        // A writer dropped unfinished leaves the store as it was.
        {
            let mut w = store.begin("tags").unwrap();
            w.found("T01", "q", "q/w.flac", 0, &no_track("q/w.flac"));
        }
        assert!(!data.join("findings/tags.jsonl.tmp").exists());
        assert_eq!(store.overview("tags").unwrap().open, 4);
        fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn many_findings_are_dismissed_in_one_write_by_items_or_by_the_filter() {
        let data = temp_data("bulk");
        let store = Store::new(&data);
        filled(&store);
        let only_t04 = Filter {
            codes: vec!["T04".to_string()],
            dismissed: false,
        };
        assert_eq!(store.dismiss_filter("tags", &only_t04, true).unwrap(), 3);
        assert_eq!(store.overview("tags").unwrap().open, 1);
        assert_eq!(
            store
                .dismiss_many("tags", &[("T06".to_string(), "a".to_string())], true)
                .unwrap(),
            1
        );
        assert_eq!(store.overview("tags").unwrap().open, 0);
        assert_eq!(
            store
                .dismiss_many(
                    "tags",
                    &[("T04".to_string(), "gone.flac".to_string())],
                    true
                )
                .unwrap(),
            0
        );
        assert_eq!(store.dismiss_filter("tags", &only_t04, false).unwrap(), 3);
        assert_eq!(store.overview("tags").unwrap().open, 3);
        fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn a_dismissal_is_kept_across_stores_and_checks_and_a_patch_replaces_a_folders_rows() {
        let data = temp_data("dismiss");
        let store = Store::new(&data);
        filled(&store);
        let rows = store.dismiss("tags", "T04", "a/x.flac", true).unwrap();
        assert!(
            rows.iter()
                .find(|r| r.row.path == "a/x.flac")
                .unwrap()
                .dismissed
        );
        let o = store.overview("tags").unwrap();
        assert_eq!((o.open, o.dismissed), (3, 1));
        assert_eq!(o.by_code_dismissed["T04"], 1);
        assert!(
            matches!(store.dismiss("tags", "T04", "nowhere", true), Err(e) if e.k == "core.findings.no_such_finding")
        );
        // Dismissed rows are left out unless asked for, and a folder of only dismissed
        // rows goes with them.
        store.dismiss("tags", "T04", "b/c/z.flac", true).unwrap();
        let page = store.folders("tags", &Filter::default(), 0, 50).unwrap();
        assert_eq!(page.total, 1);
        let with = Filter {
            codes: vec![],
            dismissed: true,
        };
        let page = store.folders("tags", &with, 0, 50).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.folders[1].dismissed, 1);
        assert_eq!(store.open_folders("tags"), vec!["a".to_string()]);
        // Taken back.
        store.dismiss("tags", "T04", "b/c/z.flac", false).unwrap();
        assert_eq!(store.overview("tags").unwrap().open, 3);
        // A check again keeps the dismissal: it is the owner's word on the path.
        filled(&store);
        assert_eq!(store.overview("tags").unwrap().dismissed, 1);
        assert_eq!(Store::new(&data).overview("tags").unwrap().dismissed, 1);
        // A patch: the folder's rows replaced, in the overlay, read by a fresh store.
        let after = store
            .patch(
                "tags",
                "a",
                vec![Row {
                    code: "T04".to_string(),
                    folder: "a".to_string(),
                    path: "a/y.flac".to_string(),
                    line: 0,
                    text: no_track("a/y.flac"),
                }],
            )
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].row.path, "a/y.flac");
        assert!(data.join("findings/tags.updates.jsonl").is_file());
        let fresh = Store::new(&data);
        let o = fresh.overview("tags").unwrap();
        assert_eq!((o.open, o.dismissed, o.meta.folders), (2, 0, 2));
        assert_eq!(fresh.rows("tags", "a").unwrap().len(), 1);
        // A folder patched to nothing leaves the list; many patches fold the overlay.
        store.patch("tags", "b/c", vec![]).unwrap();
        assert_eq!(
            store
                .folders("tags", &Filter::default(), 0, 50)
                .unwrap()
                .total,
            1
        );
        for i in 0..FOLD_AT + 2 {
            let path = format!("a/{i}.flac");
            store
                .patch(
                    "tags",
                    "a",
                    vec![Row {
                        code: "T04".to_string(),
                        folder: "a".to_string(),
                        path: path.clone(),
                        line: 0,
                        text: no_track(&path),
                    }],
                )
                .unwrap();
        }
        // Folded once past the threshold: the overlay holds only the patches since,
        // the base the rest, and a fresh store reads the latest state from both.
        let records = fs::read_to_string(data.join("findings/tags.updates.jsonl"))
            .map(|t| t.lines().count())
            .unwrap_or(0);
        assert!(records < FOLD_AT, "{records} records in the overlay");
        assert!(read_rows(&data.join("findings/tags.jsonl"))
            .iter()
            .any(|r| r.path.starts_with("a/")));
        let fresh = Store::new(&data);
        assert_eq!(
            fresh.rows("tags", "a").unwrap()[0].row.path,
            format!("a/{}.flac", FOLD_AT + 1)
        );
        assert_eq!(fresh.overview("tags").unwrap().open, 1);
        fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn the_export_is_the_rows_as_text_in_the_language_asked() {
        let data = temp_data("export");
        let store = Store::new(&data);
        filled(&store);
        store.dismiss("tags", "T04", "a/x.flac", true).unwrap();
        let en = store.export("tags", "en", &Filter::default()).unwrap();
        let lines: Vec<&str> = en.lines().collect();
        assert!(lines[0].starts_with("# mpdantic "));
        assert!(lines[0].contains(" tags findings under the library checked 20"));
        assert!(lines[0].ends_with(": 3 open, 1 dismissed"));
        assert_eq!(lines[1], "# code\tpath\tline\ttext");
        assert_eq!(
            lines[2],
            "T06\ta\t0\ta: 2 albums, 1 album artist in 2 files"
        );
        assert_eq!(lines[3], "T04\ta/y.flac\t0\ta/y.flac: no track number");
        assert_eq!(lines.len(), 5);
        let de = store
            .export(
                "tags",
                "de",
                &Filter {
                    codes: vec![],
                    dismissed: true,
                },
            )
            .unwrap();
        assert!(de.contains("T04\ta/x.flac\t0\ta/x.flac: keine Titelnummer\tdismissed\n"));
        assert!(de.contains("T06\ta\t0\ta: 2 Alben, 1 Albuminterpret in 2 Dateien\n"));
        // The file table's code chip is the export's code filter (U5).
        let one = store
            .export(
                "tags",
                "en",
                &Filter {
                    codes: vec!["T04".into()],
                    dismissed: false,
                },
            )
            .unwrap();
        assert!(one.contains("T04\ta/y.flac\t"), "{one}");
        assert!(!one.contains("T06\t"), "{one}");
        assert_eq!(iso(1_794_399_273), "2026-11-11T12:14:33Z");
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        fs::remove_dir_all(&data).unwrap();
    }
}
