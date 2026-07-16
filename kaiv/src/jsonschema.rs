//! JSON Schema → authored `.saiv` (`--features json`). The conversion
//! contract is a **sound weakening**: every kaiv constraint emitted is
//! implied by the source schema, and source constraints with no kaiv
//! equivalent are dropped with a `//` comment — so any document valid
//! under the source schema validates under the imported one, never
//! the reverse. Local `$ref`s (`#/$defs/…`, `#/definitions/…`) inline;
//! `title`/`description` become doc comments; `default` rides the
//! field's right side; `format` date-time/date/time map to `std/time`.
//!
//! **Documented limitation** (shared by every schema converter): the
//! contract holds for documents whose strings are flat. A non-flat
//! string (EOL/NUL, leading `$`) imported by a data converter rides
//! the `std/enc/json` embed channel, and an embed-typed value does
//! not match a `str`-typed field — validating such a document against
//! the converted schema reports a type mismatch even though it is
//! valid under the source schema. Data conversion itself is
//! unaffected (embeds round-trip losslessly); where multiline strings
//! are expected, widen the field by hand to `!str|std/enc/json`.

use crate::error::PipelineError;
use crate::json::{parse_val, Val};
use std::collections::BTreeSet;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

fn get<'a>(obj: &'a [(String, Val)], key: &str) -> Option<&'a Val> {
    obj.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

struct Ctx<'a> {
    root: &'a [(String, Val)],
    body: String,
    imports: BTreeSet<&'static str>,
}

pub fn import(input: &[u8], name: &str) -> Result<String, PipelineError> {
    let text = std::str::from_utf8(input).map_err(|_| err("input is not valid UTF-8"))?;
    let root = parse_val(text)?;
    let Val::Obj(schema) = &root else {
        return Err(err("root must be a JSON Schema object"));
    };
    let mut ctx = Ctx {
        root: schema,
        body: String::new(),
        imports: BTreeSet::new(),
    };
    let resolved = ctx.resolve(&root, 0)?;
    let Val::Obj(top) = &resolved else {
        unreachable!()
    };
    let root_ok = match get(top, "type") {
        None => true,
        Some(Val::Str(s)) => s == "object",
        // A nullable-object root (`["object","null"]`) is a sound
        // weakening — the kaiv root is always an object namespace.
        Some(Val::Arr(ts)) => ts.iter().any(|v| matches!(v, Val::Str(s) if s == "object")),
        _ => false,
    };
    if !root_ok {
        return Err(err("root schema must describe an object"));
    }
    ctx.object_props(top, "", 0)?;
    if let Some(Val::Bool(false)) = get(top, "additionalProperties") {
        ctx.note("additionalProperties: false (kaiv strict is document-wide; not emitted)");
    }
    let mut out = format!(".!kaivschema 1 {name}\n");
    for lib in &ctx.imports {
        out.push_str(&format!(".!types {lib}\n"));
    }
    out.push('\n');
    out.push_str(&ctx.body);
    Ok(out)
}

impl<'a> Ctx<'a> {
    fn note(&mut self, msg: &str) {
        // A note is one `//` comment line; strip control characters so
        // embedded user text (a pattern, a property name) cannot split
        // it into a second, malformed line.
        let clean: String = msg
            .chars()
            .map(|c| if matches!(c, '\n' | '\r' | '\0') { ' ' } else { c })
            .collect();
        self.body.push_str(&format!("// dropped: {clean}\n"));
    }

    /// Resolve a schema node: follow a local `$ref` chain (cloning —
    /// subschemas are small).
    fn resolve(&self, v: &Val, depth: usize) -> Result<Val, PipelineError> {
        if depth > 32 {
            return Err(err("$ref chain too deep (cycle?)"));
        }
        let Val::Obj(o) = v else { return Ok(v.clone()) };
        let Some(Val::Str(r)) = get(o, "$ref") else {
            return Ok(v.clone());
        };
        let path = r
            .strip_prefix("#/$defs/")
            .or_else(|| r.strip_prefix("#/definitions/"))
            .ok_or_else(|| err(format!("unsupported $ref (local #/$defs only): {r}")))?;
        let defs = get(self.root, "$defs")
            .or_else(|| get(self.root, "definitions"))
            .ok_or_else(|| err(format!("$ref to missing definitions: {r}")))?;
        let Val::Obj(defs) = defs else {
            return Err(err("$defs must be an object"));
        };
        let target = get(defs, path)
            .ok_or_else(|| err(format!("unresolved $ref: {r}")))?
            .clone();
        self.resolve(&target, depth + 1)
    }

    /// Emit fields for an object schema's properties at `path`
    /// ("" = root, "/server" = nested).
    fn object_props(
        &mut self,
        schema: &[(String, Val)],
        path: &str,
        depth: usize,
    ) -> Result<(), PipelineError> {
        if depth > 32 {
            return Err(err("schema nesting too deep"));
        }
        let required: BTreeSet<&str> = match get(schema, "required") {
            Some(Val::Arr(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Val::Str(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect(),
            _ => BTreeSet::new(),
        };
        let Some(Val::Obj(props)) = get(schema, "properties") else {
            // No properties: a bare typed map?
            if let Some(sub) = get(schema, "additionalProperties") {
                let sub = self.resolve(sub, 0)?;
                if let Val::Obj(so) = &sub {
                    if let Some(Val::Str(t)) = get(so, "type") {
                        if let Some(core) = scalar_core(t) {
                            let key = if path.is_empty() {
                                "".to_string()
                            } else {
                                path.to_string()
                            };
                            if !key.is_empty() {
                                self.body
                                    .push_str(&format!("!map<{core}>\n{}=\n", map_key(path)));
                                return Ok(());
                            }
                        }
                    }
                }
                self.note(&format!("additionalProperties at {}", disp(path)));
            }
            return Ok(());
        };
        let props = props.clone();
        for (prop, sub) in &props {
            let sub = self.resolve(sub, 0)?;
            self.field(&sub, path, prop, required.contains(prop.as_str()), depth)?;
        }
        Ok(())
    }

    fn field(
        &mut self,
        sub: &Val,
        path: &str,
        prop: &str,
        required: bool,
        depth: usize,
    ) -> Result<(), PipelineError> {
        let Val::Obj(so) = sub else {
            self.note(&format!("non-object subschema at {}", disp2(path, prop)));
            return Ok(());
        };
        // Sound weakening: an unrepresentable property name drops the
        // property (and its requiredness) with a note.
        let Ok(key) = kaiv_key(prop) else {
            self.note(&format!(
                "unrepresentable property name at {}: {prop:?}",
                disp(path)
            ));
            return Ok(());
        };
        // title/description → doc comments.
        for dk in ["title", "description"] {
            if let Some(Val::Str(t)) = get(so, dk) {
                if !t.contains(['\n', '\r']) {
                    self.body.push_str(&format!("// {t}\n"));
                }
            }
        }
        let tys = type_list(so);
        // Composition: anyOf/oneOf of scalar-typed subschemas → union.
        if tys.is_empty() {
            if let Some(Val::Arr(alts)) = get(so, "anyOf").or_else(|| get(so, "oneOf")) {
                let mut names = Vec::new();
                for alt in alts {
                    let alt = self.resolve(alt, 0)?;
                    let Val::Obj(ao) = &alt else {
                        names.clear();
                        break;
                    };
                    match get(ao, "type") {
                        Some(Val::Str(t)) if scalar_core(t).is_some() => {
                            names.push(scalar_core(t).unwrap())
                        }
                        _ => {
                            names.clear();
                            break;
                        }
                    }
                }
                if !names.is_empty() {
                    let null = if required || names.contains(&"null") {
                        ""
                    } else {
                        "null|"
                    };
                    self.body.push_str(&format!("!{null}{}\n", names.join("|")));
                    self.emit_kv(path, &key, required, so);
                    return Ok(());
                }
            }
        }
        for kw in [
            "multipleOf",
            "prefixItems",
            "if",
            "not",
            "allOf",
            "dependentRequired",
            "dependentSchemas",
            "uniqueItems",
            "patternProperties",
            "minProperties",
            "maxProperties",
        ] {
            if get(so, kw).is_some() {
                self.note(&format!("{kw} at {}", disp2(path, prop)));
            }
        }
        match tys.as_slice() {
            ["object"] => {
                if let Some(Val::Obj(_)) = get(so, "properties") {
                    let sub_path = format!("{path}/{key}");
                    return self.object_props(so, &sub_path, depth + 1);
                }
                // Typed map?
                if let Some(ap) = get(so, "additionalProperties") {
                    let ap = self.resolve(ap, 0)?;
                    if let Val::Obj(apo) = &ap {
                        if let Some(Val::Str(t)) = get(apo, "type") {
                            if let Some(core) = scalar_core(t) {
                                self.body.push_str(&format!(
                                    "!map<{core}>\n{}{}=\n",
                                    field_lhs(path, &key),
                                    if required { "" } else { "?" }
                                ));
                                return Ok(());
                            }
                        }
                    }
                }
                self.note(&format!("untyped object at {}", disp2(path, prop)));
                Ok(())
            }
            ["array"] => {
                let Some(items) = get(so, "items") else {
                    self.note(&format!("untyped array at {}", disp2(path, prop)));
                    return Ok(());
                };
                let items = self.resolve(items, 0)?;
                let Val::Obj(io) = &items else {
                    self.note(&format!("non-object items at {}", disp2(path, prop)));
                    return Ok(());
                };
                let itys = type_list(io);
                match itys.as_slice() {
                    ["object"] => {
                        // Array of structs → section block; minItems/
                        // maxItems graduate to table-header cardinality
                        // clauses (Level 2).
                        let mut open = format!("[{path}/@{key}");
                        for (kw, cl) in [("minItems", "min"), ("maxItems", "max")] {
                            match get(so, kw) {
                                Some(Val::Num(n)) if n.parse::<u64>().is_ok() => {
                                    open.push_str(&format!(" {cl}={n}"));
                                }
                                Some(_) => self.note(&format!("{kw} at {}", disp2(path, prop))),
                                None => {}
                            }
                        }
                        open.push_str("]\n");
                        self.body.push_str(&open);
                        let ireq: BTreeSet<&str> = match get(io, "required") {
                            Some(Val::Arr(rs)) => rs
                                .iter()
                                .filter_map(|v| match v {
                                    Val::Str(s) => Some(s.as_str()),
                                    _ => None,
                                })
                                .collect(),
                            _ => BTreeSet::new(),
                        };
                        if let Some(Val::Obj(iprops)) = get(io, "properties") {
                            let iprops = iprops.clone();
                            let ireq: BTreeSet<String> =
                                ireq.iter().map(|s| s.to_string()).collect();
                            for (ip, isub) in &iprops {
                                let isub = self.resolve(isub, 0)?;
                                let Val::Obj(iso) = &isub else { continue };
                                match type_list(iso).as_slice() {
                                    [t] if scalar_core(t).is_some() => {
                                        let Ok(ikey) = kaiv_key(ip) else {
                                            self.note(&format!(
                                                "unrepresentable element field name at {}: {ip:?}",
                                                disp2(path, prop)
                                            ));
                                            continue;
                                        };
                                        self.scalar_annotation(
                                            iso,
                                            t,
                                            ireq.contains(ip.as_str()),
                                        )?;
                                        self.body.push_str(&format!(
                                            "{ikey}{}=\n",
                                            if ireq.contains(ip.as_str()) { "" } else { "?" }
                                        ));
                                    }
                                    _ => self.note(&format!(
                                        "non-scalar element field {ip} at {}",
                                        disp2(path, prop)
                                    )),
                                }
                            }
                        }
                        self.body.push_str("[]\n");
                        Ok(())
                    }
                    [t] if scalar_core(t).is_some() => {
                        // Scalar vectors (`;=`) have no table header to
                        // carry cardinality — dropped, not graduated.
                        for kw in ["minItems", "maxItems"] {
                            if get(so, kw).is_some() {
                                self.note(&format!("{kw} at {}", disp2(path, prop)));
                            }
                        }
                        self.scalar_annotation(io, t, true)?;
                        self.body.push_str(&format!("{path}/@{key};=\n"));
                        Ok(())
                    }
                    _ => {
                        self.note(&format!("array items at {}", disp2(path, prop)));
                        Ok(())
                    }
                }
            }
            [t] if scalar_core(t).is_some() => {
                self.scalar_annotation(so, t, required)?;
                self.emit_kv(path, &key, required, so);
                Ok(())
            }
            [a, b] | [b, a] if *a == "null" && scalar_core(b).is_some() => {
                // Nullable scalar: union with constraints on the value
                // alternative (authored per-alternative attachment). A
                // union annotation is one whitespace-free item token, so
                // whitespace-bearing constraints (a space in a pattern)
                // cannot join it — drop them, keeping the type.
                let core = scalar_core(b).unwrap();
                let mut cons = self.scalar_constraints(so, b)?;
                if cons.contains([' ', '\t']) {
                    self.note(&format!(
                        "whitespace-bearing constraints on nullable {} (union items are whitespace-free)",
                        disp2(path, prop)
                    ));
                    cons = String::new();
                }
                self.body.push_str(&format!("!null|{core}{cons}\n"));
                self.emit_kv(path, &key, required, so);
                Ok(())
            }
            [] => {
                // No type at all: any value — omitting the field keeps
                // the schema sound (relaxed schemas accept it).
                self.note(&format!("untyped property at {}", disp2(path, prop)));
                Ok(())
            }
            other => {
                self.note(&format!("type {:?} at {}", other, disp2(path, prop)));
                Ok(())
            }
        }
    }

    /// The annotation line for a scalar-typed subschema, mapping the
    /// expressible constraints and noting the dropped ones.
    fn scalar_annotation(
        &mut self,
        so: &[(String, Val)],
        t: &str,
        required: bool,
    ) -> Result<(), PipelineError> {
        // format date-time/date/time → std/time named types. An
        // optional constrained field with no applicable default is
        // rejected at schema-compile time
        // (SchemaOptionalWithoutDefaultError), so optionals become
        // nullable unions — a sound weakening that gives the
        // Denormalizer `!null` to materialize for absent instances.
        if t == "string" {
            if let Some(Val::Str(f)) = get(so, "format") {
                let mapped = match f.as_str() {
                    "date-time" => Some("datetime"),
                    "date" => Some("date"),
                    "time" => Some("time"),
                    _ => None,
                };
                if let Some(name) = mapped {
                    if required {
                        self.imports.insert("std/time");
                        self.body.push_str(&format!("&{name}\n"));
                    } else {
                        self.body.push_str(&format!("!null|std/time/{name}\n"));
                    }
                    return Ok(());
                }
                self.note(&format!("format: {f}"));
            }
        }
        let core = scalar_core(t).unwrap();
        let mut cons = self.scalar_constraints(so, t)?;
        // An optional field renders as a `!null|…` union — one
        // whitespace-free item token — so whitespace-bearing
        // constraints cannot ride along.
        if !required && cons.contains([' ', '\t']) {
            self.note("whitespace-bearing constraints on a nullable field (union items are whitespace-free)");
            cons = String::new();
        }
        if core == "str" && cons.is_empty() {
            return Ok(()); // unannotated (the empty default applies)
        }
        let null = if required { "" } else { "!null|" };
        let bang = if required { "!" } else { "" };
        self.body.push_str(&format!("{null}{bang}{core}{cons}\n"));
        Ok(())
    }

    /// Inline constraints implied by the subschema (sound subset).
    fn scalar_constraints(
        &mut self,
        so: &[(String, Val)],
        t: &str,
    ) -> Result<String, PipelineError> {
        let mut out = String::new();
        // enum / const → {…} when every member is a safely spellable
        // scalar of this type.
        let members: Option<Vec<String>> = match (get(so, "enum"), get(so, "const")) {
            (Some(Val::Arr(vs)), _) => vs.iter().map(enum_member).collect(),
            (_, Some(c)) => enum_member(c).map(|m| vec![m]),
            _ => None,
        };
        match members {
            Some(ms) if !ms.is_empty() => {
                out.push_str(&format!("{{{}}}", ms.join(",")));
                return Ok(out); // enum subsumes the rest
            }
            Some(_) | None => {
                if get(so, "enum").is_some() || get(so, "const").is_some() {
                    self.note("enum/const with unspellable members");
                }
            }
        }
        // JSON Schema keywords are type-scoped: only emit a constraint
        // when the source schema's instance type implies it, else a
        // stray `minimum` on a string would invent a bogus range.
        if t == "string" {
            if let Some(Val::Str(p)) = get(so, "pattern") {
                // Escape-aware: only unescaped slashes gain the `\/`
                // spelling -- a source `\/` must not double-escape.
                let mut body = String::new();
                let mut esc = false;
                for c in p.chars() {
                    if esc {
                        body.push(c);
                        esc = false;
                    } else if c == '\\' {
                        body.push('\\');
                        esc = true;
                    } else if c == '/' {
                        body.push_str("\\/");
                    } else {
                        body.push(c);
                    }
                }
                // Reject `'` and every C0 control: a newline would
                // split the emitted annotation line, and other controls
                // produce a .saiv that fails to compile.
                if !p.contains('\'')
                    && !p.chars().any(|c| c < ' ')
                    && crate::rex::Regex::new(&body).is_some()
                {
                    out.push_str(&format!("/{body}/"));
                } else {
                    self.note(&format!("pattern outside the kaiv regex dialect: {p}"));
                }
            }
        }
        // Numeric bounds; exclusive bounds are exact for integers.
        if t == "integer" || t == "number" {
            let num = |v: &Val| match v {
                Val::Num(n) => Some(n.clone()),
                _ => None,
            };
            let int_shift = |v: &Val, delta: i64| match v {
                Val::Num(n) => n
                    .parse::<i64>()
                    .ok()
                    .and_then(|i| i.checked_add(delta))
                    .map(|i| i.to_string()),
                _ => None,
            };
            let mut lo = get(so, "minimum").and_then(num);
            let mut hi = get(so, "maximum").and_then(num);
            if let Some(v) = get(so, "exclusiveMinimum") {
                match (t, int_shift(v, 1)) {
                    ("integer", Some(shifted)) => lo = Some(shifted),
                    _ => self.note("exclusiveMinimum (inexact for kaiv inclusive ranges)"),
                }
            }
            if let Some(v) = get(so, "exclusiveMaximum") {
                match (t, int_shift(v, -1)) {
                    ("integer", Some(shifted)) => hi = Some(shifted),
                    _ => self.note("exclusiveMaximum (inexact for kaiv inclusive ranges)"),
                }
            }
            if lo.is_some() || hi.is_some() {
                out.push_str(&format!(
                    "[{},{}]",
                    lo.unwrap_or_default(),
                    hi.unwrap_or_default()
                ));
            }
        }
        // Length bounds.
        if t == "string" {
            let ilen = |k: &str| match get(so, k) {
                Some(Val::Num(n)) => n.parse::<u64>().ok(),
                _ => None,
            };
            let (minl, maxl) = (ilen("minLength"), ilen("maxLength"));
            if minl.is_some() || maxl.is_some() {
                out.push_str(&format!(
                    "#[{},{}]",
                    minl.map(|v| v.to_string()).unwrap_or_default(),
                    maxl.map(|v| v.to_string()).unwrap_or_default()
                ));
            }
        }
        // Cross-type keywords are vacuous per JSON Schema semantics —
        // dropped, but noted per the converter's convention.
        if t != "string"
            && ["pattern", "minLength", "maxLength"]
                .iter()
                .any(|k| get(so, k).is_some())
        {
            self.note(&format!("string-only constraints on a {t} property"));
        }
        if t != "integer"
            && t != "number"
            && ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"]
                .iter()
                .any(|k| get(so, k).is_some())
        {
            self.note(&format!("numeric bounds on a {t} property"));
        }
        Ok(out)
    }

    /// `key{?}={default}` line, defaults only when scalar and flat.
    fn emit_kv(&mut self, path: &str, key: &str, required: bool, so: &[(String, Val)]) {
        let default = match get(so, "default") {
            Some(Val::Str(s)) if !s.contains(['\n', '\r', '\0', '|']) && !s.starts_with('$') => {
                s.clone()
            }
            Some(Val::Num(n)) => n.clone(),
            Some(Val::Bool(b)) => b.to_string(),
            Some(_) => {
                self.note(&format!("default at {}", disp2(path, key)));
                String::new()
            }
            None => String::new(),
        };
        self.body.push_str(&format!(
            "{}{}={}\n",
            field_lhs(path, key),
            if required { "" } else { "?" },
            default
        ));
    }
}

/// JSON Schema scalar type → kaiv core type.
fn scalar_core(t: &str) -> Option<&'static str> {
    Some(match t {
        "string" => "str",
        "integer" => "int",
        "number" => "float",
        "boolean" => "bool",
        "null" => "null",
        _ => return None,
    })
}

fn type_list(so: &[(String, Val)]) -> Vec<&str> {
    match get(so, "type") {
        Some(Val::Str(t)) => vec![t.as_str()],
        Some(Val::Arr(ts)) => ts
            .iter()
            .filter_map(|v| match v {
                Val::Str(s) => Some(s.as_str()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// A safely spellable enum member (em-char excludes , } ' and space).
fn enum_member(v: &Val) -> Option<String> {
    let s = match v {
        Val::Str(s) => s.clone(),
        Val::Num(n) => n.clone(),
        Val::Bool(b) => b.to_string(),
        _ => return None,
    };
    if s.is_empty() || s.contains([',', '}', '\'', ' ', '\t', '\n', '\r']) {
        return None;
    }
    Some(s)
}

fn field_lhs(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}::{key}")
    }
}

fn map_key(path: &str) -> String {
    path.to_string()
}

fn disp(path: &str) -> String {
    if path.is_empty() {
        "root".to_string()
    } else {
        path.to_string()
    }
}

fn disp2(path: &str, prop: &str) -> String {
    format!("{}/{prop}", disp(path))
}

/// Property name → kaiv key (bare or quoted) — shared by every
/// schema converter.
pub(crate) fn kaiv_key(key: &str) -> Result<String, PipelineError> {
    if key.contains(['\n', '\r', '\0']) || key.is_empty() {
        return Err(err(format!("unrepresentable property name: {key:?}")));
    }
    let b = key.as_bytes();
    let bare = (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
        // `re` is reserved in leading name position of schema files
        // (the pattern-literal introducer) -- spell it quoted.
        && key != "re";
    if bare {
        Ok(key.to_string())
    } else {
        Ok(format!("\"{}\"", key.replace('"', "\"\"")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiles(saiv: &str) -> crate::CompiledSchema {
        let c = crate::compile_schema(saiv.as_bytes()).unwrap();
        crate::parse_csaiv(&c).unwrap()
    }

    #[test]
    fn non_object_root_rejected() {
        assert!(import(br#"{"type":"array","items":{"type":"string"}}"#, "t").is_err());
        assert!(import(br#"{"type":"string"}"#, "t").is_err());
    }

    #[test]
    fn nullable_object_root_accepted() {
        assert!(import(br#"{"type":["object","null"],"properties":{}}"#, "t").is_ok());
    }

    #[test]
    fn cross_type_constraints_dropped() {
        // A `minimum` on a string type must not emit a numeric range.
        let saiv = import(
            br#"{"type":"object","properties":{"x":{"type":"string","minimum":10}}}"#,
            "t",
        )
        .unwrap();
        assert!(!saiv.contains("[10,"), "{saiv}");
        let _ = compiles(&saiv);
        // A `maxLength` on an integer must not emit a length constraint.
        let saiv2 = import(
            br#"{"type":"object","properties":{"y":{"type":"integer","maxLength":2}}}"#,
            "t",
        )
        .unwrap();
        assert!(!saiv2.contains("#["), "{saiv2}");
    }

    #[test]
    fn exclusive_bound_overflow_noted() {
        // i64::MAX exclusiveMinimum must not panic (checked add).
        assert!(import(
            br#"{"type":"object","properties":{"x":{"type":"integer","exclusiveMinimum":9223372036854775807}}}"#,
            "t",
        )
        .is_ok());
    }

    #[test]
    fn space_pattern_on_nullable_field_drops_not_corrupts() {
        // A space-bearing pattern is fine bare (required field) but
        // cannot join a `!null|` union; both outputs must compile.
        let opt = import(
            br#"{"type":"object","properties":{"x":{"type":"string","pattern":"a b"}}}"#,
            "t",
        )
        .unwrap();
        let _ = compiles(&opt);
        let req = import(
            br#"{"type":"object","required":["x"],"properties":{"x":{"type":"string","pattern":"a b"}}}"#,
            "t",
        )
        .unwrap();
        assert!(req.contains("/a b/"), "{req}");
        let _ = compiles(&req);
    }

    #[test]
    fn pattern_with_newline_dropped() {
        let saiv = import(
            br#"{"type":"object","properties":{"x":{"type":"string","pattern":"a\nb"}}}"#,
            "t",
        )
        .unwrap();
        // The emitted schema still compiles (no split annotation line).
        let _ = compiles(&saiv);
    }

    #[test]
    fn core_mapping() {
        let src = br#"{
            "type": "object",
            "required": ["name", "port"],
            "properties": {
                "name": {"type": "string", "description": "Service name"},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535, "default": 8080},
                "ratio": {"type": "number", "exclusiveMinimum": 0},
                "tier": {"type": "string", "enum": ["gold", "silver"]},
                "note": {"type": ["string", "null"], "maxLength": 80},
                "when": {"type": "string", "format": "date-time"},
                "labels": {"type": "object", "additionalProperties": {"type": "string"}},
                "tags": {"type": "array", "items": {"type": "string"}},
                "servers": {"type": "array", "items": {
                    "type": "object", "required": ["host"],
                    "properties": {"host": {"type": "string"}, "port": {"type": "integer"}}
                }},
                "limits": {"type": "object", "properties": {"rps": {"type": "integer"}}}
            }
        }"#;
        let saiv = import(src, "acme/svc").unwrap();
        // `when` is optional, so std/time joins as a fully-qualified
        // union alternative rather than an `&name` + import.
        assert!(saiv.starts_with(".!kaivschema 1 acme/svc\n"));
        assert!(saiv.contains("// Service name\nname=\n"));
        assert!(saiv.contains("!int[1,65535]\nport=8080\n"));
        // exclusiveMinimum on number: dropped with a note.
        assert!(saiv.contains("// dropped: exclusiveMinimum"));
        assert!(saiv.contains("!null|float\nratio?=\n"));
        assert!(saiv.contains("!null|str{gold,silver}\ntier?=\n"));
        assert!(saiv.contains("!null|str#[,80]\nnote?=\n"));
        assert!(saiv.contains("!null|std/time/datetime\nwhen?=\n"));
        assert!(saiv.contains("!map<str>\nlabels?=\n"));
        assert!(saiv.contains("/@tags;=\n"));
        assert!(saiv.contains("[/@servers]\nhost=\n!null|int\nport?=\n[]\n"));
        assert!(saiv.contains("!null|int\n/limits::rps?=\n"));
        // The result is a compilable schema.
        let sc = compiles(&saiv);
        // And a conforming document validates.
        // The Denormalizer materializes the absent optional fields
        // (null alternatives) so the strict-lockstep scan passes.
        let r = crate::Resolver::offline();
        let csaiv = crate::compile_schema(saiv.as_bytes()).unwrap();
        r.preload("acme/svc", "csaiv", csaiv.into_bytes());
        let authored = ".!kaiv 1\n.!schema:acme/svc\n!str'::name=api\n!int'::port=443\n!str'::tier=gold\n!std/time/datetime'::when=2026-07-03T21:00:00Z\n!str'/@tags::0=a\n!str'/@servers/0::host=h\n!int'/limits::rps=5\n";
        let daiv = crate::denorm::denormalize_with(authored, &r).unwrap();
        assert!(daiv.contains("!null'::ratio=\n"));
        assert!(daiv.contains("!null'/@servers/0::port=\n"));
        assert_eq!(crate::validate(&daiv, &sc), Ok(()));
        // Required enforced; constraint enforced.
        assert_eq!(
            crate::validate(".!kaiv 1\n!str'::name=api\n", &sc),
            Err(crate::AppError::RequiredFieldSchema)
        );
        assert_eq!(
            crate::validate(".!kaiv 1\n!str'::name=api\n!int'::port=99999\n", &sc),
            Err(crate::AppError::ConstraintViolation)
        );
    }

    #[test]
    fn dialect_and_name_soundness() {
        // Pre-escaped slashes do not double-escape; backreferences
        // and shorthand classes are outside the dialect and drop; an
        // unrepresentable property name drops with a note; a field
        // named `re` is spelled quoted.
        let src = br#"{
            "type": "object",
            "properties": {
                "pre": {"type": "string", "pattern": "^a\\/b$"},
                "backref": {"type": "string", "pattern": "^(a)\\1$"},
                "shorthand": {"type": "string", "pattern": "^\\w+$"},
                "": {"type": "string"},
                "re": {"type": "string"}
            }
        }"#;
        let saiv = import(src, "acme/s").unwrap();
        assert!(saiv.contains("!null|str/^a\\/b$/\npre?=\n"));
        assert!(saiv.contains("// dropped: pattern outside the kaiv regex dialect: ^(a)\\1$"));
        assert!(saiv.contains("// dropped: pattern outside the kaiv regex dialect: ^\\w+$"));
        assert!(saiv.contains("// dropped: unrepresentable property name"));
        assert!(saiv.contains("\"re\"?=\n"));
        // The result compiles and accepts a conforming document.
        let sc = compiles(&saiv);
        assert_eq!(
            crate::validate(
                ".!kaiv 1\n!str'::pre=a/b\n!str'::backref=\n!str'::shorthand=\n!str'::re=x\n",
                &sc
            ),
            Ok(())
        );
    }

    #[test]
    fn array_cardinality_graduates() {
        let src = br#"{
            "type": "object",
            "properties": {
                "servers": {"type": "array", "minItems": 1, "maxItems": 3,
                    "items": {"type": "object", "required": ["host"],
                              "properties": {"host": {"type": "string"}}}},
                "tags": {"type": "array", "minItems": 2,
                    "items": {"type": "string"}}
            }
        }"#;
        let saiv = import(src, "acme/fleet").unwrap();
        // Struct arrays graduate minItems/maxItems to the table header.
        assert!(saiv.contains("[/@servers min=1 max=3]\nhost=\n[]\n"));
        assert!(!saiv.contains("dropped: minItems at root/servers"));
        // Scalar vectors have no header to carry them — dropped.
        assert!(saiv.contains("// dropped: minItems at root/tags"));
        let sc = compiles(&saiv);
        assert_eq!(
            crate::validate(".!kaiv 1\n", &sc),
            Err(crate::AppError::CardinalityViolation)
        );
        assert_eq!(
            crate::validate(".!kaiv 1\n!str'/@servers/0::host=h\n", &sc),
            Ok(())
        );
    }

    #[test]
    fn refs_inline_and_unions() {
        let src = br##"{
            "type": "object",
            "properties": {
                "id": {"$ref": "#/$defs/ident"},
                "mode": {"anyOf": [{"type": "integer"}, {"type": "string"}]}
            },
            "$defs": {"ident": {"type": "string", "pattern": "^[a-z]+$"}}
        }"##;
        let saiv = import(src, "t").unwrap();
        assert!(saiv.contains("!null|str/^[a-z]+$/\nid?=\n"));
        assert!(saiv.contains("!null|int|str\nmode?=\n"));
        compiles(&saiv);
    }

    #[test]
    fn sound_weakening_drops_never_invents() {
        // multipleOf has no kaiv equivalent: the imported schema must
        // still ACCEPT a document that violates it (weaker, not wrong).
        let src = br#"{
            "type": "object",
            "properties": {"n": {"type": "integer", "multipleOf": 5}}
        }"#;
        let saiv = import(src, "t").unwrap();
        assert!(saiv.contains("// dropped: multipleOf"));
        let sc = compiles(&saiv);
        assert_eq!(crate::validate(".!kaiv 1\n!int'::n=7\n", &sc), Ok(()));
    }

    #[test]
    fn slashes_in_patterns_escape() {
        let src = br#"{
            "type": "object",
            "properties": {"path": {"type": "string", "pattern": "^/[a-z/]+$"}}
        }"#;
        let saiv = import(src, "t").unwrap();
        assert!(saiv.contains("/^\\/[a-z\\/]+$/\npath?=\n"));
        let sc = compiles(&saiv);
        assert_eq!(crate::validate(".!kaiv 1\n!str'::path=/a/b\n", &sc), Ok(()));
    }
}
