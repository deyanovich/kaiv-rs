//! The Compiler: authored `.kaiv` → relational canonical `.raiv`.
//! Resolves variables and syntactic sugar (`+=`, `;=`, `:=`, `+:=`,
//! blocks, maps, `&` core shorthands, unit canonicalization); preserves
//! `$field` references for the Denormalizer.

use crate::anno::{parse_annotation, parse_constraint_items, Annotation, Constraint, Item};
use crate::error::{AppError, PipelineError};
use crate::lexer::{lex, FileKind, LineKind};
use crate::resolve::{resolve_named, Resolver};
use crate::unit;
use std::collections::HashMap;

/// Compile with the core-only resolver (embedded `std/core`, no
/// registry configuration).
pub fn compile(input: &[u8]) -> Result<String, PipelineError> {
    compile_with(input, &Resolver::offline())
}

pub fn compile_with(input: &[u8], resolver: &Resolver) -> Result<String, PipelineError> {
    let lines = lex(input, FileKind::Data).map_err(PipelineError::Lex)?;
    let mut c = Compiler::new(resolver);
    for line in &lines {
        c.step(&line.kind)?;
    }
    // An annotation still pending at EOF never found its data line
    // (SPEC.md § Application Errors, MetadataWithoutTargetError).
    if c.pending_anno.is_some() || c.pending_prov.is_some() {
        return Err(PipelineError::App(AppError::MetadataWithoutTarget));
    }
    let mut out = c.out.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

struct Compiler<'r> {
    resolver: &'r Resolver,
    /// `.!types` imports, in declaration order.
    imports: Vec<String>,
    /// `.!units` imports, in declaration order.
    unit_imports: Vec<String>,
    /// Custom unit names from the imports, built lazily.
    custom_units: Option<std::collections::BTreeSet<String>>,
    /// `.!registry` Layer 1 overrides (prefix → base).
    registries: Vec<(String, String)>,
    out: Vec<String>,
    pending_anno: Option<Annotation>,
    pending_prov: Option<String>,
    scalar_vars: HashMap<String, String>,
    ns_vars: HashMap<String, Vec<(String, String)>>,
    /// Next element index per canonical array path.
    counters: HashMap<String, usize>,
    blocks: Vec<Block>,
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
    ArrStruct(&'a str),
    Struct(&'a str),
    Append(&'a str),
    Extend(&'a str),
    Key(&'a str),
}

fn parse_left(left: &str) -> Left<'_> {
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
/// only at the `::` boundary; quoted interior path segments are not
/// yet supported.
pub(crate) fn split_namepath(key: &str) -> (Vec<String>, String) {
    let (path, field) = match rsplit_projection(key) {
        Some((p, f)) => (p, f),
        None => ("", key),
    };
    let steps = path
        .trim_start_matches('/')
        .split('/')
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
fn path_steps(key: &str) -> Vec<String> {
    key.trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(normalize_seg)
        .collect()
}

fn render_path(steps: &[String]) -> String {
    if steps.is_empty() {
        String::new()
    } else {
        format!("/{}", steps.join("/"))
    }
}

enum Resolved {
    Lit(String),
    /// `$field` / `$path::field` — preserved verbatim in `.raiv`.
    FieldRef(String),
}

impl<'r> Compiler<'r> {
    fn new(resolver: &'r Resolver) -> Self {
        Compiler {
            resolver,
            imports: Vec::new(),
            unit_imports: Vec::new(),
            custom_units: None,
            registries: Vec::new(),
            out: Vec::new(),
            pending_anno: None,
            pending_prov: None,
            scalar_vars: HashMap::new(),
            ns_vars: HashMap::new(),
            counters: HashMap::new(),
            blocks: Vec::new(),
        }
    }

    fn step(&mut self, kind: &LineKind<'_>) -> Result<(), PipelineError> {
        match kind {
            LineKind::Blank | LineKind::Comment(_) | LineKind::Doc(_) => {
                // A metadata annotation must be immediately followed by
                // a data line (SPEC.md MetadataWithoutTargetError).
                if self.pending_anno.is_some() || self.pending_prov.is_some() {
                    return Err(PipelineError::App(AppError::MetadataWithoutTarget));
                }
                Ok(())
            }
            LineKind::Decl(s) => {
                // `.!types` imports and `.!registry` Layer 1 overrides
                // configure resolution; all declarations pass through
                // into canonical output (resolution metadata).
                if let Some(rest) = s.strip_prefix(".!types") {
                    let lib = rest.trim_matches([' ', '\t']);
                    if !lib.is_empty() {
                        self.imports.push(lib.to_string());
                    }
                } else if let Some(rest) = s.strip_prefix(".!units") {
                    let lib = rest.trim_matches([' ', '\t']);
                    if !lib.is_empty() {
                        self.unit_imports.push(lib.to_string());
                    }
                } else if let Some(rest) = s.strip_prefix(".!registry") {
                    if let Some((p, b)) = rest.trim_matches([' ', '\t']).split_once('=') {
                        self.registries.push((p.to_string(), b.to_string()));
                    }
                }
                self.out.push((*s).to_string());
                Ok(())
            }
            LineKind::Meta(s) => self.meta(s),
            LineKind::SectionOpen(inner) => self.section_open(inner),
            LineKind::SectionClose => {
                if matches!(self.blocks.last(), Some(Block::Array { .. })) {
                    self.blocks.pop();
                }
                Ok(())
            }
            LineKind::NsOpen(inner) => self.ns_open(inner),
            LineKind::NsClose => {
                if matches!(self.blocks.last(), Some(Block::Ns { .. })) {
                    self.blocks.pop();
                }
                Ok(())
            }
            LineKind::Content { left, value } => self.content(left, value),
        }
    }

    fn meta(&mut self, s: &str) -> Result<(), PipelineError> {
        // A second metadata line while one is pending means the first
        // never reached a data line. (Type + provenance stack on ONE
        // line via the inline `!type?prov` form, not on two lines.)
        if self.pending_anno.is_some() || self.pending_prov.is_some() {
            return Err(PipelineError::App(AppError::MetadataWithoutTarget));
        }
        if s.starts_with('!') {
            let a = parse_annotation(s)
                .ok_or_else(|| PipelineError::Other(format!("bad annotation: {s}")))?;
            if let Some(u) = &a.unit {
                self.check_unit(u)?;
            }
            self.pending_anno = Some(a);
        } else if let Some(p) = s.strip_prefix('?') {
            self.pending_prov = Some(p.to_string());
        } else if let Some(rest) = s.strip_prefix('&') {
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
        let head = inner.split([' ', '\t']).next().unwrap_or("");
        // A repeated opener for the same array continues with the next
        // element; compute the base path against the *enclosing* scope.
        if let Some(Block::Array { key, .. }) = self.blocks.last() {
            let key = key.clone();
            self.blocks.pop();
            let outer = self.prefix_steps();
            let mut base = outer;
            base.extend(path_steps(head));
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
        base.extend(path_steps(head));
        let key = render_path(&base);
        let idx = self.next_index(&key);
        let mut steps = base;
        steps.push(idx.to_string());
        self.blocks.push(Block::Array { key, steps });
        Ok(())
    }

    fn ns_open(&mut self, inner: &str) -> Result<(), PipelineError> {
        let head = inner.split([' ', '\t']).next().unwrap_or("");
        let mut steps = self.prefix_steps();
        steps.extend(path_steps(head));
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
        match parse_left(left) {
            Left::VarScalar(name) => {
                let v = match self.resolve(value)? {
                    Resolved::Lit(v) => v,
                    Resolved::FieldRef(v) => v,
                };
                self.scalar_vars.insert(name.to_string(), v);
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
                steps.extend(path_steps(path));
                self.emit_pairs(&steps, value)
            }
            Left::ArrStruct(path) => {
                let mut steps = self.prefix_steps();
                steps.extend(path_steps(path));
                let key = render_path(&steps);
                let idx = self.next_index(&key);
                steps.push(idx.to_string());
                self.emit_pairs(&steps, value)
            }
            Left::Append(path) => {
                let mut steps = self.prefix_steps();
                steps.extend(path_steps(path));
                let key = render_path(&steps);
                let idx = self.next_index(&key);
                let v = self.resolve(value)?;
                self.emit(&steps, &idx.to_string(), v);
                self.clear_pending();
                Ok(())
            }
            Left::Extend(path) => {
                let mut steps = self.prefix_steps();
                steps.extend(path_steps(path));
                let key = render_path(&steps);
                for elem in value.split(';') {
                    let idx = self.next_index(&key);
                    let v = self.resolve(elem)?;
                    self.emit(&steps, &idx.to_string(), v);
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
                let v = self.resolve(value)?;
                self.emit(&steps, &field, v);
                self.clear_pending();
                Ok(())
            }
        }
    }

    fn emit_map(&mut self, key: &str, value: &str) -> Result<(), PipelineError> {
        let a = self.pending_anno.take().unwrap();
        let vtype = a.map_value.clone().unwrap_or_else(|| "str".to_string());
        self.pending_prov = None;
        if value == "{}" {
            return Ok(()); // empty map: no canonical entry lines
        }
        let (rel, field) = split_namepath(key);
        let mut steps = self.prefix_steps();
        steps.extend(rel);
        steps.push(field); // the map itself is a namespace
        for pair in value.split(';') {
            let (k, v) = pair
                .split_once(':')
                .ok_or_else(|| PipelineError::Other(format!("malformed map pair: {pair}")))?;
            let line = format!("!{vtype}'{}::{}={}", render_path(&steps), k, v);
            self.out.push(line);
        }
        Ok(())
    }

    /// Struct-assignment right side → resolved (field, value) pairs.
    fn parse_pairs(&mut self, value: &str) -> Result<Vec<(String, String)>, PipelineError> {
        if let Some(name) = value.strip_prefix("$/.") {
            return self
                .ns_vars
                .get(name)
                .cloned()
                .ok_or_else(|| PipelineError::Other(format!("undefined namespace var /.{name}")));
        }
        let mut pairs = Vec::new();
        for pair in value.split('|') {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| PipelineError::Other(format!("malformed struct pair: {pair}")))?;
            let v = match self.resolve(v)? {
                Resolved::Lit(s) | Resolved::FieldRef(s) => s,
            };
            pairs.push((k.to_string(), v));
        }
        Ok(pairs)
    }

    fn emit_pairs(&mut self, steps: &[String], value: &str) -> Result<(), PipelineError> {
        let pairs = self.parse_pairs(value)?;
        for (k, v) in pairs {
            if let Some(name) = k.strip_prefix('@') {
                // Array ops inside a struct value: @tags+=v / @tags;=a;b
                let (name, multi) = match name.strip_suffix(['+', ';']) {
                    Some(n) => (n, name.ends_with(';')),
                    None => (name, false),
                };
                let mut asteps = steps.to_vec();
                asteps.push(format!("@{name}"));
                let key = render_path(&asteps);
                let elems: Vec<&str> = if multi {
                    v.split(';').collect()
                } else {
                    vec![v.as_str()]
                };
                for e in elems {
                    let idx = self.next_index(&key);
                    self.emit(&asteps, &idx.to_string(), Resolved::Lit(e.to_string()));
                }
            } else {
                self.emit(steps, &k, Resolved::Lit(v));
            }
        }
        self.clear_pending();
        Ok(())
    }

    fn resolve(&self, value: &str) -> Result<Resolved, PipelineError> {
        if let Some(name) = value.strip_prefix("$.") {
            return self
                .scalar_vars
                .get(name)
                .cloned()
                .map(Resolved::Lit)
                .ok_or_else(|| PipelineError::Other(format!("undefined variable $.{name}")));
        }
        if value.starts_with("$@.") || value.starts_with("$/.") {
            return Err(PipelineError::Other(format!(
                "container variable reference in scalar position: {value}"
            )));
        }
        if value.starts_with('$') && value.len() > 1 {
            return Ok(Resolved::FieldRef(value.to_string()));
        }
        Ok(Resolved::Lit(value.to_string()))
    }

    fn emit(&mut self, steps: &[String], field: &str, value: Resolved) {
        let prefix = self.render_prefix();
        let v = match value {
            Resolved::Lit(s) | Resolved::FieldRef(s) => s,
        };
        self.out
            .push(format!("{prefix}'{}::{}={}", render_path(steps), field, v));
    }

    /// Canonical metadata prefix from the pending annotation state.
    /// Read-only: a line that expands to several canonical lines
    /// (`;=`, `:=`, `+:=`) applies its annotation to every one of
    /// them; the caller clears the pending state after the expansion.
    fn render_prefix(&self) -> String {
        let a = self.pending_anno.clone().unwrap_or_default();
        let prov = self.pending_prov.clone();
        let mut s = String::from("!");
        s.push_str(if a.type_name.is_empty() {
            "str"
        } else {
            &a.type_name
        });
        for alt in &a.union {
            s.push('|');
            s.push_str(&alt.name);
            for c in &alt.constraints {
                s.push_str(&render_constraint(c));
            }
        }
        // Canonical constraint order: pattern, range, enum, length.
        let mut ordered: Vec<&Constraint> = Vec::new();
        let rank = |c: &Constraint| match c {
            Constraint::Pattern(_) => 0,
            Constraint::Range(..) => 1,
            Constraint::Enum(_) => 2,
            Constraint::Length(_) => 3,
            Constraint::Span(_) => 4,
        };
        let mut cs: Vec<&Constraint> = a.constraints.iter().collect();
        cs.sort_by_key(|c| rank(c));
        ordered.extend(cs);
        for c in ordered {
            s.push_str(&render_constraint(c));
        }
        if let Some(u) = &a.unit {
            s.push(':');
            s.push_str(&unit::canonicalize(u).unwrap_or_else(|| u.clone()));
        }
        if let Some(p) = prov.or(a.provenance.clone()) {
            s.push('?');
            s.push_str(&p);
        }
        s
    }

    fn clear_pending(&mut self) {
        self.pending_anno = None;
        self.pending_prov = None;
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

    fn app_err(input: &str) -> Option<AppError> {
        match crate::compile(input.as_bytes()) {
            Err(PipelineError::App(e)) => Some(e),
            _ => None,
        }
    }

    #[test]
    fn metadata_without_target() {
        let e = Some(AppError::MetadataWithoutTarget);
        assert_eq!(app_err(".!kaiv 1\n!int\n\nx=1\n"), e); // blank between
        assert_eq!(app_err(".!kaiv 1\n!int\n# c\nx=1\n"), e); // comment between
        assert_eq!(app_err(".!kaiv 1\n!int\n!str\nx=1\n"), e); // stacked annos
        assert_eq!(app_err(".!kaiv 1\n?src\n!int\nx=1\n"), e); // prov then type
        assert_eq!(app_err(".!kaiv 1\n!int\n"), e); // EOF
        assert_eq!(app_err(".!kaiv 1\n!int\nx=1\n"), None); // legal
        assert_eq!(app_err(".!kaiv 1\n!int?sensor1\nx=1\n"), None); // inline stack
    }
}
