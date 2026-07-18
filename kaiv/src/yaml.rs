//! YAML import/export (`--features yaml`) — a thin adapter over the
//! value hub: import converts the YAML tree to `Val` and feeds the
//! shared emission engine, so every rule (inline forms, the explicit-
//! index mode, the 80-character budget, `std/enc/json` embedding) is
//! format-agnostic by construction; export walks the shared export
//! tree. Fidelity is semantic — YAML formatting, comments, and
//! quoting styles do not survive any parse. Merge keys (`<<`) are
//! resolved and materialized. Known edges: `!!binary` tags are not
//! visible through the parser (the payload arrives as its base64
//! string); unquoted timestamp-shaped scalars stay strings — the
//! parser cannot distinguish them from quoted strings, so no
//! datetime sniffing (import from TOML for typed datetimes); and
//! non-finite reals (`.inf`, `.nan`) ride the typed channel as
//! `std/num` markers, kaiv floats being deliberately finite.

use crate::error::PipelineError;
use crate::json::{self, float_token, json_number_ok, node_to_val, Val};
use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

pub fn import(input: &[u8]) -> Result<String, PipelineError> {
    let text = std::str::from_utf8(input).map_err(|_| err("input is not valid UTF-8"))?;
    let docs = YamlLoader::load_from_str(text).map_err(|e| err(format!("invalid YAML: {e}")))?;
    let doc = match docs.len() {
        0 => return Err(err("empty YAML input")),
        1 => &docs[0],
        n => return Err(err(format!("expected one YAML document, found {n}"))),
    };
    let Val::Obj(members) = to_val(doc)? else {
        return Err(err("root must be a YAML mapping"));
    };
    json::import_val(&members, false)
}

/// YAML value → `Val`. Integers are exact; a real keeps its raw token
/// when it is already a JSON-shaped number, else renders via f64
/// (non-finite reals become strings — kaiv floats are finite).
fn to_val(y: &Yaml) -> Result<Val, PipelineError> {
    Ok(match y {
        Yaml::Null => Val::Null,
        Yaml::Boolean(b) => Val::Bool(*b),
        Yaml::Integer(i) => Val::Num(i.to_string()),
        Yaml::Real(raw) => match raw.as_str() {
            // Non-finite reals are std/num marker types (canonical
            // kaiv spellings; YAML's dotted forms map onto them).
            ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => num_marker("inf"),
            "-.inf" | "-.Inf" | "-.INF" => num_marker("-inf"),
            ".nan" | ".NaN" | ".NAN" => num_marker("nan"),
            _ if json_number_ok(raw) => Val::Num(raw.clone()),
            _ => match raw.parse::<f64>() {
                Ok(f) if f.is_finite() => Val::Num(float_token(f)),
                _ => Val::Str(raw.clone()),
            },
        },
        Yaml::String(s) => Val::Str(s.clone()),
        Yaml::Array(items) => Val::Arr(items.iter().map(to_val).collect::<Result<_, _>>()?),
        Yaml::Hash(h) => Val::Obj(hash_to_members(h)?),
        Yaml::Alias(_) | Yaml::BadValue => return Err(err("unsupported YAML node")),
    })
}

/// A YAML mapping → members, applying **merge key** (`<<`) semantics:
/// merged entries materialize at the `<<` position, explicit host
/// keys win regardless of position, a sequence of sources merges in
/// order with earlier sources taking precedence, and sources resolve
/// recursively (an anchored map may itself contain `<<`).
fn hash_to_members(h: &Hash) -> Result<Vec<(String, Val)>, PipelineError> {
    // Explicit host keys, for position-independent precedence.
    let mut explicit = std::collections::BTreeSet::new();
    for (k, _) in h.iter() {
        if !matches!(k, Yaml::String(s) if s == "<<") {
            explicit.insert(scalar_key(k)?);
        }
    }
    let mut members: Vec<(String, Val)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (k, v) in h.iter() {
        if matches!(k, Yaml::String(s) if s == "<<") {
            let sources: Vec<&Yaml> = match v {
                Yaml::Array(items) => items.iter().collect(),
                other => vec![other],
            };
            for src in sources {
                let Yaml::Hash(sh) = src else {
                    return Err(err(
                        "YAML merge key value must be a mapping or a sequence of mappings",
                    ));
                };
                for (mk, mv) in hash_to_members(sh)? {
                    if !explicit.contains(&mk) && seen.insert(mk.clone()) {
                        members.push((mk, mv));
                    }
                }
            }
            continue;
        }
        let key = scalar_key(k)?;
        seen.insert(key.clone());
        members.push((key, to_val(v)?));
    }
    Ok(members)
}

fn num_marker(text: &str) -> Val {
    Val::Typed {
        lib: "std/num".to_string(),
        name: if text == "nan" { "nan" } else { "inf" }.to_string(),
        text: text.to_string(),
    }
}

fn scalar_key(y: &Yaml) -> Result<String, PipelineError> {
    match y {
        Yaml::String(s) => Ok(s.clone()),
        Yaml::Integer(i) => Ok(i.to_string()),
        Yaml::Real(r) => Ok(r.clone()),
        Yaml::Boolean(b) => Ok(b.to_string()),
        other => Err(err(format!("unsupported YAML mapping key: {other:?}"))),
    }
}

pub fn export(canonical: &str) -> Result<String, PipelineError> {
    let root = node_to_val(&json::tree(canonical)?)?;
    let y = from_val(&root);
    let mut out = String::new();
    YamlEmitter::new(&mut out)
        .dump(&y)
        .map_err(|e| err(format!("YAML emit failed: {e}")))?;
    out.push('\n');
    Ok(out)
}

/// `Val` → YAML. Typed scalars (std/time) emit as plain scalars —
/// YAML's core schema has no datetime type.
fn from_val(v: &Val) -> Yaml {
    match v {
        Val::Null => Yaml::Null,
        Val::Bool(b) => Yaml::Boolean(*b),
        Val::Num(raw) => match raw.parse::<i64>() {
            Ok(i) => Yaml::Integer(i),
            Err(_) => Yaml::Real(raw.clone()),
        },
        Val::Str(s) => Yaml::String(s.clone()),
        Val::Typed { lib, text, .. } if lib == "std/num" => Yaml::Real(match text.as_str() {
            "inf" => ".inf".to_string(),
            "-inf" => "-.inf".to_string(),
            _ => ".nan".to_string(),
        }),
        Val::Typed { text, .. } => Yaml::String(text.clone()),
        Val::Arr(items) => Yaml::Array(items.iter().map(from_val).collect()),
        Val::Obj(members) => {
            let mut h = Hash::new();
            for (k, v) in members {
                h.insert(Yaml::String(k.clone()), from_val(v));
            }
            Yaml::Hash(h)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_typing_and_natives() {
        let src = b"service: billing\nport: 8443\nratio: 1.5\non: true\nnote: null\nlimits:\n  rps: 500\n  burst: 900\ntags:\n  - prod\n  - eu\nservers:\n  - host: a\n    port: 1\n  - host: b\n    port: 2\n";
        let out = import(src).unwrap();
        assert!(out.contains("service=billing\n"));
        assert!(out.contains("!int\nport=8443\n"));
        assert!(out.contains("!float\nratio=1.5\n"));
        assert!(out.contains("!bool\non=true\n"));
        assert!(out.contains("!null\nnote=\n"));
        assert!(out.contains("!int\n/limits:=rps=500|burst=900\n"));
        assert!(out.contains("/@tags;=prod;eu\n"));
        assert!(out.contains("/@servers/0::host=a\n"));
        assert!(!out.contains("&json"));
    }

    #[test]
    fn multiline_string_imports_as_text() {
        // A block scalar is LF-only multi-line: it travels readable
        // as core !text (trailing newline = trailing separator), no
        // std/enc import needed.
        let out = import(b"motd: |\n  hello\n  world\n").unwrap();
        assert!(!out.contains(".!types std/enc"));
        assert!(out.contains("!text\nmotd=hello|:|world|:|\n"));
    }

    #[test]
    fn semantic_roundtrip() {
        let src = b"name: eu1\nport: 8443\nratio: 2.5\nlimits:\n  rps: 500\ntags:\n  - a\n  - b\nservers:\n  - host: a\n    port: 1\n";
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let back = export(&daiv).unwrap();
        let a = YamlLoader::load_from_str(std::str::from_utf8(src).unwrap()).unwrap();
        let b = YamlLoader::load_from_str(&back).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn merge_keys_resolve() {
        let src = b"base: &x\n  k: 1\n  m: 9\nb:\n  <<: *x\n  j: 2\n  k: 5\n";
        let out = import(src).unwrap();
        // Merged m materializes at the << position; explicit k=5 wins
        // over merged k=1; no "<<" field; all-int -> inline struct.
        assert!(out.contains("!int\n/b:=m=9|j=2|k=5\n"));
        assert!(!out.contains("<<"));
    }

    #[test]
    fn merge_key_sequences_and_recursion() {
        // Earlier sources win within a sequence; sources with their own
        // << resolve recursively.
        let src = b"a: &a\n  x: 1\nb: &b\n  <<: *a\n  y: 2\nc:\n  <<: [*b, *a]\n  z: 3\n";
        let out = import(src).unwrap();
        assert!(out.contains("!int\n/c:=x=1|y=2|z=3\n"));
        assert!(!out.contains("<<"));
    }

    #[test]
    fn nonfinite_reals_are_std_num() {
        let out = import(b"a: .inf\nb: -.inf\nc: .nan\n").unwrap();
        assert!(out.contains(".!types std/num\n"));
        assert!(out.contains("&inf\na=inf\n"));
        assert!(out.contains("&inf\nb=-inf\n"));
        assert!(out.contains("&nan\nc=nan\n"));
        let raiv = crate::compile(out.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let back = export(&daiv).unwrap();
        assert!(back.contains("a: .inf\n"));
        assert!(back.contains("b: -.inf\n"));
        assert!(back.contains("c: .nan\n"));
    }

    #[test]
    fn multi_document_rejected() {
        assert!(import(b"a: 1\n---\nb: 2\n").is_err());
        assert!(import(b"- just\n- a\n- list\n").is_err());
    }
}
