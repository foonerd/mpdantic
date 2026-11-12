//! MusicBrainz release lookup for a folder (M1): search by album artist, album and
//! track count, pick a release, map its tracks onto the folder's files, hand the
//! edits to the existing tag write path. A folder with more than one album tag is
//! not a dead end on search: the page gets the album groups and what to do next
//! (split, then look up each). Map refuses a mixed folder, or when nothing pairs by
//! title/track number (no order-only zip). A shorter or longer folder than the
//! release is allowed: paired files get edits; unmatched files and missing tracks
//! are reported as guidance. The search is a short ladder: as tagged, then a
//! `Last, First` sort-name flipped to display form, then without an edition
//! suffix, then without locking the track count. A shared `musicbrainz_albumid`
//! on the files short-circuits the ladder (`tagged_id`). The card says which
//! step hit.
//! One request per second on the public site, a cache under the data folder,
//! backoff on 503. A local mirror from the settings skips the gap. No AcoustID,
//! no write to MusicBrainz.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::mpd::Decoders;
use crate::organise::{self, AlbumGroup};
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
        let groups = organise::album_groups_for_folder(path, &files);
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
        if let Some(id) = album_id_from_files(&files) {
            match self.hit_from_release_id(&id) {
                Ok(hit) => {
                    return Ok(SearchPage {
                        query: query.clone(),
                        searched: query,
                        step: "tagged_id".into(),
                        releases: vec![hit],
                        groups,
                    });
                }
                Err(_) => {
                    // Stale or unreachable id: fall through to the text ladder.
                }
            }
        }
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

    /// One release by MBID as a search hit (warms the same cache map uses).
    fn hit_from_release_id(&self, release_id: &str) -> Result<ReleaseHit, Msg> {
        if !is_mbid(release_id) {
            return Err(msg!("core.mb.bad_id", "not a MusicBrainz release id"));
        }
        let url = format!(
            "{}/ws/2/release/{}?inc=recordings+artist-credits&fmt=json",
            self.base, release_id
        );
        let body = self.get_cached(&url)?;
        let detail = parse_release(&body)?;
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        Ok(ReleaseHit {
            id: detail.id,
            title: detail.title,
            artist: detail.artist,
            date: detail.date,
            country: v
                .get("country")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            status: v
                .get("status")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            track_count: detail.tracks.len(),
            score: 100,
        })
    }

    /// The search ladder for a query already taken from the folder's tags.
    fn ladder_search(&self, tagged: Query) -> Result<SearchPage, Msg> {
        let flip = artist_from_sort_name(&tagged.artist);
        let mut steps: Vec<(&str, Query)> = Vec::with_capacity(4);
        steps.push(("as_tagged", tagged.clone()));
        if let Some(ref display) = flip {
            steps.push((
                "sort_name",
                Query {
                    artist: display.clone(),
                    album: tagged.album.clone(),
                    tracks: tagged.tracks,
                },
            ));
        }
        // Later steps use the display form when the tag looked like a sort-name:
        // the tagged form already missed (or would) on MusicBrainz's artist field.
        let artist = flip.clone().unwrap_or_else(|| tagged.artist.clone());
        let normalised = normalise_album(&tagged.album);
        if !normalised.is_empty() && normalised != tagged.album {
            steps.push((
                "without_suffix",
                Query {
                    artist: artist.clone(),
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
                artist,
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
    /// Refuses mixed albums, or when nothing pairs.
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
        let plan = map_edits(&files, &release)?;
        Ok(Mapped {
            unmatched_files: plan.unmatched_files,
            unmatched_tracks: plan.unmatched_tracks,
            release,
            edits: plan.edits,
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
pub struct SearchPage {
    /// Album artist, album and track count as on the folder's files.
    pub query: Query,
    /// What the Lucene query actually used (may be normalised, tracks dropped).
    pub searched: Query,
    /// Which ladder step produced `releases`: `tagged_id`, `as_tagged`, `sort_name`,
    /// `without_suffix`, `without_tracks`, or `mixed_album` when the folder is a mix/dump guide.
    pub step: String,
    pub releases: Vec<ReleaseHit>,
    /// Album groups in the folder; more than one means look up is not safe yet.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<AlbumGroup>,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct ReleaseTrack {
    pub number: String,
    pub title: String,
    pub artist: String,
    pub length_ms: Option<u64>,
    pub disc: u32,
    /// MusicBrainz recording id (`musicbrainz_trackid`).
    pub recording_id: String,
    /// MusicBrainz release-track id (`musicbrainz_releasetrackid`).
    pub track_id: String,
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

/// Display form from a MusicBrainz-style sort-name (`Collins, Phil` →
/// `Phil Collins`). One comma only; the part after it must not look like a
/// band list (`Earth, Wind & Fire` stays put).
fn artist_from_sort_name(artist: &str) -> Option<String> {
    let artist = artist.trim();
    let (last, rest) = artist.split_once(',')?;
    let last = last.trim();
    let rest = rest.trim();
    if last.is_empty() || rest.is_empty() {
        return None;
    }
    if rest.contains(',') || rest.contains('&') || rest.contains(';') {
        return None;
    }
    let flipped = format!("{rest} {last}");
    if flipped == artist {
        return None;
    }
    Some(flipped)
}

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

/// One shared MusicBrainz release id on the folder's files, or none when absent or conflicting.
fn album_id_from_files(files: &[FileValues]) -> Option<String> {
    let mut found: Option<String> = None;
    for f in files {
        let Some(ids) = f.values.get("musicbrainz_albumid") else {
            continue;
        };
        for raw in ids {
            let id = raw.trim();
            if !is_mbid(id) {
                continue;
            }
            match &found {
                None => found = Some(id.to_string()),
                Some(prev) if prev == id => {}
                Some(_) => return None,
            }
        }
    }
    found
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
                recording_id: t
                    .pointer("/recording/id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                track_id: t
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
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

/// More than one distinct non-empty album tag refuses map (one release onto a dump).
fn refuse_mixed_album(files: &[FileValues]) -> Result<(), Msg> {
    if organise::mixed_named_albums(files) {
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

/// What map hands to Preview: the paired edits, and how many files / release tracks
/// could not be uniquely paired (guidance, not a refuse).
#[derive(Debug)]
struct MapPlan {
    edits: Vec<EditJson>,
    unmatched_files: usize,
    unmatched_tracks: usize,
}

/// Pair files to release tracks by unique normalised title, then by disc+track.
/// Paired writable files get edits; leftovers are counted. Refuses only when nothing
/// pairs. No order-only zip.
fn map_edits(files: &[FileValues], release: &ReleaseDetail) -> Result<MapPlan, Msg> {
    let mut file_of_track: Vec<Option<usize>> = vec![None; release.tracks.len()];
    let mut track_of_file: Vec<Option<usize>> = vec![None; files.len()];

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

    let mut edits = Vec::new();
    let mut unmatched_files = 0;
    for (fi, file) in files.iter().enumerate() {
        let Some(ti) = track_of_file[fi] else {
            unmatched_files += 1;
            continue;
        };
        if !file.writable {
            unmatched_files += 1;
            file_of_track[ti] = None;
            continue;
        }
        let tr = &release.tracks[ti];
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
        if !release.id.is_empty() {
            set.insert("musicbrainz_albumid".into(), release.id.clone());
        }
        if !tr.recording_id.is_empty() {
            set.insert("musicbrainz_trackid".into(), tr.recording_id.clone());
        }
        if !tr.track_id.is_empty() {
            set.insert("musicbrainz_releasetrackid".into(), tr.track_id.clone());
        }
        if set.is_empty() {
            unmatched_files += 1;
            file_of_track[ti] = None;
            continue;
        }
        edits.push(EditJson {
            name: file.name.clone(),
            set,
        });
    }
    let unmatched_tracks = file_of_track.iter().filter(|f| f.is_none()).count();
    if edits.is_empty() {
        return Err(msg!(
            "core.mb.pair_failed",
            "could not pair any file to a release track by title or track number"
        ));
    }
    Ok(MapPlan {
        edits,
        unmatched_files,
        unmatched_tracks,
    })
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
        let plan = map_edits(&files, &release).unwrap();
        assert_eq!(plan.edits.len(), 2);
        assert_eq!(plan.unmatched_files, 0);
        assert_eq!(plan.unmatched_tracks, 0);
        assert_eq!(
            plan.edits[0].set.get("title").map(String::as_str),
            Some("One")
        );
        assert_eq!(
            plan.edits[0].set.get("album").map(String::as_str),
            Some("Demo")
        );
        assert_eq!(
            plan.edits[0].set.get("date").map(String::as_str),
            Some("1999")
        );
        assert_eq!(
            plan.edits[1].set.get("title").map(String::as_str),
            Some("Two")
        );

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
    fn a_shared_album_id_on_the_files_is_taken_and_a_conflict_is_not() {
        let id = "11111111-1111-1111-1111-111111111111";
        let one = file(
            "01.flac",
            BTreeMap::from([("musicbrainz_albumid".into(), vec![id.into()])]),
        );
        let two = file(
            "02.flac",
            BTreeMap::from([("musicbrainz_albumid".into(), vec![id.into()])]),
        );
        assert_eq!(
            album_id_from_files(&[one.clone(), two]).as_deref(),
            Some(id)
        );
        let other = file(
            "03.flac",
            BTreeMap::from([(
                "musicbrainz_albumid".into(),
                vec!["22222222-2222-2222-2222-222222222222".into()],
            )]),
        );
        assert_eq!(album_id_from_files(&[one, other]), None);
        assert_eq!(
            album_id_from_files(&[file(
                "x.flac",
                BTreeMap::from([("musicbrainz_albumid".into(), vec!["not-an-id".into()])])
            )]),
            None
        );
    }

    #[test]
    fn a_tagged_release_id_becomes_a_search_hit() {
        let dir = std::env::temp_dir().join(format!("mpdantic-mb-tagged-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = dir.join("cache");
        let base = "https://musicbrainz.org";
        let id = "11111111-1111-1111-1111-111111111111";
        let release_body = r#"{"id":"11111111-1111-1111-1111-111111111111","title":"Demo","date":"1999-05-01","country":"US","status":"Official","artist-credit":[{"name":"Band","joinphrase":""}],"media":[{"position":1,"tracks":[{"number":"1","title":"One","length":1000},{"number":"2","title":"Two","length":2000}]}]}"#;
        let release_url =
            format!("{base}/ws/2/release/{id}?inc=recordings+artist-credits&fmt=json");
        let mut map = BTreeMap::new();
        map.insert(release_url, (200, release_body.into()));
        let client = Client::with_fetch(base, false, cache, Box::new(FixtureFetch { map }));
        let hit = client.hit_from_release_id(id).unwrap();
        assert_eq!(hit.id, id);
        assert_eq!(hit.title, "Demo");
        assert_eq!(hit.artist, "Band");
        assert_eq!(hit.track_count, 2);
        assert_eq!(hit.score, 100);
        assert_eq!(hit.country, "US");
        assert_eq!(hit.status, "Official");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_sort_name_flips_to_the_display_form() {
        assert_eq!(
            artist_from_sort_name("Collins, Phil").as_deref(),
            Some("Phil Collins")
        );
        assert_eq!(
            artist_from_sort_name("Beethoven, Ludwig van").as_deref(),
            Some("Ludwig van Beethoven")
        );
        assert_eq!(artist_from_sort_name("Phil Collins"), None);
        assert_eq!(artist_from_sort_name("Earth, Wind & Fire"), None);
        assert_eq!(artist_from_sort_name("Last, First, Extra"), None);
    }

    #[test]
    fn the_ladder_tries_sort_name_when_the_tag_is_last_comma_first() {
        let dir = std::env::temp_dir().join(format!("mpdantic-mb-sort-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = dir.join("cache");
        let base = "https://musicbrainz.org";
        let tagged = Query {
            artist: "Collins, Phil".into(),
            album: "Hits".into(),
            tracks: 16,
        };
        let empty = r#"{"releases":[]}"#;
        let hit = r#"{"releases":[{"id":"44444444-4444-4444-4444-444444444444","score":100,"title":"…Hits","status":"Official","artist-credit":[{"name":"Phil Collins","joinphrase":""}],"date":"1998","country":"XW","track-count":16}]}"#;
        let urls = Client::with_fetch(
            base,
            false,
            dir.join("cache-urls"),
            Box::new(FixtureFetch {
                map: BTreeMap::new(),
            }),
        );
        let url_as = urls.search_url(&tagged);
        let url_sort = urls.search_url(&Query {
            artist: "Phil Collins".into(),
            album: tagged.album.clone(),
            tracks: 16,
        });
        let mut map = BTreeMap::new();
        map.insert(url_as, (200, empty.into()));
        map.insert(url_sort, (200, hit.into()));
        let client = Client::with_fetch(base, false, cache, Box::new(FixtureFetch { map }));
        let page = client.ladder_search(tagged.clone()).unwrap();
        assert_eq!(page.step, "sort_name");
        assert_eq!(page.query, tagged);
        assert_eq!(page.searched.artist, "Phil Collins");
        assert_eq!(page.releases.len(), 1);
        assert_eq!(page.releases[0].artist, "Phil Collins");
        fs::remove_dir_all(&dir).unwrap();
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
    fn mixed_album_tags_in_the_folder_refuse_map() {
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
        ];
        let err = refuse_mixed_album(&files).unwrap_err();
        assert_eq!(err.k, "core.mb.mixed_album");
        assert!(refuse_mixed_album(&[file(
            "a.flac",
            BTreeMap::from([("album".into(), vec!["Hits".into()])]),
        )])
        .is_ok());
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
                recording_id: String::new(),
                track_id: String::new(),
            },
            ReleaseTrack {
                number: "2".into(),
                title: "Two".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: String::new(),
                track_id: String::new(),
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
        let plan = map_edits(&files, &release).unwrap();
        assert_eq!(plan.edits[0].name, "02.flac");
        assert_eq!(
            plan.edits[0].set.get("title").map(String::as_str),
            Some("Two")
        );
        assert_eq!(
            plan.edits[0].set.get("track").map(String::as_str),
            Some("2")
        );
        assert_eq!(plan.edits[1].name, "01.flac");
        assert_eq!(
            plan.edits[1].set.get("title").map(String::as_str),
            Some("One")
        );
    }

    #[test]
    fn map_writes_musicbrainz_ids_onto_each_paired_file() {
        let release = ReleaseDetail {
            id: "11111111-1111-1111-1111-111111111111".into(),
            title: "Demo".into(),
            artist: "Band".into(),
            date: "1999-05-01".into(),
            tracks: vec![ReleaseTrack {
                number: "1".into(),
                title: "One".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
                track_id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
            }],
        };
        let files = vec![file(
            "01.flac",
            BTreeMap::from([("title".into(), vec!["One".into()])]),
        )];
        let plan = map_edits(&files, &release).unwrap();
        assert_eq!(
            plan.edits[0]
                .set
                .get("musicbrainz_albumid")
                .map(String::as_str),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(
            plan.edits[0]
                .set
                .get("musicbrainz_trackid")
                .map(String::as_str),
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
        );
        assert_eq!(
            plan.edits[0]
                .set
                .get("musicbrainz_releasetrackid")
                .map(String::as_str),
            Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb")
        );
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
                recording_id: String::new(),
                track_id: String::new(),
            },
            ReleaseTrack {
                number: "2".into(),
                title: "Two".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: String::new(),
                track_id: String::new(),
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
        let plan = map_edits(&files, &release).unwrap();
        assert_eq!(
            plan.edits[0].set.get("title").map(String::as_str),
            Some("Two")
        );
        assert_eq!(
            plan.edits[1].set.get("title").map(String::as_str),
            Some("One")
        );
    }

    #[test]
    fn nothing_paired_refuses_without_order_zip() {
        let release = demo_release(vec![
            ReleaseTrack {
                number: "1".into(),
                title: "One".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: String::new(),
                track_id: String::new(),
            },
            ReleaseTrack {
                number: "2".into(),
                title: "Two".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: String::new(),
                track_id: String::new(),
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
    fn fewer_files_than_tracks_maps_what_pairs() {
        let release = demo_release(vec![
            ReleaseTrack {
                number: "1".into(),
                title: "One".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: String::new(),
                track_id: String::new(),
            },
            ReleaseTrack {
                number: "2".into(),
                title: "Two".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: String::new(),
                track_id: String::new(),
            },
            ReleaseTrack {
                number: "3".into(),
                title: "Three".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: String::new(),
                track_id: String::new(),
            },
        ]);
        let files = vec![
            file(
                "01.flac",
                BTreeMap::from([("title".into(), vec!["One".into()])]),
            ),
            file(
                "02.flac",
                BTreeMap::from([("title".into(), vec!["Two".into()])]),
            ),
        ];
        let plan = map_edits(&files, &release).unwrap();
        assert_eq!(plan.edits.len(), 2);
        assert_eq!(plan.unmatched_files, 0);
        assert_eq!(plan.unmatched_tracks, 1);
    }

    #[test]
    fn extra_files_map_unique_pairs_and_leave_leftovers() {
        let release = demo_release(vec![
            ReleaseTrack {
                number: "1".into(),
                title: "One".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: String::new(),
                track_id: String::new(),
            },
            ReleaseTrack {
                number: "2".into(),
                title: "Two".into(),
                artist: "Band".into(),
                length_ms: None,
                disc: 1,
                recording_id: String::new(),
                track_id: String::new(),
            },
        ]);
        let files = vec![
            file(
                "01.flac",
                BTreeMap::from([
                    ("title".into(), vec!["One".into()]),
                    ("track".into(), vec!["1".into()]),
                ]),
            ),
            file(
                "02.flac",
                BTreeMap::from([
                    ("title".into(), vec!["Two".into()]),
                    ("track".into(), vec!["2".into()]),
                ]),
            ),
            file(
                "Two again.flac",
                BTreeMap::from([("title".into(), vec!["Two".into()])]),
            ),
        ];
        let plan = map_edits(&files, &release).unwrap();
        assert_eq!(plan.edits.len(), 2);
        assert_eq!(plan.unmatched_files, 1);
        assert_eq!(plan.unmatched_tracks, 0);
        assert!(plan.edits.iter().all(|e| e.name != "Two again.flac"));
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
