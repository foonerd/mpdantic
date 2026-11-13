//! A read-write mount of a NAS folder, for a write. Volumio mounts every share
//! read-only; MPDantic mounts the folder in hand, as a prefix path, read-write and
//! as the volumio user, for the time of the write, through the operating system's
//! own passwordless `mount` and `umount` for that user. The credentials come from
//! Volumio's own configuration of the share into a file only root reads, for the
//! mount call, and go right after. Local roots (`USB`, `INTERNAL`) are read-write
//! already and need none of this.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::text::{self, Msg};

/// A share as Volumio knows it (`networkfs`, `NasMounts`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub key: String,
    pub name: String,
    pub ip: String,
    pub path: String,
    pub fstype: String,
    pub user: String,
    pub password: String,
    pub options: String,
}

/// The shares from Volumio's configuration file.
pub fn shares(config: &Path) -> Result<Vec<Share>, Msg> {
    let text = fs::read_to_string(config).map_err(|e| text::at(config.display(), e))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| text::at(config.display(), e))?;
    let Some(mounts) = value.get("NasMounts").and_then(|m| m.as_object()) else {
        return Ok(Vec::new());
    };
    let field = |e: &serde_json::Value, name: &str| -> String {
        let v = e.get(name);
        let v = v
            .and_then(|v| v.get("value").or(Some(v)))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        v.to_string()
    };
    Ok(mounts
        .iter()
        .map(|(key, e)| Share {
            key: key.clone(),
            name: field(e, "name"),
            ip: field(e, "ip"),
            path: field(e, "path"),
            fstype: field(e, "fstype"),
            user: field(e, "user"),
            password: field(e, "password"),
            options: field(e, "options"),
        })
        .collect())
}

/// Where a library path (MPD's form) is written: a local root, written directly; or
/// a NAS share and the folder under it, mounted for the write.
#[derive(Debug, PartialEq, Eq)]
pub enum Where<'a> {
    Local,
    Nas { share: &'a Share, folder: String },
}

/// The place of a folder: `NAS/<share name>/<folder>` is on that share, anything
/// else is local.
pub fn place<'a>(shares: &'a [Share], folder: &str) -> Result<Where<'a>, Msg> {
    let mut parts = folder.splitn(3, '/');
    let root = parts.next().unwrap_or("");
    if root != "NAS" {
        return Ok(Where::Local);
    }
    let name = parts.next().unwrap_or("");
    let share = shares.iter().find(|s| s.name == name).ok_or_else(|| {
        msg!(
            "core.mount.no_share",
            "no share named {name} in Volumio's configuration",
            name = name
        )
    })?;
    Ok(Where::Nas {
        share,
        folder: parts.next().unwrap_or("").to_string(),
    })
}

/// A mount made for a write, taken down by `unmount`.
#[derive(Serialize, Debug)]
pub struct Mount {
    pub target: PathBuf,
    pub source: String,
    pub fstype: String,
}

/// What runs a mount or an unmount: the command's words, with the credentials in a
/// file named by `credentials=`, never on the line.
pub type Runner = Box<dyn Fn(&[String]) -> Result<(), String> + Send + Sync>;

pub struct Mounter {
    run: PathBuf,
    runner: Runner,
    uid: u32,
    gid: u32,
}

/// Where the install puts the mount helper sudo is allowed to run: root-owned,
/// outside the plugin folder (which is volumio-writable after install).
pub const MOUNT_HELPER: &str = "/usr/local/libexec/mpdantic-mount";

/// The real runner: `sudo -n` the mount helper with `mount` or `umount`, never
/// raw `/bin/mount`. The helper itself refuses a target outside `/run/mpdantic/mounts`.
pub fn sudo_runner() -> Runner {
    Box::new(|words: &[String]| {
        if words.is_empty() {
            return Err("mount: empty command".to_string());
        }
        let mut argv = vec![MOUNT_HELPER.to_string()];
        if words[0].ends_with("umount") {
            argv.push("umount".to_string());
            argv.extend(words.iter().skip(1).cloned());
        } else if words[0].ends_with("mount") {
            argv.push("mount".to_string());
            argv.extend(words.iter().skip(1).cloned());
        } else {
            return Err(format!("mount: not a mount or umount: {}", words[0]));
        }
        let output = Command::new("/usr/bin/sudo")
            .arg("-n")
            .args(&argv)
            .output()
            .map_err(|e| format!("sudo: {e}"))?;
        if output.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(if err.is_empty() {
                format!("{} failed with {}", argv.join(" "), output.status)
            } else {
                err
            })
        }
    })
}

impl Mounter {
    pub fn new(run: &Path, runner: Runner) -> Mounter {
        // SAFETY: getuid and getgid read the process's ids and cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        Mounter {
            run: run.to_path_buf(),
            runner,
            uid,
            gid,
        }
    }

    /// The folder mounted read-write under the run folder; the mount proved writable
    /// by a hidden file created and removed, else taken down again with the reason.
    pub fn mount(&self, share: &Share, folder: &str) -> Result<Mount, Msg> {
        if !matches!(share.fstype.as_str(), "cifs" | "nfs") {
            return Err(msg!(
                "core.mount.type_not_mounted",
                "a share of type {fstype} is not mounted here",
                fstype = share.fstype
            ));
        }
        let mounts = self.run.join("mounts").join(&share.key);
        fs::create_dir_all(&mounts).map_err(|e| text::at(mounts.display(), e))?;
        let target = mounts.join(if folder.is_empty() {
            "root".to_string()
        } else {
            folder.replace('/', "~")
        });
        fs::create_dir_all(&target).map_err(|e| text::at(target.display(), e))?;
        let (source, fstype, options) = self.plan(share, folder);
        let result = if fstype == "cifs" {
            let credentials = match self.credentials_file(share) {
                Ok(path) => path,
                Err(e) => {
                    let _ = fs::remove_dir(&target);
                    return Err(e);
                }
            };
            let words = vec![
                "/bin/mount".to_string(),
                "-t".to_string(),
                "cifs".to_string(),
                source.clone(),
                target.display().to_string(),
                "-o".to_string(),
                format!("credentials={},{options}", credentials.display()),
            ];
            let result = (self.runner)(&words);
            let _ = fs::remove_file(&credentials);
            result
        } else {
            let words = vec![
                "/bin/mount".to_string(),
                "-t".to_string(),
                "nfs".to_string(),
                source.clone(),
                target.display().to_string(),
                "-o".to_string(),
                options,
            ];
            (self.runner)(&words)
        };
        if let Err(e) = result {
            let _ = fs::remove_dir(&target);
            return Err(msg!(
                "core.mount.failed",
                "mount {source}: {error}",
                source = source,
                error = e
            ));
        }
        let mut mount = Mount {
            target: target.clone(),
            source,
            fstype,
        };
        if let Err(e) = write_test(&target) {
            self.unmount(&mut mount);
            return Err(msg!(
                "core.mount.cannot_write",
                "{share}: the account cannot write there: {error}",
                share = share.name,
                error = e
            ));
        }
        Ok(mount)
    }

    /// The source, the type and the options of the mount, no secret among them.
    pub fn plan(&self, share: &Share, folder: &str) -> (String, String, String) {
        let sub = if folder.is_empty() {
            String::new()
        } else {
            format!("/{folder}")
        };
        let user_options = share.options.trim();
        match share.fstype.as_str() {
            "nfs" => {
                let mut options = "nosharecache,soft".to_string();
                if !user_options.is_empty() {
                    options.push(',');
                    options.push_str(user_options);
                }
                (
                    format!("{}:/{}{sub}", share.ip, share.path.trim_matches('/')),
                    "nfs".to_string(),
                    options,
                )
            }
            _ => {
                let mut options = format!(
                    "uid={},gid={},file_mode=0666,dir_mode=0777,iocharset=utf8,soft",
                    self.uid, self.gid
                );
                if !user_options.is_empty() {
                    options.push(',');
                    options.push_str(user_options);
                }
                (
                    format!("//{}/{}{sub}", share.ip, share.path.trim_matches('/')),
                    "cifs".to_string(),
                    options,
                )
            }
        }
    }

    /// The credentials for the mount call, in a file of the run folder only its
    /// owner reads; root reads anything. Removed by the caller right after.
    fn credentials_file(&self, share: &Share) -> Result<PathBuf, Msg> {
        let path = self
            .run
            .join(format!("cred.{}.{}", share.key, std::process::id()));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| text::at(path.display(), e))?;
        write!(
            file,
            "username={}\npassword={}\n",
            share.user, share.password
        )
        .map_err(|e| text::at(path.display(), e))?;
        Ok(path)
    }

    /// The mount taken down and its folder removed; a failure is logged, not fatal.
    pub fn unmount(&self, mount: &mut Mount) {
        let words = vec![
            "/bin/umount".to_string(),
            mount.target.display().to_string(),
        ];
        if let Err(e) = (self.runner)(&words) {
            eprintln!("mpdantic: umount {}: {e}", mount.target.display());
        }
        let _ = fs::remove_dir(&mount.target);
        // The share's folder of mount points, when nothing else is in it.
        if let Some(parent) = mount.target.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

/// A hidden file created and removed: the one way to know the account can write.
pub fn write_test(dir: &Path) -> Result<(), String> {
    let probe = dir.join(".mpdantic-write-test");
    fs::write(&probe, b"").map_err(|e| e.to_string())?;
    fs::remove_file(&probe).map_err(|e| e.to_string())
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    pub fn sample_config(dir: &Path) -> PathBuf {
        let file = dir.join("networkfs.json");
        fs::write(&file, r#"{"NasMounts":{"k1":{"name":{"type":"string","value":"multimedia"},"ip":{"type":"string","value":"192.168.30.1"},"path":{"type":"string","value":"multimedia/broadcast/Video/Movies"},"fstype":{"type":"string","value":"cifs"},"user":{"type":"string","value":"andrew"},"password":{"type":"string","value":"s3cret"},"options":{"type":"string","value":"vers=3.11"}},"k2":{"name":"nfsbox","ip":"10.0.0.2","path":"/export/music","fstype":"nfs","user":"","password":"","options":""}}}"#).unwrap();
        file
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mpdantic-mount-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A runner that records the words and makes the target look mounted (writable).
    fn recording_runner(calls: Arc<Mutex<Vec<Vec<String>>>>, refuse_write: bool) -> Runner {
        Box::new(move |words: &[String]| {
            calls.lock().unwrap().push(words.to_vec());
            let target = PathBuf::from(if words[0] == "/bin/mount" {
                &words[4]
            } else {
                &words[1]
            });
            if refuse_write && words[0] == "/bin/mount" {
                // An account that cannot write: the target becomes a place nothing
                // can be created in, whoever runs the test (root writes anywhere).
                fs::remove_dir(&target).unwrap();
                std::os::unix::fs::symlink(target.join("nowhere"), &target).unwrap();
            } else if refuse_write {
                fs::remove_file(&target).unwrap();
                fs::create_dir(&target).unwrap();
            }
            Ok(())
        })
    }

    #[test]
    fn the_shares_come_from_volumios_configuration_in_both_shapes() {
        let dir = temp("shares");
        let s = shares(&sample_config(&dir)).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].key, "k1");
        assert_eq!(s[0].name, "multimedia");
        assert_eq!(s[0].path, "multimedia/broadcast/Video/Movies");
        assert_eq!(s[0].password, "s3cret");
        assert_eq!(s[0].options, "vers=3.11");
        assert_eq!(s[1].fstype, "nfs");
        assert_eq!(s[1].path, "/export/music");
        assert!(shares(&dir.join("missing.json")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_is_local_or_on_a_share() {
        let dir = temp("place");
        let s = shares(&sample_config(&dir)).unwrap();
        assert_eq!(place(&s, "INTERNAL/Abba").unwrap(), Where::Local);
        assert_eq!(place(&s, "USB").unwrap(), Where::Local);
        assert_eq!(place(&s, "").unwrap(), Where::Local);
        assert_eq!(
            place(&s, "NAS/multimedia/Dune/Part 1").unwrap(),
            Where::Nas {
                share: &s[0],
                folder: "Dune/Part 1".to_string()
            }
        );
        assert_eq!(
            place(&s, "NAS/multimedia").unwrap(),
            Where::Nas {
                share: &s[0],
                folder: String::new()
            }
        );
        assert!(place(&s, "NAS/other/x").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_mount_plan_names_no_secret_and_takes_the_users_options() {
        let dir = temp("plan");
        let s = shares(&sample_config(&dir)).unwrap();
        let m = Mounter::new(&dir, Box::new(|_| Ok(())));
        let (source, fstype, options) = m.plan(&s[0], "Dune/Part 1");
        assert_eq!(
            source,
            "//192.168.30.1/multimedia/broadcast/Video/Movies/Dune/Part 1"
        );
        assert_eq!(fstype, "cifs");
        assert!(options.starts_with(&format!(
            "uid={},gid={},file_mode=0666,dir_mode=0777,iocharset=utf8,soft,vers=3.11",
            m.uid, m.gid
        )));
        assert!(!options.contains("s3cret"));
        let (source, fstype, options) = m.plan(&s[1], "");
        assert_eq!(source, "10.0.0.2:/export/music");
        assert_eq!(fstype, "nfs");
        assert_eq!(options, "nosharecache,soft");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_mount_runs_the_command_with_a_credentials_file_that_goes_right_after() {
        let dir = temp("mount");
        let s = shares(&sample_config(&dir)).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let m = Mounter::new(&dir, recording_runner(calls.clone(), false));
        let mut mount = m.mount(&s[0], "Dune").unwrap();
        assert_eq!(mount.target, dir.join("mounts/k1/Dune"));
        assert!(mount.target.is_dir());
        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0][0..3], ["/bin/mount", "-t", "cifs"]);
        assert_eq!(
            recorded[0][3],
            "//192.168.30.1/multimedia/broadcast/Video/Movies/Dune"
        );
        assert!(
            recorded[0][6].starts_with(&format!("credentials={}", dir.join("cred.k1.").display()))
        );
        assert!(!recorded[0].join(" ").contains("s3cret"));
        // The credentials file is gone, and nothing of the password is in the run folder.
        assert!(fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with("cred.")));
        m.unmount(&mut mount);
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(
            calls.lock().unwrap()[1],
            [
                "/bin/umount".to_string(),
                dir.join("mounts/k1/Dune").display().to_string()
            ]
        );
        assert!(!mount.target.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_account_that_cannot_write_is_found_and_the_mount_taken_down() {
        let dir = temp("readonly");
        let s = shares(&sample_config(&dir)).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let m = Mounter::new(&dir, recording_runner(calls.clone(), true));
        let err = m.mount(&s[0], "Dune").unwrap_err();
        assert!(err.contains("cannot write"), "{err}");
        assert_eq!(calls.lock().unwrap().len(), 2, "mount, then umount");
        assert!(!dir.join("mounts/k1/Dune").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_mount_that_fails_leaves_no_folder_and_no_credentials() {
        let dir = temp("fail");
        let s = shares(&sample_config(&dir)).unwrap();
        let m = Mounter::new(
            &dir,
            Box::new(|_| Err("mount error(13): Permission denied".to_string())),
        );
        let err = m.mount(&s[0], "").unwrap_err();
        assert!(err.contains("Permission denied"), "{err}");
        assert!(!dir.join("mounts/k1/root").exists());
        assert!(fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with("cred.")));
        assert!(m
            .mount(
                &Share {
                    fstype: "sshfs".into(),
                    ..s[0].clone()
                },
                ""
            )
            .unwrap_err()
            .contains("not mounted here"));
        assert!(!dir.join("mounts/k1/root").exists());
        fs::remove_dir_all(&dir).unwrap();
    }
}
