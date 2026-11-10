//! A batch of writes: prepared without writing, shown whole, confirmed once, applied
//! with a check that each file is what the preview read, journaled for the undo, and
//! followed by MPD's update of the folder. The first kind is `.mpdignore` lines into
//! the folder in hand.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::ignore::{ExcludeList, Pattern};
use crate::library::Library;
use crate::mount::{self, Mount, Mounter, Share, Where};
use crate::mpd;
use crate::undo::{Batch, Entry, Journal};

/// What the page asks for.
pub struct Request {
    pub kind: String,
    pub path: String,
    pub lines: Vec<String>,
}

/// The batch as it would be written, for the page to confirm.
#[derive(Serialize, Clone, Debug)]
pub struct Preview {
    pub id: u64,
    pub kind: String,
    /// The folder in hand.
    pub path: String,
    /// The file, in MPD's form.
    pub file: String,
    pub exists: bool,
    pub before: String,
    pub after: String,
    pub added: Vec<String>,
    pub already: Vec<String>,
    pub refused: Vec<(String, String)>,
    /// `local`, or the share and the folder that would be mounted.
    pub place: String,
    /// The file as read: its size and modification time, 0 when it is not there.
    pub size: u64,
    pub mtime: u64,
}

/// What an apply or an undo did.
#[derive(Serialize, Debug)]
pub struct Applied {
    pub batch: String,
    pub file: String,
    pub written: bool,
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

/// The batch prepared from the folder's `.mpdignore` as it is and the lines asked
/// for. Nothing is written.
pub fn prepare(music: &Path, shares: &[Share], id: u64, req: &Request) -> Result<Preview, String> {
    if req.kind != "mpdignore" {
        return Err("the kind is not one the core writes".to_string());
    }
    let path = Library::check(&req.path).ok_or_else(|| "not a library path".to_string())?;
    let dir = if path.is_empty() {
        music.to_path_buf()
    } else {
        music.join(&path)
    };
    if !dir.is_dir() {
        return Err("no such folder".to_string());
    }
    let place = match mount::place(shares, &path)? {
        Where::Local => "local".to_string(),
        Where::Nas { share, folder } => format!(
            "the share {}, folder {}",
            share.name,
            if folder.is_empty() { "/" } else { &folder }
        ),
    };
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
    let in_force = ExcludeList::parse(&before);
    let _ = in_force.lines();
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
    Ok(Preview {
        id,
        kind: req.kind.clone(),
        file: if path.is_empty() {
            ".mpdignore".to_string()
        } else {
            format!("{path}/.mpdignore")
        },
        path,
        exists,
        before,
        after,
        added,
        already,
        refused,
        place,
        size,
        mtime,
    })
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
            let dir = if path.is_empty() {
                music.to_path_buf()
            } else {
                music.join(path)
            };
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

/// A text file written whole and moved into place.
fn write_whole(dir: &Path, name: &str, text: &str) -> Result<(), String> {
    let part = dir.join(format!("{name}.part"));
    let file = dir.join(name);
    fs::write(&part, text).map_err(|e| format!("{}: {e}", part.display()))?;
    fs::rename(&part, &file).map_err(|e| {
        let _ = fs::remove_file(&part);
        format!("{}: {e}", file.display())
    })
}

/// The preview applied: the file checked to be what the preview read, the folder
/// made writable, the file written, the batch journaled, MPD told.
pub fn apply(
    music: &Path,
    shares: &[Share],
    mounter: &Mounter,
    journal: &Journal,
    mpd_address: &mpd::Address,
    preview: &Preview,
) -> Result<Applied, String> {
    if preview.added.is_empty() {
        return Err("nothing to write".to_string());
    }
    let ro = music.join(&preview.file);
    let (exists, size, mtime) = stat(&ro);
    if exists != preview.exists || size != preview.size || mtime != preview.mtime {
        return Err("the file changed since the preview; preview it again".to_string());
    }
    let (dir, mount) = writable(music, shares, mounter, &preview.path)?;
    let result = write_whole(&dir, ".mpdignore", &preview.after);
    let journaled = result.and_then(|_| {
        let batch = Batch {
            id: journal.new_id(),
            kind: preview.kind.clone(),
            path: preview.path.clone(),
            when: now(),
            note: format!(
                "{} {} into {}",
                preview.added.len(),
                if preview.added.len() == 1 {
                    "line"
                } else {
                    "lines"
                },
                preview.file
            ),
            entries: vec![Entry {
                path: preview.file.clone(),
                op: "write_text".to_string(),
                before: if preview.exists {
                    Some(preview.before.clone())
                } else {
                    None
                },
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
    let (updated, update_error) = match mpd::update(mpd_address, &preview.path) {
        Ok(n) => (Some(n), None),
        Err(e) => (None, Some(e)),
    };
    Ok(Applied {
        batch,
        file: preview.file.clone(),
        written: true,
        updated,
        update_error,
    })
}

/// A batch undone: each file checked to be what the batch left, the before written
/// back or the file removed, the batch marked, MPD told.
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
    let Some(entry) = batch.entries.first() else {
        return Err("an empty batch".to_string());
    };
    let current = fs::read_to_string(music.join(&entry.path)).unwrap_or_default();
    if current != entry.after {
        return Err("the file changed since the batch; not undone".to_string());
    }
    let (dir, mount) = writable(music, shares, mounter, &batch.path)?;
    let name = entry
        .path
        .rsplit('/')
        .next()
        .unwrap_or(".mpdignore")
        .to_string();
    let result = match &entry.before {
        Some(before) => write_whole(&dir, &name, before),
        None => fs::remove_file(dir.join(&name)).map_err(|e| format!("{}: {e}", entry.path)),
    };
    if let Some(mut m) = mount {
        mounter.unmount(&mut m);
    }
    result?;
    journal.mark_undone(id)?;
    let (updated, update_error) = match mpd::update(mpd_address, &batch.path) {
        Ok(n) => (Some(n), None),
        Err(e) => (None, Some(e)),
    };
    Ok(Applied {
        batch: id.to_string(),
        file: entry.path.clone(),
        written: true,
        updated,
        update_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::tests::sample_config;
    use crate::mpd::tests::fake_mpd;
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

    fn request(path: &str, lines: &[&str]) -> Request {
        Request {
            kind: "mpdignore".into(),
            path: path.into(),
            lines: lines.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_preview_says_what_would_be_written_and_refuses_what_mpd_would_not_read() {
        let dir = temp("preview");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        fs::write(music.join("INTERNAL/Jazz/.mpdignore"), "*.iso\n#recycle").unwrap();
        let p = prepare(
            &music,
            &shares,
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
        assert!(p.size > 0);
        let p = prepare(
            &music,
            &shares,
            2,
            &request("NAS/multimedia/Dune", &["*.srt"]),
        )
        .unwrap();
        assert!(!p.exists);
        assert_eq!(p.after, "*.srt\n");
        assert_eq!(p.place, "the share multimedia, folder Dune");
        assert!(prepare(&music, &shares, 3, &request("../x", &["a"])).is_err());
        assert!(prepare(&music, &shares, 3, &request("INTERNAL/Rock", &["a"])).is_err());
        assert!(prepare(
            &music,
            &shares,
            3,
            &Request {
                kind: "tags".into(),
                path: "INTERNAL".into(),
                lines: vec![]
            }
        )
        .is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_local_batch_is_written_journaled_updated_and_undone() {
        let dir = temp("local");
        let music = dir.join("music");
        let shares = mount::shares(&sample_config(&dir)).unwrap();
        let mounter = Mounter::new(
            &dir.join("run"),
            Box::new(|_| panic!("no mount for a local root")),
        );
        let journal = Journal::new(&dir.join("undo"));
        let address = fake_mpd("OK MPD 0.24.6\n");
        let p = prepare(
            &music,
            &shares,
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
        // The same preview again: the file moved on.
        assert!(apply(&music, &shares, &mounter, &journal, &address, &p)
            .unwrap_err()
            .contains("changed since"));
        // A second batch on top, then both undone in order.
        let p2 = prepare(&music, &shares, 2, &request("INTERNAL/Jazz", &["*.mov"])).unwrap();
        let a2 = apply(&music, &shares, &mounter, &journal, &address, &p2).unwrap();
        assert_eq!(
            fs::read_to_string(music.join("INTERNAL/Jazz/.mpdignore")).unwrap(),
            "*.iso\n[#]recycle\n*.mov\n"
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
        let empty = prepare(&music, &shares, 3, &request("INTERNAL/Jazz", &[""])).unwrap();
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
}
