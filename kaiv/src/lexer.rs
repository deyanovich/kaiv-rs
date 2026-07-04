//! The Lexer: the six-rule line classifier plus the document-level
//! checks from SPEC.md § Parsing Requirements. Eager model: the whole
//! text is validated before any line is handed to later stages.

use crate::error::{LexError, LexErrorAt};

/// Which file family is being lexed. Constraint checking on metadata
/// lines applies to schema-family files only (SPEC.md § Lexer Errors,
/// INVALID_CONSTRAINT_ERROR is "schemas only").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Data,
    Schema,
    TypeLib,
    /// `.faiv` unit-definition libraries.
    UnitLib,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind<'a> {
    Blank,
    Comment(&'a str),
    Doc(&'a str),
    /// Full declaration line, leading whitespace stripped (`.!kaiv 1`).
    Decl(&'a str),
    /// `[...]` section-block opener; payload is the bracket interior.
    SectionOpen(&'a str),
    /// `[]`
    SectionClose,
    /// `(...)` namespace-block opener; payload is the paren interior.
    NsOpen(&'a str),
    /// `()`
    NsClose,
    /// Rule 5 content line, split on the first `=` outside a quoted
    /// name. `left` is trimmed on both sides; `value` is verbatim.
    Content {
        left: &'a str,
        value: &'a str,
    },
    /// Rule 6 metadata annotation (`!type`, `?prov`, `&name`, or a
    /// schema/type-library constraint line).
    Meta(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line<'a> {
    pub no: usize,
    pub kind: LineKind<'a>,
}

const DECL_KEYWORDS: &[&str] = &[
    "kaiv",
    "kaivschema",
    "kaivtype",
    "kaivunit",
    "schema",
    "types",
    "units",
    "registry",
    "provenance",
    "ref",
    "compose",
];

pub fn lex(input: &[u8], kind: FileKind) -> Result<Vec<Line<'_>>, LexErrorAt> {
    // BOM detection precedes everything, including UTF-8 validation.
    if input.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(LexErrorAt {
            error: LexError::Bom,
            line: 0,
        });
    }
    let text = std::str::from_utf8(input).map_err(|_| LexErrorAt {
        error: LexError::InvalidUtf8,
        line: 0,
    })?;

    // Forbidden characters: NUL anywhere, CR not part of CRLF.
    let bytes = text.as_bytes();
    let mut line_no = 1usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            0x00 => {
                return Err(LexErrorAt {
                    error: LexError::InvalidCharacter,
                    line: line_no,
                })
            }
            b'\r' if bytes.get(i + 1) != Some(&b'\n') => {
                return Err(LexErrorAt {
                    error: LexError::InvalidCharacter,
                    line: line_no,
                })
            }
            b'\n' => line_no += 1,
            _ => {}
        }
    }

    // Mandatory final EOL (empty document is valid).
    if !bytes.is_empty() && *bytes.last().unwrap() != b'\n' {
        return Err(LexErrorAt {
            error: LexError::MissingFinalEol,
            line: 0,
        });
    }

    // The final EOL is mandatory, so the text is a sequence of
    // EOL-terminated lines; split('\n') yields one trailing "" to drop.
    let mut raw_lines: Vec<&str> = text.split('\n').collect();
    raw_lines.pop();
    // With `.!units` imports present, unit-name membership becomes
    // resolution-dependent and moves to the compile stage; without
    // them the Lexer can enforce the built-in set eagerly.
    let has_unit_imports = raw_lines
        .iter()
        .any(|l| l.trim_start_matches([' ', '\t']).starts_with(".!units"));
    let mut out = Vec::new();
    for (idx, raw) in raw_lines.into_iter().enumerate() {
        let no = idx + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let s = raw.trim_start_matches([' ', '\t']);
        let k = classify(s, no, kind, has_unit_imports)?;
        out.push(Line { no, kind: k });
    }
    Ok(out)
}

fn classify<'a>(
    s: &'a str,
    no: usize,
    kind: FileKind,
    has_unit_imports: bool,
) -> Result<LineKind<'a>, LexErrorAt> {
    if s.is_empty() {
        return Ok(LineKind::Blank);
    }
    if let Some(c) = s.strip_prefix('#') {
        return Ok(LineKind::Comment(c));
    }
    if let Some(c) = s.strip_prefix("//") {
        return Ok(LineKind::Doc(c));
    }
    if s.starts_with(".!") || s.starts_with(".?") {
        check_declaration(s, no)?;
        return Ok(LineKind::Decl(s));
    }
    if s == "[]" {
        return Ok(LineKind::SectionClose);
    }
    if s == "()" {
        return Ok(LineKind::NsClose);
    }
    if let Some(rest) = s.strip_prefix('[') {
        if let Some(inner) = rest.strip_suffix(']') {
            // A section-open may carry a Level 2 table header after
            // the array path; the Lexer validates its regular grammar
            // (SPEC.md § Level 2, Implementation Impact).
            let toks = crate::table::tokens(inner);
            if toks.len() > 1 && crate::table::parse_header(&toks[1..]).is_none() {
                return Err(LexErrorAt {
                    error: LexError::InvalidConstraint,
                    line: no,
                });
            }
            return Ok(LineKind::SectionOpen(inner));
        }
    }
    if let Some(rest) = s.strip_prefix('(') {
        if let Some(inner) = rest.strip_suffix(')') {
            return Ok(LineKind::NsOpen(inner));
        }
    }
    // Rule-6 priority for metadata-leader lines: a pattern or enum
    // item may contain `=` (e.g. !str/^a=b$/), which would
    // misclassify the line as rule-5 content. For a line whose first
    // character is a metadata leader for this file kind, the full
    // rule-6 parse is attempted first; it must consume the entire
    // line, else classification falls through to the `=`-split.
    if early_meta(s, kind) {
        check_schema_units(s, kind, has_unit_imports, no)?;
        return Ok(LineKind::Meta(s));
    }
    // A schema/type-library line leading with `re{sep}` that failed
    // the full-line parse is a malformed pattern literal, not a
    // content line — `re` is reserved in that position.
    if matches!(kind, FileKind::Schema | FileKind::TypeLib) && re_leader(s) {
        return Err(LexErrorAt {
            error: LexError::InvalidConstraint,
            line: no,
        });
    }
    if let Some(i) = split_index(s) {
        let left = s[..i].trim_end_matches([' ', '\t']);
        let value = &s[i + 1..];
        if left.is_empty() {
            return Err(LexErrorAt {
                error: LexError::EmptyKey,
                line: no,
            });
        }
        check_key(left, no, kind)?;
        return Ok(LineKind::Content { left, value });
    }
    // Rule 6: metadata annotation.
    let first = s.chars().next().unwrap();
    let meta_ok = match kind {
        FileKind::Data => matches!(first, '!' | '?' | '&'),
        // Unit-definition lines lead with a dimension (a unit
        // reference) or `$` for currencies.
        FileKind::UnitLib => first.is_ascii_alphanumeric() || first == '$',
        // Schema and type-library files additionally carry constraint
        // lines that begin with a pattern, enum, span, or range.
        // (`#`-leading length constraints cannot appear here: rule 2
        // classifies any `#`-leading line as a comment first.)
        FileKind::Schema | FileKind::TypeLib => {
            matches!(first, '!' | '?' | '&' | '/' | '{' | '.' | '[')
        }
    };
    if !meta_ok {
        return Err(LexErrorAt {
            error: LexError::MissingOperator,
            line: no,
        });
    }
    if matches!(kind, FileKind::Schema | FileKind::TypeLib)
        && (first == '!' || first == '&')
        && crate::anno::parse_annotation(s).is_none()
    {
        return Err(LexErrorAt {
            error: LexError::InvalidConstraint,
            line: no,
        });
    }
    check_schema_units(s, kind, has_unit_imports, no)?;
    Ok(LineKind::Meta(s))
}

/// Built-in unit membership on schema/type-library annotation lines —
/// eager only while no `.!units` import makes the set open-ended
/// (then the compile stage checks against the imported customs).
fn check_schema_units(
    s: &str,
    kind: FileKind,
    has_unit_imports: bool,
    no: usize,
) -> Result<(), LexErrorAt> {
    if !matches!(kind, FileKind::Schema | FileKind::TypeLib)
        || has_unit_imports
        || !s.starts_with('!')
    {
        return Ok(());
    }
    if let Some(a) = crate::anno::parse_annotation(s) {
        if let Some(u) = &a.unit {
            if !crate::unit::members_ok(u, &Default::default()) {
                return Err(LexErrorAt {
                    error: LexError::InvalidConstraint,
                    line: no,
                });
            }
        }
    }
    Ok(())
}

/// Does the entire line parse as a rule-6 metadata/constraint line
/// for this file kind? Only leaders that can legitimately carry an
/// embedded `=` (patterns, enums) — or that lead annotations — are
/// attempted; everything else keeps rule-5 priority.
fn early_meta(s: &str, kind: FileKind) -> bool {
    let Some(first) = s.chars().next() else {
        return false;
    };
    match kind {
        FileKind::Data => match first {
            '!' => crate::anno::parse_annotation(s).is_some(),
            '&' => named_annotation_ok(s),
            _ => false,
        },
        FileKind::Schema | FileKind::TypeLib => match first {
            '!' => crate::anno::parse_annotation(s).is_some(),
            '&' => named_annotation_ok(s),
            '/' | '{' | '[' => crate::anno::parse_constraint_items(s).is_some(),
            '.' if s.starts_with("..") => crate::anno::parse_constraint_items(s).is_some(),
            // `re{sep}…` — the alternative-delimiter pattern form;
            // `re` is reserved in leading name position here.
            'r' if re_leader(s) => crate::anno::parse_constraint_items(s).is_some(),
            _ => false,
        },
        // A rate-source URL may contain '=' (query strings), so the
        // whole-line parse must classify before the '='-split.
        FileKind::UnitLib => {
            (first.is_ascii_alphanumeric() || first == '$')
                && crate::faiv::parse_def_line(s).is_some()
        }
    }
}

/// Does the line lead with the reserved `re{sep}` pattern-literal
/// introducer (SPEC.md § Patterns)?
fn re_leader(s: &str) -> bool {
    s.starts_with("re") && s[2..].starts_with(crate::anno::RE_SEPS)
}

/// `&name` optionally followed by whitespace-separated constraint
/// items (the named-annotation-line production; type-name is a
/// bare-name).
fn named_annotation_ok(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('&') else {
        return false;
    };
    let end = rest.find([' ', '\t']).unwrap_or(rest.len());
    let (name, items) = rest.split_at(end);
    let bs = name.as_bytes();
    !bs.is_empty()
        && (bs[0].is_ascii_alphabetic() || bs[0] == b'_')
        && bs[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
        && (items.trim_matches([' ', '\t']).is_empty()
            || crate::anno::parse_constraint_items(items).is_some())
}

/// Index of the first `=` outside a quoted name. Quoted names use `""`
/// doubling; an `=` inside quotes is part of the name (SPEC.md
/// § Formal Grammar, rule-5 qualification).
fn split_index(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut in_quote = false;
    while i < b.len() {
        match b[i] {
            b'"' => {
                if in_quote && b.get(i + 1) == Some(&b'"') {
                    i += 1; // "" doubling: stay inside the quote
                } else {
                    in_quote = !in_quote;
                }
            }
            b'=' if !in_quote => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn check_declaration(s: &str, no: usize) -> Result<(), LexErrorAt> {
    if s.starts_with(".?") {
        return Ok(()); // .?id uri — id grammar is prov-ident; permissive here
    }
    let body = &s[2..];
    let word: String = body
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if !DECL_KEYWORDS.contains(&word.as_str()) {
        return Err(LexErrorAt {
            error: LexError::InvalidDirective,
            line: no,
        });
    }
    if matches!(
        word.as_str(),
        "kaiv" | "kaivschema" | "kaivtype" | "kaivunit"
    ) {
        let rest = body[word.len()..].trim_start_matches([' ', '\t']);
        let version: &str = rest.split([' ', '\t']).next().unwrap_or("");
        let major = check_version(version).ok_or(LexErrorAt {
            error: LexError::InvalidVersion,
            line: no,
        })?;
        // This implementation supports format major version 1.
        if major != 1 {
            return Err(LexErrorAt {
                error: LexError::UnsupportedVersion,
                line: no,
            });
        }
    }
    Ok(())
}

/// `^[0-9]+(\.[0-9]+){0,2}$` — returns the major component.
fn check_version(v: &str) -> Option<u64> {
    let mut parts = v.split('.');
    let major = parts.next()?;
    if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut n = 0;
    for p in parts {
        n += 1;
        if n > 2 || p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    major.parse().ok()
}

/// Validate the left side of a content line (SPEC.md § Lexer Errors,
/// INVALID_KEY_ERROR): no whitespace outside quoted names, quoted
/// names terminated and non-empty, and every unquoted namepath
/// segment within the bare-name grammar
/// (`( ALPHA / "_" ) *( ALPHA / DIGIT / "_" )`). `path-seg` — which
/// admits `-` and a leading digit — never applies to data names.
fn check_key(left: &str, no: usize, kind: FileKind) -> Result<(), LexErrorAt> {
    let err = || LexErrorAt {
        error: LexError::InvalidKey,
        line: no,
    };
    // Whitespace scan, quote-aware; also finds the first `'` outside
    // quotes (a canonical line's metadata/namepath delimiter).
    let b = left.as_bytes();
    let mut i = 0;
    let mut in_quote = false;
    let mut tick: Option<usize> = None;
    while i < b.len() {
        match b[i] {
            b'"' => {
                if in_quote && b.get(i + 1) == Some(&b'"') {
                    i += 1;
                } else {
                    in_quote = !in_quote;
                }
            }
            b' ' | b'\t' if !in_quote => return Err(err()),
            b'\'' if !in_quote => tick = tick.or(Some(i)),
            _ => {}
        }
        i += 1;
    }
    if in_quote {
        return Err(err()); // unterminated quoted name
    }

    // On a canonical line the key grammar governs only the namepath
    // after `'`; the metadata prefix is annotation grammar, validated
    // by the consuming stage.
    let mut key = match tick {
        Some(t) => &left[t + 1..],
        None => left,
    };

    // Strip the assignment-operator remnant (`+=`, `;=`, `:=` and
    // `+:=` leave their prefix on the left; schema files add `?=`,
    // type libraries define `&name=`).
    let named_def = key.starts_with('&');
    if kind != FileKind::Data {
        key = key.strip_suffix('?').unwrap_or(key);
        if matches!(kind, FileKind::TypeLib | FileKind::UnitLib) {
            key = key.strip_prefix('&').unwrap_or(key);
        }
        // `.faiv` currency definitions are named by their code.
        if kind == FileKind::UnitLib {
            if let Some(code) = key.strip_prefix('~') {
                return if code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase()) {
                    Ok(())
                } else {
                    Err(err())
                };
            }
        }
    }
    key = key
        .strip_suffix("+:")
        .or_else(|| key.strip_suffix(':').filter(|k| !k.is_empty()))
        .or_else(|| key.strip_suffix('+'))
        .or_else(|| key.strip_suffix(';'))
        .unwrap_or(key);

    // Quote-aware segmentation on `/` and `::`; validate each segment.
    let cs: Vec<char> = key.chars().collect();
    let mut seg = String::new();
    let mut segs: Vec<String> = Vec::new();
    let mut j = 0;
    let mut q = false;
    while j < cs.len() {
        match cs[j] {
            '"' => {
                if q && cs.get(j + 1) == Some(&'"') {
                    seg.push_str("\"\"");
                    j += 1;
                } else {
                    q = !q;
                    seg.push('"');
                }
            }
            '/' if !q => segs.push(std::mem::take(&mut seg)),
            ':' if !q && cs.get(j + 1) == Some(&':') => {
                segs.push(std::mem::take(&mut seg));
                j += 1;
            }
            c => seg.push(c),
        }
        j += 1;
    }
    segs.push(seg);

    for (idx, s) in segs.iter().enumerate() {
        // A leading `/` (or a canonical root `::field`) yields one
        // empty first segment; an empty segment anywhere else is a
        // key violation.
        if s.is_empty() {
            if idx == 0 {
                continue;
            }
            return Err(err());
        }
        if !valid_segment(s) {
            return Err(err());
        }
    }
    // In schema/type-library files a line-leading bare `re` is the
    // reserved pattern-literal introducer; a field so named must be
    // quoted (`"re"=`). `&re` type definitions are unaffected — the
    // sigil disambiguates (SPEC.md § Patterns).
    if matches!(kind, FileKind::Schema | FileKind::TypeLib)
        && !named_def
        && segs.first().is_some_and(|s| s == "re")
    {
        return Err(err());
    }
    Ok(())
}

/// One namepath segment: optional `@` (array) and/or `.` (hidden)
/// marker, then a quoted name, an all-digit index, or a bare name.
fn valid_segment(seg: &str) -> bool {
    let s = seg.strip_prefix('@').unwrap_or(seg);
    let s = s.strip_prefix('.').unwrap_or(s);
    if s.is_empty() {
        return false; // bare `@` or `.`
    }
    if let Some(rest) = s.strip_prefix('"') {
        // Fully quoted: the rest must close the quote and be non-empty.
        return rest
            .strip_suffix('"')
            .is_some_and(|inner| !inner.is_empty());
    }
    let bs = s.as_bytes();
    if bs.iter().all(|b| b.is_ascii_digit()) {
        return true; // index
    }
    (bs[0].is_ascii_alphabetic() || bs[0] == b'_')
        && bs[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(kind: FileKind, line: &str) -> Result<(), LexError> {
        let text = format!("{line}\n");
        lex(text.as_bytes(), kind).map(|_| ()).map_err(|e| e.error)
    }

    #[test]
    fn re_literal_lines_and_reservation() {
        // A valid re-literal line classifies as metadata — including
        // bodies containing `=` and the `:=`-colliding shapes.
        assert!(one(FileKind::Schema, "!str re~^https://a/b=c~").is_ok());
        assert!(one(FileKind::TypeLib, "re%[0-9]+% ..num\n&digits=").is_ok());
        assert!(one(FileKind::Schema, "re:=a:").is_ok());
        // A malformed literal is INVALID_CONSTRAINT, never a content
        // line — `re` is reserved in leading name position.
        assert_eq!(
            one(FileKind::Schema, "re:=a"),
            Err(LexError::InvalidConstraint)
        );
        assert_eq!(
            one(FileKind::Schema, "!str re:^a$"),
            Err(LexError::InvalidConstraint)
        );
        // Reserved bare name: quote to use, `&re` defs unaffected.
        assert_eq!(one(FileKind::Schema, "re=x"), Err(LexError::InvalidKey));
        assert_eq!(one(FileKind::Schema, "re?=x"), Err(LexError::InvalidKey));
        assert_eq!(
            one(FileKind::Schema, "re/sub::f=x"),
            Err(LexError::InvalidKey)
        );
        assert!(one(FileKind::Schema, "\"re\"=x").is_ok());
        assert!(one(FileKind::Schema, "/re::f=x").is_ok()); // not leading
        assert!(one(FileKind::TypeLib, "&re=").is_ok());
        // Data files have no constraint items: no reservation there.
        assert!(one(FileKind::Data, "re=5").is_ok());
        assert!(one(FileKind::Data, "re:=host=a").is_ok());
    }

    #[test]
    fn bare_name_keys() {
        // Every authored left-side shape stays valid.
        for l in [
            "host=x",
            "/server/api::port=1",
            "/@ports+=80",
            "/@tags;=a;b",
            "/server:=a=1|b=2",
            "/@servers+:=h=a|p=1",
            ".var=x",
            "@.tags;=a;b",
            "/.tpl:=a=1",
            "\"server name\"=x",
            "\"weird\"\"name\"=x",
            "\"a=b\"=v",
            "app/\"dark-mode\"::enabled=1",
            "/@a::0=x",
            "!str'::host=x", // canonical line, namepath after '
        ] {
            assert_eq!(one(FileKind::Data, l), Ok(()), "{l}");
        }
        assert_eq!(one(FileKind::Schema, "timeout?=30"), Ok(()));
        assert_eq!(one(FileKind::TypeLib, "&distance_km="), Ok(()));

        // Out-of-grammar bare segments raise INVALID_KEY.
        for l in [
            "retry-count=3", // hyphen
            "9port=1",       // leading digit
            "a.b=1",         // dot mid-name
            "\"\"=x",        // empty quoted name
            "a//b::f=1",     // empty interior segment
            "caf\u{e9}=1",   // non-ASCII
        ] {
            assert_eq!(one(FileKind::Data, l), Err(LexError::InvalidKey), "{l}");
        }
    }
}
