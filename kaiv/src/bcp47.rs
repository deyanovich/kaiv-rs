//! BCP 47 (RFC 5646) language-tag **well-formedness** — the static,
//! backend-independent check the schema compiler applies to every
//! `..lex[tag]` span (SPEC.md § Reference Collation; the D-8 ruling
//! splits unknown input by kind: an ill-formed tag is
//! INVALID_CONSTRAINT_ERROR at schema-compile time, a well-formed
//! tag the backend cannot honor is CollationUnsupportedError at
//! validation). Well-formedness is purely syntactic — the `langtag`
//! production plus whole-tag private use — with no registry
//! validity: `zz` is well-formed, `123` is not. The grandfathered
//! productions (`i-ami`, …) are not accepted: they are irregular
//! relics BCP 47 itself only admits for compatibility, and CLDR
//! collation has no data keyed by them.

/// Is `tag` a well-formed BCP 47 language tag (`langtag` or
/// `privateuse`)? Case-insensitive, as the RFC requires.
pub(crate) fn well_formed(tag: &str) -> bool {
    let subs: Vec<&str> = tag.split('-').collect();
    // Every subtag: 1-8 alphanumeric ASCII, none empty.
    if subs
        .iter()
        .any(|s| s.is_empty() || s.len() > 8 || !s.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        return false;
    }
    let alpha = |s: &str| s.bytes().all(|b| b.is_ascii_alphabetic());
    let digit = |s: &str| s.bytes().all(|b| b.is_ascii_digit());

    // Whole-tag private use: `x` 1*("-" 1*8alphanum).
    if subs[0].eq_ignore_ascii_case("x") {
        return subs.len() > 1;
    }

    // language = 2*3ALPHA ["-" extlang] / 4ALPHA / 5*8ALPHA
    if !alpha(subs[0]) {
        return false;
    }
    let mut i = 1;
    match subs[0].len() {
        2..=3 => {
            // extlang = 3ALPHA *2("-" 3ALPHA)
            let mut e = 0;
            while i < subs.len() && e < 3 && subs[i].len() == 3 && alpha(subs[i]) {
                i += 1;
                e += 1;
            }
        }
        4..=8 => {}
        _ => return false,
    }
    // script = 4ALPHA
    if i < subs.len() && subs[i].len() == 4 && alpha(subs[i]) {
        i += 1;
    }
    // region = 2ALPHA / 3DIGIT
    if i < subs.len()
        && ((subs[i].len() == 2 && alpha(subs[i]))
            || (subs[i].len() == 3 && digit(subs[i])))
    {
        i += 1;
    }
    // variant = 5*8alphanum / (DIGIT 3alphanum)
    while i < subs.len()
        && (subs[i].len() >= 5
            || (subs[i].len() == 4 && subs[i].as_bytes()[0].is_ascii_digit()))
    {
        i += 1;
    }
    // extension = singleton 1*("-" 2*8alphanum); singletons unique.
    let mut seen = [false; 36];
    while i < subs.len() && subs[i].len() == 1 && !subs[i].eq_ignore_ascii_case("x") {
        let c = subs[i].as_bytes()[0].to_ascii_lowercase();
        let slot = if c.is_ascii_digit() {
            (c - b'0') as usize
        } else {
            (c - b'a') as usize + 10
        };
        if std::mem::replace(&mut seen[slot], true) {
            return false;
        }
        i += 1;
        let start = i;
        while i < subs.len() && subs[i].len() >= 2 {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    // privateuse = "x" 1*("-" 1*8alphanum)
    if i < subs.len() && subs[i].eq_ignore_ascii_case("x") {
        return i + 1 < subs.len();
    }
    i == subs.len()
}

#[cfg(test)]
mod tests {
    use super::well_formed;

    #[test]
    fn well_formed_tags() {
        for t in [
            "en",
            "und",
            "zz", // well-formed, merely unknown to any registry
            "en-US",
            "de-DE",
            "zh-Hans",
            "zh-yue",
            "sl-rozaj-biske",
            "de-DE-u-co-phonebk-ks-level1",
            "en-a-bbb-x-a-z",
            "x-private",
            "hy-Latn-IT-arevela",
            "es-419",
        ] {
            assert!(well_formed(t), "{t}");
        }
    }

    #[test]
    fn ill_formed_tags() {
        for t in [
            "",
            "123",              // language must be alphabetic
            "a",                // too short
            "toolongsubtag123", // > 8
            "de--phonebk",      // empty subtag
            "en-",              // trailing empty
            "not a tag",        // space
            "en-u",             // extension without tail
            "en-u-ks-u-co",     // duplicate singleton
            "en-x",             // private use without tail
            "i-ami",            // grandfathered: not accepted
            "en-US-POSIX-Zzzz", // out of order: script after variant
            "en_US",            // underscore
        ] {
            assert!(!well_formed(t), "{t}");
        }
    }
}
