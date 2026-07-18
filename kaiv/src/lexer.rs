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
    /// `.maiv` mapping files: rule 6 never applies (no metadata
    /// annotations, SPEC.md § Mapping Lines), and the `/*` wildcard
    /// is a valid namepath segment on the target side.
    Mapping,
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
    /// Namespace-variable splat as a standalone line (`$/.name`,
    /// authored `.kaiv` only) — the variable's pairs expand at this
    /// point in the open block (SPEC.md § Namespace-Variable Splat).
    /// Payload is the variable name.
    VarSplat(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line<'a> {
    pub no: usize,
    pub kind: LineKind<'a>,
}

const DECL_KEYWORDS: &[&str] = &[
    "kaiv",
    "raiv",
    "daiv",
    "saiv",
    "csaiv",
    "taiv",
    "faiv",
    "maiv",
    "msaiv",
    "schema",
    "types",
    "units",
    "registry",
    "provenance",
    "ref",
    "compose",
    "source",
    "target",
    "via",
    "drop",
    "bind",
    "unique",
    "fk",
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

/// Canonical-kind gate (SPEC.md § Format Declaration): a stream
/// consumed as a canonical kind must open with the matching format
/// declaration — on the first line, or the line after an optional
/// shebang. Only authored `.kaiv` may omit the declaration, so
/// canonical consumers (Denormalizer for `.raiv`, Validator for
/// `.daiv`/`.csaiv`) call this before processing.
pub fn expect_kind(text: &str, kind: &str) -> Result<(), LexErrorAt> {
    let mut lines = text.split('\n');
    let mut first = lines.next().unwrap_or("");
    let mut no = 1;
    if first.starts_with("#!") {
        first = lines.next().unwrap_or("");
        no = 2;
    }
    let body = first.strip_suffix('\r').unwrap_or(first);
    let ok = body
        .trim_start_matches([' ', '\t'])
        .strip_prefix(".!")
        .and_then(|r| r.strip_prefix(kind))
        .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t']));
    if ok {
        Ok(())
    } else {
        Err(LexErrorAt {
            error: LexError::FormatKind,
            line: no,
        })
    }
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
    // A `#!` on the first line is a shebang, not a comment (SPEC.md
    // § Shebang Lines: MUST NOT be treated as a comment) — skipped
    // by the pipeline, classified distinctly from `#` comments.
    if no == 1 && s.starts_with("#!") {
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
            // The array-path token must satisfy the array-path grammar
            // (`["/"] *( step "/" ) "@" name`); a missing `@` or an
            // out-of-grammar segment is INVALID_KEY.
            if !toks.first().is_some_and(|p| valid_array_path(p)) {
                return Err(LexErrorAt {
                    error: LexError::InvalidKey,
                    line: no,
                });
            }
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
            // The ns-path token must satisfy `"/" step *( "/" step )`;
            // a missing leading `/` or bad segment is INVALID_KEY. The
            // only clause the grammar admits after the path is
            // `schema:` (rejected later by the compilers) — any other
            // trailing token is off-grammar and must not vanish
            // silently.
            let toks = crate::table::tokens(inner);
            if !toks.first().is_some_and(|p| valid_ns_path(p))
                || !toks[1..].iter().all(|t| t.starts_with("schema:"))
            {
                return Err(LexErrorAt {
                    error: LexError::InvalidKey,
                    line: no,
                });
            }
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
    // Rule 6: a namespace-variable splat line (`$/.name`) in an
    // authored data file — the whole line is one ns-var-ref
    // (SPEC.md § Namespace-Variable Splat, var-splat-line).
    if kind == FileKind::Data {
        if let Some(name) = s.strip_prefix("$/.") {
            if valid_bare_name(name) {
                return Ok(LineKind::VarSplat(name));
            }
        }
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
        // Rule 6 never applies in `.maiv` (SPEC.md § Mapping Lines):
        // a metadata-looking line is a missing-operator error.
        FileKind::Mapping => false,
    };
    if !meta_ok {
        return Err(LexErrorAt {
            error: LexError::MissingOperator,
            line: no,
        });
    }
    // A malformed `!`/`&` annotation is a lexer-detected INVALID_
    // CONSTRAINT wherever constraints appear, including authored .kaiv
    // (SPEC.md § 11.1). Valid annotations already returned via
    // early_meta, so only malformed ones reach here.
    if matches!(kind, FileKind::Data | FileKind::Schema | FileKind::TypeLib)
        && (first == '!' || first == '&')
        && crate::anno::parse_annotation(s).is_none()
    {
        return Err(LexErrorAt {
            error: LexError::InvalidConstraint,
            line: no,
        });
    }
    // A bare constraint-line leader (`/`, `{`, `[`, `..`) reaching this
    // point failed early_meta's constraint parse (a valid one returns
    // there; a closed `[...]` was taken as a section-open), so it is a
    // malformed/unterminated constraint.
    if matches!(kind, FileKind::Schema | FileKind::TypeLib)
        && (matches!(first, '/' | '{' | '[') || s.starts_with(".."))
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
        FileKind::Mapping => false,
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
        "kaiv" | "raiv" | "daiv" | "saiv" | "csaiv" | "taiv" | "faiv" | "maiv" | "msaiv"
    ) {
        // The data kinds — and `.!maiv`, which carries no identity
        // token since a mapping's identity is its `.!source`/`.!target`
        // endpoint pair (SPEC.md § Mappings) — admit a bare
        // (versionless) declaration meaning version 1; the
        // identity-carrying kinds require the version.
        let data_kind = matches!(word.as_str(), "kaiv" | "raiv" | "daiv" | "maiv");
        let rest = body[word.len()..].trim_start_matches([' ', '\t']);
        let version: &str = rest.split([' ', '\t']).next().unwrap_or("");
        if version.is_empty() && data_kind {
            return Ok(());
        }
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
        // `.!kaiv`/`.!raiv`/`.!daiv`/`.!maiv` admit nothing after the version
        // (SPEC.md § 10.3 format-decl); the other keywords carry a
        // mandatory operand.
        if data_kind && !rest[version.len()..].trim_matches([' ', '\t']).is_empty() {
            return Err(LexErrorAt {
                error: LexError::InvalidVersion,
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
    // A well-formed but oversized major is UNSUPPORTED_VERSION, not
    // INVALID_VERSION: saturate rather than fail so the != 1 branch
    // reports it (SPEC.md § 11.1).
    Some(major.parse().unwrap_or(u64::MAX))
}

/// Validate the left side of a content line (SPEC.md § Lexer Errors,
/// INVALID_KEY_ERROR): no whitespace outside quoted names, quoted
/// names terminated and non-empty, and every unquoted namepath
/// segment within the bare-name grammar
/// (`( ALPHA / "_" ) *( ALPHA / DIGIT / "_" )`). `path-seg` — which
/// admits `-` and a leading digit — never applies to data names.
pub(crate) fn check_key(left: &str, no: usize, kind: FileKind) -> Result<(), LexErrorAt> {
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
    // by the consuming stage. That prefix always begins with `!`
    // (metadata-prefix = "!" type-ref …), so an unquoted `'` whose
    // preceding text is not a metadata prefix is an apostrophe in an
    // authored key (e.g. `it's`) — INVALID_KEY; quote it (`"it's"`).
    let mut key = match tick {
        Some(t) => {
            if !left[..t].starts_with('!') {
                return Err(err());
            }
            &left[t + 1..]
        }
        None => left,
    };

    // Strip the assignment-operator remnant (`+=`, `;=`, `:=` and
    // `+:=` leave their prefix on the left; schema files add `?=`,
    // type libraries define `&name=`).
    let named_def = key.starts_with('&');
    if matches!(kind, FileKind::Schema | FileKind::TypeLib | FileKind::UnitLib) {
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
    // Track the array operators: append-line (`+=`), extend-line
    // (`;=`) and append-struct-line (`+:=`) all take an `array-path`
    // left side, whose final segment carries the `@` sigil (SPEC.md
    // § Formal Grammar, `array-path = [ "/" ] *( step "/" ) "@" name`).
    let mut array_op = false;
    key = if let Some(k) = key.strip_suffix("+:") {
        array_op = true;
        k
    } else if let Some(k) = key.strip_suffix(':').filter(|k| !k.is_empty()) {
        // struct-line: `ns-path ":=" …` — the grammar mandates the
        // leading `/` (unlike array-path, whose `/` is optional), the
        // same enforcement class as the `@` sigil on array operators.
        if kind == FileKind::Data && !k.starts_with('/') {
            return Err(err());
        }
        k
    } else if let Some(k) = key.strip_suffix('+') {
        array_op = true;
        k
    } else if let Some(k) = key.strip_suffix(';') {
        array_op = true;
        k
    } else {
        key
    };

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
        // The `/*` wildcard segment (every element index) is part of
        // the mapping namepath grammar (SPEC.md § Mapping Lines).
        if kind == FileKind::Mapping && s == "*" {
            continue;
        }
        if !valid_segment(s) {
            return Err(err());
        }
    }
    // `+=` / `;=` / `+:=` require an array target: the final segment
    // must carry the `@` sigil (hidden array variables `@.name` also
    // satisfy this — their segment starts with `@`).
    if array_op && !segs.last().is_some_and(|s| s.starts_with('@')) {
        return Err(err());
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

/// Split a path on `/` outside quoted names (`""` doubling aware).
fn split_slash(p: &str) -> Vec<&str> {
    let b = p.as_bytes();
    let mut segs = Vec::new();
    let mut in_quote = false;
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                if in_quote && b.get(i + 1) == Some(&b'"') {
                    i += 1;
                } else {
                    in_quote = !in_quote;
                }
            }
            b'/' if !in_quote => {
                segs.push(&p[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    segs.push(&p[start..]);
    segs
}

/// `array-path = ["/"] *( step "/" ) "@" name`: an optional leading
/// `/`, every step a valid segment, and a final `@`-prefixed name.
fn valid_array_path(p: &str) -> bool {
    let segs = split_slash(p.strip_prefix('/').unwrap_or(p));
    let Some((last, rest)) = segs.split_last() else {
        return false;
    };
    last.starts_with('@')
        && valid_segment(last)
        && rest.iter().all(|s| !s.is_empty() && valid_segment(s))
}

/// `ns-path = "/" step *( "/" step )`: a mandatory leading `/` and at
/// least one valid step.
fn valid_ns_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix('/') else {
        return false;
    };
    let segs = split_slash(rest);
    !segs.is_empty() && segs.iter().all(|s| !s.is_empty() && valid_segment(s))
}

/// The bare-name grammar: `( ALPHA / "_" ) *( ALPHA / DIGIT / "_" )`.
fn valid_bare_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
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
        // An element index has exactly one canonical spelling — no
        // leading zeros — so `00` can neither alias nor shadow `0`
        // (SPEC.md § Canonical index spelling).
        return bs.len() == 1 || bs[0] != b'0';
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
    fn leading_zero_index_is_not_canonical() {
        assert_eq!(
            one(FileKind::Data, "!str'/@a/00::x=1"),
            Err(LexError::InvalidKey)
        );
        assert!(one(FileKind::Data, "!str'/@a/0::x=1").is_ok());
        assert!(one(FileKind::Data, "!str'/@a/10::x=1").is_ok());
    }

    #[test]
    fn apostrophe_in_authored_key_is_invalid() {
        assert_eq!(one(FileKind::Data, "it's=1"), Err(LexError::InvalidKey));
        assert!(one(FileKind::Data, "\"it's\"=1").is_ok());
        assert!(one(FileKind::Data, "!str'::host=1").is_ok());
    }

    #[test]
    fn section_and_ns_open_paths_validated() {
        assert!(one(FileKind::Data, "[/@servers]").is_ok());
        assert_eq!(one(FileKind::Data, "[/servers]"), Err(LexError::InvalidKey));
        assert_eq!(one(FileKind::Data, "[/@bad-name]"), Err(LexError::InvalidKey));
        assert!(one(FileKind::Data, "(/server)").is_ok());
        assert!(one(FileKind::Data, "(/server/backup)").is_ok());
        assert_eq!(one(FileKind::Data, "(server)"), Err(LexError::InvalidKey));
        // A whitespace-only ns interior must not panic.
        assert_eq!(one(FileKind::Data, "( )"), Err(LexError::InvalidKey));
        // Only a `schema:` clause may follow the ns-path; a stray token
        // must not vanish silently downstream.
        assert_eq!(one(FileKind::Data, "(/a b)"), Err(LexError::InvalidKey));
        assert!(one(FileKind::Data, "(/a schema:acme/x)").is_ok());
        // Quoted segments may contain whitespace.
        assert!(one(FileKind::Data, "(/\"my ns\")").is_ok());
        assert!(one(FileKind::Data, "[/@\"my arr\"]").is_ok());
    }

    #[test]
    fn all_inventory_keywords_accepted() {
        for l in [
            ".!maiv",
            ".!maiv 1",
            ".!msaiv 1 corp/c",
            ".!source hub/s",
            ".!target hub/t",
            ".!via acme/m",
            ".!drop /a::b",
            ".!bind:pat sch",
            ".!unique:pat /a::b",
            ".!fk:pat /a::b",
        ] {
            assert!(one(FileKind::Data, l).is_ok(), "{l}");
        }
        assert_eq!(
            one(FileKind::Data, ".!bogus x"),
            Err(LexError::InvalidDirective)
        );
        // `.!maiv` carries no identity token — anything after the
        // version is an error (SPEC.md § Mappings).
        assert_eq!(
            one(FileKind::Data, ".!maiv 1 acme/m"),
            Err(LexError::InvalidVersion)
        );
    }

    #[test]
    fn malformed_constraints_are_lexer_detected() {
        assert_eq!(
            one(FileKind::Data, "!int[1;2]"),
            Err(LexError::InvalidConstraint)
        );
        assert_eq!(
            one(FileKind::Schema, "{red,green"),
            Err(LexError::InvalidConstraint)
        );
        assert_eq!(
            one(FileKind::Schema, "[1,2"),
            Err(LexError::InvalidConstraint)
        );
        assert_eq!(
            one(FileKind::Schema, "/unterminated"),
            Err(LexError::InvalidConstraint)
        );
    }

    #[test]
    fn version_overflow_and_trailing_junk() {
        assert_eq!(
            one(FileKind::Data, ".!kaiv 99999999999999999999"),
            Err(LexError::UnsupportedVersion)
        );
        assert_eq!(
            one(FileKind::Data, ".!kaiv 1 oops"),
            Err(LexError::InvalidVersion)
        );
        assert!(one(FileKind::Data, ".!kaiv 1").is_ok());
    }

    #[test]
    fn canonical_provenance_lines_are_content() {
        // A canonical machine line with inline provenance —
        // DaivBuilder's own output shape — must classify as rule-5
        // content, not as a full-line annotation: the provenance
        // scan stops at the `'` delimiter.
        let text = "!int?sensor1@20250115T093000Z#req-42'/readings::temp=100\n";
        let lines = lex(text.as_bytes(), FileKind::Data).unwrap();
        assert!(matches!(
            lines[0].kind,
            LineKind::Content {
                left: "!int?sensor1@20250115T093000Z#req-42'/readings::temp",
                value: "100"
            }
        ));
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
        assert!(one(FileKind::Data, "/re:=host=a").is_ok());
        // struct-line requires the leading `/` on its ns-path.
        assert_eq!(one(FileKind::Data, "server:=a=1"), Err(LexError::InvalidKey));
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
