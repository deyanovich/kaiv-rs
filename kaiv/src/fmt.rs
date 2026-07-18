//! `kaiv fmt` — the standard formatter for authoring files.
//!
//! Two entry points, one style. [`format_data`] normalizes an
//! authored `.kaiv` stream into the standard style; [`lift`] renders
//! a canonical `.raiv`/`.daiv` stream *as* idiomatic authored kaiv
//! (the human view of a machine artifact). Both share one emitter,
//! so the pretty form is defined once.
//!
//! The standard style, in brief:
//!
//! - declarations first (format declaration spelled bare), one blank
//!   line before the body;
//! - a struct with one field renders as a flat namepath line, a
//!   small all-string struct as an inline assignment
//!   (`/path:=a=1|b=2`), anything larger or typed as a `(...)`
//!   block;
//! - an array run renders uniformly: all elements inline
//!   (`/@arr+:=...`) when every element allows it, else all in
//!   `[...]` blocks;
//! - one blank line separates block-shaped groups; single-line
//!   groups pack tight; authored blank lines survive as group
//!   separators (they are grouping hints and force the block form
//!   when they fall inside a group).
//!
//! Semantics are never touched: values are preserved byte for byte,
//! fields are never reordered, and any construct the restructurer
//! does not model (variables, splats, scalar-array appends, quoted
//! path segments) passes through verbatim — for a block, the whole
//! block. `compile(format_data(x)) == compile(x)` holds for every
//! well-formed input.

use crate::error::{LexErrorAt, PipelineError};
use crate::lexer::{lex, FileKind, LineKind};

/// Inline forms must fit within this width to be chosen.
const WIDTH: usize = 72;

// ── the shared document model ───────────────────────────────────

/// One modeled field: an absolute interior path (array indices as
/// numeric steps), the field name, pending metadata lines in
/// authored spelling, and the verbatim value.
#[derive(Debug, Clone)]
struct Field {
    path: Vec<String>,
    name: String,
    metas: Vec<String>,
    value: String,
}

#[derive(Debug, Clone)]
enum Node {
    /// A verbatim (whitespace-normalized) line: declaration,
    /// variable definition, opaque-block line, fallback content.
    Raw(String),
    /// A `#`/`//` comment, normalized spacing, attaches to what
    /// follows.
    Comment(String),
    /// One or more authored blank lines (collapsed): a grouping
    /// hint.
    Gap,
    Field(Field),
}

// ── small shared helpers ────────────────────────────────────────

fn is_bare_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..].iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn is_index(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// `#comment` → `# comment`; interior alignment is preserved.
fn norm_comment(prefix: &str, body: &str) -> String {
    let body = body.trim_end();
    if body.is_empty() {
        prefix.to_string()
    } else if body.starts_with([' ', '\t']) {
        format!("{prefix}{body}")
    } else {
        format!("{prefix} {body}")
    }
}

/// Normalize a declaration line: single spaces between tokens, and
/// the format declaration of `kind` spelled bare when it names
/// version 1 (`.!kaiv 1` → `.!kaiv`).
fn norm_decl(s: &str, kind: &str) -> String {
    let toks: Vec<&str> = s.split_ascii_whitespace().collect();
    if toks.len() == 2 && toks[0] == format!(".!{kind}") && toks[1] == "1" {
        return toks[0].to_string();
    }
    toks.join(" ")
}

fn render_path(steps: &[String]) -> String {
    let mut out = String::new();
    for s in steps {
        out.push('/');
        out.push_str(s);
    }
    out
}

/// First unquoted `'` (canonical metadata delimiter); quoted names
/// use `""` doubling, never affecting `'`.
fn unquoted_tick(s: &str) -> Option<usize> {
    let b = s.as_bytes();
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
            b'\'' if !in_quote => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

// ── the authored front-end ──────────────────────────────────────

/// Mirror of the compiler's block bookkeeping, enough to compute
/// each field's absolute path and array indices.
struct Tracker {
    blocks: Vec<Blk>,
    counters: std::collections::HashMap<String, usize>,
}

enum Blk {
    Ns { steps: Vec<String> },
    Arr { key: String, steps: Vec<String> },
}

impl Blk {
    fn steps(&self) -> &[String] {
        match self {
            Blk::Ns { steps } | Blk::Arr { steps, .. } => steps,
        }
    }
}

impl Tracker {
    fn new() -> Self {
        Tracker {
            blocks: Vec::new(),
            counters: std::collections::HashMap::new(),
        }
    }

    fn prefix(&self) -> Vec<String> {
        self.blocks.last().map(|b| b.steps().to_vec()).unwrap_or_default()
    }

    fn next_index(&mut self, key: &str) -> usize {
        let c = self.counters.entry(key.to_string()).or_insert(0);
        let i = *c;
        *c += 1;
        i
    }

    /// `[path]` opener; mirrors the compiler's continuation rule
    /// (a repeated opener for the same array continues elements).
    fn section_open(&mut self, head: &str) -> Option<()> {
        if let Some(Blk::Arr { key, .. }) = self.blocks.last() {
            let key = key.clone();
            self.blocks.pop();
            let mut base = self.prefix();
            base.extend(simple_steps(head)?);
            if render_path(&base) == key {
                let idx = self.next_index(&key);
                let mut steps = base;
                steps.push(idx.to_string());
                self.blocks.push(Blk::Arr { key, steps });
                return Some(());
            }
            let key2 = render_path(&base);
            let idx = self.next_index(&key2);
            let mut steps = base;
            steps.push(idx.to_string());
            self.blocks.push(Blk::Arr { key: key2, steps });
            return Some(());
        }
        let mut base = self.prefix();
        base.extend(simple_steps(head)?);
        let key = render_path(&base);
        let idx = self.next_index(&key);
        let mut steps = base;
        steps.push(idx.to_string());
        self.blocks.push(Blk::Arr { key, steps });
        Some(())
    }

    fn ns_open(&mut self, head: &str) -> Option<()> {
        let mut steps = self.prefix();
        steps.extend(simple_steps(head)?);
        self.blocks.push(Blk::Ns { steps });
        Some(())
    }
}

/// Path steps of a simple (unquoted) path token; `None` sends the
/// construct down the verbatim road.
fn simple_steps(path: &str) -> Option<Vec<String>> {
    if path.contains('"') {
        return None;
    }
    let p = path.strip_prefix('/').unwrap_or(path);
    if p.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for seg in p.split('/') {
        let name = seg.strip_prefix('@').unwrap_or(seg);
        if !(is_bare_name(name) || is_index(seg)) {
            return None;
        }
        out.push(seg.to_string());
    }
    Some(out)
}

/// What a top-level content line's left side is.
enum LeftKind<'a> {
    /// `name` — a bare field in the current scope.
    Bare(&'a str),
    /// `/a/b::field` — flat namepath assignment.
    Namepath { interior: &'a str, field: &'a str },
    /// `/path:` — struct assignment (`:=`).
    StructAssign(&'a str),
    /// `/@arr+:` — array-element assignment (`+:=`).
    ElemAssign(&'a str),
    /// Anything else: variables, scalar-array appends, quoted
    /// names, ns-var assigns — kept verbatim.
    Other,
}

fn classify_left(left: &str) -> LeftKind<'_> {
    if left.contains('"') {
        return LeftKind::Other;
    }
    if is_bare_name(left) {
        return LeftKind::Bare(left);
    }
    if left.starts_with('.') || left.starts_with("@.") || left.starts_with("/.") {
        return LeftKind::Other; // variable definitions
    }
    if let Some(p) = left.strip_suffix("+:") {
        return LeftKind::ElemAssign(p);
    }
    if let Some(p) = left.strip_suffix(':') {
        if p.ends_with(':') {
            return LeftKind::Other; // stray `::` end — not ours
        }
        return LeftKind::StructAssign(p);
    }
    if left.ends_with('+') || left.ends_with(';') {
        return LeftKind::Other; // scalar-array appends
    }
    if let Some(i) = left.rfind("::") {
        return LeftKind::Namepath {
            interior: &left[..i],
            field: &left[i + 2..],
        };
    }
    LeftKind::Other
}

/// Split an inline `:=`/`+:=` right side into `(name, value)` pairs.
/// `None` (a pair without `=`, a splice, a quoted name) keeps the
/// line verbatim.
fn split_pairs(value: &str) -> Option<Vec<(String, String)>> {
    if value.starts_with('$') || value == "{}" {
        return None; // ns-var splice / empty-struct form
    }
    let mut out = Vec::new();
    for pair in value.split('|') {
        let (k, v) = pair.split_once('=')?;
        // A `$` in a value is a reference (or an escape); references
        // resolve against the enclosing scope, so such lines are
        // never restructured.
        if !is_bare_name(k) || v.contains('$') {
            return None;
        }
        out.push((k.to_string(), v.to_string()));
    }
    Some(out)
}

/// Parse an authored `.kaiv` stream into the document model.
fn parse_authored(input: &str) -> Result<(Option<String>, Vec<Node>), PipelineError> {
    let lines = lex(input.as_bytes(), FileKind::Data).map_err(PipelineError::Lex)?;
    let shebang = input
        .lines()
        .next()
        .filter(|l| l.starts_with("#!"))
        .map(|l| l.trim_end().to_string());

    let mut tracker = Tracker::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut metas: Vec<String> = Vec::new();

    // Pre-compute block regions at depth zero and whether each is
    // modelable; region opacity is decided before emission so a
    // block either dissolves fully or survives verbatim.
    let mut i = 0;
    while i < lines.len() {
        let kind = &lines[i].kind;
        match kind {
            LineKind::Blank => {
                if !matches!(nodes.last(), Some(Node::Gap) | None) {
                    nodes.push(Node::Gap);
                }
                i += 1;
            }
            LineKind::Comment(c) => {
                nodes.push(Node::Comment(norm_comment("#", c)));
                i += 1;
            }
            LineKind::Doc(c) => {
                nodes.push(Node::Comment(norm_comment("//", c)));
                i += 1;
            }
            LineKind::Decl(s) => {
                flush_metas(&mut nodes, &mut metas);
                nodes.push(Node::Raw(norm_decl(s, "kaiv")));
                i += 1;
            }
            LineKind::Meta(s) => {
                metas.push(s.trim_end().to_string());
                i += 1;
            }
            LineKind::VarSplat(name) => {
                flush_metas(&mut nodes, &mut metas);
                nodes.push(Node::Raw(format!("$/.{name}")));
                i += 1;
            }
            LineKind::SectionOpen(_) | LineKind::NsOpen(_) => {
                flush_metas(&mut nodes, &mut metas);
                let end = region_end(&lines, i);
                if region_modelable(&lines[i..end]) {
                    model_region(&lines[i..end], &mut tracker, &mut nodes)?;
                } else {
                    for l in &lines[i..end] {
                        raw_line(l, &mut nodes);
                    }
                    // The verbatim block still consumes array
                    // indices; replay its openers through the
                    // tracker so later elements number correctly.
                    replay_region(&lines[i..end], &mut tracker);
                }
                i = end;
            }
            LineKind::SectionClose | LineKind::NsClose => {
                // A stray close at depth zero: keep it verbatim.
                raw_line(&lines[i], &mut nodes);
                i += 1;
            }
            LineKind::Content { left, value } => {
                content_line(left, value, &mut tracker, &mut nodes, &mut metas);
                i += 1;
            }
        }
    }
    flush_metas(&mut nodes, &mut metas);
    Ok((shebang, nodes))
}

fn flush_metas(nodes: &mut Vec<Node>, metas: &mut Vec<String>) {
    for m in metas.drain(..) {
        nodes.push(Node::Raw(m));
    }
}

fn content_line(
    left: &str,
    value: &str,
    tracker: &mut Tracker,
    nodes: &mut Vec<Node>,
    metas: &mut Vec<String>,
) {
    let in_block = !tracker.blocks.is_empty();
    match classify_left(left) {
        LeftKind::Bare(name) => nodes.push(Node::Field(Field {
            path: tracker.prefix(),
            name: name.to_string(),
            metas: std::mem::take(metas),
            value: value.to_string(),
        })),
        LeftKind::Namepath { interior, field }
            if !in_block && is_bare_name(field) && !value.contains('$') =>
        {
            match simple_steps(interior) {
                Some(path) => nodes.push(Node::Field(Field {
                    path,
                    name: field.to_string(),
                    metas: std::mem::take(metas),
                    value: value.to_string(),
                })),
                None => fallback(left, value, nodes, metas),
            }
        }
        LeftKind::StructAssign(p) if !in_block && metas.is_empty() => {
            match (simple_steps(p), split_pairs(value)) {
                (Some(path), Some(pairs)) if p.starts_with('/') => {
                    for (k, v) in pairs {
                        nodes.push(Node::Field(Field {
                            path: path.clone(),
                            name: k,
                            metas: Vec::new(),
                            value: v,
                        }));
                    }
                }
                _ => fallback(left, value, nodes, metas),
            }
        }
        LeftKind::ElemAssign(p) if !in_block && metas.is_empty() => {
            match (simple_steps(p), split_pairs(value)) {
                (Some(head), Some(pairs))
                    if p.starts_with('/')
                        && head.last().is_some_and(|s| s.starts_with('@')) =>
                {
                    let key = render_path(&head);
                    let idx = tracker.next_index(&key);
                    let mut path = head;
                    path.push(idx.to_string());
                    for (k, v) in pairs {
                        nodes.push(Node::Field(Field {
                            path: path.clone(),
                            name: k,
                            metas: Vec::new(),
                            value: v,
                        }));
                    }
                }
                _ => fallback(left, value, nodes, metas),
            }
        }
        _ => fallback(left, value, nodes, metas),
    }
}

fn fallback(left: &str, value: &str, nodes: &mut Vec<Node>, metas: &mut Vec<String>) {
    flush_metas(nodes, metas);
    nodes.push(Node::Raw(format!("{left}={value}")));
}

/// Find the end (exclusive) of the block region opened at `start`:
/// the index after the line where the block stack returns to empty.
fn region_end(lines: &[crate::lexer::Line<'_>], start: usize) -> usize {
    let mut stack: Vec<bool> = Vec::new(); // true = array block
    let mut i = start;
    while i < lines.len() {
        match &lines[i].kind {
            LineKind::SectionOpen(_) => {
                if stack.last() == Some(&true) {
                    // continuation or sibling: replaces the top
                } else {
                    stack.push(true);
                }
            }
            LineKind::SectionClose => {
                if stack.last() == Some(&true) {
                    stack.pop();
                }
            }
            LineKind::NsOpen(_) => stack.push(false),
            LineKind::NsClose => {
                if stack.last() == Some(&false) {
                    stack.pop();
                }
            }
            _ => {}
        }
        i += 1;
        if stack.is_empty() {
            break;
        }
    }
    i
}

/// A region dissolves into fields iff every line in it is plain:
/// bare-name content, metadata, comments, blanks, and nested simple
/// block openers/closers. Anything else keeps the block verbatim.
fn region_modelable(region: &[crate::lexer::Line<'_>]) -> bool {
    region.iter().all(|l| match &l.kind {
        LineKind::Blank | LineKind::Comment(_) | LineKind::Doc(_) | LineKind::Meta(_) => true,
        LineKind::SectionClose | LineKind::NsClose => true,
        LineKind::SectionOpen(inner) | LineKind::NsOpen(inner) => {
            let toks = crate::table::tokens(inner);
            toks.len() == 1 && simple_steps(toks[0]).is_some()
        }
        LineKind::Content { left, value } => {
            matches!(classify_left(left), LeftKind::Bare(_)) && !value.contains('$')
        }
        _ => false,
    })
}

fn model_region(
    region: &[crate::lexer::Line<'_>],
    tracker: &mut Tracker,
    nodes: &mut Vec<Node>,
) -> Result<(), PipelineError> {
    let mut metas: Vec<String> = Vec::new();
    for l in region {
        match &l.kind {
            LineKind::Blank => {
                if !matches!(nodes.last(), Some(Node::Gap) | None) {
                    nodes.push(Node::Gap);
                }
            }
            LineKind::Comment(c) => nodes.push(Node::Comment(norm_comment("#", c))),
            LineKind::Doc(c) => nodes.push(Node::Comment(norm_comment("//", c))),
            LineKind::Meta(s) => metas.push(s.trim_end().to_string()),
            LineKind::SectionOpen(inner) => {
                let head = crate::table::tokens(inner)[0];
                tracker
                    .section_open(head)
                    .ok_or_else(|| PipelineError::Other(format!("unmodelable path: {head}")))?;
            }
            LineKind::NsOpen(inner) => {
                let head = crate::table::tokens(inner)[0];
                tracker
                    .ns_open(head)
                    .ok_or_else(|| PipelineError::Other(format!("unmodelable path: {head}")))?;
            }
            LineKind::SectionClose => {
                if matches!(tracker.blocks.last(), Some(Blk::Arr { .. })) {
                    tracker.blocks.pop();
                }
            }
            LineKind::NsClose => {
                if matches!(tracker.blocks.last(), Some(Blk::Ns { .. })) {
                    tracker.blocks.pop();
                }
            }
            LineKind::Content { left, value } => {
                if let LeftKind::Bare(name) = classify_left(left) {
                    nodes.push(Node::Field(Field {
                        path: tracker.prefix(),
                        name: name.to_string(),
                        metas: std::mem::take(&mut metas),
                        value: value.to_string(),
                    }));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Reconstruct one lexed line verbatim (whitespace-normalized).
fn raw_line(l: &crate::lexer::Line<'_>, nodes: &mut Vec<Node>) {
    match &l.kind {
        LineKind::Blank => {
            if !matches!(nodes.last(), Some(Node::Gap) | None) {
                nodes.push(Node::Gap);
            }
        }
        LineKind::Comment(c) => nodes.push(Node::Comment(norm_comment("#", c))),
        LineKind::Doc(c) => nodes.push(Node::Comment(norm_comment("//", c))),
        LineKind::Decl(s) => nodes.push(Node::Raw(norm_decl(s, "kaiv"))),
        LineKind::Meta(s) => nodes.push(Node::Raw(s.trim_end().to_string())),
        LineKind::SectionOpen(inner) => {
            nodes.push(Node::Raw(format!("[{}]", crate::table::tokens(inner).join(" "))))
        }
        LineKind::SectionClose => nodes.push(Node::Raw("[]".into())),
        LineKind::NsOpen(inner) => {
            nodes.push(Node::Raw(format!("({})", crate::table::tokens(inner).join(" "))))
        }
        LineKind::NsClose => nodes.push(Node::Raw("()".into())),
        LineKind::Content { left, value } => nodes.push(Node::Raw(format!("{left}={value}"))),
        LineKind::VarSplat(name) => nodes.push(Node::Raw(format!("$/.{name}"))),
    }
}

/// Replay a verbatim region's array openers so file-wide element
/// counters stay aligned with the compiler's.
fn replay_region(region: &[crate::lexer::Line<'_>], tracker: &mut Tracker) {
    for l in region {
        match &l.kind {
            LineKind::SectionOpen(inner) => {
                let toks = crate::table::tokens(inner);
                if let Some(head) = toks.first() {
                    let _ = tracker.section_open(head);
                }
            }
            LineKind::NsOpen(inner) => {
                let toks = crate::table::tokens(inner);
                if let Some(head) = toks.first() {
                    let _ = tracker.ns_open(head);
                }
            }
            LineKind::SectionClose => {
                if matches!(tracker.blocks.last(), Some(Blk::Arr { .. })) {
                    tracker.blocks.pop();
                }
            }
            LineKind::NsClose => {
                if matches!(tracker.blocks.last(), Some(Blk::Ns { .. })) {
                    tracker.blocks.pop();
                }
            }
            _ => {}
        }
    }
}

// ── the canonical front-end (lift) ──────────────────────────────

/// Parse a canonical `.raiv`/`.daiv` stream into the model. The
/// format declaration becomes `.!kaiv`; metadata prefixes become
/// authored annotation lines (`str` disappears — it is the
/// default); everything unparseable passes through verbatim.
fn parse_canonical(input: &str, is_daiv: bool) -> Result<Vec<Node>, PipelineError> {
    let mut nodes: Vec<Node> = Vec::new();
    for raw in input.lines() {
        // Trailing whitespace on a canonical line is value bytes —
        // only the (insignificant) leading side is trimmed.
        let s = raw.strip_suffix('\r').unwrap_or(raw).trim_start();
        if s.is_empty() {
            if !matches!(nodes.last(), Some(Node::Gap) | None) {
                nodes.push(Node::Gap);
            }
            continue;
        }
        if s.starts_with("#!") && nodes.is_empty() {
            continue; // re-added by the caller if wanted; canonical shebangs stay out
        }
        if s == ".!daiv" || s == ".!raiv" {
            nodes.push(Node::Raw(".!kaiv".into()));
            continue;
        }
        if s.starts_with(".!") || s.starts_with(".?") {
            nodes.push(Node::Raw(norm_decl(s, "kaiv")));
            continue;
        }
        // `[!meta']namepath=value`
        let (metas, rest) = if s.starts_with('!') {
            match unquoted_tick(s) {
                Some(t) => {
                    let body = &s[1..t];
                    let m = if body == "str" {
                        Vec::new()
                    } else if let Some(p) = body.strip_prefix("str?") {
                        vec![format!("?{p}")]
                    } else {
                        vec![format!("!{body}")]
                    };
                    (m, &s[t + 1..])
                }
                None => {
                    nodes.push(Node::Raw(s.to_string()));
                    continue;
                }
            }
        } else {
            (Vec::new(), s)
        };
        let Some(eq) = rest.find('=') else {
            nodes.push(Node::Raw(s.to_string()));
            continue;
        };
        let (left, mut value) = (&rest[..eq], rest[eq + 1..].to_string());
        if is_daiv {
            // .daiv values are fully resolved: every `$` is a
            // literal and re-authors as the `$$` escape.
            value = value.replace('$', "$$");
        } else if value.contains('$') {
            // A preserved reference (.raiv) or a dollar escape:
            // scope-sensitive, so the line stays flat at root scope
            // (annotation lines above, the tick-free namepath form).
            for m in metas {
                nodes.push(Node::Raw(m));
            }
            nodes.push(Node::Raw(rest.to_string()));
            continue;
        }
        let value_ref = &value;
        let parsed = left.rfind("::").and_then(|i| {
            let (interior, field) = (&left[..i], &left[i + 2..]);
            if interior.contains('"') {
                return None;
            }
            let path = simple_steps(interior)?;
            if is_bare_name(field) {
                Some((path, field))
            } else if is_index(field) && path.last().is_some_and(|s| s.starts_with('@')) {
                // A scalar-array element: the index is positional,
                // re-authored as an append (name "" marks it).
                Some((path, ""))
            } else {
                None
            }
        });
        match parsed {
            Some((path, field)) => nodes.push(Node::Field(Field {
                path,
                name: field.to_string(),
                metas,
                value,
            })),
            None => {
                // Fall back to the flat namepath form — authored-
                // legal, unlike the tick-delimited canonical line.
                for m in metas {
                    nodes.push(Node::Raw(m));
                }
                nodes.push(Node::Raw(format!("{left}={value_ref}")));
            }
        }
    }
    Ok(nodes)
}

// ── the emitter ─────────────────────────────────────────────────

/// A group of consecutive fields sharing a destination, plus its
/// interior comments/gaps (which force the block form).
struct Group {
    /// Array head (`.../@name`) for element groups, else the full
    /// struct path (empty = top level).
    items: Vec<GItem>,
    kind: GroupKind,
}

enum GItem {
    Field(Field),
    Comment(String),
    Gap,
}

#[derive(PartialEq)]
enum GroupKind {
    /// Top-level bare fields.
    Top,
    /// One struct path.
    Struct(Vec<String>),
    /// One array: head path (ending in `@name`).
    Array(Vec<String>),
    /// A scalar array: elements are positional appends.
    ScalarArray(Vec<String>),
}

/// The destination of a field, for grouping.
fn destination(f: &Field) -> GroupKind {
    if f.name.is_empty() {
        return GroupKind::ScalarArray(f.path.clone());
    }
    if f.path.is_empty() {
        return GroupKind::Top;
    }
    // An array element: head/.../@name/idx[/deeper...]. Group by
    // the OUTERMOST array; more than one index level falls back to
    // flat lines via `Struct` with the raw path (handled at
    // render time).
    let idx_positions: Vec<usize> = f
        .path
        .iter()
        .enumerate()
        .filter(|(_, s)| is_index(s))
        .map(|(i, _)| i)
        .collect();
    match idx_positions.as_slice() {
        [] => GroupKind::Struct(f.path.clone()),
        [i] if *i > 0 && f.path[i - 1].starts_with('@') => {
            GroupKind::Array(f.path[..*i].to_vec())
        }
        _ => GroupKind::Struct(f.path.clone()), // nested arrays: flat fallback
    }
}

fn render(shebang: Option<String>, nodes: Vec<Node>) -> String {
    // Split into units: Raw lines, and Groups of fields with their
    // attached interior comments/gaps.
    enum Unit {
        Raw(String),
        Comment(String),
        Gap,
        Group(Group),
    }
    let mut units: Vec<Unit> = Vec::new();
    let mut it = nodes.into_iter().peekable();
    while let Some(n) = it.next() {
        match n {
            Node::Raw(s) => units.push(Unit::Raw(s)),
            Node::Comment(c) => units.push(Unit::Comment(c)),
            Node::Gap => units.push(Unit::Gap),
            Node::Field(f) => {
                let kind = destination(&f);
                let mut g = Group {
                    items: vec![GItem::Field(f)],
                    kind,
                };
                // Absorb following fields of the same destination,
                // and comments/gaps that sit between such fields.
                loop {
                    match it.peek() {
                        Some(Node::Field(nf)) => {
                            let nk = destination(nf);
                            let same = match (&g.kind, &nk) {
                                (GroupKind::Top, GroupKind::Top) => true,
                                (GroupKind::Struct(a), GroupKind::Struct(b)) => a == b,
                                (GroupKind::Array(a), GroupKind::Array(b)) => a == b,
                                (GroupKind::ScalarArray(a), GroupKind::ScalarArray(b)) => a == b,
                                _ => false,
                            };
                            if !same {
                                break;
                            }
                            let Some(Node::Field(f)) = it.next() else { unreachable!() };
                            g.items.push(GItem::Field(f));
                        }
                        Some(Node::Comment(_) | Node::Gap) => {
                            // Only absorb if a same-destination field
                            // follows; otherwise the comment/gap
                            // belongs between groups.
                            let mut ahead = it.clone();
                            let mut absorbable = false;
                            while let Some(x) = ahead.next() {
                                match x {
                                    Node::Comment(_) | Node::Gap => continue,
                                    Node::Field(nf) => {
                                        let nk = destination(&nf);
                                        absorbable = match (&g.kind, &nk) {
                                            (GroupKind::Top, GroupKind::Top) => true,
                                            (GroupKind::Struct(a), GroupKind::Struct(b)) => a == b,
                                            (GroupKind::Array(a), GroupKind::Array(b)) => a == b,
                                            _ => false,
                                        };
                                        break;
                                    }
                                    _ => break,
                                }
                            }
                            if !absorbable {
                                break;
                            }
                            match it.next() {
                                Some(Node::Comment(c)) => g.items.push(GItem::Comment(c)),
                                Some(Node::Gap) => g.items.push(GItem::Gap),
                                _ => unreachable!(),
                            }
                        }
                        _ => break,
                    }
                }
                units.push(Unit::Group(g));
            }
        }
    }

    // Render units with the blank-line policy: authored gaps are
    // honored (collapsed to one); a blank also separates any
    // block-shaped rendering from its neighbors; declarations get
    // one blank after the run; single-line units pack tight.
    let mut out: Vec<String> = Vec::new();
    if let Some(sb) = shebang {
        out.push(sb);
    }
    let mut prev_block = false; // previous unit rendered block-shaped
    let mut prev_decl = false;
    let mut prev_comment = false; // comments glue to what follows
    let mut pending_gap = false;
    let mut started = false;
    for u in &units {
        match u {
            Unit::Gap => pending_gap = true,
            Unit::Raw(s) => {
                let is_decl = s.starts_with(".!") || s.starts_with(".?");
                if started
                    && !prev_comment
                    && (pending_gap || prev_block || (prev_decl && !is_decl))
                {
                    out.push(String::new());
                }
                out.push(s.clone());
                prev_block = false;
                prev_decl = is_decl;
                prev_comment = false;
                pending_gap = false;
                started = true;
            }
            Unit::Comment(c) => {
                if started && !prev_comment && (pending_gap || prev_block || prev_decl) {
                    out.push(String::new());
                }
                out.push(c.clone());
                prev_block = false;
                prev_decl = false;
                prev_comment = true;
                pending_gap = false;
                started = true;
            }
            Unit::Group(g) => {
                let (lines, blocky) = render_group(g);
                if started && !prev_comment && (pending_gap || prev_block || prev_decl || blocky)
                {
                    out.push(String::new());
                }
                out.extend(lines);
                prev_block = blocky;
                prev_decl = false;
                prev_comment = false;
                pending_gap = false;
                started = true;
            }
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

fn field_lines(f: &Field, key: &str) -> Vec<String> {
    let mut out = f.metas.clone();
    out.push(format!("{key}={}", f.value));
    out
}

/// Can this set of fields render as one inline `|`-joined line?
fn inline_ok(fields: &[&Field]) -> bool {
    fields.iter().all(|f| {
        f.metas.is_empty()
            && is_bare_name(&f.name)
            && !f.value.contains('|')
            && !f.value.starts_with([' ', '\t'])
            && !f.value.ends_with([' ', '\t'])
            && f.value != "{}"
    })
}

fn inline_join(path: &str, op: &str, fields: &[&Field]) -> String {
    let pairs: Vec<String> = fields
        .iter()
        .map(|f| format!("{}={}", f.name, f.value))
        .collect();
    format!("{path}{op}{}", pairs.join("|"))
}

/// Render one group; the second value is whether the rendering is
/// block-shaped (and so wants blank lines around it). Flat runs —
/// top-level fields, a single namepath line, one inline struct —
/// are not.
fn render_group(g: &Group) -> (Vec<String>, bool) {
    let has_interior = g
        .items
        .iter()
        .any(|i| matches!(i, GItem::Comment(_) | GItem::Gap));
    let fields: Vec<&Field> = g
        .items
        .iter()
        .filter_map(|i| match i {
            GItem::Field(f) => Some(f),
            _ => None,
        })
        .collect();
    match &g.kind {
        GroupKind::Top => {
            let mut out = Vec::new();
            for item in &g.items {
                match item {
                    GItem::Field(f) => out.extend(field_lines(f, &f.name)),
                    GItem::Comment(c) => out.push(c.clone()),
                    GItem::Gap => out.push(String::new()),
                }
            }
            (out, false)
        }
        GroupKind::Struct(path) => {
            let p = render_path(path);
            // Deep/odd paths (an index without an array marker,
            // nested arrays) render flat and are never grouped.
            let odd = path.iter().any(|s| is_index(s));
            if odd {
                let mut out = Vec::new();
                for item in &g.items {
                    match item {
                        GItem::Field(f) => out.extend(field_lines(f, &format!("{p}::{}", f.name))),
                        GItem::Comment(c) => out.push(c.clone()),
                        GItem::Gap => out.push(String::new()),
                    }
                }
                return (out, false);
            }
            if fields.len() == 1 && !has_interior {
                let f = fields[0];
                return (field_lines(f, &format!("{p}::{}", f.name)), false);
            }
            if !has_interior && inline_ok(&fields) {
                let line = inline_join(&p, ":=", &fields);
                if line.len() <= WIDTH {
                    return (vec![line], false);
                }
            }
            let mut out = vec![format!("({p})")];
            for item in &g.items {
                match item {
                    GItem::Field(f) => out.extend(field_lines(f, &f.name)),
                    GItem::Comment(c) => out.push(c.clone()),
                    GItem::Gap => out.push(String::new()),
                }
            }
            out.push("()".into());
            (out, true)
        }
        GroupKind::ScalarArray(head) => {
            let hp = render_path(head);
            // The joined `;=` form when it reads clean: no
            // metadata, no interior comments, no `;` inside a
            // value, everything on one line within the width.
            let joinable = !has_interior
                && fields.len() > 1
                && fields.iter().all(|f| {
                    f.metas.is_empty()
                        && !f.value.contains(';')
                        && !f.value.is_empty()
                        && !f.value.starts_with([' ', '\t'])
                        && !f.value.ends_with([' ', '\t'])
                });
            if joinable {
                let vals: Vec<&str> = fields.iter().map(|f| f.value.as_str()).collect();
                let line = format!("{hp};={}", vals.join(";"));
                if line.len() <= WIDTH {
                    return (vec![line], false);
                }
            }
            let mut out = Vec::new();
            for item in &g.items {
                match item {
                    GItem::Field(f) => {
                        out.extend(f.metas.iter().cloned());
                        out.push(format!("{hp}+={}", f.value));
                    }
                    GItem::Comment(c) => out.push(c.clone()),
                    GItem::Gap => out.push(String::new()),
                }
            }
            let blocky = out.len() > 1;
            (out, blocky)
        }
        GroupKind::Array(head) => {
            let hp = render_path(head);
            // Partition items into elements by index step.
            struct Elem<'a> {
                idx: String,
                items: Vec<&'a GItem>,
            }
            let mut elems: Vec<Elem<'_>> = Vec::new();
            for item in &g.items {
                match item {
                    GItem::Field(f) => {
                        let idx = f.path[head.len()].clone();
                        match elems.last_mut() {
                            Some(e) if e.idx == idx => e.items.push(item),
                            _ => elems.push(Elem {
                                idx,
                                items: vec![item],
                            }),
                        }
                    }
                    other => {
                        if let Some(e) = elems.last_mut() {
                            e.items.push(other);
                        }
                        // A leading comment/gap before any element
                        // was already split off by the grouper.
                    }
                }
            }
            // Inline only when EVERY element allows it — uniform
            // treatment reads best.
            let all_inline = elems.iter().all(|e| {
                let fs: Vec<&Field> = e
                    .items
                    .iter()
                    .filter_map(|i| match i {
                        GItem::Field(f) => Some(f),
                        _ => None,
                    })
                    .collect();
                e.items.iter().all(|i| matches!(i, GItem::Field(_)))
                    && fs.iter().all(|f| f.path.len() == head.len() + 1)
                    && inline_ok(&fs)
                    && inline_join(&hp, "+:=", &fs).len() <= WIDTH
            });
            if all_inline {
                let lines: Vec<String> = elems
                    .iter()
                    .map(|e| {
                        let fs: Vec<&Field> = e
                            .items
                            .iter()
                            .filter_map(|i| match i {
                                GItem::Field(f) => Some(f),
                                _ => None,
                            })
                            .collect();
                        inline_join(&hp, "+:=", &fs)
                    })
                    .collect();
                let blocky = lines.len() > 1;
                return (lines, blocky);
            }
            let mut out = Vec::new();
            for e in &elems {
                out.push(format!("[{hp}]"));
                let mut open_sub: Option<String> = None;
                for item in &e.items {
                    match item {
                        GItem::Field(f) => {
                            let sub = render_path(&f.path[head.len() + 1..]);
                            if open_sub.as_deref() != Some(&sub) {
                                if open_sub.is_some() {
                                    out.push("()".into());
                                }
                                if !sub.is_empty() {
                                    out.push(format!("({sub})"));
                                    open_sub = Some(sub);
                                } else {
                                    open_sub = None;
                                }
                            }
                            out.extend(field_lines(f, &f.name));
                        }
                        GItem::Comment(c) => out.push((*c).clone()),
                        GItem::Gap => out.push(String::new()),
                    }
                }
                if open_sub.is_some() {
                    out.push("()".into());
                }
            }
            out.push("[]".into());
            (out, true)
        }
    }
}

// ── public entry points ─────────────────────────────────────────

/// Format an authored `.kaiv` stream into the standard style.
pub fn format_data(input: &str) -> Result<String, PipelineError> {
    let (shebang, nodes) = parse_authored(input)?;
    Ok(render(shebang, nodes))
}

/// Render a canonical `.raiv`/`.daiv` stream as idiomatic authored
/// kaiv. This is a *view*: authoring sugar that compilation
/// resolved away (variables, references, shorthands) does not come
/// back, and the result is an authored `.kaiv` document, not the
/// canonical artifact.
pub fn lift(input: &str) -> Result<String, PipelineError> {
    let which = ["daiv", "raiv"]
        .iter()
        .find(|k| crate::lexer::expect_kind(input, k).is_ok());
    let Some(kind) = which else {
        return Err(PipelineError::Other(
            "lift expects a canonical .daiv or .raiv stream (with its format declaration)".into(),
        ));
    };
    let nodes = parse_canonical(input, *kind == "daiv")?;
    Ok(render(None, nodes))
}

/// Light normalization for the other authored kinds
/// (`.saiv`/`.taiv`/`.faiv`/`.maiv`): whitespace and blank-line
/// discipline only — schema lines are never restructured.
pub fn format_plain(input: &str, kind: FileKind) -> Result<String, PipelineError> {
    let lines = lex(input.as_bytes(), kind).map_err(|e: LexErrorAt| PipelineError::Lex(e))?;
    let shebang = input
        .lines()
        .next()
        .filter(|l| l.starts_with("#!"))
        .map(|l| l.trim_end().to_string());
    let mut nodes: Vec<Node> = Vec::new();
    for l in &lines {
        raw_line(l, &mut nodes);
    }
    Ok(render(shebang, nodes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(s: &str) -> String {
        format_data(s).unwrap()
    }

    #[test]
    fn env_file_passes_through() {
        assert_eq!(fmt("HOST=localhost\nPORT=8080\n"), "HOST=localhost\nPORT=8080\n");
    }

    #[test]
    fn idempotent() {
        let src = ".!kaiv\ntitle=x\n\n(/owner)\nname=Ada\n!bool\nactive=true\n()\n\n[/@m]\nhost=a\n!int\nweight=2\n[/@m]\nhost=b\n!int\nweight=1\n[]\n";
        let once = fmt(src);
        assert_eq!(fmt(&once), once);
    }

    #[test]
    fn small_struct_goes_inline() {
        let src = ".!kaiv\n/server/api::host=localhost\n/server/api::port=8080\n";
        assert_eq!(fmt(src), ".!kaiv\n\n/server/api:=host=localhost|port=8080\n");
    }

    #[test]
    fn single_field_goes_flat() {
        let src = ".!kaiv\n(/owner)\nname=Ada\n()\n";
        assert_eq!(fmt(src), ".!kaiv\n\n/owner::name=Ada\n");
    }

    #[test]
    fn typed_struct_keeps_block() {
        let src = ".!kaiv\n/db::host=h\n!int\n/db::port=5\n";
        assert_eq!(fmt(src), ".!kaiv\n\n(/db)\nhost=h\n!int\nport=5\n()\n");
    }

    #[test]
    fn uniform_array_inline() {
        let src = ".!kaiv\n[/@m]\nhost=a\nweight=2\n[/@m]\nhost=b\nweight=1\n[]\n";
        assert_eq!(
            fmt(src),
            ".!kaiv\n\n/@m+:=host=a|weight=2\n/@m+:=host=b|weight=1\n"
        );
    }

    #[test]
    fn typed_element_forces_block_for_all() {
        let src = ".!kaiv\n/@m+:=host=a\n[/@m]\nhost=b\n!int\nweight=1\n[]\n";
        assert_eq!(
            fmt(src),
            ".!kaiv\n\n[/@m]\nhost=a\n[/@m]\nhost=b\n!int\nweight=1\n[]\n"
        );
    }

    #[test]
    fn pipe_in_value_forces_block() {
        let src = ".!kaiv\n/a::x=one|two\n/a::y=2\n";
        assert_eq!(fmt(src), ".!kaiv\n\n(/a)\nx=one|two\ny=2\n()\n");
    }

    #[test]
    fn gap_splits_top_level_groups() {
        let src = ".!kaiv\na=1\n\n\nb=2\n";
        assert_eq!(fmt(src), ".!kaiv\n\na=1\n\nb=2\n");
    }

    #[test]
    fn variables_pass_verbatim() {
        let src = ".!kaiv\n.timeout=30\n/api::t=$.timeout\n";
        assert_eq!(fmt(src), ".!kaiv\n\n.timeout=30\n/api::t=$.timeout\n");
    }

    #[test]
    fn version_one_drops() {
        assert_eq!(fmt(".!kaiv 1\nx=1\n"), ".!kaiv\n\nx=1\n");
    }

    #[test]
    fn comments_attach() {
        let src = ".!kaiv\n# the mirrors\n[/@m]\nhost=a\nweight=1\n[]\n";
        assert_eq!(fmt(src), ".!kaiv\n\n# the mirrors\n/@m+:=host=a|weight=1\n");
    }

    #[test]
    fn splat_block_stays_verbatim() {
        let src = ".!kaiv\n/.base:=host=h|ssl=true\n(/api)\n$/.base\n()\n";
        let out = fmt(src);
        assert!(out.contains("(/api)\n$/.base\n()"), "{out}");
    }

    #[test]
    fn lift_groups_and_types() {
        let daiv = ".!daiv\n!str'::title=hi\n!str'/owner::name=Ada\n!bool'/owner::active=true\n!str'/@m/0::host=a\n!str'/@m/1::host=b\n";
        let out = lift(daiv).unwrap();
        assert_eq!(
            out,
            ".!kaiv\n\ntitle=hi\n\n(/owner)\nname=Ada\n!bool\nactive=true\n()\n\n/@m+:=host=a\n/@m+:=host=b\n"
        );
    }

    #[test]
    fn lift_provenance() {
        let daiv = ".!daiv\n!str?sensor@20260101T000000Z'::t=21\n";
        assert_eq!(lift(daiv).unwrap(), ".!kaiv\n\n?sensor@20260101T000000Z\nt=21\n");
    }

    #[test]
    fn lift_refuses_authored() {
        assert!(lift(".!kaiv\nx=1\n").is_err());
    }

    #[test]
    fn value_bytes_never_change() {
        let src = ".!kaiv\n  key  =  padded value  \n";
        assert_eq!(fmt(src), ".!kaiv\n\nkey=  padded value  \n");
    }
}
