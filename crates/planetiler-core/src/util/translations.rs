//! Port of `util/Translations.java`: holds planetiler configuration and utilities for translating element names to
//! other languages.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use crate::util::language_utils;
use crate::util::thread_local_transliterator::{ThreadLocalTransliterator, TransliteratorInstance};
use crate::value::{Tags, Value};

thread_local! {
    static TRANSLITERATOR: TransliteratorInstance = ThreadLocalTransliterator::new().get_instance("Any-Latin");
}

/// A source of name translations.
pub trait TranslationProvider: Send + Sync {
    /// Returns a map from language tag to translated name for a given input feature with `tags`.
    fn get_name_translations(&self, tags: &Tags) -> Option<Cow<'_, BTreeMap<String, String>>>;
}

impl<F> TranslationProvider for F
where
    F: Fn(&Tags) -> BTreeMap<String, String> + Send + Sync,
{
    fn get_name_translations(&self, tags: &Tags) -> Option<Cow<'_, BTreeMap<String, String>>> {
        Some(Cow::Owned(self(tags)))
    }
}

/// Extracts translations from `name:lang` tags on an OSM element.
struct OsmTranslationProvider;

impl TranslationProvider for OsmTranslationProvider {
    fn get_name_translations(&self, tags: &Tags) -> Option<Cow<'_, BTreeMap<String, String>>> {
        let result: BTreeMap<String, String> = tags
            .iter()
            .filter(|(key, _)| language_utils::is_valid_osm_name_tag(key))
            .filter_map(|(key, value)| match value {
                Value::String(s) => Some((key.clone(), s.to_string())),
                _ => None,
            })
            .collect();
        Some(Cow::Owned(result))
    }
}

/// Configuration and utilities for translating element names to other languages.
#[derive(Clone)]
pub struct Translations {
    should_transliterate: bool,
    providers: Vec<Arc<dyn TranslationProvider>>,
    include_languages: HashSet<String>,
    exclude_languages: HashSet<String>,
    default_include: bool,
    extra_name_tags: Vec<String>,
}

impl std::fmt::Debug for Translations {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Translations")
            .field("should_transliterate", &self.should_transliterate)
            .field("providers", &self.providers.len())
            .field("include_languages", &self.include_languages)
            .field("exclude_languages", &self.exclude_languages)
            .field("default_include", &self.default_include)
            .field("extra_name_tags", &self.extra_name_tags)
            .finish()
    }
}

impl Translations {
    fn new<S: AsRef<str>>(languages: &[S]) -> Translations {
        let mut result = Translations {
            should_transliterate: true,
            providers: Vec::new(),
            include_languages: HashSet::new(),
            exclude_languages: HashSet::new(),
            default_include: false,
            extra_name_tags: Vec::new(),
        };
        for language in languages {
            let language = language.as_ref();
            if language == "*" {
                result.default_include = true;
                continue;
            }
            let (language, include) = match language.strip_prefix('-') {
                Some(rest) => (rest, false),
                None => (language, true),
            };
            let without_prefix = language.strip_prefix("name:").unwrap_or(language);
            let set = if include { &mut result.include_languages } else { &mut result.exclude_languages };
            set.insert(without_prefix.to_string());
            set.insert(format!("name:{without_prefix}"));
        }
        result
    }

    /// Returns a new instance that does not have any translation providers.
    pub fn null_provider<S: AsRef<str>>(languages: &[S]) -> Translations {
        Translations::new(languages)
    }

    /// Returns a new instance that extracts name translations from `name:lang` tags in input features.
    ///
    /// `languages` is the set of language codes to limit output translations to (`*` for all, `-xx` to exclude).
    pub fn default_provider<S: AsRef<str>>(languages: &[S]) -> Translations {
        Translations::new(languages).with_fallback_translation_provider(OsmTranslationProvider)
    }

    /// Mutates this translation instance to add `provider` which will be used before all other providers.
    #[deprecated(note = "use add_fallback_translation_provider")]
    pub fn add_translation_provider(&mut self, provider: impl TranslationProvider + 'static) -> &mut Self {
        self.providers.insert(0, Arc::new(provider));
        self
    }

    /// Mutates this translation instance to add `provider` which will be used only if all existing providers fail to
    /// produce a translation for a given language.
    pub fn add_fallback_translation_provider(&mut self, provider: impl TranslationProvider + 'static) -> &mut Self {
        self.providers.push(Arc::new(provider));
        self
    }

    /// Like [`Self::add_fallback_translation_provider`] but takes and returns the shared provider by `Arc`.
    pub fn add_fallback_translation_provider_arc(&mut self, provider: Arc<dyn TranslationProvider>) -> &mut Self {
        self.providers.push(provider);
        self
    }

    /// Builder-style version of [`Self::add_fallback_translation_provider`].
    pub fn with_fallback_translation_provider(mut self, provider: impl TranslationProvider + 'static) -> Self {
        self.add_fallback_translation_provider(provider);
        self
    }

    /// Convenience wrapper for [`Self::add_translations`] that creates a new map for results.
    pub fn get_translations(&self, tags: &Tags) -> Tags {
        let mut result = Tags::new();
        self.add_translations(&mut result, tags);
        result
    }

    /// Gets name translations for `input` using all registered providers and puts the results into `output` where
    /// `key=name:lang` and value is the translated name.
    pub fn add_translations(&self, output: &mut Tags, input: &Tags) {
        for provider in &self.providers {
            if let Some(translations) = provider.get_name_translations(input) {
                for (key, value) in translations.iter() {
                    if self.care_about_language(key) {
                        let key = if key.starts_with("name:") { key.clone() } else { format!("name:{key}") };
                        output.entry(key).or_insert_with(|| Value::str(value));
                    }
                }
            }
        }
        self.add_extra_name_tags(output, input);
    }

    pub fn get_should_transliterate(&self) -> bool {
        self.should_transliterate
    }

    /// Store whether client should use expensive name transliteration.
    pub fn set_should_transliterate(&mut self, should_transliterate: bool) -> &mut Self {
        self.should_transliterate = should_transliterate;
        self
    }

    /// Builder-style version of [`Self::set_should_transliterate`].
    pub fn with_should_transliterate(mut self, should_transliterate: bool) -> Self {
        self.should_transliterate = should_transliterate;
        self
    }

    /// Returns true if `language` is in the set of language translations to use.
    pub fn care_about_language(&self, language: &str) -> bool {
        if self.exclude_languages.contains(language) {
            return false;
        }
        if self.include_languages.contains(language) {
            return true;
        }
        self.default_include
    }

    /// The configured list of extra name tags to copy into translation outputs.
    pub fn get_extra_name_tags(&self) -> &[String] {
        &self.extra_name_tags
    }

    /// Set a list of extra name tags to be copied from the OSM source to the output result.
    pub fn set_extra_name_tags<S: AsRef<str>>(&mut self, extra_name_tags: &[S]) -> &mut Self {
        self.extra_name_tags = extra_name_tags.iter().map(|s| s.as_ref().to_string()).collect();
        self
    }

    fn add_extra_name_tags(&self, output: &mut Tags, input: &Tags) {
        for tag in &self.extra_name_tags {
            if let Some(value) = input.get(tag)
                && !value.is_null()
                && value.as_str() != Some("")
            {
                output.insert(tag.clone(), value.clone());
            }
        }
    }
}

/// Attempts to translate non-latin characters to latin characters that preserve the *sound* of the word (as opposed
/// to translation which attempts to preserve meaning) using an emulation of ICU's `Any-Latin` transform (see
/// [`crate::util::thread_local_transliterator`]).
///
/// NOTE: This can be expensive and the quality is hit or miss, so exhaust all other options first.
pub fn transliterate(input: &str) -> String {
    TRANSLITERATOR.with(|t| t.transliterate(input))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags;

    #[test]
    fn test_null() {
        let translations = Translations::null_provider(&["en"]);
        assert_eq!(Tags::new(), translations.get_translations(&tags! {"name:en" => "name"}));
    }

    #[test]
    fn test_wildcard() {
        let translations = Translations::default_provider(&["*"]);
        assert_eq!(tags! {"name:en" => "name"}, translations.get_translations(&tags! {"name:en" => "name"}));
        assert_eq!(tags! {"name:sr-Latn" => "name"}, translations.get_translations(&tags! {"name:sr-Latn" => "name"}));
        assert_eq!(
            tags! {"name:zh-Hant-TW" => "name"},
            translations.get_translations(&tags! {"name:zh-Hant-TW" => "name"})
        );
        assert_eq!(Tags::new(), translations.get_translations(&tags! {"name:left" => "name"}));
        assert_eq!(Tags::new(), translations.get_translations(&tags! {"name:etymology:wikidata" => "name"}));
    }

    #[test]
    fn test_default_provider() {
        let translations = Translations::default_provider(&["en"]);
        assert_eq!(
            tags! {"name:en" => "name"},
            translations.get_translations(&tags! {"name:en" => "name", "name:de" => "de"})
        );
    }

    #[test]
    fn test_two_providers_prefers_first() {
        let translations =
            Translations::default_provider(&["en", "es", "de"]).with_fallback_translation_provider(|_: &Tags| {
                BTreeMap::from([("name:de".to_string(), "de2".to_string()), ("name:en".to_string(), "en2".to_string())])
            });
        let mut result = translations.get_translations(&tags! {"name:en" => "en1", "name:es" => "es1"});
        result.sort_keys();
        assert_eq!(tags! {"name:de" => "de2", "name:en" => "en1", "name:es" => "es1"}, result);
    }

    #[test]
    fn test_transliterate() {
        assert_eq!("rì běn", transliterate("日本"));
    }

    #[test]
    fn test_include_exclude() {
        let cases: &[(&[&str], &[&str], &[&str])] = &[
            (&["jbo", "tlh"], &["jbo", "tlh"], &["en", "fr"]),
            (&["*"], &["jbo", "tlh", "en", "fr"], &[]),
            (&["*", "-tlh"], &["jbo", "fr"], &["tlh"]),
            (&["tlh", "-tlh"], &[], &["tlh", "en"]),
        ];
        for (languages, should_care, should_not_care) in cases {
            let translations = Translations::null_provider(languages);
            for lang in *should_care {
                assert!(translations.care_about_language(lang), "{languages:?} {lang}");
            }
            for lang in *should_not_care {
                assert!(!translations.care_about_language(lang), "{languages:?} {lang}");
            }
        }
    }

    #[test]
    fn test_extra_name_tags() {
        let input = tags! {
            "extra" => "extra",
            "name" => "name",
            "name:en" => "english name",
            "name:de" => "german name",
        };
        let mut translations = Translations::default_provider(&["en", "es", "de"]);
        let mut output1 = Tags::new();
        translations.add_translations(&mut output1, &input);
        assert!(!output1.contains_key("extra"));

        let mut output2 = Tags::new();
        translations.set_extra_name_tags(&["extra"]);
        translations.add_translations(&mut output2, &input);
        assert!(output2.contains_key("extra"));
    }
}
