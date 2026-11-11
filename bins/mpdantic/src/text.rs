//! The languages of the pages and of the core's own sentences. The pages are served
//! in the player's language (the shell writes Volumio's code into the settings, and
//! `lang=` in the address overrides it), each with its catalogue built into it; a
//! sentence of the core's travels as a `Msg`, its key and its arguments for the page
//! to render from the catalogue, the English rendered beside them.
//!
//! One template language serves the Rust source, the catalogues and the page:
//! `{name}` is an argument, `{name|one|other}` a word by the number in `name` (the
//! forms a language needs, in its order: English one and other, Polish and Russian
//! one, few and many, Japanese and Chinese one form). An argument is a text, a
//! message of its own, or a list of them.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The languages carried, by Volumio's codes: English first, the source of the rest.
pub const LANGUAGES: [&str; 11] = [
    "en", "de", "es", "fr", "it", "ja", "nl", "pl", "pt", "ru", "zh",
];

/// The catalogue of a language as built in: flat keys to texts; none for a language
/// not carried.
pub fn catalogue(code: &str) -> Option<&'static str> {
    match code {
        "en" => Some(include_str!("../assets/lang/en.json")),
        _ => None,
    }
}

/// Volumio's code as one carried: as it is, else by its language part (`zh_TW` as
/// `zh`), else nothing.
pub fn carried(code: &str) -> Option<&'static str> {
    let code = code.trim();
    LANGUAGES.iter().copied().find(|l| *l == code).or_else(|| {
        let part = code
            .split(['_', '-'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        LANGUAGES.iter().copied().find(|l| *l == part)
    })
}

/// The language the pages are served in: the address's `lang=` when carried, else the
/// player's from the settings, else English; always one with a catalogue.
pub fn language(settings: &Path, query: &str) -> &'static str {
    let asked = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("lang="))
        .and_then(carried);
    [asked, player_language(settings)]
        .into_iter()
        .flatten()
        .find(|c| catalogue(c).is_some())
        .unwrap_or("en")
}

/// The player's language from the settings the shell writes; none when the file or
/// the field is not there, or the code is not one carried.
pub fn player_language(settings: &Path) -> Option<&'static str> {
    let text = fs::read_to_string(settings).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("language")
        .and_then(|l| l.as_str())
        .and_then(carried)
}

/// The slot in each page the catalogue goes into.
const SLOT: &str = "<script id=\"lang\" type=\"application/json\">{}</script>";

/// A page in a language: its catalogue in the slot, its `lang` attribute set.
pub fn page_for(page: &str, code: &str) -> String {
    inlined(page, code, catalogue(code).unwrap_or("{}"))
}

/// The page with the catalogue's JSON in the slot, `</` escaped so that no text in it
/// can end the script, and the `lang` attribute set to the code.
fn inlined(page: &str, code: &str, json: &str) -> String {
    let json = json.trim().replace("</", "<\\/");
    page.replacen("<html lang=\"en\">", &format!("<html lang=\"{code}\">"), 1)
        .replacen(
            SLOT,
            &format!("<script id=\"lang\" type=\"application/json\">{json}</script>"),
            1,
        )
}

/// An argument of a message: a text (a name, a number, a path), a message of its
/// own, or a list of them, which renders joined.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Arg {
    Text(String),
    Msg(Box<Msg>),
    List(Vec<Arg>),
}

/// What goes into a message as an argument.
pub trait ToArg {
    fn to_arg(&self) -> Arg;
}

impl<T: ToArg + ?Sized> ToArg for &T {
    fn to_arg(&self) -> Arg {
        (**self).to_arg()
    }
}

impl ToArg for str {
    fn to_arg(&self) -> Arg {
        Arg::Text(self.to_string())
    }
}

impl ToArg for String {
    fn to_arg(&self) -> Arg {
        Arg::Text(self.clone())
    }
}

/// A message as an argument; the words as they are go in as the text itself, which is
/// also how they read back.
impl ToArg for Msg {
    fn to_arg(&self) -> Arg {
        if self.k == RAW {
            Arg::Text(self.en.clone())
        } else {
            Arg::Msg(Box::new(self.clone()))
        }
    }
}

impl ToArg for Arg {
    fn to_arg(&self) -> Arg {
        self.clone()
    }
}

impl<T: ToArg> ToArg for Vec<T> {
    fn to_arg(&self) -> Arg {
        Arg::List(self.iter().map(ToArg::to_arg).collect())
    }
}

impl<T: ToArg> ToArg for [T] {
    fn to_arg(&self) -> Arg {
        Arg::List(self.iter().map(ToArg::to_arg).collect())
    }
}

macro_rules! arg_by_display {
    ($($t:ty),*) => {
        $(impl ToArg for $t {
            fn to_arg(&self) -> Arg {
                Arg::Text(self.to_string())
            }
        })*
    };
}
arg_by_display!(
    usize,
    u8,
    u16,
    u32,
    u64,
    i32,
    i64,
    std::path::Display<'_>,
    std::io::Error
);

impl Arg {
    /// The argument as English text: a message by its English, a list joined.
    fn en(&self) -> String {
        match self {
            Arg::Text(t) => t.clone(),
            Arg::Msg(m) => m.en.clone(),
            Arg::List(items) => items.iter().map(Arg::en).collect::<Vec<_>>().join(", "),
        }
    }
}

/// A placeholder as the templates write it: a name of lowercase letters and
/// underscores, then its plural forms after a bar, if any.
fn placeholder(inner: &str) -> Option<(&str, Option<&str>)> {
    let (name, forms) = match inner.split_once('|') {
        Some((n, f)) => (n, Some(f)),
        None => (inner, None),
    };
    let named = !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_');
    named.then_some((name, forms))
}

/// The names of a template's placeholders, in order, each once.
fn placeholders(template: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else { break };
        if let Some((name, _)) = placeholder(&after[..close]) {
            if !out.contains(&name) {
                out.push(name);
            }
        }
        rest = &after[close + 1..];
    }
    out
}

/// A template rendered in English: `{name}` by the argument, `{name|one|other}` by
/// the form the number in `name` takes (one for exactly 1); a placeholder with no
/// argument stays as written, so that it is seen.
pub fn fill(template: &str, args: &BTreeMap<String, Arg>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let whole = &rest[open..open + 1 + close + 1];
        match placeholder(&after[..close]) {
            Some((name, forms)) => match (args.get(name), forms) {
                (Some(arg), Some(forms)) => {
                    let forms: Vec<&str> = forms.split('|').collect();
                    let n: f64 = arg.en().trim().parse().unwrap_or(f64::NAN);
                    let index = if n == 1.0 { 0 } else { forms.len() - 1 };
                    out.push_str(forms[index]);
                }
                (Some(arg), None) => out.push_str(&arg.en()),
                (None, _) => out.push_str(whole),
            },
            None => out.push_str(whole),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// A sentence of the core's for the page: its key in the catalogue, its arguments,
/// and the English rendered, which the logs, the tests and a page without the key
/// show. `raw` is the key of a system's own words (an io error, MPD's reply, a file
/// name), shown as they are in every language; such a message is written as the
/// plain string, and read back from one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Msg {
    pub k: String,
    pub a: BTreeMap<String, Arg>,
    pub en: String,
}

/// The key of a system's own words.
pub const RAW: &str = "raw";

impl Msg {
    /// The key, the arguments, and the English template rendered with them. In a
    /// debug build the template's placeholders and the arguments are held to each
    /// other, so a test that renders the message finds a mismatch.
    pub fn new(key: &str, args: Vec<(&str, Arg)>, template: &str) -> Msg {
        let a: BTreeMap<String, Arg> = args.into_iter().map(|(n, v)| (n.to_string(), v)).collect();
        if cfg!(debug_assertions) {
            let named = placeholders(template);
            for n in a.keys() {
                assert!(
                    named.contains(&n.as_str()),
                    "{key}: the template does not name {{{n}}}: {template}"
                );
            }
            for n in &named {
                assert!(
                    a.contains_key(*n),
                    "{key}: no argument for {{{n}}}: {template}"
                );
            }
        }
        let en = fill(template, &a);
        Msg {
            k: key.to_string(),
            a,
            en,
        }
    }

    /// A system's own words, as they are.
    pub fn raw(text: impl Into<String>) -> Msg {
        Msg {
            k: RAW.to_string(),
            a: BTreeMap::new(),
            en: text.into(),
        }
    }
}

impl fmt::Display for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.en)
    }
}

/// As text, a message is its English: what the logs print and the tests read.
impl std::ops::Deref for Msg {
    type Target = str;
    fn deref(&self) -> &str {
        &self.en
    }
}

impl PartialEq<str> for Msg {
    fn eq(&self, other: &str) -> bool {
        self.en == other
    }
}

impl PartialEq<&str> for Msg {
    fn eq(&self, other: &&str) -> bool {
        self.en == *other
    }
}

impl From<&str> for Msg {
    fn from(text: &str) -> Msg {
        Msg::raw(text)
    }
}

impl From<String> for Msg {
    fn from(text: String) -> Msg {
        Msg::raw(text)
    }
}

/// Written as `{ k, a, en }`, the arguments left out when there are none; the words
/// as they are written as the plain string.
impl Serialize for Msg {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.k == RAW {
            return s.serialize_str(&self.en);
        }
        let mut st = s.serialize_struct("Msg", 3)?;
        st.serialize_field("k", &self.k)?;
        if self.a.is_empty() {
            st.skip_field("a")?;
        } else {
            st.serialize_field("a", &self.a)?;
        }
        st.serialize_field("en", &self.en)?;
        st.end()
    }
}

/// Read back as written, or from a plain string (the words as they are, and a
/// journal written before the keys).
impl<'de> Deserialize<'de> for Msg {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Msg, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Text(String),
            Full {
                k: String,
                #[serde(default)]
                a: BTreeMap<String, Arg>,
                en: String,
            },
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Text(t) => Msg::raw(t),
            Repr::Full { k, a, en } => Msg { k, a, en },
        })
    }
}

/// `msg!("core.x.y", "English with {n} {n|file|files} in {dir}", n = count, dir =
/// name)`: a `Msg` with the key, the arguments and the English rendered. The
/// catalogue carries the same template under the key, and the check holds the two to
/// each other, and the arguments to the placeholders.
macro_rules! msg {
    ($key:literal, $en:literal $(, $name:ident = $value:expr)* $(,)?) => {
        $crate::text::Msg::new(
            $key,
            vec![$((stringify!($name), $crate::text::ToArg::to_arg(&$value))),*],
            $en,
        )
    };
}

/// A system's words about a path, "{path}: {error}", the shape every io error takes.
pub fn at(path: impl fmt::Display, error: impl fmt::Display) -> Msg {
    msg!(
        "core.io.at",
        "{path}: {error}",
        path = path.to_string(),
        error = error.to_string()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volumios_code_is_carried_as_it_is_or_by_its_language_part() {
        assert_eq!(carried("en"), Some("en"));
        assert_eq!(carried("zh_TW"), Some("zh"));
        assert_eq!(carried(" de "), Some("de"));
        assert_eq!(carried("pt-BR"), Some("pt"));
        assert_eq!(carried("gr"), None);
        assert_eq!(carried(""), None);
        for code in LANGUAGES {
            assert_eq!(carried(code), Some(code));
        }
    }

    #[test]
    fn the_language_is_the_addresss_then_the_players_then_english_always_with_a_catalogue() {
        let dir = std::env::temp_dir().join(format!("mpdantic-text-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        let _ = fs::remove_file(&settings);
        assert_eq!(language(&settings, ""), "en");
        assert_eq!(player_language(&settings), None);
        fs::write(&settings, "{ \"port\": 6218, \"language\": \"gr\" }\n").unwrap();
        assert_eq!(player_language(&settings), None);
        assert_eq!(language(&settings, "tab=cue"), "en");
        fs::write(&settings, "{ \"port\": 6218, \"language\": \"zh_TW\" }\n").unwrap();
        assert_eq!(player_language(&settings), Some("zh"));
        // The player's language serves when its catalogue is built in, else English.
        let expected = if catalogue("zh").is_some() {
            "zh"
        } else {
            "en"
        };
        assert_eq!(language(&settings, ""), expected);
        assert_eq!(language(&settings, "lang=xx&tab=cue"), expected);
        // The address decides before the player's setting.
        assert_eq!(language(&settings, "tab=cue&lang=en"), "en");
        fs::write(&settings, "not json").unwrap();
        assert_eq!(language(&settings, ""), "en");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_english_catalogue_is_built_in_and_parses_to_flat_texts() {
        let value: serde_json::Value = serde_json::from_str(catalogue("en").unwrap()).unwrap();
        let map = value.as_object().unwrap();
        assert!(map.contains_key("page.state.up"));
        assert!(map.values().all(|v| v.is_string()));
        assert_eq!(catalogue("xx"), None);
    }

    #[test]
    fn a_page_gets_its_catalogue_in_the_slot_and_its_lang_attribute_set() {
        let page = "<!doctype html>\n<html lang=\"en\">\n<head><script id=\"lang\" type=\"application/json\">{}</script></head>\n<body><script>'x';</script></body></html>";
        let out = inlined(page, "de", "{\n  \"a\": \"one </script> two\"\n}\n");
        assert!(out.starts_with("<!doctype html>\n<html lang=\"de\">"));
        assert!(out
            .contains("type=\"application/json\">{\n  \"a\": \"one <\\/script> two\"\n}</script>"));
        assert_eq!(out.matches("</script>").count(), 2);
        let value: serde_json::Value =
            serde_json::from_str("{\n  \"a\": \"one <\\/script> two\"\n}").unwrap();
        assert_eq!(value["a"], "one </script> two");
        assert!(page_for(page, "en").contains("\"page.state.up\""));
    }

    #[test]
    fn a_template_is_filled_with_its_arguments_and_the_english_plural_forms() {
        let args: BTreeMap<String, Arg> = [
            ("n".to_string(), Arg::Text("1".to_string())),
            ("m".to_string(), Arg::Text("3".to_string())),
            ("dir".to_string(), Arg::Text("a/b".to_string())),
        ]
        .into_iter()
        .collect();
        assert_eq!(fill("{n} {n|file|files} in {dir}", &args), "1 file in a/b");
        assert_eq!(fill("{m} {m|file|files}", &args), "3 files");
        assert_eq!(fill("{m|a file}", &args), "a file");
        assert_eq!(fill("{dir|one|other}", &args), "other");
        // A placeholder without an argument stays, and braces that name nothing are text.
        assert_eq!(fill("{x} of {n}", &args), "{x} of 1");
        assert_eq!(fill("a { b", &args), "a { b");
        assert_eq!(fill("{Not a name} {1}", &args), "{Not a name} {1}");
        assert_eq!(
            placeholders("{n} {n|file|files} in {dir}, {Not}"),
            vec!["n", "dir"]
        );
    }

    #[test]
    fn a_message_carries_its_key_its_arguments_and_the_english_rendered() {
        let m = msg!(
            "core.test.files",
            "{n} {n|file|files} in {dir}",
            n = 3,
            dir = "a/b"
        );
        assert_eq!(m.k, "core.test.files");
        assert_eq!(m.en, "3 files in a/b");
        assert_eq!(m.to_string(), "3 files in a/b");
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            "{\"k\":\"core.test.files\",\"a\":{\"dir\":\"a/b\",\"n\":\"3\"},\"en\":\"3 files in a/b\"}"
        );
        let plain = msg!("core.test.none", "Nothing here.");
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            "{\"k\":\"core.test.none\",\"en\":\"Nothing here.\"}"
        );
        // The words as they are: written as the plain string.
        let raw = Msg::raw("No such file or directory (os error 2)");
        assert_eq!(raw.k, RAW);
        assert_eq!(
            serde_json::to_string(&raw).unwrap(),
            "\"No such file or directory (os error 2)\""
        );
        // An argument that is a message, and one that is a list of them.
        let inner = msg!("core.test.inner", "line {n}", n = 2);
        let nested = msg!(
            "core.test.nested",
            "at {place}: {items}",
            place = inner,
            items = vec![plain.clone(), raw.clone()]
        );
        assert_eq!(
            nested.en,
            "at line 2: Nothing here., No such file or directory (os error 2)"
        );
        let json = serde_json::to_string(&nested).unwrap();
        assert!(json.contains(
            "\"place\":{\"k\":\"core.test.inner\",\"a\":{\"n\":\"2\"},\"en\":\"line 2\"}"
        ));
        assert!(json.contains("\"items\":[{\"k\":\"core.test.none\",\"en\":\"Nothing here.\"},\"No such file or directory (os error 2)\"]"));
        // Read back as written, and from a plain string.
        assert_eq!(serde_json::from_str::<Msg>(&json).unwrap(), nested);
        assert_eq!(
            serde_json::from_str::<Msg>(&serde_json::to_string(&m).unwrap()).unwrap(),
            m
        );
        assert_eq!(
            serde_json::from_str::<Msg>("\"the old note\"").unwrap(),
            Msg::raw("the old note")
        );
        assert_eq!(
            serde_json::from_str::<Msg>("{\"k\":\"core.test.none\",\"en\":\"Nothing here.\"}")
                .unwrap(),
            plain
        );
        assert_eq!(at("a/b", "gone").en, "a/b: gone");
    }

    #[test]
    #[should_panic(expected = "no argument for {n}")]
    fn a_placeholder_without_an_argument_is_refused_in_a_debug_build() {
        let _ = Msg::new("core.test.bad", Vec::new(), "{n} files");
    }
}
