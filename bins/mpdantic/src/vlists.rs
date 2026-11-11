//! Volumio's own lists: its playlists, favourites, radio favourites, web radios and
//! the queue it persists, read as Volumio reads them (JSON arrays of items under
//! /data), each item checked against the library as the playlist check checks an
//! entry, and a list repaired where it is simply messed up: an item whose file is
//! gone found again by the longest tail of its path or dropped, a uri in twice
//! dropped, an item without a uri or a service dropped, a file cut short written
//! back with the items that could be read.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::classify;
use crate::library::Library;
use crate::mpd::Decoders;
use crate::plfix::{self, Index};
use crate::text::Msg;

/// Items past this many are counted, not listed.
pub const MAX_LISTED: usize = 300;
/// A list file larger than this is not read.
pub const MAX_LIST: u64 = 16 * 1024 * 1024;

/// Where Volumio keeps its lists: all under its data folder, beside the playlists.
#[derive(Clone, Debug)]
pub struct Places {
    pub playlists: PathBuf,
    pub favourites: PathBuf,
    pub queue: PathBuf,
}

impl Places {
    /// From the playlist folder: `/data/playlist` has `/data/favourites` and
    /// `/data/queue` beside it.
    pub fn beside(playlists: &Path) -> Places {
        let data = playlists.parent().unwrap_or(playlists);
        Places {
            playlists: playlists.to_path_buf(),
            favourites: data.join("favourites"),
            queue: data.join("queue"),
        }
    }
}

/// One of Volumio's lists: its kind, its name as the page says it, its file.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ListFile {
    /// `playlist`, `favourites`, `radio-favourites`, `my-web-radio`, `queue`.
    pub kind: String,
    pub name: String,
    /// `playlist:<name>` or the kind: what a repair request names.
    pub id: String,
    pub path: PathBuf,
}

/// Every list Volumio has, the playlists sorted by name.
pub fn lists(places: &Places) -> Vec<ListFile> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&places.playlists) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| !n.starts_with('.') && !n.ends_with(".part"))
            .collect();
        names.sort();
        for name in names {
            out.push(ListFile {
                kind: "playlist".to_string(),
                id: format!("playlist:{name}"),
                path: places.playlists.join(&name),
                name,
            });
        }
    }
    for kind in ["favourites", "radio-favourites", "my-web-radio"] {
        let path = places.favourites.join(kind);
        if path.is_file() {
            out.push(ListFile {
                kind: kind.to_string(),
                name: kind.to_string(),
                id: kind.to_string(),
                path,
            });
        }
    }
    if places.queue.is_file() {
        out.push(ListFile {
            kind: "queue".to_string(),
            name: "queue".to_string(),
            id: "queue".to_string(),
            path: places.queue.clone(),
        });
    }
    out
}

/// The list a repair request names, or none.
pub fn by_id(places: &Places, id: &str) -> Option<ListFile> {
    lists(places).into_iter().find(|l| l.id == id)
}

/// The items of a list file: the array parsed; when the text does not parse, the
/// items that can be read before the cut (the array closed after the last complete
/// item), said as salvaged.
pub struct Items {
    pub items: Vec<serde_json::Value>,
    pub problem: Option<Msg>,
    pub salvaged: bool,
}

pub fn read_items(bytes: &[u8]) -> Items {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Items {
            items: Vec::new(),
            problem: None,
            salvaged: false,
        };
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Array(items)) => Items {
            items,
            problem: None,
            salvaged: false,
        },
        Ok(_) => Items {
            items: Vec::new(),
            problem: Some(msg!(
                "core.vlists.not_array",
                "not a list: the JSON is not an array"
            )),
            salvaged: false,
        },
        Err(e) => {
            // The last complete item: the longest prefix that parses once closed.
            let mut end = trimmed.len();
            while let Some(i) = trimmed[..end].rfind('}') {
                let candidate = format!("{}]", trimmed[..=i].trim_end_matches(','));
                if let Ok(serde_json::Value::Array(items)) =
                    serde_json::from_str::<serde_json::Value>(&candidate)
                {
                    return Items {
                        items,
                        problem: Some(msg!(
                            "core.vlists.cut_short",
                            "cut short: {error}",
                            error = e.to_string()
                        )),
                        salvaged: true,
                    };
                }
                end = i;
            }
            Items {
                items: Vec::new(),
                problem: Some(msg!(
                    "core.vlists.not_parse",
                    "does not parse: {error}",
                    error = e.to_string()
                )),
                salvaged: false,
            }
        }
    }
}

/// One item as checked.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ItemState {
    pub index: usize,
    pub uri: String,
    pub service: String,
    pub title: String,
    /// `ok`, `missing`, `not_decoded`, `foreign`, `stream`, `other_service`, `broken`,
    /// `duplicate`.
    pub state: &'static str,
}

fn field<'a>(item: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| item.get(*k).and_then(|v| v.as_str()))
}

/// An mpd uri's library path: `mnt/<path>`, checked as a library path.
fn library_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("mnt/")?;
    Library::check(rest).filter(|p| !p.is_empty())
}

/// What an item is against the library; duplicates are decided by the caller.
pub fn state_of(music: &Path, item: &serde_json::Value, decoders: &Decoders) -> &'static str {
    let Some(uri) = field(item, &["uri"]) else {
        return "broken";
    };
    let service = field(item, &["service"]).unwrap_or("");
    if uri.trim().is_empty() || service.is_empty() && !uri.starts_with("mnt/") {
        return "broken";
    }
    if service == "webradio" || (service.is_empty() && uri.contains("://")) {
        return "stream";
    }
    if service != "mpd" {
        return "other_service";
    }
    let Some(path) = library_path(uri) else {
        return "foreign";
    };
    let full = music.join(&path);
    if full.is_dir() {
        return "ok";
    }
    if !full.is_file() {
        return "missing";
    }
    // Claimed by any of MPD's decoders, as the playlist check judges an entry: a
    // video container the ffmpeg decoder claims plays (Glass plays it), so it is found.
    let claimed = classify::suffix(&path)
        .map(|s| decoders.suffixes.contains(&s))
        .unwrap_or(false);
    if claimed {
        "ok"
    } else {
        "not_decoded"
    }
}

/// A list as checked.
#[derive(Serialize, Clone, Debug)]
pub struct ListReport {
    pub kind: String,
    pub name: String,
    pub id: String,
    pub file: String,
    pub bytes: u64,
    pub items: usize,
    pub ok: usize,
    pub missing: usize,
    pub not_decoded: usize,
    pub foreign: usize,
    pub streams: usize,
    pub other_services: usize,
    pub broken: usize,
    pub duplicates: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<Msg>,
    pub salvaged: bool,
    /// The first items, each with its state.
    pub list: Vec<ItemState>,
    pub truncated: bool,
    /// Whether a repair has anything to do.
    pub repairable: bool,
}

fn states(music: &Path, items: &[serde_json::Value], decoders: &Decoders) -> Vec<ItemState> {
    let mut seen = std::collections::HashSet::new();
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let uri = field(item, &["uri"]).unwrap_or("").to_string();
            let mut state = state_of(music, item, decoders);
            if state != "broken" && !seen.insert(uri.clone()) {
                state = "duplicate";
            }
            ItemState {
                index,
                service: field(item, &["service"]).unwrap_or("").to_string(),
                title: field(item, &["title", "name"]).unwrap_or("").to_string(),
                uri,
                state,
            }
        })
        .collect()
}

/// A list read and checked.
pub fn check(music: &Path, decoders: &Decoders, list: &ListFile) -> ListReport {
    let mut r = ListReport {
        kind: list.kind.clone(),
        name: list.name.clone(),
        id: list.id.clone(),
        file: list.path.display().to_string(),
        bytes: 0,
        items: 0,
        ok: 0,
        missing: 0,
        not_decoded: 0,
        foreign: 0,
        streams: 0,
        other_services: 0,
        broken: 0,
        duplicates: 0,
        problem: None,
        salvaged: false,
        list: Vec::new(),
        truncated: false,
        repairable: false,
    };
    let bytes = match fs::metadata(&list.path) {
        Ok(m) => m.len(),
        Err(e) => {
            r.problem = Some(msg!("core.vlists.not_read", "not read: {error}", error = e));
            return r;
        }
    };
    r.bytes = bytes;
    if bytes > MAX_LIST {
        r.problem = Some(msg!(
            "core.vlists.too_large",
            "{bytes} bytes: not read",
            bytes = bytes
        ));
        return r;
    }
    let text = match fs::read(&list.path) {
        Ok(t) => t,
        Err(e) => {
            r.problem = Some(msg!("core.vlists.not_read", "not read: {error}", error = e));
            return r;
        }
    };
    let read = read_items(&text);
    r.problem = read.problem;
    r.salvaged = read.salvaged;
    r.items = read.items.len();
    let all = states(music, &read.items, decoders);
    for s in &all {
        match s.state {
            "ok" => r.ok += 1,
            "missing" => r.missing += 1,
            "not_decoded" => r.not_decoded += 1,
            "foreign" => r.foreign += 1,
            "stream" => r.streams += 1,
            "other_service" => r.other_services += 1,
            "broken" => r.broken += 1,
            "duplicate" => r.duplicates += 1,
            _ => {}
        }
    }
    r.repairable = r.missing + r.foreign + r.broken + r.duplicates > 0 || r.salvaged;
    r.truncated = all.len() > MAX_LISTED;
    r.list = all.into_iter().take(MAX_LISTED).collect();
    r
}

/// One item's fate in a repair.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ItemChange {
    pub index: usize,
    pub uri: String,
    pub title: String,
    /// `kept`, `relocated`, `dropped`.
    pub action: &'static str,
    pub why: Msg,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

/// The repairs a list allows and whether each is on.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ListRepair {
    pub id: &'static str,
    pub what: Msg,
    pub on: bool,
}

/// The list as it would be after the repairs chosen.
#[derive(Serialize, Clone, Debug, Default)]
pub struct Fixed {
    pub after: String,
    pub repairs: Vec<ListRepair>,
    pub changes: Vec<ItemChange>,
    pub kept: usize,
    pub changed: bool,
    pub notes: Vec<Msg>,
}

const REPAIR_IDS: [&str; 4] = ["relocate", "drop", "dedupe", "salvage"];

/// The repair planned: nothing written. `chosen` is the repairs by id, every
/// applicable one when not given.
pub fn plan(
    music: &Path,
    decoders: &Decoders,
    list: &ListFile,
    bytes: &[u8],
    chosen: Option<&[String]>,
) -> Result<Fixed, Msg> {
    let on = |id: &str| match chosen {
        None => true,
        Some(c) => c.iter().any(|x| x == id),
    };
    let read = read_items(bytes);
    if read.problem.is_some() && !read.salvaged && read.items.is_empty() && !bytes.is_empty() {
        return Err(msg!(
            "core.vlists.problem",
            "{name}: {problem}",
            name = list.name,
            problem = read.problem.unwrap_or_else(|| Msg::raw(""))
        ));
    }
    let all = states(music, &read.items, decoders);
    let mut fixed = Fixed::default();
    let mut index: Option<Index> = None;
    let mut applicable = std::collections::BTreeMap::new();
    let mut kept_items: Vec<serde_json::Value> = Vec::new();
    for (s, item) in all.iter().zip(read.items.iter()) {
        let mut change = ItemChange {
            index: s.index,
            uri: s.uri.clone(),
            title: s.title.clone(),
            action: "kept",
            why: Msg::raw(""),
            after: None,
        };
        let mut item = item.clone();
        match s.state {
            "missing" | "foreign" => {
                applicable.insert("relocate", ());
                applicable.insert("drop", ());
                let mut found = None;
                if on("relocate") {
                    if index.is_none() {
                        index = Some(Index::build(music, "", decoders)?);
                    }
                    let parts = plfix::components(s.uri.strip_prefix("mnt/").unwrap_or(&s.uri));
                    found = index.as_ref().and_then(|i| i.find(&parts));
                }
                match found {
                    Some((library_path, ties)) => {
                        let new_uri = format!("mnt/{library_path}");
                        change.action = "relocated";
                        change.why = if ties > 0 {
                            msg!(
                                "core.vlists.relocated_ties",
                                "found by its name and folders; {ties} others matched as well",
                                ties = ties
                            )
                        } else {
                            msg!("core.vlists.relocated", "found by its name and folders")
                        };
                        change.after = Some(new_uri.clone());
                        if let Some(obj) = item.as_object_mut() {
                            obj.insert("uri".to_string(), serde_json::Value::String(new_uri));
                        }
                    }
                    None if on("drop") => {
                        change.action = "dropped";
                        change.why = if s.state == "foreign" {
                            msg!(
                                "core.vlists.dropped_foreign",
                                "a foreign path, nothing on this player"
                            )
                        } else {
                            msg!(
                                "core.vlists.dropped_missing",
                                "the file is not under the library"
                            )
                        };
                    }
                    None => {
                        change.why = msg!("core.vlists.left", "not found; left as it is");
                    }
                }
            }
            "broken" => {
                applicable.insert("drop", ());
                if on("drop") {
                    change.action = "dropped";
                    change.why = msg!("core.vlists.dropped_broken", "no uri or no service");
                }
            }
            "duplicate" => {
                applicable.insert("dedupe", ());
                if on("dedupe") {
                    change.action = "dropped";
                    change.why = msg!("core.vlists.dropped_duplicate", "the same uri again");
                }
            }
            _ => {}
        }
        if change.action != "dropped" {
            kept_items.push(item);
        }
        fixed.changes.push(change);
    }
    if read.salvaged {
        applicable.insert("salvage", ());
        fixed.notes.push(msg!(
            "core.vlists.salvaged",
            "the file was cut short ({problem}); {n} items could be read and are what is written",
            problem = read.problem.clone().unwrap_or_else(|| Msg::raw("")),
            n = read.items.len()
        ));
    }
    if let Some(i) = &index {
        if i.truncated {
            fixed.notes.push(msg!(
                "core.vlists.index_truncated",
                "the library holds more than the index looks at; only the first files were searched"
            ));
        }
    }
    for id in REPAIR_IDS {
        if !applicable.contains_key(id) {
            continue;
        }
        let what = match id {
            "relocate" => msg!(
                "core.vlists.relocate",
                "an item whose file is gone found again by its name and folders"
            ),
            "drop" => msg!(
                "core.vlists.drop",
                "an item not found, foreign or broken dropped"
            ),
            "dedupe" => msg!("core.vlists.dedupe", "a uri in the list twice kept once"),
            _ => msg!(
                "core.vlists.salvage",
                "a file cut short written back with the items that could be read"
            ),
        };
        fixed.repairs.push(ListRepair {
            id,
            what,
            on: on(id),
        });
    }
    fixed.kept = kept_items.len();
    fixed.after = serde_json::Value::Array(kept_items).to_string();
    let salvage_on = read.salvaged && on("salvage");
    fixed.changed = fixed.changes.iter().any(|c| c.action != "kept") || salvage_on;
    if read.salvaged && !salvage_on {
        // The cut file stays as it is, so nothing else can be written either.
        fixed.changed = false;
        fixed.notes.push(msg!(
            "core.vlists.cut_left",
            "the cut file is left as it is"
        ));
    }
    Ok(fixed)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::library::tests::temp_library;
    use crate::mpd;

    fn decoders() -> Decoders {
        mpd::tests::decoders_of(&[
            ("flac", &["flac"]),
            ("mad", &["mp3"]),
            ("ffmpeg", &["mp4", "mkv"]),
        ])
    }

    fn places(root: &Path) -> Places {
        let p = Places::beside(&root.join("volumio/playlist"));
        fs::create_dir_all(&p.playlists).unwrap();
        fs::create_dir_all(&p.favourites).unwrap();
        p
    }

    #[test]
    fn the_lists_are_found_and_a_cut_file_gives_the_items_before_the_cut() {
        let root = temp_library("vlists-read");
        let p = places(&root);
        fs::write(p.playlists.join("Mix"), "[]").unwrap();
        fs::write(p.playlists.join(".hidden"), "[]").unwrap();
        fs::write(p.favourites.join("favourites"), "[]").unwrap();
        fs::write(&p.queue, "[]").unwrap();
        let ids: Vec<String> = lists(&p).into_iter().map(|l| l.id).collect();
        assert_eq!(ids, ["playlist:Mix", "favourites", "queue"]);
        assert_eq!(by_id(&p, "playlist:Mix").unwrap().name, "Mix");
        assert!(by_id(&p, "playlist:none").is_none());
        let whole = read_items(br#"[{"uri":"a"},{"uri":"b"}]"#);
        assert_eq!(whole.items.len(), 2);
        assert_eq!(whole.problem, None);
        let cut = read_items(br#"[{"uri":"a"},{"uri":"b"},{"uri":"c","ser"#);
        assert_eq!(cut.items.len(), 2, "{:?}", cut.problem);
        assert!(cut.salvaged);
        assert!(cut.problem.as_deref().unwrap().starts_with("cut short"));
        let empty = read_items(b"\n");
        assert_eq!(empty.items.len(), 0);
        assert_eq!(empty.problem, None);
        let junk = read_items(b"not json at all");
        assert!(junk.items.is_empty() && !junk.salvaged && junk.problem.is_some());
        let object = read_items(b"{\"uri\":\"a\"}");
        assert!(object.problem.as_deref().unwrap().starts_with("not a list"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_list_is_checked_item_by_item_and_repaired_where_messed_up() {
        let root = temp_library("vlists-check");
        let p = places(&root);
        fs::create_dir_all(root.join("INTERNAL/Jazz/Live")).unwrap();
        fs::write(root.join("INTERNAL/Jazz/Live/01.flac"), b"x").unwrap();
        fs::write(root.join("INTERNAL/Jazz/Live/notes.txt"), b"x").unwrap();
        fs::write(root.join("INTERNAL/Jazz/Live/S1.E01.mp4"), b"x").unwrap();
        let text = concat!(
            r#"[{"service":"mpd","uri":"mnt/INTERNAL/Jazz/Live/01.flac","title":"One"},"#,
            r#"{"service":"mpd","uri":"mnt/INTERNAL/Old/Live/01.flac","title":"Moved"},"#,
            r#"{"service":"mpd","uri":"mnt/INTERNAL/Jazz/Live/gone.flac","title":"Gone"},"#,
            r#"{"service":"mpd","uri":"mnt/INTERNAL/Jazz/Live/notes.txt","title":"Text"},"#,
            r#"{"service":"mpd","uri":"/absolute/x.flac","title":"Foreign"},"#,
            r#"{"service":"webradio","uri":"http://radio.example/live","title":"Radio"},"#,
            r#"{"service":"spotify","uri":"spotify:track:1","title":"Spotify"},"#,
            r#"{"title":"Broken"},"#,
            r#"{"service":"mpd","uri":"mnt/INTERNAL/Jazz/Live/01.flac","title":"One again"},"#,
            r#"{"service":"mpd","uri":"mnt/INTERNAL/Jazz","title":"A folder"},"#,
            r#"{"service":"mpd","uri":"mnt/INTERNAL/Jazz/Live/S1.E01.mp4","title":"An episode Glass plays"}]"#
        );
        fs::write(p.playlists.join("Mix"), text).unwrap();
        let list = by_id(&p, "playlist:Mix").unwrap();
        let r = check(&root, &decoders(), &list);
        assert_eq!(r.items, 11);
        let states: Vec<&str> = r.list.iter().map(|s| s.state).collect();
        assert_eq!(
            states,
            [
                "ok",
                "missing",
                "missing",
                "not_decoded",
                "foreign",
                "stream",
                "other_service",
                "broken",
                "duplicate",
                "ok",
                "ok"
            ],
            "a video container the ffmpeg decoder claims is found"
        );
        assert_eq!(
            (
                r.ok,
                r.missing,
                r.not_decoded,
                r.foreign,
                r.streams,
                r.other_services,
                r.broken,
                r.duplicates
            ),
            (3, 2, 1, 1, 1, 1, 1, 1)
        );
        assert!(r.repairable);
        assert_eq!(r.list[1].title, "Moved");
        // The repair with everything on: the moved file found again, the gone one and
        // the foreign one dropped, the broken one and the duplicate dropped.
        let f = plan(&root, &decoders(), &list, text.as_bytes(), None).unwrap();
        let actions: Vec<(&str, &str)> = f
            .changes
            .iter()
            .map(|c| (c.action, c.why.en.as_str()))
            .collect();
        assert_eq!(actions[0], ("kept", ""));
        assert_eq!(actions[1].0, "relocated");
        assert_eq!(
            f.changes[1].after.as_deref(),
            Some("mnt/INTERNAL/Jazz/Live/01.flac")
        );
        assert_eq!(actions[2].0, "dropped");
        assert_eq!(
            actions[3],
            ("kept", ""),
            "a file no decoder claims is left alone"
        );
        assert_eq!(actions[4].0, "dropped");
        assert_eq!(actions[5], ("kept", ""));
        assert_eq!(actions[6], ("kept", ""));
        assert_eq!(actions[7], ("dropped", "no uri or no service"));
        assert_eq!(actions[8], ("dropped", "the same uri again"));
        assert_eq!(actions[9], ("kept", ""));
        assert_eq!(f.kept, 7);
        assert!(f.changed);
        let ids: Vec<&str> = f.repairs.iter().map(|r| r.id).collect();
        assert_eq!(ids, ["relocate", "drop", "dedupe"]);
        let after: serde_json::Value = serde_json::from_str(&f.after).unwrap();
        assert_eq!(after.as_array().unwrap().len(), 7);
        assert_eq!(after[1]["uri"], "mnt/INTERNAL/Jazz/Live/01.flac");
        assert_eq!(after[1]["title"], "Moved", "the rest of the item kept");
        assert!(!f.after.contains('\n'), "compact, as Volumio writes it");
        // Relocation off: the moved one is dropped with the rest.
        let f = plan(
            &root,
            &decoders(),
            &list,
            text.as_bytes(),
            Some(&["drop".to_string(), "dedupe".to_string()]),
        )
        .unwrap();
        assert_eq!(f.changes[1].action, "dropped");
        assert_eq!(f.kept, 6);
        // Nothing chosen: everything kept, nothing to write.
        let f = plan(&root, &decoders(), &list, text.as_bytes(), Some(&[])).unwrap();
        assert!(!f.changed);
        assert_eq!(f.kept, 11);
        // A cut file: written back with what could be read, when salvage is on.
        let cut = &text[..text.len() - 40];
        let f = plan(&root, &decoders(), &list, cut.as_bytes(), None).unwrap();
        assert!(f.changed);
        assert!(f.repairs.iter().any(|r| r.id == "salvage" && r.on));
        assert!(f.notes[0].starts_with("the file was cut short"));
        let f = plan(
            &root,
            &decoders(),
            &list,
            cut.as_bytes(),
            Some(&["drop".to_string()]),
        )
        .unwrap();
        assert!(!f.changed, "the cut file left as it is");
        // Junk: refused.
        assert!(plan(&root, &decoders(), &list, b"not json", None).is_err());
        fs::remove_dir_all(&root).unwrap();
    }
}
