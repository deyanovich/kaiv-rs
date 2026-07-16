//! JSON import/export (`--features json`). Import produces a flat
//! authored `.kaiv`: top-level scalars become typed lines; anything
//! not representable as a flat scalar line — nested containers,
//! strings containing EOL/NUL, strings starting with `$` — embeds as
//! `std/enc/json` (base64url of its JSON text). Export inverts:
//! canonical lines become a JSON object tree, `std/enc/json` payloads
//! decode and splice verbatim.
//!
//! The parser is hand-rolled so that number tokens and nested
//! containers are kept as **raw source slices**, never re-serialized
//! — values are verbatim strings in kaiv, and imports honor that.

use crate::error::PipelineError;

const MAX_DEPTH: usize = 512;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

// ---------------------------------------------------------------- b64url

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Base64url, unpadded (RFC 4648 §5) — the `!b64` variant.
pub fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let sextets = [n >> 18, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        for (i, s) in sextets.iter().enumerate() {
            if i <= chunk.len() {
                out.push(B64URL[*s as usize] as char);
            }
        }
    }
    out
}

pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32)
    }
    let b = s.as_bytes();
    if b.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() * 3 / 4);
    for chunk in b.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        // Non-canonical trailing bits (RFC 4648 §3.5) are tolerated by
        // discarding them: the spec pins validation to the base64url
        // SHAPE and nothing more, so the compiler and validator accept
        // `aR` — an exporter rejecting it would break the pipeline's
        // emit-what-you-accept closure.
        out.push((n >> 16) as u8);
        if chunk.len() >= 3 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() == 4 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// ------------------------------------------------------------ JSON parse

/// A parsed JSON value. Numbers and containers stay raw source slices.
pub(crate) enum Jv<'a> {
    Null,
    Bool(bool),
    Num(&'a str),
    Str(String),
    Container(&'a str),
}

pub(crate) struct P<'a> {
    pub(crate) s: &'a str,
    pub(crate) i: usize,
}

impl<'a> P<'a> {
    fn b(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }
    pub(crate) fn ws(&mut self) {
        while matches!(self.b(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }
    fn lit(&mut self, l: &str) -> Result<(), PipelineError> {
        if self.s[self.i..].starts_with(l) {
            self.i += l.len();
            Ok(())
        } else {
            Err(err(format!("invalid JSON at byte {}", self.i)))
        }
    }

    pub(crate) fn value(&mut self, depth: usize) -> Result<Jv<'a>, PipelineError> {
        if depth > MAX_DEPTH {
            return Err(err("JSON nesting too deep"));
        }
        self.ws();
        match self.b() {
            Some(b'{') => {
                let start = self.i;
                self.object(depth)?;
                Ok(Jv::Container(&self.s[start..self.i]))
            }
            Some(b'[') => {
                let start = self.i;
                self.array(depth)?;
                Ok(Jv::Container(&self.s[start..self.i]))
            }
            Some(b'"') => Ok(Jv::Str(self.string()?)),
            Some(b'n') => self.lit("null").map(|()| Jv::Null),
            Some(b't') => self.lit("true").map(|()| Jv::Bool(true)),
            Some(b'f') => self.lit("false").map(|()| Jv::Bool(false)),
            Some(b'-' | b'0'..=b'9') => {
                let start = self.i;
                self.number()?;
                Ok(Jv::Num(&self.s[start..self.i]))
            }
            _ => Err(err(format!("invalid JSON at byte {}", self.i))),
        }
    }

    /// Validate an object, discarding values.
    fn object(&mut self, depth: usize) -> Result<(), PipelineError> {
        self.pairs(depth, |_, _| Ok(()))
    }

    /// Iterate an object's pairs.
    pub(crate) fn pairs(
        &mut self,
        depth: usize,
        mut f: impl FnMut(String, Jv<'a>) -> Result<(), PipelineError>,
    ) -> Result<(), PipelineError> {
        self.lit("{")?;
        self.ws();
        if self.b() == Some(b'}') {
            self.i += 1;
            return Ok(());
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            self.lit(":")?;
            let v = self.value(depth + 1)?;
            f(key, v)?;
            self.ws();
            match self.b() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(err(format!("invalid JSON at byte {}", self.i))),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), PipelineError> {
        self.elements(depth, |_| Ok(()))
    }

    /// Iterate an array's elements.
    pub(crate) fn elements(
        &mut self,
        depth: usize,
        mut f: impl FnMut(Jv<'a>) -> Result<(), PipelineError>,
    ) -> Result<(), PipelineError> {
        self.lit("[")?;
        self.ws();
        if self.b() == Some(b']') {
            self.i += 1;
            return Ok(());
        }
        loop {
            let v = self.value(depth + 1)?;
            f(v)?;
            self.ws();
            match self.b() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(err(format!("invalid JSON at byte {}", self.i))),
            }
        }
    }

    fn string(&mut self) -> Result<String, PipelineError> {
        self.lit("\"")?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.b() else {
                return Err(err("unterminated JSON string"));
            };
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(e) = self.b() else {
                        return Err(err("unterminated JSON escape"));
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let ch = if (0xD800..0xDC00).contains(&hi) {
                                self.lit("\\u")?;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err(err("invalid JSON surrogate pair"));
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else if (0xDC00..0xE000).contains(&hi) {
                                return Err(err("lone JSON low surrogate"));
                            } else {
                                hi
                            };
                            let ch = char::from_u32(ch).ok_or_else(|| err("invalid codepoint"))?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(err("invalid JSON escape")),
                    }
                }
                0x00..=0x1F => return Err(err("unescaped control character in JSON string")),
                _ => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|_| err("invalid UTF-8 in JSON string"))
    }

    fn hex4(&mut self) -> Result<u32, PipelineError> {
        let h = self
            .s
            .get(self.i..self.i + 4)
            .ok_or_else(|| err("truncated \\u escape"))?;
        self.i += 4;
        // RFC 8259 requires exactly four hex digits; from_str_radix
        // otherwise tolerates a leading `+` (e.g. `\u+041`).
        if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(err("invalid \\u escape"));
        }
        u32::from_str_radix(h, 16).map_err(|_| err("invalid \\u escape"))
    }

    pub(crate) fn number(&mut self) -> Result<(), PipelineError> {
        let start = self.i;
        if self.b() == Some(b'-') {
            self.i += 1;
        }
        match self.b() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.b(), Some(b'0'..=b'9')) {
                    self.i += 1;
                }
            }
            _ => return Err(err(format!("invalid JSON number at byte {start}"))),
        }
        if self.b() == Some(b'.') {
            self.i += 1;
            if !matches!(self.b(), Some(b'0'..=b'9')) {
                return Err(err(format!("invalid JSON number at byte {start}")));
            }
            while matches!(self.b(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        if matches!(self.b(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !matches!(self.b(), Some(b'0'..=b'9')) {
                return Err(err(format!("invalid JSON number at byte {start}")));
            }
            while matches!(self.b(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }
        Ok(())
    }
}

// --------------------------------------------------------------- import

/// JSON → authored `.kaiv`. The root must be an object. Structures
/// import natively — arrays as `;=`/`+=`/`+:=` forms, objects as
/// namespaces with `:=` inlining — and only empty containers,
/// anonymous nested arrays, and strings not representable as flat
/// scalar lines embed as `std/enc/json` (base64url of their JSON
/// text).
pub fn import(input: &[u8]) -> Result<String, PipelineError> {
    import_impl(input, false)
}

/// Fully flat import: every container embeds as `std/enc/json`.
pub fn import_flat(input: &[u8]) -> Result<String, PipelineError> {
    import_impl(input, true)
}

fn import_impl(input: &[u8], flat: bool) -> Result<String, PipelineError> {
    let text = std::str::from_utf8(input).map_err(|_| err("input is not valid UTF-8"))?;
    let root = parse_val(text)?;
    let Val::Obj(members) = root else {
        return Err(err("root must be a JSON object"));
    };
    import_val(&members, flat)
}

// ------------------------------------------------------------- value hub

/// The interchange tree every importer feeds and the shared emission
/// engine walks. JSON produces the untyped subset; other formats may
/// tag scalars with a named-type annotation (`Typed`) — TOML
/// datetimes arrive as `std/time` types, for example.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Val {
    Null,
    Bool(bool),
    /// Raw number token, preserved verbatim.
    Num(String),
    Str(String),
    /// Scalar carrying an explicit named-type annotation: emitted as
    /// `&{name}` with a `.!types {lib}` import.
    Typed {
        lib: String,
        name: String,
        text: String,
    },
    Arr(Vec<Val>),
    Obj(Vec<(String, Val)>),
}

/// Parse a complete JSON text into a `Val` tree.
pub(crate) fn parse_val(text: &str) -> Result<Val, PipelineError> {
    let mut p = P { s: text, i: 0 };
    let v = p.value(0)?;
    let out = jv_to_val(&v)?;
    p.ws();
    if p.i != text.len() {
        return Err(err("trailing content after JSON document"));
    }
    Ok(out)
}

fn jv_to_val(v: &Jv) -> Result<Val, PipelineError> {
    Ok(match v {
        Jv::Null => Val::Null,
        Jv::Bool(b) => Val::Bool(*b),
        Jv::Num(raw) => Val::Num((*raw).to_string()),
        Jv::Str(s) => Val::Str(s.clone()),
        Jv::Container(raw) if raw.starts_with('[') => {
            let mut p = P { s: raw, i: 0 };
            let mut items = Vec::new();
            p.elements(0, |v| {
                items.push(jv_to_val(&v)?);
                Ok(())
            })?;
            Val::Arr(items)
        }
        Jv::Container(raw) => {
            let mut p = P { s: raw, i: 0 };
            let mut members = Vec::new();
            p.pairs(0, |k, v| {
                members.push((k, jv_to_val(&v)?));
                Ok(())
            })?;
            Val::Obj(members)
        }
    })
}

/// Compact JSON text for a `Val`. `Typed` scalars degrade to plain
/// strings — JSON has no annotation channel; the type survives only
/// on native lines.
pub(crate) fn val_to_json(v: &Val, out: &mut String) {
    match v {
        Val::Null => out.push_str("null"),
        Val::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Val::Num(raw) => out.push_str(raw),
        Val::Str(s) => out.push_str(&json_string(s)),
        Val::Typed { text, .. } => out.push_str(&json_string(text)),
        Val::Arr(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                val_to_json(v, out);
            }
            out.push(']');
        }
        Val::Obj(members) => {
            out.push('{');
            for (i, (k, v)) in members.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(k));
                out.push(':');
                val_to_json(v, out);
            }
            out.push('}');
        }
    }
}

/// Library imports the emitted document needs (`.!types` lines).
type Needs = std::collections::BTreeSet<String>;

/// The shared emission engine: a root member list → authored `.kaiv`.
/// Every importer ends here, so the inline forms, the explicit-index
/// mode, the line budget, and the embedding rule are format-agnostic.
pub(crate) fn import_val(members: &[(String, Val)], flat: bool) -> Result<String, PipelineError> {
    let mut fields = String::new();
    let mut needs = Needs::new();
    for (k, v) in members {
        let key = kaiv_key(k)?;
        if let Some((anno, val)) = scalar_parts(v, &mut needs) {
            fields.push_str(&format!("{anno}{key}={val}\n"));
            continue;
        }
        if flat {
            fields.push_str(&embed_line(&key, "=", v, &mut needs));
            continue;
        }
        match v {
            Val::Arr(items) if !items.is_empty() => {
                emit_array(&mut fields, &mut needs, &format!("/@{key}"), items)?;
            }
            Val::Obj(subs) if !subs.is_empty() => {
                emit_object(&mut fields, &mut needs, &format!("/{key}"), subs)?;
            }
            // Empty containers embed — they exist only through their
            // element/field lines.
            _ => fields.push_str(&embed_line(&key, "=", v, &mut needs)),
        }
    }
    let mut out = String::from(".!kaiv 1\n");
    for lib in &needs {
        out.push_str(&format!(".!types {lib}\n"));
    }
    out.push('\n');
    out.push_str(&fields);
    Ok(out)
}

/// `&json` + base64url of the value's JSON text, at `{target}{op}`.
fn embed_line(target: &str, op: &str, v: &Val, needs: &mut Needs) -> String {
    needs.insert("std/enc".to_string());
    let mut json = String::new();
    val_to_json(v, &mut json);
    format!("&json\n{target}{op}{}\n", b64url_encode(json.as_bytes()))
}

/// Inline `:=`/`;=` line length cut-off: beyond this, structs and
/// vectors break into per-member/per-element lines.
const INLINE_STRUCT_MAX: usize = 80;

/// Emit a native array at `path` (`/@tags`, `/limits/@tags`): the
/// inline `;=` vector when homogeneous and short, else per-element
/// lines. Struct elements go native — the idiomatic `+:=` append when
/// every element is a fitting homogeneous scalar object, otherwise
/// explicit-index emission (no counters, so element kinds mix
/// freely). Empty containers and anonymous nested arrays embed.
fn emit_array(
    fields: &mut String,
    needs: &mut Needs,
    path: &str,
    items: &[Val],
) -> Result<(), PipelineError> {
    if let Some(inline) = inline_vector(path, items, needs) {
        fields.push_str(&inline);
        return Ok(());
    }
    let has_object = items.iter().any(|i| matches!(i, Val::Obj(_)));
    if !has_object {
        for item in items {
            if let Some((anno, val)) = scalar_parts(item, needs) {
                fields.push_str(&format!("{anno}{path}+={val}\n"));
            } else {
                fields.push_str(&embed_line(path, "+=", item, needs));
            }
        }
        return Ok(());
    }
    if let Some(lines) = all_append_structs(path, items, needs) {
        fields.push_str(&lines);
        return Ok(());
    }
    for (i, item) in items.iter().enumerate() {
        if let Some((anno, val)) = scalar_parts(item, needs) {
            fields.push_str(&format!("{anno}{path}::{i}={val}\n"));
            continue;
        }
        match item {
            Val::Obj(subs) if !subs.is_empty() => {
                emit_object(fields, needs, &format!("{path}/{i}"), subs)?;
            }
            _ => fields.push_str(&embed_line(&format!("{path}::{i}"), "=", item, needs)),
        }
    }
    Ok(())
}

/// When every element is a non-empty homogeneous scalar object whose
/// `+:=` line fits INLINE_STRUCT_MAX, render the whole array as
/// append-struct lines; None as soon as any element disqualifies.
fn all_append_structs(path: &str, items: &[Val], needs: &mut Needs) -> Option<String> {
    let mut out = String::new();
    let mut probe = needs.clone();
    for item in items {
        let Val::Obj(subs) = item else {
            return None;
        };
        if subs.is_empty() {
            return None;
        }
        let (anno, pairs) = inline_pairs(subs, &mut probe)?;
        let line = format!("{path}+:={pairs}");
        if line.len() > INLINE_STRUCT_MAX {
            return None;
        }
        out.push_str(&format!("{anno}{line}\n"));
    }
    *needs = probe;
    Some(out)
}

/// Emit a native namespace at `path` (`/limits`, `/a/b`): the inline
/// `:=` struct when homogeneous and short, else one field line per
/// member — scalars in place, arrays via emit_array, objects by
/// recursion. Empty containers embed.
fn emit_object(
    fields: &mut String,
    needs: &mut Needs,
    path: &str,
    members: &[(String, Val)],
) -> Result<(), PipelineError> {
    if let Some(inline) = inline_struct(path, members, needs) {
        fields.push_str(&inline);
        return Ok(());
    }
    for (mk, mv) in members {
        let mkey = kaiv_key(mk)?;
        if let Some((anno, val)) = scalar_parts(mv, needs) {
            fields.push_str(&format!("{anno}{path}::{mkey}={val}\n"));
            continue;
        }
        match mv {
            Val::Arr(items) if !items.is_empty() => {
                emit_array(fields, needs, &format!("{path}/@{mkey}"), items)?;
            }
            Val::Obj(subs) if !subs.is_empty() => {
                emit_object(fields, needs, &format!("{path}/{mkey}"), subs)?;
            }
            _ => fields.push_str(&embed_line(&format!("{path}::{mkey}"), "=", mv, needs)),
        }
    }
    Ok(())
}

/// The inline homogeneous-vector form: `!anno` + `path;=1;2;3`.
fn inline_vector(path: &str, items: &[Val], needs: &mut Needs) -> Option<String> {
    let mut probe = needs.clone();
    let mut anno: Option<String> = None;
    let mut vals: Vec<String> = Vec::new();
    for item in items {
        let (a, val) = scalar_parts_inline(item, &mut probe, ';')?;
        match &anno {
            None => anno = Some(a),
            Some(prev) if *prev == a => {}
            Some(_) => return None, // heterogeneous
        }
        vals.push(val);
    }
    let line = format!("{path};={}", vals.join(";"));
    if line.len() > INLINE_STRUCT_MAX {
        return None;
    }
    *needs = probe;
    Some(format!("{}{line}\n", anno.unwrap_or_default()))
}

/// The inline homogeneous-struct form: `!anno` + `path:=a=1|b=2`.
fn inline_struct(path: &str, members: &[(String, Val)], needs: &mut Needs) -> Option<String> {
    let mut probe = needs.clone();
    let (anno, pairs) = inline_pairs(members, &mut probe)?;
    let line = format!("{path}:={pairs}");
    if line.len() > INLINE_STRUCT_MAX {
        return None;
    }
    *needs = probe;
    Some(format!("{anno}{line}\n"))
}

/// The shared eligibility core for `:=` and `+:=`: every member a
/// scalar of one same type, bare-name keys, inline-safe values.
/// Returns (annotation line, joined pairs); the caller applies its
/// own line-length budget.
fn inline_pairs(members: &[(String, Val)], needs: &mut Needs) -> Option<(String, String)> {
    let mut anno: Option<String> = None;
    let mut pairs: Vec<String> = Vec::new();
    for (mk, mv) in members {
        let b = mk.as_bytes();
        let bare = !b.is_empty()
            && (b[0].is_ascii_alphabetic() || b[0] == b'_')
            && b[1..]
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == b'_');
        if !bare {
            return None;
        }
        let (a, val) = scalar_parts_inline(mv, needs, '|')?;
        match &anno {
            None => anno = Some(a),
            Some(prev) if *prev == a => {}
            Some(_) => return None, // heterogeneous
        }
        pairs.push(format!("{mk}={val}"));
    }
    Some((anno.unwrap_or_default(), pairs.join("|")))
}

/// scalar_parts, additionally rejecting the inline separator (`;` for
/// vectors, `|` for struct pairs) and embedded fallbacks — inline
/// forms hold plain scalars only.
fn scalar_parts_inline(v: &Val, needs: &mut Needs, sep: char) -> Option<(String, String)> {
    let text_ok = |t: &str| flat_ok(t) && !t.contains(sep);
    match v {
        Val::Str(s) if !text_ok(s) => None,
        Val::Typed { text, .. } if !text_ok(text) => None,
        _ => match scalar_parts(v, needs) {
            Some((a, _)) if a.starts_with("&json") => None, // embedded, not inline-safe
            other => other,
        },
    }
}

/// (annotation line, value text) for a scalar; None for containers.
/// Strings and typed scalars failing the flat rule degrade to
/// `std/enc/json` embeddings of their JSON string literal.
fn scalar_parts(v: &Val, needs: &mut Needs) -> Option<(String, String)> {
    match v {
        Val::Null => Some(("!null\n".into(), String::new())),
        Val::Bool(b) => Some(("!bool\n".into(), b.to_string())),
        Val::Num(raw) => {
            let t = if raw.bytes().all(|b| b == b'-' || b.is_ascii_digit()) {
                "int"
            } else {
                "float"
            };
            Some((format!("!{t}\n"), raw.clone()))
        }
        Val::Str(s) if flat_ok(s) => Some((String::new(), s.clone())),
        Val::Str(s) => {
            needs.insert("std/enc".to_string());
            Some(("&json\n".into(), b64url_encode(json_string(s).as_bytes())))
        }
        Val::Typed { lib, name, text } if flat_ok(text) => {
            needs.insert(lib.clone());
            Some((format!("&{name}\n"), text.clone()))
        }
        Val::Typed { text, .. } => {
            needs.insert("std/enc".to_string());
            Some((
                "&json\n".into(),
                b64url_encode(json_string(text).as_bytes()),
            ))
        }
        Val::Arr(_) | Val::Obj(_) => None,
    }
}

/// A string is representable as a flat scalar line unless it contains
/// EOL/NUL (forbidden in kaiv values) or leads with `$` (dereference).
fn flat_ok(s: &str) -> bool {
    !s.contains(['\n', '\r', '\0']) && !s.starts_with('$')
}

/// JSON key → kaiv key: bare if within the bare-name grammar, quoted
/// (with `""` doubling) otherwise; EOL/NUL are unrepresentable.
fn kaiv_key(key: &str) -> Result<String, PipelineError> {
    if key.contains(['\n', '\r', '\0']) {
        return Err(err(format!("unrepresentable JSON key: {key:?}")));
    }
    let b = key.as_bytes();
    let bare = !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_');
    if bare {
        Ok(key.to_string())
    } else if key.is_empty() {
        Err(err("empty JSON key is unrepresentable"))
    } else {
        Ok(format!("\"{}\"", key.replace('"', "\"\"")))
    }
}

/// JSON string literal (quoted, escaped) for a Rust string.
pub(crate) fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Is `s` exactly one JSON number token?
pub(crate) fn json_number_ok(s: &str) -> bool {
    let mut p = P { s, i: 0 };
    p.number().is_ok() && p.i == s.len()
}

/// A JSON number token for a finite f64, keeping floats float-shaped
/// (`5.0`, not `5` — the type distinction feeds kaiv annotations).
/// A float-shaped token for a finite double. Whole values keep a
/// `.0` (or exponent form beyond 1e16 — `Display` alone would print
/// hundreds of bare digits and silently turn the token
/// integer-shaped); fractional values only occur below 2^53, where
/// `Display` is exact and always carries a point.
pub(crate) fn float_token(f: f64) -> String {
    if f.fract() != 0.0 {
        format!("{f}")
    } else if f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        format!("{f:e}")
    }
}

// ------------------------------------------------ binary-format helpers

/// A decoded binary float: finite ones become number tokens,
/// non-finite ones ride the typed channel as `std/num` markers.
#[cfg(any(feature = "cbor", feature = "avro", feature = "proto"))]
pub(crate) fn float_val(f: f64) -> Val {
    if f.is_finite() {
        return Val::Num(float_token(f));
    }
    let (name, text) = if f.is_nan() {
        ("nan", "nan")
    } else if f > 0.0 {
        ("inf", "inf")
    } else {
        ("inf", "-inf")
    };
    Val::Typed {
        lib: "std/num".to_string(),
        name: name.to_string(),
        text: text.to_string(),
    }
}

/// Big-endian magnitude bytes → decimal token (empty bytes are 0).
#[cfg(any(feature = "cbor", feature = "avro", feature = "asn1"))]
pub(crate) fn be_bytes_to_decimal(bytes: &[u8]) -> String {
    let mut digits = vec![0u8]; // little-endian decimal digits
    for &byte in bytes {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            let v = (*d as u32) * 256 + carry;
            *d = (v % 10) as u8;
            carry = v / 10;
        }
        while carry > 0 {
            digits.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    digits.iter().rev().map(|d| (d + b'0') as char).collect()
}

/// Decimal digits → big-endian magnitude bytes (empty for 0).
#[cfg(any(feature = "cbor", feature = "avro", feature = "asn1"))]
pub(crate) fn decimal_to_be_bytes(dec: &str) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new(); // little-endian
    for c in dec.bytes() {
        let mut carry = (c - b'0') as u32;
        for b in bytes.iter_mut() {
            let v = (*b as u32) * 10 + carry;
            *b = (v & 0xff) as u8;
            carry = v >> 8;
        }
        while carry > 0 {
            bytes.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    bytes.reverse();
    bytes
}

/// Big-endian +1, growing on carry out of the top byte.
#[cfg(any(feature = "cbor", feature = "avro", feature = "asn1"))]
pub(crate) fn increment(bytes: &mut Vec<u8>) {
    for i in (0..bytes.len()).rev() {
        if bytes[i] == 0xff {
            bytes[i] = 0;
        } else {
            bytes[i] += 1;
            return;
        }
    }
    bytes.insert(0, 1);
}

/// Two's-complement big-endian value → exact decimal token, with a
/// decimal point inserted `scale` digits from the right when scale
/// is nonzero (Avro decimals; ASN.1 INTEGERs use scale 0).
#[cfg(any(feature = "avro", feature = "asn1"))]
pub(crate) fn twos_complement_token(bytes: &[u8], scale: usize) -> String {
    let neg = bytes.first().is_some_and(|b| b & 0x80 != 0);
    let mag = if neg {
        let mut m: Vec<u8> = bytes.iter().map(|b| !b).collect();
        increment(&mut m);
        m
    } else {
        bytes.to_vec()
    };
    let mut digits = be_bytes_to_decimal(&mag);
    let zero = digits == "0";
    if scale > 0 {
        if digits.len() <= scale {
            digits = format!("{}{digits}", "0".repeat(scale + 1 - digits.len()));
        }
        digits.insert(digits.len() - scale, '.');
    }
    if neg && !zero {
        format!("-{digits}")
    } else {
        digits
    }
}

/// Integer-shaped decimal token → minimal two's-complement bytes.
#[cfg(any(feature = "avro", feature = "asn1"))]
pub(crate) fn token_to_twos_complement(raw: &str) -> Result<Vec<u8>, PipelineError> {
    let neg = raw.starts_with('-');
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(err(format!("not an integer token: {raw}")));
    }
    let mut m = decimal_to_be_bytes(digits);
    if m.is_empty() {
        return Ok(vec![0]); // zero
    }
    if neg {
        // Negate in place; if the sign bit fails to set, the value
        // needs one more byte of sign extension.
        for b in m.iter_mut() {
            *b = !*b;
        }
        increment(&mut m);
        if m[0] & 0x80 == 0 {
            m.insert(0, 0xff);
        }
        Ok(m)
    } else {
        if m[0] & 0x80 != 0 {
            m.insert(0, 0x00);
        }
        Ok(m)
    }
}

/// Big-endian -1; the caller guarantees a non-zero value.
#[cfg(any(feature = "cbor", feature = "avro", feature = "asn1"))]
pub(crate) fn decrement(bytes: &mut [u8]) {
    for b in bytes.iter_mut().rev() {
        if *b == 0 {
            *b = 0xff;
        } else {
            *b -= 1;
            break;
        }
    }
}

// --------------------------------------------------------------- export

pub(crate) enum Node {
    Obj(Vec<(String, Node)>),
    Arr(Vec<Node>),
    /// Rendered JSON text (a full value — spliced payloads included).
    Leaf(String),
    /// A typed scalar from an embedded marker library (`std/time`,
    /// `std/num`): the library, type name, and verbatim value — so
    /// type-native targets can emit it bare.
    Typed {
        lib: String,
        name: String,
        text: String,
    },
}

/// Canonical kaiv text (`.daiv`) → JSON object text.
pub fn export(canonical: &str) -> Result<String, PipelineError> {
    let root = tree(canonical)?;
    let mut out = String::new();
    serialize(&root, &mut out);
    out.push('\n');
    Ok(out)
}

/// Canonical kaiv text → the shared export tree all format exporters
/// walk (JSON serializes it; YAML/TOML convert via node_to_val).
pub(crate) fn tree(canonical: &str) -> Result<Node, PipelineError> {
    let mut root = Node::Obj(Vec::new());
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
        let eq = split_eq(rest).ok_or_else(|| err(format!("canonical line without =: {s}")))?;
        let (namepath, value) = (&rest[..eq], &rest[eq + 1..]);
        // No `$` heuristics: exporter input is .daiv, where values are
        // truly verbatim (`$5` is a legal literal — the `$$` doubling
        // was collapsed at denormalization). Relational .raiv is
        // denormalized by the caller before it reaches an exporter.
        let node = render_node(&a.type_name, value)?;
        insert(&mut root, &segments(namepath)?, node)?;
    }
    Ok(root)
}

/// An export-tree node → `Val`, for non-JSON exporters. Leaf JSON
/// text (including spliced payloads) parses back into structure;
/// `Time` becomes a `Typed` scalar.
pub(crate) fn node_to_val(node: &Node) -> Result<Val, PipelineError> {
    Ok(match node {
        Node::Leaf(text) => parse_val(text)?,
        Node::Typed { lib, name, text } => Val::Typed {
            lib: lib.clone(),
            name: name.clone(),
            text: text.clone(),
        },
        Node::Arr(items) => Val::Arr(items.iter().map(node_to_val).collect::<Result<_, _>>()?),
        Node::Obj(members) => Val::Obj(
            members
                .iter()
                .map(|(k, v)| Ok((k.clone(), node_to_val(v)?)))
                .collect::<Result<_, PipelineError>>()?,
        ),
    })
}

/// First `=` outside quoted names.
fn split_eq(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut q = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                if q && b.get(i + 1) == Some(&b'"') {
                    i += 1;
                } else {
                    q = !q;
                }
            }
            b'=' if !q => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// One namepath segment: the unquoted text plus whether any part was
/// quoted. `marker` is true when the segment begins with an unquoted
/// `@` — the array namespace marker (`@tags`, `@"m:Item"`); a fully
/// quoted `"@arr"` is a literal field named `@arr`.
struct Seg {
    text: String,
    quoted: bool,
    marker: bool,
}

/// Canonical namepath → segments (quote-aware split on `/` and `::`);
/// the final segment is the projected field.
fn segments(np: &str) -> Result<Vec<Seg>, PipelineError> {
    let cs: Vec<char> = np.chars().collect();
    let mut segs: Vec<Seg> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut marker = false;
    let mut q = false;
    let mut i = 0;
    let mut push = |cur: &mut String, quoted: &mut bool, marker: &mut bool| {
        segs.push(Seg {
            text: std::mem::take(cur),
            quoted: std::mem::take(quoted),
            marker: std::mem::take(marker),
        });
    };
    while i < cs.len() {
        match cs[i] {
            '"' => {
                if q && cs.get(i + 1) == Some(&'"') {
                    cur.push('"');
                    i += 1;
                } else {
                    q = !q;
                    quoted = true;
                }
            }
            '@' if !q && cur.is_empty() && !quoted => {
                marker = true;
                cur.push('@');
            }
            '/' if !q => push(&mut cur, &mut quoted, &mut marker),
            ':' if !q && cs.get(i + 1) == Some(&':') => {
                push(&mut cur, &mut quoted, &mut marker);
                i += 1;
            }
            c => cur.push(c),
        }
        i += 1;
    }
    push(&mut cur, &mut quoted, &mut marker);
    segs.retain(|s| !s.text.is_empty());
    if segs.is_empty() {
        return Err(err(format!("empty namepath: {np}")));
    }
    Ok(segs)
}

fn render_node(type_name: &str, value: &str) -> Result<Node, PipelineError> {
    for lib in ["std/time", "std/num"] {
        if let Some(name) = type_name
            .strip_prefix(lib)
            .and_then(|r| r.strip_prefix('/'))
        {
            return Ok(Node::Typed {
                lib: lib.to_string(),
                name: name.to_string(),
                text: value.to_string(),
            });
        }
    }
    let leaf = match type_name {
        "int" | "float" => {
            let mut p = P { s: value, i: 0 };
            if p.number().is_ok() && p.i == value.len() {
                value.to_string()
            } else {
                return Err(err(format!(
                    "!{type_name} value is not a JSON number: {value}"
                )));
            }
        }
        "bool" if value == "true" || value == "false" => value.to_string(),
        "bool" => return Err(err(format!("!bool value is not true/false: {value}"))),
        "null" if value.is_empty() => "null".to_string(),
        "null" => return Err(err("!null value carries a payload")),
        "std/enc/json" => {
            let bytes = b64url_decode(value).ok_or_else(|| err("invalid base64url payload"))?;
            let text =
                String::from_utf8(bytes).map_err(|_| err("std/enc/json payload is not UTF-8"))?;
            let mut p = P { s: &text, i: 0 };
            p.value(0)?;
            p.ws();
            if p.i != text.len() {
                return Err(err("std/enc/json payload is not a single JSON value"));
            }
            text
        }
        // Any other std/enc type: the payload stays base64url — its
        // verbatim canonical value, like std/time datetimes stay
        // their tokens. Text-tree targets render it as a string;
        // payload-native exporters (XML splices `xml`, CBOR decodes
        // `bin` to a byte string) reconstruct from it.
        name if name.starts_with("std/enc/") => {
            b64url_decode(value).ok_or_else(|| err("invalid base64url payload"))?;
            return Ok(Node::Typed {
                lib: "std/enc".to_string(),
                name: name["std/enc/".len()..].to_string(),
                text: value.to_string(),
            });
        }
        _ => json_string(value),
    };
    Ok(Node::Leaf(leaf))
}

fn insert(node: &mut Node, segs: &[Seg], leaf: Node) -> Result<(), PipelineError> {
    let (head, rest) = segs.split_first().expect("segments are non-empty");
    // Only an unquoted leading `@` marks an array namespace (the name
    // itself may be quoted: `@"m:Item"`); a fully quoted segment is a
    // literal field name whatever it contains.
    if head.marker {
        let slot = obj_slot(node, &head.text[1..])?;
        // Mirror the object-side collision guards: only a fresh (empty
        // Obj) slot may become an array; a prior leaf or populated
        // namespace under the same name is a collision.
        match slot {
            Node::Arr(_) => {}
            Node::Obj(p) if p.is_empty() => *slot = Node::Arr(Vec::new()),
            _ => {
                return Err(err(format!(
                    "namespace/leaf collision at {}",
                    &head.text[1..]
                )))
            }
        }
        let Node::Arr(items) = slot else {
            unreachable!()
        };
        let (idx_seg, rest2) = rest
            .split_first()
            .ok_or_else(|| err("array without index"))?;
        let idx: usize = idx_seg
            .text
            .parse::<usize>()
            .ok()
            // Only the canonical spelling addresses an element — a
            // leading-zero index must not silently alias (`00` → 0).
            .filter(|_| {
                !idx_seg.quoted && (idx_seg.text.len() == 1 || !idx_seg.text.starts_with('0'))
            })
            .ok_or_else(|| err(format!("bad array index: {}", idx_seg.text)))?;
        // Canonical arrays are contiguous (the denormalizer emits
        // ascending 0-based indices), so a gap is malformed input, not
        // a reason to fill billions of null placeholders (OOM).
        if idx > items.len() {
            return Err(err(format!(
                "array index {idx} leaves a gap (canonical arrays are contiguous)"
            )));
        }
        // A placeholder exists only for the element being appended
        // right now; any pre-existing element is real data.
        let fresh = idx == items.len();
        while items.len() <= idx {
            items.push(Node::Leaf("null".to_string()));
        }
        if rest2.is_empty() {
            match &items[idx] {
                Node::Obj(p) if !p.is_empty() => {
                    return Err(err(format!("namespace/leaf collision at element {idx}")))
                }
                Node::Arr(_) => {
                    return Err(err(format!("namespace/leaf collision at element {idx}")))
                }
                _ => items[idx] = leaf,
            }
            Ok(())
        } else {
            // Descend: legit into an Obj (deeper revisit) or into the
            // just-pushed placeholder; a real leaf element here is a
            // collision.
            if !matches!(items[idx], Node::Obj(_)) {
                if !fresh {
                    return Err(err(format!("namespace/leaf collision at element {idx}")));
                }
                items[idx] = Node::Obj(Vec::new());
            }
            insert(&mut items[idx], rest2, leaf)
        }
    } else if rest.is_empty() {
        let slot = obj_slot(node, &head.text)?;
        // Assigning a scalar where a populated namespace/array already
        // stands is a collision, not a silent clobber. A freshly created
        // empty Obj is the normal new-scalar-field case and passes.
        match slot {
            Node::Obj(p) if !p.is_empty() => {
                Err(err(format!("namespace/leaf collision at {}", head.text)))
            }
            Node::Arr(_) => Err(err(format!("namespace/leaf collision at {}", head.text))),
            _ => {
                *slot = leaf;
                Ok(())
            }
        }
    } else {
        let slot = obj_slot(node, &head.text)?;
        // Descending into a slot already holding a scalar/typed value is
        // a collision (a prior leaf assignment), not something to
        // overwrite with an empty namespace.
        if !matches!(slot, Node::Obj(_)) {
            return Err(err(format!("namespace/leaf collision at {}", head.text)));
        }
        insert(slot, rest, leaf)
    }
}

fn obj_slot<'n>(node: &'n mut Node, key: &str) -> Result<&'n mut Node, PipelineError> {
    let Node::Obj(pairs) = node else {
        return Err(err(format!("namespace/leaf collision at {key}")));
    };
    if let Some(pos) = pairs.iter().position(|(k, _)| k == key) {
        Ok(&mut pairs[pos].1)
    } else {
        pairs.push((key.to_string(), Node::Obj(Vec::new())));
        Ok(&mut pairs.last_mut().unwrap().1)
    }
}

fn serialize(node: &Node, out: &mut String) {
    match node {
        Node::Leaf(s) => out.push_str(s),
        Node::Typed { text, .. } => out.push_str(&json_string(text)),
        Node::Obj(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(k));
                out.push(':');
                serialize(v, out);
            }
            out.push('}');
        }
        Node::Arr(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                serialize(v, out);
            }
            out.push(']');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_roundtrip() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"{\"on\":true}"] {
            let e = b64url_encode(data);
            assert!(!e.contains('='));
            assert_eq!(b64url_decode(&e).as_deref(), Some(data));
        }
        assert!(b64url_decode("a").is_none()); // len % 4 == 1
        assert!(b64url_decode("ab=c").is_none()); // padding rejected
        // Non-canonical trailing bits decode tolerantly (validation is
        // shape-only, so `aR` flows through the whole pipeline).
        assert_eq!(b64url_decode("aR"), Some(vec![0x69]));
        assert_eq!(b64url_decode("aQ"), Some(vec![0x69]));
    }

    #[test]
    fn array_index_gap_and_collision_rejected() {
        // Huge index (OOM before the fix) and a gap are both rejected.
        assert!(export(".!kaiv 1\n!str'/@a::18446744073709551615=x\n").is_err());
        assert!(export(".!kaiv 1\n!int'/@a::0=1\n!int'/@a::5=2\n").is_err());
        // Contiguous indices still export.
        assert!(export(".!kaiv 1\n!int'/@a::0=1\n!int'/@a::1=2\n").is_ok());
        // A leading-zero index must not silently alias element 0.
        assert!(export(".!kaiv 1\n!int'/@a::0=1\n!int'/@a::00=2\n").is_err());
        // Namespace/leaf collision, both orders.
        assert!(export(".!kaiv 1\n!str'::a=x\n!str'/a::b=y\n").is_err());
        assert!(export(".!kaiv 1\n!str'/a::b=y\n!str'::a=x\n").is_err());
        // A normal multi-field namespace still exports.
        assert!(export(".!kaiv 1\n!str'/a/b::c=1\n!str'/a/d::e=2\n").is_ok());
    }

    #[test]
    fn json_u_escape_rejects_sign() {
        assert!(import(br#"{"k":"\u+041"}"#).is_err());
        assert!(import(br#"{"k":"A"}"#).is_ok());
    }

    #[test]
    fn daiv_values_are_verbatim_including_dollar() {
        // `$5` is a legal .daiv literal (the `$$` doubling collapsed at
        // denormalization) — exporters must not mistake it for an
        // unresolved reference.
        let out = export(".!kaiv 1\n!str'::price=$5\n").unwrap();
        assert!(out.contains("\"$5\""), "{out}");
    }

    #[test]
    fn compile_export_closure_holds_for_noncanonical_b64() {
        // Validation is shape-only, so `aR` flows through compile; the
        // export path must accept its own pipeline's output.
        let daiv = crate::compile(b".!kaiv 1\n.!types std/enc\n&bin\nb=aR\n").unwrap();
        assert!(export(&daiv).is_ok());
    }

    #[test]
    fn array_object_collisions_error_in_both_orders() {
        assert!(export(".!kaiv 1\n!str'::a=x\n!str'/@a::0=y\n").is_err());
        assert!(export(".!kaiv 1\n!str'/@a/0::x=1\n!str'/@a::0=y\n").is_err());
        assert!(export(".!kaiv 1\n!str'/@a::0=y\n!str'/@a/0::x=1\n").is_err());
        // A deeper revisit of an element namespace is legitimate.
        assert!(export(".!kaiv 1\n!str'/@a/0::x=1\n!str'/@a/1::y=2\n!str'/@a/0/sub::z=3\n").is_ok());
    }

    #[test]
    fn import_scalars_verbatim() {
        let out = import(br#"{"host":"web01","port":8080,"ratio":1e2,"big":9007199254740993,"on":true,"note":null}"#).unwrap();
        assert_eq!(
            out,
            ".!kaiv 1\n\nhost=web01\n!int\nport=8080\n!float\nratio=1e2\n!int\nbig=9007199254740993\n!bool\non=true\n!null\nnote=\n"
        );
    }

    #[test]
    fn import_embeds_unrepresentable() {
        let out = import(br#"{"nested":{"a":[1,2]},"multi":"l1\nl2","ref":"$x","weird key":"v"}"#)
            .unwrap();
        assert!(out.starts_with(".!kaiv 1\n.!types std/enc\n"));
        // The object and its array member both go native now.
        assert!(out.contains("!int\n/nested/@a;=1;2\n"));
        let multi = b64url_encode(b"\"l1\\nl2\"");
        assert!(out.contains(&format!("&json\nmulti={multi}\n")));
        assert!(out.contains("\"weird key\"=v\n"));
    }

    #[test]
    fn export_types_and_splice() {
        let payload = b64url_encode(br#"{"a":[1,2]}"#);
        let daiv = format!(
            ".!kaiv 1\n!str'::host=web01\n!int'/server::port=8080\n!std/enc/json'::cfg={payload}\n!str'/@tags::0=x\n!str'/@tags::1=y\n"
        );
        let json = export(&daiv).unwrap();
        assert_eq!(
            json,
            "{\"host\":\"web01\",\"server\":{\"port\":8080},\"cfg\":{\"a\":[1,2]},\"tags\":[\"x\",\"y\"]}\n"
        );
    }

    #[test]
    fn roundtrip_compact_is_byte_identical() {
        let src = br#"{"host":"web01","port":8080,"nested":{"a":[1,2.5,"s"]},"on":true}"#;
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = export(&daiv).unwrap();
        assert_eq!(json.trim_end(), std::str::from_utf8(src).unwrap());
    }

    #[test]
    fn import_native_arrays() {
        let out = import(br#"{"tags":["a","b"],"mixed":[1,true,null,"s"],"deep":[[1,2]],"objs":[{"a":1}],"none":[]}"#)
            .unwrap();
        // Homogeneous strings -> inline vector, no annotation.
        assert!(out.contains("/@tags;=a;b\n"));
        assert!(
            out.contains("!int\n/@mixed+=1\n!bool\n/@mixed+=true\n!null\n/@mixed+=\n/@mixed+=s\n")
        );
        // Directly nested arrays and objects embed per element.
        let inner = b64url_encode(b"[1,2]");
        assert!(out.contains(&format!("&json\n/@deep+={inner}\n")));
        // Object elements go native via the append-struct form.
        assert!(out.contains("!int\n/@objs+:=a=1\n"));
        // Empty arrays embed -- no element lines could represent them.
        let empty = b64url_encode(b"[]");
        assert!(out.contains(&format!("&json\nnone={empty}\n")));
    }

    #[test]
    fn import_native_objects() {
        let out = import(
            br#"{"limits":{"rps":500,"label":"std","on":true,"deep":{"x":1},"tags":[1]},"none":{}}"#,
        )
        .unwrap();
        assert!(out.contains("!int\n/limits::rps=500\n"));
        assert!(out.contains("/limits::label=std\n"));
        assert!(out.contains("!bool\n/limits::on=true\n"));
        // A member object recurses: inline struct at the nested path.
        assert!(out.contains("!int\n/limits/deep:=x=1\n"));
        // A member array goes native at its nested path.
        assert!(out.contains("!int\n/limits/@tags;=1\n"));
        // The empty object embeds -- no field lines could represent it.
        let empty = b64url_encode(b"{}");
        assert!(out.contains(&format!("&json\nnone={empty}\n")));
    }

    #[test]
    fn homogeneous_arrays_inline() {
        // Uniform ints -> one annotation, one ;= line.
        let out = import(br#"{"ports":[8443,8444,9000]}"#).unwrap();
        assert!(out.contains("!int\n/@ports;=8443;8444;9000\n"));
        // Uniform nulls -> empty elements under !null.
        let out = import(br#"{"x":[null,null]}"#).unwrap();
        assert!(out.contains("!null\n/@x;=;\n"));
        // A semicolon in a value defeats the element separator.
        let out = import(br#"{"t":["a;b","c"]}"#).unwrap();
        assert!(out.contains("/@t+=a;b\n/@t+=c\n"));
        // Over the 80-char cut-off -> per-element lines.
        let long = "v".repeat(60);
        let src = format!(r#"{{"t":["{long}","{long}"]}}"#);
        let out = import(src.as_bytes()).unwrap();
        assert!(out.contains(&format!("/@t+={long}\n")));
        assert!(!out.contains(";="));
    }

    #[test]
    fn member_arrays_native() {
        let out = import(
            br#"{"limits":{"rps":500,"tags":["a","b"],"mix":[1,"x"],"none":[],"objs":[{"k":1}]}}"#,
        )
        .unwrap();
        assert!(out.contains("!int\n/limits::rps=500\n"));
        // Homogeneous member array -> inline vector at the nested path.
        assert!(out.contains("/limits/@tags;=a;b\n"));
        // Heterogeneous -> per-element lines at the nested path.
        assert!(out.contains("!int\n/limits/@mix+=1\n/limits/@mix+=x\n"));
        // Empty member arrays embed; object elements embed per element.
        let empty = b64url_encode(b"[]");
        assert!(out.contains(&format!("&json\n/limits::none={empty}\n")));
        // Object elements go native via the append-struct form.
        assert!(out.contains("!int\n/limits/@objs+:=k=1\n"));
    }

    #[test]
    fn nested_objects_recursive() {
        let out = import(
            br#"{"a":{"b":{"c":1,"tags":["x","y"],"d":{"e":"deep"}},"f":"s"},"g":{"h":{}}}"#,
        )
        .unwrap();
        assert!(out.contains("!int\n/a/b::c=1\n"));
        assert!(out.contains("/a/b/@tags;=x;y\n"));
        // Homogeneous all the way down -> inline struct at depth 3.
        assert!(out.contains("/a/b/d:=e=deep\n"));
        assert!(out.contains("/a::f=s\n"));
        // Empty nested object embeds at its member path.
        let empty = b64url_encode(b"{}");
        assert!(out.contains(&format!("&json\n/g::h={empty}\n")));
    }

    #[test]
    fn struct_array_struct_native() {
        // Homogeneous struct elements -> idiomatic +:= appends.
        let out = import(
            br#"{"cluster":{"probes":[{"a":1,"b":5},{"a":2,"b":60}],"servers":[{"host":"a","port":1}],"mixed":[1,{"k":2}]}}"#,
        )
        .unwrap();
        assert!(
            out.contains("!int\n/cluster/@probes+:=a=1|b=5\n!int\n/cluster/@probes+:=a=2|b=60\n")
        );
        // Heterogeneous element members -> explicit-index per-member lines.
        assert!(out.contains("/cluster/@servers/0::host=a\n"));
        assert!(out.contains("!int\n/cluster/@servers/0::port=1\n"));
        // Scalar + object elements mix under explicit indices.
        assert!(out.contains("!int\n/cluster/@mixed::0=1\n"));
        assert!(out.contains("!int\n/cluster/@mixed/1:=k=2\n"));
        assert!(!out.contains("&json"));
    }

    #[test]
    fn struct_array_struct_roundtrip_byte_identical() {
        let src = br#"{"cluster":{"name":"eu1","servers":[{"host":"a","port":1},{"host":"b","port":2}],"probes":[{"path":"/hc","interval":5}],"mixed":[1,{"k":[2,3]},[4]],"empty":[{}]}}"#;
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = export(&daiv).unwrap();
        assert_eq!(json.trim_end(), std::str::from_utf8(src).unwrap());
    }

    #[test]
    fn deep_roundtrip_byte_identical() {
        let src = br#"{"a":{"b":{"c":1,"tags":["x","y"],"d":{"e":2.5,"f":true}},"s":"v"},"empty":{"h":{}},"top":[1,2]}"#;
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = export(&daiv).unwrap();
        assert_eq!(json.trim_end(), std::str::from_utf8(src).unwrap());
    }

    #[test]
    fn member_array_roundtrip_byte_identical() {
        let src =
            br#"{"limits":{"rps":500,"tags":["a","b"],"mix":[1,"x",null],"none":[]},"top":[1,2]}"#;
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = export(&daiv).unwrap();
        assert_eq!(json.trim_end(), std::str::from_utf8(src).unwrap());
    }

    #[test]
    fn inline_vector_roundtrip_byte_identical() {
        let src =
            br#"{"ports":[8443,8444,9000],"tags":["prod","eu"],"n":[null,null],"f":[1.5,2.5]}"#;
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = export(&daiv).unwrap();
        assert_eq!(json.trim_end(), std::str::from_utf8(src).unwrap());
    }

    #[test]
    fn homogeneous_objects_inline() {
        // Uniform int members -> one annotation, one := line.
        let out = import(br#"{"limits":{"rps":500,"burst":200}}"#).unwrap();
        assert!(out.contains("!int\n/limits:=rps=500|burst=200\n"));
        // Uniform strings -> inline without an annotation.
        let out = import(br#"{"svc":{"name":"api","tier":"gold"}}"#).unwrap();
        assert!(out.contains("/svc:=name=api|tier=gold\n"));
        // Heterogeneous -> per-member lines.
        let out = import(br#"{"m":{"a":1,"b":"x"}}"#).unwrap();
        assert!(out.contains("!int\n/m::a=1\n/m::b=x\n"));
        // A pipe in a value defeats the pair separator -> per-line.
        let out = import(br#"{"m":{"a":"x|y","b":"z"}}"#).unwrap();
        assert!(out.contains("/m::a=x|y\n/m::b=z\n"));
        // Over the 80-char cut-off -> per-line.
        let long = "v".repeat(40);
        let src = format!(r#"{{"m":{{"a":"{long}","b":"{long}"}}}}"#);
        let out = import(src.as_bytes()).unwrap();
        assert!(out.contains(&format!("/m::a={long}\n")));
        assert!(!out.contains(":="));
    }

    #[test]
    fn inline_struct_roundtrip_byte_identical() {
        let src = br#"{"limits":{"rps":500,"burst":200},"svc":{"name":"api","tier":"gold"}}"#;
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = export(&daiv).unwrap();
        assert_eq!(json.trim_end(), std::str::from_utf8(src).unwrap());
    }

    #[test]
    fn object_roundtrip_byte_identical() {
        let src = br#"{"svc":"billing","limits":{"rps":500,"burst":2.5,"on":true,"note":null,"deep":{"x":[1]}},"empty":{}}"#;
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = export(&daiv).unwrap();
        assert_eq!(json.trim_end(), std::str::from_utf8(src).unwrap());
    }

    #[test]
    fn import_flat_embeds_arrays() {
        let out = import_flat(br#"{"tags":["a","b"]}"#).unwrap();
        let arr = b64url_encode(br#"["a","b"]"#);
        assert!(out.contains(&format!("&json\ntags={arr}\n")));
        assert!(!out.contains("+="));
    }

    #[test]
    fn array_roundtrip_byte_identical() {
        let src = br#"{"tags":["a","b"],"mixed":[1,true,null,"s",2.5],"deep":[[1,2],{"k":"v"}],"empty":[]}"#;
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = export(&daiv).unwrap();
        assert_eq!(json.trim_end(), std::str::from_utf8(src).unwrap());
    }

    #[test]
    fn surrogate_pairs_decode() {
        let out = import(r#"{"emoji":"😀!"}"#.as_bytes()).unwrap();
        assert!(out.contains("emoji=\u{1F600}!\n"));
        assert!(import(br#"{"bad":"\ud83d"}"#).is_err()); // lone surrogate
    }

    #[test]
    fn root_must_be_object() {
        assert!(import(b"[1,2]").is_err());
        assert!(import(b"42").is_err());
        assert!(import(br#"{"a":1} trailing"#).is_err());
    }

    #[test]
    fn sigil_shaped_keys_roundtrip() {
        // Keys that look like kaiv syntax stay literal end to end: a
        // quoted `"@arr"` is a field named `@arr`, never an array
        // marker (export's segment walk is quote-aware).
        let src = br#"{"@arr":"at","a=b":"eq","a/b":"sl","a::b":"pr","min":1,"re":"r"}"#;
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let back = export(&daiv).unwrap();
        assert_eq!(back.trim_end().as_bytes(), src.as_slice());
    }

    #[test]
    fn quoted_key_arrays_roundtrip() {
        // An array under a key needing quotes compiles to an
        // `@"name"` segment: the marker is outside the quotes and
        // must still read as an array on export.
        let src = br#"{"m:Item":[{"c":"EUR","t":"Apples"},{"c":"EUR","t":"Pears"}],"a b":[1,2]}"#;
        let authored = import(src).unwrap();
        assert!(authored.contains("/@\"m:Item\"+:=c=EUR|t=Apples\n"));
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let back = export(&daiv).unwrap();
        assert_eq!(back.trim_end().as_bytes(), src.as_slice());
    }
}
