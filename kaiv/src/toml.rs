//! TOML import/export (`--features toml`) — a thin adapter over the
//! value hub, exactly like the YAML pair. TOML's four datetime
//! flavors ride the hub's typed-scalar channel as `std/time` named
//! types (`&datetime`, `&localdatetime`, `&date`, `&time`) and emit
//! back as bare TOML datetimes; in datetime-less targets they degrade
//! to strings. TOML cannot represent null — exporting a `!null` field
//! is an error, as is an integer beyond i64 (it would silently round
//! through f64). Fidelity is semantic: hex/octal/binary/underscored
//! integer literals normalize to decimal (value exact via i64).
//! Non-finite floats ride the typed channel as `std/num` marker
//! types (`&inf`, `&nan`) — kaiv floats are deliberately finite, and
//! extended reals are the union idiom `!float|std/num/inf`.

use crate::error::PipelineError;
use crate::json::{self, float_token, node_to_val, Val};
use ::toml::Value;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

pub fn import(input: &[u8]) -> Result<String, PipelineError> {
    let text = std::str::from_utf8(input).map_err(|_| err("input is not valid UTF-8"))?;
    let table: ::toml::Table = text
        .parse()
        .map_err(|e| err(format!("invalid TOML: {e}")))?;
    let Val::Obj(members) = to_val(&Value::Table(table)) else {
        unreachable!("a TOML document is always a table");
    };
    json::import_val(&members, false)
}

/// TOML value → `Val`. Integers are exact; floats stay float-shaped
/// (`5.0` keeps its `!float`); non-finite floats become strings;
/// datetimes become `std/time` typed scalars by flavor.
fn to_val(v: &Value) -> Val {
    match v {
        Value::String(s) => Val::Str(s.clone()),
        Value::Integer(i) => Val::Num(i.to_string()),
        Value::Float(f) if f.is_finite() => Val::Num(float_token(*f)),
        // Non-finite floats are std/num marker types, like datetimes
        // are std/time: kaiv floats stay finite.
        Value::Float(f) if f.is_nan() => Val::Typed {
            lib: "std/num".to_string(),
            name: "nan".to_string(),
            text: "nan".to_string(),
        },
        Value::Float(f) => Val::Typed {
            lib: "std/num".to_string(),
            name: "inf".to_string(),
            text: if f.is_sign_negative() { "-inf" } else { "inf" }.to_string(),
        },
        Value::Boolean(b) => Val::Bool(*b),
        Value::Datetime(d) => Val::Typed {
            lib: "std/time".to_string(),
            name: flavor(d).to_string(),
            text: d.to_string(),
        },
        Value::Array(items) => Val::Arr(items.iter().map(to_val).collect()),
        Value::Table(t) => Val::Obj(t.iter().map(|(k, v)| (k.clone(), to_val(v))).collect()),
    }
}

/// The `std/time` type for a TOML datetime's flavor.
fn flavor(d: &::toml::value::Datetime) -> &'static str {
    match (d.date.is_some(), d.time.is_some(), d.offset.is_some()) {
        (true, true, true) => "datetime",
        (true, true, false) => "localdatetime",
        (true, false, false) => "date",
        (false, true, false) => "time",
        _ => "datetime", // unreachable by TOML grammar
    }
}

pub fn export(canonical: &str) -> Result<String, PipelineError> {
    let root = node_to_val(&json::tree(canonical)?)?;
    let Value::Table(t) = from_val(&root)? else {
        return Err(err("TOML root must be a table"));
    };
    Ok(t.to_string())
}

/// `Val` → TOML. std/time typed scalars emit as bare datetimes; null
/// is unrepresentable.
fn from_val(v: &Val) -> Result<Value, PipelineError> {
    Ok(match v {
        Val::Null => return Err(err("TOML cannot represent null")),
        Val::Bool(b) => Value::Boolean(*b),
        Val::Num(raw) => match raw.parse::<i64>() {
            Ok(i) => Value::Integer(i),
            // An integer-shaped token beyond i64 must not silently
            // round through f64 — TOML has no wider integer.
            Err(_) if raw.bytes().all(|b| b == b'-' || b.is_ascii_digit()) => {
                return Err(err(format!(
                    "TOML cannot represent the integer {raw} (exceeds i64)"
                )))
            }
            Err(_) => {
                let f = raw
                    .parse::<f64>()
                    .map_err(|_| err(format!("bad number: {raw}")))?;
                // f64 FromStr saturates: an overflowing finite token
                // becomes inf and an underflowing one becomes 0.0.
                // Refuse rather than corrupt, mirroring the integer arm.
                if !f.is_finite() {
                    return Err(err(format!(
                        "TOML cannot represent the float {raw} (overflows f64)"
                    )));
                }
                // Only the significand decides zeroness; a nonzero
                // exponent over a zero mantissa (`0e5`) is exactly zero,
                // not an underflow.
                let mantissa = raw.split(['e', 'E']).next().unwrap_or(raw);
                if f == 0.0 && mantissa.bytes().any(|b| matches!(b, b'1'..=b'9')) {
                    return Err(err(format!(
                        "TOML cannot represent the float {raw} (underflows f64)"
                    )));
                }
                Value::Float(f)
            }
        },
        Val::Str(s) => Value::String(s.clone()),
        Val::Typed { lib, text, .. } if lib == "std/time" => Value::Datetime(
            text.parse()
                .map_err(|_| err(format!("invalid datetime: {text}")))?,
        ),
        Val::Typed { lib, text, .. } if lib == "std/num" => Value::Float(match text.as_str() {
            "inf" => f64::INFINITY,
            "-inf" => f64::NEG_INFINITY,
            "nan" => f64::NAN,
            other => return Err(err(format!("invalid std/num marker: {other}"))),
        }),
        Val::Typed { text, .. } => Value::String(text.clone()),
        Val::Arr(items) => Value::Array(items.iter().map(from_val).collect::<Result<_, _>>()?),
        Val::Obj(members) => {
            // TOML requires plain values before subtables within a
            // table; partition while preserving relative order.
            let mut scalars: Vec<(String, Value)> = Vec::new();
            let mut tables: Vec<(String, Value)> = Vec::new();
            for (k, v) in members {
                let tv = from_val(v)?;
                let is_table = matches!(&tv, Value::Table(_))
                    || matches!(&tv, Value::Array(a)
                        if !a.is_empty() && a.iter().all(|x| matches!(x, Value::Table(_))));
                if is_table {
                    tables.push((k.clone(), tv));
                } else {
                    scalars.push((k.clone(), tv));
                }
            }
            let mut t = ::toml::map::Map::new();
            for (k, v) in scalars.into_iter().chain(tables) {
                t.insert(k, v);
            }
            Value::Table(t)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_float_export_rejected() {
        assert!(export(".!kaiv 1\n!float'::x=1e400\n").is_err());
        assert!(export(".!kaiv 1\n!float'::y=1e-400\n").is_err());
        // A representable float and genuine zeros (incl. `0e5`) export.
        assert!(export(".!kaiv 1\n!float'::z=1.5\n").is_ok());
        assert!(export(".!kaiv 1\n!float'::w=0.0\n").is_ok());
        assert!(export(".!kaiv 1\n!float'::v=0e5\n").is_ok());
    }

    #[test]
    fn import_typing_and_natives() {
        let src = b"service = \"billing\"\nport = 8443\nratio = 5.0\non = true\ntags = [\"prod\", \"eu\"]\n\n[limits]\nrps = 500\nburst = 900\n\n[[servers]]\nhost = \"a\"\nport = 1\n\n[[servers]]\nhost = \"b\"\nport = 2\n";
        let out = import(src).unwrap();
        assert!(out.contains("service=billing\n"));
        assert!(out.contains("!int\nport=8443\n"));
        assert!(out.contains("!float\nratio=5.0\n"));
        assert!(out.contains("/@tags;=prod;eu\n"));
        assert!(out.contains("!int\n/limits:=rps=500|burst=900\n"));
        assert!(out.contains("/@servers/0::host=a\n"));
        assert!(out.contains("!int\n/@servers/0::port=1\n"));
        assert!(!out.contains("&json"));
    }

    #[test]
    fn datetimes_are_std_time_types() {
        let src = b"when = 2026-07-03T21:00:00Z\nlocal = 2026-07-03T21:00:00\nday = 2026-07-03\nat = 21:00:00\n";
        let out = import(src).unwrap();
        assert!(out.contains(".!types std/time\n"));
        assert!(out.contains("&datetime\nwhen=2026-07-03T21:00:00Z\n"));
        assert!(out.contains("&localdatetime\nlocal=2026-07-03T21:00:00\n"));
        assert!(out.contains("&date\nday=2026-07-03\n"));
        assert!(out.contains("&time\nat=21:00:00\n"));
        // Full pipeline: resolves, validates as std/time, exports bare.
        let raiv = crate::compile(out.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        assert!(daiv.contains("!std/time/datetime'::when=2026-07-03T21:00:00Z\n"));
        let back = export(&daiv).unwrap();
        assert!(back.contains("when = 2026-07-03T21:00:00Z\n"));
        let a: ::toml::Table = std::str::from_utf8(src).unwrap().parse().unwrap();
        let b: ::toml::Table = back.parse().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn datetime_arrays_and_structs() {
        let src = b"window = { start = 2026-07-03T21:00:00Z, end = 2026-07-04T21:00:00Z }\nmarks = [2026-07-03, 2026-07-04]\n";
        let out = import(src).unwrap();
        // Homogeneous datetimes inline like any other uniform type.
        assert!(out
            .contains("&datetime\n/window:=start=2026-07-03T21:00:00Z|end=2026-07-04T21:00:00Z\n"));
        assert!(out.contains("&date\n/@marks;=2026-07-03;2026-07-04\n"));
        let raiv = crate::compile(out.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let back = export(&daiv).unwrap();
        let a: ::toml::Table = std::str::from_utf8(src).unwrap().parse().unwrap();
        let b: ::toml::Table = back.parse().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn semantic_roundtrip() {
        let src = b"name = \"eu1\"\nport = 8443\nratio = 2.5\ntags = [\"a\", \"b\"]\n\n[limits]\nrps = 500\n\n[[servers]]\nhost = \"a\"\nport = 1\n";
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let back = export(&daiv).unwrap();
        let a: ::toml::Table = std::str::from_utf8(src).unwrap().parse().unwrap();
        let b: ::toml::Table = back.parse().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn literal_normalization_and_nonfinite() {
        let src = b"i = 0xFF\nu = 1_000_000\nf = inf\ng = -inf\nn = nan\n";
        let out = import(src).unwrap();
        assert!(out.contains("!int\ni=255\n"));
        assert!(out.contains("!int\nu=1000000\n"));
        // Non-finite floats are std/num marker types.
        assert!(out.contains(".!types std/num\n"));
        assert!(out.contains("&inf\nf=inf\n"));
        assert!(out.contains("&inf\ng=-inf\n"));
        assert!(out.contains("&nan\nn=nan\n"));
        // Round trip back to bare TOML tokens.
        let raiv = crate::compile(out.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        assert!(daiv.contains("!std/num/inf'::f=inf\n"));
        let back = export(&daiv).unwrap();
        assert!(back.contains("f = inf\n"));
        assert!(back.contains("g = -inf\n"));
        assert!(back.contains("n = nan\n"));
    }

    #[test]
    fn null_export_rejected() {
        let daiv = ".!kaiv 1\n!null'::note=\n";
        assert!(export(daiv).is_err());
    }

    #[test]
    fn oversize_integer_export_rejected() {
        // Beyond i64 the value would silently round through f64;
        // refuse instead of corrupting it.
        let daiv = ".!kaiv 1\n!int'::count=18446744073709551616\n";
        assert!(export(daiv).is_err());
        let daiv = ".!kaiv 1\n!int'::max=9223372036854775807\n";
        assert!(export(daiv).unwrap().contains("max = 9223372036854775807"));
    }

    #[test]
    fn cross_format_datetime_degrades_to_string() {
        let src = b"when = 2026-07-03T21:00:00Z\n";
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = crate::json::export(&daiv).unwrap();
        assert_eq!(json.trim_end(), r#"{"when":"2026-07-03T21:00:00Z"}"#);
    }
}
