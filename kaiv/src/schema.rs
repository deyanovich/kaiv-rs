//! The schema compiler: authored `.saiv` → compiled `.csaiv`
//! (SPEC.md § The Schema Compiler). Lowers named types — core and
//! registry-resolved — to their constraint forms; carries requiredness
//! in the `=`/`?=` operator; propagates the `.!kaivschema` header
//! (including the strict modifier) verbatim.

use crate::anno::{parse_annotation, parse_constraint_items, Annotation, Constraint, Item};
use crate::error::{AppError, PipelineError};
use crate::lexer::{lex, FileKind, LineKind};
use crate::resolve::{resolve_named, Resolver};
use crate::taiv::std_core;
use std::collections::{HashMap, HashSet};

/// Do all `/regex/` bodies in a field's compiled items compile under
/// the pinned dialect? Union alternatives' groups included.
fn patterns_in_dialect(items: &[Item]) -> bool {
    fn group_ok(cs: &[Constraint]) -> bool {
        cs.iter().all(|c| match c {
            Constraint::Pattern(b) => crate::rex::Regex::new(b).is_some(),
            _ => true,
        })
    }
    items.iter().all(|it| match it {
        Item::Constraint(Constraint::Pattern(b)) => crate::rex::Regex::new(b).is_some(),
        Item::Constraint(_) | Item::Named(_) => true,
        Item::Anno(a) => {
            group_ok(&a.constraints) && a.union.iter().all(|alt| group_ok(&alt.constraints))
        }
    })
}

/// Compile with the core-only resolver (embedded `std/core`, no
/// registry configuration).
pub fn compile_schema(input: &[u8]) -> Result<String, PipelineError> {
    compile_schema_with(input, &Resolver::offline())
}

pub fn compile_schema_with(input: &[u8], resolver: &Resolver) -> Result<String, PipelineError> {
    compile_schema_chain(input, resolver, &mut Vec::new())
}

const MAX_INHERIT_DEPTH: usize = 8;

/// Validate a type library: parse, then lower every definition —
/// transitively, through the resolver — so unresolvable or cyclic
/// references are caught at publish time rather than first use.
/// The library's own bytes are preloaded, so same-library `&name`
/// references resolve without a fetch.
pub fn check_type_lib(
    input: &[u8],
    resolver: &Resolver,
) -> Result<crate::taiv::TypeLib, PipelineError> {
    let lib = crate::taiv::parse_taiv(input)?;
    if lib.library.is_empty() {
        return Err(PipelineError::Other(
            ".taiv missing .!kaivtype library identity".into(),
        ));
    }
    resolver.preload(&lib.library, "taiv", input.to_vec());
    for (name, def) in &lib.types {
        let mut b = Buckets::default();
        let mut col = Collected::default();
        bucket_items(&def.items, &lib.library, resolver, &[], &mut b, &mut col, 0)
            .map_err(|e| PipelineError::Other(format!("type &{name}: {e}")))?;
    }
    Ok(lib)
}

fn compile_schema_chain(
    input: &[u8],
    resolver: &Resolver,
    // The `.!schema` references already being compiled, outermost
    // first — revisiting one is a SchemaInheritanceCycleError.
    chain: &mut Vec<String>,
) -> Result<String, PipelineError> {
    let lines = lex(input, FileKind::Schema).map_err(PipelineError::Lex)?;
    let mut out: Vec<String> = Vec::new();
    let mut pending: Option<Annotation> = None;
    let mut ns_prefix: Vec<String> = Vec::new();
    // Segment count pushed by each open namespace block, so a
    // multi-segment `(/a/b)` is popped whole on `()` (mirrors the data
    // compiler's block-wise push/pop).
    let mut ns_depth: Vec<usize> = Vec::new();
    let mut arr_prefix: Option<String> = None;
    let mut imports: Vec<String> = Vec::new();
    let mut unit_imports: Vec<String> = Vec::new();
    let mut registries: Vec<(String, String)> = Vec::new();
    // Inherited-line index (namepath / array path → out position):
    // a field the extending schema redeclares — narrowing, per SPEC.md
    // § Constraint inheritance — replaces the inherited line in place,
    // keeping the base schema's field order for the parallel scan.
    let mut inherited: HashMap<String, usize> = HashMap::new();
    // Namepaths this schema body has itself emitted. Redeclaring an
    // *inherited* field narrows it (allowed); redeclaring one this
    // body already defined is a duplicate (SPEC.md § Schema
    // Compilation Errors).
    let mut local_keys: HashSet<String> = HashSet::new();

    for line in &lines {
        match &line.kind {
            LineKind::Blank | LineKind::Comment(_) | LineKind::Doc(_) => {}
            // Splat lines are authored-`.kaiv`-only; the Data-file
            // lexer never produces them for FileKind::Schema.
            LineKind::VarSplat(_) => unreachable!("var-splat in schema lex"),
            LineKind::Decl(s) => {
                // .!kaivschema (with any strict modifier) passes through;
                // .!types imports and .!registry overrides configure
                // resolution and are resolved away at compile time.
                if s.starts_with(".!kaivschema") || s.starts_with(".!provenance") {
                    // Both propagate verbatim into the .csaiv header —
                    // the Validator reads the compiled artifact.
                    out.push((*s).to_string());
                } else if let Some(rest) = s.strip_prefix(".!schema") {
                    // Schema inheritance: the referenced schema's
                    // compiled lines merge at the declaration's
                    // position — flat, under a namespace qualifier,
                    // or as element lines of an array (SPEC.md
                    // § Encapsulated Hub Schema Extension).
                    let (qualifier, reference) = parse_schema_ref(rest)
                        .ok_or_else(|| PipelineError::Other(format!("bad .!schema: {s}")))?;
                    inherit(
                        &reference,
                        qualifier.as_deref(),
                        resolver,
                        &registries,
                        &mut out,
                        &mut inherited,
                        chain,
                    )?;
                } else if let Some(rest) = s.strip_prefix(".!types") {
                    let lib = rest.trim_matches([' ', '\t']);
                    if !lib.is_empty() {
                        imports.push(lib.to_string());
                    }
                } else if let Some(rest) = s.strip_prefix(".!units") {
                    let lib = rest.trim_matches([' ', '\t']);
                    if !lib.is_empty() {
                        unit_imports.push(lib.to_string());
                    }
                } else if let Some(rest) = s.strip_prefix(".!registry") {
                    if let Some((p, b)) = rest.trim_matches([' ', '\t']).split_once('=') {
                        registries.push((p.to_string(), b.to_string()));
                    }
                }
            }
            LineKind::Meta(s) => {
                if s.starts_with('!') {
                    let a = parse_annotation(s)
                        .ok_or_else(|| PipelineError::Other(format!("bad annotation: {s}")))?;
                    if let Some(u) = &a.unit {
                        // Membership with imports; the Lexer already
                        // covered the no-imports case eagerly.
                        let mut customs = std::collections::BTreeSet::new();
                        for lib in &unit_imports {
                            customs.extend(resolver.unit_names(lib, &registries)?);
                        }
                        if !crate::unit::members_ok(u, &customs) {
                            return Err(PipelineError::Other(format!(
                                "unknown unit '{u}' (not built-in, not defined by any .!units import)"
                            )));
                        }
                    }
                    pending = Some(a);
                } else if let Some(rest) = s.strip_prefix('&') {
                    // `&name` + optional narrowing items, resolved to
                    // the canonical library-path type reference.
                    let end = rest.find([' ', '\t']).unwrap_or(rest.len());
                    let (name, extra) = rest.split_at(end);
                    let type_name = resolve_named(name, &imports, resolver, &registries)?;
                    let mut a = Annotation {
                        type_name,
                        ..Annotation::default()
                    };
                    let extra = extra.trim_matches([' ', '\t']);
                    if !extra.is_empty() {
                        let items = parse_constraint_items(extra).ok_or_else(|| {
                            PipelineError::Other(format!("bad annotation items: {s}"))
                        })?;
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
                    pending = Some(a);
                } else {
                    // A bare constraint line: anonymous refinement of
                    // the next field's implicit `str` type (SPEC.md
                    // § Anonymous Refinement) — the .taiv definition
                    // shape applied to a field. Anything else landing
                    // in Meta (a `?` provenance list, a stray no-`=`
                    // line) has no .saiv meaning and must reject, not
                    // drop: a silently dropped annotation weakens the
                    // compiled contract relative to the authored one.
                    let items = parse_constraint_items(s)
                        .ok_or_else(|| PipelineError::Other(format!("bad constraint line: {s}")))?;
                    let mut a = Annotation {
                        type_name: "str".into(),
                        ..Annotation::default()
                    };
                    for it in items {
                        match it {
                            Item::Constraint(c) => a.constraints.push(c),
                            _ => {
                                return Err(PipelineError::Other(format!(
                                    "only value-constraint items may appear on a bare constraint line: {s}"
                                )))
                            }
                        }
                    }
                    pending = Some(a);
                }
            }
            LineKind::NsOpen(inner) => {
                // A `schema:` composition annotation on a namespace
                // block is DFA composition — unsupported; reject rather
                // than silently drop (matches the data compiler and the
                // module's no-silent-drop policy).
                if inner.split([' ', '\t']).any(|t| t.starts_with("schema:")) {
                    return Err(PipelineError::Other(
                        "namespace-block schema: annotations (DFA composition) are not supported"
                            .into(),
                    ));
                }
                // Quote-aware: a quoted segment may contain whitespace.
                let toks = crate::table::tokens(inner);
                let head = toks.first().copied().unwrap_or("");
                let segs: Vec<String> = head
                    .trim_start_matches('/')
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .map(crate::compiler::normalize_seg)
                    .collect();
                ns_depth.push(segs.len());
                ns_prefix.extend(segs);
            }
            LineKind::NsClose => {
                // Pop the whole block's segments, not just one — a
                // multi-segment `(/a/b)` pushed several.
                if let Some(n) = ns_depth.pop() {
                    ns_prefix.truncate(ns_prefix.len() - n);
                }
            }
            LineKind::SectionOpen(inner) => {
                if arr_prefix.is_some() {
                    return Err(PipelineError::Other(
                        "nested schema section blocks are not supported".into(),
                    ));
                }
                let toks = crate::table::tokens(inner);
                let head = toks.first().copied().unwrap_or("");
                let mut all = ns_prefix.clone();
                all.extend(
                    head.trim_start_matches('/')
                        .split('/')
                        .filter(|s| !s.is_empty())
                        .map(crate::compiler::normalize_seg),
                );
                let arr = format!("/{}", all.join("/"));
                // A table header (Level 2) lowers to the collection
                // constraint line, emitted immediately before the
                // element field definitions (SPEC.md § Table
                // Declarations in the Compiled Schema).
                if toks.len() > 1 {
                    let header = crate::table::parse_header(&toks[1..]).ok_or_else(|| {
                        PipelineError::Other(format!("bad table header: {inner}"))
                    })?;
                    emit(
                        &mut out,
                        &inherited,
                        &arr,
                        format!("{arr} {}", crate::table::render_compiled(&header)),
                    );
                }
                arr_prefix = Some(arr);
            }
            LineKind::SectionClose => {
                arr_prefix = None;
            }
            LineKind::Content { left, value } => {
                // `/@path;=` declares a scalar vector: the annotation
                // constrains every element (compiled `items'/@path::=`).
                if let Some(path) = left.strip_suffix(';') {
                    let a = pending.take().unwrap_or_default();
                    let items = lower(&a, resolver, &registries)?;
                    let mut all = ns_prefix.clone();
                    all.extend(
                        path.trim_start_matches('/')
                            .split('/')
                            .filter(|s| !s.is_empty())
                            .map(crate::compiler::normalize_seg),
                    );
                    let np = format!("/{}::", all.join("/"));
                    check_dup(&inherited, &mut local_keys, &np)?;
                    emit(
                        &mut out,
                        &inherited,
                        &np,
                        format!("{}'{np}=", items.join(" ")),
                    );
                    continue;
                }
                let (key, optional) = match left.strip_suffix('?') {
                    Some(k) => (k, true),
                    None => (left as &str, false),
                };
                let a = pending.take().unwrap_or_default();
                // A map field compiles to a map-entry line: the value
                // type's constraint against the empty-terminal namepath
                // `mapnamespace::` (SPEC.md § Maps in the Compiled
                // Schema); the entry constraint applies to every entry.
                let (items, type_defaults, namepath) = if let Some(arr) = &arr_prefix {
                    // Element-level field of the open section block:
                    // the elided-index namepath `{arr}/::field`. The
                    // name canonicalizes like any other segment — a
                    // quoted bare-shaped spelling (`"re"`, quoted to
                    // clear the authored reserved-word rule) loses its
                    // quotes so data-side namepaths match.
                    let (items, td) = lower_with_defaults(&a, resolver, &registries)?;
                    (
                        items,
                        td,
                        format!("{arr}/::{}", crate::compiler::normalize_seg(key)),
                    )
                } else if a.type_name == "map" {
                    // The map-type grammar admits neither a unit nor
                    // inline constraints (SPEC.md § 10.5) — reject
                    // rather than silently drop them.
                    if a.unit.is_some() || !a.constraints.is_empty() {
                        return Err(PipelineError::Other(
                            "a map annotation admits neither a unit nor inline constraints"
                                .into(),
                        ));
                    }
                    let va = Annotation {
                        type_name: a.map_value.clone().unwrap_or_else(|| "str".into()),
                        ..Annotation::default()
                    };
                    (
                        lower(&va, resolver, &registries)?,
                        Vec::new(),
                        map_namepath(&ns_prefix, key),
                    )
                } else {
                    let (items, td) = lower_with_defaults(&a, resolver, &registries)?;
                    (items, td, field_namepath(&ns_prefix, key))
                };
                // Resolve the default cascade at compile time: the
                // field's own default, then the type chain's, most
                // specific first — the first APPLICABLE one (satisfying
                // the field's compiled constraints) is baked into the
                // .csaiv right side (SPEC.md § Default Values).
                let joined = items.join(" ");
                let parsed = parse_constraint_items(&joined)
                    .ok_or_else(|| PipelineError::Other(format!("bad lowered items: {joined}")))?;
                // Every pattern must sit inside the pinned regex
                // dialect (SPEC.md § Regex dialect) — enforced here so
                // a bad pattern is the schema author's
                // INVALID_CONSTRAINT_ERROR at compile time, not a data
                // consumer's mystery ConstraintViolation at validate.
                if !patterns_in_dialect(&parsed) {
                    return Err(PipelineError::Lex(crate::error::LexErrorAt {
                        error: crate::error::LexError::InvalidConstraint,
                        line: 0,
                    }));
                }
                let resolved = std::iter::once(*value)
                    .chain(type_defaults.iter().map(String::as_str))
                    .chain(std::iter::once(""))
                    .find(|d| crate::validator::default_applicable(&parsed, d));
                // An optional non-collection field whose resolved
                // default is inapplicable and whose type does not
                // admit `!null` leaves the Denormalizer nothing to
                // materialize for an absent instance — rejected here
                // (SPEC.md § Default Values,
                // SchemaOptionalWithoutDefaultError). Map-entry lines
                // (`…::`) are collections: absent means empty, fine.
                if optional
                    && resolved.is_none()
                    && !namepath.ends_with("::")
                    && !admits_null(&parsed)
                {
                    return Err(PipelineError::App(AppError::SchemaOptionalWithoutDefault));
                }
                let default = resolved.unwrap_or("");
                check_dup(&inherited, &mut local_keys, &namepath)?;
                emit(
                    &mut out,
                    &inherited,
                    &namepath,
                    format!(
                        "{}'{}{}={}",
                        joined,
                        namepath,
                        if optional { "?" } else { "" },
                        default
                    ),
                );
            }
        }
    }
    let mut s = out.join("\n");
    if !s.is_empty() {
        s.push('\n');
        // `required`/`source` provenance levels are statically
        // incompatible with optional fields: materialization
        // synthesizes provenance-less lines, so the pipeline could
        // never produce a valid artifact (SPEC.md § Requiring
        // Provenance in Schemas). Checked on the assembled artifact so
        // inherited optional fields are covered too.
        let compiled = crate::validator::parse_csaiv(&s)?;
        if matches!(
            compiled.provenance,
            Some(crate::validator::ProvenanceLevel::Required)
                | Some(crate::validator::ProvenanceLevel::Source)
        ) && compiled.fields.iter().any(|f| f.optional)
        {
            return Err(PipelineError::App(AppError::ProvenanceSchema));
        }
    }
    Ok(s)
}

/// Does the lowered item set admit `!null`? Only a union with a
/// `null` head or alternative does — constraint-only lowerings have
/// no null representation.
fn admits_null(items: &[Item]) -> bool {
    items.iter().any(|i| match i {
        Item::Anno(a) => a.type_name == "null" || a.union.iter().any(|alt| alt.name == "null"),
        _ => false,
    })
}

/// A field redeclared within one schema body — not a narrowing of
/// an inherited field — is a duplicate key (SchemaDuplicateKeyError).
fn check_dup(
    inherited: &HashMap<String, usize>,
    local_keys: &mut HashSet<String>,
    key: &str,
) -> Result<(), PipelineError> {
    if !inherited.contains_key(key) && !local_keys.insert(key.to_string()) {
        return Err(PipelineError::App(AppError::SchemaDuplicateKey));
    }
    Ok(())
}

/// Emit a compiled line: a key already indexed by inheritance is
/// replaced in place (redeclaration narrows the inherited field);
/// anything else appends.
pub(crate) fn emit(
    out: &mut Vec<String>,
    inherited: &HashMap<String, usize>,
    key: &str,
    line: String,
) {
    match inherited.get(key) {
        Some(&i) => out[i] = line,
        None => out.push(line),
    }
}

/// Parse the tail of a `.!schema` declaration into (qualifier,
/// reference): `.!schema hub/x`, `.!schema:hub/x` (flat),
/// `.!schema:/ns hub/x` (encapsulated), `.!schema:/@arr hub/x`
/// (array-element).
pub(crate) fn parse_schema_ref(rest: &str) -> Option<(Option<String>, String)> {
    let (qual, r) = if let Some(c) = rest.strip_prefix(':') {
        if c.starts_with('/') {
            let mut it = c.splitn(2, [' ', '\t']);
            let q = it.next()?;
            (Some(q.to_string()), it.next().unwrap_or(""))
        } else {
            (None, c)
        }
    } else {
        (None, rest)
    };
    let r = r.trim_matches([' ', '\t']);
    if r.is_empty() {
        return None;
    }
    Some((qual, r.to_string()))
}

/// Resolve a `.!schema` reference and merge its compiled lines into
/// `out`, indexing their keys so the extending schema's own
/// declarations can narrow them in place.
fn inherit(
    reference: &str,
    qualifier: Option<&str>,
    resolver: &Resolver,
    layer1: &[(String, String)],
    out: &mut Vec<String>,
    inherited: &mut HashMap<String, usize>,
    chain: &mut Vec<String>,
) -> Result<(), PipelineError> {
    // A `.!schema` chain that revisits a schema already being
    // compiled is a cycle (SPEC.md § Errors).
    if chain.iter().any(|r| r == reference) {
        return Err(PipelineError::App(AppError::SchemaInheritanceCycle));
    }
    if chain.len() >= MAX_INHERIT_DEPTH {
        return Err(PipelineError::Other("schema inheritance too deep".into()));
    }
    if reference.starts_with("http://") || reference.starts_with("https://") {
        // URL references are network resolution — unimplemented
        // offline, like http(s) registry bases.
        return Err(PipelineError::App(AppError::SchemaResolution));
    }
    let bytes = resolver.schema_bytes(reference, layer1)?;
    chain.push(reference.to_string());
    let compiled = compile_schema_chain(&bytes, resolver, chain);
    chain.pop();
    let compiled = compiled?;
    let element_wise =
        qualifier.is_some_and(|q| q.split('/').next_back().is_some_and(|s| s.starts_with('@')));
    for line in compiled.lines() {
        // Only field and collection lines inherit; the extending
        // schema's own header (.!kaivschema, .!provenance) governs.
        if line.is_empty() || line.starts_with(".!") || line.starts_with(".?") {
            continue;
        }
        let line = match qualifier {
            None => line.to_string(),
            Some(arr) if element_wise => element_line(line, arr)?,
            Some(ns) => reprefix(line, ns)?,
        };
        let key = line_key(&line);
        emit(out, inherited, &key, line.clone());
        inherited.entry(key).or_insert(out.len() - 1);
    }
    Ok(())
}

/// The override key of a compiled line: the namepath of a field /
/// map / vector line, or the array path of a collection line.
pub(crate) fn line_key(line: &str) -> String {
    match line.find('\'') {
        Some(t) => {
            let rest = &line[t + 1..];
            let eq = crate::validator::first_eq(rest).unwrap_or(rest.len());
            let lhs = &rest[..eq];
            lhs.strip_suffix('?').unwrap_or(lhs).to_string()
        }
        None => line.split([' ', '\t']).next().unwrap_or(line).to_string(),
    }
}

/// Scope an inherited compiled line under a namespace: the namepath
/// gains the prefix; a collection line's array path and foreign-key
/// targets are re-anchored with it.
pub(crate) fn reprefix(line: &str, ns: &str) -> Result<String, PipelineError> {
    match line.find('\'') {
        Some(t) => Ok(format!("{}'{ns}{}", &line[..t], &line[t + 1..])),
        None => {
            let (arr, clauses) = line
                .split_once([' ', '\t'])
                .ok_or_else(|| PipelineError::Other(format!("bad inherited line: {line}")))?;
            let mut h = crate::table::parse_compiled(clauses)
                .ok_or_else(|| PipelineError::Other(format!("bad inherited line: {line}")))?;
            for c in h.groups.iter_mut().flatten() {
                if let crate::table::Clause::Ref { target_arr, .. } = c {
                    *target_arr = format!("{ns}{target_arr}");
                }
            }
            Ok(format!("{ns}{arr} {}", crate::table::render_compiled(&h)))
        }
    }
}

/// Turn an inherited root scalar field line into an element-level
/// line of `arr` (`!str'::host=` → `!str'/@arr/::host=`). Deeper
/// structure is not expressible in the element-level compiled subset.
pub(crate) fn element_line(line: &str, arr: &str) -> Result<String, PipelineError> {
    let not_element = || {
        PipelineError::Other(format!(
            "only root scalar fields extend array elements: {line}"
        ))
    };
    let t = line.find('\'').ok_or_else(not_element)?;
    let rest = &line[t + 1..];
    let eq = crate::validator::first_eq(rest).ok_or_else(not_element)?;
    let lhs = rest[..eq].strip_suffix('?').unwrap_or(&rest[..eq]);
    let field = lhs.strip_prefix("::").ok_or_else(not_element)?;
    if field.is_empty() || field.contains('/') {
        return Err(not_element());
    }
    Ok(format!("{}'{arr}/{rest}", &line[..t]))
}

fn field_namepath(prefix: &[String], key: &str) -> String {
    let (steps, field) = crate::compiler::split_namepath(key);
    let mut all = prefix.to_vec();
    all.extend(steps);
    if all.is_empty() {
        format!("::{field}")
    } else {
        format!("/{}::{}", all.join("/"), field)
    }
}

/// A map's entries live *under* the field: the entry namepath is the
/// map namespace with an elided (empty) terminal — `/config/settings::`.
fn map_namepath(prefix: &[String], key: &str) -> String {
    let (steps, field) = crate::compiler::split_namepath(key);
    let mut all = prefix.to_vec();
    all.extend(steps);
    all.push(field);
    format!("/{}::", all.join("/"))
}

/// Lower a type annotation to `.csaiv` constraint items, in the
/// canonical order pattern, span, range/enum, length. Unconstrained
/// strings stay `!str`; unions pass through as a type item. Named
/// types — `std/core` and registry-resolved — lower transitively
/// through their `.taiv` definitions.
fn lower(
    a: &Annotation,
    resolver: &Resolver,
    layer1: &[(String, String)],
) -> Result<Vec<String>, PipelineError> {
    Ok(lower_with_defaults(a, resolver, layer1)?.0)
}

/// What the definition walk collects besides constraint buckets.
#[derive(Default)]
struct Collected {
    /// Type-default cascade, most-specific first.
    defaults: Vec<String>,
    /// Unit from the most specific carrying level (canonical form).
    unit: Option<String>,
    /// Does the type chain derive from `b64`? (Length constraints
    /// then translate to encoded-character bounds.)
    b64: bool,
}

/// Lower an annotation and collect the type-default cascade
/// encountered along the definition walk, most-specific first.
fn lower_with_defaults(
    a: &Annotation,
    resolver: &Resolver,
    layer1: &[(String, String)],
) -> Result<(Vec<String>, Vec<String>), PipelineError> {
    if !a.union.is_empty() {
        // A unit annotation is exclusive with a union type in the
        // grammar; the union lowering has no place to carry it, so
        // reject rather than silently drop it.
        if a.unit.is_some() {
            return Err(PipelineError::Other(format!(
                "unit annotation on a union type: !{}",
                a.type_name
            )));
        }
        // Union: the type names survive as the discriminant, and each
        // alternative carries its lowered definition + narrowing as a
        // whitespace-free parenthesized group, so the union stays one
        // item token (SPEC.md § Tagged unions).
        let mut u = String::from("!");
        u.push_str(&render_union_alt(
            &a.type_name,
            &a.constraints,
            resolver,
            layer1,
        )?);
        for alt in &a.union {
            u.push('|');
            u.push_str(&render_union_alt(
                &alt.name,
                &alt.constraints,
                resolver,
                layer1,
            )?);
        }
        return Ok((vec![u], Vec::new()));
    }
    let mut b = Buckets::default();
    let mut col = Collected::default();
    bucket_annotation(a, resolver, layer1, &mut b, &mut col, 0)?;
    // Units annotate only numeric (`..num`-derived) types (SPEC.md
    // § Units on Types); a unit on any other type is a compile error,
    // not a silently-accepted `!str:km`.
    if col.unit.is_some()
        && !b
            .spans
            .iter()
            .any(|s| s == "..num" || s.starts_with("..num"))
    {
        return Err(PipelineError::Other(format!(
            "unit annotation on a non-numeric type: !{}",
            a.type_name
        )));
    }
    let mut items = b.into_items(col.b64);
    // A unit-carrying type retains its token first — the unit is not
    // captured by any value constraint, and the Validator
    // byte-compares it (SPEC.md § Validation: units do not convert).
    if let Some(u) = &col.unit {
        items.insert(0, format!("!{}:{u}", a.type_name));
    }
    if items.is_empty() {
        items.push("!str".into());
    }
    // A field line may not begin with `#` (rule 2 → comment) or `//`
    // (an empty pattern `//`, or empty `re` body, renders leading
    // `//`, which parse_csaiv also skips as a comment). In either case
    // the retained `!str` type item is emitted first so the compiled
    // line re-lexes as a field rather than being swallowed (SPEC.md
    // § The Schema Compiler).
    if items[0].starts_with('#') || items[0].starts_with("//") {
        items.insert(0, "!str".into());
    }
    Ok((items, col.defaults))
}

/// One union alternative in `.csaiv` form: the type name, plus a
/// `(…)` group of its lowered definition + narrowing constraints —
/// concatenated without whitespace (the lowered items are
/// self-delimiting). An alternative with no constraints stays bare.
pub(crate) fn render_union_alt(
    name: &str,
    narrowing: &[Constraint],
    resolver: &Resolver,
    layer1: &[(String, String)],
) -> Result<String, PipelineError> {
    let pseudo = Annotation {
        type_name: name.to_string(),
        constraints: narrowing.to_vec(),
        ..Annotation::default()
    };
    let mut b = Buckets::default();
    let mut col = Collected::default();
    bucket_annotation(&pseudo, resolver, layer1, &mut b, &mut col, 0)?;
    let items = b.into_items(col.b64);
    if items.is_empty() {
        Ok(name.to_string())
    } else {
        Ok(format!("{name}({})", items.concat()))
    }
}

/// Canonical `.csaiv` item buckets: pattern, span, range/enum, length.
#[derive(Default)]
struct Buckets {
    patterns: Vec<String>,
    spans: Vec<String>,
    ranges: Vec<String>,
    lengths: Vec<Constraint>,
}

impl Buckets {
    fn add(&mut self, c: &Constraint) {
        match c {
            Constraint::Pattern(_) => self.patterns.push(render(c)),
            Constraint::Span(_) => self.spans.push(render(c)),
            Constraint::Range(..) | Constraint::Enum(_) => self.ranges.push(render(c)),
            Constraint::Length(_) => self.lengths.push(c.clone()),
        }
    }

    /// Render, translating b64 decoded-byte lengths into encoded-
    /// character lengths — exact for unpadded base64url — so the
    /// Validator's counting stays character-based.
    fn into_items(self, b64: bool) -> Vec<String> {
        let mut items = self.patterns;
        items.extend(self.spans);
        items.extend(self.ranges);
        for c in &self.lengths {
            let c = if b64 {
                translate_b64_length(c)
            } else {
                c.clone()
            };
            items.push(render(&c));
        }
        items
    }
}

/// Encoded length of n bytes in unpadded base64url.
fn b64_chars(n: u64) -> u64 {
    4 * (n / 3) + [0, 2, 3][(n % 3) as usize]
}

fn translate_b64_length(c: &Constraint) -> Constraint {
    let tr = |s: &Option<String>| {
        s.as_ref()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|n| b64_chars(n).to_string())
    };
    match c {
        Constraint::Length(inner) => Constraint::Length(Box::new(match &**inner {
            Constraint::Range(lo, hi) => Constraint::Range(tr(lo), tr(hi)),
            Constraint::Enum(vs) => Constraint::Enum(
                vs.iter()
                    .map(|v| {
                        v.parse::<u64>()
                            .map(|n| b64_chars(n).to_string())
                            .unwrap_or_else(|_| v.clone())
                    })
                    .collect(),
            ),
            other => other.clone(),
        })),
        other => other.clone(),
    }
}

const MAX_LOWER_DEPTH: usize = 32;

/// Bucket a type annotation: the named type's own definition first
/// (transitively), then the annotation's inline constraints. Type
/// defaults encountered along the walk accumulate into `defaults`,
/// most-specific first — the cascade the compiler resolves.
fn bucket_annotation(
    a: &Annotation,
    resolver: &Resolver,
    layer1: &[(String, String)],
    b: &mut Buckets,
    col: &mut Collected,
    depth: usize,
) -> Result<(), PipelineError> {
    if depth > MAX_LOWER_DEPTH {
        return Err(PipelineError::Other(
            "type-definition recursion too deep (cycle?)".into(),
        ));
    }
    if let Some(u) = &a.unit {
        // Most specific carrying level wins (the field's annotation
        // is walked before its type chain).
        if col.unit.is_none() {
            col.unit =
                Some(crate::unit::canonicalize(u).ok_or_else(|| {
                    PipelineError::Other(format!("invalid unit expression: {u}"))
                })?);
        }
    }
    match a.type_name.as_str() {
        // The identity type contributes nothing; maps are handled
        // before lowering (map_namepath entry lines).
        "" | "str" | "map" => {}
        core if std_core().types.contains_key(core) => {
            if core == "b64" {
                col.b64 = true;
            }
            let def = std_core().types[core].clone();
            col.defaults.push(def.default.clone());
            bucket_items(&def.items, "std/core", resolver, layer1, b, col, depth + 1)?;
        }
        path if path.contains('/') => {
            let (lib, name) = path.rsplit_once('/').unwrap();
            let def = resolver.def(lib, name, layer1)?;
            col.defaults.push(def.default.clone());
            bucket_items(&def.items, lib, resolver, layer1, b, col, depth + 1)?;
        }
        other => {
            return Err(PipelineError::Other(format!(
                "unresolvable type in schema: !{other}"
            )))
        }
    }
    for c in &a.constraints {
        b.add(c);
    }
    Ok(())
}

/// Bucket a `.taiv` definition's items; `lib` scopes same-library
/// `&name` references.
fn bucket_items(
    items: &[Item],
    lib: &str,
    resolver: &Resolver,
    layer1: &[(String, String)],
    b: &mut Buckets,
    col: &mut Collected,
    depth: usize,
) -> Result<(), PipelineError> {
    if depth > MAX_LOWER_DEPTH {
        return Err(PipelineError::Other(
            "type-definition recursion too deep (cycle?)".into(),
        ));
    }
    for it in items {
        match it {
            Item::Constraint(c) => b.add(c),
            Item::Anno(base) => bucket_annotation(base, resolver, layer1, b, col, depth + 1)?,
            Item::Named(n) => {
                let sub = resolver.def(lib, n, layer1)?;
                col.defaults.push(sub.default.clone());
                bucket_items(&sub.items, lib, resolver, layer1, b, col, depth + 1)?;
            }
        }
    }
    Ok(())
}

fn render(c: &Constraint) -> String {
    match c {
        Constraint::Pattern(b) => format!("/{b}/"),
        Constraint::Range(lo, hi) => format!(
            "[{},{}]",
            lo.as_deref().unwrap_or(""),
            hi.as_deref().unwrap_or("")
        ),
        Constraint::Enum(vs) => format!("{{{}}}", vs.join(",")),
        Constraint::Length(inner) => format!("#{}", render(inner)),
        Constraint::Span(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_segment_ns_block_scopes_symmetrically() {
        // `(/a/b) ... ()` must pop both segments; a field after the
        // close scopes at root, not the stale `/a`.
        let out = compile_schema(b".!kaivschema 1 a/n\n(/a/b)\nhost=\n()\nport=\n").unwrap();
        assert!(out.contains("'/a/b::host="), "got: {out}");
        assert!(out.contains("'::port="), "got: {out}");
        assert!(!out.contains("'/a::port="), "stale prefix leaked: {out}");
    }

    #[test]
    fn ns_block_schema_annotation_is_rejected() {
        assert!(compile_schema(b".!kaivschema 1 a/n\n(/server schema:hub/x)\nhost=\n()\n").is_err());
    }

    #[test]
    fn unit_on_union_type_is_rejected() {
        assert!(compile_schema(b".!kaivschema 1 a/u\n!int:s|null\ntimeout=\n").is_err());
    }

    #[test]
    fn strict_provenance_levels_reject_optional_fields() {
        // Materialized lines carry no provenance, so required/source
        // plus an optional field can never validate — reject statically.
        assert!(compile_schema(
            b".!kaivschema 1 a/p\n.!provenance:required\n!str\nhost=\ntimeout?=5\n"
        )
        .is_err());
        assert!(compile_schema(
            b".!kaivschema 1 a/p\n.!provenance:source\n!str\ntimeout?=5\n"
        )
        .is_err());
        // Required-only fields, or the none level, are fine.
        assert!(
            compile_schema(b".!kaivschema 1 a/p\n.!provenance:required\n!str\nhost=\n").is_ok()
        );
        assert!(compile_schema(b".!kaivschema 1 a/p\n.!provenance:none\n!str\ntimeout?=5\n").is_ok());
    }

    #[test]
    fn unit_or_constraints_on_map_type_are_rejected() {
        assert!(compile_schema(b".!kaivschema 1 a/m\n!map<int>:km\nsettings=\n").is_err());
        assert!(compile_schema(b".!kaivschema 1 a/m\n!map<int>[1,5]\nsettings=\n").is_err());
        assert!(compile_schema(b".!kaivschema 1 a/m\n!map<int>\nsettings=\n").is_ok());
    }

    #[test]
    fn empty_pattern_field_is_not_swallowed_as_comment() {
        let out = compile_schema(b".!kaivschema 1 a/p\n!str//\nname=\n").unwrap();
        for line in out.lines() {
            assert!(
                !line.trim_start().starts_with("//"),
                "field emitted as comment: {out}"
            );
        }
        assert_eq!(crate::validator::parse_csaiv(&out).unwrap().fields.len(), 1);
    }

    #[test]
    fn schema_ref_forms() {
        let p = parse_schema_ref;
        assert_eq!(p(" hub/x"), Some((None, "hub/x".into())));
        assert_eq!(p(":hub/x"), Some((None, "hub/x".into())));
        assert_eq!(p(":/ns hub/x"), Some((Some("/ns".into()), "hub/x".into())));
        assert_eq!(
            p(":/@arr hub/x"),
            Some((Some("/@arr".into()), "hub/x".into()))
        );
        assert_eq!(p(""), None);
        assert_eq!(p(":/ns"), None); // qualifier without a reference
    }

    /// A resolver whose base lookups all miss on the filesystem —
    /// keeps these tests off the Layer 4 network hosts.
    fn dead_end() -> Resolver {
        let mut config = crate::config::Config::default();
        config
            .registries
            .insert("default".into(), "/nonexistent/kaiv-test".into());
        Resolver::new(config)
    }

    #[test]
    fn check_type_lib_lowers_all_definitions() {
        // Same-library &name reference and a core base type both
        // resolve without any external fetch.
        let ok = b".!kaivtype 1 acme/net\n\n!int[1,65535]\n&port=\n\n&port [80,443]\n&webport=\n";
        let lib = check_type_lib(ok, &dead_end()).unwrap();
        assert_eq!(lib.library, "acme/net");
        assert_eq!(lib.types.len(), 2);

        // A cross-library base reference to an unpublished library
        // fails, and the missing artifact is recorded for the host.
        let dangling = b".!kaivtype 1 acme/net\n\n!other/lib/base\n&derived=\n";
        let r = dead_end();
        assert!(check_type_lib(dangling, &r).is_err());
        assert_eq!(
            r.take_missing(),
            vec![("other/lib".to_string(), "taiv".to_string())]
        );

        // Identity is required.
        assert!(check_type_lib(b"&x=\n", &dead_end()).is_err());
    }

    #[test]
    fn unit_on_non_numeric_type_is_rejected() {
        assert!(compile_schema(b".!kaivschema 1 a/u\n!str:km\ndist=\n").is_err());
        // A numeric type is fine.
        assert!(compile_schema(b".!kaivschema 1 a/u\n!float:km\ndist=\n").is_ok());
    }

    #[test]
    fn bare_constraint_line_refines_implicit_str() {
        // Anonymous refinement (SPEC.md § Anonymous Refinement):
        // the .taiv definition shape above a field — implicit str,
        // lowered to a bare constraint group like !str + items.
        let csaiv = compile_schema(
            b".!kaivschema 1 a/c\n/^[a-z]+$/ #[1,8]\nname=\n..lex [aa,mm]\nbucket=\n",
        )
        .unwrap();
        assert_eq!(
            csaiv,
            ".!kaivschema 1 a/c\n/^[a-z]+$/ #[1,8]'::name=\n..lex [aa,mm]'::bucket=\n"
        );
    }

    #[test]
    fn uninterpretable_schema_meta_lines_reject() {
        // A type-reference item has its own line forms; a `?`
        // provenance list and a stray no-`=` line have no .saiv
        // meaning. All reject rather than silently dropping.
        assert!(compile_schema(b".!kaivschema 1 a/c\n/^[a-z]+$/ &port\nname=\n").is_err());
        assert!(compile_schema(b".!kaivschema 1 a/c\n?required\nname=\n").is_err());
        assert!(compile_schema(b".!kaivschema 1 a/c\nstray words\nname=\n").is_err());
    }

    #[test]
    fn duplicate_schema_field_is_an_error() {
        assert_eq!(
            compile_schema(b".!kaivschema 1 a/d\nhost=\nhost=\n"),
            Err(PipelineError::App(AppError::SchemaDuplicateKey))
        );
    }

    #[test]
    fn url_reference_is_resolution_error() {
        // Network layers are unimplemented offline (SPEC.md § Type
        // Registry Resolution) — URL references fail loudly.
        let saiv = b".!kaivschema 1 acme/x\n.!schema https://example.org/base.saiv\n";
        assert_eq!(
            compile_schema(saiv),
            Err(PipelineError::App(AppError::SchemaResolution))
        );
    }

    #[test]
    fn element_lines_reject_structure() {
        // Only root scalar fields extend array elements.
        assert!(element_line("!str'/deep::f=", "/@a").is_err());
        assert!(element_line("!str'/m::=", "/@a").is_err()); // map line
        assert!(element_line("/@x [min=1]", "/@a").is_err()); // collection
        assert_eq!(
            element_line("!str'::host?=", "/@a").unwrap(),
            "!str'/@a/::host?="
        );
    }
}
