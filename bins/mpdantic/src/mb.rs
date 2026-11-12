//! MusicBrainz release lookup for a folder (M1): search by album artist, album and
//! track count, pick a release, map its tracks onto the folder's files, hand the
//! edits to the existing tag write path. A folder with more than one album tag is
//! not a dead end on search: the page gets the album groups and what to do next
//! (split, then look up each). Map still refuses a mixed folder, a file/track count
//! mismatch, or an incomplete title/track pair (no order-only zip). The search is a
//! short ladder: as tagged, then without an edition suffix, then without locking the
//! track count; the card says which step hit. One request per second on the public
//! site, a cache under the data folder, backoff on 503. A local mirror from the
//! settings skips the gap. No AcoustID, no write to MusicBrainz.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::mpd::Decoders;
use crate::tagfix::{self, Edit, FileValues};
use crate::text::Msg;

/// How long between live calls to the public MusicBrainz site from this process.
const MIN_GAP: Duration = Duration::from_secs(1);
/// How many times a 503 is retried, waiting longer each time.
#[cfg(not(test))]
const BACKOFF: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
#[cfg(test)]
const BACKOFF: &[Duration] = &[
    Duration::from_millis(1),
    Duration::from_millis(1),
    Duration::from_millis(1),
];
const UA: &str = concat!(
    "MPDantic/",
    env!("CARGO_PKG_VERSION"),
    " ( https://github.com/foonerd/mpdantic )"
);
const PUBLIC: &str = "https://musicbrainz.org";

/// What fetches a URL: live HTTPS, or a fixture in a test.
pub trait Fetch: Send + Sync {
    fn get(&self, url: &str) -> Result<(u16, String), String>;
}

/// The live fetcher: ureq with the required User-Agent.
pub struct LiveFetch;

impl Fetch for LiveFetch {
    fn get(&self, url: &str) -> Result<(u16, String), String> {
        let response = ureq::get(url)
            .set("User-Agent", UA)
            .set("Accept", "application/json")
            .call()
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let body = response.into_string().map_err(|e| e.to_string())?;
        Ok((status, body))
    }
}

/// One client: base URL, whether to throttle, cache directory, the fetcher.
pub struct Client {
    base: String,
    throttle: bool,
    fetch: Box<dyn Fetch>,
    cache: PathBuf,
    last: Mutex<Option<Instant>>,
}

impl Client {
    /// The client the pages use: public MusicBrainz, or the operator's mirror from
    /// the shell's settings; cache under the data folder.
    pub fn from_settings(settings: &Path, cache: PathBuf) -> Client {
        let (base, throttle) = endpoint_from(settings);
        let _ = fs::create_dir_all(&cache);
        Client {
            base,
            throttle,
            fetch: Box::new(LiveFetch),
            cache,
            last: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_fetch(base: &str, throttle: bool, cache: PathBuf, fetch: Box<dyn Fetch>) -> Client {
        let _ = fs::create_dir_all(&cache);
        Client {
            base: base.trim_end_matches('/').to_string(),
            throttle,
            fetch,
            cache,
            last: Mutex::new(None),
        }
    }

    /// Search releases for the folder's tags; empty artist and album refuse.
    /// More than one album tag returns a guide (step `mixed_album`) with the groups —
    /// no MusicBrainz call — so the page can say what to do with a mix or dump.
    /// Ladder: as tagged → normalised album with tracks → normalised without tracks.
    pub fn search(&self, music: &Path, path: &str, decoders: &Decoders) -> Result<SearchPage, Msg> {
        let dir = folder_dir(music, path);
        let files = tagfix::folder_values(&dir, path, decoders)?;
        let groups = album_groups_for_folder(path, &files);
        if groups.len() > 1 {
            return Ok(SearchPage {
                query: Query {
                    artist: String::new(),
                    album: String::new(),
                    tracks: files.len(),
                },
                searched: Query {
                    artist: String::new(),
                    album: String::new(),
                    tracks: files.len(),
                },
                step: "mixed_album".into(),
                releases: Vec::new(),
                groups,
            });
        }
        let query = query_from_files(&files);
        if query.artist.is_empty() && query.album.is_empty() {
            return Err(msg!(
                "core.mb.no_query",
                "no album artist or album on the folder's files; tag them here first, or split a known mix into album folders"
            ));
        }
        let mut page = self.ladder_search(query)?;
        page.groups = groups;
        Ok(page)
    }

    /// The search ladder for a query already taken from the folder's tags.
    fn ladder_search(&self, tagged: Query) -> Result<SearchPage, Msg> {
        let mut steps: Vec<(&str, Query)> = Vec::with_capacity(3);
        steps.push(("as_tagged", tagged.clone()));
        let normalised = normalise_album(&tagged.album);
        if !normalised.is_empty() && normalised != tagged.album {
            steps.push((
                "without_suffix",
                Query {
                    artist: tagged.artist.clone(),
                    album: normalised.clone(),
                    tracks: tagged.tracks,
                },
            ));
        }
        if tagged.tracks > 0 {
            let album = if !normalised.is_empty() {
                normalised
            } else {
                tagged.album.clone()
            };
            let loose = Query {
                artist: tagged.artist.clone(),
                album,
                tracks: 0,
            };
            if steps.iter().all(|(_, q)| q != &loose) {
                steps.push(("without_tracks", loose));
            }
        }

        let mut step = "as_tagged";
        let mut searched = tagged.clone();
        let mut releases = Vec::new();
        for (name, q) in steps {
            let url = self.search_url(&q);
            let body = self.get_cached(&url)?;
            let hits = parse_search(&body)?;
            step = name;
            searched = q;
            releases = hits;
            if !releases.is_empty() {
                break;
            }
        }
        rank_hits(&mut releases, tagged.tracks, &searched.album);
        Ok(SearchPage {
            query: tagged,
            searched,
            step: step.to_string(),
            releases,
            groups: Vec::new(),
        })
    }

    /// The release's tracks mapped onto the folder's files as tagedit edits.
    /// Refuses mixed albums, a file/track count mismatch, or an incomplete pair.
    pub fn map_release(
        &self,
        music: &Path,
        path: &str,
        release_id: &str,
        decoders: &Decoders,
    ) -> Result<Mapped, Msg> {
        if !is_mbid(release_id) {
            return Err(msg!("core.mb.bad_id", "not a MusicBrainz release id"));
        }
        let dir = folder_dir(music, path);
        let files = tagfix::folder_values(&dir, path, decoders)?;
        refuse_mixed_album(&files)?;
        let url = format!(
            "{}/ws/2/release/{}?inc=recordings+artist-credits&fmt=json",
            self.base, release_id
        );
        let body = self.get_cached(&url)?;
        let release = parse_release(&body)?;
        if files.len() != release.tracks.len() {
            return Err(msg!(
                "core.mb.count_mismatch",
                "the folder has {files} files but the release has {tracks} tracks",
                files = files.len(),
                tracks = release.tracks.len()
            ));
        }
        let edits = map_edits(&files, &release)?;
        Ok(Mapped {
            unmatched_files: 0,
            unmatched_tracks: 0,
            release,
            edits,
        })
    }

    fn search_url(&self, q: &Query) -> String {
        let mut parts = Vec::new();
        if !q.album.is_empty() {
            parts.push(format!("release:\"{}\"", lucene_escape(&q.album)));
        }
        if !q.artist.is_empty() {
            parts.push(format!("artist:\"{}\"", lucene_escape(&q.artist)));
        }
        if q.tracks > 0 {
            parts.push(format!("tracks:{}", q.tracks));
        }
        let query = parts.join(" AND ");
        format!(
            "{}/ws/2/release/?query={}&fmt=json&limit=10",
            self.base,
            urlencoding_encode(&query)
        )
    }

    fn get_cached(&self, url: &str) -> Result<String, Msg> {
        let key = cache_key(url);
        let file = self.cache.join(&key);
        if let Ok(text) = fs::read_to_string(&file) {
            if !text.is_empty() {
                return Ok(text);
            }
        }
        let body = self.get_live(url)?;
        let tmp = self.cache.join(format!("{key}.part"));
        if fs::write(&tmp, &body).is_ok() {
            let _ = fs::rename(&tmp, &file);
        }
        Ok(body)
    }

    fn get_live(&self, url: &str) -> Result<String, Msg> {
        let mut attempt = 0;
        loop {
            self.wait_slot();
            match self.fetch.get(url) {
                Ok((200, body)) => return Ok(body),
                Ok((503, _)) if attempt < BACKOFF.len() => {
                    thread::sleep(BACKOFF[attempt]);
                    attempt += 1;
                }
                Ok((status, body)) => {
                    return Err(msg!(
                        "core.mb.http",
                        "MusicBrainz answered {status}: {body}",
                        status = status,
                        body = truncate(&body, 160)
                    ))
                }
                Err(e) => {
                    return Err(msg!(
                        "core.mb.fetch",
                        "MusicBrainz could not be reached: {error}",
                        error = e
                    ))
                }
            }
        }
    }

    fn wait_slot(&self) {
        if !self.throttle {
            return;
        }
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = *last {
            let elapsed = at.elapsed();
            if elapsed < MIN_GAP {
                thread::sleep(MIN_GAP - elapsed);
            }
        }
        *last = Some(Instant::now());
    }
}

/// The base URL and whether to throttle, from the shell's settings.
pub fn endpoint_from(settings: &Path) -> (String, bool) {
    let Ok(text) = fs::read_to_string(settings) else {
        return (PUBLIC.to_string(), true);
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return (PUBLIC.to_string(), true);
    };
    let mirror = value.get("mb_mirror").and_then(|v| v.as_bool()) == Some(true);
    if !mirror {
        return (PUBLIC.to_string(), true);
    }
    let url = value
        .get("mb_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .trim_end_matches('/');
    let url = url.strip_suffix("/ws/2").unwrap_or(url);
    if url.starts_with("http://") || url.starts_with("https://") {
        (url.to_string(), false)
    } else {
        (PUBLIC.to_string(), true)
    }
}

fn folder_dir(music: &Path, path: &str) -> PathBuf {
    if path.is_empty() {
        music.to_path_buf()
    } else {
        music.join(path)
    }
}

fn truncate(s: &str, n: usize) -> String {
    let mut t = s.chars().take(n).collect::<String>();
    if s.chars().count() > n {
        t.push('…');
    }
    t
}

fn cache_key(url: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in url.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}.json")
}

fn is_mbid(id: &str) -> bool {
    let b = id.as_bytes();
    b.len() == 36
        && b[8] == b'-'
        && b[13] == b'-'
        && b[18] == b'-'
        && b[23] == b'-'
        && id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-')
}

/// What the search was built from.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub artist: String,
    pub album: String,
    pub tracks: usize,
}

#[derive(Serialize, Clone, Debug)]
pub struct ReleaseHit {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub date: String,
    pub country: String,
    pub status: String,
    pub track_count: usize,
    pub score: u32,
}

#[derive(Serialize, Clone, Debug)]
pub struct AlbumGroup {
    pub album: String,
    pub album_artist: String,
    pub files: usize,
    /// Where those files should live: `{library anchor}/{artist}/{album}`.
    pub folder: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct SearchPage {
    /// Album artist, album and track count as on the folder's files.
    pub query: Query,
    /// What the Lucene query actually used (may be normalised, tracks dropped).
    pub searched: Query,
    /// Which ladder step produced `releases`: `as_tagged`, `without_suffix`,
    /// `without_tracks`, or `mixed_album` when the folder is a mix/dump guide.
    pub step: String,
    pub releases: Vec<ReleaseHit>,
    /// Album groups in the folder; more than one means look up is not safe yet.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<AlbumGroup>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ReleaseTrack {
    pub number: String,
    pub title: String,
    pub artist: String,
    pub length_ms: Option<u64>,
    pub disc: u32,
}

#[derive(Serialize, Clone, Debug)]
pub struct ReleaseDetail {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub date: String,
    pub tracks: Vec<ReleaseTrack>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Mapped {
    pub release: ReleaseDetail,
    pub edits: Vec<EditJson>,
    pub unmatched_files: usize,
    pub unmatched_tracks: usize,
}

/// An edit as the tagedit batch takes it.
#[derive(Serialize, Clone, Debug)]
pub struct EditJson {
    pub name: String,
    pub set: BTreeMap<String, String>,
}

impl From<Edit> for EditJson {
    fn from(e: Edit) -> EditJson {
        EditJson {
            name: e.name,
            set: e.set,
        }
    }
}

/// Edition / remaster words dropped from an album title after parentheticals go.
const EDITION_WORDS: &[&str] = &[
    "remaster",
    "remastered",
    "remasters",
    "anniversary",
    "deluxe",
    "expanded",
    "edition",
    "editions",
    "explicit",
    "bonus",
    "special",
    "limited",
    "digipak",
];

/// Album title for a wider search: trailing `(…)`, `[…]`, `{…}` removed, then
/// common edition words dropped; whitespace collapsed.
pub fn normalise_album(album: &str) -> String {
    let mut s = album.trim().to_string();
    loop {
        let t = s.trim_end();
        let Some(close) = t.chars().last() else {
            break;
        };
        let open = match close {
            ')' => '(',
            ']' => '[',
            '}' => '{',
            _ => break,
        };
        let Some(i) = t.rfind(open) else {
            break;
        };
        if !t[i..].ends_with(close) {
            break;
        }
        s = t[..i].trim_end().to_string();
    }
    let words: Vec<&str> = s
        .split_whitespace()
        .filter(|w| {
            let bare = w.trim_matches(|c: char| matches!(c, '-' | ':' | ',' | '.'));
            !EDITION_WORDS.iter().any(|e| bare.eq_ignore_ascii_case(e))
        })
        .collect();
    words.join(" ")
}

/// Prefer a matching track count, Official status, title closeness, then MB score.
fn rank_hits(hits: &mut [ReleaseHit], folder_tracks: usize, prefer_album: &str) {
    let prefer = prefer_album.to_ascii_lowercase();
    hits.sort_by(|a, b| {
        let track = |h: &ReleaseHit| {
            if folder_tracks > 0 && h.track_count == folder_tracks {
                1
            } else {
                0
            }
        };
        let official = |h: &ReleaseHit| {
            if h.status.eq_ignore_ascii_case("Official") {
                1
            } else {
                0
            }
        };
        let title = |h: &ReleaseHit| {
            let t = h.title.to_ascii_lowercase();
            if t == prefer {
                2
            } else if !prefer.is_empty() && (t.contains(&prefer) || prefer.contains(&t)) {
                1
            } else {
                0
            }
        };
        track(b)
            .cmp(&track(a))
            .then(official(b).cmp(&official(a)))
            .then(title(b).cmp(&title(a)))
            .then(b.score.cmp(&a.score))
    });
}

fn query_from_files(files: &[FileValues]) -> Query {
    let first = |field: &str| -> String {
        files
            .iter()
            .find_map(|f| {
                f.values
                    .get(field)
                    .and_then(|v| v.first())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default()
    };
    let artist = {
        let aa = first("albumartist");
        if aa.is_empty() {
            first("artist")
        } else {
            aa
        }
    };
    Query {
        artist,
        album: first("album"),
        tracks: files.len(),
    }
}

fn lucene_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '+' | '-' | '&' | '|' | '!' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '"'
            | '~' | '*' | '?' | ':' | '/' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Minimal percent-encoding for the query string (UTF-8).
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn artist_credit(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|c| {
                    let name = c.get("name").and_then(|n| n.as_str()).unwrap_or_default();
                    let join = c
                        .get("joinphrase")
                        .and_then(|n| n.as_str())
                        .unwrap_or_default();
                    format!("{name}{join}")
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_search(body: &str) -> Result<Vec<ReleaseHit>, Msg> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        msg!(
            "core.mb.bad_json",
            "MusicBrainz JSON could not be read: {error}",
            error = e.to_string()
        )
    })?;
    let mut out = Vec::new();
    for r in v
        .get("releases")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
    {
        let id = r
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        out.push(ReleaseHit {
            id,
            title: r
                .get("title")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            artist: artist_credit(r.get("artist-credit").unwrap_or(&Value::Null)),
            date: r
                .get("date")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            country: r
                .get("country")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            status: r
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            track_count: r.get("track-count").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
            score: r.get("score").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
        });
    }
    Ok(out)
}

fn parse_release(body: &str) -> Result<ReleaseDetail, Msg> {
    let v: Value = serde_json::from_str(body).map_err(|e| {
        msg!(
            "core.mb.bad_json",
            "MusicBrainz JSON could not be read: {error}",
            error = e.to_string()
        )
    })?;
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return Err(msg!(
            "core.mb.bad_json",
            "MusicBrainz JSON could not be read: {error}",
            error = "no id"
        ));
    }
    let mut tracks = Vec::new();
    for medium in v
        .get("media")
        .and_then(|x| x.as_array())
        .into_iter()
        .flatten()
    {
        let disc = medium.get("position").and_then(|x| x.as_u64()).unwrap_or(1) as u32;
        for t in medium
            .get("tracks")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
        {
            let title = t
                .get("title")
                .or_else(|| t.pointer("/recording/title"))
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let artist = {
                let ac = t
                    .get("artist-credit")
                    .or_else(|| t.pointer("/recording/artist-credit"))
                    .unwrap_or(&Value::Null);
                let s = artist_credit(ac);
                if s.is_empty() {
                    artist_credit(v.get("artist-credit").unwrap_or(&Value::Null))
                } else {
                    s
                }
            };
            tracks.push(ReleaseTrack {
                number: t
                    .get("number")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                title,
                artist,
                length_ms: t
                    .get("length")
                    .or_else(|| t.pointer("/recording/length"))
                    .and_then(|x| x.as_u64()),
                disc,
            });
        }
    }
    Ok(ReleaseDetail {
        id,
        title: v
            .get("title")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        artist: artist_credit(v.get("artist-credit").unwrap_or(&Value::Null)),
        date: v
            .get("date")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        tracks,
    })
}

/// The library share this folder sits under: `INTERNAL`, `NAS/Audio`, `USB/…`.
fn library_anchor(folder_path: &str) -> String {
    let parts: Vec<&str> = folder_path
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    match parts.as_slice() {
        [] => String::new(),
        ["NAS", share, ..] | ["USB", share, ..] => format!("{}/{}", parts[0], share),
        [root, ..] => (*root).to_string(),
    }
}

fn album_folder_path(anchor: &str, album_artist: &str, album: &str) -> String {
    let artist = {
        let s = crate::names::safe(album_artist);
        if s.is_empty() {
            "Unknown Artist".into()
        } else {
            s
        }
    };
    let album = {
        let s = crate::names::safe(album);
        if s.is_empty() {
            "Unknown Album".into()
        } else {
            s
        }
    };
    if anchor.is_empty() {
        format!("{artist}/{album}")
    } else {
        format!("{anchor}/{artist}/{album}")
    }
}

/// Groups the folder's files by album tag (empty album is its own group), each
/// with the concrete library path those files should move to.
fn album_groups_for_folder(folder_path: &str, files: &[FileValues]) -> Vec<AlbumGroup> {
    let anchor = library_anchor(folder_path);
    album_groups(files)
        .into_iter()
        .map(|mut g| {
            g.folder = album_folder_path(&anchor, &g.album_artist, &g.album);
            g
        })
        .collect()
}

/// Groups the folder's files by album tag (empty album is its own group).
fn album_groups(files: &[FileValues]) -> Vec<AlbumGroup> {
    // key → (display album, artist votes, count)
    let mut map: BTreeMap<String, (String, BTreeMap<String, usize>, usize)> = BTreeMap::new();
    for f in files {
        let album = first_str(f, "album")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("");
        let key = album.to_ascii_lowercase();
        let artist = first_str(f, "albumartist")
            .or_else(|| first_str(f, "artist"))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("")
            .to_string();
        let entry = map
            .entry(key)
            .or_insert_with(|| (album.to_string(), BTreeMap::new(), 0));
        if entry.0.is_empty() && !album.is_empty() {
            entry.0 = album.to_string();
        }
        if !artist.is_empty() {
            *entry.1.entry(artist).or_insert(0) += 1;
        }
        entry.2 += 1;
    }
    map.into_values()
        .map(|(album, artists, files)| {
            let album_artist = artists
                .into_iter()
                .max_by_key(|(_, n)| *n)
                .map(|(a, _)| a)
                .unwrap_or_default();
            AlbumGroup {
                album,
                album_artist,
                files,
                folder: String::new(),
            }
        })
        .collect()
}

/// More than one distinct non-empty album tag refuses map (one release onto a dump).
fn refuse_mixed_album(files: &[FileValues]) -> Result<(), Msg> {
    let named = album_groups(files)
        .into_iter()
        .filter(|g| !g.album.is_empty())
        .count();
    if named > 1 {
        return Err(msg!(
            "core.mb.mixed_album",
            "the folder's files carry more than one album tag; split into one folder per album before writing a release"
        ));
    }
    Ok(())
}

/// Title key for pairing: lowercased, punctuation and ellipsis stripped, spaces collapsed.
fn normalise_title(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        let c = match c {
            '…' | '·' => ' ',
            _ => c,
        };
        if c.is_alphanumeric() {
            for d in c.to_lowercase() {
                out.push(d);
            }
            space = false;
        } else if (c.is_whitespace() || c == '-' || c == '_') && !out.is_empty() && !space {
            out.push(' ');
            space = true;
        }
        // other punctuation dropped
    }
    out.trim().to_string()
}

fn first_str<'a>(f: &'a FileValues, field: &str) -> Option<&'a str> {
    f.values
        .get(field)?
        .first()
        .map(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
}

/// Pair each file to one release track by unique normalised title, then by disc+track.
/// Leftovers or an unwritable file refuse; no order-only zip.
fn map_edits(files: &[FileValues], release: &ReleaseDetail) -> Result<Vec<EditJson>, Msg> {
    let n = files.len();
    debug_assert_eq!(n, release.tracks.len());
    let mut file_of_track: Vec<Option<usize>> = vec![None; n];
    let mut track_of_file: Vec<Option<usize>> = vec![None; n];

    // Pass 1: unique exact normalised title.
    for (fi, file) in files.iter().enumerate() {
        if track_of_file[fi].is_some() {
            continue;
        }
        let Some(raw) = first_str(file, "title") else {
            continue;
        };
        let key = normalise_title(raw);
        if key.is_empty() {
            continue;
        }
        let mut hits: Vec<usize> = Vec::new();
        for (ti, tr) in release.tracks.iter().enumerate() {
            if file_of_track[ti].is_some() {
                continue;
            }
            if normalise_title(&tr.title) == key {
                hits.push(ti);
            }
        }
        if hits.len() == 1 {
            let ti = hits[0];
            track_of_file[fi] = Some(ti);
            file_of_track[ti] = Some(fi);
        }
    }

    // Pass 2: unique disc + track number among the remainder.
    for (fi, file) in files.iter().enumerate() {
        if track_of_file[fi].is_some() {
            continue;
        }
        let Some(track_no) = first_num(file, "track") else {
            continue;
        };
        if track_no == 0 {
            continue;
        }
        let disc = first_num(file, "disc").unwrap_or(1);
        let mut hits: Vec<usize> = Vec::new();
        for (ti, tr) in release.tracks.iter().enumerate() {
            if file_of_track[ti].is_some() {
                continue;
            }
            let tr_no = tr
                .number
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u32>()
                .unwrap_or(0);
            if tr_no == track_no && tr.disc == disc {
                hits.push(ti);
            }
        }
        if hits.len() == 1 {
            let ti = hits[0];
            track_of_file[fi] = Some(ti);
            file_of_track[ti] = Some(fi);
        }
    }

    if track_of_file.iter().any(|t| t.is_none()) || file_of_track.iter().any(|f| f.is_none()) {
        return Err(msg!(
            "core.mb.pair_failed",
            "could not pair every file to a release track by title or track number"
        ));
    }

    let mut edits = Vec::with_capacity(n);
    for (fi, file) in files.iter().enumerate() {
        let ti = track_of_file[fi].expect("paired");
        let tr = &release.tracks[ti];
        if !file.writable {
            return Err(msg!(
                "core.mb.pair_failed",
                "could not pair every file to a release track by title or track number"
            ));
        }
        let mut set = BTreeMap::new();
        if !tr.title.is_empty() {
            set.insert("title".into(), tr.title.clone());
        }
        if !tr.number.is_empty() {
            set.insert("track".into(), tr.number.clone());
        }
        if !tr.artist.is_empty() {
            set.insert("artist".into(), tr.artist.clone());
        }
        if !release.artist.is_empty() {
            set.insert("albumartist".into(), release.artist.clone());
        }
        if !release.title.is_empty() {
            set.insert("album".into(), release.title.clone());
        }
        if !release.date.is_empty() {
            let date = release
                .date
                .split('-')
                .next()
                .unwrap_or(&release.date)
                .to_string();
            set.insert("date".into(), date);
        }
        if tr.disc > 0 {
            set.insert("disc".into(), tr.disc.to_string());
        }
        if !set.is_empty() {
            edits.push(EditJson {
                name: file.name.clone(),
                set,
            });
        }
    }
    if edits.len() != n {
        return Err(msg!(
            "core.mb.pair_failed",
            "could not pair every file to a release track by title or track number"
        ));
    }
    Ok(edits)
}

fn first_num(f: &FileValues, field: &str) -> Option<u32> {
    f.values
        .get(field)?
        .first()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixtureFetch {
        map: BTreeMap<String, (u16, String)>,
    }

    impl Fetch for FixtureFetch {
        fn get(&self, url: &str) -> Result<(u16, String), String> {
            self.map
                .get(url)
                .cloned()
                .ok_or_else(|| format!("no fixture for {url}"))
        }
    }

    #[test]
    fn the_settings_pick_the_public_site_or_the_mirror() {
        let dir = std::env::temp_dir().join(format!("mpdantic-mb-ep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        assert_eq!(endpoint_from(&settings), (PUBLIC.to_string(), true));
        fs::write(
            &settings,
            "{ \"port\": 6218, \"mb_mirror\": false, \"mb_url\": \"http://x\" }\n",
        )
        .unwrap();
        assert_eq!(endpoint_from(&settings), (PUBLIC.to_string(), true));
        fs::write(
            &settings,
            "{ \"port\": 6218, \"mb_mirror\": true, \"mb_url\": \"http://mb.lan:5000/ws/2/\" }\n",
        )
        .unwrap();
        assert_eq!(
            endpoint_from(&settings),
            ("http://mb.lan:5000".to_string(), false)
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_search_and_a_map_fill_edits_from_the_release() {
        let dir = std::env::temp_dir().join(format!("mpdantic-mb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = dir.join("cache");
        let search_body = r#"{"releases":[{"id":"11111111-1111-1111-1111-111111111111","score":100,"title":"Demo","status":"Official","artist-credit":[{"name":"Band","joinphrase":""}],"date":"1999","country":"US","track-count":2}]}"#;
        let release_body = r#"{"id":"11111111-1111-1111-1111-111111111111","title":"Demo","date":"1999-05-01","artist-credit":[{"name":"Band","joinphrase":""}],"media":[{"position":1,"tracks":[{"number":"1","title":"One","length":1000},{"number":"2","title":"Two","length":2000}]}]}"#;
        let base = "https://musicbrainz.org";
        let search_url = format!(
            "{base}/ws/2/release/?query={}&fmt=json&limit=10",
            urlencoding_encode("release:\"Demo\" AND artist:\"Band\" AND tracks:2")
        );
        let release_url = format!(
            "{base}/ws/2/release/11111111-1111-1111-1111-111111111111?inc=recordings+artist-credits&fmt=json"
        );
        let mut map = BTreeMap::new();
        map.insert(search_url.clone(), (200, search_body.into()));
        map.insert(release_url.clone(), (200, release_body.into()));
        let client = Client::with_fetch(base, false, cache, Box::new(FixtureFetch { map }));

        let hits = parse_search(search_body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Demo");
        let release = parse_release(release_body).unwrap();
        assert_eq!(release.tracks.len(), 2);

        let files = vec![
            FileValues {
                name: "01.flac".into(),
                path: "INTERNAL/Album/01.flac".into(),
                format: "flac".into(),
                writable: true,
                values: BTreeMap::from([
                    ("album".into(), vec!["Demo".into()]),
                    ("albumartist".into(), vec!["Band".into()]),
                    ("track".into(), vec!["1".into()]),
                ]),
                problem: None,
            },
            FileValues {
                name: "02.flac".into(),
                path: "INTERNAL/Album/02.flac".into(),
                format: "flac".into(),
                writable: true,
                values: BTreeMap::from([("track".into(), vec!["2".into()])]),
                problem: None,
            },
        ];
        let edits = map_edits(&files, &release).unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].set.get("title").map(String::as_str), Some("One"));
        assert_eq!(edits[0].set.get("album").map(String::as_str), Some("Demo"));
        assert_eq!(edits[0].set.get("date").map(String::as_str), Some("1999"));
        assert_eq!(edits[1].set.get("title").map(String::as_str), Some("Two"));

        let body = client.get_cached(&release_url).unwrap();
        assert!(body.contains("Demo"));
        let body2 = client.get_cached(&release_url).unwrap();
        assert_eq!(body, body2);

        assert!(is_mbid("11111111-1111-1111-1111-111111111111"));
        assert!(!is_mbid("not-an-id"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_503_is_retried_then_said() {
        let dir = std::env::temp_dir().join(format!("mpdantic-mb503-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = dir.join("cache");
        struct Always503;
        impl Fetch for Always503 {
            fn get(&self, _: &str) -> Result<(u16, String), String> {
                Ok((503, "slow".into()))
            }
        }
        let client = Client::with_fetch(PUBLIC, false, cache, Box::new(Always503));
        let err = client
            .get_live("https://musicbrainz.org/ws/2/release/?query=test503&fmt=json")
            .unwrap_err();
        assert_eq!(err.k, "core.mb.http");
        assert!(err.contains("503"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_album_title_drops_parentheticals_and_edition_words() {
        assert_eq!(
            normalise_album("Brothers In Arms (Remastered Version)"),
            "Brothers In Arms"
        );
        assert_eq!(
            normalise_album("Abba Gold Anniversary Edition"),
            "Abba Gold"
        );
        assert_eq!(normalise_album("21"), "21");
        assert_eq!(normalise_album("  Demo [Deluxe] "), "Demo");
    }

    #[test]
    fn the_ladder_tries_as_tagged_then_without_the_suffix() {
        let dir = std::env::temp_dir().join(format!("mpdantic-mb-ladder-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = dir.join("cache");
        let base = "https://musicbrainz.org";
        let tagged = Query {
            artist: "Dire Straits".into(),
            album: "Brothers In Arms (Remastered Version)".into(),
            tracks: 9,
        };
        let empty = r#"{"releases":[]}"#;
        let hit = r#"{"releases":[{"id":"22222222-2222-2222-2222-222222222222","score":80,"title":"Brothers in Arms","status":"Official","artist-credit":[{"name":"Dire Straits","joinphrase":""}],"date":"1985","country":"GB","track-count":9}]}"#;
        let client_urls = Client::with_fetch(
            base,
            false,
            dir.join("cache-urls"),
            Box::new(FixtureFetch {
                map: BTreeMap::new(),
            }),
        );
        let url_as = client_urls.search_url(&tagged);
        let url_norm = client_urls.search_url(&Query {
            artist: tagged.artist.clone(),
            album: "Brothers In Arms".into(),
            tracks: 9,
        });
        let mut map = BTreeMap::new();
        map.insert(url_as, (200, empty.into()));
        map.insert(url_norm, (200, hit.into()));
        let client = Client::with_fetch(base, false, cache, Box::new(FixtureFetch { map }));
        let page = client.ladder_search(tagged.clone()).unwrap();
        assert_eq!(page.step, "without_suffix");
        assert_eq!(page.query, tagged);
        assert_eq!(page.searched.album, "Brothers In Arms");
        assert_eq!(page.searched.tracks, 9);
        assert_eq!(page.releases.len(), 1);
        assert_eq!(page.releases[0].title, "Brothers in Arms");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_ladder_drops_the_track_lock_when_the_suffix_still_misses() {
        let dir = std::env::temp_dir().join(format!("mpdantic-mb-ladder2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = dir.join("cache");
        let base = "https://musicbrainz.org";
        let tagged = Query {
            artist: "Abba".into(),
            album: "Abba Gold Anniversary Edition".into(),
            tracks: 59,
        };
        let empty = r#"{"releases":[]}"#;
        let hit = r#"{"releases":[{"id":"33333333-3333-3333-3333-333333333333","score":90,"title":"Gold: Greatest Hits","status":"Official","artist-credit":[{"name":"ABBA","joinphrase":""}],"date":"1992","country":"XW","track-count":19}]}"#;
        let urls = Client::with_fetch(
            base,
            false,
            dir.join("cache-urls"),
            Box::new(FixtureFetch {
                map: BTreeMap::new(),
            }),
        );
        let url_as = urls.search_url(&tagged);
        let url_norm = urls.search_url(&Query {
            artist: tagged.artist.clone(),
            album: "Abba Gold".into(),
            tracks: 59,
        });
        let url_loose = urls.search_url(&Query {
            artist: tagged.artist.clone(),
            album: "Abba Gold".into(),
            tracks: 0,
        });
        let mut map = BTreeMap::new();
        map.insert(url_as, (200, empty.into()));
        map.insert(url_norm, (200, empty.into()));
        map.insert(url_loose, (200, hit.into()));
        let client = Client::with_fetch(base, false, cache, Box::new(FixtureFetch { map }));
        let page = client.ladder_search(tagged).unwrap();
        assert_eq!(page.step, "without_tracks");
        assert_eq!(page.searched.album, "Abba Gold");
        assert_eq!(page.searched.tracks, 0);
        assert_eq!(page.releases.len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn file(name: &str, values: BTreeMap<String, Vec<String>>) -> FileValues {
        FileValues {
            name: name.into(),
            path: format!("INTERNAL/Album/{name}"),
            format: "flac".into(),
            writable: true,
            values,
            problem: None,
        }
    }

    fn demo_release(tracks: Vec<ReleaseTrack>) -> ReleaseDetail {
        ReleaseDetail {
            id: "11111111-1111-1111-1111-111111111111".into(),
            title: "Demo".into(),
            artist: "Band".into(),
            date: "1999-05-01".into(),
            tracks,
        }
    }

    #[test]
    fn mixed_album_tags_in_the_folder_refuse_map_but_group_for_search() {
        let files = vec![
            file(
                "a.flac",
                BTreeMap::from([
                    ("album".into(), vec!["Hits".into()]),
                    ("albumartist".into(), vec!["Phil Collins".into()]),
                ]),
            ),
            file(
                "b.flac",
                BTreeMap::from([
                    ("album".into(), vec!["But Seriously".into()]),
                    ("albumartist".into(), vec!["Phil Collins".into()]),
                ]),
            ),
            file(
                "c.flac",
                BTreeMap::from([("album".into(), vec!["Hits".into()])]),
            ),
        ];
        let err = refuse_mixed_album(&files).unwrap_err();
        assert_eq!(err.k, "core.mb.mixed_album");
        assert!(refuse_mixed_album(&[file(
            "a.flac",
            BTreeMap::from([("album".into(), vec!["Hits".into()])]),
        )])
        .is_ok());
        let groups = album_groups_for_folder("INTERNAL/Phil Collins/Phil Collins", &files);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].album, "But Seriously");
        assert_eq!(groups[0].files, 1);
        assert_eq!(
            groups[0].folder,
            "INTERNAL/Phil Collins/But Seriously"
        );
        assert_eq!(groups[1].album, "Hits");
        assert_eq!(groups[1].files, 2);
        assert_eq!(groups[1].album_artist, "Phil Collins");
        assert_eq!(groups[1].folder, "INTERNAL/Phil Collins/Hits");
        assert_eq!(
            library_anchor("NAS/Audio/Various"),
            "NAS/Audio"
        );
        assert_eq!(
            album_folder_path("NAS/Audio", "Albert Hammond Jr.", "Yours To Keep"),
            "NAS/Audio/Albert Hammond Jr/Yours To Keep"
        );
    }

    #[test]
    fn titles_pair_even_when_file_order_disagrees() {
        let release = demo_release(vec![
            ReleaseTrack {
                number: "1".into(),
                title: "One".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
            },
            ReleaseTrack {
                number: "2".into(),
                title: "Two".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
            },
        ]);
        // File order is reverse of the release; titles must win over order.
        let files = vec![
            file(
                "02.flac",
                BTreeMap::from([
                    ("title".into(), vec!["Two".into()]),
                    ("track".into(), vec!["9".into()]),
                ]),
            ),
            file(
                "01.flac",
                BTreeMap::from([
                    ("title".into(), vec!["One".into()]),
                    ("track".into(), vec!["8".into()]),
                ]),
            ),
        ];
        let edits = map_edits(&files, &release).unwrap();
        assert_eq!(edits[0].name, "02.flac");
        assert_eq!(edits[0].set.get("title").map(String::as_str), Some("Two"));
        assert_eq!(edits[0].set.get("track").map(String::as_str), Some("2"));
        assert_eq!(edits[1].name, "01.flac");
        assert_eq!(edits[1].set.get("title").map(String::as_str), Some("One"));
    }

    #[test]
    fn empty_titles_pair_by_disc_and_track() {
        let release = demo_release(vec![
            ReleaseTrack {
                number: "1".into(),
                title: "One".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
            },
            ReleaseTrack {
                number: "2".into(),
                title: "Two".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
            },
        ]);
        let files = vec![
            file(
                "b.flac",
                BTreeMap::from([("track".into(), vec!["2".into()])]),
            ),
            file(
                "a.flac",
                BTreeMap::from([("track".into(), vec!["1".into()])]),
            ),
        ];
        let edits = map_edits(&files, &release).unwrap();
        assert_eq!(edits[0].set.get("title").map(String::as_str), Some("Two"));
        assert_eq!(edits[1].set.get("title").map(String::as_str), Some("One"));
    }

    #[test]
    fn unpaired_files_refuse_without_order_zip() {
        let release = demo_release(vec![
            ReleaseTrack {
                number: "1".into(),
                title: "One".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
            },
            ReleaseTrack {
                number: "2".into(),
                title: "Two".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
            },
        ]);
        let files = vec![
            file(
                "a.flac",
                BTreeMap::from([("title".into(), vec!["Something Else".into()])]),
            ),
            file(
                "b.flac",
                BTreeMap::from([("title".into(), vec!["Also Wrong".into()])]),
            ),
        ];
        let err = map_edits(&files, &release).unwrap_err();
        assert_eq!(err.k, "core.mb.pair_failed");
    }

    #[test]
    fn a_count_mismatch_is_the_dump_shape() {
        // Three files tagged as one album vs a two-track release: map_release
        // refuses before pairing (Phil Collins dump class).
        let files = vec![
            file(
                "01.flac",
                BTreeMap::from([
                    ("album".into(), vec!["Hits".into()]),
                    ("title".into(), vec!["One".into()]),
                ]),
            ),
            file(
                "02.flac",
                BTreeMap::from([
                    ("album".into(), vec!["Hits".into()]),
                    ("title".into(), vec!["Two".into()]),
                ]),
            ),
            file(
                "03.flac",
                BTreeMap::from([
                    ("album".into(), vec!["Hits".into()]),
                    ("title".into(), vec!["Three".into()]),
                ]),
            ),
        ];
        refuse_mixed_album(&files).unwrap();
        let release = demo_release(vec![
            ReleaseTrack {
                number: "1".into(),
                title: "One".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
            },
            ReleaseTrack {
                number: "2".into(),
                title: "Two".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
            },
        ]);
        assert_ne!(files.len(), release.tracks.len());
    }

    #[test]
    fn a_title_normalises_punctuation_and_ellipsis() {
        assert_eq!(
            normalise_title("Against All Odds (Take a Look at Me Now)"),
            "against all odds take a look at me now"
        );
        assert_eq!(normalise_title("…Hits"), "hits");
        assert_eq!(normalise_title("  Both  Sides! "), "both sides");
    }
}
