//! Unit-definition (`.faiv`, Factor AIV) parsing — SPEC.md § Unit
//! Definition Files. A library is a sequence of definition lines,
//! each a dimension (expressed by unit reference, avoiding non-ASCII
//! dimension symbols) plus a conversion factor to the dimension's
//! base unit, above a `&name=` line. Currencies use the dimension
//! `$`, deliberately carry no factor, and may declare a rate-source
//! URL template instead. `&alias=name` defines an alias.

use crate::error::PipelineError;
use crate::lexer::{lex, FileKind, LineKind};
use crate::unit;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct UnitLib {
    /// Library path from `.!kaivunit` (e.g. `astro/units`).
    pub library: String,
    pub units: BTreeMap<String, UnitDef>,
}

#[derive(Debug, Clone, Default)]
pub struct UnitDef {
    /// Dimension by unit reference (`m`, `kg*m/s^2`) or `$` (currency).
    pub dimension: String,
    /// Conversion factor to the dimension's base unit; absent for
    /// currencies (rates are external and time-varying).
    pub factor: Option<String>,
    /// Currency rate-source URL template (`{code}`, `{timestamp}`).
    pub rate_source: Option<String>,
    /// Set when this entry is an alias of another unit in the library.
    pub alias_of: Option<String>,
}

/// One parsed definition metadata line.
pub(crate) struct DefLine {
    pub dimension: String,
    pub factor: Option<String>,
    pub rate_source: Option<String>,
}

/// Parse `DIMENSION [FACTOR | @RATE-URL]`. Grammar-level only — the
/// dimension's unit references are membership-checked by parse_faiv,
/// where earlier same-library definitions are known.
pub(crate) fn parse_def_line(s: &str) -> Option<DefLine> {
    let mut toks = s.split([' ', '\t']).filter(|t| !t.is_empty());
    let dim = toks.next()?;
    if dim != "$" && unit::canonicalize(dim).is_none() {
        return None;
    }
    let mut def = DefLine {
        dimension: dim.to_string(),
        factor: None,
        rate_source: None,
    };
    match toks.next() {
        None => {
            // A factor is required for physical dimensions; only a
            // currency line may stand alone.
            if dim != "$" {
                return None;
            }
        }
        Some(t) => {
            if let Some(url) = t.strip_prefix('@') {
                if dim != "$" || url.is_empty() {
                    return None; // rate sources are currency-only
                }
                def.rate_source = Some(url.to_string());
            } else {
                if dim == "$" || !decimal_ok(t) {
                    return None; // currencies carry no factor
                }
                def.factor = Some(t.to_string());
            }
        }
    }
    if toks.next().is_some() {
        return None;
    }
    Some(def)
}

/// Positive decimal with optional exponent: `1*DIGIT ["." 1*DIGIT]
/// [("e"/"E") ["+"/"-"] 1*DIGIT]`.
fn decimal_ok(s: &str) -> bool {
    let (mantissa, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (s, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((i, f)) => {
            !i.is_empty()
                && !f.is_empty()
                && i.bytes().all(|b| b.is_ascii_digit())
                && f.bytes().all(|b| b.is_ascii_digit())
        }
        None => !mantissa.is_empty() && mantissa.bytes().all(|b| b.is_ascii_digit()),
    };
    let exp_ok = exp.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())
    });
    mantissa_ok && exp_ok
}

pub fn parse_faiv(input: &[u8]) -> Result<UnitLib, PipelineError> {
    let lines = lex(input, FileKind::UnitLib).map_err(PipelineError::Lex)?;
    let mut library = String::new();
    let mut pending: Option<DefLine> = None;
    let mut units: BTreeMap<String, UnitDef> = BTreeMap::new();
    for line in &lines {
        match &line.kind {
            LineKind::Blank | LineKind::Comment(_) | LineKind::Doc(_) => {}
            LineKind::Decl(s) => {
                // `.!kaivunit VERSION LIBRARY-ID`
                if let Some(rest) = s.strip_prefix(".!kaivunit") {
                    let mut toks = rest.split_ascii_whitespace();
                    let _version = toks.next();
                    if let Some(lib) = toks.next() {
                        library = lib.to_string();
                    }
                }
            }
            LineKind::Meta(s) => {
                let def = parse_def_line(s).ok_or_else(|| {
                    PipelineError::Other(format!("bad definition line in .faiv: {s}"))
                })?;
                // Dimension unit references must be built-in or defined
                // earlier in this library.
                if def.dimension != "$" {
                    let known: std::collections::BTreeSet<String> = units.keys().cloned().collect();
                    if !unit::members_ok(&def.dimension, &known) {
                        return Err(PipelineError::Other(format!(
                            "unknown unit in .faiv dimension: {}",
                            def.dimension
                        )));
                    }
                }
                pending = Some(def);
            }
            LineKind::Content { left, value } => {
                // `&name=` (definition) or `&alias=target`.
                let name = left.strip_prefix('&').ok_or_else(|| {
                    PipelineError::Other(format!("unexpected .faiv content line: {left}"))
                })?;
                // unit-name = 1*ALPHA (or a `~`+3-uppercase currency
                // code); the lexer's looser bare-name grammar admits
                // `_`/digits, which unit::parse_expr can never reference.
                if !valid_unit_name(name) {
                    return Err(PipelineError::Other(format!(
                        "invalid unit name in .faiv: &{name} (unit-name is 1*ALPHA)"
                    )));
                }
                let def = if value.is_empty() {
                    let d = pending.take().ok_or_else(|| {
                        PipelineError::Other(format!(
                            "definition &{name}= has no dimension line above it"
                        ))
                    })?;
                    UnitDef {
                        dimension: d.dimension,
                        factor: d.factor,
                        rate_source: d.rate_source,
                        alias_of: None,
                    }
                } else {
                    // A dimension line pairs only with a bare `&name=`
                    // definition; above an alias it defines nothing.
                    if pending.is_some() {
                        return Err(PipelineError::Other(format!(
                            "alias &{name}={value} is preceded by a dimension line that defines nothing"
                        )));
                    }
                    // Alias: the target must already be defined.
                    let target = units.get(*value).cloned().ok_or_else(|| {
                        PipelineError::Other(format!("alias &{name}={value} has no target"))
                    })?;
                    UnitDef {
                        alias_of: Some((*value).to_string()),
                        ..target
                    }
                };
                if units.insert(name.to_string(), def).is_some() {
                    return Err(PipelineError::Other(format!(
                        "duplicate unit definition in .faiv: &{name}"
                    )));
                }
            }
            other => {
                return Err(PipelineError::Other(format!(
                    "unsupported .faiv line: {other:?}"
                )))
            }
        }
    }
    if pending.is_some() {
        return Err(PipelineError::Other(
            "trailing dimension line in .faiv with no &name= definition".into(),
        ));
    }
    if library.is_empty() {
        return Err(PipelineError::Other(
            "missing .!kaivunit declaration in .faiv".into(),
        ));
    }
    Ok(UnitLib { library, units })
}

/// unit-name = 1*ALPHA, or a currency `~` + 3 uppercase letters.
fn valid_unit_name(n: &str) -> bool {
    match n.strip_prefix('~') {
        Some(code) => code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase()),
        None => !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphabetic()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_library() {
        let src = b".!kaivunit 1 astro/units\n\n// Astronomical unit\nm 1.495978707e11\n&au=\n&ua=au\n\n// Custom currency with a rate source\n$ @https://rates.example.com/v1?code={code}&at={timestamp}\n&~XYZ=\n";
        let lib = parse_faiv(src).unwrap();
        assert_eq!(lib.library, "astro/units");
        let au = &lib.units["au"];
        assert_eq!(au.dimension, "m");
        assert_eq!(au.factor.as_deref(), Some("1.495978707e11"));
        assert_eq!(lib.units["ua"].alias_of.as_deref(), Some("au"));
        let xyz = &lib.units["~XYZ"];
        assert_eq!(xyz.dimension, "$");
        assert!(xyz.factor.is_none());
        assert!(xyz.rate_source.as_deref().unwrap().contains("{timestamp}"));
    }

    #[test]
    fn invalid_unit_names_and_orphan_lines_rejected() {
        // Underscore name is not 1*ALPHA.
        assert!(parse_faiv(b".!kaivunit 1 x/u\nm 1.0\n&light_second=\n").is_err());
        // Dimension line above an alias defines nothing.
        assert!(parse_faiv(b".!kaivunit 1 x/u\nm 1000\n&km=\nm 1609.344\n&mile=km\n").is_err());
        // Trailing dimension line with no &name=.
        assert!(parse_faiv(b".!kaivunit 1 x/u\nm 1.0\n").is_err());
    }

    #[test]
    fn def_line_rules() {
        assert!(parse_def_line("m 1.495978707e11").is_some());
        assert!(parse_def_line("kg*m/s^2 4.44822").is_some());
        assert!(parse_def_line("$").is_some()); // bare currency
        assert!(parse_def_line("$ @https://r.example/x?a=b").is_some());
        assert!(parse_def_line("m").is_none()); // factor required
        assert!(parse_def_line("$ 1.5").is_none()); // currencies: no factor
        assert!(parse_def_line("m @https://r.example").is_none()); // rate is $-only
        assert!(parse_def_line("m -2.5").is_none()); // factors are positive
    }
}
