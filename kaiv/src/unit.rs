//! Compound-unit canonicalization (SPEC.md § Canonical form:
//! ASCII-sorted factors) and built-in unit membership (§ Built-in
//! units). `*` places a factor in the numerator, `/` in the
//! denominator; negative authored exponents flip sides; repeated
//! factors collapse; shared factors cancel; factors sort by base
//! name; `1` is the dimensionless identity. Every factor must be a
//! built-in unit or a well-formed currency — `kfaiv.com` resolution
//! is not implemented in this seed, so unknown means invalid.

use std::collections::BTreeMap;

/// SPEC.md § Built-in units — SI base units.
const SI_BASE: &[&str] = &["m", "kg", "s", "A", "K", "mol", "cd"];

/// Named SI-derived units (coherent, factor 1). ASCII spellings per
/// the spec: `u` = micro, `ohm` = Ω.
const SI_DERIVED: &[&str] = &[
    "rad", "sr", "Hz", "N", "Pa", "J", "W", "C", "V", "F", "ohm", "S", "Wb", "T", "H", "lm", "lx",
    "Bq", "Gy", "Sv", "kat",
];

/// Curated non-SI / US-imperial units.
const NON_SI: &[&str] = &[
    "min", "h", "d", "t", "L", "in", "ft", "yd", "mi", "nmi", "lb", "oz", "gal",
];

/// SI decimal prefixes ("da" first so the two-char prefix wins).
const PREFIXES: &[&str] = &[
    "da", "Y", "Z", "E", "P", "T", "G", "M", "k", "h", "d", "c", "m", "u", "n", "p", "f", "a", "z",
    "y",
];

/// Prefixes attach to SI base units (gram `g`, not `kg`, for mass),
/// named SI-derived units, and the litre — never to non-SI units or
/// currencies (SPEC.md § Built-in units, prefix rule).
fn prefixable(stem: &str) -> bool {
    stem == "g"
        || stem == "L"
        || SI_DERIVED.contains(&stem)
        || (SI_BASE.contains(&stem) && stem != "kg")
}

/// Is `name` a member of the frozen built-in set? (Currencies are
/// shape-checked in the grammar, not membership-checked — the ISO
/// register is external and time-varying.)
pub fn builtin(name: &str) -> bool {
    if name.starts_with('~') {
        return false;
    }
    if name == "g"
        || SI_BASE.contains(&name)
        || SI_DERIVED.contains(&name)
        || NON_SI.contains(&name)
    {
        return true;
    }
    PREFIXES
        .iter()
        .any(|p| name.strip_prefix(p).is_some_and(prefixable))
}

/// Membership check for every factor of a grammatically valid unit
/// expression: built-in, a well-formed currency, or one of the
/// document's imported custom units (`.!units` → `.faiv` definitions).
pub fn members_ok(expr: &str, customs: &std::collections::BTreeSet<String>) -> bool {
    let Some(names) = factor_names(expr) else {
        return false;
    };
    names
        .iter()
        .all(|n| n == "1" || n.starts_with('~') || builtin(n) || customs.contains(n))
}

/// The factor names of a unit expression (grammar check included).
fn factor_names(expr: &str) -> Option<Vec<String>> {
    canonicalize(expr)?;
    let mut names = Vec::new();
    for part in expr.split(['*', '/']) {
        let name = part.split('^').next().unwrap_or(part);
        names.push(name.to_string());
    }
    Some(names)
}

pub fn canonicalize(expr: &str) -> Option<String> {
    Some(format_exps(&parse_expr(expr)?))
}

/// Parse a unit expression into name → signed exponent (grammar
/// check included; the dimensionless `1` contributes nothing).
fn parse_expr(expr: &str) -> Option<BTreeMap<String, i64>> {
    if expr.is_empty() {
        return None;
    }
    let cs: Vec<char> = expr.chars().collect();
    let mut i = 0;
    let mut exps: BTreeMap<String, i64> = BTreeMap::new();
    let mut op = '*';
    loop {
        // factor: optional ~, letters (or the literal "1"), optional ^exp
        let name = if cs.get(i) == Some(&'1') {
            i += 1;
            "1".to_string()
        } else {
            let start = i;
            if cs.get(i) == Some(&'~') {
                i += 1;
            }
            while i < cs.len() && cs[i].is_ascii_alphabetic() {
                i += 1;
            }
            if i == start || (cs[start] == '~' && i == start + 1) {
                return None;
            }
            let name: String = cs[start..i].iter().collect();
            // Grammar-level checks only: currency shape is fixed
            // (`~` + 3 uppercase); name membership is context-dependent
            // (imported custom units) and checked via members_ok.
            if let Some(code) = name.strip_prefix('~') {
                if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_uppercase()) {
                    return None;
                }
            }
            name
        };
        let mut exp: i64 = 1;
        if cs.get(i) == Some(&'^') {
            i += 1;
            let neg = cs.get(i) == Some(&'-');
            if neg {
                i += 1;
            }
            let start = i;
            while i < cs.len() && cs[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return None;
            }
            let n: i64 = cs[start..i].iter().collect::<String>().parse().ok()?;
            if n == 0 {
                return None;
            }
            exp = if neg { -n } else { n };
        }
        if name != "1" {
            let signed = if op == '*' { exp } else { -exp };
            // A parsed exponent is a positive magnitude, so negation is
            // safe; accumulation across repeated factors can overflow —
            // a pathological unit is unscalable (None), not a panic.
            let slot = exps.entry(name).or_insert(0);
            *slot = slot.checked_add(signed)?;
        }
        if i >= cs.len() {
            break;
        }
        op = cs[i];
        if op != '*' && op != '/' {
            return None;
        }
        i += 1;
    }
    Some(exps)
}

/// Format an exponent map in canonical form: ASCII-sorted factors,
/// numerator then `/`-chained denominator, `1` when empty.
fn format_exps(exps: &BTreeMap<String, i64>) -> String {
    let fmt = |name: &str, e: i64| {
        if e == 1 {
            name.to_string()
        } else {
            format!("{name}^{e}")
        }
    };
    let num: Vec<String> = exps
        .iter()
        .filter(|(_, &e)| e > 0)
        .map(|(n, &e)| fmt(n, e))
        .collect();
    let den: Vec<String> = exps
        .iter()
        .filter(|(_, &e)| e < 0)
        .map(|(n, &e)| fmt(n, -e))
        .collect();

    let mut out = if num.is_empty() {
        "1".to_string()
    } else {
        num.join("*")
    };
    for d in den {
        out.push('/');
        out.push_str(&d);
    }
    out
}

/// The SI prefixes with their powers of ten ("da" first so the
/// two-character prefix wins the strip).
const PREFIX_POWERS: &[(&str, i32)] = &[
    ("da", 1),
    ("Y", 24),
    ("Z", 21),
    ("E", 18),
    ("P", 15),
    ("T", 12),
    ("G", 9),
    ("M", 6),
    ("k", 3),
    ("h", 2),
    ("d", -1),
    ("c", -2),
    ("m", -3),
    ("u", -6),
    ("n", -9),
    ("p", -12),
    ("f", -15),
    ("a", -18),
    ("z", -21),
    ("y", -24),
];

/// One unprefixed built-in unit's exact scale factor and SI-base
/// expansion (SPEC.md § Built-in units — the frozen table).
fn full_name_scale(name: &str) -> Option<(f64, &'static [(&'static str, i64)])> {
    Some(match name {
        // SI base units.
        "m" => (1.0, &[("m", 1)]),
        "kg" => (1.0, &[("kg", 1)]),
        "s" => (1.0, &[("s", 1)]),
        "A" => (1.0, &[("A", 1)]),
        "K" => (1.0, &[("K", 1)]),
        "mol" => (1.0, &[("mol", 1)]),
        "cd" => (1.0, &[("cd", 1)]),
        // The prefix-attachment base for mass.
        "g" => (0.001, &[("kg", 1)]),
        // Named SI-derived units (coherent: factor 1).
        "rad" | "sr" => (1.0, &[]),
        "Hz" | "Bq" => (1.0, &[("s", -1)]),
        "N" => (1.0, &[("kg", 1), ("m", 1), ("s", -2)]),
        "Pa" => (1.0, &[("kg", 1), ("m", -1), ("s", -2)]),
        "J" => (1.0, &[("kg", 1), ("m", 2), ("s", -2)]),
        "W" => (1.0, &[("kg", 1), ("m", 2), ("s", -3)]),
        "C" => (1.0, &[("A", 1), ("s", 1)]),
        "V" => (1.0, &[("kg", 1), ("m", 2), ("A", -1), ("s", -3)]),
        "F" => (1.0, &[("A", 2), ("s", 4), ("kg", -1), ("m", -2)]),
        "ohm" => (1.0, &[("kg", 1), ("m", 2), ("A", -2), ("s", -3)]),
        "S" => (1.0, &[("A", 2), ("s", 3), ("kg", -1), ("m", -2)]),
        "Wb" => (1.0, &[("kg", 1), ("m", 2), ("A", -1), ("s", -2)]),
        "T" => (1.0, &[("kg", 1), ("A", -1), ("s", -2)]),
        "H" => (1.0, &[("kg", 1), ("m", 2), ("A", -2), ("s", -2)]),
        "lm" => (1.0, &[("cd", 1)]),
        "lx" => (1.0, &[("cd", 1), ("m", -2)]),
        "Gy" | "Sv" => (1.0, &[("m", 2), ("s", -2)]),
        "kat" => (1.0, &[("mol", 1), ("s", -1)]),
        // The litre (prefixable).
        "L" => (0.001, &[("m", 3)]),
        // Non-SI / US-imperial (exact factors per the spec).
        "min" => (60.0, &[("s", 1)]),
        "h" => (3600.0, &[("s", 1)]),
        "d" => (86400.0, &[("s", 1)]),
        "t" => (1000.0, &[("kg", 1)]),
        "in" => (0.0254, &[("m", 1)]),
        "ft" => (0.3048, &[("m", 1)]),
        "yd" => (0.9144, &[("m", 1)]),
        "mi" => (1609.344, &[("m", 1)]),
        "nmi" => (1852.0, &[("m", 1)]),
        "lb" => (0.45359237, &[("kg", 1)]),
        "oz" => (0.028349523125, &[("kg", 1)]),
        "gal" => (0.003785411784, &[("m", 3)]),
        _ => return None,
    })
}

/// The scale of one built-in unit name, prefix included:
/// (exact factor, SI-base expansion). Full names win before
/// prefix-stripping (`min` is minutes, never milli-inches; `Pa`
/// never splits); prefixes attach only to prefixable stems.
fn name_scale(name: &str) -> Option<(f64, &'static [(&'static str, i64)])> {
    if let Some(hit) = full_name_scale(name) {
        return Some(hit);
    }
    for (p, pow) in PREFIX_POWERS {
        if let Some(stem) = name.strip_prefix(p) {
            if prefixable(stem) {
                let (f, exp) = full_name_scale(stem)?;
                return Some((f * 10f64.powi(*pow), exp));
            }
        }
    }
    None
}

/// The numeric scale of a unit expression relative to its SI-base
/// expansion: `scale("km/h")` = (1000/3600, `"m/s"`), `scale("kW")`
/// = (1000, `"kg*m^2/s^3"`), `scale("1")` = (1, `"1"`). The unit
/// model is factor-only (affine units are excluded by design);
/// currencies and unknown names have no scale.
pub fn scale(expr: &str) -> Option<(f64, String)> {
    scale_with(expr, &BTreeMap::new())
}

/// Like [`scale`], with a document's imported custom-unit
/// definitions (`.!units` → `.faiv`): a custom unit contributes its
/// declared factor times the scale of its dimension, recursively
/// (aliases follow their target). Currencies carry no factor (rates
/// are external and time-varying) and never scale.
pub fn scale_with(
    expr: &str,
    customs: &BTreeMap<String, crate::faiv::UnitDef>,
) -> Option<(f64, String)> {
    let (factor, base) = scale_with_depth(expr, customs, 0)?;
    Some((factor, format_exps(&base)))
}

fn scale_with_depth(
    expr: &str,
    customs: &BTreeMap<String, crate::faiv::UnitDef>,
    depth: usize,
) -> Option<(f64, BTreeMap<String, i64>)> {
    if depth > 8 {
        return None; // defensive: .faiv definitions cannot cycle
    }
    let exps = parse_expr(expr)?;
    let mut factor = 1.0f64;
    let mut base: BTreeMap<String, i64> = BTreeMap::new();
    for (name, e) in &exps {
        let (f, bmap): (f64, BTreeMap<String, i64>) =
            if let Some((f, expansion)) = name_scale(name) {
                (
                    f,
                    expansion.iter().map(|(b, be)| (b.to_string(), *be)).collect(),
                )
            } else {
                let mut n = name.as_str();
                let mut hops = 0;
                let def = loop {
                    let d = customs.get(n)?;
                    match &d.alias_of {
                        Some(a) if hops < 8 => {
                            n = a;
                            hops += 1;
                        }
                        _ => break d,
                    }
                };
                let f: f64 = def.factor.as_deref()?.trim().parse().ok()?;
                let (df, dmap) = scale_with_depth(&def.dimension, customs, depth + 1)?;
                (f * df, dmap)
            };
        // An exponent that does not fit i32 is unscalable rather than
        // silently wrapped modulo 2^32 to a plausible-but-wrong factor.
        let e32 = i32::try_from(*e).ok()?;
        factor *= f.powi(e32);
        for (b, be) in bmap {
            let contrib = be.checked_mul(*e)?;
            let slot = base.entry(b).or_insert(0);
            *slot = slot.checked_add(contrib)?;
        }
    }
    base.retain(|_, v| *v != 0);
    Some((factor, base))
}

#[cfg(test)]
mod tests {
    use super::canonicalize as c;

    #[test]
    fn pathological_exponents_are_unscalable_not_panics() {
        // Accumulation overflow: a debug panic before the fix.
        assert_eq!(
            super::canonicalize("m^9223372036854775807*m^9223372036854775807"),
            None
        );
        // Exponent beyond i32 is unscalable, not silently wrapped to a
        // plausible-but-wrong factor.
        assert_eq!(super::scale("km^4294967298"), None);
        // Ordinary large-but-fitting exponents still work.
        assert_eq!(super::canonicalize("m^100").as_deref(), Some("m^100"));
        assert!(super::scale("km^2").is_some());
    }

    #[test]
    fn spec_table() {
        assert_eq!(c("m*kg/s^2").as_deref(), Some("kg*m/s^2"));
        assert_eq!(c("s*A").as_deref(), Some("A*s"));
        assert_eq!(c("m/s/s").as_deref(), Some("m/s^2"));
        // Side-assignment is per-operator: a/b*c == (a*c)/b.
        assert_eq!(c("m/s*kg").as_deref(), Some("kg*m/s"));
        assert_eq!(c("kg*m^2/A/s^3").as_deref(), Some("kg*m^2/A/s^3"));
        assert_eq!(c("N/s/m^2").as_deref(), Some("N/m^2/s"));
        assert_eq!(c("m*s^-1").as_deref(), Some("m/s"));
        assert_eq!(c("s^-1").as_deref(), Some("1/s"));
        assert_eq!(c("m/m").as_deref(), Some("1"));
        assert_eq!(c("kg*s/kg/s").as_deref(), Some("1"));
        assert_eq!(c("m/m^2").as_deref(), Some("1/m"));
        assert_eq!(c("1/s").as_deref(), Some("1/s"));
        assert_eq!(c("km").as_deref(), Some("km"));
        assert_eq!(c("~EUR").as_deref(), Some("~EUR"));
        assert_eq!(c("~USD/h").as_deref(), Some("~USD/h"));
    }

    #[test]
    fn scale_factors() {
        use super::scale;
        // SI base and prefixes.
        assert_eq!(scale("m"), Some((1.0, "m".into())));
        assert_eq!(scale("km"), Some((1000.0, "m".into())));
        assert_eq!(scale("mm"), Some((0.001, "m".into())));
        assert_eq!(scale("mg"), Some((1e-6, "kg".into())));
        assert_eq!(scale("g"), Some((0.001, "kg".into())));
        // Full names win before prefix-stripping: min is minutes,
        // never milli-inches.
        assert_eq!(scale("min"), Some((60.0, "s".into())));
        assert_eq!(scale("Pa"), Some((1.0, "kg/m/s^2".into())));
        // Derived units expand to the SI base; prefixes scale them.
        assert_eq!(scale("W"), Some((1.0, "kg*m^2/s^3".into())));
        assert_eq!(scale("kW"), Some((1000.0, "kg*m^2/s^3".into())));
        assert_eq!(scale("MHz"), Some((1e6, "1/s".into())));
        // Non-SI exact factors.
        assert_eq!(scale("mi"), Some((1609.344, "m".into())));
        assert_eq!(scale("lb"), Some((0.45359237, "kg".into())));
        // Compound expressions: factors multiply per exponent.
        let (f, base) = scale("km/h").unwrap();
        assert!((f - 1000.0 / 3600.0).abs() < 1e-12);
        assert_eq!(base, "m/s");
        let (f, base) = scale("mm^2").unwrap();
        assert!((f - 1e-6).abs() < 1e-18);
        assert_eq!(base, "m^2");
        // Dimensionless, unknown, and currencies.
        assert_eq!(scale("1"), Some((1.0, "1".into())));
        assert_eq!(scale("m/m"), Some((1.0, "1".into())));
        assert_eq!(scale("parsec"), None);
        assert_eq!(scale("~USD"), None);
    }

    #[test]
    fn scale_with_customs() {
        use super::scale_with;
        use crate::faiv::UnitDef;
        use std::collections::BTreeMap;
        let mut customs = BTreeMap::new();
        customs.insert(
            "au".to_string(),
            UnitDef {
                dimension: "m".into(),
                factor: Some("149597870700".into()),
                rate_source: None,
                alias_of: None,
            },
        );
        customs.insert(
            "AU".to_string(),
            UnitDef {
                alias_of: Some("au".into()),
                ..Default::default()
            },
        );
        let (f, base) = scale_with("au", &customs).unwrap();
        assert_eq!((f, base.as_str()), (149597870700.0, "m"));
        // Aliases follow their target; compounds mix with built-ins.
        let (f, base) = scale_with("AU/d", &customs).unwrap();
        assert!((f - 149597870700.0 / 86400.0).abs() < 1e-3);
        assert_eq!(base, "m/s");
        // A currency-style def (no factor) never scales.
        customs.insert(
            "usd".to_string(),
            UnitDef {
                dimension: "$".into(),
                ..Default::default()
            },
        );
        assert_eq!(scale_with("usd", &customs), None);
    }

    #[test]
    fn membership() {
        use super::members_ok;
        use std::collections::BTreeSet;
        let none = BTreeSet::new();
        // Built-in: base, derived, prefixed, non-SI.
        for u in [
            "m", "kg", "ohm", "Pa", "km", "mA", "MHz", "mL", "mg", "ft", "min", "g",
        ] {
            assert!(members_ok(u, &none), "{u} should be built-in");
        }
        // Unknown names, prefixed non-SI, double prefixes.
        for u in ["xyz", "kft", "ums"] {
            assert!(!members_ok(u, &none), "{u} should be rejected");
        }
        // Currency shape is grammar-level; membership never rejects it.
        for u in ["k~USD", "~usd", "~EURO", "~EU"] {
            assert!(c(u).is_none(), "{u} should fail the grammar");
        }
        assert!(members_ok("~EUR/kg", &none));
        assert_eq!(c("~EUR/kg").as_deref(), Some("~EUR/kg"));
        // Imported custom units extend the set.
        let customs: BTreeSet<String> = ["au".to_string()].into();
        assert!(members_ok("au/s", &customs));
        assert!(!members_ok("au/s", &none));
    }
}
