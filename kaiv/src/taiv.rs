//! Type-library (`.taiv`) parsing and the embedded `std/core`
//! (SPEC.md § Type Library Files, § The `std/core` Standard Library).
//! A library is a sequence of constraint lines accumulating above each
//! `&name=` definition; `std/core` ships embedded (SPEC.md: bundled,
//! never fetched) and is parsed by the same path as any other library.

use crate::anno::{parse_constraint_items, Item};
use crate::error::PipelineError;
use crate::lexer::{lex, FileKind, LineKind};
use std::collections::BTreeMap;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct TypeLib {
    /// Library path from `.!kaivtype` (e.g. `std/core`, `acme/net`).
    pub library: String,
    /// Type name → definition, unlowered (base-type references and
    /// `&name` same-library references still symbolic).
    pub types: BTreeMap<String, TypeDef>,
}

#[derive(Debug, Clone, Default)]
pub struct TypeDef {
    pub items: Vec<Item>,
    /// The type's default value — the `&name=` line's right side. A
    /// kaiv value is never absent, only empty; the empty string is
    /// the degenerate (usually inert) default.
    pub default: String,
}

pub fn parse_taiv(input: &[u8]) -> Result<TypeLib, PipelineError> {
    let lines = lex(input, FileKind::TypeLib).map_err(PipelineError::Lex)?;
    let mut library = String::new();
    let mut pending: Vec<Item> = Vec::new();
    let mut types = BTreeMap::new();
    for line in &lines {
        match &line.kind {
            LineKind::Blank | LineKind::Comment(_) | LineKind::Doc(_) => {}
            LineKind::Decl(s) => {
                // `.!kaivtype VERSION LIBRARY-ID`
                if let Some(rest) = s.strip_prefix(".!kaivtype") {
                    let mut toks = rest.split_ascii_whitespace();
                    let _version = toks.next();
                    if let Some(lib) = toks.next() {
                        library = lib.to_string();
                    }
                }
            }
            LineKind::Meta(s) => {
                let items = parse_constraint_items(s).ok_or_else(|| {
                    PipelineError::Other(format!("bad constraint line in .taiv: {s}"))
                })?;
                pending.extend(items);
            }
            LineKind::Content { left, value } => {
                // `&name=` — the accumulated constraint lines are the
                // definition; the right side is the type's default.
                let name = left.strip_prefix('&').ok_or_else(|| {
                    PipelineError::Other(format!("unexpected .taiv content line: {left}"))
                })?;
                let def = TypeDef {
                    items: std::mem::take(&mut pending),
                    default: (*value).to_string(),
                };
                if types.insert(name.to_string(), def).is_some() {
                    return Err(PipelineError::Other(format!(
                        "duplicate type definition in .taiv: &{name}"
                    )));
                }
            }
            other => {
                return Err(PipelineError::Other(format!(
                    "unsupported .taiv line: {other:?}"
                )))
            }
        }
    }
    if !pending.is_empty() {
        return Err(PipelineError::Other(
            "trailing constraint line(s) in .taiv with no &name= definition".into(),
        ));
    }
    if library.is_empty() {
        return Err(PipelineError::Other(
            "missing .!kaivtype declaration in .taiv".into(),
        ));
    }
    Ok(TypeLib { library, types })
}

/// The embedded `std/core` library — always available, no lookup.
pub fn std_core() -> &'static TypeLib {
    static CORE: OnceLock<TypeLib> = OnceLock::new();
    CORE.get_or_init(|| {
        parse_taiv(include_bytes!("std_core.taiv")).expect("embedded std/core.taiv is valid")
    })
}

/// The embedded `std/enc` encoding library — b64-derived named types
/// that type the decoded payload (`std/enc/json`, …). Unlike
/// `std/core` it is not implicitly imported: documents opt in with
/// `.!types std/enc`.
pub fn std_enc() -> &'static TypeLib {
    static ENC: OnceLock<TypeLib> = OnceLock::new();
    ENC.get_or_init(|| {
        parse_taiv(include_bytes!("std_enc.taiv")).expect("embedded std/enc.taiv is valid")
    })
}

/// The embedded `std/time` library — RFC 3339 shapes with `..time`
/// ordering, mirroring the four TOML datetime flavors. Imported
/// explicitly, like `std/enc`.
pub fn std_time() -> &'static TypeLib {
    static TIME: OnceLock<TypeLib> = OnceLock::new();
    TIME.get_or_init(|| {
        parse_taiv(include_bytes!("std_time.taiv")).expect("embedded std/time.taiv is valid")
    })
}

/// The embedded `std/num` library — IEEE non-finite marker values as
/// enum types (`inf` = {inf,-inf}, `nan` = {nan}); kaiv floats stay
/// finite, and extended reals are the union idiom `!float|std/num/inf`.
pub fn std_num() -> &'static TypeLib {
    static NUM: OnceLock<TypeLib> = OnceLock::new();
    NUM.get_or_init(|| {
        parse_taiv(include_bytes!("std_num.taiv")).expect("embedded std/num.taiv is valid")
    })
}

/// Embedded libraries, resolvable without any registry lookup.
pub fn embedded(lib: &str) -> Option<&'static TypeLib> {
    match lib {
        "std/core" => Some(std_core()),
        "std/enc" => Some(std_enc()),
        "std/num" => Some(std_num()),
        "std/time" => Some(std_time()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anno::Constraint;

    #[test]
    fn embedded_time_parses() {
        let time = std_time();
        assert_eq!(time.library, "std/time");
        for t in ["datetime", "localdatetime", "date", "time"] {
            assert!(time.types.contains_key(t), "{t} missing from std/time");
        }
    }

    #[test]
    fn embedded_num_parses() {
        let num = std_num();
        assert_eq!(num.library, "std/num");
        assert!(num.types.contains_key("inf"));
        assert!(num.types.contains_key("nan"));
    }

    #[test]
    fn embedded_core_parses() {
        let core = std_core();
        assert_eq!(core.library, "std/core");
        for t in ["int", "float", "bool", "null", "b64"] {
            assert!(core.types.contains_key(t), "{t} missing from std/core");
        }
        // Base64url, unpadded (RFC 4648 section 5).
        let b64 = &core.types["b64"];
        assert!(matches!(
            &b64.items[0],
            Item::Constraint(Constraint::Pattern(p)) if p == "^[A-Za-z0-9_-]*$"
        ));
        assert_eq!(b64.default, ""); // core types carry the inert default
    }
}
