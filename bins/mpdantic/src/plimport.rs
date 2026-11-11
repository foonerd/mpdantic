//! A playlist of the library imported as one of Volumio's own: the entries MPD can
//! load, each with its title, artist and album read from the file's tags, and the
//! streams, written as the JSON Volumio keeps under `/data/playlist/<name>`, in the
//! shape Volumio itself writes when a song is added to a playlist from its browser
//! (`app/playlistManager.js`, `commonAddToPlaylist`). An entry MPD cannot find is
//! left out and said.

use std::path::Path;

use serde::Serialize;

use crate::classify;
use crate::mpd::Decoders;
use crate::playlist;
use crate::plcheck;
use crate::tags;
use crate::text::Msg;

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ImportEntry {
    pub line: usize,
    pub uri: String,
    /// `song`, `stream`, `left_out`.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<Msg>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Imported {
    pub name: String,
    /// The JSON Volumio reads, compact as `fs.writeJson` writes it.
    pub after: String,
    pub entries: Vec<ImportEntry>,
    pub songs: usize,
    pub streams: usize,
    pub left_out: usize,
}

/// A playlist name Volumio keeps as a file name: trimmed, no slash, not hidden.
pub fn valid_name(name: &str) -> Result<String, Msg> {
    let name = name.trim();
    if name.is_empty() {
        return Err(msg!(
            "core.plimport.needs_name",
            "the playlist needs a name"
        ));
    }
    if name.contains('/') || name.contains('\0') {
        return Err(msg!(
            "core.plimport.slash",
            "a playlist name cannot hold a slash"
        ));
    }
    if name.starts_with('.') {
        return Err(msg!(
            "core.plimport.dot",
            "a playlist name cannot start with a dot"
        ));
    }
    if name.len() > 200 {
        return Err(msg!(
            "core.plimport.too_long",
            "the playlist name is too long"
        ));
    }
    Ok(name.to_string())
}

/// JavaScript's `encodeURIComponent`: everything percent-encoded but the letters,
/// the digits and `- _ . ! ~ * ' ( )`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn first(values: &[String]) -> Option<String> {
    values
        .first()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The playlist's entries as Volumio's playlist items: nothing written.
pub fn plan(
    music: &Path,
    path: &str,
    bytes: &[u8],
    decoders: &Decoders,
    name: Option<&str>,
) -> Result<Imported, Msg> {
    let suffix = classify::suffix(path).unwrap_or_default();
    let parsed = playlist::parse(bytes, &suffix);
    let folder = path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let stem = file_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .filter(|s| !s.is_empty())
        .unwrap_or(file_name);
    let name = valid_name(name.unwrap_or(stem))?;
    // Volumio's cache-buster is a counter it keeps; any number serves. One drawn from
    // the playlist's path, so the same playlist imported twice is the same bytes and
    // the preview says there is nothing to change.
    let cacheid = {
        let mut h: u32 = 0x811c_9dc5;
        for b in path.bytes() {
            h ^= u32::from(b);
            h = h.wrapping_mul(0x0100_0193);
        }
        h % 1_000_000
    };
    let mut out = Imported {
        name,
        ..Imported::default()
    };
    let mut items: Vec<serde_json::Value> = Vec::new();
    for e in &parsed.entries {
        let (state, resolved) = plcheck::state_of(music, &e.uri, folder, decoders);
        let mut entry = ImportEntry {
            line: e.line,
            uri: e.uri.clone(),
            state: "left_out",
            title: None,
            artist: None,
            album: None,
            why: None,
        };
        match (state, resolved) {
            ("ok", Some(lib)) => {
                let file_suffix = classify::suffix(&lib).unwrap_or_default();
                let seen = tags::view(&tags::read(&music.join(&lib), &file_suffix));
                let song_name = lib.rsplit('/').next().unwrap_or(&lib);
                let title = first(&seen.title)
                    .or_else(|| e.name.clone())
                    .unwrap_or_else(|| {
                        song_name
                            .rsplit_once('.')
                            .map(|(s, _)| s.to_string())
                            .unwrap_or_else(|| song_name.to_string())
                    });
                let artist = first(&seen.artist).or_else(|| first(&seen.album_artist));
                let album = first(&seen.album);
                let song_folder = lib.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                let web = artist
                    .as_ref()
                    .map(|a| {
                        format!(
                            "&web={}/{}/extralarge",
                            encode_uri_component(a),
                            encode_uri_component(album.as_deref().unwrap_or(""))
                        )
                    })
                    .unwrap_or_default();
                let albumart = format!(
                    "/albumart?cacheid={cacheid}{web}&path={}&icon=fa-tags&metadata=false",
                    encode_uri_component(&format!("/mnt/{song_folder}"))
                );
                let mut item = serde_json::Map::new();
                if let Some(a) = &album {
                    item.insert("album".into(), serde_json::Value::String(a.clone()));
                }
                item.insert("albumart".into(), serde_json::Value::String(albumart));
                if let Some(a) = &artist {
                    item.insert("artist".into(), serde_json::Value::String(a.clone()));
                }
                item.insert("service".into(), serde_json::Value::String("mpd".into()));
                item.insert("title".into(), serde_json::Value::String(title.clone()));
                item.insert(
                    "uri".into(),
                    serde_json::Value::String(format!("mnt/{lib}")),
                );
                items.push(serde_json::Value::Object(item));
                entry.state = "song";
                entry.title = Some(title);
                entry.artist = artist;
                entry.album = album;
                out.songs += 1;
            }
            ("stream", _) => {
                let title = e.name.clone().unwrap_or_else(|| e.uri.clone());
                items.push(serde_json::json!({
                    "icon": "fa fa-microphone",
                    "service": "webradio",
                    "title": title,
                    "uri": e.uri,
                }));
                entry.state = "stream";
                entry.title = Some(title);
                out.streams += 1;
            }
            (other, _) => {
                entry.why = Some(match other {
                    "missing" => msg!("core.plimport.left_missing", "not found where MPD looks"),
                    "not_decoded" => msg!(
                        "core.plimport.left_not_decoded",
                        "found, but no decoder claims it"
                    ),
                    _ => msg!(
                        "core.plimport.left_foreign",
                        "a foreign path; repair the playlist first"
                    ),
                });
                out.left_out += 1;
            }
        }
        out.entries.push(entry);
    }
    out.after = serde_json::Value::Array(items).to_string();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpd;
    use crate::tagfix::tests::real_flac;
    use std::fs;

    #[test]
    fn the_entries_become_volumios_items_with_their_tags_and_the_rest_is_left_out() {
        let root = std::env::temp_dir().join(format!("mpdantic-plimport-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("INTERNAL/Jazz/Live")).unwrap();
        fs::write(
            root.join("INTERNAL/Jazz/Live/01.flac"),
            real_flac(&[
                ("TITLE", "One & Only"),
                ("ARTIST", "Ann"),
                ("ALBUM", "Live at Caf\u{e9}"),
            ]),
        )
        .unwrap();
        fs::write(root.join("INTERNAL/Jazz/Live/02.flac"), real_flac(&[])).unwrap();
        let decoders = mpd::tests::decoders_of(&[("flac", &["flac"])]);
        let bytes = b"#EXTM3U\n#EXTINF:1,First\nLive/01.flac\n#EXTINF:1,Second\nLive/02.flac\nhttp://radio.example/live\nLive/gone.flac\nD:\\x.flac\n";
        let p = plan(&root, "INTERNAL/Jazz/mix.m3u8", bytes, &decoders, None).unwrap();
        assert_eq!(p.name, "mix");
        assert_eq!((p.songs, p.streams, p.left_out), (2, 1, 2));
        let states: Vec<&str> = p.entries.iter().map(|e| e.state).collect();
        assert_eq!(states, ["song", "song", "stream", "left_out", "left_out"]);
        assert_eq!(p.entries[0].title.as_deref(), Some("One & Only"));
        assert_eq!(p.entries[0].artist.as_deref(), Some("Ann"));
        assert_eq!(
            p.entries[1].title.as_deref(),
            Some("Second"),
            "the EXTINF name when the file has no title"
        );
        assert_eq!(
            p.entries[2].title.as_deref(),
            Some("http://radio.example/live")
        );
        assert!(p.entries[3].why.as_deref().unwrap().contains("not found"));
        assert!(p.entries[4].why.as_deref().unwrap().contains("foreign"));
        let items: serde_json::Value = serde_json::from_str(&p.after).unwrap();
        assert_eq!(items.as_array().unwrap().len(), 3);
        assert_eq!(items[0]["service"], "mpd");
        assert_eq!(items[0]["uri"], "mnt/INTERNAL/Jazz/Live/01.flac");
        assert_eq!(items[0]["title"], "One & Only");
        assert_eq!(items[0]["album"], "Live at Caf\u{e9}");
        let art = items[0]["albumart"].as_str().unwrap();
        assert!(art.starts_with("/albumart?cacheid="), "{art}");
        assert!(art.contains("&web=Ann/Live%20at%20Caf%C3%A9/extralarge&path=%2Fmnt%2FINTERNAL%2FJazz%2FLive&icon=fa-tags&metadata=false"), "{art}");
        assert!(items[1].get("artist").is_none());
        assert_eq!(items[2]["service"], "webradio");
        assert_eq!(items[2]["icon"], "fa fa-microphone");
        assert!(!p.after.contains('\n'), "compact, as Volumio writes it");
        assert_eq!(
            plan(&root, "INTERNAL/Jazz/mix.m3u8", bytes, &decoders, None)
                .unwrap()
                .after,
            p.after,
            "the same playlist is the same bytes again"
        );
        // The name: given, or the file's stem; refused when it would be a path.
        assert_eq!(
            plan(
                &root,
                "INTERNAL/Jazz/mix.m3u8",
                bytes,
                &decoders,
                Some("  Mix of mine ")
            )
            .unwrap()
            .name,
            "Mix of mine"
        );
        assert!(plan(
            &root,
            "INTERNAL/Jazz/mix.m3u8",
            bytes,
            &decoders,
            Some("a/b")
        )
        .is_err());
        assert!(plan(
            &root,
            "INTERNAL/Jazz/mix.m3u8",
            bytes,
            &decoders,
            Some(".hidden")
        )
        .is_err());
        assert!(plan(
            &root,
            "INTERNAL/Jazz/mix.m3u8",
            bytes,
            &decoders,
            Some("  ")
        )
        .is_err());
        assert_eq!(encode_uri_component("a b/c&d'e"), "a%20b%2Fc%26d'e");
        fs::remove_dir_all(&root).unwrap();
    }
}
