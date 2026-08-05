//! The Compiler: authored `.kaiv` → relational canonical `.raiv`.
//! Resolves variables and syntactic sugar (`+=`, `;=`, `:=`, `+:=`,
//! blocks, maps, `&` core shorthands, unit canonicalization); preserves
//! `$field` references for the Denormalizer.

use crate::anno::{parse_annotation, parse_constraint_items, Annotation, Constraint, Item};
use crate::error::{AppError, LexError, LexErrorAt, PipelineError};
use crate::lexer::{lex, FileKind, LineKind};
use crate::resolve::{resolve_named, Resolver};
use crate::unit;
use std::collections::HashMap;

/// Compile with the core-only resolver (embedded `std/core`, no
/// registry configuration).
pub fn compile(input: &[u8]) -> Result<String, PipelineError> {
    compile_with(input, &Resolver::offline())
}

/// [`compile`] with a resolver for `.!types` imports and `&name`
/// references.
pub fn compile_with(input: &[u8], resolver: &Resolver) -> Result<String, PipelineError> {
    let lines = lex(input, FileKind::Data).map_err(PipelineError::Lex)?;
    let mut c = Compiler::new(resolver);
    for line in &lines {
        c.cur_line = line.no;
        c.step(&line.kind)?;
    }
    // An annotation still pending at EOF never found its data line
    // (SPEC.md § Application Errors, MetadataWithoutTargetError).
    if c.pending_anno.is_some() || c.pending_prov.is_some() {
        return Err(PipelineError::app(AppError::MetadataWithoutTarget));
    }
    // Scoped `.!schema` declarations (block discriminants, D-10)
    // join the leading declaration run — declarations precede
    // content in canonical form.
    if !c.scoped_schemas.is_empty() {
        let mut at = 0;
        while at < c.out.len() {
            let t = c.out[at].trim_start_matches([' ', '\t']);
            if t.starts_with(".!") || t.starts_with(".?") {
                at += 1;
            } else {
                break;
            }
        }
        let scoped = std::mem::take(&mut c.scoped_schemas);
        for (i, d) in scoped.into_iter().enumerate() {
            c.out.insert(at + i, d);
        }
    }
    // Canonical output always opens with its kind's declaration
    // (SPEC.md § Format Declaration) — the authored `.!kaiv` (or its
    // absence) becomes bare `.!raiv`.
    c.out.insert(0, ".!raiv".to_string());
    let mut out = c.out.join("\n");
    out.push('\n');
    Ok(out)
}

/// The format-declaration kinds a Compiler input must not declare —
/// everything except authored `kaiv` (SPEC.md § Format Declaration).
const FORMAT_KINDS: &[&str] = &[
    "raiv", "daiv", "saiv", "csaiv", "taiv", "faiv", "maiv", "msaiv",
];

/// Whether `s` is the format declaration for `kind` — the keyword
/// alone or followed by whitespace (and a version).
fn is_format_decl(s: &str, kind: &str) -> bool {
    s.strip_prefix(".!")
        .and_then(|r| r.strip_prefix(kind))
        .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t']))
}

struct Compiler<'r> {
    resolver: &'r Resolver,
    /// `.!types` imports, in declaration order.
    imports: Vec<String>,
    /// `.!units` imports, in declaration order.
    unit_imports: Vec<String>,
    /// Custom unit names from the imports, built lazily.
    custom_units: Option<std::collections::BTreeSet<String>>,
    /// Alias → primary-name map from the imports, built lazily.
    custom_aliases: Option<std::collections::BTreeMap<String, String>>,
    /// `.!registry` Layer 1 overrides (prefix → base).
    registries: Vec<(String, String)>,
    /// `.!verbatim` declared: values are fully verbatim — no `$$`
    /// doubling, no references — and the variable machinery is a
    /// VerbatimContextError (SPEC.md § Verbatim Documents).
    verbatim: bool,
    /// A content-bearing line has been compiled — `.!verbatim` may
    /// no longer appear (declarations precede content).
    content_seen: bool,
    out: Vec<String>,
    /// Scoped `.!schema:/ns ID` declarations collected from block
    /// annotations (D-10) — the discriminants, inserted into the
    /// header at finish.
    scoped_schemas: Vec<String>,
    pending_anno: Option<Annotation>,
    pending_prov: Option<String>,
    scalar_vars: HashMap<String, String>,
    array_vars: HashMap<String, Vec<String>>,
    ns_vars: HashMap<String, Vec<(String, String)>>,
    /// Next element index per canonical array path.
    counters: HashMap<String, usize>,
    blocks: Vec<Block>,
    /// 1-based line of the authored line being compiled, for errors.
    cur_line: usize,
}

enum Block {
    /// `[/@path]` — steps includes the element index as its last step.
    Array { key: String, steps: Vec<String> },
    /// `(/ns)` — accumulated canonical steps.
    Ns { steps: Vec<String> },
}

impl Block {
    fn steps(&self) -> &[String] {
        match self {
            Block::Array { steps, .. } | Block::Ns { steps } => steps,
        }
    }
}

enum Left<'a> {
    VarScalar(&'a str),
    VarNs(&'a str),
    /// `@.name+` — hidden array variable, append one.
    VarArrayAppend(&'a str),
    /// `@.name;` — hidden array variable, extend with `;`-split.
    VarArrayExtend(&'a str),
    ArrStruct(&'a str),
    Struct(&'a str),
    Append(&'a str),
    Extend(&'a str),
    Key(&'a str),
}

fn parse_left(left: &str) -> Left<'_> {
    // Hidden array variables `@.name+`/`@.name;` are checked before
    // the generic append/extend suffixes, since they share them.
    if let Some(rest) = left.strip_prefix("@.") {
        if let Some(name) = rest.strip_suffix('+') {
            if !name.is_empty() {
                return Left::VarArrayAppend(name);
            }
        }
        if let Some(name) = rest.strip_suffix(';') {
            if !name.is_empty() {
                return Left::VarArrayExtend(name);
            }
        }
    }
    if let Some(name) = left.strip_prefix('.') {
        if !name.starts_with('.') && !name.is_empty() {
            return Left::VarScalar(name);
        }
    }
    if let Some(rest) = left.strip_prefix("/.") {
        if let Some(name) = rest.strip_suffix(':') {
            return Left::VarNs(name);
        }
    }
    if let Some(p) = left.strip_suffix("+:") {
        return Left::ArrStruct(p);
    }
    if let Some(p) = left.strip_suffix(':') {
        return Left::Struct(p);
    }
    if let Some(p) = left.strip_suffix('+') {
        return Left::Append(p);
    }
    if let Some(p) = left.strip_suffix(';') {
        return Left::Extend(p);
    }
    Left::Key(left)
}

/// Split an authored namepath into steps and a terminal field
/// (`/a/b::f` → (["a","b"], "f"); `key` → ([], "key")). Quote-aware
/// at both boundaries: the `::` split and the `/` step split skip
/// quoted names, so a quoted segment may contain either operator
/// literally (SPEC.md § Quoted Names).
pub(crate) fn split_namepath(key: &str) -> (Vec<String>, String) {
    let (path, field) = match rsplit_projection(key) {
        Some((p, f)) => (p, f),
        None => ("", key),
    };
    let steps = crate::lexer::split_slash(path.strip_prefix('/').unwrap_or(path))
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(normalize_seg)
        .collect();
    (steps, normalize_seg(field))
}

/// Canonical spelling of one authored segment: a quoted name whose
/// content is a valid bare-name loses its quotes — quoting is
/// deterministic, so there is exactly one canonical representation
/// (SPEC.md § When to Quote). Array/hidden markers pass through.
pub(crate) fn normalize_seg(seg: &str) -> String {
    let (at, rest) = match seg.strip_prefix('@') {
        Some(r) => ("@", r),
        None => ("", seg),
    };
    let (dot, rest) = match rest.strip_prefix('.') {
        Some(r) => (".", r),
        None => ("", rest),
    };
    if let Some(inner) = rest
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .filter(|r| !r.is_empty())
    {
        let b = inner.as_bytes();
        let bare = (b[0].is_ascii_alphabetic() || b[0] == b'_')
            && b[1..]
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == b'_');
        if bare {
            return format!("{at}{dot}{inner}");
        }
    }
    seg.to_string()
}

/// Find the last `::` outside quoted names; return (before, after).
fn rsplit_projection(s: &str) -> Option<(&str, &str)> {
    let b = s.as_bytes();
    let mut in_quote = false;
    let mut found = None;
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
            b':' if !in_quote && b.get(i + 1) == Some(&b':') => {
                found = Some(i);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    found.map(|i| (&s[..i], &s[i + 2..]))
}

/// Just the steps of a pure path key (no `::`), e.g. `/@servers`.
/// A `::` here is a field projection, which has no place in a
/// namespace/array/map-assign path — reject it rather than fold it
/// into a step and emit an off-grammar namepath.
fn path_steps(key: &str) -> Result<Vec<String>, PipelineError> {
    let trimmed = key.trim_start_matches('/');
    if rsplit_projection(trimmed).is_some() {
        return Err(PipelineError::Other(
            "'::' is not allowed in a path (namespace/array/map-assign) position".into(),
        ));
    }
    Ok(crate::lexer::split_slash(trimmed)
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(normalize_seg)
        .collect())
}

fn render_path(steps: &[String]) -> String {
    if steps.is_empty() {
        String::new()
    } else {
        format!("/{}", steps.join("/"))
    }
}

/// Length of a leading identifier run (`[A-Za-z0-9_]`).
fn ident_len(b: &[u8]) -> usize {
    b.iter()
        .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
        .unwrap_or(b.len())
}

/// Length of a leading field-reference token: identifier chars plus
/// the path separators `/` and `::` (SPEC.md § Field References).
/// A trailing `:` that is not part of `::` (and a trailing `/`) are
/// excluded so adjacent literal text is not consumed.
fn fieldref_len(b: &[u8]) -> usize {
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // `@` marks an array step, legal only where a step can begin —
        // token start or right after `/` (`@servers/0::name`,
        // `a/@b/0::f`). Mid-token `@` is adjacent literal text
        // (`$user@example.com` references `$user`), so it ends the ref.
        if c.is_ascii_alphanumeric()
            || c == b'_'
            || c == b'/'
            || (c == b'@' && (i == 0 || b[i - 1] == b'/'))
        {
            i += 1;
        } else if c == b':' && b.get(i + 1) == Some(&b':') {
            i += 2;
        } else {
            break;
        }
    }
    // Do not end on a dangling separator.
    while i > 0 && matches!(b[i - 1], b'/' | b':' | b'@') {
        i -= 1;
    }
    i
}

/// Double every `$` so a resolved literal survives the
/// Denormalizer's `$$` → `$` collapse unaltered.
fn escape_dollars(s: &str) -> String {
    s.replace('$', "$$")
}

/// Whether an authored left side contains a `'` outside a quoted name.
/// Quoted names use `""` doubling (never `''`), so the first bare `'`
/// is a canonical metadata delimiter, invalid in an authored key.
fn has_unquoted_tick(left: &str) -> bool {
    let b = left.as_bytes();
    let mut in_quote = false;
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
            b'\'' if !in_quote => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// Whether a standalone provenance annotation obeys the provenance-list
/// grammar (SPEC.md § 10.5): a non-empty comma-separated list of
/// `prov-ident ["@" timestamp] ["#" prov-ident]`, where a prov-ident is
/// `(ALPHA/DIGIT/_)(ALPHA/DIGIT/_/-)*` and a timestamp is
/// `8DIGIT "T" 6DIGIT "Z"`. No whitespace, apostrophe, or `=`.
fn valid_provenance_list(s: &str) -> bool {
    let ident_ok = |t: &str| {
        let b = t.as_bytes();
        !b.is_empty()
            && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
            && b.iter()
                .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    };
    // Both instant forms: the canonical dashed extended one and
    // the deprecated compact basic one, which is accepted through
    // the 1.0-draft series and canonicalized away on emission
    // (SPEC.md § Provenance).
    let ts_ok = crate::anno::valid_instant;
    let prov_ok = |mut p: &str| {
        // Peel the optional `#dpid` then the optional `@timestamp`
        // (right to left; a prov-ident admits neither `@` nor `#`).
        if let Some((head, dpid)) = p.rsplit_once('#') {
            if !ident_ok(dpid) {
                return false;
            }
            p = head;
        }
        if let Some((ident, ts)) = p.split_once('@') {
            ident_ok(ident) && ts_ok(ts)
        } else {
            ident_ok(p)
        }
    };
    !s.is_empty() && s.split(',').all(prov_ok)
}

/// A single well-formed quoted name: `"…"` with `""` doubling and a
/// non-empty body, nothing before or after.
pub(crate) fn is_quoted_name(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 3 || b[0] != b'"' || b[b.len() - 1] != b'"' {
        return false;
    }
    let inner = &b[1..b.len() - 1];
    let mut i = 0;
    while i < inner.len() {
        if inner[i] == b'"' {
            if inner.get(i + 1) == Some(&b'"') {
                i += 2;
            } else {
                return false; // stray quote: the closing `"` is not ours
            }
        } else {
            i += 1;
        }
    }
    !inner.is_empty()
}

/// Canonical spelling of an authored pair/entry key: an already-quoted
/// name is kept (normalized to bare when its content allows) rather
/// than re-quoted as raw text; anything else canonicalizes like a name.
fn canonical_pair_key(k: &str) -> String {
    if is_quoted_name(k) {
        normalize_seg(k)
    } else {
        canonical_name(k)
    }
}

/// Canonical spelling of a name: a valid bare name stays bare; any
/// other is quoted with `""` doubling (SPEC.md § When to Quote).
fn canonical_name(name: &str) -> String {
    let b = name.as_bytes();
    let bare = !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_');
    // `re` is reserved only in the leading name position of an
    // AUTHORED .saiv/.taiv content line, where it would open a
    // constraint triple. This is canonical output, where the
    // bare-name rule is absolute — quoting it here would give one
    // document two canonical spellings, since a name arriving
    // already quoted normalizes down to bare.
    if bare {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

impl<'r> Compiler<'r> {
    fn new(resolver: &'r Resolver) -> Self {
        Compiler {
            resolver,
            imports: Vec::new(),
            unit_imports: Vec::new(),
            custom_units: None,
            custom_aliases: None,
            registries: Vec::new(),
            verbatim: false,
            content_seen: false,
            out: Vec::new(),
            scoped_schemas: Vec::new(),
            pending_anno: None,
            pending_prov: None,
            scalar_vars: HashMap::new(),
            array_vars: HashMap::new(),
            ns_vars: HashMap::new(),
            counters: HashMap::new(),
            blocks: Vec::new(),
            cur_line: 0,
        }
    }

    fn step(&mut self, kind: &LineKind<'_>) -> Result<(), PipelineError> {
        match kind {
            LineKind::Blank | LineKind::Comment(_) | LineKind::Doc(_) => {
                // A metadata annotation must be immediately followed by
                // a data line (SPEC.md MetadataWithoutTargetError).
                if self.pending_anno.is_some() || self.pending_prov.is_some() {
                    return Err(PipelineError::app(AppError::MetadataWithoutTarget));
                }
                Ok(())
            }
            LineKind::Decl(s) => {
                // Format declarations: authored `.kaiv` may carry
                // `.!kaiv [VERSION]` (or nothing); the Compiler emits
                // `.!raiv` itself, so the authored declaration is
                // consumed, not passed through. A declaration naming
                // any other kind means this is not an authored `.kaiv`
                // stream (SPEC.md § Format Declaration).
                if is_format_decl(s, "kaiv") {
                    return Ok(());
                }
                if FORMAT_KINDS.iter().any(|k| is_format_decl(s, k)) {
                    return Err(PipelineError::Lex(LexErrorAt {
                        error: LexError::FormatKind,
                        line: self.cur_line,
                    }));
                }
                // `.!types` imports and `.!registry` Layer 1 overrides
                // configure resolution; all declarations pass through
                // into canonical output (resolution metadata).
                if let Some(rest) = s.strip_prefix(".!types") {
                    let lib = rest.trim_matches([' ', '\t']);
                    if !lib.is_empty() {
                        self.imports.push(lib.to_string());
                    }
                    // Resolved away: canonical form carries fully-
                    // qualified type names, so the import does not
                    // survive into .raiv/.daiv (SPEC.md § Declaration
                    // Inventory).
                    return Ok(());
                } else if let Some(rest) = s.strip_prefix(".!units") {
                    let lib = rest.trim_matches([' ', '\t']);
                    if !lib.is_empty() {
                        self.unit_imports.push(lib.to_string());
                    }
                } else if let Some(rest) = s.strip_prefix(".!verbatim") {
                    // No arguments, at most once, before any content
                    // (SPEC.md § Verbatim Documents). Carried into
                    // .raiv via the pass-through below.
                    if !rest.trim_matches([' ', '\t']).is_empty()
                        || self.verbatim
                        || self.content_seen
                    {
                        return Err(PipelineError::app(AppError::VerbatimContext));
                    }
                    self.verbatim = true;
                } else if let Some(rest) = s.strip_prefix(".!registry") {
                    if let Some((p, b)) = rest.trim_matches([' ', '\t']).split_once('=') {
                        self.registries.push((p.to_string(), b.to_string()));
                    }
                }
                self.out.push((*s).to_string());
                Ok(())
            }
            LineKind::Meta(s) => {
                self.content_seen = true;
                self.meta(s)
            }
            LineKind::SectionOpen(inner) => {
                self.content_seen = true;
                self.section_open(inner)
            }
            LineKind::SectionClose => {
                if matches!(self.blocks.last(), Some(Block::Array { .. })) {
                    self.blocks.pop();
                }
                Ok(())
            }
            LineKind::NsOpen(inner) => {
                self.content_seen = true;
                self.ns_open(inner)
            }
            LineKind::NsClose => {
                if matches!(self.blocks.last(), Some(Block::Ns { .. })) {
                    self.blocks.pop();
                }
                Ok(())
            }
            LineKind::Content { left, value } => {
                self.content_seen = true;
                self.content(left, value)
            }
            LineKind::VarSplat(name) => {
                self.content_seen = true;
                self.var_splat(name)
            }
        }
    }

    /// A standalone `$/.name` line: the namespace variable's pairs
    /// expand as `key=value` lines at this point. Valid only inside
    /// an open section or namespace block (SPEC.md
    /// § Namespace-Variable Splat) — elsewhere the splat has no
    /// target namespace and is a VariableContextError.
    fn var_splat(&mut self, name: &str) -> Result<(), PipelineError> {
        // A splat line is machinery use — banned in a verbatim
        // document (SPEC.md § Verbatim Documents).
        if self.verbatim {
            return Err(PipelineError::app(AppError::VerbatimContext));
        }
        if self.blocks.is_empty() {
            return Err(PipelineError::app(AppError::VariableContext));
        }
        let pairs = self
            .ns_vars
            .get(name)
            .cloned()
            .ok_or(PipelineError::app(AppError::UndefinedReference))?;
        let steps = self.prefix_steps();
        for (k, v) in pairs {
            self.emit_pair(&steps, &k, v)?;
        }
        self.clear_pending();
        Ok(())
    }

    fn meta(&mut self, s: &str) -> Result<(), PipelineError> {
        // Different-kind annotations stack — one type-designating
        // (`!type` / `&name`) plus one provenance (`?…`), in either
        // order, above one content line (SPEC.md § 1.3.4). Only a
        // second annotation of the SAME kind means the first never
        // reached a data line.
        if s.starts_with('!') {
            if self.pending_anno.is_some() {
                return Err(PipelineError::app(AppError::MetadataWithoutTarget));
            }
            let mut a = parse_annotation(s)
                .ok_or_else(|| PipelineError::Other(format!("bad annotation: {s}")))?;
            // Inline provenance (`!type?prov`) occupies the provenance
            // slot: a standalone `?` line pending alongside it is a
            // same-kind second annotation.
            if a.provenance.is_some() && self.pending_prov.is_some() {
                return Err(PipelineError::app(AppError::MetadataWithoutTarget));
            }
            // A unit is exclusive with a union (grammar; same rule as
            // the schema compiler): the active variant may be `null`,
            // which a unit cannot qualify.
            // Units ride alternatives (D-11): membership-check the
            // head's and every alternative's unit, and rewrite
            // `.faiv` alias spellings to their primary names —
            // canonical lines never carry an alias. The
            // active-variant resolution then stamps the matched
            // alternative's own unit.
            if let Some(u) = a.unit.clone() {
                self.check_unit(&u)?;
                if let Some(d) = self.dealias_unit(&u)? {
                    a.unit = Some(d);
                }
            }
            for i in 0..a.union.len() {
                if let Some(u) = a.union[i].unit.clone() {
                    self.check_unit(&u)?;
                    if let Some(d) = self.dealias_unit(&u)? {
                        a.union[i].unit = Some(d);
                    }
                }
            }
            self.pending_anno = Some(a);
        } else if let Some(p) = s.strip_prefix('?') {
            // The provenance slot may already be filled by a standalone
            // `?` line or by a pending annotation's inline `?prov` —
            // either way a second one is a same-kind duplicate; silently
            // replacing audit data is never acceptable.
            if self.pending_prov.is_some()
                || self
                    .pending_anno
                    .as_ref()
                    .is_some_and(|a| a.provenance.is_some())
            {
                return Err(PipelineError::app(AppError::MetadataWithoutTarget));
            }
            // A standalone provenance line must obey the provenance-list
            // grammar so the emitted prefix re-lexes (SPEC.md § 10.5).
            if !valid_provenance_list(p) {
                return Err(PipelineError::Other(format!(
                    "invalid provenance annotation: {s}"
                )));
            }
            self.pending_prov = Some(p.to_string());
        } else if let Some(rest) = s.strip_prefix('&') {
            if self.pending_anno.is_some() {
                return Err(PipelineError::app(AppError::MetadataWithoutTarget));
            }
            // `&name` resolves against std/core (short form) or the
            // document's `.!types` imports (canonical library path).
            // Trailing constraint items narrow the named type.
            let end = rest.find([' ', '\t']).unwrap_or(rest.len());
            let (name, extra) = rest.split_at(end);
            let type_name = resolve_named(name, &self.imports, self.resolver, &self.registries)?;
            let mut a = Annotation {
                type_name,
                ..Annotation::default()
            };
            let extra = extra.trim_matches([' ', '\t']);
            if !extra.is_empty() {
                let items = parse_constraint_items(extra)
                    .ok_or_else(|| PipelineError::Other(format!("bad annotation items: {s}")))?;
                for it in items {
                    match it {
                        // A span belongs to a schema/.csaiv position, not
                        // a data metadata-prefix (which parse_annotation
                        // cannot re-parse) — reject to keep output
                        // re-lexable.
                        Item::Constraint(Constraint::Span(_)) => {
                            return Err(PipelineError::Other(format!(
                                "span constraints are not valid in a data annotation: {s}"
                            )))
                        }
                        Item::Constraint(c) => a.constraints.push(c),
                        _ => {
                            return Err(PipelineError::Other(format!(
                                "only constraint items may follow &{name}: {s}"
                            )))
                        }
                    }
                }
            }
            self.pending_anno = Some(a);
        }
        Ok(())
    }

    fn prefix_steps(&self) -> Vec<String> {
        self.blocks
            .last()
            .map(|b| b.steps().to_vec())
            .unwrap_or_default()
    }

    fn section_open(&mut self, inner: &str) -> Result<(), PipelineError> {
        // Quote-aware tokenization: a quoted path segment may contain
        // whitespace (`[/@"my arr"]`), which a bare whitespace split
        // would truncate into an unterminated quote.
        let toks = crate::table::tokens(inner);
        let head = toks.first().copied().unwrap_or("");
        // A repeated opener for the same array continues with the next
        // element; compute the base path against the *enclosing* scope.
        if let Some(Block::Array { key, .. }) = self.blocks.last() {
            let key = key.clone();
            self.blocks.pop();
            let outer = self.prefix_steps();
            let mut base = outer;
            base.extend(path_steps(head)?);
            if render_path(&base) == key {
                let idx = self.next_index(&key);
                let mut steps = base;
                steps.push(idx.to_string());
                self.blocks.push(Block::Array { key, steps });
                return Ok(());
            }
            // Different array: fall through with the popped context.
        }
        let mut base = self.prefix_steps();
        base.extend(path_steps(head)?);
        let key = render_path(&base);
        let idx = self.next_index(&key);
        let mut steps = base;
        steps.push(idx.to_string());
        self.blocks.push(Block::Array { key, steps });
        Ok(())
    }

    fn ns_open(&mut self, inner: &str) -> Result<(), PipelineError> {
        // A `schema:` annotation on a namespace block selects the
        // member that validates the block (SPEC.md § Namespace-Scoped
        // Schemas, D-10). The Compiler's part is mechanical and
        // schema-free: emit the selection as the scoped `.!schema`
        // declaration — the block's discriminant in canonical form
        // (§ Canonical Form of namespace blocks) — and flatten the
        // block as usual. A data block names exactly ONE schema; the
        // pipe-separated set belongs to the parent `.saiv`.
        let toks = crate::table::tokens(inner);
        let head = toks.first().copied().unwrap_or("");
        let mut steps = self.prefix_steps();
        steps.extend(path_steps(head)?);
        if let Some(sel) = toks.iter().find_map(|t| t.strip_prefix("schema:")) {
            if sel.is_empty() || sel.contains('|') || toks.len() != 2 {
                return Err(PipelineError::app(AppError::SchemaDelegation));
            }
            let mut np = String::new();
            for s in &steps {
                np.push('/');
                np.push_str(s);
            }
            let decl = format!(".!schema:{np} {sel}");
            if !self.scoped_schemas.contains(&decl) {
                self.scoped_schemas.push(decl);
            }
        }
        self.blocks.push(Block::Ns { steps });
        Ok(())
    }

    fn next_index(&mut self, key: &str) -> usize {
        let c = self.counters.entry(key.to_string()).or_insert(0);
        let idx = *c;
        *c += 1;
        idx
    }

    fn content(&mut self, left: &str, value: &str) -> Result<(), PipelineError> {
        // An unquoted `'` in an authored left side is the canonical
        // metadata/namepath delimiter fed as a key — the lexer defers
        // this metadata-prefix check to the compiler (its consuming
        // stage). Reject it as INVALID_KEY rather than fold it into a
        // namepath and emit an unparseable double-tick line; an
        // apostrophe in a name must be quoted (`"it's"=5`).
        if has_unquoted_tick(left) {
            return Err(PipelineError::Lex(LexErrorAt {
                error: LexError::InvalidKey,
                line: self.cur_line,
            }));
        }
        match parse_left(left) {
            // The variable machinery is banned positionally in a
            // verbatim document (SPEC.md § Verbatim Documents) — a
            // definition of any shape is a contradiction with the
            // declaration, surfaced rather than left dead.
            Left::VarScalar(_)
            | Left::VarArrayAppend(_)
            | Left::VarArrayExtend(_)
            | Left::VarNs(_)
                if self.verbatim =>
            {
                Err(PipelineError::app(AppError::VerbatimContext))
            }
            Left::VarScalar(name) => {
                // A scalar variable stores its *literal* value; the
                // single escape happens when it is substituted.
                let v = self.resolve_literal(value)?;
                self.scalar_vars.insert(name.to_string(), v);
                self.clear_pending();
                Ok(())
            }
            Left::VarArrayAppend(name) => {
                // `@.a+=$@.b` splices, like the visible-array forms.
                let vs = self.splice_or_single(value)?;
                self.array_vars
                    .entry(name.to_string())
                    .or_default()
                    .extend(vs);
                self.clear_pending();
                Ok(())
            }
            Left::VarArrayExtend(name) => {
                let mut vs = Vec::new();
                for elem in value.split(';') {
                    vs.extend(self.splice_or_single(elem)?);
                }
                self.array_vars
                    .entry(name.to_string())
                    .or_default()
                    .extend(vs);
                self.clear_pending();
                Ok(())
            }
            Left::VarNs(name) => {
                let pairs = self.parse_pairs(value)?;
                self.ns_vars.insert(name.to_string(), pairs);
                self.clear_pending();
                Ok(())
            }
            Left::Struct(path) => {
                let mut steps = self.prefix_steps();
                steps.extend(path_steps(path)?);
                self.emit_pairs(&steps, value)
            }
            Left::ArrStruct(path) => {
                let mut steps = self.prefix_steps();
                steps.extend(path_steps(path)?);
                let key = render_path(&steps);
                let idx = self.next_index(&key);
                steps.push(idx.to_string());
                self.emit_pairs(&steps, value)
            }
            Left::Append(path) => {
                let mut steps = self.prefix_steps();
                steps.extend(path_steps(path)?);
                let key = render_path(&steps);
                // `field+=$@.name` appends the whole hidden array.
                let elems = self.splice_or_single(value)?;
                for e in elems {
                    let idx = self.next_index(&key);
                    self.emit(&steps, &idx.to_string(), e)?;
                }
                self.clear_pending();
                Ok(())
            }
            Left::Extend(path) => {
                let mut steps = self.prefix_steps();
                steps.extend(path_steps(path)?);
                let key = render_path(&steps);
                for elem in value.split(';') {
                    for e in self.splice_or_single(elem)? {
                        let idx = self.next_index(&key);
                        self.emit(&steps, &idx.to_string(), e)?;
                    }
                }
                self.clear_pending();
                Ok(())
            }
            Left::Key(key) => {
                if self
                    .pending_anno
                    .as_ref()
                    .is_some_and(|a| a.type_name == "map")
                {
                    return self.emit_map(key, value);
                }
                let (rel, field) = split_namepath(key);
                let mut steps = self.prefix_steps();
                steps.extend(rel);
                let v = self.resolve_value(value)?;
                self.emit(&steps, &field, v)?;
                self.clear_pending();
                Ok(())
            }
        }
    }

    /// A value that is exactly `$@.name` splices the hidden array
    /// variable's elements; anything else is one resolved value.
    fn splice_or_single(&self, value: &str) -> Result<Vec<String>, PipelineError> {
        if let Some(name) = value.strip_prefix("$@.") {
            // In a verbatim document only the exact whole-value
            // splice shape is the banned position; anything else is
            // literal bytes (SPEC.md § Verbatim Documents).
            if self.verbatim {
                if !name.is_empty() && ident_len(name.as_bytes()) == name.len() {
                    return Err(PipelineError::app(AppError::VerbatimContext));
                }
            } else {
                return self
                    .array_vars
                    .get(name)
                    .cloned()
                    .ok_or(PipelineError::app(AppError::UndefinedReference));
            }
        }
        Ok(vec![self.resolve_value(value)?])
    }

    fn emit_map(&mut self, key: &str, value: &str) -> Result<(), PipelineError> {
        let a = self.pending_anno.take().unwrap();
        // The map-type grammar admits neither a unit nor inline
        // constraints (SPEC.md § 10.5) — reject rather than silently
        // drop them.
        if a.unit.is_some() || !a.constraints.is_empty() {
            return Err(PipelineError::Other(
                "a map annotation admits neither a unit nor inline constraints".into(),
            ));
        }
        let vtype = a.map_value.clone().unwrap_or_else(|| "str".to_string());
        // Provenance stacked on the map annotation applies to every
        // emitted entry line, like a `;=`/`:=` expansion (SPEC.md
        // § an annotation applies to every line of its expansion).
        let prov = self.pending_prov.take().or_else(|| a.provenance.clone());
        let p = prov.as_ref().map(|p| format!("?{p}")).unwrap_or_default();
        // The map itself is a namespace: a map-assign key is a pure
        // path (`options`, `/config/settings` — SPEC.md
        // map-assign-line), every segment a step. Validate the path
        // even when the map is empty.
        let mut steps = self.prefix_steps();
        steps.extend(path_steps(key)?);
        if value == "{}" {
            return Ok(()); // empty map: no canonical entry lines
        }
        for pair in value.split(';') {
            let (k, v) = pair
                .split_once(':')
                // An entry without `:` means a `:`/`;` collided with
                // the inline map form's delimiters (SPEC.md § Errors).
                .ok_or(PipelineError::app(AppError::DelimiterCollision))?;
            if k.is_empty() {
                // An empty map key would emit an empty quoted name
                // (`""`), which violates 1*qn-char and fails re-lex.
                return Err(PipelineError::Lex(LexErrorAt {
                    error: LexError::InvalidKey,
                    line: self.cur_line,
                }));
            }
            // Map keys canonicalize like any name: a non-bare key is
            // quoted so the emitted line re-lexes (SPEC.md § Maps in
            // the Compiled Schema).
            let line = format!(
                "!{vtype}{p}'{}::{}={}",
                render_path(&steps),
                canonical_pair_key(k),
                self.resolve_value(v)?
            );
            self.out.push(line);
        }
        Ok(())
    }

    /// Struct-assignment right side → resolved (field, value) pairs.
    fn parse_pairs(&mut self, value: &str) -> Result<Vec<(String, String)>, PipelineError> {
        if let Some(name) = value.strip_prefix("$/.") {
            // Same whole-value rule as splice_or_single: in a
            // verbatim document the exact splice shape is banned;
            // anything else falls through to ordinary pair parsing.
            if self.verbatim {
                if !name.is_empty() && ident_len(name.as_bytes()) == name.len() {
                    return Err(PipelineError::app(AppError::VerbatimContext));
                }
            } else {
                return self
                    .ns_vars
                    .get(name)
                    .cloned()
                    .ok_or(PipelineError::app(AppError::UndefinedReference));
            }
        }
        let mut pairs = Vec::new();
        for pair in value.split('|') {
            let (k, v) = pair
                .split_once('=')
                // A piece without `=` means a `|` collided with the
                // pair form's delimiter (SPEC.md § Errors).
                .ok_or(PipelineError::app(AppError::DelimiterCollision))?;
            let v = self.resolve_value(v)?;
            pairs.push((k.to_string(), v));
        }
        Ok(pairs)
    }

    fn emit_pairs(&mut self, steps: &[String], value: &str) -> Result<(), PipelineError> {
        let pairs = self.parse_pairs(value)?;
        for (k, v) in pairs {
            self.emit_pair(steps, &k, v)?;
        }
        self.clear_pending();
        Ok(())
    }

    /// Emit one struct-assignment pair. A plain key is canonicalized
    /// (quoted when not a bare name) so the emitted line re-lexes,
    /// exactly as map entries are; an `@name` prefix is an inline array
    /// op whose name segment is likewise canonicalized.
    fn emit_pair(&mut self, steps: &[String], k: &str, v: String) -> Result<(), PipelineError> {
        if let Some(name) = k.strip_prefix('@') {
            // Array ops inside a struct value: @tags+=v / @tags;=a;b
            let (name, multi) = match name.strip_suffix(['+', ';']) {
                Some(n) => (n, name.ends_with(';')),
                None => (name, false),
            };
            let mut asteps = steps.to_vec();
            asteps.push(format!("@{}", canonical_pair_key(name)));
            let key = render_path(&asteps);
            let elems: Vec<&str> = if multi {
                v.split(';').collect()
            } else {
                vec![v.as_str()]
            };
            for e in elems {
                let idx = self.next_index(&key);
                self.emit(&asteps, &idx.to_string(), e.to_string())?;
            }
        } else {
            self.emit(steps, &canonical_pair_key(k), v)?;
        }
        Ok(())
    }

    /// Resolve a scalar value into its `.raiv` form (SPEC.md
    /// § The "Almost Verbatim" Principle). Values are verbatim
    /// except: `$$` is a literal `$` (preserved for the
    /// Denormalizer, which collapses it); `$.name` (and the
    /// container forms `$@.`/`$/.`) are hidden-variable references
    /// substituted here — arrays/namespaces only whole-value;
    /// `$field` / `$path::field` are field references, rewritten to
    /// their fully-qualified (block-prefixed) namepath and left for
    /// the Denormalizer to inline. A lone `$` that forms no valid
    /// reference is an error — literal dollars are written `$$`.
    fn resolve_value(&self, value: &str) -> Result<String, PipelineError> {
        // A verbatim document has no reference machinery: the value
        // is the value, every `$` literal (SPEC.md § Verbatim
        // Documents).
        if self.verbatim {
            return Ok(value.to_string());
        }
        // Container references in a scalar position have no text
        // representation to substitute — VariableContextError
        // (SPEC.md § Namespace-Variable Splat). Splice positions go
        // through splice_or_single / parse_pairs, never here.
        if value.starts_with("$@.") || value.starts_with("$/.") {
            return Err(PipelineError::app(AppError::VariableContext));
        }
        let b = value.as_bytes();
        let mut out = String::with_capacity(value.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'$' {
                // Copy the whole non-`$` run as a str slice: `$` is
                // ASCII, so the run boundaries are char boundaries
                // and multibyte UTF-8 passes through verbatim.
                let start = i;
                while i < b.len() && b[i] != b'$' {
                    i += 1;
                }
                out.push_str(&value[start..i]);
                continue;
            }
            // b[i] == '$'
            match b.get(i + 1) {
                Some(b'$') => {
                    out.push_str("$$"); // literal dollar, deferred to denorm
                    i += 2;
                }
                Some(b'.') => {
                    // `$.name` hidden scalar variable.
                    let start = i + 2;
                    let end = start + ident_len(&b[start..]);
                    if end == start {
                        return Err(PipelineError::app(AppError::UndefinedReference));
                    }
                    let name = &value[start..end];
                    let v = self
                        .scalar_vars
                        .get(name)
                        .ok_or(PipelineError::app(AppError::UndefinedReference))?;
                    out.push_str(&escape_dollars(v));
                    i = end;
                }
                // Only the dot form (`$@.name` / `$/.name`) is a
                // container variable with no text representation; a
                // dot-less `$@servers/0::field` / `$/path::field` is a
                // field reference and falls through to the `_` arm
                // (SPEC.md § 2.5.3: the `.` is the discriminant).
                Some(b'@') if b.get(i + 2) == Some(&b'.') => {
                    return Err(PipelineError::app(AppError::VariableContext));
                }
                Some(b'/') if b.get(i + 2) == Some(&b'.') => {
                    return Err(PipelineError::app(AppError::VariableContext));
                }
                _ => {
                    // `$field` / `$path::field` field reference.
                    let start = i + 1;
                    let end = start + fieldref_len(&b[start..]);
                    if end == start {
                        // Lone `$` — write `$$` for a literal dollar.
                        return Err(PipelineError::app(AppError::UndefinedReference));
                    }
                    out.push('$');
                    out.push_str(&self.qualify_ref(&value[start..end]));
                    i = end;
                }
            }
        }
        Ok(out)
    }

    /// A scalar variable's literal value: variables substituted,
    /// `$$` collapsed to a real `$`. Field references are not
    /// permitted in a variable definition (they resolve at
    /// denormalization, after variables are gone).
    fn resolve_literal(&self, value: &str) -> Result<String, PipelineError> {
        let b = value.as_bytes();
        let mut out = String::with_capacity(value.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'$' {
                // Non-`$` run copied as a str slice (UTF-8-safe; see
                // resolve_value).
                let start = i;
                while i < b.len() && b[i] != b'$' {
                    i += 1;
                }
                out.push_str(&value[start..i]);
                continue;
            }
            match b.get(i + 1) {
                Some(b'$') => {
                    out.push('$');
                    i += 2;
                }
                Some(b'.') => {
                    let start = i + 2;
                    let end = start + ident_len(&b[start..]);
                    if end == start {
                        return Err(PipelineError::app(AppError::UndefinedReference));
                    }
                    let name = &value[start..end];
                    let v = self
                        .scalar_vars
                        .get(name)
                        .ok_or(PipelineError::app(AppError::UndefinedReference))?;
                    out.push_str(v);
                    i = end;
                }
                _ => {
                    return Err(PipelineError::Other(format!(
                        "field references are not allowed in a variable definition: {value}"
                    )));
                }
            }
        }
        Ok(out)
    }

    /// Rewrite a field reference to its fully-qualified namepath by
    /// applying the active namespace-block prefix (SPEC.md § Field
    /// References: "the fully-qualified namepath is constructed,
    /// applying any active namespace block prefix"). At root the
    /// reference is preserved as authored.
    fn qualify_ref(&self, r: &str) -> String {
        let prefix = self.prefix_steps();
        if prefix.is_empty() {
            return r.to_string();
        }
        let (rel, field) = split_namepath(r);
        let mut steps = prefix;
        steps.extend(rel);
        format!("{}::{}", steps.join("/"), field)
    }

    fn emit(&mut self, steps: &[String], field: &str, value: String) -> Result<(), PipelineError> {
        // A canonical metadata prefix carries no union (SPEC.md
        // § 10.6): a union annotation is authoring sugar the Compiler
        // resolves to the ACTIVE VARIANT — per emitted value, since a
        // splice's elements may each pick a different alternative.
        let prefix = match &self.pending_anno {
            Some(a) if !a.union.is_empty() => {
                let active = self.pick_active_variant(a, &value)?;
                render_prefix_for(&active, self.pending_prov.as_deref())
            }
            _ => self.render_prefix(),
        };
        self.out.push(format!(
            "{prefix}'{}::{}={}",
            render_path(steps),
            field,
            value
        ));
        Ok(())
    }

    /// The active variant of a union annotation for one value: the
    /// first alternative — head first, then left to right — whose
    /// lowered definition (base type plus authored narrowing) the
    /// value satisfies (SPEC.md § Null Semantics, § Tagged unions).
    /// A value satisfying no alternative is a TypeMismatchError at
    /// compile time.
    fn pick_active_variant(
        &self,
        a: &Annotation,
        value: &str,
    ) -> Result<Annotation, PipelineError> {
        let alts = std::iter::once((a.type_name.as_str(), &a.constraints, a.unit.as_ref())).chain(
            a.union
                .iter()
                .map(|alt| (alt.name.as_str(), &alt.constraints, alt.unit.as_ref())),
        );
        for (name, narrowing, unit) in alts {
            if self.variant_accepts(name, narrowing, value)? {
                return Ok(Annotation {
                    type_name: name.to_string(),
                    constraints: narrowing.clone(),
                    // The matched alternative's own unit rides the
                    // active variant (D-11) — never another's.
                    unit: unit.cloned(),
                    provenance: a.provenance.clone(),
                    ..Annotation::default()
                });
            }
        }
        Err(PipelineError::app(AppError::TypeMismatch))
    }

    /// Whether a value satisfies one union alternative, lowered
    /// exactly as the schema compiler lowers it (base definition plus
    /// narrowing, resolver- and registry-aware).
    fn variant_accepts(
        &self,
        name: &str,
        narrowing: &[Constraint],
        value: &str,
    ) -> Result<bool, PipelineError> {
        // Value acceptance is unit-independent (the unit is identity,
        // not a value constraint), so the alternative renders unitless.
        let rendered = crate::schema::render_union_alt(
            name,
            narrowing,
            None,
            self.resolver,
            &self.registries,
        )?;
        let inner = rendered
            .strip_prefix(name)
            .and_then(|s| s.strip_prefix('('))
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or("");
        if inner.is_empty() {
            return Ok(true); // unconstrained (str-like): accepts anything
        }
        let items = parse_constraint_items(inner)
            .ok_or_else(|| PipelineError::Other(format!("unloadable union alternative: {name}")))?;
        Ok(crate::validator::default_applicable(&items, value))
    }

    /// Canonical metadata prefix from the pending annotation state.
    /// Read-only: a line that expands to several canonical lines
    /// (`;=`, `:=`, `+:=`) applies its annotation to every one of
    /// them; the caller clears the pending state after the expansion.
    fn render_prefix(&self) -> String {
        let a = self.pending_anno.clone().unwrap_or_default();
        render_prefix_for(&a, self.pending_prov.as_deref())
    }

    fn clear_pending(&mut self) {
        self.pending_anno = None;
        self.pending_prov = None;
    }

    /// Rewrite `.faiv` alias factor names in a unit expression to
    /// their primary targets; `None` when nothing changed.
    fn dealias_unit(&mut self, u: &str) -> Result<Option<String>, PipelineError> {
        if self.unit_imports.is_empty() {
            return Ok(None);
        }
        if self.custom_aliases.is_none() {
            let mut map = std::collections::BTreeMap::new();
            for lib in &self.unit_imports {
                map.extend(self.resolver.unit_aliases(lib, &self.registries)?);
            }
            self.custom_aliases = Some(map);
        }
        let map = self.custom_aliases.as_ref().unwrap();
        if map.is_empty() {
            return Ok(None);
        }
        Ok(unit::dealias(u, map))
    }

    /// Unit-name membership: built-in, currency, or defined by an
    /// imported `.faiv` library (SPEC.md § Built-in units, § Unit
    /// Definition Files).
    fn check_unit(&mut self, u: &str) -> Result<(), PipelineError> {
        if self.custom_units.is_none() {
            let mut set = std::collections::BTreeSet::new();
            for lib in &self.unit_imports {
                set.extend(self.resolver.unit_names(lib, &self.registries)?);
            }
            self.custom_units = Some(set);
        }
        if !unit::members_ok(u, self.custom_units.as_ref().unwrap()) {
            return Err(PipelineError::Other(format!(
                "unknown unit '{u}' (not built-in, not defined by any .!units import)"
            )));
        }
        Ok(())
    }
}

/// Render one annotation (plus a stacked provenance line, which wins
/// over inline provenance) as a canonical metadata prefix. Unions
/// never reach here — `emit` resolves them to the active variant
/// first, since §10.6's metadata-prefix has no union form.
fn render_prefix_for(a: &Annotation, prov: Option<&str>) -> String {
    let mut s = String::from("!");
    s.push_str(if a.type_name.is_empty() {
        // The elided-type unit annotation stays elided in `.raiv`
        // (D-15) — the schema-aware Denormalizer resolves it; a
        // fully unannotated line is the identity str.
        if a.unit.is_some() {
            ""
        } else {
            "str"
        }
    } else {
        &a.type_name
    });
    // Canonical constraint order: pattern, range, enum, length.
    let rank = |c: &Constraint| match c {
        Constraint::Pattern(_) => 0,
        Constraint::Range(..) => 1,
        Constraint::Enum(_) => 2,
        Constraint::Length(_) => 3,
        Constraint::Span(_) => 4,
    };
    let mut cs: Vec<&Constraint> = a.constraints.iter().collect();
    cs.sort_by_key(|c| rank(c));
    for c in cs {
        s.push_str(&render_constraint(c));
    }
    if let Some(u) = &a.unit {
        s.push(':');
        s.push_str(&unit::canonicalize(u).unwrap_or_else(|| u.clone()));
    }
    if let Some(p) = prov.map(str::to_string).or_else(|| a.provenance.clone()) {
        s.push('?');
        // Canonicalize each entry's instant on the way out, the way
        // the unit above canonicalizes: a deprecated compact instant
        // is accepted on input and never survives into canonical
        // output (SPEC.md § Provenance).
        s.push_str(&canonical_provenance(&p));
    }
    s
}

/// Rewrite every instant in a provenance list into its canonical
/// dashed form, leaving ids and data-point identifiers alone.
fn canonical_provenance(list: &str) -> String {
    list.split(',')
        .map(|entry| {
            // `id ["@" instant] ["#" dpid]` — peel the dpid first,
            // since an instant admits neither separator.
            let (head, dpid) = match entry.rsplit_once('#') {
                Some((h, d)) => (h, Some(d)),
                None => (entry, None),
            };
            let head = match head.split_once('@') {
                Some((id, ts)) => format!("{id}@{}", crate::anno::canonical_instant(ts)),
                None => head.to_string(),
            };
            match dpid {
                Some(d) => format!("{head}#{d}"),
                None => head,
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn render_constraint(c: &Constraint) -> String {
    match c {
        Constraint::Pattern(b) => format!("/{b}/"),
        Constraint::Range(lo, hi) => format!(
            "[{},{}]",
            lo.as_deref().unwrap_or(""),
            hi.as_deref().unwrap_or("")
        ),
        Constraint::Enum(vs) => format!("{{{}}}", vs.join(",")),
        Constraint::Length(inner) => format!("#{}", render_constraint(inner)),
        Constraint::Span(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use crate::error::{AppError, PipelineError};

    #[test]
    fn quoted_bare_able_names_normalize() {
        // Exactly one canonical representation (SPEC.md § When to
        // Quote): a quoted name that is a valid bare-name loses its
        // quotes; one that is not keeps them.
        let daiv = crate::compile(b".!kaiv 1\n\"host\"=y\n\"a b\"=z\n\"re\"=x\n").unwrap();
        assert!(daiv.contains("!str'::host=y\n"));
        assert!(daiv.contains("!str'::\"a b\"=z\n"));
        assert!(daiv.contains("!str'::re=x\n"));
        // Path steps normalize too.
        let d2 = crate::compile(b".!kaiv 1\n/\"srv\"::port=1\n").unwrap();
        assert!(d2.contains("!str'/srv::port=1\n"));
    }

    fn build(input: &str) -> String {
        let raiv = crate::compile(input.as_bytes()).unwrap();
        crate::denorm::denormalize(&raiv).unwrap()
    }

    #[test]
    fn canonical_quoting_does_not_depend_on_the_authored_form() {
        // Canonical form has exactly one spelling per namepath
        // (SPEC.md § When to Quote): a name matching `bare-name` MUST
        // NOT be quoted, whichever authoring form produced it. `re`
        // is the interesting case — it is reserved in the leading
        // name position of an AUTHORED .saiv/.taiv content line, and
        // the inline-pair path used to carry that quoting into
        // canonical data, so `/x:=re=` and `/x::re=` canonicalized
        // differently and a schema inferred from one rejected the
        // other.
        for authored in [
            ".!kaiv 1\n!null\n/x:=re=\n",
            ".!kaiv 1\n!null\n/x::re=\n",
            ".!kaiv 1\n!null\n/x::\"re\"=\n",
        ] {
            let d = build(authored);
            assert!(d.contains("!null'/x::re=\n"), "{authored:?} -> {d}");
        }
        // A name that genuinely needs quoting still gets it, from
        // either form.
        for authored in [
            ".!kaiv 1\n!null\n/x:=\"a-b\"=\n",
            ".!kaiv 1\n!null\n/x::\"a-b\"=\n",
        ] {
            let d = build(authored);
            assert!(d.contains("!null'/x::\"a-b\"=\n"), "{authored:?} -> {d}");
        }
    }

    #[test]
    fn quoted_interior_segments() {
        // Quoted names are legal as any namepath segment (SPEC.md
        // § Quoted Names): between `/` steps, after `@`, after `::`.
        let d = build(".!kaiv 1\n!str\n/app/\"dark-mode\"::enabled=true\n");
        assert!(d.contains("!str'/app/\"dark-mode\"::enabled=true\n"), "{d}");
        let d = build(".!kaiv 1\n[/@\"x-servers\"]\n!int\n\"retry-count\"=3\n[]\n");
        assert!(
            d.contains("!int'/@\"x-servers\"/0::\"retry-count\"=3\n"),
            "{d}"
        );
        // A quoted all-digit segment is an ordinary name, never an
        // array index — it stays quoted (a bare `0` would address an
        // element).
        let d = build(".!kaiv 1\n/x/\"0\"::f=1\n");
        assert!(d.contains("!str'/x/\"0\"::f=1\n"), "{d}");
        // A quoted segment may contain the path operators literally —
        // the step split is quote-aware, so `//` inside quotes is one
        // name, not an empty step to be dropped.
        for (input, want) in [
            ("/\"a//b\"::f=1\n", "!str'/\"a//b\"::f=1\n"),
            ("/\"a/b\"/x::f=1\n", "!str'/\"a/b\"/x::f=1\n"),
            ("/\"a::b\"::f=1\n", "!str'/\"a::b\"::f=1\n"),
            ("/\"a@b\"::f=1\n", "!str'/\"a@b\"::f=1\n"),
        ] {
            let d = build(&format!(".!kaiv 1\n{input}"));
            assert!(d.contains(want), "{input} -> {d}");
        }
        // Over-quoted bare-able interior segments normalize (the
        // MUST-iff rule): authored quoting of a bare-able name is
        // legal but canonical form strips it.
        let d = build(".!kaiv 1\n/\"app\"/\"x\"::f=1\n");
        assert!(d.contains("!str'/app/x::f=1\n"), "{d}");
        let d = build(".!kaiv 1\n[/@\"servers\"]\nh=a\n[]\n");
        assert!(d.contains("!str'/@servers/0::h=a\n"), "{d}");
    }

    #[test]
    fn array_variables_splice_not_corrupt() {
        // `@.name` is a hidden array variable — elided, spliced by
        // `$@.name`, never a `.`-name in canonical output.
        let d = build(".!kaiv 1\n@.tags;=a;b\n@.tags+=c\n/@labels;=$@.tags\n");
        assert_eq!(
            d,
            ".!daiv\n!str'/@labels::0=a\n!str'/@labels::1=b\n!str'/@labels::2=c\n"
        );
        assert!(!d.contains("/@.tags"), "hidden name leaked: {d}");
    }

    #[test]
    fn field_ref_honors_namespace_block() {
        // `$host` inside `(/server)` resolves to `/server::host`,
        // not the root `::host`.
        let d = build(".!kaiv 1\n(/server)\nhost=web1\nalias=$host\n()\n");
        assert!(d.contains("!str'/server::alias=web1\n"), "{d}");
    }

    #[test]
    fn dollar_escape_and_mid_value_interpolation() {
        assert!(build(".!kaiv 1\nprice=$$5\n").contains("!str'::price=$5\n"));
        let d = build(".!kaiv 1\n.h=example.com\nurl=http://$.h/api\n");
        assert!(d.contains("!str'::url=http://example.com/api\n"), "{d}");
        // A variable value carrying a `$` survives verbatim.
        let d2 = build(".!kaiv 1\n.p=$$9\nout=$.p\n");
        assert!(d2.contains("!str'::out=$9\n"), "{d2}");
    }

    #[test]
    fn verbatim_documents_take_dollars_literally() {
        // `.!verbatim` (SPEC.md § Verbatim Documents): every `$` in
        // a value is literal — no doubling, no references — and the
        // declaration survives into .raiv but not .daiv.
        let raiv = crate::compile(b".!kaiv 1\n.!verbatim\nprice=$5\npattern=^abc$\n").unwrap();
        assert!(raiv.contains(".!verbatim\n"), "{raiv}");
        assert!(raiv.contains("!str'::price=$5\n"), "{raiv}");
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        assert!(!daiv.contains(".!verbatim"), "must discharge: {daiv}");
        assert!(daiv.contains("!str'::price=$5\n"), "{daiv}");
        assert!(daiv.contains("!str'::pattern=^abc$\n"), "{daiv}");
        // Byte-identical .daiv to the escaped-authored equivalent.
        let escaped = build(".!kaiv 1\nprice=$$5\npattern=^abc$$\n");
        assert_eq!(daiv, escaped);

        // `$$` is two literal dollars in a verbatim document.
        let d = build(".!kaiv 1\n.!verbatim\nsep=$$\n");
        assert!(d.contains("!str'::sep=$$\n"), "{d}");
        // Mid-value machinery lookalikes are bytes, not errors.
        let d = build(".!kaiv 1\n.!verbatim\nnote=see $@.other and $/.tpl\n");
        assert!(d.contains("!str'::note=see $@.other and $/.tpl\n"), "{d}");
    }

    #[test]
    fn verbatim_bans_the_machinery_positionally() {
        let err = |src: &str| match crate::compile(src.as_bytes()) {
            Err(PipelineError::App(e)) => e.error,
            other => panic!("expected app error, got {other:?}"),
        };
        // Class 1: variable definitions of every shape.
        assert_eq!(err(".!verbatim\n.x=1\n"), AppError::VerbatimContext);
        assert_eq!(err(".!verbatim\n@.a+=x\n"), AppError::VerbatimContext);
        assert_eq!(err(".!verbatim\n@.a;=x;y\n"), AppError::VerbatimContext);
        assert_eq!(err(".!verbatim\n/.t:=a=1\n"), AppError::VerbatimContext);
        // Class 2: the standalone splat line.
        assert_eq!(
            err(".!verbatim\n(/ns)\n$/.t\n()\n"),
            AppError::VerbatimContext
        );
        // Class 3: whole-value splices.
        assert_eq!(err(".!verbatim\n/@a;=$@.t\n"), AppError::VerbatimContext);
        assert_eq!(err(".!verbatim\n/ns:=$/.t\n"), AppError::VerbatimContext);
        // Misuse of the declaration itself: arguments, a repeat, or
        // a position after content.
        assert_eq!(err(".!verbatim extra\n"), AppError::VerbatimContext);
        assert_eq!(err(".!verbatim\n.!verbatim\n"), AppError::VerbatimContext);
        assert_eq!(err("a=1\n.!verbatim\n"), AppError::VerbatimContext);
    }

    #[test]
    fn verbatim_raiv_is_consumed_without_collapse() {
        // Hand-crafted `.raiv` carrying the declaration: values pass
        // through with no `$$` collapse and no reference lookup.
        let daiv = crate::denorm::denormalize(
            ".!raiv\n.!verbatim\n!str'::a=$$x\n!str'::b=$undefined\n",
        )
        .unwrap();
        assert_eq!(daiv, ".!daiv\n!str'::a=$$x\n!str'::b=$undefined\n");
        // Misuse in canonical input errors like the Compiler's.
        for bad in [
            ".!raiv\n!str'::a=1\n.!verbatim\n",
            ".!raiv\n.!verbatim\n.!verbatim\n",
            ".!raiv\n.!verbatim x\n",
        ] {
            match crate::denorm::denormalize(bad) {
                Err(PipelineError::App(e)) => {
                    assert_eq!(e.error, AppError::VerbatimContext)
                }
                other => panic!("{bad:?} -> {other:?}"),
            }
        }
    }

    #[test]
    fn map_keys_and_shebang() {
        // Non-bare map keys are quoted; a bare key stays bare.
        let d = build(".!kaiv 1\n!map<int>\np=api:1;my key:2\n");
        assert!(d.contains("!int'/p::api=1\n"), "{d}");
        assert!(d.contains("!int'/p::\"my key\"=2\n"), "{d}");
        // A first-line shebang is skipped, not a comment or error.
        assert!(build("#!/usr/bin/env kaiv\n.!kaiv 1\nx=1\n").contains("!str'::x=1\n"));
    }

    #[test]
    fn lone_dollar_and_ns_schema_are_loud() {
        // A trailing `$` forms no reference — must be written `$$`.
        assert!(crate::compile(b".!kaiv 1\nx=a$\n").is_err());
        // A block `schema:` annotation is the member selection
        // (D-10): the Compiler emits it as the scoped declaration —
        // the discriminant — in the header run.
        let r = crate::compile(b".!kaiv 1\n(/p schema:acme/x)\na=1\n()\n").unwrap();
        assert!(r.starts_with(".!raiv\n.!schema:/p acme/x\n"), "{r}");
        assert!(r.contains("!str'/p::a=1\n"), "{r}");
        // A data block names exactly ONE schema; a set is the parent
        // `.saiv`'s surface.
        assert!(crate::compile(b".!kaiv 1\n(/p schema:a/x|a/y)\na=1\n()\n").is_err());
    }

    fn app_err(input: &str) -> Option<AppError> {
        match crate::compile(input.as_bytes()) {
            Err(PipelineError::App(e)) => Some(e.error),
            _ => None,
        }
    }

    #[test]
    fn metadata_without_target() {
        let e = Some(AppError::MetadataWithoutTarget);
        assert_eq!(app_err(".!kaiv 1\n!int\n\nx=1\n"), e); // blank between
        assert_eq!(app_err(".!kaiv 1\n!int\n# c\nx=1\n"), e); // comment between
        assert_eq!(app_err(".!kaiv 1\n!int\n!str\nx=1\n"), e); // same-kind: two types
        assert_eq!(app_err(".!kaiv 1\n?a\n?b\nx=1\n"), e); // same-kind: two provs
        assert_eq!(app_err(".!kaiv 1\n!int\n"), e); // EOF
        assert_eq!(app_err(".!kaiv 1\n!int\nx=1\n"), None); // legal
        assert_eq!(app_err(".!kaiv 1\n!int?sensor1\nx=1\n"), None); // inline stack
    }

    fn relexes(raiv: &str) -> bool {
        crate::lexer::lex(raiv.as_bytes(), crate::lexer::FileKind::Data).is_ok()
    }

    #[test]
    fn two_line_metadata_stack_is_legal_and_merges() {
        // One type + one provenance, in either order, stack above a
        // content line and merge onto the canonical prefix.
        let a = crate::compile(b".!kaiv 1\n!int\n?sensor1\ntemp=100\n").unwrap();
        assert!(a.contains("!int?sensor1'::temp=100"), "{a}");
        let b = crate::compile(b".!kaiv 1\n?sensor1\n!int\ntemp=100\n").unwrap();
        assert!(b.contains("!int?sensor1'::temp=100"), "{b}");
    }

    #[test]
    fn struct_pair_keys_are_canonicalized_and_relex() {
        for input in [
            ".!kaiv 1\n/s:=a=1| b=2\n",
            ".!kaiv 1\n/server:=my host=a|port=1\n",
            ".!kaiv 1\n/s:=9bad=1\n",
        ] {
            let out = crate::compile(input.as_bytes()).unwrap();
            assert!(relexes(&out), "did not re-lex: {out}");
        }
        let out = crate::compile(b".!kaiv 1\n/s:=a=1| b=2\n").unwrap();
        assert!(out.contains("::\" b\"=2"), "{out}");
    }

    #[test]
    fn standalone_provenance_is_validated() {
        assert!(crate::compile(b".!kaiv 1\n?bad src\nx=1\n").is_err());
        assert!(crate::compile(b".!kaiv 1\n?src'oops\nx=1\n").is_err());
        assert!(crate::compile(b".!kaiv 1\n?a,b#=c\nx=1\n").is_err());
        // The canonical instant is the dashed extended form; the
        // compact one is accepted on input and canonicalized away
        // (SPEC.md § Provenance, deprecated until 1.0).
        let ok =
            crate::compile(b".!kaiv 1\n?sensor1@2025-01-15T09:30:00Z#req-42\ntemp=100\n").unwrap();
        assert!(
            ok.contains("?sensor1@2025-01-15T09:30:00Z#req-42'::temp=100"),
            "{ok}"
        );
        // The deprecated compact form parses and canonicalizes.
        let from_compact =
            crate::compile(b".!kaiv 1\n?sensor1@20250115T093000Z#req-42\ntemp=100\n").unwrap();
        assert_eq!(ok, from_compact, "compact must canonicalize to dashed");
        // A malformed instant in either shape is still rejected.
        assert!(crate::compile(b".!kaiv 1\n?s@2025-01-15T09:30:00\nx=1\n").is_err());
        assert!(crate::compile(b".!kaiv 1\n?s@20250115T0930Z\nx=1\n").is_err());
    }

    #[test]
    fn unquoted_apostrophe_key_rejected_quoted_ok() {
        assert!(crate::compile(b".!kaiv 1\nit's=5\n").is_err());
        assert!(crate::compile(
            b".!kaiv 1\n!int?sensor1@20250115T093000Z#req-42'/readings::temp=100\n"
        )
        .is_err());
        let ok = crate::compile(b".!kaiv 1\n\"it's\"=5\n").unwrap();
        assert!(ok.contains("::\"it's\"=5"), "{ok}");
        assert!(relexes(&ok));
    }

    #[test]
    fn colon_colon_in_path_position_rejected() {
        assert!(crate::compile(b".!kaiv 1\n/server::@tags+=x\n").is_err());
        assert!(crate::compile(b".!kaiv 1\na::b:=x=1\n").is_err());
        // A normal namepath (Key path) still works.
        let ok = crate::compile(b".!kaiv 1\n/server/api::port=1\n").unwrap();
        assert!(ok.contains("!str'/server/api::port=1"), "{ok}");
    }

    #[test]
    fn map_provenance_propagates_empty_key_rejected() {
        let ok = crate::compile(b".!kaiv 1\n!map<int>?sensor1\nsettings=x:1;y:2\n").unwrap();
        assert!(ok.contains("!int?sensor1'/settings::x=1"), "{ok}");
        assert!(crate::compile(b".!kaiv 1\n!map\nm=:v\n").is_err());
    }

    #[test]
    fn union_annotations_resolve_to_the_active_variant() {
        // A canonical metadata prefix carries no union (§10.6): the
        // Compiler picks the first alternative — head first, then left
        // to right — whose lowered definition the value satisfies.
        let out = crate::compile(b".!kaiv 1\n!int[1,5]|str\nx=3\n").unwrap();
        assert!(out.contains("!int[1,5]'::x=3"), "{out}");
        assert!(!out.contains('|'), "{out}");
        let s = crate::compile(b".!kaiv 1\n!int|str\nx=abc\n").unwrap();
        assert!(s.contains("!str'::x=abc"), "{s}");
        // The classic nullable field: empty payload picks null.
        let n = crate::compile(b".!kaiv 1\n!null|int[1,3600]\ntimeout=\n").unwrap();
        assert!(n.contains("!null'::timeout="), "{n}");
        let v = crate::compile(b".!kaiv 1\n!null|int[1,3600]\ntimeout=42\n").unwrap();
        assert!(v.contains("!int[1,3600]'::timeout=42"), "{v}");
        // Provenance rides onto the active variant.
        let p = crate::compile(b".!kaiv 1\n!null|int?src\ntemp=7\n").unwrap();
        assert!(p.contains("!int?src'::temp=7"), "{p}");
        // A span in a data annotation prefix is rejected.
        assert!(crate::compile(b".!kaiv 1\n&int ..num\nx=1\n").is_err());
    }

    #[test]
    fn union_variant_is_picked_per_spliced_element() {
        let out = crate::compile(b".!kaiv 1\n!null|int\n@xs;=1;;2\n").unwrap();
        assert!(out.contains("!int'/@xs::0=1"), "{out}");
        assert!(out.contains("!null'/@xs::1="), "{out}");
        assert!(out.contains("!int'/@xs::2=2"), "{out}");
    }

    #[test]
    fn union_with_no_matching_variant_is_a_type_mismatch() {
        assert_eq!(
            app_err(".!kaiv 1\n!null|int\nx=abc\n"),
            Some(AppError::TypeMismatch)
        );
        // Units ride alternatives on data lines too (D-11): the
        // active variant carries its own unit.
        let r = crate::compile(b".!kaiv 1\n!int:s|null\nt=1\n").unwrap();
        assert!(r.contains("!int:s'::t=1\n"), "{r}");
    }

    #[test]
    fn field_ref_in_section_block_resolves() {
        let raiv = crate::compile(b".!kaiv 1\n[/@servers]\nname=web1\nalias=$name\n[]\n").unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        assert!(daiv.contains("/@servers/0::alias=web1"), "{daiv}");
    }

    #[test]
    fn mid_value_at_sign_ends_a_field_reference() {
        // `@` is a step marker only at token start or after `/`;
        // mid-token it is adjacent literal text.
        let raiv = crate::compile(b".!kaiv 1\nuser=alice\nemail=$user@example.com\n").unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        assert!(daiv.contains("::email=alice@example.com"), "{daiv}");
    }

    #[test]
    fn quoted_pair_and_map_keys_are_not_requoted() {
        let out = crate::compile(b".!kaiv 1\n/s:=\"my key\"=1\n").unwrap();
        assert!(out.contains("'/s::\"my key\"=1"), "{out}");
        assert!(relexes(&out));
        let arr = crate::compile(b".!kaiv 1\n/s:=@\"my tag\"+=x\n").unwrap();
        assert!(arr.contains("@\"my tag\"::0=x"), "{arr}");
        assert!(relexes(&arr));
        let map = crate::compile(b".!kaiv 1\n!map\nm=\"my key\":1\n").unwrap();
        assert!(map.contains("'/m::\"my key\"=1"), "{map}");
        assert!(relexes(&map));
    }

    #[test]
    fn second_provenance_of_any_spelling_is_same_kind() {
        // Inline `!type?prov` occupies the provenance slot: a stacked
        // standalone `?` line (either order) is a same-kind duplicate —
        // audit data must never be silently replaced.
        let e = Some(AppError::MetadataWithoutTarget);
        assert_eq!(app_err(".!kaiv 1\n!int?src\n?src2\nx=1\n"), e);
        assert_eq!(app_err(".!kaiv 1\n?src2\n!int?src\nx=1\n"), e);
    }

    #[test]
    fn quoted_block_opener_segments_survive() {
        let arr = crate::compile(b".!kaiv 1\n[/@\"my arr\"]\nhost=a\n[]\n").unwrap();
        assert!(arr.contains("'/@\"my arr\"/0::host=a"), "{arr}");
        assert!(relexes(&arr));
        let ns = crate::compile(b".!kaiv 1\n(/\"my ns\")\nx=1\n()\n").unwrap();
        assert!(ns.contains("'/\"my ns\"::x=1"), "{ns}");
        assert!(relexes(&ns));
    }

    #[test]
    fn map_annotation_rejects_unit_and_constraints() {
        assert!(crate::compile(b".!kaiv 1\n!map<int>:km\nm=x:1\n").is_err());
        assert!(crate::compile(b".!kaiv 1\n!map<int>[1,5]\nm=x:1\n").is_err());
        // The empty-map form still validates its path.
        assert!(crate::compile(b".!kaiv 1\n!map\na::b={}\n").is_err());
        assert!(crate::compile(b".!kaiv 1\n!map\nm={}\n").is_ok());
    }
}
