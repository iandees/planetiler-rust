//! Port of `util/ThreadLocalTransliterator.java`.
//!
//! Java loads a separate copy of ICU4J's `Transliterator` per thread to avoid lock contention. In Rust,
//! transliterators come from [ICU4X](https://github.com/unicode-org/icu4x) (`icu_experimental::transliterate`), which
//! is not thread-safe, so instances are cached per thread and [`TransliteratorInstance`] is `!Send`.
//!
//! ICU4X does not ship ICU's compound `Any-Latin` transform, so it is emulated the same way ICU's
//! `AnyTransliterator` works: split the input into script runs (Common/Inherited characters join the surrounding run),
//! leave Latin runs untouched and apply the CLDR `<Script>-Latin` transform (`und-Latn-t-und-<script>`) to every other
//! run. Differences from ICU4J are documented in `PORTING.md`:
//! - Han uses the CLDR `Hans-Latin` data from ICU4X (same Han-Latin pinyin rules as ICU).
//! - Scripts whose CLDR transform can't be loaded by ICU4X (for example Thai, which needs a dictionary segmenter) or
//!   that have no transform at all fall back to [`any_ascii`], where ICU would leave them unchanged (or use its own
//!   Thai rules).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use icu_experimental::transliterate::Transliterator;
use icu_properties::props::Script;
use icu_properties::{CodePointMapData, PropertyNamesShort};

static LOGGED_ONCE: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Per-thread cache of loaded ICU4X transliterators by BCP-47 transform ID (`None` if it failed to load).
    static CACHE: RefCell<HashMap<String, Option<Rc<Transliterator>>>> = RefCell::new(HashMap::new());
}

fn load(id: &str) -> Option<Rc<Transliterator>> {
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(id.to_string())
            .or_insert_with(|| {
                let locale = id.parse().ok()?;
                match Transliterator::try_new(&locale) {
                    Ok(t) => Some(Rc::new(t)),
                    Err(e) => {
                        tracing::debug!("Unable to load transliterator {id}: {e:?}");
                        None
                    }
                }
            })
            .clone()
    })
}

/// Factory for per-thread transliterators.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadLocalTransliterator;

impl ThreadLocalTransliterator {
    pub fn new() -> ThreadLocalTransliterator {
        ThreadLocalTransliterator
    }

    /// Returns a transliterator for `id` that does not share any data with transliterators on other threads.
    ///
    /// Supported IDs are ICU's `Any-Latin` and BCP-47 transform IDs understood by ICU4X (like
    /// `und-Latn-t-und-cyrl`). Unknown IDs log a warning once and return the input unchanged.
    pub fn get_instance(&self, id: &str) -> TransliteratorInstance {
        if id.eq_ignore_ascii_case("Any-Latin") {
            return TransliteratorInstance { kind: Kind::AnyLatin, _not_send: Default::default() };
        }
        match load(id) {
            Some(t) => TransliteratorInstance { kind: Kind::Icu4x(t), _not_send: Default::default() },
            None => {
                if !LOGGED_ONCE.swap(true, Ordering::Relaxed) {
                    tracing::warn!("Could not get transliterator {id}, transliteration will be a no-op");
                }
                TransliteratorInstance { kind: Kind::Identity, _not_send: Default::default() }
            }
        }
    }
}

enum Kind {
    AnyLatin,
    Icu4x(Rc<Transliterator>),
    Identity,
}

/// A transliterator bound to the current thread.
pub struct TransliteratorInstance {
    kind: Kind,
    _not_send: std::marker::PhantomData<Rc<()>>,
}

impl TransliteratorInstance {
    pub fn transliterate(&self, input: &str) -> String {
        match &self.kind {
            Kind::AnyLatin => any_latin(input),
            Kind::Icu4x(t) => t.transliterate(input.to_string()),
            Kind::Identity => input.to_string(),
        }
    }
}

fn is_neutral(script: Script) -> bool {
    script == Script::Common || script == Script::Inherited || script == Script::Unknown
}

/// Emulates ICU's `Any-Latin` compound transliterator.
pub fn any_latin(input: &str) -> String {
    let scripts = CodePointMapData::<Script>::new();
    // fast path: nothing to do for pure Latin/Common text
    if input.chars().all(|c| {
        let s = scripts.get(c);
        s == Script::Latin || is_neutral(s)
    }) {
        return input.to_string();
    }

    // split into runs of (script, text) where neutral characters join the preceding run (or the first run)
    let mut runs: Vec<(Option<Script>, String)> = Vec::new();
    for c in input.chars() {
        let s = scripts.get(c);
        let s = if is_neutral(s) { None } else { Some(s) };
        match runs.last_mut() {
            Some((run_script, text)) if s.is_none() || *run_script == s || run_script.is_none() => {
                if run_script.is_none() {
                    *run_script = s;
                }
                text.push(c);
            }
            _ => runs.push((s, c.to_string())),
        }
    }

    let names = PropertyNamesShort::<Script>::new();
    let mut result = String::with_capacity(input.len() * 2);
    for (script, text) in runs {
        match script {
            None | Some(Script::Latin) => result.push_str(&text),
            Some(script) => {
                let id = match script {
                    Script::Han => Some("und-Latn-t-und-hans".to_string()),
                    _ => names.get(script).map(|name| format!("und-Latn-t-und-{}", name.to_ascii_lowercase())),
                };
                match id.and_then(|id| load(&id)) {
                    Some(t) => result.push_str(&t.transliterate(text)),
                    None => result.push_str(&any_ascii::any_ascii(&text)),
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test() {
        let t1 = ThreadLocalTransliterator::new().get_instance("Any-Latin");
        let t2 = ThreadLocalTransliterator::new().get_instance("Any-Latin");
        assert_eq!("rì běn", t1.transliterate("日本"));
        assert_eq!("rì běn", t2.transliterate("日本"));
    }

    #[test]
    fn test_other_threads() {
        std::thread::spawn(|| {
            assert_eq!("rì běn", ThreadLocalTransliterator::new().get_instance("Any-Latin").transliterate("日本"));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn test_scripts() {
        let t = ThreadLocalTransliterator::new().get_instance("Any-Latin");
        // matches ICU4J Any-Latin output
        assert_eq!("Moskva", t.transliterate("Москва"));
        assert_eq!("Athḗna", t.transliterate("Αθήνα"));
        assert_eq!("seoul", t.transliterate("서울"));
        assert_eq!("hiragana katakana", t.transliterate("ひらがな カタカナ"));
        assert_eq!("tái běi", t.transliterate("臺北"));
        // latin text is left alone
        assert_eq!("Zürich - Ærø", t.transliterate("Zürich - Ærø"));
        assert_eq!("Café Moskva 123", t.transliterate("Café Москва 123"));
        assert_eq!("", t.transliterate(""));
    }

    #[test]
    fn test_bcp47_id_and_unknown() {
        let t = ThreadLocalTransliterator::new().get_instance("und-Latn-t-und-cyrl");
        assert_eq!("Moskva", t.transliterate("Москва"));
        let t = ThreadLocalTransliterator::new().get_instance("Garbage-Id");
        assert_eq!("Москва", t.transliterate("Москва"));
    }
}
