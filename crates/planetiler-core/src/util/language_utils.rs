//! Port of `util/LanguageUtils.java`: utilities to extract common name fields (name, name_en, name_de, ...) that the
//! OpenMapTiles schema uses.

use std::sync::LazyLock;

use regex::Regex;

use crate::util::translations;
use crate::value::{Tags, Value};

/// Name tags that should be eligible for finding a latin name.
///
/// See <https://wiki.openstreetmap.org/wiki/Multilingual_names>.
#[deprecated(note = "Use is_valid_osm_name_tag")]
pub static VALID_NAME_TAGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^name:[a-z]{2,3}(-[A-Z][a-z]{3})?([-_](x-)?[a-z]{2,})?(-([A-Z]{2}|\d{3}))?$").unwrap());

// See https://github.com/onthegomap/planetiler/issues/86
// Match strings that only contain latin characters.
static ONLY_LATIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\P{L}[\p{L}&&\p{sc=Latin}]]+$").unwrap());
// Match only latin letters
static LATIN_LETTER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}&&\p{sc=Latin}]+").unwrap());
static EMPTY_PARENS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\([ -.]*\)|\[[ -.]*\])").unwrap());
// Java's \s only matches ASCII whitespace
static LEADING_TRAILING_JUNK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"((^[ \t\n\x0B\f\r./-]*)|([ \t\n\x0B\f\r./-]*$))").unwrap());
static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\n\x0B\f\r]+").unwrap());

pub const EN_DE_NAME_KEYS: [&str; 2] = ["name:en", "name:de"];

/// Java's `String.trim()`: strips leading/trailing characters `<= ' '`.
fn java_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c <= ' ')
}

/// Puts `value` into `dest` under `key` unless it is null or an empty string.
pub fn put_if_not_empty(dest: &mut Tags, key: &str, value: impl Into<Value>) {
    let value = value.into();
    if !value.is_null() && value.as_str() != Some("") {
        dest.insert(key.to_string(), value);
    }
}

pub fn null_if_empty(a: Option<&str>) -> Option<&str> {
    a.filter(|s| !s.is_empty())
}

#[deprecated(note = "Use OmtLanguageUtils::string()")]
pub fn string(obj: Option<&Value>) -> Option<String> {
    obj.filter(|v| !v.is_null()).map(|v| v.to_string()).filter(|s| !s.is_empty())
}

pub fn contains_only_latin_characters(string: Option<&str>) -> bool {
    string.is_some_and(|s| ONLY_LATIN.is_match(s))
}

#[deprecated(note = "Use translations::transliterate(string(tags.get(\"name\")))")]
#[allow(deprecated)]
pub fn transliterated_name(tags: &Tags) -> Option<String> {
    string(tags.get("name")).map(|s| translations::transliterate(&s))
}

pub fn remove_latin_characters(name: Option<&str>) -> Option<String> {
    let name = name?;
    if LATIN_LETTER.is_match(name) {
        let result = LATIN_LETTER.replace_all(name, "");
        // if the name was "<nonlatin text> (<latin description)"
        // or "<nonlatin text> - <latin description>"
        // then remove any of those extra characters now
        let result = EMPTY_PARENS.replace_all(&result, "");
        let result = LEADING_TRAILING_JUNK.replace_all(&result, "");
        let result = WHITESPACE.replace_all(&result, " ");
        let result = java_trim(&result);
        if result.chars().all(char::is_whitespace) { None } else { Some(result.to_string()) }
    } else {
        Some(java_trim(name).to_string())
    }
}

/// Legacy (grandfathered) BCP 47 tags and the language of their preferred mapping (from Java's `LanguageTag`).
const LEGACY_TAGS: &[(&str, &str)] = &[
    ("art-lojban", "jbo"),
    ("cel-gaulish", "xtg"),
    ("en-gb-oed", "en"),
    ("i-ami", "ami"),
    ("i-bnn", "bnn"),
    ("i-default", "en"),
    ("i-enochian", "und"),
    ("i-hak", "hak"),
    ("i-klingon", "tlh"),
    ("i-lux", "lb"),
    ("i-mingo", "see"),
    ("i-navajo", "nv"),
    ("i-pwn", "pwn"),
    ("i-tao", "tao"),
    ("i-tay", "tay"),
    ("i-tsu", "tsu"),
    ("no-bok", "nb"),
    ("no-nyn", "nn"),
    ("sgn-be-fr", "sfb"),
    ("sgn-be-nl", "vgt"),
    ("sgn-ch-de", "sgg"),
    ("zh-guoyu", "cmn"),
    ("zh-hakka", "hak"),
    ("zh-min", "nan"),
    ("zh-min-nan", "nan"),
    ("zh-xiang", "hsn"),
];

fn is_alpha(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphabetic())
}

fn is_alnum(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Parses a BCP 47 language tag like Java's `Locale.Builder.setLanguageTag` and returns the language subtag (empty
/// for private-use-only tags), or `None` if it is ill-formed.
fn parse_language_tag(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    if let Some((_, lang)) = LEGACY_TAGS.iter().find(|(t, _)| *t == lower) {
        return Some(if *lang == "und" { String::new() } else { lang.to_string() });
    }
    let subtags: Vec<&str> = lower.split('-').collect();
    let mut i = 0;
    let n = subtags.len();
    let len_in = |s: &str, min: usize, max: usize| s.len() >= min && s.len() <= max;

    let mut language = String::new();
    // language
    if len_in(subtags[0], 2, 8) && is_alpha(subtags[0]) {
        language = subtags[0].to_string();
        i += 1;
        // extlang
        if language.len() <= 3 {
            let mut ext = 0;
            while i < n && ext < 3 && subtags[i].len() == 3 && is_alpha(subtags[i]) {
                i += 1;
                ext += 1;
            }
        }
        // script
        if i < n && subtags[i].len() == 4 && is_alpha(subtags[i]) {
            i += 1;
        }
        // region
        if i < n
            && ((subtags[i].len() == 2 && is_alpha(subtags[i]))
                || (subtags[i].len() == 3 && subtags[i].bytes().all(|b| b.is_ascii_digit())))
        {
            i += 1;
        }
        // variants
        let mut variants = Vec::new();
        while i < n {
            let s = subtags[i];
            let is_variant = is_alnum(s)
                && (len_in(s, 5, 8) || (s.len() == 4 && s.as_bytes()[0].is_ascii_digit()));
            if !is_variant {
                break;
            }
            if variants.contains(&s) {
                return None;
            }
            variants.push(s);
            i += 1;
        }
        // extensions
        let mut singletons = Vec::new();
        while i < n && subtags[i].len() == 1 && subtags[i] != "x" && is_alnum(subtags[i]) {
            if singletons.contains(&subtags[i]) {
                return None;
            }
            singletons.push(subtags[i]);
            i += 1;
            let start = i;
            while i < n && len_in(subtags[i], 2, 8) && is_alnum(subtags[i]) {
                i += 1;
            }
            if i == start {
                return None;
            }
        }
    }
    // private use
    if i < n && subtags[i] == "x" {
        i += 1;
        let start = i;
        while i < n && len_in(subtags[i], 1, 8) && is_alnum(subtags[i]) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    if i != n || (language.is_empty() && i == 0) {
        return None;
    }
    Some(language)
}

/// Returns whether a BCP 47 language tag conforms OpenStreetMap's conventions for a localized name subkey.
pub fn is_valid_language_tag(tag: &str) -> bool {
    // Locale maps the empty string to "und", but we wouldn't use a subkey for that.
    let Some(first) = tag.chars().next() else { return false };
    // A subkey starting with a capital letter likely qualifies a name by the organization that assigned it.
    if !first.is_lowercase() {
        return false;
    }
    // Check whether the code is well-formed according to BCP 47.
    match parse_language_tag(tag) {
        // BCP 47 technically allows a language code up to 8 characters long for future use.
        // Such a long subkey is likely to be something other than a language tag.
        Some(lang) => !lang.is_empty() && lang.len() <= 3,
        None => false,
    }
}

/// Returns whether a key conforms to OpenStreetMap's conventions for a localized name subkey, but not the primary name
/// key or a subkey of some other kind of name such as the short name.
pub fn is_valid_osm_name_tag(tag: &str) -> bool {
    tag.strip_prefix("name:").is_some_and(is_valid_language_tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_non_latin() {
        let cases: &[(Option<&str>, Option<&str>)] = &[
            (None, None),
            (Some("abcaāíìś+"), Some("+")),
            (Some("abcaāíìś"), None),
            (Some("日本"), Some("日本")),
            (Some("abca日āíìś+"), Some("日+")),
            (Some("(abc)"), None),
            (Some("日本 (Japan)"), Some("日本")),
            (Some("日本 [Japan - Nippon]"), Some("日本")),
            (Some("  Japan - Nippon (Japan) - Japan - 日本 - Japan - Nippon (Japan)"), Some("日本")),
            (Some("Japan - 日本~+  "), Some("日本~+")),
            (Some("Japan / 日本 / Japan  "), Some("日本")),
        ];
        for (input, output) in cases {
            assert_eq!(output.map(String::from), remove_latin_characters(*input), "{input:?}");
        }
    }

    #[test]
    fn test_contains_only_latin() {
        assert!(contains_only_latin_characters(Some("abc ÄÖÜ 123 - ()")));
        assert!(!contains_only_latin_characters(Some("abc 日本")));
        assert!(!contains_only_latin_characters(Some("")));
        assert!(!contains_only_latin_characters(None));
    }

    #[test]
    fn test_is_valid_language_tag() {
        for tag in [
            "es",
            "en-US",
            "en-001",
            "fr-x-gallo",
            "ko-Latn",
            "be-tarask",
            "ja-Latn",
            "ja-Hira",
            "vls",
            "zh-hant-CN",
            "zh-Bopo",
            "zh-Latn-tongyong",
            "zh-Latn-pinyin",
            "zh-Latn-wadegile",
            "yue-Latn-jyutping",
            "tec",
            "be-tarask",
            "nan-Latn-pehoeji",
            "zh-Latn-pinyin",
            "en-t-zh",
            "zh-u-nu-hant",
            "en-u-sd-usnc",
            "es-fonipa",
            "fr-x-gallo",
            "i-mingo",
        ] {
            assert!(is_valid_language_tag(tag), "{tag}");
        }
    }

    #[test]
    fn test_is_not_valid_language_tag() {
        for tag in [
            "nombre",
            "",
            "xxxxx",
            "TEC",
            "en-x",
            "ja_rm",
            "ja_kana",
            "zh_pinyin",
            "zh_zhuyin",
            "zh-Latn-wadegiles",
            "etymology",
            "etymology:wikidata",
            "x-private",
            "en--us",
            "en-",
        ] {
            assert!(!is_valid_language_tag(tag), "{tag}");
        }
    }

    #[test]
    fn test_is_valid_osm_name_tag() {
        assert!(is_valid_osm_name_tag("name:tlh"));
        for tag in ["name", "name:", "name:TEC", "official_name:en-US"] {
            assert!(!is_valid_osm_name_tag(tag), "{tag}");
        }
    }

    #[test]
    #[allow(deprecated)]
    fn test_helpers() {
        let mut tags = Tags::new();
        put_if_not_empty(&mut tags, "a", "");
        put_if_not_empty(&mut tags, "b", Value::Null);
        put_if_not_empty(&mut tags, "c", "x");
        put_if_not_empty(&mut tags, "d", 1);
        assert_eq!(crate::tags! {"c" => "x", "d" => 1}, tags);
        assert_eq!(None, null_if_empty(Some("")));
        assert_eq!(Some("a"), null_if_empty(Some("a")));
        assert_eq!(None, string(Some(&Value::str(""))));
        assert_eq!(Some("1".to_string()), string(Some(&Value::Int(1))));
        assert!(VALID_NAME_TAGS.is_match("name:en-US"));
        assert_eq!(Some("rì běn".to_string()), transliterated_name(&crate::tags! {"name" => "日本"}));
    }
}
