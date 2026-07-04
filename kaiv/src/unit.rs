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
            *exps.entry(name).or_insert(0) += signed;
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
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::canonicalize as c;

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
