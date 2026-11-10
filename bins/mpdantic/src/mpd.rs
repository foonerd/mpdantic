//! A small client of MPD's protocol, for what the core asks of the running MPD: the
//! decoders it has (the suffixes it would try as songs), later an update of one
//! folder. One connection per question, never a whole-library command.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Where MPD listens: a unix socket path, or host:port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    Unix(PathBuf),
    Tcp(String),
}

impl Address {
    /// A path is a unix socket; anything else is host:port.
    pub fn parse(text: &str) -> Address {
        if text.starts_with('/') {
            Address::Unix(PathBuf::from(text))
        } else {
            Address::Tcp(text.to_string())
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Address::Unix(path) => write!(f, "{}", path.display()),
            Address::Tcp(addr) => write!(f, "{addr}"),
        }
    }
}

trait Socket: Read + Write + Send {}
impl Socket for UnixStream {}
impl Socket for TcpStream {}

/// One connection: the greeting read, commands answered line by line.
pub struct Connection {
    socket: BufReader<Box<dyn Socket>>,
    pub version: String,
}

pub fn connect(address: &Address) -> Result<Connection, String> {
    let socket: Box<dyn Socket> = match address {
        Address::Unix(path) => {
            let s = UnixStream::connect(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let _ = s.set_read_timeout(Some(TIMEOUT));
            let _ = s.set_write_timeout(Some(TIMEOUT));
            Box::new(s)
        }
        Address::Tcp(addr) => {
            let s = TcpStream::connect(addr).map_err(|e| format!("{addr}: {e}"))?;
            let _ = s.set_read_timeout(Some(TIMEOUT));
            let _ = s.set_write_timeout(Some(TIMEOUT));
            Box::new(s)
        }
    };
    let mut socket = BufReader::new(socket);
    let mut line = String::new();
    socket
        .read_line(&mut line)
        .map_err(|e| format!("{address}: {e}"))?;
    let version = line
        .strip_prefix("OK MPD ")
        .map(|v| v.trim().to_string())
        .ok_or_else(|| format!("{address}: not MPD: {}", line.trim()))?;
    Ok(Connection { socket, version })
}

impl Connection {
    /// One command, its `key: value` lines; an ACK is the error.
    pub fn command(&mut self, command: &str) -> Result<Vec<(String, String)>, String> {
        self.socket
            .get_mut()
            .write_all(format!("{command}\n").as_bytes())
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        loop {
            let mut line = String::new();
            let n = self
                .socket
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("MPD closed the connection".to_string());
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line == "OK" {
                return Ok(out);
            }
            if let Some(ack) = line.strip_prefix("ACK ") {
                return Err(ack.to_string());
            }
            if let Some((key, value)) = line.split_once(": ") {
                out.push((key.to_string(), value.to_string()));
            }
        }
    }
}

/// The decoders of an MPD: the protocol version its greeting names, its plugins with
/// what each claims, every suffix lowercased, and the suffixes only the ffmpeg
/// plugin claims (libavformat's whole list, text and subtitles included).
#[derive(Clone, Debug)]
pub struct Decoders {
    pub protocol: String,
    pub plugins: Vec<(String, BTreeSet<String>)>,
    pub suffixes: BTreeSet<String>,
    pub ffmpeg_only: BTreeSet<String>,
}

impl Decoders {
    /// From the plugins and their suffixes, the rest derived.
    pub fn from_plugins(protocol: String, plugins: Vec<(String, BTreeSet<String>)>) -> Decoders {
        let suffixes: BTreeSet<String> = plugins
            .iter()
            .flat_map(|(_, s)| s.iter().cloned())
            .collect();
        let others: BTreeSet<String> = plugins
            .iter()
            .filter(|(name, _)| name != "ffmpeg")
            .flat_map(|(_, s)| s.iter().cloned())
            .collect();
        let ffmpeg_only = plugins
            .iter()
            .filter(|(name, _)| name == "ffmpeg")
            .flat_map(|(_, s)| s.iter().cloned())
            .filter(|s| !others.contains(s))
            .collect();
        Decoders {
            protocol,
            plugins,
            suffixes,
            ffmpeg_only,
        }
    }
}

/// What the running MPD says it decodes.
pub fn decoders(address: &Address) -> Result<Decoders, String> {
    let mut c = connect(address)?;
    let lines = c.command("decoders")?;
    let mut plugins: Vec<(String, BTreeSet<String>)> = Vec::new();
    for (key, value) in lines {
        match key.as_str() {
            "plugin" => plugins.push((value, BTreeSet::new())),
            "suffix" => {
                if let Some((_, suffixes)) = plugins.last_mut() {
                    suffixes.insert(value.to_lowercase());
                }
            }
            _ => {}
        }
    }
    Ok(Decoders::from_plugins(c.version.clone(), plugins))
}

/// When MPD cannot be asked: the suffixes Volumio's MPD build commonly claims, so the
/// analysis still tells audio from the rest. The report says they are assumed.
pub fn assumed() -> Decoders {
    const SUFFIXES: &[&str] = &[
        "flac", "mp3", "mp2", "ogg", "oga", "opus", "spx", "m4a", "m4b", "aac", "mp4", "wav",
        "aif", "aiff", "aifc", "wv", "ape", "mpc", "dsf", "dff", "dst", "wma", "tta", "mka",
    ];
    Decoders {
        protocol: String::new(),
        plugins: Vec::new(),
        suffixes: SUFFIXES.iter().map(|s| s.to_string()).collect(),
        ffmpeg_only: BTreeSet::new(),
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::net::TcpListener;

    /// An MPD that answers `decoders` with two plugins and refuses anything else.
    pub fn fake_mpd(greeting: &'static str) -> Address {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut stream = stream;
                stream.write_all(greeting.as_bytes()).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let answer = match line.trim() {
                        "decoders" => "plugin: mad\nsuffix: mp3\nsuffix: mp2\nmime_type: audio/mpeg\nplugin: flac\nsuffix: flac\nplugin: ffmpeg\nsuffix: FLAC\nsuffix: mkv\nsuffix: txt\nOK\n".to_string(),
                        other => format!("ACK [5@0] {{}} unknown command \"{other}\"\n"),
                    };
                    let _ = stream.write_all(answer.as_bytes());
                    line.clear();
                }
            }
        });
        Address::Tcp(format!("127.0.0.1:{port}"))
    }

    #[test]
    fn an_address_is_a_socket_path_or_host_and_port() {
        assert_eq!(
            Address::parse("/run/mpd/socket"),
            Address::Unix(PathBuf::from("/run/mpd/socket"))
        );
        assert_eq!(
            Address::parse("127.0.0.1:6600"),
            Address::Tcp("127.0.0.1:6600".to_string())
        );
    }

    #[test]
    fn the_decoders_are_read_from_the_running_mpd_lowercased() {
        let address = fake_mpd("OK MPD 0.24.6\n");
        let d = decoders(&address).unwrap();
        assert_eq!(d.protocol, "0.24.6");
        assert_eq!(
            d.plugins
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["mad", "flac", "ffmpeg"]
        );
        assert_eq!(
            d.suffixes.iter().map(String::as_str).collect::<Vec<_>>(),
            ["flac", "mkv", "mp2", "mp3", "txt"]
        );
        // flac is ffmpeg's too, but MPD's own flac plugin claims it first.
        assert_eq!(
            d.ffmpeg_only.iter().map(String::as_str).collect::<Vec<_>>(),
            ["mkv", "txt"]
        );
        let mut c = connect(&address).unwrap();
        assert_eq!(c.version, "0.24.6");
        assert!(c
            .command("nothing")
            .unwrap_err()
            .contains("unknown command"));
    }

    #[test]
    fn something_that_is_not_mpd_is_refused() {
        let address = fake_mpd("HTTP/1.0 400 Bad Request\n");
        let refused = connect(&address).err().expect("not refused");
        assert!(refused.contains("not MPD"), "{refused}");
        assert!(decoders(&Address::Tcp("127.0.0.1:1".to_string())).is_err());
    }

    /// Decoders from a list of plugins and their suffixes, for other modules' tests.
    pub fn decoders_of(plugins: &[(&str, &[&str])]) -> Decoders {
        Decoders::from_plugins(
            "0.24.0".to_string(),
            plugins
                .iter()
                .map(|(name, suffixes)| {
                    (
                        name.to_string(),
                        suffixes.iter().map(|s| s.to_string()).collect(),
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn the_assumed_set_is_audio_without_video() {
        let d = assumed();
        assert!(d.suffixes.contains("flac"));
        assert!(!d.suffixes.contains("mkv"));
        assert!(d.ffmpeg_only.is_empty());
    }
}
