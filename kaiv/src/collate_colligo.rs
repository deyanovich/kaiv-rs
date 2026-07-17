//! Level 3 collation: locale-aware comparison for `..lex[locale]`
//! spans (SPEC.md § Level 3: Collation), built on colligo — the
//! lightweight, context-free collator that replaced ICU4X here as
//! in quarb (a fraction of the dependency weight, wasm-friendly).
//!
//! Conformance posture: colligo's embedded data derives from the
//! spec's pinned reference line (CLDR 48.2 / UCA 17.0.0), and only
//! its ICU-oracle-verified **exact** tiers build here — a locale
//! colligo covers approximately, deliberately differently, or not
//! at all does not resolve, and the Validator reports
//! `CollationUnsupportedError` up front. Rejecting is always
//! conforming at Level 3; silently disagreeing is not.
//!
//! Comparisons run at the spec's pinned defaults — tertiary
//! strength with the UCA lowercase-first case order (colligo's own
//! default is case-insensitive, so the mode is set explicitly).
//! BCP 47 `-u-` extension tags are not resolved by this backend:
//! any tag carrying an extension is unsupported and rejects.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

use colligo::{CaseMode, Collator};

/// CLDR version of the embedded collation data — the spec's pinned
/// reference version (SPEC.md § Reference Collation), exposed as
/// conformance metadata. colligo's data provenance is
/// CLDR 48.2 / UCA 17.0.0.
pub const CLDR_VERSION: &str = "48";

// The Validator walks a document line by line on one thread; a
// per-thread cache avoids rebuilding a collator for every data
// line that repeats a locale tag. `None` caches an unresolvable
// tag.
thread_local! {
    static COLLATORS: RefCell<HashMap<String, Option<Rc<Collator>>>> =
        RefCell::new(HashMap::new());
}

fn cached(tag: &str) -> Option<Rc<Collator>> {
    COLLATORS.with(|cache| {
        cache
            .borrow_mut()
            .entry(tag.to_string())
            .or_insert_with(|| build(tag).map(Rc::new))
            .clone()
    })
}

/// Build a collator for the tag inside `..lex[…]`. `None` when the
/// tag carries a BCP 47 extension, or colligo does not cover it at
/// exact fidelity.
fn build(tag: &str) -> Option<Collator> {
    // Extension singletons (`-u-`, `-t-`, …) select behavior this
    // backend cannot honor; a tag carrying one is unsupported.
    // (`-x-` private use passes through: colligo's registry names
    // ICU-identical variants like `ru-x-icu` that way.)
    let mut parts = tag.split('-').skip(1);
    if parts.any(|p| p.len() == 1 && !p.eq_ignore_ascii_case("x")) {
        return None;
    }
    let collator = Collator::builder(tag)
        .case_mode(CaseMode::LowerFirst) // UCA tertiary default
        .build()
        .ok()?;
    Some(collator)
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
        // French collation keeps it with e. Case matters at the
        // pinned tertiary default.
        assert_eq!(compare("fr", "étude", "zebra"), Some(Ordering::Less));
        assert_eq!(compare("fr", "café", "cafe"), Some(Ordering::Greater));
        assert_ne!(compare("fr", "Cafe", "cafe"), Some(Ordering::Equal));
    }

    #[test]
    fn equality_crosses_normalization_forms() {
        // NFC "é" vs NFD "e\u{301}" — equal under UCA weights.
        assert_eq!(
            compare("fr", "caf\u{e9}", "cafe\u{301}"),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn non_exact_tiers_and_extensions_reject() {
        // fr-CA needs backwards secondaries (approximate tier);
        // ja is unsupported outright; any -u- extension tag is
        // outside this backend. All must refuse to resolve —
        // CollationUnsupportedError at the Validator, never a
        // silently different order.
        assert!(!resolves("fr-CA"));
        assert!(!resolves("ja"));
        assert!(!resolves("de-u-ks-level1"));
        assert!(!resolves("en-u-kc-true"));
        // The ICU-identical Russian variant rides -x- private use.
        assert!(resolves("ru-x-icu"));
        assert!(resolves("de"));
    }
}
