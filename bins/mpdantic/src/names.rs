//! One pattern, either way: a file's name made from its tags (`{track} {title}`),
//! or its tags read from its name by the same pattern. The fields are the tag
//! editor's; a name is made safe for a filesystem and for a share.

use std::collections::BTreeMap;

use crate::text::Msg;

/// The fields a pattern may name: the tag editor's.
pub const FIELDS: [&str; 10] = [
    "track",
    "title",
    "artist",
    "albumartist",
    "album",
    "date",
    "disc",
    "genre",
    "composer",
    "comment",
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Field(String),
}

/// A pattern as parsed: literal text and fields, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    parts: Vec<Part>,
}

impl Pattern {
    /// Parsed from its text, `{field}` among literal text. Refused: a brace that
    /// does not close, a field that is not one of the editor's, no field at all, and
    /// two fields with nothing between them, which a name read back could not tell
    /// apart.
    pub fn parse(text: &str) -> Result<Pattern, Msg> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut rest = text;
        while let Some(open) = rest.find('{') {
            literal.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                return Err(msg!("core.names.unclosed", "a brace is not closed"));
            };
            let field = after[..close].trim();
            if !FIELDS.contains(&field) {
                return Err(msg!(
                    "core.names.bad_field",
                    "{field}: not a field a name carries",
                    field = field
                ));
            }
            if !literal.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut literal)));
            }
            if matches!(parts.last(), Some(Part::Field(_))) {
                return Err(msg!(
                    "core.names.adjacent",
                    "two fields with nothing between them cannot be told apart"
                ));
            }
            parts.push(Part::Field(field.to_string()));
            rest = &after[close + 1..];
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            parts.push(Part::Text(literal));
        }
        if !parts.iter().any(|p| matches!(p, Part::Field(_))) {
            return Err(msg!("core.names.no_field", "the pattern names no field"));
        }
        Ok(Pattern { parts })
    }

    /// A name from the values, without the suffix: each field by its value, the
    /// track as at least two digits with a total dropped. The field a value is
    /// missing for is the error.
    pub fn render(&self, value: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Text(t) => out.push_str(t),
                Part::Field(f) => {
                    let v = value(f)
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| f.clone())?;
                    if f == "track" {
                        out.push_str(&track_text(&v));
                    } else {
                        out.push_str(&v);
                    }
                }
            }
        }
        Ok(out)
    }

    /// The fields read from a stem: each field takes up to the literal that follows
    /// it (its first occurrence), the last takes the rest; the track and the disc
    /// take digits only. None when the stem does not fit the pattern.
    pub fn capture(&self, stem: &str) -> Option<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        let mut rest = stem;
        for (i, part) in self.parts.iter().enumerate() {
            match part {
                Part::Text(t) => rest = rest.strip_prefix(t.as_str())?,
                Part::Field(f) => {
                    let taken = if f == "track" || f == "disc" {
                        let n = rest
                            .find(|c: char| !c.is_ascii_digit())
                            .unwrap_or(rest.len());
                        if n == 0 {
                            return None;
                        }
                        n
                    } else {
                        match self.parts.get(i + 1) {
                            Some(Part::Text(next)) => rest.find(next.as_str())?,
                            _ => rest.len(),
                        }
                    };
                    let value = rest[..taken].trim();
                    if value.is_empty() {
                        return None;
                    }
                    out.insert(f.clone(), value.to_string());
                    rest = &rest[taken..];
                }
            }
        }
        rest.is_empty().then_some(out)
    }
}

/// A track number as a name carries it: at least two digits, the total dropped; a
/// track that is not a number as it is.
fn track_text(v: &str) -> String {
    let n = v.split('/').next().unwrap_or("").trim();
    match n.parse::<u64>() {
        Ok(number) if !n.is_empty() => format!("{number:02}"),
        _ => v.to_string(),
    }
}

/// A name made safe for a filesystem and for a share: the characters a Windows
/// share refuses and the control characters become `_`, whitespace is one space,
/// the ends are trimmed, trailing dots are dropped. Empty when nothing is left.
pub fn safe(name: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in name.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        let c = if c.is_control() || "\\/:*?\"<>|".contains(c) {
            '_'
        } else {
            c
        };
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out.trim_end_matches(['.', ' ']).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pattern_is_parsed_and_refused_where_it_cannot_work() {
        let p = Pattern::parse("{track} {title}").unwrap();
        assert_eq!(
            p.parts,
            vec![
                Part::Field("track".into()),
                Part::Text(" ".into()),
                Part::Field("title".into())
            ]
        );
        assert_eq!(
            Pattern::parse("{track}. {artist} - {title}")
                .unwrap()
                .parts
                .len(),
            5
        );
        assert_eq!(
            Pattern::parse("{track").unwrap_err().k,
            "core.names.unclosed"
        );
        assert_eq!(
            Pattern::parse("{size}").unwrap_err().k,
            "core.names.bad_field"
        );
        assert_eq!(
            Pattern::parse("{artist}{title}").unwrap_err().k,
            "core.names.adjacent"
        );
        assert_eq!(
            Pattern::parse("plain").unwrap_err().k,
            "core.names.no_field"
        );
        assert_eq!(Pattern::parse("").unwrap_err().k, "core.names.no_field");
    }

    #[test]
    fn a_name_is_rendered_from_the_values_with_the_track_padded() {
        let p = Pattern::parse("{track} {title}").unwrap();
        let values = |track: &str, title: &str| {
            let (track, title) = (track.to_string(), title.to_string());
            move |f: &str| -> Option<String> {
                match f {
                    "track" => Some(track.clone()),
                    "title" => Some(title.clone()),
                    _ => None,
                }
            }
        };
        assert_eq!(p.render(&values("1", "One")).unwrap(), "01 One");
        assert_eq!(p.render(&values("7/12", " Seven ")).unwrap(), "07 Seven");
        assert_eq!(p.render(&values("012", "Twelve")).unwrap(), "12 Twelve");
        assert_eq!(p.render(&values("A1", "Side")).unwrap(), "A1 Side");
        assert_eq!(p.render(&values("1", "")).unwrap_err(), "title");
        assert_eq!(p.render(&values("", "One")).unwrap_err(), "track");
        let artist = Pattern::parse("{artist} - {title}").unwrap();
        assert_eq!(artist.render(&values("1", "One")).unwrap_err(), "artist");
    }

    #[test]
    fn the_fields_are_read_back_from_a_stem_by_the_same_pattern() {
        let p = Pattern::parse("{track} {title}").unwrap();
        let got = p.capture("01 One").unwrap();
        assert_eq!(got["track"], "01");
        assert_eq!(got["title"], "One");
        assert_eq!(p.capture("12 Two Words").unwrap()["title"], "Two Words");
        assert!(p.capture("One").is_none());
        assert!(p.capture("01").is_none());
        assert!(p.capture("01 ").is_none());
        let p = Pattern::parse("{track} - {artist} - {title}").unwrap();
        let got = p.capture("03 - A Band - The Song - Live").unwrap();
        assert_eq!(got["artist"], "A Band");
        assert_eq!(got["title"], "The Song - Live");
        assert!(p.capture("03 - A Band").is_none());
        let p = Pattern::parse("{disc}-{track} {title}").unwrap();
        let got = p.capture("2-11 Eleven").unwrap();
        assert_eq!((got["disc"].as_str(), got["track"].as_str()), ("2", "11"));
        assert!(Pattern::parse("{title}")
            .unwrap()
            .capture("Anything at all")
            .is_some());
        assert!(Pattern::parse("{title} x")
            .unwrap()
            .capture("Anything")
            .is_none());
    }

    #[test]
    fn a_name_is_made_safe_for_a_filesystem_and_a_share() {
        assert_eq!(safe("AC/DC: Back in Black?"), "AC_DC_ Back in Black_");
        assert_eq!(safe("  two   spaces  "), "two spaces");
        assert_eq!(safe("ends with dots..."), "ends with dots");
        assert_eq!(safe("tab\there"), "tab here");
        assert_eq!(safe("a\u{7}b"), "a_b");
        assert_eq!(safe("..."), "");
        assert_eq!(safe("fine - as it is"), "fine - as it is");
    }
}
