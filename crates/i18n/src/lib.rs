//! radixdlt-i18n — System-locale detection and multilingual text for the SDK.
//!
//! This is the internationalization foundation of the RadixDLT Rust SDK. Every SDK
//! crate emits its user-facing text (errors, CLI output) in the system language:
//! Spanish when the locale starts with `es`, English otherwise. English is always
//! the fallback, so adding a language to this crate never breaks existing callers.
//!
//! # Detection
//!
//! The language is resolved once (cached) in this order:
//!   1. `RADIXDLT_LANG` (explicit override: `es`/`en`; handy in tests)
//!   2. `LC_ALL`
//!   3. `LC_MESSAGES`
//!   4. `LANG`
//!
//! `C`/`POSIX`/empty values are ignored and fall back to English.
//!
//! # Usage
//!
//! ```
//! use radixdlt_i18n::{Lang, tr};
//!
//! let lang = Lang::detect();
//! // Two-argument form (English + Spanish):
//! let msg = tr!(lang,
//!     format!("invalid public key"),
//!     format!("clave pública inválida"));
//! // Labelled form — same meaning, ready for more languages:
//! let msg = tr!(lang, format!("invalid public key"),
//!     Es: format!("clave pública inválida"));
//! println!("{msg}");
//! ```
//!
//! # Adding a language
//!
//! 1. Add a variant to [`Lang`] (it is `#[non_exhaustive]`, so this is not a
//!    breaking change) and teach [`Lang::from_locale_str`] to detect it.
//! 2. Add `Xx: "…"` arms to the `tr!` call sites you want translated. Untouched
//!    call sites keep compiling and fall back to English.

// In TESTS a panic IS the failure mechanism. Library code keeps the deny: a panic there is
// taken in the CONSUMER's process, which they neither chose nor can catch.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

use std::sync::OnceLock;

/// Language supported by the SDK.
///
/// Marked `#[non_exhaustive]`: new languages may be added in minor releases, so
/// match with a `_ => …` fallback (the [`tr!`] macro already does).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Lang {
    /// English (default and universal fallback).
    #[default]
    En,
    /// Spanish.
    Es,
}

impl Lang {
    /// The system language, resolved once and cached for the process lifetime.
    pub fn detect() -> Lang {
        static CACHED: OnceLock<Lang> = OnceLock::new();
        *CACHED.get_or_init(detect_uncached)
    }

    /// Infers the language from a locale string (e.g. `"es_ES.UTF-8"`).
    pub fn from_locale_str(s: &str) -> Lang {
        if s.trim_start().to_ascii_lowercase().starts_with("es") {
            Lang::Es
        } else {
            Lang::En
        }
    }
}

fn detect_uncached() -> Lang {
    // Explicit override first (tests / integrator control).
    if let Ok(v) = std::env::var("RADIXDLT_LANG") {
        let v = v.trim();
        if !v.is_empty() {
            return Lang::from_locale_str(v);
        }
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            let t = v.trim();
            if !t.is_empty() && t != "C" && t != "POSIX" {
                return Lang::from_locale_str(t);
            }
        }
    }
    // Nothing in the environment. That is NOT the same as "no preference": a unit started by
    // systemd, or a command run over a non-interactive ssh session, simply inherits no locale.
    // Defaulting to English there makes a Spanish machine speak English to its owner — and
    // worse, whatever is WRITTEN at that moment (an sshd banner, say) keeps doing so at every
    // later login. So fall back to what the system itself is configured for.
    for path in ["/etc/locale.conf", "/etc/default/locale"] {
        if let Ok(txt) = std::fs::read_to_string(path) {
            if let Some(l) = lang_from_locale_file(&txt) {
                return l;
            }
        }
    }
    Lang::En
}

/// Reads `LC_ALL`/`LC_MESSAGES`/`LANG` out of a locale file, in that order of precedence
/// (the same the environment uses). Values may be quoted. `None` when it names none usefully.
fn lang_from_locale_file(text: &str) -> Option<Lang> {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            if k.trim() != key {
                continue;
            }
            let v = v.trim().trim_matches('"').trim();
            if !v.is_empty() && v != "C" && v != "POSIX" {
                return Some(Lang::from_locale_str(v));
            }
        }
    }
    None
}

/// Shortcut for the system language (same as [`Lang::detect`]).
pub fn lang() -> Lang {
    Lang::detect()
}

/// Picks the text variant by language, falling back to English.
///
/// Two forms (all arms must be the same type, usually `String`):
///
/// * `tr!(lang, <english>, <spanish>)` — the common bilingual shorthand.
/// * `tr!(lang, <english>, Es: <spanish>, Fr: <french>, …)` — labelled arms, one
///   per [`Lang`] variant; any language without an arm gets the English text.
///
/// Because unmatched languages fall back to English, adding a variant to `Lang`
/// never breaks existing call sites.
#[macro_export]
macro_rules! tr {
    ($lang:expr, $en:expr $(, $variant:ident : $txt:expr)+ $(,)?) => {
        match $lang {
            $($crate::Lang::$variant => $txt,)+
            _ => $en,
        }
    };
    ($lang:expr, $en:expr, $es:expr $(,)?) => {
        match $lang {
            $crate::Lang::Es => $es,
            _ => $en,
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_spanish_locale() {
        assert_eq!(Lang::from_locale_str("es_ES.UTF-8"), Lang::Es);
        assert_eq!(Lang::from_locale_str("es"), Lang::Es);
        assert_eq!(Lang::from_locale_str("ES_es"), Lang::Es);
    }

    #[test]
    fn falls_back_to_english() {
        assert_eq!(Lang::from_locale_str("en_US.UTF-8"), Lang::En);
        assert_eq!(Lang::from_locale_str("fr_FR"), Lang::En);
        assert_eq!(Lang::from_locale_str(""), Lang::En);
    }

    #[test]
    fn tr_picks_the_right_arm() {
        assert_eq!(tr!(Lang::Es, "hello".to_string(), "hola".to_string()), "hola");
        assert_eq!(tr!(Lang::En, "hello".to_string(), "hola".to_string()), "hello");
    }

    #[test]
    fn tr_labelled_form_falls_back_to_english() {
        assert_eq!(tr!(Lang::Es, "hello".to_string(), Es: "hola".to_string()), "hola");
        // A language with no labelled arm gets the English text.
        assert_eq!(
            tr!(Lang::En, "hello".to_string(), Es: "hola".to_string()),
            "hello"
        );
    }

    /// A unit started by systemd, or a command over non-interactive ssh, inherits no locale.
    /// Falling back to English there makes a Spanish machine speak English to its owner, and
    /// whatever is written at that moment (an sshd banner) keeps doing so at every later login.
    #[test]
    fn the_system_locale_file_is_read_when_the_environment_is_empty() {
        assert_eq!(lang_from_locale_file("LANG=es_ES.UTF-8\n"), Some(Lang::Es));
        assert_eq!(lang_from_locale_file("LANG=\"es_ES.UTF-8\"\n"), Some(Lang::Es));
        assert_eq!(lang_from_locale_file("LANG=en_GB.UTF-8\n"), Some(Lang::En));
    }

    /// The same precedence the environment uses, so a machine does not answer differently
    /// depending on which of the two keys its locale happened to be written to.
    #[test]
    fn lc_all_outranks_lc_messages_which_outranks_lang() {
        assert_eq!(
            lang_from_locale_file("LANG=en_US.UTF-8\nLC_MESSAGES=es_ES.UTF-8\n"),
            Some(Lang::Es)
        );
        assert_eq!(
            lang_from_locale_file("LANG=es_ES.UTF-8\nLC_ALL=en_US.UTF-8\n"),
            Some(Lang::En)
        );
    }

    /// "C"/"POSIX" mean "no locale chosen", not "English requested": treating them as an
    /// answer would stop the next source from ever being consulted.
    #[test]
    fn a_placeholder_locale_is_not_an_answer() {
        assert_eq!(lang_from_locale_file("LANG=C\n"), None);
        assert_eq!(lang_from_locale_file("LANG=POSIX\n"), None);
        assert_eq!(lang_from_locale_file("LANG=\n"), None);
        assert_eq!(lang_from_locale_file("# LANG=es_ES.UTF-8\n"), None);
        assert_eq!(lang_from_locale_file(""), None);
    }
}
