//! The player report: one block of plain text for an issue, every figure read from
//! where it lives on the player and rendered once here, so the page copies exactly
//! what it shows. Nothing of a share but a count by type: no host, no path, no
//! credentials.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::mount;
use crate::mpd::{self, Address};

/// MPD's `stats`.
#[derive(Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub artists: u64,
    pub albums: u64,
    pub songs: u64,
    pub playtime_secs: u64,
    pub db_update: u64,
}

/// The album query measured: `search album ""`, the reply Volumio's album view asks
/// for, read as fast as the core can and counted.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Album {
    pub bytes: u64,
    pub millis: u64,
    /// False when MPD closed the connection first: the reply is past its buffer.
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the backend would take to read it, from the rule's constant for the
    /// architecture, when the shell's plan carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_secs: Option<f64>,
}

/// The shell's plan for the output buffer, as it writes it beside the settings.
#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub line: String,
    pub mib: u64,
    pub floor_mib: u64,
    pub raised: bool,
    pub heap_mib: u64,
    pub memory_mib: u64,
    pub time_mib: u64,
    pub heap_limit_mib: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mpd_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quadratic: Option<f64>,
}

/// Where the figures are read from; the real places, or a test's.
pub struct Sources {
    pub os_release: PathBuf,
    pub model: Vec<PathBuf>,
    pub meminfo: PathBuf,
    pub hostname: PathBuf,
    pub build: Option<PathBuf>,
    pub plan: PathBuf,
    pub networkfs: PathBuf,
}

impl Sources {
    /// The player's own: `build.json` at the plugin's root, three folders up from the
    /// binary (`bin/<arch>/mpdantic`).
    pub fn system(plan: &Path, networkfs: &Path) -> Sources {
        let build = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.ancestors().nth(3).map(|root| root.join("build.json")));
        Sources {
            os_release: PathBuf::from("/etc/os-release"),
            model: vec![
                PathBuf::from("/proc/device-tree/model"),
                PathBuf::from("/sys/class/dmi/id/sys_vendor"),
                PathBuf::from("/sys/class/dmi/id/product_name"),
            ],
            meminfo: PathBuf::from("/proc/meminfo"),
            hostname: PathBuf::from("/etc/hostname"),
            build,
            plan: plan.to_path_buf(),
            networkfs: networkfs.to_path_buf(),
        }
    }
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Report {
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    pub host: String,
    pub when: u64,
    pub model: String,
    pub mem_total: u64,
    pub arch: String,
    pub bits: u32,
    pub volumio_version: String,
    pub volumio_hardware: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mpd_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub decoders_source: String,
    pub decoders_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<Stats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats_error: Option<String>,
    pub shares_cifs: usize,
    pub shares_nfs: usize,
    pub shares_other: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<Album>,
    pub text: String,
}

fn read_trimmed(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// `KEY="value"` lines, the quotes off.
fn os_release(path: &Path) -> Vec<(String, String)> {
    read_trimmed(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

fn mem_total(path: &Path) -> u64 {
    read_trimmed(path)
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// The model: the device tree's, else the DMI vendor and product.
fn model(paths: &[PathBuf]) -> String {
    if let Some(tree) = paths.first().and_then(|p| read_trimmed(p)) {
        return tree;
    }
    let parts: Vec<String> = paths
        .iter()
        .skip(1)
        .filter_map(|p| read_trimmed(p))
        .collect();
    if parts.is_empty() {
        "unknown".to_string()
    } else {
        parts.join(" ")
    }
}

/// The shell's `buffer.json`, loosely: a field missing is zero or none.
pub fn plan(path: &Path) -> Option<Plan> {
    let text = fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let num = |x: Option<&serde_json::Value>| x.and_then(|n| n.as_u64()).unwrap_or(0);
    let bounds = v.get("bounds");
    Some(Plan {
        line: v
            .get("line")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        mib: num(v.get("mib")),
        floor_mib: num(v.get("floor_mib")),
        raised: v.get("raised").and_then(|b| b.as_bool()).unwrap_or(false),
        heap_mib: num(bounds.and_then(|b| b.get("heap"))),
        memory_mib: num(bounds.and_then(|b| b.get("memory"))),
        time_mib: num(bounds.and_then(|b| b.get("time"))),
        heap_limit_mib: num(v.get("inputs").and_then(|i| i.get("heapLimitMib"))),
        mpd_version: v
            .get("inputs")
            .and_then(|i| i.get("mpdVersion"))
            .and_then(|s| s.as_str())
            .map(str::to_string),
        quadratic: v.get("quadratic").and_then(|q| q.as_f64()),
    })
}

fn stats(address: &Address) -> Result<(Stats, String), String> {
    let mut c = mpd::connect(address).map_err(|e| e.to_string())?;
    let protocol = c.version.clone();
    let pairs = c.command("stats").map_err(|e| e.to_string())?;
    let get = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    Ok((
        Stats {
            artists: get("artists"),
            albums: get("albums"),
            songs: get("songs"),
            playtime_secs: get("db_playtime"),
            db_update: get("db_update"),
        },
        protocol,
    ))
}

/// The album query measured now: the reply read and counted, never kept.
pub fn measure(address: &Address, quadratic: Option<f64>) -> Album {
    let started = Instant::now();
    let done = |bytes: u64, complete: bool, error: Option<String>| Album {
        bytes,
        millis: started.elapsed().as_millis() as u64,
        complete,
        error,
        backend_secs: quadratic.map(|k| {
            let mib = bytes as f64 / 1_048_576.0;
            (k * mib * mib * 10.0).round() / 10.0
        }),
    };
    let mut c = match mpd::connect(address) {
        Ok(c) => c,
        Err(e) => return done(0, false, Some(e.to_string())),
    };
    match c.measure("search album \"\"") {
        Ok(m) => done(m.bytes, m.complete, None),
        Err(e) => done(0, false, Some(e)),
    }
}

/// Everything read now, with the last measurement of the album query if one was made.
pub fn gather(
    sources: &Sources,
    address: &Address,
    decoders: (&str, usize),
    album: Option<Album>,
) -> Report {
    let release = os_release(&sources.os_release);
    let field = |key: &str| {
        release
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let commit = sources
        .build
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("commit").and_then(|c| c.as_str()).map(str::to_string));
    let (stats, protocol, stats_error) = match stats(address) {
        Ok((s, p)) => (Some(s), Some(p), None),
        Err(e) => (None, None, Some(e)),
    };
    let (mut cifs, mut nfs, mut other) = (0, 0, 0);
    if let Ok(shares) = mount::shares(&sources.networkfs) {
        for s in shares {
            match s.fstype.as_str() {
                "cifs" => cifs += 1,
                "nfs" => nfs += 1,
                _ => other += 1,
            }
        }
    }
    let plan = plan(&sources.plan);
    let mut r = Report {
        version: crate::VERSION.to_string(),
        commit,
        host: read_trimmed(&sources.hostname).unwrap_or_else(|| "player".to_string()),
        when: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        model: model(&sources.model),
        mem_total: mem_total(&sources.meminfo),
        arch: std::env::consts::ARCH.to_string(),
        bits: usize::BITS,
        volumio_version: field("VOLUMIO_VERSION"),
        volumio_hardware: field("VOLUMIO_HARDWARE"),
        mpd_version: plan.as_ref().and_then(|p| p.mpd_version.clone()),
        protocol,
        decoders_source: decoders.0.to_string(),
        decoders_count: decoders.1,
        stats,
        stats_error,
        shares_cifs: cifs,
        shares_nfs: nfs,
        shares_other: other,
        plan,
        album,
        text: String::new(),
    };
    r.text = render(&r);
    r
}

fn commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
}

/// A UTC time as `YYYY-MM-DD HH:MM`, from the days since the epoch.
fn utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

fn days_of(secs: u64) -> String {
    let days = secs as f64 / 86_400.0;
    if days >= 1.0 {
        format!("{days:.0} days of audio")
    } else {
        format!("{:.1} hours of audio", secs as f64 / 3600.0)
    }
}

/// The block as the page shows and copies it.
pub fn render(r: &Report) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "MPDantic {}{} on {}, {}\n",
        r.version,
        r.commit
            .as_ref()
            .map(|c| format!(" ({c})"))
            .unwrap_or_default(),
        r.host,
        utc(r.when)
    ));
    out.push_str(&format!(
        "player: {}, {}, {} ({}-bit), Volumio {}{}\n",
        r.model,
        gb(r.mem_total),
        r.arch,
        r.bits,
        if r.volumio_version.is_empty() {
            "?"
        } else {
            &r.volumio_version
        },
        if r.volumio_hardware.is_empty() {
            String::new()
        } else {
            format!(" ({})", r.volumio_hardware)
        }
    ));
    out.push_str(&format!(
        "mpd: {}, protocol {}; decoders from {} ({} suffixes)\n",
        r.mpd_version.as_deref().unwrap_or("?"),
        r.protocol.as_deref().unwrap_or("?"),
        r.decoders_source,
        r.decoders_count
    ));
    match (&r.stats, &r.stats_error) {
        (Some(s), _) => out.push_str(&format!(
            "library: {} artists, {} albums, {} songs, {}; last update {}\n",
            commas(s.artists),
            commas(s.albums),
            commas(s.songs),
            days_of(s.playtime_secs),
            if s.db_update == 0 {
                "never".to_string()
            } else {
                utc(s.db_update)
            }
        )),
        (None, e) => out.push_str(&format!(
            "library: not read ({})\n",
            e.as_deref().unwrap_or("MPD not asked")
        )),
    }
    out.push_str(&format!(
        "shares: {} cifs, {} nfs{}\n",
        r.shares_cifs,
        r.shares_nfs,
        if r.shares_other > 0 {
            format!(", {} other", r.shares_other)
        } else {
            String::new()
        }
    ));
    match &r.plan {
        Some(p) => out.push_str(&format!(
            "output buffer: {} MiB{}; bounds heap {}, memory {}, time {} MiB; backend heap {} MiB\n",
            p.mib,
            if p.raised {
                format!(" (raised from Volumio's {})", p.floor_mib)
            } else {
                " (Volumio's own)".to_string()
            },
            p.heap_mib,
            p.memory_mib,
            p.time_mib,
            p.heap_limit_mib
        )),
        None => out.push_str("output buffer: the plugin's plan is not written yet\n"),
    }
    match &r.album {
        Some(a) if a.complete => out.push_str(&format!(
            "album query: {} in {:.1} s from MPD{}\n",
            mib(a.bytes),
            a.millis as f64 / 1000.0,
            a.backend_secs
                .map(|s| format!("; Volumio's backend would take about {s} s to read it"))
                .unwrap_or_default()
        )),
        Some(a) => out.push_str(&format!(
            "album query: {}\n",
            match &a.error {
                Some(e) => format!("not measured ({e})"),
                None => format!(
                    "MPD closed the connection after {}: the reply is past the output buffer",
                    mib(a.bytes)
                ),
            }
        )),
        None => out.push_str("album query: not measured (the button on the System tab)\n"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpd::tests::fake_mpd;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpdantic-report-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_report_reads_the_player_and_renders_one_block() {
        let dir = temp("player");
        fs::write(
            dir.join("os-release"),
            "PRETTY_NAME=\"Raspbian GNU/Linux 12 (bookworm)\"\nVOLUMIO_VERSION=\"4.239\"\nVOLUMIO_HARDWARE=\"pi\"\n",
        )
        .unwrap();
        fs::write(dir.join("model"), b"Raspberry Pi 5 Model B Rev 1.1\0").unwrap();
        fs::write(
            dir.join("meminfo"),
            "MemTotal:       16387992 kB\nMemFree: 1 kB\n",
        )
        .unwrap();
        fs::write(dir.join("hostname"), "hanger\n").unwrap();
        fs::write(
            dir.join("build.json"),
            "{ \"commit\": \"13c4555\", \"built\": \"x\" }",
        )
        .unwrap();
        fs::write(
            dir.join("buffer.json"),
            "{\"line\":\"Volumio's own 80 MiB stands\",\"mib\":80,\"floor_mib\":80,\"raised\":false,\"bounds\":{\"heap\":76,\"memory\":1106,\"time\":86},\"inputs\":{\"heapLimitMib\":1048,\"mpdVersion\":\"0.24.6\"},\"quadratic\":0.016}",
        )
        .unwrap();
        fs::write(
            dir.join("networkfs.json"),
            "{\"NasMounts\":{\"a\":{\"name\":\"Music\",\"ip\":\"10.0.0.2\",\"path\":\"music\",\"fstype\":\"cifs\",\"user\":\"u\",\"password\":\"p\",\"options\":\"\"},\"b\":{\"name\":\"Films\",\"ip\":\"10.0.0.2\",\"path\":\"films\",\"fstype\":\"nfs\",\"user\":\"\",\"password\":\"\",\"options\":\"\"}}}",
        )
        .unwrap();
        let sources = Sources {
            os_release: dir.join("os-release"),
            model: vec![dir.join("model"), dir.join("vendor"), dir.join("product")],
            meminfo: dir.join("meminfo"),
            hostname: dir.join("hostname"),
            build: Some(dir.join("build.json")),
            plan: dir.join("buffer.json"),
            networkfs: dir.join("networkfs.json"),
        };
        let address = fake_mpd("OK MPD 0.24.6\n");
        let r = gather(&sources, &address, ("mpd", 37), None);
        assert_eq!(r.commit.as_deref(), Some("13c4555"));
        assert_eq!(r.model, "Raspberry Pi 5 Model B Rev 1.1");
        assert_eq!(r.mem_total, 16387992 * 1024);
        assert_eq!(r.volumio_version, "4.239");
        assert_eq!(r.mpd_version.as_deref(), Some("0.24.6"));
        assert_eq!(r.protocol.as_deref(), Some("0.24.6"));
        assert_eq!(
            r.stats,
            Some(Stats {
                artists: 1234,
                albums: 5678,
                songs: 123456,
                playtime_secs: 3542400,
                db_update: 1794342000
            })
        );
        assert_eq!((r.shares_cifs, r.shares_nfs, r.shares_other), (1, 1, 0));
        let text = &r.text;
        assert!(
            text.starts_with(&format!(
                "MPDantic {} (13c4555) on hanger, ",
                crate::VERSION
            )),
            "{text}"
        );
        assert!(
            text.contains("player: Raspberry Pi 5 Model B Rev 1.1, 15.6 GB, "),
            "{text}"
        );
        assert!(text.contains("Volumio 4.239 (pi)\n"), "{text}");
        assert!(
            text.contains("mpd: 0.24.6, protocol 0.24.6; decoders from mpd (37 suffixes)\n"),
            "{text}"
        );
        assert!(text.contains("library: 1,234 artists, 5,678 albums, 123,456 songs, 41 days of audio; last update 2026-11-10 "), "{text}");
        assert!(text.contains("shares: 1 cifs, 1 nfs\n"), "{text}");
        assert!(text.contains("output buffer: 80 MiB (Volumio's own); bounds heap 76, memory 1106, time 86 MiB; backend heap 1048 MiB\n"), "{text}");
        assert!(
            text.ends_with("album query: not measured (the button on the System tab)\n"),
            "{text}"
        );
        assert!(text.is_ascii(), "{text}");
        assert!(
            !text.contains("10.0.0.2") && !text.contains("Music"),
            "no share detail: {text}"
        );
        // The album query measured against the fake MPD, which answers with three songs.
        let album = measure(&address, Some(0.016));
        assert!(album.complete, "{album:?}");
        assert!(album.bytes > 60, "{album:?}");
        assert_eq!(album.error, None);
        let r = gather(&sources, &address, ("mpd", 37), Some(album));
        assert!(r.text.contains("album query: 0.0 MiB in 0.0 s from MPD; Volumio's backend would take about 0 s to read it\n"), "{}", r.text);
        // A DMI model when there is no device tree; the plan file missing.
        fs::write(dir.join("vendor"), "innotek GmbH\n").unwrap();
        fs::write(dir.join("product"), "VirtualBox\n").unwrap();
        let sources = Sources {
            model: vec![dir.join("no-tree"), dir.join("vendor"), dir.join("product")],
            plan: dir.join("no-plan.json"),
            ..sources
        };
        let r = gather(&sources, &address, ("assumed", 20), None);
        assert_eq!(r.model, "innotek GmbH VirtualBox");
        assert!(
            r.text
                .contains("output buffer: the plugin's plan is not written yet\n"),
            "{}",
            r.text
        );
        assert!(r.text.contains("mpd: ?, protocol 0.24.6"), "{}", r.text);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_times_and_numbers_render_as_said() {
        assert_eq!(utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc(1794342000), "2026-11-10 20:20 UTC");
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1000), "1,000");
        assert_eq!(commas(1234567), "1,234,567");
        assert_eq!(mib(48_234_496), "46.0 MiB");
        assert_eq!(gb(4008184 * 1024), "3.8 GB");
        assert_eq!(days_of(3542400), "41 days of audio");
        assert_eq!(days_of(7200), "2.0 hours of audio");
    }
}
