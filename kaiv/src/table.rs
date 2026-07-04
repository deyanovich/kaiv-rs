//! Level 2 table machinery: authored table headers on section-open
//! lines (`[/@servers host=!,port=! min=1]`) and the compiled
//! `.csaiv` collection constraint lines they lower to
//! (`/@servers [unique::host,port] [min=1]`) — SPEC.md § Table
//! Declaration Syntax, § Table Declarations in the Compiled Schema.

/// One unique or foreign-key clause. Field names are stored unquoted:
/// a field literally named `min`/`max` is authored quoted (`"min"=!`)
/// to clear the reserved-word rule but participates in namepaths by
/// its plain spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clause {
    /// `[unique::f1,f2]` — the field-value tuple must be distinct
    /// across all elements.
    Unique(Vec<String>),
    /// `[ref::field=/@arr/*::name]` — every `field` value must appear
    /// as a `name` value in the referenced array.
    Ref {
        field: String,
        target_arr: String,
        target_field: String,
    },
}

/// The collection-level constraints of one table declaration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Header {
    /// Pipe-joined clause groups, in authored order. Grouping is
    /// notational — every clause must hold independently.
    pub groups: Vec<Vec<Clause>>,
    pub min: Option<u64>,
    pub max: Option<u64>,
}

/// A compiled collection constraint line: the array it constrains
/// plus its clauses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collection {
    pub array: String,
    pub header: Header,
}

/// Whitespace-split outside quoted names (a quoted field name may
/// contain spaces).
pub fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let (mut start, mut i, mut q) = (0usize, 0usize, false);
    while i < b.len() {
        match b[i] {
            b'"' => {
                if q && b.get(i + 1) == Some(&b'"') {
                    i += 1;
                } else {
                    q = !q;
                }
            }
            b' ' | b'\t' if !q => {
                if start < i {
                    out.push(&s[start..i]);
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < b.len() {
        out.push(&s[start..]);
    }
    out
}

/// Split on an ASCII delimiter outside quoted names.
fn split_q(s: &str, delim: u8) -> Vec<&str> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let (mut start, mut i, mut q) = (0usize, 0usize, false);
    while i < b.len() {
        match b[i] {
            b'"' => {
                if q && b.get(i + 1) == Some(&b'"') {
                    i += 1;
                } else {
                    q = !q;
                }
            }
            x if x == delim && !q => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(&s[start..]);
    out
}

fn all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Parse the authored table header — the whitespace-separated tokens
/// after the array path on a section-open line. `min=N` / `max=N`
/// with all-digit N is always a cardinality clause (the complete
/// reserved-word set); everything else is a pipe-joined group of
/// unique specs and fk clauses.
pub fn parse_header(toks: &[&str]) -> Option<Header> {
    let mut h = Header::default();
    for t in toks {
        if let Some(n) = t.strip_prefix("min=").filter(|n| all_digits(n)) {
            h.min = Some(n.parse().ok()?);
            continue;
        }
        if let Some(n) = t.strip_prefix("max=").filter(|n| all_digits(n)) {
            h.max = Some(n.parse().ok()?);
            continue;
        }
        let mut group = Vec::new();
        for spec in split_q(t, b'|') {
            group.push(parse_spec(spec)?);
        }
        h.groups.push(group);
    }
    Some(h)
}

/// One authored clause spec: `f=!` / `f1=!,f2=!` (unique) or
/// `field=/@arr/*::name` (foreign key).
fn parse_spec(s: &str) -> Option<Clause> {
    if s.ends_with("=!") {
        let mut names = Vec::new();
        for part in split_q(s, b',') {
            // An unquoted `min=!`/`max=!` is reserved — a field so
            // named must be quoted to appear in a table header.
            names.push(parse_name(part.strip_suffix("=!")?, false)?);
        }
        return Some(Clause::Unique(names));
    }
    let parts = split_q(s, b'=');
    let [field, path] = parts.as_slice() else {
        return None;
    };
    let (target_arr, target_field) = parse_fk_path(path)?;
    Some(Clause::Ref {
        field: parse_name(field, false)?,
        target_arr,
        target_field,
    })
}

/// A clause field name — bare or quoted — returned in its unquoted
/// spelling (the spelling canonical namepaths use for bare-able
/// names). Bare `min`/`max` are rejected unless `allow_reserved`
/// (they are unambiguous in compiled `[unique::…]` clauses).
fn parse_name(s: &str, allow_reserved: bool) -> Option<String> {
    if let Some(rest) = s.strip_prefix('"') {
        let inner = rest.strip_suffix('"')?;
        if inner.is_empty() || rest.len() < 2 {
            return None;
        }
        return Some(inner.replace("\"\"", "\""));
    }
    let ok = bare_ok(s) && (allow_reserved || (s != "min" && s != "max"));
    ok.then(|| s.to_string())
}

/// The bare-name grammar: `( ALPHA / "_" ) *( ALPHA / DIGIT / "_" )`.
fn bare_ok(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn render_name(n: &str) -> String {
    if bare_ok(n) {
        n.to_string()
    } else {
        format!("\"{}\"", n.replace('"', "\"\""))
    }
}

/// `fk-path = "/" *( step "/" ) "@" name "/*::" name` →
/// (target array namepath `/…/@arr`, projected field).
fn parse_fk_path(s: &str) -> Option<(String, String)> {
    let i = s.find("/*::")?;
    let (arr, field) = (&s[..i], &s[i + 4..]);
    let steps: Vec<&str> = arr.strip_prefix('/')?.split('/').collect();
    let (last, init) = steps.split_last()?;
    if !init.iter().all(|st| !st.is_empty() && !st.starts_with('@')) {
        return None;
    }
    let name = last.strip_prefix('@')?;
    if name.is_empty() {
        return None;
    }
    Some((arr.to_string(), parse_name(field, true)?))
}

/// Render the compiled clause part of a collection constraint line:
/// groups in authored order (pipe-joined), then `[min=N]` `[max=M]`.
pub fn render_compiled(h: &Header) -> String {
    let mut parts: Vec<String> = h
        .groups
        .iter()
        .map(|g| g.iter().map(render_clause).collect::<Vec<_>>().join("|"))
        .collect();
    if let Some(n) = h.min {
        parts.push(format!("[min={n}]"));
    }
    if let Some(n) = h.max {
        parts.push(format!("[max={n}]"));
    }
    parts.join(" ")
}

fn render_clause(c: &Clause) -> String {
    match c {
        Clause::Unique(names) => format!(
            "[unique::{}]",
            names
                .iter()
                .map(|n| render_name(n))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Clause::Ref {
            field,
            target_arr,
            target_field,
        } => format!(
            "[ref::{}={target_arr}/*::{}]",
            render_name(field),
            render_name(target_field)
        ),
    }
}

/// Parse the clause part of a compiled collection constraint line
/// (everything after the array path).
pub fn parse_compiled(s: &str) -> Option<Header> {
    let mut h = Header::default();
    for t in tokens(s) {
        let mut group = Vec::new();
        for c in split_q(t, b'|') {
            let inner = c.strip_prefix('[')?.strip_suffix(']')?;
            if let Some(r) = inner.strip_prefix("unique::") {
                let names = split_q(r, b',')
                    .iter()
                    .map(|n| parse_name(n, true))
                    .collect::<Option<Vec<_>>>()?;
                group.push(Clause::Unique(names));
            } else if let Some(r) = inner.strip_prefix("ref::") {
                let parts = split_q(r, b'=');
                let [field, path] = parts.as_slice() else {
                    return None;
                };
                let (target_arr, target_field) = parse_fk_path(path)?;
                group.push(Clause::Ref {
                    field: parse_name(field, true)?,
                    target_arr,
                    target_field,
                });
            } else if let Some(n) = inner.strip_prefix("min=").filter(|n| all_digits(n)) {
                h.min = Some(n.parse().ok()?);
            } else if let Some(n) = inner.strip_prefix("max=").filter(|n| all_digits(n)) {
                h.max = Some(n.parse().ok()?);
            } else {
                return None;
            }
        }
        if !group.is_empty() {
            h.groups.push(group);
        }
    }
    Some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = parse_header(&["id=!|host=!,port=!", "min=1", "max=50"]).unwrap();
        assert_eq!(h.min, Some(1));
        assert_eq!(h.max, Some(50));
        assert_eq!(h.groups.len(), 1);
        assert_eq!(h.groups[0].len(), 2);
        let compiled = render_compiled(&h);
        assert_eq!(
            compiled,
            "[unique::id]|[unique::host,port] [min=1] [max=50]"
        );
        assert_eq!(parse_compiled(&compiled).unwrap(), h);
    }

    #[test]
    fn fk_clause() {
        let h = parse_header(&["department=/@departments/*::name"]).unwrap();
        assert_eq!(
            h.groups[0][0],
            Clause::Ref {
                field: "department".into(),
                target_arr: "/@departments".into(),
                target_field: "name".into(),
            }
        );
        assert_eq!(
            render_compiled(&h),
            "[ref::department=/@departments/*::name]"
        );
        // Nested target array.
        let h = parse_header(&["d=/company/@departments/*::name"]).unwrap();
        let Clause::Ref { target_arr, .. } = &h.groups[0][0] else {
            panic!()
        };
        assert_eq!(target_arr, "/company/@departments");
    }

    #[test]
    fn reserved_words() {
        // min=N is always cardinality; a field named min must quote.
        assert!(parse_header(&["min=!"]).is_none());
        let h = parse_header(&["\"min\"=!"]).unwrap();
        assert_eq!(h.groups[0][0], Clause::Unique(vec!["min".into()]));
        // Unquoted spelling in the compiled clause is unambiguous.
        assert_eq!(render_compiled(&h), "[unique::min]");
    }

    #[test]
    fn malformed_specs() {
        assert!(parse_header(&["host="]).is_none());
        assert!(parse_header(&["host"]).is_none());
        assert!(parse_header(&["=!"]).is_none());
        assert!(parse_header(&["f=/@arr/name"]).is_none()); // no /*::
        assert!(parse_header(&["f=/arr/*::name"]).is_none()); // no @
        assert!(parse_header(&["min=1x"]).is_none()); // not digits, not a spec
        assert!(parse_compiled("[key::/^a$/]").is_none()); // map key clauses: later
    }
}
