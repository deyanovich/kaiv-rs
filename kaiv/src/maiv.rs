//! Mappings (`.maiv`): SPEC.md § Mappings. A mapping is a pure
//! structural correspondence between two schemas — a name-to-name
//! rewrite table with no value transformations. This module parses
//! `.maiv` files, validates them against both schemas at publish
//! time, applies them (the mapper: source `.daiv` → target `.daiv`),
//! and composes them (a join on namepaths).

use crate::error::{AppError, LexError, LexErrorAt, PipelineError};
use crate::lexer::{lex, FileKind, LineKind};
use crate::validator::{default_applicable, CompiledSchema, SchemaField};

/// A parsed `.maiv` mapping. A mapping has no name of its own: its
/// identity is the `(source, target)` endpoint pair, and its
/// registry address derives from it — `{source}/{target}.maiv`,
/// with the target's namespace omitted when it equals the source's
/// (SPEC.md § Mappings).
#[derive(Debug, Clone)]
pub struct Mapping {
    /// Source schema reference (`.!source`).
    pub source: String,
    /// Target schema reference (`.!target`).
    pub target: String,
    /// Composition provenance trail (`.!via`), in application order.
    pub via: Vec<String>,
    /// Deliberately unmapped source fields (`.!drop`).
    pub drops: Vec<String>,
    /// Mapping lines, in authored order.
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    /// Target namepath (may carry the `/*` wildcard).
    pub target: String,
    pub rhs: Rhs,
}

#[derive(Debug, Clone)]
pub enum Rhs {
    /// `$source_namepath`, with an optional `|`-override.
    Ref {
        source: String,
        fallback: Option<Fallback>,
    },
    /// A literal constant (the `$$` doubling already collapsed).
    Const(String),
}

#[derive(Debug, Clone)]
pub enum Fallback {
    /// `|constant` — emitted when the source value fails the target
    /// field's constraint.
    Constant(String),
    /// `|!null` — the target field must be declared nullable.
    Null,
}

pub fn parse_maiv(input: &[u8]) -> Result<Mapping, PipelineError> {
    let lines = lex(input, FileKind::Mapping).map_err(PipelineError::Lex)?;
    let mut seen_maiv = false;
    let mut source = None;
    let mut target = None;
    let mut via = Vec::new();
    let mut drops = Vec::new();
    let mut rules = Vec::new();
    let one = |what: &str, slot: &mut Option<String>, v: &str, no: usize| {
        if slot.is_some() {
            return Err(PipelineError::Other(format!(
                "duplicate {what} declaration (line {no})"
            )));
        }
        *slot = Some(v.to_string());
        Ok(())
    };
    for line in &lines {
        match &line.kind {
            LineKind::Blank | LineKind::Comment(_) | LineKind::Doc(_) => {}
            LineKind::Decl(s) => {
                if let Some(rest) = s.strip_prefix(".!maiv") {
                    // Bare or versioned; carries no identity token —
                    // the lexer already validated the version syntax.
                    let _ = rest;
                    if seen_maiv {
                        return Err(PipelineError::Other(format!(
                            "duplicate .!maiv declaration (line {})",
                            line.no
                        )));
                    }
                    seen_maiv = true;
                } else if let Some(rest) = s.strip_prefix(".!source") {
                    one(".!source", &mut source, rest.trim_matches([' ', '\t']), line.no)?;
                } else if let Some(rest) = s.strip_prefix(".!target") {
                    one(".!target", &mut target, rest.trim_matches([' ', '\t']), line.no)?;
                } else if let Some(rest) = s.strip_prefix(".!via") {
                    via.push(rest.trim_matches([' ', '\t']).to_string());
                } else if let Some(rest) = s.strip_prefix(".!drop") {
                    drops.push(rest.trim_matches([' ', '\t']).to_string());
                } else {
                    return Err(PipelineError::Other(format!(
                        "declaration out of place in .maiv (line {}): {s}",
                        line.no
                    )));
                }
            }
            LineKind::Content { left, value } => {
                rules.push(Rule {
                    target: (*left).to_string(),
                    rhs: parse_rhs(value, line.no)?,
                });
            }
            _ => {
                return Err(PipelineError::Lex(LexErrorAt {
                    error: LexError::InvalidKey,
                    line: line.no,
                }))
            }
        }
    }
    let need = |what: &str, v: Option<String>| {
        v.ok_or_else(|| PipelineError::Other(format!(".maiv is missing its {what} declaration")))
    };
    if !seen_maiv {
        return Err(PipelineError::Other(
            ".maiv is missing its .!maiv format declaration".into(),
        ));
    }
    Ok(Mapping {
        source: need(".!source", source)?,
        target: need(".!target", target)?,
        via,
        drops,
        rules,
    })
}

/// Parse a mapping line's right side (SPEC.md § Mapping Lines):
/// `$namepath`, `$namepath|constant`, `$namepath|!null`, or a
/// literal constant (`$$` doubling collapsed).
fn parse_rhs(value: &str, no: usize) -> Result<Rhs, PipelineError> {
    if let Some(lit) = value.strip_prefix("$$") {
        return Ok(Rhs::Const(format!("${lit}")));
    }
    let Some(rest) = value.strip_prefix('$') else {
        return Ok(Rhs::Const(value.to_string()));
    };
    match rest.split_once('|') {
        None => Ok(Rhs::Ref {
            source: rest.to_string(),
            fallback: None,
        }),
        Some((np, fb)) => {
            let fallback = if fb == "!null" {
                Fallback::Null
            } else {
                // One override per line, no logic: a literal `|` in
                // the override constant collides with the delimiter
                // (SPEC.md § Mapping Lines).
                if fb.contains('|') {
                    return Err(PipelineError::App(AppError::DelimiterCollision));
                }
                let _ = no;
                Fallback::Constant(fb.to_string())
            };
            Ok(Rhs::Ref {
                source: np.to_string(),
                fallback: Some(fallback),
            })
        }
    }
}

/// A mapping namepath in the compiled schema's shape: the `/*`
/// wildcard (every element index) becomes the `.csaiv` elided-index
/// form (`/@servers/*::host` → `/@servers/::host`).
fn schema_shape(np: &str) -> String {
    np.replace("/*", "/")
}

/// Find the schema field a mapping namepath addresses. Wildcards
/// address the elided-index element field; a concrete element index
/// (`/@servers/0::hostname` — one element bound, as composition
/// produces) addresses the same field via the collection match.
fn field_for<'s>(schema: &'s CompiledSchema, np: &str) -> Option<&'s SchemaField> {
    let shaped = schema_shape(np);
    schema
        .fields
        .iter()
        .find(|f| f.namepath == shaped || crate::validator::line_matches(f, &shaped))
}

/// Is the field declared nullable (`!null|T` or `!null`)?
fn nullable(f: &SchemaField) -> bool {
    f.items.iter().any(|i| match i {
        crate::anno::Item::Anno(a) => {
            a.type_name == "null" || a.union.iter().any(|alt| alt.name == "null")
        }
        _ => false,
    })
}

/// Publish-time validation (SPEC.md § Publish-Time Validation):
/// namepath existence on both sides, override admissibility, and the
/// static completeness check.
pub fn validate_maiv(
    m: &Mapping,
    src: &CompiledSchema,
    tgt: &CompiledSchema,
) -> Result<(), PipelineError> {
    for rule in &m.rules {
        let Some(tf) = field_for(tgt, &rule.target) else {
            return Err(PipelineError::Other(format!(
                "UndefinedReferenceError: target namepath {} resolves to no \
                 declared field in {}",
                rule.target, m.target
            )));
        };
        match &rule.rhs {
            Rhs::Ref { source, fallback } => {
                if field_for(src, source).is_none() {
                    return Err(PipelineError::Other(format!(
                        "UndefinedReferenceError: source namepath {source} \
                         resolves to no declared field in {}",
                        m.source
                    )));
                }
                match fallback {
                    Some(Fallback::Constant(c)) => {
                        if !default_applicable(&tf.items, c) {
                            return Err(PipelineError::Other(format!(
                                "ConstraintViolationError: override constant \
                                 {c:?} does not satisfy the target field {}",
                                rule.target
                            )));
                        }
                    }
                    Some(Fallback::Null) => {
                        if !nullable(tf) {
                            return Err(PipelineError::Other(format!(
                                "|!null fallback on a non-nullable target \
                                 field: {}",
                                rule.target
                            )));
                        }
                    }
                    None => {}
                }
            }
            Rhs::Const(c) => {
                if !default_applicable(&tf.items, c) {
                    return Err(PipelineError::Other(format!(
                        "ConstraintViolationError: constant {c:?} does not \
                         satisfy the target field {}",
                        rule.target
                    )));
                }
            }
        }
    }
    for d in &m.drops {
        if field_for(src, d).is_none() {
            return Err(PipelineError::Other(format!(
                "UndefinedReferenceError: .!drop namepath {d} resolves to no \
                 declared field in {}",
                m.source
            )));
        }
    }
    // Completeness (static): every required non-collection target
    // field must be produced by a mapping or constant line. Optional
    // fields materialize from the target schema's own defaults;
    // collection element fields are per-element (an empty collection
    // is valid), so runtime validation governs them.
    for f in &tgt.fields {
        if f.optional || crate::validator::is_collection(f) {
            continue;
        }
        let produced = m
            .rules
            .iter()
            .any(|r| schema_shape(&r.target) == f.namepath);
        if !produced {
            return Err(PipelineError::App(AppError::IncompleteMapping));
        }
    }
    Ok(())
}

/// Match a source data namepath against a mapping source pattern.
/// Exact match, or — when the pattern carries `/*` wildcards — every
/// wildcard binds one element index; returns the bound indices.
fn match_source<'a>(pattern: &str, np: &'a str) -> Option<Vec<&'a str>> {
    if !pattern.contains("/*") {
        return (pattern == np).then(Vec::new);
    }
    let mut idxs = Vec::new();
    let mut rest = np;
    let parts: Vec<&str> = pattern.split("/*").collect();
    for (i, part) in parts.iter().enumerate() {
        rest = rest.strip_prefix(part)?;
        if i + 1 < parts.len() {
            let s = rest.strip_prefix('/')?;
            let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
            if end == 0 {
                return None;
            }
            idxs.push(&s[..end]);
            rest = &s[end..];
        }
    }
    rest.is_empty().then_some(idxs)
}

/// Substitute bound element indices into a target pattern's `/*`
/// wildcards, in order.
fn bind_target(pattern: &str, idxs: &[&str]) -> String {
    let mut out = String::new();
    let parts: Vec<&str> = pattern.split("/*").collect();
    for (i, part) in parts.iter().enumerate() {
        out.push_str(part);
        if i + 1 < parts.len() {
            out.push('/');
            out.push_str(idxs.get(i).copied().unwrap_or("0"));
        }
    }
    out
}

/// The annotation a produced line carries: the target field's
/// retained type item when it has one (`!str`, `!text`, a
/// unit-carrying `!type:unit`), else the fallback the caller
/// suggests (the source line's own annotation for mapped lines,
/// `str` for constants). Union-typed fields keep the suggestion —
/// the Validator checks alternative membership.
fn target_annotation(f: &SchemaField, suggested: &str) -> String {
    for item in &f.items {
        if let crate::anno::Item::Anno(a) = item {
            if !a.union.is_empty() {
                return suggested.to_string();
            }
            let mut t = a.type_name.clone();
            if let Some(u) = &a.unit {
                t.push(':');
                t.push_str(u);
            }
            return t;
        }
    }
    suggested.to_string()
}

/// Apply a mapping: source `.daiv` → target `.daiv` (SPEC.md
/// § Execution Model). Single-pass over the source; the output is
/// assembled in target schema order, materialized against the
/// target schema, and carries a `.!schema` declaration for the
/// target when the reference is a registry path.
pub fn apply(
    m: &Mapping,
    source_daiv: &str,
    tgt: &CompiledSchema,
) -> Result<String, PipelineError> {
    crate::lexer::expect_kind(source_daiv, "daiv").map_err(PipelineError::Lex)?;
    // (target namepath, emitted body) accumulated per rule match.
    let mut produced: Vec<(String, String)> = Vec::new();
    let mut prov_decls: Vec<&str> = Vec::new();

    for raw in source_daiv.lines() {
        let s = raw.trim_start_matches([' ', '\t']);
        if s.is_empty() || s.starts_with('#') || s.starts_with("//") {
            continue;
        }
        if s.starts_with(".?") {
            prov_decls.push(s);
            continue;
        }
        if s.starts_with(".!") {
            continue; // source declarations do not carry over
        }
        let tick = s.find('\'').ok_or_else(|| {
            PipelineError::Other(format!("not a canonical line: {s}"))
        })?;
        let prefix = &s[..tick];
        let rest = &s[tick + 1..];
        let eq = crate::validator::first_eq(rest)
            .ok_or_else(|| PipelineError::Other(format!("canonical line without =: {s}")))?;
        let (np, value) = (&rest[..eq], &rest[eq + 1..]);
        let a = crate::anno::parse_annotation(prefix)
            .ok_or_else(|| PipelineError::Other(format!("bad metadata prefix: {s}")))?;
        // Provenance survives the rewrite; the type annotation is
        // resolved from the target schema.
        let prov = a
            .provenance
            .as_deref()
            .map(|p| format!("?{p}"))
            .unwrap_or_default();

        for rule in &m.rules {
            let Rhs::Ref { source, fallback } = &rule.rhs else {
                continue;
            };
            let Some(idxs) = match_source(source, np) else {
                continue;
            };
            let target_np = bind_target(&rule.target, &idxs);
            let Some(tf) = field_for(tgt, &rule.target) else {
                continue; // validate_maiv rejects this; be permissive here
            };
            let source_type = if a.type_name.is_empty() { "str" } else { &a.type_name };
            let ok = default_applicable(&tf.items, value);
            let line = if ok {
                let t = target_annotation(tf, source_type);
                format!("!{t}{prov}'{target_np}={value}")
            } else {
                match fallback {
                    Some(Fallback::Constant(c)) => {
                        let t = target_annotation(tf, "str");
                        format!("!{t}{prov}'{target_np}={c}")
                    }
                    Some(Fallback::Null) => format!("!null{prov}'{target_np}="),
                    // No override: emit anyway — the target's
                    // Validator reports it (SPEC.md § Execution
                    // Model, step 3f).
                    None => {
                        let t = target_annotation(tf, source_type);
                        format!("!{t}{prov}'{target_np}={value}")
                    }
                }
            };
            produced.push((target_np, line));
        }
    }

    // Constant lines for targets not produced from the source.
    for rule in &m.rules {
        let Rhs::Const(c) = &rule.rhs else { continue };
        if produced.iter().any(|(np, _)| np == &rule.target) {
            continue;
        }
        if let Some(tf) = field_for(tgt, &rule.target) {
            let t = target_annotation(tf, "str");
            produced.push((rule.target.clone(), format!("!{t}'{}={c}", rule.target)));
        }
    }

    // Assemble in target schema order (SPEC.md § Execution Model):
    // walk the compiled schema's fields and emit each field's lines,
    // element lines sorted by index.
    let mut out = String::from(".!daiv\n");
    if !m.target.contains("://") {
        out.push_str(&format!(".!schema:{}\n", m.target));
    }
    for d in &prov_decls {
        out.push_str(d);
        out.push('\n');
    }
    let mut used = vec![false; produced.len()];
    for f in &tgt.fields {
        let mut group: Vec<(usize, &String)> = produced
            .iter()
            .enumerate()
            .filter(|(i, (np, _))| !used[*i] && crate::validator::line_matches(f, np))
            .map(|(i, (np, _))| (i, np))
            .collect();
        // Element lines sort by their bound index (numeric).
        group.sort_by_key(|(_, np)| {
            np.strip_prefix(&schema_shape(&f.namepath).replace("::", "/"))
                .and_then(|r| r.split("::").next())
                .and_then(|d| d.parse::<u64>().ok())
                .unwrap_or(0)
        });
        for (i, _) in group {
            used[i] = true;
            out.push_str(&produced[i].1);
            out.push('\n');
        }
    }
    for (i, (_, line)) in produced.iter().enumerate() {
        if !used[i] {
            out.push_str(line);
            out.push('\n');
        }
    }
    // Materialize the target schema's absent optional fields — the
    // output is a complete deployment artifact (§ Null Semantics).
    crate::denorm::materialize(&out, tgt)
}

/// Compose two mappings (SPEC.md § Composition): given `b`←`a` with
/// `b.source == a.target`, produce `b.target`←`a.source` by joining
/// on namepaths — each source reference of `b` is replaced by the
/// rule of `a` that produces it. String substitution, no synthesis;
/// a `b`-reference no rule of `a` produces is dropped (the target
/// schema's own defaults and the completeness check govern).
pub fn compose(a: &Mapping, b: &Mapping) -> Result<Mapping, PipelineError> {
    if a.target != b.source {
        return Err(PipelineError::Other(format!(
            "cannot compose: {} targets {} but {} sources {}",
            a.edge_path(),
            a.target,
            b.edge_path(),
            b.source
        )));
    }
    let mut rules = Vec::new();
    for rule in &b.rules {
        match &rule.rhs {
            Rhs::Const(_) => rules.push(rule.clone()),
            Rhs::Ref { source, fallback } => {
                // Join on namepaths. Three shapes: pattern equality
                // (wildcard-to-wildcard or plain), and a concrete
                // element reference in `b` joining `a`'s wildcard
                // target — the bound indices carry through to `a`'s
                // source pattern.
                let hit = a.rules.iter().find_map(|r| {
                    if schema_shape(&r.target) == schema_shape(source) {
                        return Some((r, None));
                    }
                    match_source(&r.target, source).map(|idxs| {
                        let owned: Vec<String> =
                            idxs.iter().map(|i| i.to_string()).collect();
                        (r, Some(owned))
                    })
                });
                let Some((ar, idxs)) = hit else {
                    continue;
                };
                match &ar.rhs {
                    Rhs::Ref { source: s0, .. } => {
                        let source = match &idxs {
                            Some(bound) => {
                                let refs: Vec<&str> =
                                    bound.iter().map(String::as_str).collect();
                                bind_target(s0, &refs)
                            }
                            None => s0.clone(),
                        };
                        rules.push(Rule {
                            target: rule.target.clone(),
                            rhs: Rhs::Ref {
                                source,
                                fallback: fallback.clone(),
                            },
                        });
                    }
                    Rhs::Const(c) => rules.push(Rule {
                        target: rule.target.clone(),
                        rhs: Rhs::Const(c.clone()),
                    }),
                }
            }
        }
    }
    let mut via = a.via.clone();
    via.push(a.edge_path());
    via.extend(b.via.iter().cloned());
    via.push(b.edge_path());
    Ok(Mapping {
        source: a.source.clone(),
        target: b.target.clone(),
        via,
        drops: Vec::new(),
        rules,
    })
}

impl Mapping {
    /// The mapping's canonical edge identity:
    /// `{source}/mapto/{target}` with the target's namespace omitted
    /// when it equals the source's — the short form is canonical
    /// (SPEC.md § Registry Addressing). Appending an edition segment
    /// and `.maiv` gives the eternalink address on `ksaiv.com`; the
    /// `mapto` marker makes the address read as a sentence and names
    /// the publisher (the source's owner — the target-owner variant
    /// uses `mapfrom` and reverses the endpoints).
    pub fn edge_path(&self) -> String {
        let sns = self.source.split('/').next().unwrap_or("");
        match self.target.strip_prefix(sns).and_then(|r| r.strip_prefix('/')) {
            Some(tname) if !sns.is_empty() => {
                format!("{}/mapto/{tname}", self.source)
            }
            _ => format!("{}/mapto/{}", self.source, self.target),
        }
    }

    /// Render back to `.maiv` text.
    pub fn render(&self) -> String {
        let mut out = String::from(".!maiv\n");
        out.push_str(&format!(".!source {}\n", self.source));
        out.push_str(&format!(".!target {}\n", self.target));
        for v in &self.via {
            out.push_str(&format!(".!via {v}\n"));
        }
        for d in &self.drops {
            out.push_str(&format!(".!drop {d}\n"));
        }
        for r in &self.rules {
            match &r.rhs {
                Rhs::Ref {
                    source,
                    fallback: None,
                } => out.push_str(&format!("{}=${}\n", r.target, source)),
                Rhs::Ref {
                    source,
                    fallback: Some(Fallback::Constant(c)),
                } => out.push_str(&format!("{}=${}|{}\n", r.target, source, c)),
                Rhs::Ref {
                    source,
                    fallback: Some(Fallback::Null),
                } => out.push_str(&format!("{}=${}|!null\n", r.target, source)),
                Rhs::Const(c) => {
                    let c = if c.starts_with('$') {
                        format!("${c}")
                    } else {
                        c.clone()
                    };
                    out.push_str(&format!("{}={c}\n", r.target));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{AppError, PipelineError};

    const SRC: &str = ".!saiv 1 acme/fleet\n!str\nlegacy_region=\n[/@servers]\n!str\nhostname=\n!int\nweight=\n[]\n";
    const TGT: &str = ".!saiv 1 hub/nodes\n!null|int\nregion=\n[/@nodes]\n!str\nhost=\n[]\n";
    const MAP: &str = ".!maiv\n.!source acme/fleet\n.!target hub/nodes\n.!drop /@servers/*::weight\n\n/@nodes/*::host=$/@servers/*::hostname\n::region=$::legacy_region|!null\n";

    fn schemas() -> (CompiledSchema, CompiledSchema) {
        let s = crate::compile_schema(SRC.as_bytes()).unwrap();
        let t = crate::compile_schema(TGT.as_bytes()).unwrap();
        (
            crate::parse_csaiv(&s).unwrap(),
            crate::parse_csaiv(&t).unwrap(),
        )
    }

    #[test]
    fn parse_validate_apply() {
        let m = parse_maiv(MAP.as_bytes()).unwrap();
        // Cross-namespace edge: the derived path is 4-segment.
        assert_eq!(m.edge_path(), "acme/fleet/mapto/hub/nodes");
        assert_eq!(m.drops, ["/@servers/*::weight"]);
        let (src, tgt) = schemas();
        validate_maiv(&m, &src, &tgt).unwrap();
        let daiv = ".!daiv\n!str'::legacy_region=eu-legacy\n!str'/@servers/0::hostname=a\n!int'/@servers/0::weight=1\n!str'/@servers/1::hostname=b\n!int'/@servers/1::weight=2\n";
        let out = apply(&m, daiv, &tgt).unwrap();
        // Target schema order; wildcard binds both elements; weight
        // dropped; the non-int region value falls back to !null.
        assert_eq!(
            out,
            ".!daiv\n.!schema:hub/nodes\n!null'::region=\n!str'/@nodes/0::host=a\n!str'/@nodes/1::host=b\n"
        );
        assert!(crate::validate(&out, &tgt).is_ok());
    }

    #[test]
    fn publish_time_rejections() {
        let (src, tgt) = schemas();
        // Unknown source namepath.
        let m = parse_maiv(
            b".!maiv\n.!source a/s\n.!target b/t\n/@nodes/*::host=$::nope\n",
        )
        .unwrap();
        assert!(validate_maiv(&m, &src, &tgt).is_err());
        // Required target field produced by nothing.
        let m = parse_maiv(b".!maiv\n.!source a/s\n.!target b/t\n").unwrap();
        assert!(matches!(
            validate_maiv(&m, &src, &tgt),
            Err(PipelineError::App(AppError::IncompleteMapping))
        ));
        // Override constant must satisfy the target constraint.
        let m = parse_maiv(
            b".!maiv\n.!source a/s\n.!target b/t\n::region=$::legacy_region|oops\n",
        )
        .unwrap();
        assert!(validate_maiv(&m, &src, &tgt).is_err());
    }

    #[test]
    fn composition_joins_on_namepaths() {
        let a = parse_maiv(MAP.as_bytes()).unwrap();
        let b = parse_maiv(
            b".!maiv\n.!source hub/nodes\n.!target helm/values\n::fullnameOverride=$/@nodes/0::host\n",
        )
        .unwrap();
        let c = compose(&a, &b).unwrap();
        assert_eq!(c.source, "acme/fleet");
        assert_eq!(c.target, "helm/values");
        // .!via records each hop by its derived edge path.
        assert_eq!(
            c.via,
            ["acme/fleet/mapto/hub/nodes", "hub/nodes/mapto/helm/values"]
        );
        // The concrete index binds through the wildcard hop.
        let rendered = c.render();
        assert!(rendered.contains("::fullnameOverride=$/@servers/0::hostname\n"));
        // Mismatched endpoints refuse to compose.
        assert!(compose(&b, &a).is_err());
    }

    #[test]
    fn same_namespace_edge_path_is_short() {
        let m = parse_maiv(
            b".!maiv\n.!source acme/config-v1\n.!target acme/config-v2\n::a=$::a\n",
        )
        .unwrap();
        // The target's namespace equals the source's: the canonical
        // derived path omits it (3-segment short form).
        assert_eq!(m.edge_path(), "acme/config-v1/mapto/config-v2");
    }

    #[test]
    fn constants_and_doubling() {
        let m = parse_maiv(
            b".!maiv\n.!source a/s\n.!target b/t\n::api_version=v2\n::price=$$5\n",
        )
        .unwrap();
        assert!(matches!(&m.rules[0].rhs, Rhs::Const(c) if c == "v2"));
        assert!(matches!(&m.rules[1].rhs, Rhs::Const(c) if c == "$5"));
        // A literal | in an override constant collides.
        assert!(matches!(
            parse_maiv(b".!maiv\n.!source a/s\n.!target b/t\n::a=$::b|x|y\n"),
            Err(PipelineError::App(AppError::DelimiterCollision))
        ));
    }
}
