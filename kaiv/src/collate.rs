//! Level 3 collation: locale-aware comparison for `..lex[locale]`
//! spans (SPEC.md § Level 3: Collation), built on ICU4X. The locale
//! is a BCP 47 tag; comparisons run at the spec's pinned defaults —
//! tertiary strength, non-ignorable variables, no case level —
//! unless the tag carries one of the recognized `-u-` collation
//! extension overrides: `ks` (strength), `ka` (variable handling),
//! `kc` (case level), `co` (named collation).
//!
//! The spec pins CLDR 48 as the conformance reference; this
//! implementation embeds exactly that (the ICU4X 2.1 line's data)
//! and reports it as [`CLDR_VERSION`] — the conformance metadata
//! SPEC.md § Reference Collation calls for.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

use icu_collator::options::{AlternateHandling, CaseLevel, CollatorOptions, Strength};
use icu_collator::{Collator, CollatorBorrowed, CollatorPreferences};
use icu_locale_core::extensions::unicode::{key, Key};
use icu_locale_core::Locale;

/// CLDR version of the embedded collation data — matches the spec's
/// pinned reference version (SPEC.md § Reference Collation), exposed
/// as conformance metadata.
pub const CLDR_VERSION: &str = "48";

// The Validator walks a document line by line on one thread; a
// per-thread cache avoids rebuilding a collator (data lookups and
// allocation) for every data line that repeats a locale tag. `None`
// caches an unresolvable tag.
thread_local! {
    static COLLATORS: RefCell<HashMap<String, Option<Rc<CollatorBorrowed<'static>>>>> =
        RefCell::new(HashMap::new());
}

fn cached(tag: &str) -> Option<Rc<CollatorBorrowed<'static>>> {
    COLLATORS.with(|cache| {
        cache
            .borrow_mut()
            .entry(tag.to_string())
            .or_insert_with(|| build(tag).map(Rc::new))
            .clone()
    })
}

/// Build a collator for the tag inside `..lex[…]`. `None` when the
/// tag is not a well-formed BCP 47 tag or carries an override value
/// outside the spec's recognized set.
fn build(tag: &str) -> Option<CollatorBorrowed<'static>> {
    let locale: Locale = tag.parse().ok()?;
    let mut options = CollatorOptions::default();
    options.strength = Some(match keyword(&locale, key!("ks")).as_deref() {
        None | Some("level3") => Strength::Tertiary,
        Some("level1") => Strength::Primary,
        Some("level2") => Strength::Secondary,
        Some("identic") => Strength::Identical,
        Some(_) => return None,
    });
    options.alternate_handling = Some(match keyword(&locale, key!("ka")).as_deref() {
        None | Some("noignore") => AlternateHandling::NonIgnorable,
        Some("shifted") => AlternateHandling::Shifted,
        Some(_) => return None,
    });
    options.case_level = Some(match keyword(&locale, key!("kc")).as_deref() {
        None | Some("false") => CaseLevel::Off,
        Some("true") => CaseLevel::On,
        Some(_) => return None,
    });
    // `-u-co-<name>` (and the base locale's own tailoring) flows in
    // through the preferences.
    Collator::try_new(CollatorPreferences::from(&locale), options).ok()
}

fn keyword(locale: &Locale, k: Key) -> Option<String> {
    locale
        .extensions
        .unicode
        .keywords
        .get(&k)
        .map(|v| v.to_string())
}

/// Whether this runtime can compare under the tag.
pub fn resolves(tag: &str) -> bool {
    cached(tag).is_some()
}

/// Compare `a` and `b` under the tag's collation. `None` when the
/// tag does not resolve.
pub fn compare(tag: &str, a: &str, b: &str) -> Option<Ordering> {
    Some(cached(tag)?.compare(a, b))
}

/// `[lo,hi]` range evaluation under the tag's collation (collation
/// governs order — SPEC.md § Reference Collation).
pub(crate) fn range_ok(tag: &str, value: &str, lo: Option<&str>, hi: Option<&str>) -> Option<bool> {
    let coll = cached(tag)?;
    let above = lo.is_none_or(|l| coll.compare(l, value) != Ordering::Greater);
    let below = hi.is_none_or(|h| coll.compare(value, h) != Ordering::Greater);
    Some(above && below)
}

/// Enum membership under the tag's collation (collation governs
/// equality as well as order — SPEC.md § Reference Collation).
pub(crate) fn enum_has(tag: &str, value: &str, vs: &[String]) -> Option<bool> {
    let coll = cached(tag)?;
    Some(vs.iter().any(|v| coll.compare(v, value) == Ordering::Equal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tertiary_default_orders_accents_with_base() {
        // Byte order puts é (0xC3 0xA9) after every ASCII letter;
        // French collation keeps it with e.
        assert_eq!(compare("fr-CA", "étude", "zebra"), Some(Ordering::Less));
        assert_eq!(compare("fr-CA", "café", "cafe"), Some(Ordering::Greater));
    }

    #[test]
    fn equality_crosses_normalization_forms() {
        // NFC "é" vs NFD "e\u{301}" — equal under UCA at any strength.
        assert_eq!(
            compare("fr", "caf\u{e9}", "cafe\u{301}"),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn strength_overrides() {
        // Primary: accents ignored.
        assert_eq!(
            compare("en-u-ks-level1", "resume", "résumé"),
            Some(Ordering::Equal)
        );
        // Secondary: case ignored, accents kept.
        assert_eq!(
            compare("en-u-ks-level2", "Cafe", "cafe"),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare("en-u-ks-level2", "café", "cafe"),
            Some(Ordering::Greater)
        );
        // German ß ≡ ss at primary (CLDR marks the expansion with a
        // secondary difference), distinct at the tertiary default.
        assert_eq!(
            compare("de-u-ks-level1", "straße", "strasse"),
            Some(Ordering::Equal)
        );
        assert_ne!(compare("de", "straße", "strasse"), Some(Ordering::Equal));
    }

    #[test]
    fn named_collation() {
        // German phonebook: ä sorts as ae — before "af"; standard
        // German sorts ä as a + diacritic, after "af".
        assert_eq!(compare("de-u-co-phonebk", "än", "af"), Some(Ordering::Less));
        assert_eq!(compare("de", "än", "af"), Some(Ordering::Greater));
    }

    #[test]
    fn unresolvable_tags() {
        assert!(!resolves("123"));
        assert!(!resolves(""));
        assert!(!resolves("not a tag"));
        // Well-formed but unrecognized override value (the spec
        // recognizes level1|level2|level3|identic only).
        assert!(!resolves("en-u-ks-level4"));
        // Well-formed unknown language falls back to root — usable.
        assert!(resolves("zz"));
        assert!(resolves("en-US"));
    }
}
