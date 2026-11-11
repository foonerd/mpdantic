//! The languages of the pages and of the core's own sentences. The pages are served
//! in the player's language (the shell writes Volumio's code into the settings, and
//! `lang=` in the address overrides it), each with its catalogue built into it; a
//! sentence of the core's travels as a `Msg`, its key and its arguments for the page
//! to render from the catalogue, the English rendered beside them.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize};

/// The languages carried, by Volumio's codes: English first, the source of the rest.
pub const LANGUAGES: [&str; 11] = [
    "en", "de", "es", "fr", "it", "ja", "nl", "pl", "pt", "ru", "zh",
];

/// The catalogue of a language as built in: flat keys to texts, with `{name}`
/// placeholders; none for a language not carried.
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

/// A sentence of the core's for the page: its key in the catalogue, its arguments as
/// text, and the English rendered, which the logs, the tests and a page without the
/// key show. `raw` is the key of a system's own words (an io error, MPD's reply),
/// shown as they are in every language.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Msg {
    pub k: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub a: BTreeMap<String, String>,
    pub en: String,
}

/// The key of a system's own words.
pub const RAW: &str = "raw";

impl Msg {
    pub fn new(key: &str, args: Vec<(&str, String)>, en: String) -> Msg {
        Msg {
            k: key.to_string(),
            a: args.into_iter().map(|(n, v)| (n.to_string(), v)).collect(),
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

/// Read back as written, or from a plain string (a journal written before the keys),
/// which is the words as they are.
impl<'de> Deserialize<'de> for Msg {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Msg, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Text(String),
            Full {
                k: String,
                #[serde(default)]
                a: BTreeMap<String, String>,
                en: String,
            },
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Text(t) => Msg::raw(t),
            Repr::Full { k, a, en } => Msg { k, a, en },
        })
    }
}

/// `msg!("core.x.y", "English with {n} and {name}", n = count, name = x)`: a `Msg`
/// with the key, the arguments as text and the English rendered. The catalogue
/// carries the same English under the key, and the check holds the two to each other.
macro_rules! msg {
    ($key:literal, $en:literal $(, $name:ident = $value:expr)* $(,)?) => {{
        $(let $name = $value;)*
        $crate::text::Msg::new(
            $key,
            vec![$((stringify!($name), $name.to_string())),*],
            ::std::fmt::format(::std::format_args!($en $(, $name = $name)*)),
        )
    }};
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
    fn a_message_carries_its_key_its_arguments_and_the_english_rendered() {
        let m = msg!("core.test.files", "{n} files in {dir}", n = 3, dir = "a/b");
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
        let raw = Msg::raw("No such file or directory (os error 2)");
        assert_eq!(raw.k, RAW);
        assert_eq!(
            serde_json::to_string(&raw).unwrap(),
            "{\"k\":\"raw\",\"en\":\"No such file or directory (os error 2)\"}"
        );
        // Read back as written, and from a plain string.
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
    }
}
