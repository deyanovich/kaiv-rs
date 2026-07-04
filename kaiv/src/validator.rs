//! The Validator: parallel scan of `.daiv` against `.csaiv`
//! (SPEC.md § Parallel Scan Validation, § Validator Pseudocode).
//! Constant-memory in spirit: one pass, one schema pointer, plus the
//! duplicate-detection set.

use crate::anno::{parse_annotation, parse_constraint_items, Constraint, Item};
use crate::error::{AppError, PipelineError};
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

/// The four `.!provenance` requirement levels (SPEC.md § Requiring
/// Provenance in Schemas). `#dpid` is never constrained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvenanceLevel {
    /// Both `?sourceID` and `@timestamp` on every data line.
    Required,
    /// `?sourceID` required; `@timestamp` optional.
    Source,
    /// `@timestamp` required; `?sourceID` optional.
    Timestamp,
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
                    "timestamp" => ProvenanceLevel::Timestamp,
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
                let group_ok = |cs: &[Constraint]| {
                    let gspan = cs
                        .iter()
                        .find_map(|c| match c {
                            Constraint::Span(sp) => Some(sp.as_str()),
                            _ => None,
                        })
                        .unwrap_or("..lex");
                    cs.iter().all(|c| check_constraint(c, gspan, value).is_ok())
                };
                if !group_ok(&a.constraints)
                    && !a.union.iter().any(|alt| group_ok(&alt.constraints))
                {
                    return false;
                }
            }
            Item::Named(_) => {}
        }
    }
    true
}

struct DataLine {
    type_name: String,
    unit: Option<String>,
    provenance: Option<String>,
    namepath: String,
    value: String,
}

fn parse_daiv(text: &str) -> Result<Vec<DataLine>, PipelineError> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let s = raw.trim_start_matches([' ', '\t']);
        if s.is_empty() || s.starts_with('#') || s.starts_with("//") {
            continue;
        }
        if s.starts_with(".!") || s.starts_with(".?") {
            continue;
        }
        let tick = find_tick(s)
            .ok_or_else(|| PipelineError::Other(format!("canonical line without ': {s}")))?;
        let prefix = &s[..tick];
        let rest = &s[tick + 1..];
        // Quote-aware: a quoted name may contain `=`.
        let eq = first_eq(rest)
            .ok_or_else(|| PipelineError::Other(format!("canonical line without =: {s}")))?;
        let a = parse_annotation(prefix)
            .ok_or_else(|| PipelineError::Other(format!("bad metadata prefix: {prefix}")))?;
        out.push(DataLine {
            type_name: a.type_name,
            unit: a.unit,
            provenance: a.provenance,
            namepath: rest[..eq].to_string(),
            value: rest[eq + 1..].to_string(),
        });
    }
    Ok(out)
}

pub fn validate(daiv: &str, schema: &CompiledSchema) -> Result<(), AppError> {
    let data = match parse_daiv(daiv) {
        Ok(d) => d,
        Err(_) => return Err(AppError::ConstraintViolation),
    };
    let defined: HashSet<&str> = schema.fields.iter().map(|f| f.namepath.as_str()).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut si = 0usize;
    let mut di = 0usize;

    while di < data.len() {
        let d = &data[di];
        if let Some(level) = schema.provenance {
            check_provenance(level, d.provenance.as_deref())?;
        }
        if defined.contains(d.namepath.as_str()) && !seen.insert(d.namepath.as_str()) {
            return Err(AppError::DuplicateKeySchema);
        }
        // An undefined data line — matching no schema line at all —
        // must not consume the schema pointer: relaxed schemas MAY
        // interleave undefined fields anywhere (SPEC.md § Errors,
        // strict-vs-relaxed), and the defined fields that follow
        // still have to find their schema lines.
        if !schema.fields.iter().any(|f| line_matches(f, &d.namepath)) {
            if schema.strict {
                return Err(AppError::UndefinedFieldStrictSchema);
            }
            di += 1;
            continue;
        }
        while si < schema.fields.len() && !line_matches(&schema.fields[si], &d.namepath) {
            // Collection lines (maps, arrays) are never themselves
            // required: empty collections are valid.
            if !schema.fields[si].optional && !is_collection(&schema.fields[si]) {
                return Err(AppError::RequiredFieldSchema);
            }
            si += 1;
        }
        if si == schema.fields.len() {
            if schema.strict {
                return Err(AppError::UndefinedFieldStrictSchema);
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
        check_field(f, d)?;
        // Map-entry and scalar-array element lines consume a
        // variable-length run: the scan stays on them until the
        // namepath prefix changes (the spec's array-loop rule).
        if !is_map_entry(f) && scalar_arr_prefix(f).is_none() {
            si += 1;
        }
        di += 1;
    }
    while si < schema.fields.len() {
        if !schema.fields[si].optional && !is_collection(&schema.fields[si]) {
            return Err(AppError::RequiredFieldSchema);
        }
        si += 1;
    }
    check_collections(&data, &schema.collections)
}

/// Level 2 collection constraints. Cardinality is a Pass 1 concern
/// (O(1) counters, checked on scan completion) and fires before the
/// O(N) Pass 2 table-constraint check (SPEC.md § Validation).
fn check_collections(data: &[DataLine], colls: &[Collection]) -> Result<(), AppError> {
    let colls: Vec<(&Collection, Vec<HashMap<&str, &str>>)> = colls
        .iter()
        .map(|c| (c, elements(data, &c.array)))
        .collect();
    for (coll, els) in &colls {
        let n = els.len() as u64;
        if coll.header.min.is_some_and(|m| n < m) || coll.header.max.is_some_and(|m| n > m) {
            return Err(AppError::CardinalityViolation);
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
                        // encoding). An omitted optional field
                        // participates as its empty value — a kaiv
                        // value is never absent, only empty.
                        let key: String = fields
                            .iter()
                            .map(|f| {
                                let v = el.get(f.as_str()).copied().unwrap_or("");
                                format!("{}:{v}", v.len())
                            })
                            .collect();
                        if !seen.insert(key) {
                            return Err(AppError::UniquenessViolation);
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
                            return Err(AppError::ReferentialIntegrity);
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
        ProvenanceLevel::Timestamp => has_ts,
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
) -> Result<usize, AppError> {
    let prefix = format!("{arr}/");
    while di < data.len() && data[di].namepath.starts_with(&prefix) {
        let Some((idx, _)) = split_element(&data[di].namepath, &prefix) else {
            // Deeper structure inside an element — not expressible in
            // the compiled subset; undefined under strict.
            if strict {
                return Err(AppError::UndefinedFieldStrictSchema);
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
                    return Err(AppError::UndefinedFieldStrictSchema);
                }
                di += 1;
                continue;
            };
            if i2 != idx {
                break; // next element
            }
            let mut hit = None;
            let mut gk = gj;
            while gk < group.len() {
                if ns_arr_parts(&group[gk]).is_some_and(|(_, f)| f == field) {
                    hit = Some(gk);
                    break;
                }
                gk += 1;
            }
            match hit {
                Some(gk) => {
                    // Fields skipped on the way must have been optional.
                    for g in &group[gj..gk] {
                        if !g.optional {
                            return Err(AppError::RequiredFieldSchema);
                        }
                    }
                    check_field(&group[gk], d)?;
                    gj = gk + 1;
                }
                None => {
                    if strict {
                        return Err(AppError::UndefinedFieldStrictSchema);
                    }
                }
            }
            di += 1;
        }
        for g in &group[gj..] {
            if !g.optional {
                return Err(AppError::RequiredFieldSchema);
            }
        }
    }
    Ok(di)
}

/// `{prefix}{digits}::{field}` → (index, field).
fn split_element<'a>(np: &'a str, prefix: &str) -> Option<(usize, &'a str)> {
    let rest = np.strip_prefix(prefix)?;
    let (idx, field) = rest.split_once("::")?;
    if idx.is_empty() || !idx.bytes().all(|b| b.is_ascii_digit()) || field.contains('/') {
        return None;
    }
    Some((idx.parse().ok()?, field))
}

/// A compiled map-entry line: empty-terminal namepath (`…::`) with no
/// `@` in the steps.
fn is_map_entry(f: &SchemaField) -> bool {
    f.namepath.ends_with("::") && !f.namepath.contains('@')
}

/// A compiled scalar-array element line: empty-terminal namepath with
/// an `@` step (`/@ports::`); the constraint applies to every index.
fn scalar_arr_prefix(f: &SchemaField) -> Option<&str> {
    (f.namepath.ends_with("::") && f.namepath.contains('@')).then_some(f.namepath.as_str())
}

/// A compiled namespace-array element field line: the elided-index
/// namepath `{arr}/::{field}`.
fn ns_arr_parts(f: &SchemaField) -> Option<(&str, &str)> {
    let i = f.namepath.find("/::")?;
    Some((&f.namepath[..i], &f.namepath[i + 3..]))
}

fn is_collection(f: &SchemaField) -> bool {
    is_map_entry(f) || scalar_arr_prefix(f).is_some() || ns_arr_parts(f).is_some()
}

/// Exact namepath match, or the collection-line prefix matches.
fn line_matches(f: &SchemaField, np: &str) -> bool {
    if let Some((arr, _)) = ns_arr_parts(f) {
        return np.starts_with(&format!("{arr}/"));
    }
    if let Some(prefix) = scalar_arr_prefix(f) {
        return np
            .strip_prefix(prefix)
            .is_some_and(|i| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()));
    }
    if is_map_entry(f) {
        return np
            .strip_prefix(f.namepath.as_str())
            .is_some_and(|key| !key.is_empty() && !key.contains('/') && !key.contains("::"));
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

    for item in &f.items {
        match item {
            Item::Anno(a) => {
                // A unit on a retained type item byte-compares against
                // the data line's canonical unit (SPEC.md § Validation:
                // units do not convert); a mismatch is a type mismatch.
                if a.unit.is_some() {
                    let canon =
                        |u: &Option<String>| u.as_deref().and_then(crate::unit::canonicalize);
                    if canon(&a.unit) != canon(&d.unit) {
                        return Err(AppError::TypeMismatch);
                    }
                }
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

fn check_constraint(c: &Constraint, span: &str, value: &str) -> Result<(), AppError> {
    match c {
        Constraint::Pattern(body) => {
            let re = Regex::new(body).ok_or(AppError::ConstraintViolation)?;
            if !re.is_match(value) {
                return Err(AppError::ConstraintViolation);
            }
        }
        Constraint::Range(lo, hi) => {
            if !range_ok(span, value, lo.as_deref(), hi.as_deref()) {
                return Err(AppError::ConstraintViolation);
            }
        }
        Constraint::Enum(vs) => {
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
        // ..lex byte order; ..time (ISO 8601) and ..ver compare correctly
        // as strings only in their fixed-width/segmented canonical forms —
        // full semantics are Level 3+ concerns beyond this seed.
        _ => lo.is_none_or(|l| value >= l) && hi.is_none_or(|h| value <= h),
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
        assert_eq!(validate(doc, &schema), Ok(()));
        let strict = parse_csaiv(".!kaivschema 1 acme/m strict\n!str'::a=\n!str'::b=\n").unwrap();
        assert_eq!(
            validate(doc, &strict),
            Err(AppError::UndefinedFieldStrictSchema)
        );
        // Ordering of DEFINED fields is still enforced.
        let ooo = ".!kaiv 1\n!str'::b=y\n!str'::a=x\n";
        assert_eq!(validate(ooo, &schema), Err(AppError::RequiredFieldSchema));
    }

    #[test]
    fn quoted_names_with_operators_inside() {
        // parse_daiv splits quote-aware: a name may contain `=`.
        let schema = parse_csaiv(".!kaivschema 1 acme/m\n!str'::\"a=b\"=\n").unwrap();
        assert_eq!(
            validate(".!kaiv 1\n!str'::\"a=b\"=equals\n", &schema),
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
            validate(&doc(&[("a", "1"), ("a", "1")]), &schema),
            Err(AppError::UniquenessViolation)
        );
        // Length-prefixed keys: ("a","bc") vs ("ab","c") do not collide.
        assert_eq!(validate(&doc(&[("a", "bc"), ("ab", "c")]), &schema), Ok(()));
        // Cardinality bounds.
        assert_eq!(
            validate(".!kaiv 1\n", &schema),
            Err(AppError::CardinalityViolation)
        );
        assert_eq!(
            validate(
                &doc(&[("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")]),
                &schema
            ),
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
        assert_eq!(validate(ok, &schema), Ok(()));
        let dangling = concat!(
            ".!kaiv 1\n",
            "!str'/@departments/0::name=eng\n",
            "!str'/@employees/0::name=ada\n",
            "!str'/@employees/0::department=ops\n",
        );
        assert_eq!(
            validate(dangling, &schema),
            Err(AppError::ReferentialIntegrity)
        );
        // Empty collections are valid absent an explicit [min=N].
        assert_eq!(validate(".!kaiv 1\n", &schema), Ok(()));
    }

    #[test]
    fn map_entry_scan() {
        let schema = parse_csaiv(
            ".!kaivschema 1 acme/m strict\n!str'::host=\n/^-?[0-9]+$/ ..num'/settings::=\n",
        )
        .unwrap();
        let ok = ".!kaiv 1\n!str'::host=a\n!str'/settings::x=1\n!str'/settings::y=2\n";
        assert_eq!(validate(ok, &schema), Ok(()));
        // Zero entries is a valid map.
        let empty = ".!kaiv 1\n!str'::host=a\n";
        assert_eq!(validate(empty, &schema), Ok(()));
        // Entry value must satisfy the value-type constraint.
        let bad = ".!kaiv 1\n!str'::host=a\n!str'/settings::x=oops\n";
        assert_eq!(validate(bad, &schema), Err(AppError::ConstraintViolation));
        // Strict: a non-entry under an unrelated path is still undefined.
        let undef = ".!kaiv 1\n!str'::host=a\n!str'/other::x=1\n";
        assert_eq!(
            validate(undef, &schema),
            Err(AppError::UndefinedFieldStrictSchema)
        );
    }
}
