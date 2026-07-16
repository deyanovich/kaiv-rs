//! Programmatic construction of canonical `.daiv` documents.
//!
//! The rest of the crate is text-in/text-out; this module is the one
//! place a consumer builds kaiv *from values* — a query engine
//! emitting typed results, a sensor emitting readings — without
//! hand-formatting lines. The builder emits canonical form directly
//! (`!type?prov'namepath=value`, SPEC.md §1.3.1) and validates each
//! component against the canonical grammar, so its output always
//! lexes: what `finish` returns, `lex(_, FileKind::Data)` accepts.
//!
//! Scope: data documents only (declarations `.!kaiv`, `.?id uri`,
//! and data lines). Values that canonical kaiv cannot carry on a
//! flat line — text containing EOL or NUL, or leading with `$` — are
//! rejected with an error naming the `std/enc` embed route rather
//! than silently mangled.

use crate::error::PipelineError;
use std::fmt::Write as _;

/// One provenance annotation: `?source@timestamp#dpid`, each
/// component optional and independent (SPEC.md §2.4.1).
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    /// A source ID declared via [`DaivBuilder::declare_source`].
    pub source: Option<String>,
    /// Compact ISO 8601 UTC, exactly `YYYYMMDDTHHmmSSZ`.
    pub timestamp: Option<String>,
    /// The data point identifier (`#row-17`-style).
    pub dpid: Option<String>,
}

/// A canonical `.daiv` document under construction.
///
/// ```
/// use kaiv::builder::{DaivBuilder, Provenance};
///
/// let mut b = DaivBuilder::new();
/// b.declare_source("t", "file:titanic.csv").unwrap();
/// b.leaf(
///     "/@rows/0::name",
///     "str",
///     "Ada",
///     Some(&Provenance {
///         source: Some("t".into()),
///         dpid: Some("row-1".into()),
///         ..Default::default()
///     }),
/// )
/// .unwrap();
/// let daiv = b.finish();
/// assert!(daiv.contains("!str?t#row-1'/@rows/0::name=Ada"));
/// kaiv::lex(daiv.as_bytes(), kaiv::FileKind::Data).unwrap();
/// ```
#[derive(Debug, Default)]
pub struct DaivBuilder {
    sources: Vec<(String, String)>,
    types: Vec<String>,
    lines: Vec<String>,
}

impl DaivBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a `.!types` import, emitted in the header — needed
    /// when leaves carry library type names (`std/time/datetime`).
    /// Idempotent per library.
    pub fn declare_types(&mut self, lib: &str) -> Result<(), PipelineError> {
        let ok = !lib.is_empty()
            && lib
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/'));
        if !ok {
            return Err(PipelineError::Other(format!(
                "'{lib}' is not a valid type-library path"
            )));
        }
        if !self.types.iter().any(|t| t == lib) {
            self.types.push(lib.to_string());
        }
        Ok(())
    }

    /// Declare a provenance source (`.?id uri`). The declaration is
    /// emitted in the header regardless of use; `id` must be a
    /// provenance identifier, `uri` a single-line string.
    pub fn declare_source(&mut self, id: &str, uri: &str) -> Result<(), PipelineError> {
        check_ident(id, "provenance source id")?;
        if uri.is_empty() || uri.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
            return Err(PipelineError::Other(format!(
                "provenance source uri for '{id}' must be a non-empty single line"
            )));
        }
        self.sources.push((id.to_string(), uri.to_string()));
        Ok(())
    }

    /// Append one data leaf: `!type?prov'namepath=value`.
    ///
    /// `namepath` is the fully-qualified canonical address — an
    /// optional namespace path (`/server`, `/@rows/0`) and a
    /// terminal `::field`. `type_name` is a type annotation
    /// (`str`, `int`, a lib path like `std/time/datetime`, or a
    /// union `null|str`). A provenance source must have been
    /// declared first.
    pub fn leaf(
        &mut self,
        namepath: &str,
        type_name: &str,
        value: &str,
        prov: Option<&Provenance>,
    ) -> Result<(), PipelineError> {
        self.leaf_with_unit(namepath, type_name, None, value, prov)
    }

    /// Like [`leaf`], with a unit annotation on the type
    /// (`!float:W'...`). The unit must be a well-formed unit
    /// expression (canonicalizable grammar; membership is the
    /// emitting tool's concern, as with type names).
    pub fn leaf_with_unit(
        &mut self,
        namepath: &str,
        type_name: &str,
        unit: Option<&str>,
        value: &str,
        prov: Option<&Provenance>,
    ) -> Result<(), PipelineError> {
        check_namepath(namepath)?;
        check_type_name(type_name)?;
        if let Some(u) = unit {
            if crate::unit::canonicalize(u).is_none() {
                return Err(PipelineError::Other(format!(
                    "'{u}' is not a well-formed unit expression"
                )));
            }
        }
        check_value(namepath, value)?;
        let mut line = String::new();
        write!(line, "!{type_name}").expect("string write");
        if let Some(u) = unit {
            write!(line, ":{u}").expect("string write");
        }
        if let Some(p) = prov {
            line.push_str(&self.render_prov(p)?);
        }
        write!(line, "'{namepath}={value}").expect("string write");
        self.lines.push(line);
        Ok(())
    }

    /// The finished canonical document: the `.!kaiv 1` declaration,
    /// type-library imports, source declarations, then the data
    /// lines, one leaf per line.
    pub fn finish(&self) -> String {
        let mut out = String::from(".!kaiv 1\n");
        for lib in &self.types {
            let _ = writeln!(out, ".!types {lib}");
        }
        for (id, uri) in &self.sources {
            let _ = writeln!(out, ".?{id} {uri}");
        }
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    fn render_prov(&self, p: &Provenance) -> Result<String, PipelineError> {
        let mut out = String::new();
        if let Some(src) = &p.source {
            check_ident(src, "provenance source id")?;
            if !self.sources.iter().any(|(id, _)| id == src) {
                return Err(PipelineError::Other(format!(
                    "provenance source '{src}' is not declared (declare_source first)"
                )));
            }
            write!(out, "?{src}").expect("string write");
        }
        if let Some(ts) = &p.timestamp {
            check_timestamp(ts)?;
            if out.is_empty() {
                out.push('?');
            }
            write!(out, "@{ts}").expect("string write");
        }
        if let Some(dpid) = &p.dpid {
            check_ident(dpid, "data point identifier")?;
            if out.is_empty() {
                out.push('?');
            }
            write!(out, "#{dpid}").expect("string write");
        }
        Ok(out)
    }
}

/// Identifier charset for provenance ids and dpids: the spec's
/// `#row-17` / `#request-42` shape.
fn check_ident(s: &str, what: &str) -> Result<(), PipelineError> {
    // prov-ident = ( ALPHA / DIGIT / "_" ) *( ALPHA / DIGIT / "_" / "-" )
    // — no '.', and '-' only after the first character (SPEC.md §10.4).
    let b = s.as_bytes();
    let ok = !b.is_empty()
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(PipelineError::Other(format!(
            "{what} '{s}' is not a valid identifier ([A-Za-z0-9_][A-Za-z0-9_-]*)"
        )))
    }
}

/// `YYYYMMDDTHHmmSSZ`, exactly 16 characters (SPEC.md §2.4.1).
fn check_timestamp(ts: &str) -> Result<(), PipelineError> {
    let b = ts.as_bytes();
    let ok = b.len() == 16
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z';
    if ok {
        Ok(())
    } else {
        Err(PipelineError::Other(format!(
            "provenance timestamp '{ts}' is not YYYYMMDDTHHmmSSZ"
        )))
    }
}

/// A canonical namepath: optional `/`-led namespace segments (an
/// `@`-marked array segment may be followed by integer elements),
/// then the terminal `::field`.
fn check_namepath(namepath: &str) -> Result<(), PipelineError> {
    let bad = |why: &str| Err(PipelineError::Other(format!("namepath '{namepath}' {why}")));
    // Delegate the character/segment/quote grammar to the lexer's own
    // key validator: the synthetic `!x'` prefix makes the first `'` the
    // metadata delimiter so `namepath` is validated exactly as a
    // canonical key — the language `lex(_, FileKind::Data)` on finish()
    // output must accept.
    if crate::lexer::check_key(&format!("!x'{namepath}"), 0, crate::lexer::FileKind::Data).is_err() {
        return bad("is not a valid canonical namepath");
    }
    // check_key silently strips a trailing `:`/`+`/`;` as an
    // assignment-operator remnant; a canonical `'namepath=` line must
    // not end in one, else `::a:` would re-lex as a `:=` line.
    if namepath.ends_with([':', '+', ';']) {
        return bad("ends with an assignment-operator sigil");
    }
    // Builder policy: exactly one terminal `::field` (outside quotes),
    // non-empty field, namespace empty or `/`-led.
    let projs = unquoted_projections(namepath);
    if projs.len() != 1 {
        return bad("needs exactly one terminal ::field");
    }
    let split = projs[0];
    let (ns, field) = (&namepath[..split], &namepath[split + 2..]);
    if field.is_empty() {
        return bad("has an empty terminal field");
    }
    if !ns.is_empty() && !ns.starts_with('/') {
        return bad("has a namespace part that does not start with '/'");
    }
    Ok(())
}

/// Byte offsets of every `::` occurring outside a quoted name.
fn unquoted_projections(s: &str) -> Vec<usize> {
    let b = s.as_bytes();
    let mut in_quote = false;
    let mut positions = Vec::new();
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
                positions.push(i);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    positions
}

/// A type annotation: a name, a lib path (`std/time/datetime`), or a
/// union (`null|str`).
fn check_type_name(t: &str) -> Result<(), PipelineError> {
    let ok = !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '|'));
    if ok {
        Ok(())
    } else {
        Err(PipelineError::Other(format!(
            "type annotation '{t}' is not a valid type name"
        )))
    }
}

/// A value must fit a flat canonical line: no line breaks or NUL, no
/// leading `$` (the reference sigil). Anything else goes through the
/// `std/enc` embed route, which this builder deliberately does not
/// guess at.
fn check_value(namepath: &str, value: &str) -> Result<(), PipelineError> {
    if value.starts_with('$') {
        return Err(PipelineError::Other(format!(
            "value for '{namepath}' starts with '$' (reserved); embed it via std/enc"
        )));
    }
    if value.chars().any(|c| c == '\n' || c == '\r' || c == '\0') {
        return Err(PipelineError::Other(format!(
            "value for '{namepath}' contains a line break or NUL; embed it via std/enc"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::{lex, FileKind};

    fn prov(src: &str, dpid: &str) -> Provenance {
        Provenance {
            source: Some(src.into()),
            timestamp: None,
            dpid: Some(dpid.into()),
        }
    }

    #[test]
    fn builds_the_conformance_shape() {
        // Mirrors conformance valid/015-provenance's canonical form.
        let mut b = DaivBuilder::new();
        b.declare_source("sensor1", "https://sensors.example.com/1")
            .unwrap();
        b.leaf(
            "::temp",
            "str",
            "100",
            Some(&Provenance {
                source: Some("sensor1".into()),
                timestamp: Some("20250115T093000Z".into()),
                dpid: Some("req-42".into()),
            }),
        )
        .unwrap();
        assert_eq!(
            b.finish(),
            ".!kaiv 1\n.?sensor1 https://sensors.example.com/1\n\
             !str?sensor1@20250115T093000Z#req-42'::temp=100\n"
        );
    }

    #[test]
    fn output_always_lexes() {
        let mut b = DaivBuilder::new();
        b.declare_source("t", "file:cart.json").unwrap();
        b.leaf("/@rows/0::name", "str", "Ada", Some(&prov("t", "row-1")))
            .unwrap();
        b.leaf("/@rows/0::total", "int", "12", Some(&prov("t", "row-1")))
            .unwrap();
        b.leaf("/@rows/1::total", "null", "", None).unwrap();
        b.leaf("/server::host", "null|str", "", None).unwrap();
        let daiv = b.finish();
        lex(daiv.as_bytes(), FileKind::Data).expect("built .daiv must lex");
    }

    #[test]
    fn rejects_invalid_components() {
        let mut b = DaivBuilder::new();
        // namepaths
        assert!(b.leaf("no-projection", "str", "x", None).is_err());
        assert!(b.leaf("::a::b", "str", "x", None).is_err());
        assert!(b.leaf("server::host", "str", "x", None).is_err());
        assert!(b.leaf("/a//b::c", "str", "x", None).is_err());
        assert!(b.leaf("::fie'ld", "str", "x", None).is_err());
        // segment grammar now enforced via the lexer's own validator
        assert!(b.leaf("::my-field", "str", "x", None).is_err()); // hyphen
        assert!(b.leaf("::a b", "str", "x", None).is_err()); // space
        assert!(b.leaf("::café", "str", "x", None).is_err()); // non-ascii
        assert!(b.leaf("::a:", "str", "x", None).is_err()); // trailing op sigil
        // idents: prov-ident forbids '.' and a leading '-'
        assert!(b.declare_source("row.17", "u").is_err());
        assert!(b.declare_source("-lead", "u").is_err());
        // values
        assert!(b.leaf("::a", "str", "two\nlines", None).is_err());
        assert!(b.leaf("::a", "str", "$ref", None).is_err());
        // types and provenance
        assert!(b.leaf("::a", "st r", "x", None).is_err());
        assert!(b
            .leaf("::a", "str", "x", Some(&prov("undeclared", "d")))
            .is_err());
        assert!(b.declare_source("bad id", "u").is_err());
        let ts = Provenance {
            timestamp: Some("2025-01-15T09:30Z".into()),
            ..Default::default()
        };
        assert!(b.leaf("::a", "str", "x", Some(&ts)).is_err());
        // nothing invalid leaked into the document
        assert_eq!(b.finish(), ".!kaiv 1\n");
    }

    #[test]
    fn accepts_quoted_namepaths_that_lex() {
        let mut b = DaivBuilder::new();
        b.leaf(r#"::"a=b""#, "str", "x", None).unwrap();
        b.leaf(r#"::"a::b""#, "str", "y", None).unwrap();
        let daiv = b.finish();
        lex(daiv.as_bytes(), FileKind::Data).expect("built .daiv must lex");
    }
}
