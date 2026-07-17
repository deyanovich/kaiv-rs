//! The Validator: parallel scan of `.daiv` against `.csaiv`
//! (SPEC.md § Parallel Scan Validation, § Validator Pseudocode).
//! Constant-memory in spirit: one pass, one schema pointer, plus the
//! duplicate-detection set.

use crate::anno::{parse_annotation, parse_constraint_items, Constraint, Item};
use crate::error::{AppError, AppErrorAt, PipelineError};
use crate::resolve::Resolver;
use crate::rex::Regex;
use crate::table::{Clause, Collection};
use std::collections::{BTreeMap, HashMap, HashSet};

pub struct CompiledSchema {
    pub strict: bool,
    /// `.!provenance:LEVEL` requirement, propagated from the header.
    pub provenance: Option<ProvenanceLevel>,
    pub fields: Vec<SchemaField>,
    /// Level 2 collection constraint lines, checked by Pass 2.
    pub collections: Vec<Collection>,
}

/// The three `.!provenance` requirement levels (SPEC.md § Requiring
/// Provenance in Schemas). `#dpid` is never constrained. There is no
/// timestamp-only level: the grammar anchors every provenance entry
/// to its source id, so "timestamp required" is exactly `required`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceLevel {
    /// Both `?sourceID` and `@timestamp` on every data line.
    Required,
    /// `?sourceID` required; `@timestamp` optional.
    Source,
    /// Provenance prohibited.
    None,
}

pub struct SchemaField {
    pub items: Vec<Item>,
    pub namepath: String,
    pub optional: bool,
    /// The compile-time-resolved applicable default (often empty).
    /// The Validator never materializes it; it rides the `.csaiv` for
    /// consumers (SPEC.md § Default Values).
    pub default: String,
    /// The raw `.csaiv` metadata prefix (everything before `'`),
    /// verbatim — carried for error context, never re-parsed.
    pub prefix: String,
}

pub fn parse_csaiv(text: &str) -> Result<CompiledSchema, PipelineError> {
    let mut strict = false;
    let mut provenance = None;
    let mut fields = Vec::new();
    let mut collections = Vec::new();
    for raw in text.lines() {
        let s = raw.trim_start_matches([' ', '\t']);
        if s.is_empty() || s.starts_with('#') || s.starts_with("//") {
            continue;
        }
        if s.starts_with(".!") || s.starts_with(".?") {
            if s.starts_with(".!kaivschema") && s.split([' ', '\t']).any(|t| t == "strict") {
                strict = true;
            }
            if let Some(level) = s.strip_prefix(".!provenance:") {
                provenance = Some(match level.trim_matches([' ', '\t']) {
                    "required" => ProvenanceLevel::Required,
                    "source" => ProvenanceLevel::Source,
                    "none" => ProvenanceLevel::None,
                    other => {
                        return Err(PipelineError::Other(format!(
                            "unknown provenance level: {other}"
                        )))
                    }
                });
            }
            continue;
        }
        // Collection constraint lines (Level 2) have no `'` — an
        // array namepath followed by bracket clauses.
        let Some(tick) = find_tick(s) else {
            if let Some((array, clauses)) = s.split_once([' ', '\t']) {
                if array.starts_with('/') {
                    if let Some(header) =
                        crate::table::parse_compiled(clauses.trim_matches([' ', '\t']))
                    {
                        collections.push(Collection {
                            array: array.to_string(),
                            header,
                        });
                        continue;
                    }
                }
            }
            return Err(PipelineError::Other(format!(
                "unsupported .csaiv line (no ' delimiter): {s}"
            )));
        };
        let (citems, rest) = (&s[..tick], &s[tick + 1..]);
        let eq = first_eq(rest)
            .ok_or_else(|| PipelineError::Other(format!("missing = on .csaiv line: {s}")))?;
        let (lhs, default) = (&rest[..eq], &rest[eq + 1..]);
        let (namepath, optional) = match lhs.strip_suffix('?') {
            Some(p) => (p, true),
            None => (lhs, false),
        };
        let items = parse_constraint_items(citems)
            .ok_or_else(|| PipelineError::Other(format!("bad constraint items: {citems}")))?;
        fields.push(SchemaField {
            items,
            namepath: namepath.to_string(),
            optional,
            default: default.to_string(),
            prefix: citems.to_string(),
        });
    }
    Ok(CompiledSchema {
        strict,
        provenance,
        fields,
        collections,
    })
}

/// First `'` outside quoted names — quoted names use `""`, never `''`,
/// so the first `'` is always the delimiter (SPEC.md splitting rule).
fn find_tick(s: &str) -> Option<usize> {
    s.find('\'')
}

/// First `=` outside quoted names.
pub(crate) fn first_eq(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut q = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                if q && b.get(i + 1) == Some(&b'"') {
                    i += 1;
                } else {
                    q = !q;
                }
            }
            b'=' if !q => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Would `value` satisfy the field's compiled constraints? Used by
/// the schema compiler to resolve the default cascade — a default is
/// applicable only if it satisfies the field's own constraints.
pub(crate) fn default_applicable(items: &[Item], value: &str) -> bool {
    let span = items
        .iter()
        .find_map(|i| match i {
            Item::Constraint(Constraint::Span(sp)) => Some(sp.as_str()),
            _ => None,
        })
        .unwrap_or("..lex");
    for item in items {
        match item {
            Item::Constraint(c) => {
                if check_constraint(c, span, value).is_err() {
                    return false;
                }
            }
            Item::Anno(a) if a.union.is_empty() => {
                // Retained type item (!str): its constraints, if any.
                for c in &a.constraints {
                    if check_constraint(c, span, value).is_err() {
                        return false;
                    }
                }
            }
            Item::Anno(a) => {
                // Union: some alternative's group must accept it.
                if !group_ok(&a.constraints, value)
                    && !a.union.iter().any(|alt| group_ok(&alt.constraints, value))
                {
                    return false;
                }
            }
            Item::Named(_) => {}
        }
    }
    true
}

/// Does one union alternative's constraint group (with its own span)
/// accept the value?
fn group_ok(cs: &[Constraint], value: &str) -> bool {
    let gspan = cs
        .iter()
        .find_map(|c| match c {
            Constraint::Span(sp) => Some(sp.as_str()),
            _ => None,
        })
        .unwrap_or("..lex");
    cs.iter().all(|c| check_constraint(c, gspan, value).is_ok())
}

/// The union alternative (head first, then declaration order) whose
/// constraint group accepts `value` — the type name the Denormalizer
/// stamps on a materialized line (SPEC.md § Null Semantics,
/// Materialization of Absent Fields).
pub(crate) fn union_pick<'a>(a: &'a crate::anno::Annotation, value: &str) -> Option<&'a str> {
    if group_ok(&a.constraints, value) {
        return Some(&a.type_name);
    }
    a.union
        .iter()
        .find(|alt| group_ok(&alt.constraints, value))
        .map(|alt| alt.name.as_str())
}

/// Resolve a canonical document's `.!schema` declarations into one
/// merged `CompiledSchema` — allOf composition (SPEC.md § Schema
/// Composition): every reference's compiled `.csaiv` lines
/// contribute, flat at root or transformed by the declaration's
/// namespace / array qualifier. `None` when the document declares no
/// schema. URL references are network resolution — unimplemented
/// offline, `SchemaResolutionError` — and qualified references
/// contribute field lines only (their headers' strictness is scoped
/// semantics the flat merge cannot express).
pub fn schema_for_daiv(
    daiv: &str,
    resolver: &Resolver,
) -> Result<Option<CompiledSchema>, PipelineError> {
    let mut layer1: Vec<(String, String)> = Vec::new();
    let mut refs: Vec<(Option<String>, String)> = Vec::new();
    for raw in daiv.lines() {
        let s = raw.trim_start_matches([' ', '\t']);
        if let Some(rest) = s.strip_prefix(".!schema") {
            let parsed = crate::schema::parse_schema_ref(rest)
                .ok_or_else(|| PipelineError::Other(format!("bad .!schema: {s}")))?;
            refs.push(parsed);
        } else if let Some(rest) = s.strip_prefix(".!registry") {
            if let Some((p, b)) = rest.trim_matches([' ', '\t']).split_once('=') {
                layer1.push((p.to_string(), b.to_string()));
            }
        }
    }
    if refs.is_empty() {
        return Ok(None);
    }
    let mut out: Vec<String> = Vec::new();
    let mut merged: HashMap<String, usize> = HashMap::new();
    for (qualifier, reference) in &refs {
        if reference.starts_with("http://") || reference.starts_with("https://") {
            return Err(PipelineError::App(AppError::SchemaResolution));
        }
        let bytes = resolver.csaiv_bytes(reference, &layer1)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| PipelineError::Other(format!("{reference}.csaiv is not UTF-8")))?;
        let element_wise = qualifier
            .as_deref()
            .is_some_and(|q| q.split('/').next_back().is_some_and(|s| s.starts_with('@')));
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            if line.starts_with(".!") || line.starts_with(".?") {
                if qualifier.is_none() {
                    out.push(line.to_string());
                }
                continue;
            }
            let line = match qualifier.as_deref() {
                None => line.to_string(),
                Some(arr) if element_wise => crate::schema::element_line(line, arr)?,
                Some(ns) => crate::schema::reprefix(line, ns)?,
            };
            let key = crate::schema::line_key(&line);
            crate::schema::emit(&mut out, &merged, &key, line);
            merged.entry(key).or_insert(out.len() - 1);
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    parse_csaiv(&text).map(Some)
}

struct DataLine {
    type_name: String,
    unit: Option<String>,
    provenance: Option<String>,
    namepath: String,
    value: String,
    /// 1-based source line in the `.daiv` input, for error context.
    line: usize,
}

fn parse_daiv(text: &str) -> Result<Vec<DataLine>, PipelineError> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let s = raw.trim_start_matches([' ', '\t']);
        if s.is_empty() || s.starts_with('#') || s.starts_with("//") {
            continue;
        }
        if s.starts_with(".!") || s.starts_with(".?") {
            continue;
        }
        let line = i + 1;
        let tick = find_tick(s).ok_or_else(|| {
            PipelineError::Other(format!("canonical line without ' (line {line}): {s}"))
        })?;
        let prefix = &s[..tick];
        let rest = &s[tick + 1..];
        // Quote-aware: a quoted name may contain `=`.
        let eq = first_eq(rest).ok_or_else(|| {
            PipelineError::Other(format!("canonical line without = (line {line}): {s}"))
        })?;
        let a = parse_annotation(prefix).ok_or_else(|| {
            PipelineError::Other(format!("bad metadata prefix (line {line}): {prefix}"))
        })?;
        out.push(DataLine {
            type_name: a.type_name,
            unit: a.unit,
            provenance: a.provenance,
            namepath: rest[..eq].to_string(),
            value: rest[eq + 1..].to_string(),
            line,
        });
    }
    Ok(out)
}

/// Attach failure-site context to an [`AppError`].
fn at(error: AppError, line: usize, context: String) -> AppErrorAt {
    AppErrorAt {
        error,
        line,
        context,
    }
}

/// Context for a per-field check failure: the data line against the
/// schema line's verbatim constraint prefix. Values are elided past
/// 40 characters (on a char boundary) — context, not payload.
fn field_ctx(error: AppError, f: &SchemaField, d: &DataLine) -> AppErrorAt {
    let mut value: String = d.value.chars().take(40).collect();
    if value.len() < d.value.len() {
        value.push('…');
    }
    let unit = d.unit.as_deref().map(|u| format!(":{u}")).unwrap_or_default();
    let verb = match error {
        AppError::TypeMismatch => "does not satisfy",
        AppError::CollationUnsupported => "needs a collation unavailable in",
        _ => "violates",
    };
    at(
        error,
        d.line,
        format!(
            "{}={value} (type !{}{unit}) {verb} {}",
            d.namepath, d.type_name, f.prefix
        ),
    )
}

pub fn validate(daiv: &str, schema: &CompiledSchema) -> Result<(), AppErrorAt> {
    let data = match parse_daiv(daiv) {
        Ok(d) => d,
        Err(e) => {
            let detail = match e {
                PipelineError::Other(s) => s,
                other => other.to_string(),
            };
            return Err(at(AppError::ConstraintViolation, 0, detail));
        }
    };
    // Provenance is a per-line requirement (SPEC.md § Provenance)
    // orthogonal to the field-matching scan, so enforce it up front
    // over every parsed line. The main scan below consumes namespace-
    // array element runs inside validate_ns_array without revisiting
    // them, so an in-loop check would miss every element line past the
    // first.
    if let Some(level) = schema.provenance {
        for d in &data {
            check_provenance(level, d.provenance.as_deref()).map_err(|e| {
                let want = match level {
                    ProvenanceLevel::Required => "requires source and timestamp on every line",
                    ProvenanceLevel::Source => "requires a source on every line",
                    ProvenanceLevel::None => "prohibits provenance",
                };
                at(e, d.line, format!("{}: schema {want}", d.namepath))
            })?;
        }
    }
    let defined: HashSet<&str> = schema.fields.iter().map(|f| f.namepath.as_str()).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut si = 0usize;
    let mut di = 0usize;

    while di < data.len() {
        let d = &data[di];
        if defined.contains(d.namepath.as_str()) && !seen.insert(d.namepath.as_str()) {
            return Err(at(
                AppError::DuplicateKeySchema,
                d.line,
                format!("second entry for schema field {}", d.namepath),
            ));
        }
        // An undefined data line — matching no schema line at all —
        // must not consume the schema pointer: relaxed schemas MAY
        // interleave undefined fields anywhere (SPEC.md § Errors,
        // strict-vs-relaxed), and the defined fields that follow
        // still have to find their schema lines.
        if !schema.fields.iter().any(|f| line_matches(f, &d.namepath)) {
            if schema.strict {
                return Err(at(
                    AppError::UndefinedFieldStrictSchema,
                    d.line,
                    format!("{} is not defined in the strict schema", d.namepath),
                ));
            }
            di += 1;
            continue;
        }
        while si < schema.fields.len() && !line_matches(&schema.fields[si], &d.namepath) {
            // Only empty-collection element lines may be skipped:
            // materialization guarantees every declared field a
            // `.daiv` line, so the scan never branches on the
            // optional marker (SPEC.md § Parallel Scan Validation).
            // Element counts are enforced by Pass-1 cardinality.
            if !is_collection(&schema.fields[si]) {
                return Err(at(
                    AppError::RequiredFieldSchema,
                    d.line,
                    format!(
                        "declared field {} missing (scan reached {})",
                        schema.fields[si].namepath, d.namepath
                    ),
                ));
            }
            si += 1;
        }
        if si == schema.fields.len() {
            if schema.strict {
                return Err(at(
                    AppError::UndefinedFieldStrictSchema,
                    d.line,
                    format!("{} is not defined in the strict schema", d.namepath),
                ));
            }
            di += 1;
            continue;
        }
        let f = &schema.fields[si];
        if let Some((arr, _)) = ns_arr_parts(f) {
            // A namespace-array element group: consecutive element
            // lines of the same array, cycled once per element run.
            let arr = arr.to_string();
            let mut gend = si;
            while gend < schema.fields.len()
                && ns_arr_parts(&schema.fields[gend]).is_some_and(|(a, _)| a == arr)
            {
                gend += 1;
            }
            di = validate_ns_array(&schema.fields[si..gend], &arr, &data, di, schema.strict)?;
            si = gend;
            continue;
        }
        check_field(f, d).map_err(|e| field_ctx(e, f, d))?;
        // Map-entry and scalar-array element lines consume a
        // variable-length run: the scan stays on them until the
        // namepath prefix changes (the spec's array-loop rule).
        if !is_map_entry(f) && scalar_arr_prefix(f).is_none() {
            si += 1;
        }
        di += 1;
    }
    while si < schema.fields.len() {
        // Remaining schema lines: empty collections are fine,
        // anything else is a missing declared field — materialization
        // guarantees presence, so no optional-marker branch.
        if !is_collection(&schema.fields[si]) {
            return Err(at(
                AppError::RequiredFieldSchema,
                0,
                format!(
                    "declared field {} missing at end of document",
                    schema.fields[si].namepath
                ),
            ));
        }
        si += 1;
    }
    check_collections(&data, &schema.collections)
}

/// Level 2 collection constraints. Cardinality is a Pass 1 concern
/// (O(1) counters, checked on scan completion) and fires before the
/// O(N) Pass 2 table-constraint check (SPEC.md § Validation).
fn check_collections(data: &[DataLine], colls: &[Collection]) -> Result<(), AppErrorAt> {
    let colls: Vec<(&Collection, Vec<HashMap<&str, &str>>)> = colls
        .iter()
        .map(|c| (c, elements(data, &c.array)))
        .collect();
    for (coll, els) in &colls {
        let n = els.len() as u64;
        if coll.header.min.is_some_and(|m| n < m) || coll.header.max.is_some_and(|m| n > m) {
            let bounds = match (coll.header.min, coll.header.max) {
                (Some(lo), Some(hi)) => format!("min={lo} max={hi}"),
                (Some(lo), None) => format!("min={lo}"),
                (None, Some(hi)) => format!("max={hi}"),
                (None, None) => unreachable!(),
            };
            return Err(at(
                AppError::CardinalityViolation,
                0,
                format!("{} has {n} elements, schema requires {bounds}", coll.array),
            ));
        }
    }
    for (coll, els) in &colls {
        for clause in coll.header.groups.iter().flatten() {
            match clause {
                Clause::Unique(fields) => {
                    let mut seen: HashSet<String> = HashSet::new();
                    for el in els {
                        // Length-prefix each value so distinct tuples
                        // never collide (SPEC.md § Compound-key
                        // encoding). Pass 2 sees the materialized
                        // .daiv, so an omitted optional field
                        // participates as its materialized value —
                        // the resolved default, or a `!null` line's
                        // empty payload (the `unwrap_or` covers only
                        // hand-assembled, unmaterialized input).
                        let key: String = fields
                            .iter()
                            .map(|f| {
                                let v = el.get(f.as_str()).copied().unwrap_or("");
                                format!("{}:{v}", v.len())
                            })
                            .collect();
                        if !seen.insert(key) {
                            let tuple: Vec<&str> = fields
                                .iter()
                                .map(|f| el.get(f.as_str()).copied().unwrap_or(""))
                                .collect();
                            return Err(at(
                                AppError::UniquenessViolation,
                                0,
                                format!(
                                    "{}: duplicate value ({}) for unique key ({})",
                                    coll.array,
                                    tuple.join(", "),
                                    fields.join(", ")
                                ),
                            ));
                        }
                    }
                }
                Clause::Ref {
                    field,
                    target_arr,
                    target_field,
                } => {
                    let targets: HashSet<&str> = elements(data, target_arr)
                        .iter()
                        .filter_map(|el| el.get(target_field.as_str()).copied())
                        .collect();
                    for el in els {
                        let v = el.get(field.as_str()).copied().unwrap_or("");
                        if !targets.contains(v) {
                            return Err(at(
                                AppError::ReferentialIntegrity,
                                0,
                                format!(
                                    "{}: {field}={v} has no match in {target_arr}/*::{target_field}",
                                    coll.array
                                ),
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Group an array's element lines (`{arr}/{i}::{field}`) into
/// per-element field maps, in index order — the flat-stream
/// reconstruction of "for each element" (SPEC.md § Reconstructing
/// elements). First occurrence of a field wins.
fn elements<'a>(data: &'a [DataLine], arr: &str) -> Vec<HashMap<&'a str, &'a str>> {
    let prefix = format!("{arr}/");
    let mut by_idx: BTreeMap<usize, HashMap<&str, &str>> = BTreeMap::new();
    for d in data {
        if let Some((i, f)) = split_element(&d.namepath, &prefix) {
            by_idx
                .entry(i)
                .or_default()
                .entry(f)
                .or_insert(d.value.as_str());
        }
    }
    by_idx.into_values().collect()
}

/// `.!provenance` enforcement, per data line. The grammar attaches a
/// timestamp to a source entry, so "timestamp present" means some
/// provenance entry carries `@`; `#dpid` is never constrained.
fn check_provenance(level: ProvenanceLevel, prov: Option<&str>) -> Result<(), AppError> {
    let has_ts = prov.is_some_and(|p| p.contains('@'));
    let ok = match level {
        ProvenanceLevel::Required => has_ts,
        ProvenanceLevel::Source => prov.is_some(),
        ProvenanceLevel::None => prov.is_none(),
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::ProvenanceSchema)
    }
}

/// One namespace-array element run: cycle the group's field lines for
/// each indexed element, enforcing per-element required fields in
/// order. Returns the data index after the array's lines.
fn validate_ns_array(
    group: &[SchemaField],
    arr: &str,
    data: &[DataLine],
    mut di: usize,
    strict: bool,
) -> Result<usize, AppErrorAt> {
    let undefined = |d: &DataLine| {
        at(
            AppError::UndefinedFieldStrictSchema,
            d.line,
            format!("{} is not defined in the strict schema", d.namepath),
        )
    };
    let prefix = format!("{arr}/");
    while di < data.len() && data[di].namepath.starts_with(&prefix) {
        let Some((idx, _)) = split_element(&data[di].namepath, &prefix) else {
            // Deeper structure inside an element — not expressible in
            // the compiled subset; undefined under strict.
            if strict {
                return Err(undefined(&data[di]));
            }
            di += 1;
            continue;
        };
        let mut gj = 0usize;
        while di < data.len() {
            let d = &data[di];
            if !d.namepath.starts_with(&prefix) {
                break;
            }
            let Some((i2, field)) = split_element(&d.namepath, &prefix) else {
                if strict {
                    return Err(undefined(d));
                }
                di += 1;
                continue;
            };
            if i2 != idx {
                break; // next element
            }
            // Search the whole group, not just forward from gj, so a
            // repeat of an already-consumed field (behind gj) is caught
            // as a duplicate rather than silently slipping through with
            // its value never constraint-checked.
            let hit = (0..group.len())
                .find(|&g| ns_arr_parts(&group[g]).is_some_and(|(_, f)| f == field));
            match hit {
                Some(gk) if gk < gj => {
                    // A field already consumed in this element — a
                    // duplicate schema-defined key (SPEC.md § Errors,
                    // DuplicateKeySchemaError), same as the flat-field
                    // duplicate check above.
                    return Err(at(
                        AppError::DuplicateKeySchema,
                        d.line,
                        format!("second entry for schema field {}", d.namepath),
                    ));
                }
                Some(gk) => {
                    // Materialization guarantees every group field a
                    // line per element — a skipped field is missing,
                    // optional or not (strict lockstep).
                    if gk > gj {
                        return Err(at(
                            AppError::RequiredFieldSchema,
                            d.line,
                            format!(
                                "element {arr}/{idx} missing declared field {} (scan reached {})",
                                group[gj].namepath, d.namepath
                            ),
                        ));
                    }
                    check_field(&group[gk], d).map_err(|e| field_ctx(e, &group[gk], d))?;
                    gj = gk + 1;
                }
                None => {
                    if strict {
                        return Err(undefined(d));
                    }
                }
            }
            di += 1;
        }
        // Group fields not seen by the element's end are missing —
        // materialization would have emitted them (strict lockstep).
        if gj < group.len() {
            return Err(at(
                AppError::RequiredFieldSchema,
                0,
                format!(
                    "element {arr}/{idx} missing declared field {}",
                    group[gj].namepath
                ),
            ));
        }
    }
    Ok(di)
}

/// `{prefix}{digits}::{field}` → (index, field). A `/` in the field
/// part signals deeper structure — unless the field is quoted, where
/// it is literal name content (`"a/b"` is a flat terminal field).
pub(crate) fn split_element<'a>(np: &'a str, prefix: &str) -> Option<(usize, &'a str)> {
    let rest = np.strip_prefix(prefix)?;
    let (idx, field) = rest.split_once("::")?;
    // A quoted terminal field is flat only when it is exactly one
    // well-formed quoted name; a malformed or trailing-text quoted form
    // is deeper/off-grammar structure.
    let deeper = if field.starts_with('"') {
        !crate::compiler::is_quoted_name(field)
    } else {
        field.contains('/')
    };
    if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) || deeper {
        return None;
    }
    // Only the canonical spelling is covered by a collection line: a
    // leading-zero index (rejected at the Lexer anyway) or one beyond
    // usize (an implementation limit, SPEC.md § Implementation Limits)
    // is an uncovered — undefined — field, not this element.
    if idx.len() > 1 && idx.starts_with('0') {
        return None;
    }
    Some((idx.parse().ok()?, field))
}

/// A compiled map-entry line: empty-terminal namepath (`…::`) with no
/// `@` in the steps.
pub(crate) fn is_map_entry(f: &SchemaField) -> bool {
    f.namepath.ends_with("::") && !f.namepath.contains('@')
}

/// A compiled scalar-array element line: empty-terminal namepath with
/// an `@` step (`/@ports::`); the constraint applies to every index.
pub(crate) fn scalar_arr_prefix(f: &SchemaField) -> Option<&str> {
    (f.namepath.ends_with("::") && f.namepath.contains('@')).then_some(f.namepath.as_str())
}

/// A compiled namespace-array element field line: the elided-index
/// namepath `{arr}/::{field}`.
pub(crate) fn ns_arr_parts(f: &SchemaField) -> Option<(&str, &str)> {
    let i = f.namepath.find("/::")?;
    Some((&f.namepath[..i], &f.namepath[i + 3..]))
}

pub(crate) fn is_collection(f: &SchemaField) -> bool {
    is_map_entry(f) || scalar_arr_prefix(f).is_some() || ns_arr_parts(f).is_some()
}

/// Exact namepath match, or the collection-line prefix matches.
pub(crate) fn line_matches(f: &SchemaField, np: &str) -> bool {
    if let Some((arr, _)) = ns_arr_parts(f) {
        return np.starts_with(&format!("{arr}/"));
    }
    if let Some(prefix) = scalar_arr_prefix(f) {
        return np
            .strip_prefix(prefix)
            .is_some_and(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()));
    }
    if is_map_entry(f) {
        return np.strip_prefix(f.namepath.as_str()).is_some_and(|key| {
            // A quoted key is a single literal terminal name, so `/`
            // and `::` inside it are content, not deeper structure —
            // but only when it is exactly one well-formed quoted name
            // (the same exemption split_element applies).
            !key.is_empty()
                && (crate::compiler::is_quoted_name(key)
                    || (!key.contains('"') && !key.contains('/') && !key.contains("::")))
        });
    }
    f.namepath == np
}

fn check_field(f: &SchemaField, d: &DataLine) -> Result<(), AppError> {
    let span = f
        .items
        .iter()
        .find_map(|i| match i {
            Item::Constraint(Constraint::Span(s)) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("..lex");
    collation_tag(span)?;

    // Units byte-compare, both directions (SPEC.md § Validation:
    // units do not convert). The data line's unit must canonicalize
    // to exactly the field's — including the case where the field
    // carries no unit (a unit-bearing data line is then a mismatch)
    // and where the data line's unit is not a known unit at all.
    let field_unit = f.items.iter().find_map(|i| match i {
        Item::Anno(a) => a.unit.as_deref().and_then(crate::unit::canonicalize),
        _ => None,
    });
    let data_unit = match &d.unit {
        None => None,
        Some(u) => Some(crate::unit::canonicalize(u).ok_or(AppError::TypeMismatch)?),
    };
    if field_unit != data_unit {
        return Err(AppError::TypeMismatch);
    }

    for item in &f.items {
        match item {
            Item::Anno(a) => {
                // The data line's type selects the alternative; the
                // matched alternative's own constraint group (and its
                // span) governs the value (SPEC.md § Tagged unions).
                let matched: Option<&[Constraint]> = if a.type_name == d.type_name {
                    Some(&a.constraints)
                } else {
                    a.union
                        .iter()
                        .find(|alt| alt.name == d.type_name)
                        .map(|alt| alt.constraints.as_slice())
                };
                let Some(cs) = matched else {
                    return Err(AppError::TypeMismatch);
                };
                let aspan = cs
                    .iter()
                    .find_map(|c| match c {
                        Constraint::Span(s) => Some(s.as_str()),
                        _ => None,
                    })
                    .unwrap_or(span);
                for c in cs {
                    check_constraint(c, aspan, &d.value)?;
                }
            }
            Item::Named(_) => {} // never appears in lowered .csaiv
            Item::Constraint(c) => check_constraint(c, span, &d.value)?,
        }
    }
    Ok(())
}

/// The locale tag of a `..lex[tag]` span (Level 3), if this runtime
/// can compare under it. `Ok(None)` for every other span. Without
/// the `collation` feature this is an L0-2 runtime, which must not
/// silently fall back to byte order (SPEC.md § Collation) — no
/// warning channel here, so reject; with it, a tag that is malformed
/// or carries an unrecognized `-u-` override is equally unusable.
fn collation_tag(span: &str) -> Result<Option<&str>, AppError> {
    let Some(tag) = span
        .strip_prefix("..lex[")
        .and_then(|r| r.strip_suffix(']'))
    else {
        return Ok(None);
    };
    #[cfg(any(feature = "collation-icu", feature = "collation-colligo"))]
    if crate::collate::resolves(tag) {
        return Ok(Some(tag));
    }
    let _ = tag;
    Err(AppError::CollationUnsupported)
}

fn check_constraint(c: &Constraint, span: &str, value: &str) -> Result<(), AppError> {
    let ctag = collation_tag(span)?;
    #[cfg(not(any(feature = "collation-icu", feature = "collation-colligo")))]
    let _ = ctag;
    match c {
        Constraint::Pattern(body) => {
            let re = Regex::new(body).ok_or(AppError::ConstraintViolation)?;
            if !re.is_match(value) {
                return Err(AppError::ConstraintViolation);
            }
        }
        Constraint::Range(lo, hi) => {
            // Collation governs order: a `..lex[tag]` range compares
            // under the tag's collation, not byte order (SPEC.md
            // § Reference Collation).
            #[cfg(any(feature = "collation-icu", feature = "collation-colligo"))]
            if let Some(tag) = ctag {
                return match crate::collate::range_ok(tag, value, lo.as_deref(), hi.as_deref()) {
                    Some(true) => Ok(()),
                    Some(false) => Err(AppError::ConstraintViolation),
                    None => Err(AppError::CollationUnsupported),
                };
            }
            if !range_ok(span, value, lo.as_deref(), hi.as_deref()) {
                return Err(AppError::ConstraintViolation);
            }
        }
        Constraint::Enum(vs) => {
            // Collation governs equality too: `..lex[tag]` enum
            // membership is collation equality (SPEC.md § Reference
            // Collation).
            #[cfg(any(feature = "collation-icu", feature = "collation-colligo"))]
            if let Some(tag) = ctag {
                return match crate::collate::enum_has(tag, value, vs) {
                    Some(true) => Ok(()),
                    Some(false) => Err(AppError::ConstraintViolation),
                    None => Err(AppError::CollationUnsupported),
                };
            }
            if !vs.iter().any(|v| v == value) {
                return Err(AppError::ConstraintViolation);
            }
        }
        Constraint::Length(inner) => {
            let n = value.chars().count().to_string();
            // Length comparisons are numeric regardless of the value span.
            check_constraint(inner, "..num", &n)?;
        }
        Constraint::Span(_) => {}
    }
    Ok(())
}

fn range_ok(span: &str, value: &str, lo: Option<&str>, hi: Option<&str>) -> bool {
    match span {
        // Exact integer comparison when both operands are integer-shaped
        // (SPEC.md § Numeric domain of ..num: int-derived types compare
        // at arbitrary precision, no truncation at 2^53); IEEE double
        // for float-shaped operands.
        "..num" => lo.is_none_or(|l| num_le(l, value)) && hi.is_none_or(|h| num_le(value, h)),
        // Dotted version order: segment-wise numeric, not lexical, so
        // 1.10.0 > 1.9.0 (SPEC.md § The ..ver span).
        "..ver" => lo.is_none_or(|l| ver_le(l, value)) && hi.is_none_or(|h| ver_le(value, h)),
        // ..lex byte order; ..time (ISO 8601) compares correctly as
        // strings within a fixed offset — full temporal semantics are a
        // Level 3+ concern beyond this seed.
        _ => lo.is_none_or(|l| value >= l) && hi.is_none_or(|h| value <= h),
    }
}

/// `a <= b` by dotted numeric version segments; a shorter prefix
/// orders before its extensions (`1.2` < `1.2.1`). A non-numeric
/// segment falls back to lexical comparison at that position.
fn ver_le(a: &str, b: &str) -> bool {
    use std::cmp::Ordering::*;
    let mut ai = a.split('.');
    let mut bi = b.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (None, Some(_)) => return true,
            (Some(_), None) => return false,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    _ => x.cmp(y),
                };
                match ord {
                    Less => return true,
                    Greater => return false,
                    Equal => continue,
                }
            }
        }
    }
}

/// `a <= b` in the `..num` domain.
fn num_le(a: &str, b: &str) -> bool {
    match (int_parts(a), int_parts(b)) {
        (Some(x), Some(y)) => cmp_int(x, y) != std::cmp::Ordering::Greater,
        _ => match (a.parse::<f64>(), b.parse::<f64>()) {
            (Ok(x), Ok(y)) => x <= y,
            _ => false,
        },
    }
}

/// `(negative, magnitude without leading zeros)` for strings matching
/// `^-?[0-9]+$`; None for anything else (float-shaped, empty, garbage).
fn int_parts(s: &str) -> Option<(bool, &str)> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mag = digits.trim_start_matches('0');
    let mag = if mag.is_empty() { "0" } else { mag };
    Some((neg && mag != "0", mag))
}

/// Arbitrary-precision integer comparison on the normalized parts:
/// sign first, then magnitude length, then lexicographic digits.
fn cmp_int((an, am): (bool, &str), (bn, bm): (bool, &str)) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (an, bn) {
        (true, false) => Less,
        (false, true) => Greater,
        (false, false) => am.len().cmp(&bm.len()).then_with(|| am.cmp(bm)),
        (true, true) => bm.len().cmp(&am.len()).then_with(|| bm.cmp(am)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_errors_carry_site_context() {
        let schema =
            parse_csaiv(".!kaivschema 1 a/x\n!int[1,65535]'/server::port=\n").unwrap();
        let err = validate(".!kaiv 1\n!int'/server::port=99999\n", &schema).unwrap_err();
        assert_eq!(err.error, AppError::ConstraintViolation);
        assert_eq!(err.line, 2);
        assert_eq!(
            err.context,
            "/server::port=99999 (type !int) violates !int[1,65535]"
        );
        assert_eq!(
            err.to_string(),
            "ConstraintViolationError: /server::port=99999 (type !int) violates \
             !int[1,65535] (line 2)"
        );

        // A missing declared field names the field, without a line.
        let err = validate(".!kaiv 1\n", &schema).unwrap_err();
        assert_eq!(err.error, AppError::RequiredFieldSchema);
        assert_eq!(err.line, 0);
        assert_eq!(
            err.context,
            "declared field /server::port missing at end of document"
        );
    }

    #[test]
    fn timestamp_provenance_level_is_gone() {
        // The grammar anchors every entry to its source id, so a
        // timestamp-only level cannot exist — `timestamp` was exactly
        // `required` and is no longer a recognized level.
        assert!(parse_csaiv(".!kaivschema 1 a/x\n.!provenance:timestamp\n!str'::a=\n").is_err());
        assert!(parse_csaiv(".!kaivschema 1 a/x\n.!provenance:required\n!str'::a=\n").is_ok());
    }

    #[test]
    fn provenance_required_on_every_element_line() {
        let schema = parse_csaiv(
            ".!kaivschema 1 a/x\n.!provenance:required\n!str'/@servers/::host=\n!str'/@servers/::port=\n",
        )
        .unwrap();
        // The port element line carries no provenance — must fail even
        // though it is not the first line of the run.
        let bad =
            ".!kaiv 1\n!str?s1@20250101T000000Z'/@servers/0::host=a\n!str'/@servers/0::port=1\n";
        assert_eq!(validate(bad, &schema).map_err(|e| e.error), Err(AppError::ProvenanceSchema));
        // Fully-provenanced element validates.
        let ok = ".!kaiv 1\n!str?s1@20250101T000000Z'/@servers/0::host=a\n!str?s1@20250101T000000Z'/@servers/0::port=1\n";
        assert_eq!(validate(ok, &schema).map_err(|e| e.error), Ok(()));
    }

    #[test]
    fn duplicate_element_field_is_a_duplicate_key() {
        let schema =
            parse_csaiv(".!kaivschema 1 a/s\n/^-?[0-9]+$/ ..num[1,65535]'/@servers/::port=\n")
                .unwrap();
        // Two ::port lines for element 0 — a repeated schema-defined
        // element key; the out-of-range second value must not slip
        // through unchecked.
        let dup = ".!kaiv 1\n!str'/@servers/0::port=80\n!str'/@servers/0::port=999999\n";
        assert_eq!(validate(dup, &schema).map_err(|e| e.error), Err(AppError::DuplicateKeySchema));
    }

    #[test]
    fn quoted_map_key_with_slash_is_matched() {
        let schema = parse_csaiv(
            ".!kaivschema 1 acme/m strict\n!str'::host=\n/^-?[0-9]+$/ ..num'/settings::=\n",
        )
        .unwrap();
        // A quoted key literally named `a/b` is a flat map entry, not
        // deeper structure — it matches and its value is checked.
        let ok = ".!kaiv 1\n!str'::host=a\n!str'/settings::\"a/b\"=1\n";
        assert_eq!(validate(ok, &schema).map_err(|e| e.error), Ok(()));
        let bad = ".!kaiv 1\n!str'::host=a\n!str'/settings::\"a/b\"=oops\n";
        assert_eq!(validate(bad, &schema).map_err(|e| e.error), Err(AppError::ConstraintViolation));
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

    const SERVER_CSAIV: &str = concat!(
        ".!kaivschema 1 hub/server strict\n",
        "!str'::host=\n",
        "/^-?[0-9]+$/ ..num [1,65535]'::port=8080\n",
    );

    #[test]
    fn schema_for_daiv_none_without_declaration() {
        let r = dead_end();
        assert!(schema_for_daiv(".!kaiv 1\n!str'::a=x\n", &r)
            .unwrap()
            .is_none());
    }

    #[test]
    fn schema_for_daiv_flat_reference() {
        let r = dead_end();
        r.preload("hub/server", "csaiv", SERVER_CSAIV.into());
        let daiv = ".!kaiv 1\n.!schema:hub/server\n!str'::host=a\n!int'::port=80\n";
        let s = schema_for_daiv(daiv, &r).unwrap().unwrap();
        assert!(s.strict); // the flat reference's header governs
        assert_eq!(s.fields.len(), 2);
        assert!(validate(daiv, &s).is_ok());
        assert!(validate(".!kaiv 1\n!str'::host=a\n!int'::port=99999\n", &s).is_err());
    }

    #[test]
    fn schema_for_daiv_encapsulated_composition() {
        let r = dead_end();
        r.preload("hub/server", "csaiv", SERVER_CSAIV.into());
        let daiv = concat!(
            ".!kaiv 1\n",
            ".!schema:/upstream hub/server\n",
            ".!schema:/downstream hub/server\n",
            "!str'/upstream::host=a\n!int'/upstream::port=80\n",
            "!str'/downstream::host=b\n!int'/downstream::port=81\n",
        );
        let s = schema_for_daiv(daiv, &r).unwrap().unwrap();
        // Qualified references contribute fields, not headers.
        assert!(!s.strict);
        assert_eq!(s.fields.len(), 4);
        assert!(s.fields.iter().any(|f| f.namepath == "/upstream::port"));
        assert!(validate(daiv, &s).is_ok());
    }

    #[test]
    fn schema_for_daiv_unresolved_is_recorded() {
        let r = dead_end();
        let daiv = ".!kaiv 1\n.!schema:acme/missing\n!str'::a=x\n";
        assert!(schema_for_daiv(daiv, &r).is_err());
        assert_eq!(
            r.take_missing(),
            vec![("acme/missing".to_string(), "csaiv".to_string())]
        );
    }

    #[test]
    fn schema_for_daiv_url_reference_rejected() {
        let r = dead_end();
        let daiv = ".!kaiv 1\n.!schema https://example.org/x.csaiv\n";
        assert!(matches!(
            schema_for_daiv(daiv, &r),
            Err(PipelineError::App(AppError::SchemaResolution))
        ));
    }

    #[test]
    fn ver_ranges_are_segment_numeric() {
        // Lexical order would reject 1.10.0 from [1.9.0, 2.0.0].
        assert!(range_ok("..ver", "1.10.0", Some("1.9.0"), Some("2.0.0")));
        assert!(!range_ok("..ver", "2.0.1", Some("1.9.0"), Some("2.0.0")));
        assert!(range_ok("..ver", "1.2", Some("1.2"), Some("1.2.1"))); // prefix orders low
        assert!(!range_ok("..ver", "1.2.1", None, Some("1.2")));
    }

    /// Without the `collation` feature this is an L0-2 runtime:
    /// `..lex[locale]` rejects rather than falling back to byte
    /// order (SPEC.md § Collation).
    #[cfg(not(any(feature = "collation-icu", feature = "collation-colligo")))]
    #[test]
    fn locale_collation_is_rejected() {
        let s = parse_csaiv(".!kaivschema 1 a/c\n!str ..lex[en]'::n=\n").unwrap();
        assert_eq!(
            validate(".!kaiv 1\n!str'::n=x\n", &s).map_err(|e| e.error),
            Err(AppError::CollationUnsupported)
        );
    }

    #[cfg(any(feature = "collation-icu", feature = "collation-colligo"))]
    #[test]
    fn locale_collation_ranges() {
        // Byte order puts "étude" (0xC3…) past "f", outside [e,f];
        // French collation keeps é with e, inside it.
        let s = parse_csaiv(".!kaivschema 1 a/c\n!str [e,f] ..lex[fr]'::n=\n").unwrap();
        assert!(validate(".!kaiv 1\n!str'::n=étude\n", &s).is_ok());
        assert_eq!(
            validate(".!kaiv 1\n!str'::n=granite\n", &s).map_err(|e| e.error),
            Err(AppError::ConstraintViolation)
        );
        let bare = parse_csaiv(".!kaivschema 1 a/c\n!str [e,f] ..lex'::n=\n").unwrap();
        assert_eq!(
            validate(".!kaiv 1\n!str'::n=étude\n", &bare).map_err(|e| e.error),
            Err(AppError::ConstraintViolation)
        );
    }

    #[cfg(any(feature = "collation-icu", feature = "collation-colligo"))]
    #[test]
    fn locale_collation_enum_equality() {
        // Collation governs equality: the NFD spelling of "résumé"
        // is a member of the NFC-spelled enum under fr, and a plain
        // "resume" is not (tertiary default — accents distinguish).
        let s = parse_csaiv(".!kaivschema 1 a/c\n!str {résumé} ..lex[fr]'::n=\n").unwrap();
        assert!(validate(".!kaiv 1\n!str'::n=re\u{301}sume\u{301}\n", &s).is_ok());
        assert_eq!(
            validate(".!kaiv 1\n!str'::n=resume\n", &s).map_err(|e| e.error),
            Err(AppError::ConstraintViolation)
        );
        // Strength overrides (`-u-ks-…`) split the backends: ICU4X
        // honors them (primary strength ignores accents), colligo
        // rejects the tag rather than silently collating at the
        // wrong strength.
        let s2 =
            parse_csaiv(".!kaivschema 1 a/c\n!str {résumé} ..lex[en-u-ks-level1]'::n=\n").unwrap();
        #[cfg(feature = "collation-icu")]
        assert!(validate(".!kaiv 1\n!str'::n=resume\n", &s2).is_ok());
        #[cfg(all(feature = "collation-colligo", not(feature = "collation-icu")))]
        assert_eq!(
            validate(".!kaiv 1\n!str'::n=resume\n", &s2).map_err(|e| e.error),
            Err(AppError::CollationUnsupported)
        );
    }

    #[cfg(any(feature = "collation-icu", feature = "collation-colligo"))]
    #[test]
    fn unresolvable_locale_tag_is_rejected() {
        // Malformed tag — and rejected up front, before any
        // comparison is attempted (the field carries no constraint).
        let s = parse_csaiv(".!kaivschema 1 a/c\n!str ..lex[123]'::n=\n").unwrap();
        assert_eq!(
            validate(".!kaiv 1\n!str'::n=x\n", &s).map_err(|e| e.error),
            Err(AppError::CollationUnsupported)
        );
    }

    #[test]
    fn unit_mismatch_both_directions() {
        // Field carries a unit, data does not — and the reverse.
        let with = parse_csaiv(".!kaivschema 1 a/u\n!float:km'::d=\n").unwrap();
        assert!(validate(".!kaiv 1\n!float:km'::d=5\n", &with).is_ok());
        assert_eq!(
            validate(".!kaiv 1\n!float'::d=5\n", &with).map_err(|e| e.error),
            Err(AppError::TypeMismatch)
        );
        let without = parse_csaiv(".!kaivschema 1 a/u\n!float'::d=\n").unwrap();
        assert_eq!(
            validate(".!kaiv 1\n!float:km'::d=5\n", &without).map_err(|e| e.error),
            Err(AppError::TypeMismatch)
        );
    }

    #[test]
    fn exact_integer_ranges() {
        // Beyond 2^53: f64 would conflate these neighbors.
        assert!(range_ok(
            "..num",
            "9007199254740993",
            None,
            Some("9007199254740993")
        ));
        assert!(!range_ok(
            "..num",
            "9007199254740993",
            None,
            Some("9007199254740992")
        ));
        assert!(range_ok("..num", "-007", Some("-10"), Some("0"))); // leading zeros
        assert!(range_ok("..num", "3.5", Some("1"), Some("4"))); // float falls back
        assert!(!range_ok("..num", "abc", None, Some("10")));
    }

    #[test]
    fn undefined_fields_interleave_in_relaxed_schemas() {
        // Relaxed schemas MAY contain undefined fields anywhere —
        // they must not consume the schema pointer (SPEC.md § Errors,
        // strict-vs-relaxed).
        let schema = parse_csaiv(".!kaivschema 1 acme/m\n!str'::a=\n!str'::b=\n").unwrap();
        let doc = ".!kaiv 1\n!str'::zzz=1\n!str'::a=x\n!str'::mid=2\n!str'::b=y\n!str'::tail=3\n";
        assert_eq!(validate(doc, &schema).map_err(|e| e.error), Ok(()));
        let strict = parse_csaiv(".!kaivschema 1 acme/m strict\n!str'::a=\n!str'::b=\n").unwrap();
        assert_eq!(
            validate(doc, &strict).map_err(|e| e.error),
            Err(AppError::UndefinedFieldStrictSchema)
        );
        // Ordering of DEFINED fields is still enforced.
        let ooo = ".!kaiv 1\n!str'::b=y\n!str'::a=x\n";
        assert_eq!(validate(ooo, &schema).map_err(|e| e.error), Err(AppError::RequiredFieldSchema));
    }

    #[test]
    fn quoted_names_with_operators_inside() {
        // parse_daiv splits quote-aware: a name may contain `=`.
        let schema = parse_csaiv(".!kaivschema 1 acme/m\n!str'::\"a=b\"=\n").unwrap();
        assert_eq!(
            validate(".!kaiv 1\n!str'::\"a=b\"=equals\n", &schema).map_err(|e| e.error),
            Ok(())
        );
    }

    #[test]
    fn pass2_table_constraints() {
        let schema = parse_csaiv(concat!(
            ".!kaivschema 1 acme/fleet\n",
            "/@servers [unique::host,port] [min=1] [max=3]\n",
            "!str'/@servers/::host=\n",
            "!str'/@servers/::port=\n",
        ))
        .unwrap();
        let doc = |els: &[(&str, &str)]| {
            let mut s = String::from(".!kaiv 1\n");
            for (i, (h, p)) in els.iter().enumerate() {
                s.push_str(&format!(
                    "!str'/@servers/{i}::host={h}\n!str'/@servers/{i}::port={p}\n"
                ));
            }
            s
        };
        assert_eq!(validate(&doc(&[("a", "1"), ("a", "2")]), &schema), Ok(()));
        // Compound uniqueness: same (host, port) pair violates.
        assert_eq!(
            validate(&doc(&[("a", "1"), ("a", "1")]), &schema).map_err(|e| e.error),
            Err(AppError::UniquenessViolation)
        );
        // Length-prefixed keys: ("a","bc") vs ("ab","c") do not collide.
        assert_eq!(validate(&doc(&[("a", "bc"), ("ab", "c")]), &schema), Ok(()));
        // Cardinality bounds.
        assert_eq!(
            validate(".!kaiv 1\n", &schema).map_err(|e| e.error),
            Err(AppError::CardinalityViolation)
        );
        assert_eq!(
            validate(
                &doc(&[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")]),
                &schema
            )
            .map_err(|e| e.error),
            Err(AppError::CardinalityViolation)
        );
    }

    #[test]
    fn pass2_foreign_keys() {
        let schema = parse_csaiv(concat!(
            ".!kaivschema 1 acme/org\n",
            "/@departments [unique::name]\n",
            "!str'/@departments/::name=\n",
            "/@employees [ref::department=/@departments/*::name]\n",
            "!str'/@employees/::name=\n",
            "!str'/@employees/::department=\n",
        ))
        .unwrap();
        let ok = concat!(
            ".!kaiv 1\n",
            "!str'/@departments/0::name=eng\n",
            "!str'/@employees/0::name=ada\n",
            "!str'/@employees/0::department=eng\n",
        );
        assert_eq!(validate(ok, &schema).map_err(|e| e.error), Ok(()));
        let dangling = concat!(
            ".!kaiv 1\n",
            "!str'/@departments/0::name=eng\n",
            "!str'/@employees/0::name=ada\n",
            "!str'/@employees/0::department=ops\n",
        );
        assert_eq!(
            validate(dangling, &schema).map_err(|e| e.error),
            Err(AppError::ReferentialIntegrity)
        );
        // Empty collections are valid absent an explicit [min=N].
        assert_eq!(validate(".!kaiv 1\n", &schema).map_err(|e| e.error), Ok(()));
    }

    #[test]
    fn map_entry_scan() {
        let schema = parse_csaiv(
            ".!kaivschema 1 acme/m strict\n!str'::host=\n/^-?[0-9]+$/ ..num'/settings::=\n",
        )
        .unwrap();
        let ok = ".!kaiv 1\n!str'::host=a\n!str'/settings::x=1\n!str'/settings::y=2\n";
        assert_eq!(validate(ok, &schema).map_err(|e| e.error), Ok(()));
        // Zero entries is a valid map.
        let empty = ".!kaiv 1\n!str'::host=a\n";
        assert_eq!(validate(empty, &schema).map_err(|e| e.error), Ok(()));
        // Entry value must satisfy the value-type constraint.
        let bad = ".!kaiv 1\n!str'::host=a\n!str'/settings::x=oops\n";
        assert_eq!(validate(bad, &schema).map_err(|e| e.error), Err(AppError::ConstraintViolation));
        // Strict: a non-entry under an unrelated path is still undefined.
        let undef = ".!kaiv 1\n!str'::host=a\n!str'/other::x=1\n";
        assert_eq!(
            validate(undef, &schema).map_err(|e| e.error),
            Err(AppError::UndefinedFieldStrictSchema)
        );
    }
}
