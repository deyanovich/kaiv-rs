//! Schema inference: canonical kaiv text → an authored `.saiv` that
//! the example document validates against. Types come from the
//! canonical annotations (str/int/float/bool, std library types via
//! `&name` + `.!types`); `{int,float}` widens to float and null joins
//! as a union alternative; scalar arrays become `;=` vector
//! declarations, namespace arrays become `[/@name]…[]` blocks with
//! per-element-field optionality (a field missing from some elements
//! infers `?=`). Shapes outside the compiled subset (nested arrays,
//! structure inside elements) are skipped with a comment — inferred
//! schemas are relaxed, so skipped fields still validate.

use crate::error::PipelineError;
use std::collections::{BTreeMap, BTreeSet};

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

/// One inferred entry, in first-seen document order.
enum Entry {
    Scalar {
        key: String,
        tys: BTreeSet<String>,
    },
    Vector {
        path: String,
        tys: BTreeSet<String>,
    },
    Table {
        path: String,
        // field → (types, element-presence count), first-seen order.
        fields: Vec<(String, BTreeSet<String>, usize)>,
        elements: BTreeSet<usize>,
    },
    Skipped(String),
}

pub fn infer(canonical: &str, name: &str) -> Result<String, PipelineError> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new(); // path → entries idx

    for raw in canonical.lines() {
        let s = raw.trim_start_matches([' ', '\t']);
        if s.is_empty()
            || s.starts_with('#')
            || s.starts_with("//")
            || s.starts_with(".!")
            || s.starts_with(".?")
        {
            continue;
        }
        let tick = s
            .find('\'')
            .ok_or_else(|| err(format!("not a canonical line: {s}")))?;
        let a = crate::anno::parse_annotation(&s[..tick])
            .ok_or_else(|| err(format!("bad metadata prefix: {s}")))?;
        let rest = &s[tick + 1..];
        // Quote-aware: a quoted name may contain `=`.
        let eq = crate::validator::first_eq(rest)
            .ok_or_else(|| err(format!("canonical line without =: {s}")))?;
        let np = &rest[..eq];
        let ty = a.type_name.clone();

        match classify(np) {
            Shape::Scalar => {
                let key = np.strip_prefix("::").unwrap_or(np).to_string();
                match index.get(np) {
                    Some(&i) => {
                        if let Entry::Scalar { tys, .. } = &mut entries[i] {
                            tys.insert(ty);
                        }
                    }
                    None => {
                        index.insert(np.to_string(), entries.len());
                        entries.push(Entry::Scalar {
                            key,
                            tys: [ty].into(),
                        });
                    }
                }
            }
            Shape::VectorElem { path } => match index.get(&path) {
                Some(&i) => match &mut entries[i] {
                    Entry::Vector { tys, .. } => {
                        tys.insert(ty);
                    }
                    // The array also has namespace elements: a mixed
                    // shape outside the compiled subset — demote.
                    e => *e = Entry::Skipped(path),
                },
                None => {
                    index.insert(path.clone(), entries.len());
                    entries.push(Entry::Vector {
                        path,
                        tys: [ty].into(),
                    });
                }
            },
            Shape::TableField { path, idx, field } => {
                let i = *index.entry(path.clone()).or_insert_with(|| {
                    entries.push(Entry::Table {
                        path: path.clone(),
                        fields: Vec::new(),
                        elements: BTreeSet::new(),
                    });
                    entries.len() - 1
                });
                match &mut entries[i] {
                    Entry::Table {
                        fields, elements, ..
                    } => {
                        elements.insert(idx);
                        match fields.iter_mut().find(|(f, _, _)| *f == field) {
                            Some((_, tys, count)) => {
                                tys.insert(ty);
                                *count += 1;
                            }
                            None => fields.push((field, [ty].into(), 1)),
                        }
                    }
                    // Scalar elements seen earlier: mixed shape — demote.
                    e => *e = Entry::Skipped(path),
                }
            }
            Shape::Unsupported => {
                let i = *index.entry(np.to_string()).or_insert_with(|| {
                    entries.push(Entry::Skipped(np.to_string()));
                    entries.len() - 1
                });
                entries[i] = Entry::Skipped(np.to_string());
            }
        }
    }

    // Assemble the authored schema.
    let mut body = String::new();
    let mut imports: BTreeSet<String> = BTreeSet::new();
    for e in &entries {
        match e {
            Entry::Scalar { key, tys } => {
                body.push_str(&field_lines(key, tys, false, &mut imports));
            }
            Entry::Vector { path, tys } => {
                let anno = anno_line(tys, &mut imports);
                body.push_str(&format!("{anno}{path};=\n"));
            }
            Entry::Table {
                path,
                fields,
                elements,
            } => {
                body.push_str(&format!("[{path}]\n"));
                for (field, tys, count) in fields {
                    let optional = *count < elements.len();
                    body.push_str(&field_lines(field, tys, optional, &mut imports));
                }
                body.push_str("[]\n");
            }
            Entry::Skipped(np) => {
                body.push_str(&format!("// skipped (shape not expressible yet): {np}\n"));
            }
        }
    }
    let mut out = format!(".!kaivschema 1 {name}\n");
    for lib in &imports {
        out.push_str(&format!(".!types {lib}\n"));
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
}

/// annotation line (possibly empty) + `key{?}=`.
fn field_lines(
    key: &str,
    tys: &BTreeSet<String>,
    optional: bool,
    imports: &mut BTreeSet<String>,
) -> String {
    let anno = anno_line(tys, imports);
    // A line-leading bare `re` is the reserved pattern-literal
    // introducer in schema files -- spell the field quoted.
    let key = if key == "re" { "\"re\"" } else { key };
    format!("{anno}{key}{}=\n", if optional { "?" } else { "" })
}

/// The annotation for an observed type set: `{int,float}` widens to
/// float, null joins as a union alternative (a lone null infers
/// nullable string), plain str stays unannotated, and std library
/// types use `&name` with a `.!types` import.
fn anno_line(tys: &BTreeSet<String>, imports: &mut BTreeSet<String>) -> String {
    let had_null = tys.contains("null");
    let mut list: Vec<String> = tys.iter().filter(|t| *t != "null").cloned().collect();
    if list.contains(&"float".to_string()) {
        list.retain(|t| t != "int"); // widen
    }
    if list.is_empty() {
        list.push("str".to_string()); // only null observed
    }
    for t in &list {
        if let Some((lib, _)) = t.rsplit_once('/') {
            imports.insert(lib.to_string());
        }
    }
    if had_null {
        return format!("!null|{}\n", list.join("|"));
    }
    if list.len() > 1 {
        return format!("!{}\n", list.join("|"));
    }
    match list[0].as_str() {
        "str" => String::new(),
        t if !t.contains('/') => format!("!{t}\n"),
        t => {
            // &name form for library types (import collected above).
            let name = t.rsplit_once('/').map(|(_, n)| n).unwrap_or(t);
            format!("&{name}\n")
        }
    }
}

enum Shape {
    Scalar,
    VectorElem {
        path: String,
    },
    TableField {
        path: String,
        idx: usize,
        field: String,
    },
    Unsupported,
}

/// Classify a canonical namepath by its array structure, quote-aware.
fn classify(np: &str) -> Shape {
    // Raw segment spans, split on `/` and `::` outside quoted names.
    let b = np.as_bytes();
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut q = false;
    while i < b.len() {
        match b[i] {
            b'"' => {
                if q && b.get(i + 1) == Some(&b'"') {
                    i += 1;
                } else {
                    q = !q;
                }
            }
            b'/' if !q => {
                segs.push((start, i));
                start = i + 1;
            }
            b':' if !q && b.get(i + 1) == Some(&b':') => {
                segs.push((start, i));
                i += 1;
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    segs.push((start, b.len()));
    segs.retain(|(a, z)| a != z);
    let seg = |k: usize| &np[segs[k].0..segs[k].1];

    let arrays: Vec<usize> = (0..segs.len())
        .filter(|&k| seg(k).starts_with('@'))
        .collect();
    match arrays.len() {
        0 => Shape::Scalar,
        1 => {
            let k = arrays[0];
            let digits = |t: &str| !t.is_empty() && t.bytes().all(|c| c.is_ascii_digit());
            if k + 2 == segs.len() && digits(seg(k + 1)) {
                // …/@name::idx — scalar-array element.
                Shape::VectorElem {
                    path: np[..segs[k].1].to_string(),
                }
            } else if k + 3 == segs.len() && digits(seg(k + 1)) {
                // …/@name/idx::field — namespace-array element field.
                Shape::TableField {
                    path: np[..segs[k].1].to_string(),
                    idx: seg(k + 1).parse().unwrap_or(0),
                    field: seg(k + 2).to_string(),
                }
            } else {
                Shape::Unsupported
            }
        }
        _ => Shape::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_types_arrays_and_tables() {
        let daiv = ".!kaiv 1\n!str'::name=eu1\n!int'::port=8443\n!float'::ratio=2.5\n!null'::note=\n!std/time/datetime'::when=2026-07-03T21:00:00Z\n!int'/@ports::0=80\n!int'/@ports::1=443\n!str'/@servers/0::host=a\n!int'/@servers/0::port=1\n!str'/@servers/1::host=b\n!int'/@servers/1::port=2\n!str'/@servers/1::role=spare\n";
        let saiv = infer(daiv, "acme/cluster").unwrap();
        assert!(saiv.starts_with(".!kaivschema 1 acme/cluster\n.!types std/time\n"));
        assert!(saiv.contains("name=\n"));
        assert!(saiv.contains("!int\nport=\n"));
        assert!(saiv.contains("!null|str\nnote=\n"));
        assert!(saiv.contains("&datetime\nwhen=\n"));
        assert!(saiv.contains("!int\n/@ports;=\n"));
        assert!(saiv.contains("[/@servers]\n"));
        assert!(saiv.contains("!str\nrole?=\n") || saiv.contains("role?=\n"));
        // The example validates against its own inferred schema.
        let c = crate::compile_schema(saiv.as_bytes()).unwrap();
        let sc = crate::parse_csaiv(&c).unwrap();
        assert_eq!(crate::validate(daiv, &sc), Ok(()));
    }

    #[test]
    fn widening_and_unions() {
        let daiv = ".!kaiv 1\n!int'/@xs::0=1\n!float'/@xs::1=2.5\n!null'/@xs::2=\n";
        let saiv = infer(daiv, "t").unwrap();
        assert!(saiv.contains("!null|float\n/@xs;=\n"));
    }

    #[test]
    fn mixed_scalar_and_ns_elements_demote_to_skipped() {
        // An array with both scalar elements and namespace elements
        // is outside the compiled subset — infer skips it and the
        // (relaxed) schema still validates the document.
        let daiv = ".!kaiv 1\n!int'/@mixed::0=1\n!int'/@mixed/1::x=2\n!str'::after=z\n";
        let saiv = infer(daiv, "t").unwrap();
        assert!(saiv.contains("// skipped"));
        assert!(!saiv.contains("/@mixed;="));
        let c = crate::compile_schema(saiv.as_bytes()).unwrap();
        let sc = crate::parse_csaiv(&c).unwrap();
        assert_eq!(crate::validate(daiv, &sc), Ok(()));
    }

    #[test]
    fn exotic_names_survive_inference() {
        // Quoted names containing `=` split quote-aware; a field
        // named `re` is spelled quoted (reserved in schema files).
        let daiv = ".!kaiv 1\n!str'::\"a=b\"=x\n!str'::re=y\n";
        let saiv = infer(daiv, "t").unwrap();
        assert!(saiv.contains("\"a=b\"=\n"));
        assert!(saiv.contains("\"re\"=\n"));
        let c = crate::compile_schema(saiv.as_bytes()).unwrap();
        let sc = crate::parse_csaiv(&c).unwrap();
        assert_eq!(crate::validate(daiv, &sc), Ok(()));
    }

    #[test]
    fn unsupported_shapes_skip_with_comment() {
        let daiv = ".!kaiv 1\n!str'/@m/0/@inner::0=x\n";
        let saiv = infer(daiv, "t").unwrap();
        assert!(saiv.contains("// skipped"));
        let c = crate::compile_schema(saiv.as_bytes()).unwrap();
        let sc = crate::parse_csaiv(&c).unwrap();
        // Relaxed schema: the skipped field still validates.
        assert_eq!(crate::validate(daiv, &sc), Ok(()));
    }

    #[cfg(feature = "json")]
    #[test]
    fn self_validation_invariant_via_json() {
        let src = br#"{"service":"billing","port":8443,"limits":{"rps":500,"regions":["eu","us"]},"servers":[{"host":"a","port":1},{"host":"b","port":2,"role":"spare"}],"when":null}"#;
        let authored = crate::json::import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let saiv = infer(&daiv, "inferred").unwrap();
        let c = crate::compile_schema(saiv.as_bytes()).unwrap();
        let sc = crate::parse_csaiv(&c).unwrap();
        assert_eq!(crate::validate(&daiv, &sc), Ok(()));
    }
}
