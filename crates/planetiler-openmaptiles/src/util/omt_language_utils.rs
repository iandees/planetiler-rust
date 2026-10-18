/*
Copyright (c) 2021, MapTiler.com & OpenMapTiles contributors.
All rights reserved.

Code license: BSD 3-Clause License

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice,
  this list of conditions and the following disclaimer in the documentation
  and/or other materials provided with the distribution.

* Neither the name of the copyright holder nor the names of its
  contributors may be used to endorse or promote products derived from
  this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Design license: CC-BY 4.0

See https://github.com/openmaptiles/openmaptiles/blob/master/LICENSE.md for details on usage
*/
//! Port of `util/OmtLanguageUtils.java`: utilities to extract common name fields (name, name_en, name_de,
//! name:latin, name:nonlatin, name_int) that the OpenMapTiles schema uses across any map element with a name.
//!
//! Ported from [openmaptiles-tools](https://github.com/openmaptiles/openmaptiles-tools/blob/master/sql/zzz_language.sql).

use std::borrow::Cow;

use planetiler_core::util::language_utils::{
    EN_DE_NAME_KEYS, contains_only_latin_characters, is_valid_osm_name_tag, put_if_not_empty, remove_latin_characters,
};
use planetiler_core::util::translations::{self, Translations};
use planetiler_core::value::{Tags, Value};

/// Returns a map with default name attributes (name, name_en, name_de, name:latin, name:nonlatin, name_int) that
/// every element should have, derived from name, int_name, name:en, and name:de tags on the input element.
///
/// - name is the original name value from the element
/// - name_en is the original name:en value from the element, or name if missing
/// - name_de is the original name:de value from the element, or name/ name_en if missing
/// - name:latin is the first of name, int_name, or any name: attribute that contains only latin characters
/// - name:nonlatin is any nonlatin part of name if present
/// - name_int is the first of int_name name:en name:latin name
pub fn get_names_without_translations(tags: &Tags) -> Tags {
    get_names(tags, None)
}

/// Returns a map with default name attributes that [`get_names_without_translations`] adds, but also translations
/// for every language that `translations` is configured to handle.
pub fn get_names(tags: &Tags, translations: Option<&Translations>) -> Tags {
    let mut result = Tags::new();

    let name = string(tags.get("name"));
    let int_name = string(tags.get("int_name"));
    let name_en = string(tags.get("name:en"));
    let name_de = string(tags.get("name:de"));

    let is_latin = contains_only_latin_characters(name.as_deref());
    let mut latin: Option<Cow<'_, str>> = if is_latin {
        name.clone()
    } else {
        [&name_en, &int_name, &name_de]
            .into_iter()
            .cloned()
            .chain(all_name_translations_besides_english_and_german(tags))
            .find(|s| contains_only_latin_characters(s.as_deref()))
            .flatten()
    };
    if latin.is_none() && translations.is_some_and(Translations::get_should_transliterate) {
        latin = transliterated_name(tags).map(Cow::Owned);
    }
    let mut non_latin = if is_latin { None } else { remove_latin_characters(name.as_deref()) };
    if latin.as_deref().is_some_and(|l| non_latin.as_deref().unwrap_or("") == l) {
        non_latin = None;
    }

    put_if_not_empty(&mut result, "name", value(&name));
    put_if_not_empty(&mut result, "name_en", value(&name_en.clone().or_else(|| name.clone())));
    put_if_not_empty(&mut result, "name_de", value(&name_de.or_else(|| name.clone()).or_else(|| name_en.clone())));
    put_if_not_empty(&mut result, "name:latin", value(&latin));
    put_if_not_empty(&mut result, "name:nonlatin", non_latin.map(Value::from).unwrap_or(Value::Null));
    put_if_not_empty(&mut result, "name_int", value(&int_name.or(name_en).or(latin).or(name)));

    if let Some(translations) = translations {
        translations.add_translations(&mut result, tags);
    }

    result
}

fn value(s: &Option<Cow<'_, str>>) -> Value {
    s.as_deref().map(Value::from).unwrap_or(Value::Null)
}

/// The string value of `obj`, or `None` if missing or empty.
pub fn string(obj: Option<&Value>) -> Option<Cow<'_, str>> {
    match obj {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.to_string_lossy()).filter(|s| !s.is_empty()),
    }
}

/// The transliterated `name` tag.
pub fn transliterated_name(tags: &Tags) -> Option<String> {
    string(tags.get("name")).map(|s| translations::transliterate(&s))
}

fn all_name_translations_besides_english_and_german(tags: &Tags) -> impl Iterator<Item = Option<Cow<'_, str>>> {
    tags.iter()
        .filter(|(k, _)| !EN_DE_NAME_KEYS.contains(&k.as_str()) && is_valid_osm_name_tag(k))
        .map(|(_, v)| string(Some(v)))
}

#[cfg(test)]
mod tests {
    use planetiler_core::tags;

    use super::*;
    use crate::test_support::{assert_submap, test_translations};

    fn get(result: &Tags, key: &str) -> Option<String> {
        result.get(key).map(|v| v.to_string())
    }

    #[test]
    fn test_simple_example() {
        let (_, translations) = test_translations();
        assert_submap(
            &tags! {"name" => "name", "name_en" => "english name", "name_de" => "german name"},
            &get_names(
                &tags! {"name" => "name", "name:en" => "english name", "name:de" => "german name"},
                Some(&translations),
            ),
        );
        assert_submap(
            &tags! {"name" => "name", "name_en" => "name", "name_de" => "german name"},
            &get_names(&tags! {"name" => "name", "name:de" => "german name"}, Some(&translations)),
        );
        assert_submap(
            &tags! {"name" => "name", "name_en" => "english name", "name_de" => "name"},
            &get_names(&tags! {"name" => "name", "name:en" => "english name"}, Some(&translations)),
        );
    }

    #[test]
    fn test_is_latin() {
        let (_, translations) = test_translations();
        for (input, is_latin) in [
            ("abc", true),
            ("5!", true),
            ("5~", true),
            ("é", true),
            ("éś", true),
            ("ɏə", true),
            ("ɐ", true),
            ("ᵿἀ", false),
            ("Ḁỿ", true),
            ("\u{02ff}\u{0370}", false),
            ("\u{0030}\u{036f}", true),
            ("日本", false),
            ("abc本123", false),
        ] {
            if !is_latin {
                assert!(!contains_only_latin_characters(Some(input)), "{input}");
            } else {
                assert_eq!(
                    Some(input.to_string()),
                    get(&get_names(&tags! {"name" => input}, Some(&translations)), "name:latin"),
                    "{input}"
                );
            }
        }
    }

    #[test]
    fn test_remove_non_latin() {
        let (_, translations) = test_translations();
        for (input, out) in [
            ("abcaāíìś+", None),
            ("abca日āíìś+", Some("日+")),
            ("(abc)", None),
            ("日本 (Japan)", Some("日本")),
            ("日本 [Japan - Nippon]", Some("日本")),
            ("  Japan - Nippon (Japan) - Japan - 日本 - Japan - Nippon (Japan)", Some("日本")),
            ("Japan - 日本~+  ", Some("日本~+")),
            ("Japan / 日本 / Japan  ", Some("日本")),
        ] {
            assert_eq!(
                out.map(String::from),
                get(&get_names(&tags! {"name" => input}, Some(&translations)), "name:nonlatin"),
                "{input}"
            );
        }
    }

    #[test]
    fn test_latin_fallbacks() {
        let (_, translations) = test_translations();
        for key in [
            // OSM tags that SHOULD be eligible for name:latin feature in the output
            "name:en",
            "name:en-US",
            "name:en-010",
            "int_name",
            "name:fr",
            "name:es",
            "name:pt",
            "name:de",
            "name:ar",
            "name:it",
            "name:ko-Latn",
            "name:be-tarask",
            // https://wiki.openstreetmap.org/wiki/Multilingual_names#Japan
            "name:ja",
            "name:ja-Latn",
            // https://wiki.openstreetmap.org/wiki/Multilingual_names#China
            "name:zh-CN",
            "name:zh-hant-CN",
            "name:zh-Latn-tongyong",
            "name:zh-Latn-pinyin",
            "name:yue-Latn-jyutping",
            // https://wiki.openstreetmap.org/wiki/Multilingual_names#France
            "name:fr",
            "name:fr-x-gallo",
            "name:br",
            "name:oc",
            "name:vls",
            "name:frp",
            "name:gcf",
            "name:gsw",
        ] {
            assert_eq!(
                Some("a".into()),
                get(&get_names(&tags! {key => "a"}, Some(&translations)), "name:latin"),
                "{key}"
            );
            assert_eq!(None, get(&get_names(&tags! {key => "ア"}, Some(&translations)), "name:latin"), "{key}");
            assert_eq!(None, get(&get_names(&tags! {key => "غ"}, Some(&translations)), "name:latin"), "{key}");
        }
    }

    #[test]
    fn test_no_latin_fallback() {
        let (_, translations) = test_translations();
        for key in [
            // OSM tags that should NOT be eligible for name:latin feature in the output
            "name:signed",
            "name:prefix",
            "name:abbreviation",
            "name:source",
            "name:full",
            "name:adjective",
            "name:proposed",
            "name:pronunciation",
            "name:etymology",
            "name:etymology:wikidata",
            "name:etymology:wikipedia",
            "name:etymology:right",
            "name:etymology:left",
            "name:genitive",
        ] {
            assert_submap(
                &tags! {
                    "name" => "Branch Hill–Loveland Road",
                    "name_en" => "Branch Hill–Loveland Road",
                    "name_de" => "Branch Hill–Loveland Road",
                    "name:latin" => "Branch Hill–Loveland Road",
                    "name_int" => "Branch Hill–Loveland Road",
                },
                &get_names(
                    &tags! {"name" => "Branch Hill–Loveland Road", key => "Q22133584;Q843993"},
                    Some(&translations),
                ),
            );
            assert_submap(
                &tags! {"name" => "日", "name_en" => "日", "name_de" => "日", "name:latin" => "rì", "name_int" => "rì"},
                // don't use this latin string with invalid name keys
                &get_names(&tags! {"name" => "日", key => "other"}, Some(&translations)),
            );
        }
    }

    #[test]
    fn test_transliterate() {
        for (input, out) in [
            ("キャンパス", "kyanpasu"),
            ("Αλφαβητικός Κατάλογος", "Alphabētikós Katálogos"),
            ("биологическом", "biologičeskom"),
        ] {
            let (_, mut translations) = test_translations();
            assert_eq!(
                Some(out.to_string()),
                get(&get_names(&tags! {"name" => input}, Some(&translations)), "name:latin"),
                "{input}"
            );
            translations.set_should_transliterate(false);
            assert_eq!(None, get(&get_names(&tags! {"name" => input}, Some(&translations)), "name:latin"), "{input}");
        }
    }

    #[test]
    fn test_use_wikidata() {
        let (wikidata, translations) = test_translations();
        wikidata.put(123, "es", "es name");
        assert_submap(
            &tags! {"name:es" => "es name"},
            &get_names(&tags! {"name" => "name", "wikidata" => "Q123"}, Some(&translations)),
        );
    }

    #[test]
    fn test_use_osm() {
        let (_, translations) = test_translations();
        assert_submap(
            &tags! {"name:es" => "es name osm"},
            &get_names(
                &tags! {"name" => "name", "wikidata" => "Q123", "name:es" => "es name osm"},
                Some(&translations),
            ),
        );
    }

    #[test]
    fn test_prefer_osm() {
        let (wikidata, translations) = test_translations();
        wikidata.put(123, "es", "wd es name");
        wikidata.put(123, "de", "wd de name");
        assert_submap(
            &tags! {"name:es" => "wd es name", "name:de" => "de name osm"},
            &get_names(
                &tags! {"name" => "name", "wikidata" => "Q123", "name:de" => "de name osm"},
                Some(&translations),
            ),
        );
    }

    #[test]
    fn test_dont_use_translations_when_not_specified() {
        let result = get_names_without_translations(&tags! {
            "name" => "name",
            "wikidata" => "Q123",
            "name:es" => "es name osm",
            "name:de" => "de name osm",
        });
        assert_eq!(None, result.get("name:es"));
        assert_eq!(None, result.get("name:de"));
        assert_eq!(Some("name"), result.get("name").and_then(Value::as_str));
    }

    #[test]
    fn test_ignore_languages() {
        let (wikidata, _) = test_translations();
        wikidata.put(123, "ja", "ja name wd");
        let result =
            get_names_without_translations(&tags! {"name" => "name", "wikidata" => "Q123", "name:ja" => "ja name osm"});
        assert_eq!(None, result.get("name:ja"));
    }
}
