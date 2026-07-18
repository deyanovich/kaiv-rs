//! The Denormalizer: `.raiv` → `.daiv`. Resolves `$field` /
//! `$path::field` references left-to-right against the field table
//! and collapses `$$` to a literal `$` (SPEC.md § The "Almost
//! Verbatim" Principle). References may appear mid-value; no forward
//! references. When the data declares a schema, the Denormalizer is
//! additionally schema-aware: it reads the compiled `.csaiv` and
//! materializes absent optional fields — the resolved default, or
//! `!null` — so every schema-declared field appears in `.daiv`, in
//! schema-declared order (SPEC.md § Default Values, § Null
//! Semantics "Materialization of Absent Fields"). A required field
//! absent from the authored data is a build-time
//! `RequiredFieldSchemaError`.

use crate::error::{AppError, PipelineError};
use crate::resolve::Resolver;
use crate::validator::{
    is_collection, line_matches, ns_arr_parts, split_element, CompiledSchema, SchemaField,
};
use std::collections::{HashMap, HashSet};

/// Schema-unaware core: reference resolution only. Documents that
/// declare a schema need [`denormalize_with`] for the materialization
/// pass.
pub fn denormalize(raiv: &str) -> Result<String, PipelineError> {
    // The Denormalizer consumes canonical `.raiv` only: the stream
    // must open with `.!raiv`, which is rewritten to `.!daiv` in the
    // output (SPEC.md § Format Declaration).
    crate::lexer::expect_kind(raiv, "raiv").map_err(PipelineError::Lex)?;
    let mut table: HashMap<String, String> = HashMap::new();
    let mut out = String::new();
    for line in raiv.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let eol = &line[body.len()..];
        if is_format_raiv(body) {
            out.push_str(".!daiv");
            out.push_str(eol);
            continue;
        }
        // Comments and declarations are not data lines: they neither
        // define references nor undergo `$`-resolution (a `$` inside a
        // comment or `.!`/`.?` header is literal). Pass them through
        // verbatim — same non-data classification `namepath_of` uses
        // for the materialization pass.
        if !is_nondata_line(body) {
            if let Some(tick) = body.find('\'') {
                if let Some(eq_rel) = body[tick..].find('=') {
                    let eq = tick + eq_rel;
                    let namepath = &body[tick + 1..eq];
                    let value = &body[eq + 1..];
                    let resolved = resolve_value(value, &table)?;
                    table.insert(namepath.to_string(), resolved.clone());
                    out.push_str(&body[..eq + 1]);
                    out.push_str(&resolved);
                    out.push_str(eol);
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    Ok(out)
}

/// Whether a line body is the `.!raiv` format declaration (bare or
/// versioned), modulo leading whitespace.
fn is_format_raiv(body: &str) -> bool {
    body.trim_start_matches([' ', '\t'])
        .strip_prefix(".!raiv")
        .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t']))
}

/// The full Denormalizer: reference resolution, then — when the data
/// declares a schema — materialization of absent fields from the
/// resolved `.csaiv` (SPEC.md § Default Values).
pub fn denormalize_with(raiv: &str, resolver: &Resolver) -> Result<String, PipelineError> {
    let daiv = denormalize(raiv)?;
    match crate::validator::schema_for_daiv(&daiv, resolver)? {
        None => Ok(daiv),
        Some(schema) => materialize(&daiv, &schema),
    }
}

/// Merge-walk the resolved document against the compiled schema's
/// field order, inserting a line for every schema-declared field
/// absent from the data: the resolved default when applicable, else
/// `!null'…=` when the type admits null. Absent required fields are
/// a build-time `RequiredFieldSchemaError`. Out-of-order or undefined
/// data lines pass through untouched — ordering is the Validator's
/// verdict, not the Denormalizer's to repair.
pub(crate) fn materialize(daiv: &str, schema: &CompiledSchema) -> Result<String, PipelineError> {
    let lines: Vec<&str> = daiv.split_inclusive('\n').collect();
    // Line namepaths (None for declarations/comments/blank lines).
    let nps: Vec<Option<String>> = lines.iter().map(|l| namepath_of(l)).collect();
    // Fields present anywhere in the document: a schema field found
    // here but not at the scan position is out of order, not absent —
    // nothing to materialize.
    let present: HashSet<&str> = nps.iter().flatten().map(String::as_str).collect();
    let eol = if daiv.contains("\r\n") { "\r\n" } else { "\n" };

    let fields = &schema.fields;
    let mut out = String::new();
    let mut si = 0usize;
    let mut di = 0usize;
    while di < lines.len() {
        let Some(np) = &nps[di] else {
            out.push_str(lines[di]);
            di += 1;
            continue;
        };
        // Undefined field: emit as-is, keep the schema pointer.
        if !fields.iter().any(|f| line_matches(f, np)) {
            out.push_str(lines[di]);
            di += 1;
            continue;
        }
        while si < fields.len() && !line_matches(&fields[si], np) {
            emit_absent(&fields[si], &present, eol, &mut out)?;
            si += 1;
        }
        if si == fields.len() {
            // A defined field past the schema's end is out of order;
            // pass it through for the Validator to flag.
            out.push_str(lines[di]);
            di += 1;
            continue;
        }
        if let Some((arr, _)) = ns_arr_parts(&fields[si]) {
            // Namespace-array element run: cycle the group per
            // element, materializing absent optional element fields.
            let arr = arr.to_string();
            let mut gend = si;
            while gend < fields.len()
                && ns_arr_parts(&fields[gend]).is_some_and(|(a, _)| a == arr)
            {
                gend += 1;
            }
            di = materialize_ns_array(
                &fields[si..gend],
                &arr,
                &lines,
                &nps,
                di,
                eol,
                &mut out,
            )?;
            si = gend;
            continue;
        }
        emit_matched(&fields[si], lines[di], &mut out)?;
        // Map-entry and scalar-array element lines consume a run;
        // the pointer stays until the namepath prefix changes.
        if !crate::validator::is_map_entry(&fields[si])
            && crate::validator::scalar_arr_prefix(&fields[si]).is_none()
        {
            si += 1;
        }
        di += 1;
    }
    while si < fields.len() {
        emit_absent(&fields[si], &present, eol, &mut out)?;
        si += 1;
    }
    Ok(out)
}

/// One namespace-array element run. For each element present in the
/// data, group fields absent from that element are materialized in
/// group order (required-absent errors); lines whose field is not in
/// the group (or out of group order) pass through for the Validator.
#[allow(clippy::too_many_arguments)]
fn materialize_ns_array(
    group: &[SchemaField],
    arr: &str,
    lines: &[&str],
    nps: &[Option<String>],
    mut di: usize,
    eol: &str,
    out: &mut String,
) -> Result<usize, PipelineError> {
    let prefix = format!("{arr}/");
    let in_run = |i: usize| {
        nps.get(i)
            .and_then(|n| n.as_ref())
            .is_some_and(|n| n.starts_with(&prefix))
    };
    while di < lines.len() && in_run(di) {
        let np = nps[di].as_ref().expect("in_run checked");
        let Some((idx, _)) = split_element(np, &prefix) else {
            // Deeper structure inside an element — pass through.
            out.push_str(lines[di]);
            di += 1;
            continue;
        };
        // Collect this element's contiguous lines and field set.
        let mut dj = di;
        let mut elem_fields: HashSet<&str> = HashSet::new();
        while dj < lines.len() && in_run(dj) {
            let n = nps[dj].as_ref().expect("in_run checked");
            match split_element(n, &prefix) {
                Some((i2, f)) if i2 == idx => {
                    elem_fields.insert(f);
                    dj += 1;
                }
                Some(_) => break, // next element
                None => dj += 1,  // deeper structure, stays in run
            }
        }
        let mut gj = 0usize;
        for k in di..dj {
            let n = nps[k].as_ref().expect("in_run checked");
            let field = match split_element(n, &prefix) {
                Some((_, f)) => f,
                None => {
                    out.push_str(lines[k]);
                    continue;
                }
            };
            let hit = (gj..group.len())
                .find(|&g| ns_arr_parts(&group[g]).is_some_and(|(_, f)| f == field));
            if let Some(gk) = hit {
                for g in &group[gj..gk] {
                    emit_absent_element(g, &elem_fields, arr, idx, eol, out)?;
                }
                gj = gk + 1;
                emit_matched(&group[gk], lines[k], out)?;
            } else {
                out.push_str(lines[k]);
            }
        }
        for g in &group[gj..] {
            emit_absent_element(g, &elem_fields, arr, idx, eol, out)?;
        }
        di = dj;
    }
    Ok(di)
}

/// Emit a schema-matched data line, retyping `!str` to `!text` when
/// the schema declares text (SPEC.md § The text Type): the schema
/// type wins in the deployment artifact, so downstream consumers get
/// the export semantics without re-reading the schema. The coercion
/// is meaning-preserving only when the value carries no literal
/// `|:|` — such content would be silently reinterpreted as line
/// breaks, so it is a `DelimiterCollisionError` instead.
fn emit_matched(f: &SchemaField, line: &str, out: &mut String) -> Result<(), PipelineError> {
    if field_wants_text(f) {
        let body = line.trim_start_matches([' ', '\t']);
        if let Some(rest) = body.strip_prefix("!str") {
            if rest.starts_with(['\'', '?']) {
                if let Some(eq) = rest.find('\'').and_then(|t| {
                    crate::validator::first_eq(&rest[t + 1..]).map(|e| t + 1 + e)
                }) {
                    if rest[eq + 1..].trim_end_matches(['\n', '\r']).contains("|:|") {
                        return Err(PipelineError::App(AppError::DelimiterCollision));
                    }
                }
                out.push_str(&line[..line.len() - body.len()]);
                out.push_str("!text");
                out.push_str(rest);
                return Ok(());
            }
        }
    }
    out.push_str(line);
    Ok(())
}

/// Does the compiled field declare the `text` type (a retained
/// non-union `!text` item)?
fn field_wants_text(f: &SchemaField) -> bool {
    f.items.iter().any(|i| match i {
        crate::anno::Item::Anno(a) => a.type_name == "text" && a.union.is_empty(),
        _ => false,
    })
}

/// Materialize one group field absent from an element (skipping
/// fields the element carries elsewhere — out-of-order lines are the
/// Validator's concern).
fn emit_absent_element(
    g: &SchemaField,
    elem_fields: &HashSet<&str>,
    arr: &str,
    idx: usize,
    eol: &str,
    out: &mut String,
) -> Result<(), PipelineError> {
    let field = ns_arr_parts(g).map(|(_, f)| f).unwrap_or_default();
    if elem_fields.contains(field) {
        return Ok(());
    }
    if !g.optional {
        return Err(PipelineError::App(AppError::RequiredFieldSchema));
    }
    let (ty, value) = materialized_parts(g)?;
    ensure_eol(out, eol);
    out.push_str(&format!("{ty}'{arr}/{idx}::{field}={value}{eol}"));
    Ok(())
}

/// Guarantee `out` is newline-terminated before a materialized line is
/// appended. The source document's final line may lack a trailing
/// newline (`split_inclusive` yields it without one); without this the
/// first materialized absent line would be glued onto that line's
/// value, silently corrupting both.
fn ensure_eol(out: &mut String, eol: &str) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
}

/// Materialize one absent top-level schema field into `out` (or
/// error if it is required). Collection lines (map entries, vector
/// element lines, ns-array element lines) contribute nothing when
/// absent — an empty collection is valid; counts are a Pass-1
/// cardinality concern.
fn emit_absent(
    f: &SchemaField,
    present: &HashSet<&str>,
    eol: &str,
    out: &mut String,
) -> Result<(), PipelineError> {
    if is_collection(f) || present.contains(f.namepath.as_str()) {
        return Ok(());
    }
    if !f.optional {
        return Err(PipelineError::App(AppError::RequiredFieldSchema));
    }
    let (ty, value) = materialized_parts(f)?;
    ensure_eol(out, eol);
    out.push_str(&format!("{ty}'{}={value}{eol}", f.namepath));
    Ok(())
}

/// The metadata token and value of a materialized line (SPEC.md
/// § Null Semantics, Materialization of Absent Fields): the resolved
/// default under the field's own type token — for a union, the first
/// alternative whose group accepts it (`!null` when only the null
/// alternative's empty payload applies). A field whose `.csaiv`
/// carries no retained type item materializes as the identity type
/// `!str`; the parallel scan's type check does not compare names for
/// such fields, only constraints.
fn materialized_parts(f: &SchemaField) -> Result<(String, String), PipelineError> {
    let anno = f.items.iter().find_map(|i| match i {
        crate::anno::Item::Anno(a) => Some(a),
        _ => None,
    });
    match anno {
        Some(a) if !a.union.is_empty() => {
            let name = crate::validator::union_pick(a, &f.default)
                // A compiled schema in which an optional field has
                // neither an applicable default nor a null alternative
                // is itself invalid (SchemaOptionalWithoutDefaultError
                // at schema-compile time); a stale artifact surfaces
                // here at build time.
                .ok_or(PipelineError::App(AppError::SchemaOptionalWithoutDefault))?;
            let value = if name == "null" { "" } else { f.default.as_str() };
            Ok((format!("!{name}"), value.to_string()))
        }
        Some(a) => {
            if !crate::validator::default_applicable(&f.items, &f.default) {
                return Err(PipelineError::App(AppError::SchemaOptionalWithoutDefault));
            }
            let unit = a
                .unit
                .as_ref()
                .map(|u| format!(":{u}"))
                .unwrap_or_default();
            Ok((format!("!{}{unit}", a.type_name), f.default.clone()))
        }
        None => {
            if !crate::validator::default_applicable(&f.items, &f.default) {
                return Err(PipelineError::App(AppError::SchemaOptionalWithoutDefault));
            }
            Ok(("!str".to_string(), f.default.clone()))
        }
    }
}

/// Whether a line is a non-data line — blank, a `#`/`//` comment, or
/// a `.!`/`.?` declaration — which carries no reference definition and
/// no `$`-resolvable value. Leading whitespace is ignored.
fn is_nondata_line(body: &str) -> bool {
    let s = body.trim_start_matches([' ', '\t']);
    s.is_empty()
        || s.starts_with('#')
        || s.starts_with("//")
        || s.starts_with(".!")
        || s.starts_with(".?")
}

/// The namepath of a canonical data line; None for declarations,
/// comments, and blank lines.
fn namepath_of(line: &str) -> Option<String> {
    let s = line
        .trim_end_matches(['\n', '\r'])
        .trim_start_matches([' ', '\t']);
    if is_nondata_line(s) {
        return None;
    }
    let tick = s.find('\'')?;
    let rest = &s[tick + 1..];
    let eq = crate::validator::first_eq(rest)?;
    Some(rest[..eq].to_string())
}

/// Inline every `$namepath` field reference from the table and
/// collapse `$$` → `$`, scanning the whole value.
fn resolve_value(
    value: &str,
    table: &HashMap<String, String>,
) -> Result<String, PipelineError> {
    let b = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'$' {
            // Copy the whole non-`$` run as a str slice: `$` is
            // ASCII, so the run boundaries are char boundaries and
            // multibyte UTF-8 passes through verbatim.
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
                return Err(PipelineError::Other(format!(
                    "unresolved variable in .raiv: {value}"
                )));
            }
            _ => {
                let start = i + 1;
                let end = start + fieldref_len(&b[start..]);
                if end == start {
                    // Lone `$` in a `.raiv` value.
                    return Err(PipelineError::App(AppError::UndefinedReference));
                }
                let target = canonical_ref(&value[start..end]);
                let v = table
                    .get(&target)
                    // Forward or dangling field reference.
                    .ok_or(PipelineError::App(AppError::UndefinedReference))?;
                out.push_str(v);
                i = end;
            }
        }
    }
    Ok(out)
}

/// Length of a leading field-reference token (identifier chars plus
/// `/` and `::`), trimming a dangling separator. Mirrors the
/// compiler's `fieldref_len`.
fn fieldref_len(b: &[u8]) -> usize {
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        // `@` marks an array step, legal only where a step can begin —
        // token start (`@servers/0::name`, a block-qualified reference
        // the compiler emits) or right after `/`. Mid-token `@` is
        // adjacent literal text (`$user@example.com`), ending the ref.
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
    while i > 0 && matches!(b[i - 1], b'/' | b':' | b'@') {
        i -= 1;
    }
    i
}

/// `server/api::host` → `/server/api::host`; `field` → `::field`.
fn canonical_ref(r: &str) -> String {
    if r.contains("::") {
        if r.starts_with('/') {
            r.to_string()
        } else {
            format!("/{r}")
        }
    } else {
        format!("::{r}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commented_out_line_does_not_define_reference() {
        // The port line is commented out, so `$port` has no definition
        // and must fail rather than silently resolve to the disabled
        // value.
        let r = denormalize(".!raiv\n#!int'::port=8080\n!int'::port_backup=$port\n");
        assert!(matches!(
            r,
            Err(PipelineError::App(AppError::UndefinedReference))
        ));
    }

    #[test]
    fn schema_text_field_retypes_str_lines() {
        // The schema type wins in .daiv: a plain (str) line on a
        // !text-declared field is retyped at materialization, so the
        // deployment artifact carries the export semantics.
        let r = Resolver::offline();
        let csaiv = ".!csaiv 1 acme/notes\n!text'::basho=\n";
        r.preload("acme/notes", "csaiv", csaiv.as_bytes().to_vec());
        let raiv = ".!raiv\n.!schema:acme/notes\n!str'::basho=old pond\n";
        let daiv = denormalize_with(raiv, &r).unwrap();
        assert!(daiv.contains("!text'::basho=old pond\n"));
        // Explicit !text passes through untouched.
        let raiv2 = ".!raiv\n.!schema:acme/notes\n!text'::basho=a|:|b\n";
        assert!(denormalize_with(raiv2, &r).unwrap().contains("!text'::basho=a|:|b\n"));
        // A str value carrying a literal `|:|` cannot be coerced —
        // the retype would reinterpret it as line breaks.
        let raiv3 = ".!raiv\n.!schema:acme/notes\n!str'::basho=a|:|b\n";
        assert!(matches!(
            denormalize_with(raiv3, &r),
            Err(PipelineError::App(AppError::DelimiterCollision))
        ));
    }

    #[test]
    fn comment_with_dollar_and_eq_passes_through() {
        // A prose comment carrying an apostrophe, `=`, and `$` is not a
        // data line: it defines no reference, is not `$`-resolved, and
        // passes through verbatim.
        let out = denormalize(".!raiv\n# don't touch: x=$y\n!int'::a=1\n").unwrap();
        assert!(out.contains("# don't touch: x=$y"));
        assert!(out.contains("!int'::a=1"));
    }

    #[test]
    fn fieldref_len_admits_array_element_reference() {
        // `@servers/0::name` is one field-reference token.
        assert_eq!(
            fieldref_len(b"@servers/0::name"),
            "@servers/0::name".len()
        );
    }
}
