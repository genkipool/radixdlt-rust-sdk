//! radixdlt-i18n — System-locale detection and bilingual (Spanish/English) text.
//!
//! This is the internationalization foundation of the RadixDLT Rust SDK. Every SDK
//! crate emits its user-facing text (errors, CLI output) in the system language:
//! Spanish when the locale starts with `es`, English otherwise.
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
//! let msg = tr!(lang,
//!     format!("invalid public key"),
//!     format!("clave pública inválida"));
//! println!("{msg}");
//! ```

use std::sync::OnceLock;

/// Language supported by the SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Lang {
    /// English (default).
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
    Lang::En
}

/// Shortcut for the system language (same as [`Lang::detect`]).
pub fn lang() -> Lang {
    Lang::detect()
}

/// Picks the text variant by language: `tr!(lang, <english>, <spanish>)`.
///
/// Both arms must be the same type (usually `String`).
#[macro_export]
macro_rules! tr {
    ($lang:expr, $en:expr, $es:expr $(,)?) => {
        match $lang {
            $crate::Lang::Es => $es,
            $crate::Lang::En => $en,
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
        assert_eq!(
            tr!(Lang::Es, "hello".to_string(), "hola".to_string()),
            "hola"
        );
        assert_eq!(
            tr!(Lang::En, "hello".to_string(), "hola".to_string()),
            "hello"
        );
    }
}
