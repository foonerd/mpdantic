//! `.mpdignore` as MPD reads it (`src/db/update/ExcludeList.cxx` and `src/fs/Glob.hxx`
//! at v0.24.6): each line stripped, blank lines and lines starting with `#` dropped,
//! every other line one `fnmatch` pattern with no flags, matched against a name alone;
//! a directory's list inherits its parent's, which is checked first.

use std::ffi::{CString, OsStr};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// One pattern, matched as MPD matches it.
pub struct Pattern {
    text: String,
    c: CString,
}

impl Pattern {
    pub fn new(text: &str) -> Option<Pattern> {
        CString::new(text).ok().map(|c| Pattern {
            text: text.to_string(),
            c,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn matches(&self, name: &OsStr) -> bool {
        let Ok(name) = CString::new(name.as_bytes()) else {
            return false;
        };
        // SAFETY: both are NUL-terminated C strings that outlive the call; fnmatch
        // reads them and nothing else.
        unsafe { libc::fnmatch(self.c.as_ptr(), name.as_ptr(), 0) == 0 }
    }
}

/// The lines of one `.mpdignore`, parsed as MPD parses them; and the lines MPD drops
/// as comments that look like a name (`#recycle`), which the owner meant as patterns.
pub struct ExcludeList {
    patterns: Vec<Pattern>,
    dead: Vec<String>,
}

impl ExcludeList {
    pub fn parse(text: &str) -> ExcludeList {
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let patterns = lines
            .iter()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| Pattern::new(l))
            .collect();
        // A comment is `#` and a space, or `#` alone; `#recycle` is a name nobody
        // meant as a comment.
        let dead = lines
            .iter()
            .filter(|l| {
                l.len() > 1 && l.starts_with('#') && !l[1..].starts_with(char::is_whitespace)
            })
            .map(|l| l.to_string())
            .collect();
        ExcludeList { patterns, dead }
    }

    pub fn lines(&self) -> Vec<String> {
        self.patterns.iter().map(|p| p.text.clone()).collect()
    }

    /// The `#` lines that look like names: comments to MPD, dead to their writer.
    pub fn dead(&self) -> Vec<String> {
        self.dead.clone()
    }

    /// The first line that matches the name.
    pub fn matches(&self, name: &OsStr) -> Option<&str> {
        self.patterns
            .iter()
            .find(|p| p.matches(name))
            .map(Pattern::text)
    }
}

/// The `.mpdignore` of a directory, if it has one it can be read.
pub fn read_list(dir: &Path) -> Option<ExcludeList> {
    fs::read_to_string(dir.join(".mpdignore"))
        .ok()
        .map(|text| ExcludeList::parse(&text))
}

/// The lists in force for the names in a directory: the root's down to its own, the
/// way MPD inherits them, each with the directory it came from in MPD's form.
#[derive(Default)]
pub struct InForce {
    lists: Vec<(String, ExcludeList)>,
}

impl InForce {
    pub fn push(&mut self, dir: String, list: ExcludeList) {
        self.lists.push((dir, list));
    }

    pub fn pop(&mut self) {
        self.lists.pop();
    }

    /// The directory and the line that exclude the name, the outermost first.
    pub fn matches(&self, name: &OsStr) -> Option<(&str, &str)> {
        self.lists
            .iter()
            .find_map(|(dir, list)| list.matches(name).map(|line| (dir.as_str(), line)))
    }

    /// Every list in force with its lines and its dead lines, outermost first.
    pub fn lines(&self) -> Vec<(String, Vec<String>, Vec<String>)> {
        self.lists
            .iter()
            .map(|(dir, list)| (dir.clone(), list.lines(), list.dead()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn a_list_is_parsed_as_mpd_parses_it() {
        // A line starting with # is a comment, so Synology's bin needs the bracket.
        let list =
            ExcludeList::parse("  *.iso  \n\n# a comment\n99*\n#recycle\n[#]recycle\n#\n  \n");
        assert_eq!(list.lines(), ["*.iso", "99*", "[#]recycle"]);
        assert_eq!(list.dead(), ["#recycle"]);
        assert_eq!(list.matches(name("x.iso")), Some("*.iso"));
        assert_eq!(list.matches(name("99 - track.flac")), Some("99*"));
        assert_eq!(list.matches(name("#recycle")), Some("[#]recycle"));
        assert_eq!(list.matches(name("track.flac")), None);
    }

    #[test]
    fn a_pattern_matches_the_name_alone_as_fnmatch_does() {
        let p = Pattern::new("*.iso").unwrap();
        assert!(p.matches(name("album.iso")));
        assert!(p.matches(name(".hidden.iso")));
        assert!(!p.matches(name("album.ISO")));
        assert!(!p.matches(name("album.iso.bak")));
        let p = Pattern::new("[!._]*").unwrap();
        assert!(p.matches(name("album")));
        assert!(!p.matches(name("._album")));
        let p = Pattern::new("Live at ?").unwrap();
        assert!(p.matches(name("Live at 9")));
        assert!(!p.matches(name("Live at 10")));
        assert!(Pattern::new("a\0b").is_none());
    }

    #[test]
    fn the_lists_in_force_are_inherited_and_the_outermost_counts_first() {
        let mut in_force = InForce::default();
        in_force.push(String::new(), ExcludeList::parse("*.iso\n"));
        in_force.push("NAS".to_string(), ExcludeList::parse("*.mov\n*.iso\n"));
        assert_eq!(in_force.matches(name("x.iso")), Some(("", "*.iso")));
        assert_eq!(in_force.matches(name("x.mov")), Some(("NAS", "*.mov")));
        assert_eq!(in_force.matches(name("x.flac")), None);
        assert_eq!(
            in_force.lines(),
            vec![
                (String::new(), vec!["*.iso".to_string()], vec![]),
                (
                    "NAS".to_string(),
                    vec!["*.mov".to_string(), "*.iso".to_string()],
                    vec![]
                ),
            ]
        );
        in_force.pop();
        assert_eq!(in_force.matches(name("x.mov")), None);
    }

    #[test]
    fn a_folder_without_the_file_has_no_list() {
        let dir = std::env::temp_dir().join(format!("mpdantic-ignore-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert!(read_list(&dir).is_none());
        fs::write(dir.join(".mpdignore"), "*.zip\n").unwrap();
        assert_eq!(read_list(&dir).unwrap().lines(), ["*.zip"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
