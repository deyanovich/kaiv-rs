//! Protocol Buffers import/export (`--features proto`) — hand-rolled,
//! zero dependencies. Unlike every other converter, the wire format
//! is not self-describing: both directions are driven by a `.proto`
//! schema (proto3; proto2 labels are tolerated), passed alongside the
//! data, plus a message name when the file has more than one
//! top-level message.
//!
//! Import decodes the wire bytes against the message: nested messages
//! land as namespaces, repeated fields as arrays (packed and unpacked
//! both accepted), maps as namespaces with stringified keys, enum
//! numbers as their symbol names, bytes as `std/enc/bin`, non-finite
//! floats as the `std/num` markers. Absent fields are omitted —
//! proto3 cannot distinguish absence from the default, so nothing is
//! invented. Unknown field numbers are skipped, like every protobuf
//! decoder (a documented loss). Groups are rejected.
//!
//! Export encodes the tree against the message, fields in schema
//! order: present members always serialize, even at default values
//! (minimal lossiness beats canonical omission); absent members are
//! omitted; numeric repeated fields pack. Known edges: proto3 cannot
//! represent null — a `!null` member is an error (omit the field
//! instead); an empty array vanishes (absence and emptiness coincide
//! on the wire); map keys stringify on import and re-parse on export;
//! enum symbols unknown to the schema are errors, but unknown enum
//! *numbers* pass through as integers; `import` statements are not
//! supported (schemas must be self-contained).

use crate::error::PipelineError;
use crate::json::{self, float_val, node_to_val, Val};

const MAX_DEPTH: usize = 512;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

pub fn import(input: &[u8], schema: &str, message: Option<&str>) -> Result<String, PipelineError> {
    let reg = parse_proto(schema)?;
    let mi = pick_message(&reg, message)?;
    let Val::Obj(members) = decode_msg(input, mi, &reg, 0)? else {
        unreachable!("decode_msg returns an object");
    };
    json::import_val(&members, false)
}

pub fn export(
    canonical: &str,
    schema: &str,
    message: Option<&str>,
) -> Result<Vec<u8>, PipelineError> {
    let reg = parse_proto(schema)?;
    let mi = pick_message(&reg, message)?;
    let root = node_to_val(&json::tree(canonical)?)?;
    let mut out = Vec::new();
    encode_msg(&root, mi, &reg, &mut out, 0)?;
    Ok(out)
}

// -------------------------------------------------------------- schema

/// A field's type. Message and enum types are registry indices, so
/// recursive schemas cost nothing.
#[derive(Clone, PartialEq)]
enum Pt {
    Double,
    Float,
    Int32,
    Int64,
    UInt32,
    UInt64,
    SInt32,
    SInt64,
    Fixed32,
    Fixed64,
    SFixed32,
    SFixed64,
    Bool,
    Str,
    Bytes,
    Msg(usize),
    Enm(usize),
}

struct Field {
    name: String,
    number: u32,
    ty: Pt,
    repeated: bool,
    /// `map<K, V>` — the wire shape is a repeated entry message with
    /// key = 1, value = 2.
    map: Option<(Pt, Pt)>,
}

struct Msg {
    fullname: String,
    top: bool,
    fields: Vec<Field>,
}

struct Enm {
    fullname: String,
    syms: Vec<(String, i32)>,
}

struct Registry {
    msgs: Vec<Msg>,
    enums: Vec<Enm>,
}

fn pick_message(reg: &Registry, name: Option<&str>) -> Result<usize, PipelineError> {
    match name {
        Some(n) => {
            let hits: Vec<usize> = reg
                .msgs
                .iter()
                .enumerate()
                .filter(|(_, m)| m.fullname == n || m.fullname.rsplit('.').next() == Some(n))
                .map(|(i, _)| i)
                .collect();
            match hits.as_slice() {
                [one] => Ok(*one),
                [] => Err(err(format!("no message named {n} in the schema"))),
                _ => Err(err(format!(
                    "message name {n} is ambiguous: {}",
                    hits.iter()
                        .map(|i| reg.msgs[*i].fullname.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))),
            }
        }
        None => {
            let tops: Vec<usize> = reg
                .msgs
                .iter()
                .enumerate()
                .filter(|(_, m)| m.top)
                .map(|(i, _)| i)
                .collect();
            match tops.as_slice() {
                [one] => Ok(*one),
                [] => Err(err("the schema declares no messages")),
                _ => Err(err(format!(
                    "the schema has several top-level messages ({}); pass --message",
                    tops.iter()
                        .map(|i| reg.msgs[*i].fullname.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))),
            }
        }
    }
}

// -------------------------------------------------------- .proto parse

#[derive(Clone, PartialEq, Debug)]
enum Tok {
    Ident(String),
    Int(i64),
    Text(String),
    Sym(u8),
    Eof,
}

struct Lex<'a> {
    b: &'a [u8],
    i: usize,
}

impl Lex<'_> {
    fn ws(&mut self) -> Result<(), PipelineError> {
        loop {
            while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                self.i += 1;
            }
            if self.b[self.i..].starts_with(b"//") {
                while !matches!(self.b.get(self.i), None | Some(b'\n')) {
                    self.i += 1;
                }
            } else if self.b[self.i..].starts_with(b"/*") {
                let rest = &self.b[self.i + 2..];
                let n = rest
                    .windows(2)
                    .position(|w| w == b"*/")
                    .ok_or_else(|| err("unterminated /* comment in .proto"))?;
                self.i += 2 + n + 2;
            } else {
                return Ok(());
            }
        }
    }

    fn next(&mut self) -> Result<Tok, PipelineError> {
        self.ws()?;
        let Some(&c) = self.b.get(self.i) else {
            return Ok(Tok::Eof);
        };
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = self.i;
            while matches!(self.b.get(self.i), Some(c) if c.is_ascii_alphanumeric() || *c == b'_') {
                self.i += 1;
            }
            return Ok(Tok::Ident(
                String::from_utf8_lossy(&self.b[start..self.i]).into_owned(),
            ));
        }
        if c.is_ascii_digit()
            || (c == b'-' && matches!(self.b.get(self.i + 1), Some(d) if d.is_ascii_digit()))
        {
            let start = self.i;
            self.i += 1;
            while matches!(self.b.get(self.i), Some(c) if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'x' | b'X'))
            {
                self.i += 1;
            }
            let text = std::str::from_utf8(&self.b[start..self.i]).expect("ascii");
            let v = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                i64::from_str_radix(hex, 16).ok()
            } else {
                text.parse::<i64>().ok()
            };
            return Ok(Tok::Int(v.ok_or_else(|| {
                err(format!("bad numeric literal in .proto: {text}"))
            })?));
        }
        if c == b'"' || c == b'\'' {
            self.i += 1;
            let start = self.i;
            loop {
                match self.b.get(self.i) {
                    None => return Err(err("unterminated string literal in .proto")),
                    Some(q) if *q == c => break,
                    Some(b'\\') => self.i += 2,
                    Some(_) => self.i += 1,
                }
            }
            let text = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
            self.i += 1;
            return Ok(Tok::Text(text));
        }
        self.i += 1;
        Ok(Tok::Sym(c))
    }

    fn peek(&mut self) -> Result<Tok, PipelineError> {
        let save = self.i;
        let t = self.next()?;
        self.i = save;
        Ok(t)
    }

    fn expect_sym(&mut self, s: u8) -> Result<(), PipelineError> {
        match self.next()? {
            Tok::Sym(c) if c == s => Ok(()),
            other => Err(err(format!(
                "expected `{}` in .proto, found {other:?}",
                s as char
            ))),
        }
    }

    fn ident(&mut self) -> Result<String, PipelineError> {
        match self.next()? {
            Tok::Ident(s) => Ok(s),
            other => Err(err(format!("expected a name in .proto, found {other:?}"))),
        }
    }

    /// A possibly dotted, possibly leading-dot type name.
    fn type_name(&mut self) -> Result<String, PipelineError> {
        let mut name = String::new();
        if self.peek()? == Tok::Sym(b'.') {
            self.next()?;
            name.push('.');
        }
        name.push_str(&self.ident()?);
        while self.peek()? == Tok::Sym(b'.') {
            self.next()?;
            name.push('.');
            name.push_str(&self.ident()?);
        }
        Ok(name)
    }

    /// Skip one statement: to `;` at depth 0, or over one balanced
    /// `{…}` block.
    fn skip_statement(&mut self) -> Result<(), PipelineError> {
        let mut depth = 0i32;
        loop {
            match self.next()? {
                Tok::Eof => return Err(err("unterminated statement in .proto")),
                Tok::Sym(b'{') => depth += 1,
                Tok::Sym(b'}') => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Tok::Sym(b';') if depth == 0 => return Ok(()),
                _ => {}
            }
        }
    }
}

/// A field before name resolution.
struct RawField {
    name: String,
    number: u32,
    repeated: bool,
    ty: String,
    map: Option<(String, String)>,
}

fn parse_proto(text: &str) -> Result<Registry, PipelineError> {
    let mut lex = Lex {
        b: text.as_bytes(),
        i: 0,
    };
    let mut package = String::new();
    let mut raw: Vec<(usize, Vec<RawField>)> = Vec::new(); // (msg index, fields)
    let mut reg = Registry {
        msgs: Vec::new(),
        enums: Vec::new(),
    };
    loop {
        match lex.next()? {
            Tok::Eof => break,
            Tok::Sym(b';') => {}
            Tok::Ident(kw) => match kw.as_str() {
                "syntax" | "edition" => lex.skip_statement()?,
                "package" => {
                    package = lex.type_name()?;
                    lex.expect_sym(b';')?;
                }
                "option" => lex.skip_statement()?,
                "import" => {
                    return Err(err(
                        "`import` is not supported: pass a self-contained .proto",
                    ))
                }
                "message" => parse_message(&mut lex, &package, true, &mut reg, &mut raw)?,
                "enum" => parse_enum(&mut lex, &package, &mut reg)?,
                "service" | "extend" => lex.skip_statement()?,
                other => return Err(err(format!("unexpected `{other}` in .proto"))),
            },
            other => return Err(err(format!("unexpected token in .proto: {other:?}"))),
        }
    }
    // Resolve raw field types now that every name is registered.
    for (mi, fields) in raw {
        let scope = reg.msgs[mi].fullname.clone();
        let mut resolved = Vec::with_capacity(fields.len());
        let mut numbers = std::collections::BTreeSet::new();
        for f in fields {
            if !numbers.insert(f.number) {
                return Err(err(format!(
                    "duplicate field number {} in {}",
                    f.number, scope
                )));
            }
            // Map fields carry their types in `map`; `ty` is unused.
            let ty = if f.map.is_some() {
                Pt::Str
            } else {
                resolve(&f.ty, &scope, &package, &reg)?
            };
            let map = match f.map {
                Some((k, v)) => {
                    let kt = resolve(&k, &scope, &package, &reg)?;
                    if !matches!(
                        kt,
                        Pt::Int32
                            | Pt::Int64
                            | Pt::UInt32
                            | Pt::UInt64
                            | Pt::SInt32
                            | Pt::SInt64
                            | Pt::Fixed32
                            | Pt::Fixed64
                            | Pt::SFixed32
                            | Pt::SFixed64
                            | Pt::Bool
                            | Pt::Str
                    ) {
                        return Err(err(format!("invalid map key type in {scope}")));
                    }
                    Some((kt, resolve(&v, &scope, &package, &reg)?))
                }
                None => None,
            };
            resolved.push(Field {
                name: f.name,
                number: f.number,
                ty,
                repeated: f.repeated,
                map,
            });
        }
        reg.msgs[mi].fields = resolved;
    }
    Ok(reg)
}

fn parse_message(
    lex: &mut Lex,
    scope: &str,
    top: bool,
    reg: &mut Registry,
    raw: &mut Vec<(usize, Vec<RawField>)>,
) -> Result<(), PipelineError> {
    let name = lex.ident()?;
    let fullname = join(scope, &name);
    lex.expect_sym(b'{')?;
    let mi = reg.msgs.len();
    reg.msgs.push(Msg {
        fullname: fullname.clone(),
        top,
        fields: Vec::new(),
    });
    let mut fields = Vec::new();
    parse_body(lex, &fullname, reg, raw, &mut fields)?;
    raw.push((mi, fields));
    Ok(())
}

/// A message body up to its closing `}`; `oneof` bodies recurse here
/// too (their fields are plain fields of the enclosing message).
fn parse_body(
    lex: &mut Lex,
    scope: &str,
    reg: &mut Registry,
    raw: &mut Vec<(usize, Vec<RawField>)>,
    fields: &mut Vec<RawField>,
) -> Result<(), PipelineError> {
    loop {
        match lex.next()? {
            Tok::Sym(b'}') => return Ok(()),
            Tok::Sym(b';') => {}
            Tok::Eof => return Err(err(format!("unterminated message {scope}"))),
            Tok::Ident(kw) => match kw.as_str() {
                "message" => parse_message(lex, scope, false, reg, raw)?,
                "enum" => parse_enum(lex, scope, reg)?,
                "option" | "reserved" | "extensions" | "extend" => lex.skip_statement()?,
                "oneof" => {
                    lex.ident()?;
                    lex.expect_sym(b'{')?;
                    parse_body(lex, scope, reg, raw, fields)?;
                }
                "group" => return Err(err("proto2 groups are not supported")),
                "map" => {
                    lex.expect_sym(b'<')?;
                    let k = lex.type_name()?;
                    lex.expect_sym(b',')?;
                    let v = lex.type_name()?;
                    lex.expect_sym(b'>')?;
                    fields.push(parse_field_tail(lex, String::new(), false, Some((k, v)))?);
                }
                "repeated" | "optional" | "required" => {
                    let repeated = kw == "repeated";
                    let ty = lex.type_name()?;
                    fields.push(parse_field_tail(lex, ty, repeated, None)?);
                }
                // A plain field whose type starts with this ident.
                first => {
                    let mut ty = first.to_string();
                    while lex.peek()? == Tok::Sym(b'.') {
                        lex.next()?;
                        ty.push('.');
                        ty.push_str(&lex.ident()?);
                    }
                    fields.push(parse_field_tail(lex, ty, false, None)?);
                }
            },
            Tok::Sym(b'.') => {
                // An absolute type name: `.pkg.Type name = n;`
                let ty = format!(".{}", lex.type_name()?);
                fields.push(parse_field_tail(lex, ty, false, None)?);
            }
            other => return Err(err(format!("unexpected token in {scope}: {other:?}"))),
        }
    }
}

/// `name = number [\[options\]] ;` after the type.
fn parse_field_tail(
    lex: &mut Lex,
    ty: String,
    repeated: bool,
    map: Option<(String, String)>,
) -> Result<RawField, PipelineError> {
    let name = lex.ident()?;
    lex.expect_sym(b'=')?;
    let number = match lex.next()? {
        Tok::Int(n) if n > 0 && n <= 536_870_911 => n as u32,
        other => return Err(err(format!("bad field number for {name}: {other:?}"))),
    };
    match lex.next()? {
        Tok::Sym(b';') => {}
        Tok::Sym(b'[') => {
            // Field options: skip to the matching `]`, then `;`.
            let mut depth = 1;
            while depth > 0 {
                match lex.next()? {
                    Tok::Sym(b'[') => depth += 1,
                    Tok::Sym(b']') => depth -= 1,
                    Tok::Eof => return Err(err("unterminated field options")),
                    _ => {}
                }
            }
            lex.expect_sym(b';')?;
        }
        other => return Err(err(format!("expected `;` after field {name}: {other:?}"))),
    }
    Ok(RawField {
        name,
        number,
        repeated,
        ty,
        map,
    })
}

fn parse_enum(lex: &mut Lex, scope: &str, reg: &mut Registry) -> Result<(), PipelineError> {
    let name = lex.ident()?;
    let fullname = join(scope, &name);
    lex.expect_sym(b'{')?;
    let mut syms = Vec::new();
    loop {
        match lex.next()? {
            Tok::Sym(b'}') => break,
            Tok::Sym(b';') => {}
            Tok::Eof => return Err(err(format!("unterminated enum {fullname}"))),
            Tok::Ident(kw) if kw == "option" || kw == "reserved" => lex.skip_statement()?,
            Tok::Ident(sym) => {
                lex.expect_sym(b'=')?;
                let n = match lex.next()? {
                    Tok::Int(n) => i32::try_from(n)
                        .map_err(|_| err(format!("enum value out of range: {sym} = {n}")))?,
                    other => return Err(err(format!("bad enum value for {sym}: {other:?}"))),
                };
                // Value options, then `;`.
                if lex.peek()? == Tok::Sym(b'[') {
                    lex.next()?;
                    let mut depth = 1;
                    while depth > 0 {
                        match lex.next()? {
                            Tok::Sym(b'[') => depth += 1,
                            Tok::Sym(b']') => depth -= 1,
                            Tok::Eof => return Err(err("unterminated enum value options")),
                            _ => {}
                        }
                    }
                }
                lex.expect_sym(b';')?;
                syms.push((sym, n));
            }
            other => return Err(err(format!("unexpected token in enum: {other:?}"))),
        }
    }
    reg.enums.push(Enm { fullname, syms });
    Ok(())
}

fn join(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        format!("{scope}.{name}")
    }
}

/// Resolve a type name lexically: innermost scope outward, then the
/// bare name; a leading `.` is absolute (package-qualified).
fn resolve(name: &str, scope: &str, package: &str, reg: &Registry) -> Result<Pt, PipelineError> {
    let scalar = match name {
        "double" => Some(Pt::Double),
        "float" => Some(Pt::Float),
        "int32" => Some(Pt::Int32),
        "int64" => Some(Pt::Int64),
        "uint32" => Some(Pt::UInt32),
        "uint64" => Some(Pt::UInt64),
        "sint32" => Some(Pt::SInt32),
        "sint64" => Some(Pt::SInt64),
        "fixed32" => Some(Pt::Fixed32),
        "fixed64" => Some(Pt::Fixed64),
        "sfixed32" => Some(Pt::SFixed32),
        "sfixed64" => Some(Pt::SFixed64),
        "bool" => Some(Pt::Bool),
        "string" => Some(Pt::Str),
        "bytes" => Some(Pt::Bytes),
        _ => None,
    };
    if let Some(s) = scalar {
        return Ok(s);
    }
    let lookup = |full: &str| -> Option<Pt> {
        if let Some(i) = reg.msgs.iter().position(|m| m.fullname == full) {
            return Some(Pt::Msg(i));
        }
        reg.enums
            .iter()
            .position(|e| e.fullname == full)
            .map(Pt::Enm)
    };
    if let Some(abs) = name.strip_prefix('.') {
        return lookup(abs).ok_or_else(|| err(format!("unresolved type in .proto: {name}")));
    }
    let mut prefix = scope;
    loop {
        if let Some(t) = lookup(&join(prefix, name)) {
            return Ok(t);
        }
        match prefix.rfind('.') {
            Some(dot) => prefix = &prefix[..dot],
            None if !prefix.is_empty() => prefix = "",
            None => break,
        }
    }
    // The package root, then the bare name.
    if !package.is_empty() {
        if let Some(t) = lookup(&join(package, name)) {
            return Ok(t);
        }
    }
    lookup(name).ok_or_else(|| err(format!("unresolved type in .proto: {name}")))
}

// -------------------------------------------------------------- decode

struct R<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> R<'a> {
    fn varint(&mut self) -> Result<u64, PipelineError> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = *self
                .b
                .get(self.i)
                .ok_or_else(|| err("truncated protobuf input"))?;
            self.i += 1;
            if shift > 63 {
                return Err(err("varint too long"));
            }
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PipelineError> {
        let end = self
            .i
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .ok_or_else(|| err("truncated protobuf input"))?;
        let out = &self.b[self.i..end];
        self.i = end;
        Ok(out)
    }

    fn ldelim(&mut self) -> Result<&'a [u8], PipelineError> {
        let n = usize::try_from(self.varint()?).map_err(|_| err("length overflows"))?;
        self.take(n)
    }

    fn skip(&mut self, wt: u8) -> Result<(), PipelineError> {
        match wt {
            0 => {
                self.varint()?;
            }
            1 => {
                self.take(8)?;
            }
            2 => {
                self.ldelim()?;
            }
            5 => {
                self.take(4)?;
            }
            _ => return Err(err("protobuf groups are not supported")),
        }
        Ok(())
    }
}

/// The wire type a scalar of this type uses.
fn wire_type(ty: &Pt) -> u8 {
    match ty {
        Pt::Int32
        | Pt::Int64
        | Pt::UInt32
        | Pt::UInt64
        | Pt::SInt32
        | Pt::SInt64
        | Pt::Bool
        | Pt::Enm(_) => 0,
        Pt::Fixed64 | Pt::SFixed64 | Pt::Double => 1,
        Pt::Str | Pt::Bytes | Pt::Msg(_) => 2,
        Pt::Fixed32 | Pt::SFixed32 | Pt::Float => 5,
    }
}

fn packable(ty: &Pt) -> bool {
    wire_type(ty) != 2
}

fn decode_msg(b: &[u8], mi: usize, reg: &Registry, depth: usize) -> Result<Val, PipelineError> {
    if depth > MAX_DEPTH {
        return Err(err("protobuf nesting too deep"));
    }
    let msg = &reg.msgs[mi];
    let mut slots: Vec<Option<Val>> = (0..msg.fields.len()).map(|_| None).collect();
    let mut r = R { b, i: 0 };
    while r.i < b.len() {
        let key = r.varint()?;
        let (fno, wt) = ((key >> 3) as u32, (key & 7) as u8);
        if fno == 0 {
            return Err(err("field number 0 on the wire"));
        }
        let Some(fi) = msg.fields.iter().position(|f| f.number == fno) else {
            r.skip(wt)?; // unknown field: skipped, like every decoder
            continue;
        };
        let f = &msg.fields[fi];
        if let Some((kt, vt)) = &f.map {
            if wt != 2 {
                return Err(err(format!("map field {} with wire type {wt}", f.name)));
            }
            let (k, v) = decode_map_entry(r.ldelim()?, kt, vt, reg, depth)?;
            let slot = slots[fi].get_or_insert_with(|| Val::Obj(Vec::new()));
            let Val::Obj(entries) = slot else {
                unreachable!()
            };
            match entries.iter_mut().find(|(ek, _)| *ek == k) {
                Some((_, ev)) => *ev = v, // last entry wins
                None => entries.push((k, v)),
            }
        } else if f.repeated {
            let slot = slots[fi].get_or_insert_with(|| Val::Arr(Vec::new()));
            let Val::Arr(items) = slot else {
                unreachable!()
            };
            if wt == 2 && packable(&f.ty) {
                let mut pr = R {
                    b: r.ldelim()?,
                    i: 0,
                };
                while pr.i < pr.b.len() {
                    items.push(decode_value(&mut pr, wire_type(&f.ty), &f.ty, reg, depth)?);
                }
            } else {
                items.push(decode_value(&mut r, wt, &f.ty, reg, depth)?);
            }
        } else {
            slots[fi] = Some(decode_value(&mut r, wt, &f.ty, reg, depth)?); // last wins
        }
    }
    let members = msg
        .fields
        .iter()
        .zip(slots)
        .filter_map(|(f, s)| s.map(|v| (f.name.clone(), v)))
        .collect();
    Ok(Val::Obj(members))
}

fn decode_value(
    r: &mut R,
    wt: u8,
    ty: &Pt,
    reg: &Registry,
    depth: usize,
) -> Result<Val, PipelineError> {
    if wt != wire_type(ty) {
        return Err(err(format!("unexpected wire type {wt} for this field")));
    }
    Ok(match ty {
        Pt::Int32 => Val::Num(i64::from(r.varint()? as i32).to_string()),
        Pt::Int64 => Val::Num((r.varint()? as i64).to_string()),
        Pt::UInt32 => Val::Num(u64::from(r.varint()? as u32).to_string()),
        Pt::UInt64 => Val::Num(r.varint()?.to_string()),
        Pt::SInt32 | Pt::SInt64 => {
            let v = r.varint()?;
            Val::Num((((v >> 1) as i64) ^ -((v & 1) as i64)).to_string())
        }
        Pt::Bool => Val::Bool(r.varint()? != 0),
        Pt::Fixed32 => {
            Val::Num(u32::from_le_bytes(r.take(4)?.try_into().expect("4 bytes")).to_string())
        }
        Pt::SFixed32 => {
            Val::Num(i32::from_le_bytes(r.take(4)?.try_into().expect("4 bytes")).to_string())
        }
        Pt::Float => float_val(f32::from_le_bytes(r.take(4)?.try_into().expect("4 bytes")) as f64),
        Pt::Fixed64 => {
            Val::Num(u64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes")).to_string())
        }
        Pt::SFixed64 => {
            Val::Num(i64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes")).to_string())
        }
        Pt::Double => float_val(f64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes"))),
        Pt::Str => Val::Str(
            String::from_utf8(r.ldelim()?.to_vec())
                .map_err(|_| err("protobuf string is not UTF-8"))?,
        ),
        Pt::Bytes => Val::Typed {
            lib: "std/enc".to_string(),
            name: "bin".to_string(),
            text: json::b64url_encode(r.ldelim()?),
        },
        Pt::Msg(mi) => decode_msg(r.ldelim()?, *mi, reg, depth + 1)?,
        Pt::Enm(ei) => {
            let n = r.varint()? as i64 as i32;
            match reg.enums[*ei].syms.iter().find(|(_, v)| *v == n) {
                Some((sym, _)) => Val::Str(sym.clone()),
                // Unknown enum numbers pass through, proto3-style.
                None => Val::Num(n.to_string()),
            }
        }
    })
}

/// One map entry message: key = 1, value = 2; proto3 defaults apply
/// when either is absent.
fn decode_map_entry(
    b: &[u8],
    kt: &Pt,
    vt: &Pt,
    reg: &Registry,
    depth: usize,
) -> Result<(String, Val), PipelineError> {
    let mut key: Option<Val> = None;
    let mut val: Option<Val> = None;
    let mut r = R { b, i: 0 };
    while r.i < b.len() {
        let tag = r.varint()?;
        match tag >> 3 {
            1 => key = Some(decode_value(&mut r, (tag & 7) as u8, kt, reg, depth)?),
            2 => val = Some(decode_value(&mut r, (tag & 7) as u8, vt, reg, depth + 1)?),
            _ => r.skip((tag & 7) as u8)?,
        }
    }
    let key = match key.unwrap_or_else(|| default_value(kt, reg)) {
        Val::Str(s) => s,
        Val::Num(n) => n,
        Val::Bool(b) => b.to_string(),
        _ => return Err(err("unsupported map key shape")),
    };
    Ok((key, val.unwrap_or_else(|| default_value(vt, reg))))
}

/// The proto3 default for a type (used for absent map entry halves).
fn default_value(ty: &Pt, reg: &Registry) -> Val {
    match ty {
        Pt::Bool => Val::Bool(false),
        Pt::Str => Val::Str(String::new()),
        Pt::Bytes => Val::Typed {
            lib: "std/enc".to_string(),
            name: "bin".to_string(),
            text: String::new(),
        },
        Pt::Float | Pt::Double => Val::Num("0.0".to_string()),
        Pt::Msg(_) => Val::Obj(Vec::new()),
        Pt::Enm(ei) => match reg.enums[*ei].syms.iter().find(|(_, v)| *v == 0) {
            Some((sym, _)) => Val::Str(sym.clone()),
            None => Val::Num("0".to_string()),
        },
        _ => Val::Num("0".to_string()),
    }
}

// -------------------------------------------------------------- encode

fn encode_msg(
    v: &Val,
    mi: usize,
    reg: &Registry,
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<(), PipelineError> {
    if depth > MAX_DEPTH {
        return Err(err("nesting too deep for protobuf export"));
    }
    let msg = &reg.msgs[mi];
    let Val::Obj(members) = v else {
        return Err(err(format!("{} expects a namespace", msg.fullname)));
    };
    for (k, _) in members {
        if !msg.fields.iter().any(|f| f.name == *k) {
            return Err(err(format!("no field named {k} in {}", msg.fullname)));
        }
    }
    for f in &msg.fields {
        let Some((_, mv)) = members.iter().find(|(k, _)| k == &f.name) else {
            continue; // absent: omitted from the wire
        };
        if matches!(mv, Val::Null) {
            return Err(err(format!(
                "proto3 cannot represent null (omit the field {} instead)",
                f.name
            )));
        }
        if let Some((kt, vt)) = &f.map {
            let Val::Obj(entries) = mv else {
                return Err(err(format!("map field {} expects a namespace", f.name)));
            };
            for (ek, ev) in entries {
                let mut entry = Vec::new();
                tag(1, wire_type(kt), &mut entry);
                encode_key(ek, kt, &mut entry)?;
                tag(2, wire_type(vt), &mut entry);
                encode_value(ev, vt, reg, &mut entry, depth)?;
                tag(f.number, 2, out);
                wvarint(entry.len() as u64, out);
                out.extend_from_slice(&entry);
            }
        } else if f.repeated {
            let Val::Arr(items) = mv else {
                return Err(err(format!("repeated field {} expects an array", f.name)));
            };
            if packable(&f.ty) {
                if items.is_empty() {
                    continue;
                }
                let mut packed = Vec::new();
                for item in items {
                    encode_value(item, &f.ty, reg, &mut packed, depth)?;
                }
                tag(f.number, 2, out);
                wvarint(packed.len() as u64, out);
                out.extend_from_slice(&packed);
            } else {
                for item in items {
                    tag(f.number, wire_type(&f.ty), out);
                    encode_value(item, &f.ty, reg, out, depth)?;
                }
            }
        } else {
            tag(f.number, wire_type(&f.ty), out);
            encode_value(mv, &f.ty, reg, out, depth)?;
        }
    }
    Ok(())
}

fn encode_key(k: &str, kt: &Pt, out: &mut Vec<u8>) -> Result<(), PipelineError> {
    let v = match kt {
        Pt::Str => Val::Str(k.to_string()),
        Pt::Bool => match k {
            "true" => Val::Bool(true),
            "false" => Val::Bool(false),
            _ => return Err(err(format!("bad bool map key: {k}"))),
        },
        _ => Val::Num(k.to_string()),
    };
    encode_value(
        &v,
        kt,
        &Registry {
            msgs: Vec::new(),
            enums: Vec::new(),
        },
        out,
        0,
    )
}

fn encode_value(
    v: &Val,
    ty: &Pt,
    reg: &Registry,
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<(), PipelineError> {
    let bad = |what: &str| err(format!("value does not fit a {what} field"));
    match (ty, v) {
        (Pt::Int32 | Pt::SFixed32, Val::Num(raw)) => {
            let n: i32 = raw.parse().map_err(|_| bad("32-bit integer"))?;
            match ty {
                Pt::Int32 => wvarint(i64::from(n) as u64, out),
                _ => out.extend(n.to_le_bytes()),
            }
        }
        (Pt::Int64 | Pt::SFixed64, Val::Num(raw)) => {
            let n: i64 = raw.parse().map_err(|_| bad("64-bit integer"))?;
            match ty {
                Pt::Int64 => wvarint(n as u64, out),
                _ => out.extend(n.to_le_bytes()),
            }
        }
        (Pt::UInt32 | Pt::Fixed32, Val::Num(raw)) => {
            let n: u32 = raw.parse().map_err(|_| bad("unsigned 32-bit integer"))?;
            match ty {
                Pt::UInt32 => wvarint(u64::from(n), out),
                _ => out.extend(n.to_le_bytes()),
            }
        }
        (Pt::UInt64 | Pt::Fixed64, Val::Num(raw)) => {
            let n: u64 = raw.parse().map_err(|_| bad("unsigned 64-bit integer"))?;
            match ty {
                Pt::UInt64 => wvarint(n, out),
                _ => out.extend(n.to_le_bytes()),
            }
        }
        (Pt::SInt32, Val::Num(raw)) => {
            let n: i32 = raw.parse().map_err(|_| bad("32-bit integer"))?;
            wvarint(u64::from(((n << 1) ^ (n >> 31)) as u32), out);
        }
        (Pt::SInt64, Val::Num(raw)) => {
            let n: i64 = raw.parse().map_err(|_| bad("64-bit integer"))?;
            wvarint(((n << 1) ^ (n >> 63)) as u64, out);
        }
        (Pt::Float, Val::Num(raw)) => {
            let f: f64 = raw.parse().map_err(|_| bad("float"))?;
            out.extend((f as f32).to_le_bytes());
        }
        (Pt::Double, Val::Num(raw)) => {
            let f: f64 = raw.parse().map_err(|_| bad("double"))?;
            out.extend(f.to_le_bytes());
        }
        (Pt::Float | Pt::Double, Val::Typed { lib, text, .. }) if lib == "std/num" => {
            let f = match text.as_str() {
                "inf" => f64::INFINITY,
                "-inf" => f64::NEG_INFINITY,
                _ => f64::NAN,
            };
            match ty {
                Pt::Float => out.extend((f as f32).to_le_bytes()),
                _ => out.extend(f.to_le_bytes()),
            }
        }
        (Pt::Bool, Val::Bool(b)) => wvarint(u64::from(*b), out),
        (Pt::Str, Val::Str(s)) => {
            wvarint(s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        (Pt::Str, Val::Typed { lib, name, text })
            if !(lib == "std/enc" && name == "bin") && lib != "std/num" =>
        {
            wvarint(text.len() as u64, out);
            out.extend_from_slice(text.as_bytes());
        }
        (Pt::Bytes, Val::Typed { lib, name, text }) if lib == "std/enc" && name == "bin" => {
            let b = json::b64url_decode(text).ok_or_else(|| err("invalid base64url payload"))?;
            wvarint(b.len() as u64, out);
            out.extend_from_slice(&b);
        }
        // A plain string fits a bytes field as its UTF-8.
        (Pt::Bytes, Val::Str(s)) => {
            wvarint(s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        (Pt::Msg(mi), Val::Obj(_)) => {
            let mut body = Vec::new();
            encode_msg(v, *mi, reg, &mut body, depth + 1)?;
            wvarint(body.len() as u64, out);
            out.extend_from_slice(&body);
        }
        (Pt::Enm(ei), Val::Str(sym)) => {
            let n = reg.enums[*ei]
                .syms
                .iter()
                .find(|(s, _)| s == sym)
                .map(|(_, n)| *n)
                .ok_or_else(|| {
                    err(format!(
                        "unknown enum symbol {sym} for {}",
                        reg.enums[*ei].fullname
                    ))
                })?;
            wvarint(i64::from(n) as u64, out);
        }
        (Pt::Enm(_), Val::Num(raw)) => {
            let n: i32 = raw.parse().map_err(|_| bad("enum"))?;
            wvarint(i64::from(n) as u64, out);
        }
        _ => return Err(err("value kind does not match the schema field type")),
    }
    Ok(())
}

fn tag(fno: u32, wt: u8, out: &mut Vec<u8>) {
    wvarint(u64::from(fno) << 3 | u64::from(wt), out);
}

fn wvarint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        if v < 0x80 {
            out.push(v as u8);
            return;
        }
        out.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
}

// ------------------------------------------------------ schema convert

/// `.proto` → authored `.saiv`, a sound weakening like the JSON
/// Schema converter: every emitted constraint is implied by the
/// source (wire-range bounds on the sized integers, enum symbol sets
/// unioned with `!int` because proto3 enums are open), and what kaiv
/// cannot express drops with a `//` comment. Every proto3 field is
/// optional (absence and the default coincide on the wire), so every
/// field emits `?=`; nested messages become namespaces, repeated
/// scalars vectors, repeated messages element blocks (scalar element
/// fields only), maps typed maps. Recursive messages cannot unfold
/// into a tree — the field drops with a note. The contract holds for
/// flat strings: see the `jsonschema` module doc for the shared
/// `std/enc/json` embed-channel limitation (non-flat strings need a
/// hand-widened `!str|std/enc/json` field).
pub fn import_schema(
    schema: &str,
    message: Option<&str>,
    name: &str,
) -> Result<String, PipelineError> {
    let reg = parse_proto(schema)?;
    let mi = pick_message(&reg, message)?;
    let mut ctx = SchemaCtx {
        reg: &reg,
        body: String::new(),
        imports: std::collections::BTreeSet::new(),
    };
    ctx.message_fields(mi, "", &mut vec![mi])?;
    let mut out = format!(".!kaivschema 1 {name}\n");
    for lib in &ctx.imports {
        out.push_str(&format!(".!types {lib}\n"));
    }
    out.push('\n');
    out.push_str(&ctx.body);
    Ok(out)
}

struct SchemaCtx<'a> {
    reg: &'a Registry,
    body: String,
    imports: std::collections::BTreeSet<&'static str>,
}

impl SchemaCtx<'_> {
    fn note(&mut self, msg: &str) {
        self.body.push_str(&format!("// dropped: {msg}\n"));
    }

    /// Optional fields must leave the Denormalizer something to
    /// materialize when absent (SchemaOptionalWithoutDefaultError):
    /// a `!`-typed annotation gains a null alternative — a sound
    /// weakening. `&bin` (b64, empty-string default applicable) and
    /// plain strings stay as they are.
    fn nullable(anno: String) -> String {
        match anno.strip_prefix('!') {
            Some(rest) if !rest.starts_with("null") => format!("!null|{rest}"),
            _ => anno,
        }
    }

    /// The annotation line for a scalar field type, or None for a
    /// plain string (unannotated).
    fn scalar_annotation(&mut self, ty: &Pt) -> Option<String> {
        Some(match ty {
            Pt::Double | Pt::Float => {
                self.imports.insert("std/num");
                "!float|std/num/inf|std/num/nan".to_string()
            }
            Pt::Int32 | Pt::SInt32 | Pt::SFixed32 => "!int[-2147483648,2147483647]".to_string(),
            Pt::Int64 | Pt::SInt64 | Pt::SFixed64 => {
                "!int[-9223372036854775808,9223372036854775807]".to_string()
            }
            Pt::UInt32 | Pt::Fixed32 => "!int[0,4294967295]".to_string(),
            Pt::UInt64 | Pt::Fixed64 => "!int[0,18446744073709551615]".to_string(),
            Pt::Bool => "!bool".to_string(),
            Pt::Str => return None,
            Pt::Bytes => {
                self.imports.insert("std/enc");
                "&bin".to_string()
            }
            // proto3 enums are open: unknown numbers decode as ints.
            Pt::Enm(ei) => {
                let syms: Vec<&str> = self.reg.enums[*ei]
                    .syms
                    .iter()
                    .map(|(s, _)| s.as_str())
                    .collect();
                if syms.is_empty() {
                    "!int".to_string()
                } else {
                    format!("!int|str{{{}}}", syms.join(","))
                }
            }
            Pt::Msg(_) => unreachable!("message fields are handled structurally"),
        })
    }

    fn message_fields(
        &mut self,
        mi: usize,
        path: &str,
        visiting: &mut Vec<usize>,
    ) -> Result<(), PipelineError> {
        if visiting.len() > 32 {
            return Err(err("message nesting too deep"));
        }
        let fields: Vec<usize> = (0..self.reg.msgs[mi].fields.len()).collect();
        for fi in fields {
            let f = &self.reg.msgs[mi].fields[fi];
            let (fname, fty, frep, fmap) =
                (f.name.clone(), f.ty.clone(), f.repeated, f.map.clone());
            let Ok(key) = crate::jsonschema::kaiv_key(&fname) else {
                self.note(&format!("unrepresentable field name: {fname:?}"));
                continue;
            };
            if let Some((kt, vt)) = fmap {
                if !matches!(kt, Pt::Str) {
                    self.note(&format!(
                        "map key type of {} (kaiv map keys are names; keys stringify)",
                        disp2(path, &fname)
                    ));
                }
                match vt {
                    Pt::Msg(_) => {
                        self.note(&format!("message-valued map at {}", disp2(path, &fname)))
                    }
                    ref v => {
                        let core = match v {
                            Pt::Bool => "bool",
                            Pt::Str => "str",
                            Pt::Double | Pt::Float => "float",
                            Pt::Bytes | Pt::Enm(_) => {
                                self.note(&format!(
                                    "map value type of {} (untyped map emitted)",
                                    disp2(path, &fname)
                                ));
                                "str"
                            }
                            _ => "int",
                        };
                        self.body
                            .push_str(&format!("!map<{core}>\n{}?=\n", lhs(path, &key)));
                    }
                }
                continue;
            }
            if frep {
                match fty {
                    Pt::Msg(m) => {
                        if visiting.contains(&m) {
                            self.note(&format!("recursive message at {}", disp2(path, &fname)));
                            continue;
                        }
                        self.body.push_str(&format!("[{path}/@{key}]\n"));
                        let inner: Vec<usize> = (0..self.reg.msgs[m].fields.len()).collect();
                        for ifi in inner {
                            let inf = &self.reg.msgs[m].fields[ifi];
                            let (iname, ity, irep, imap) = (
                                inf.name.clone(),
                                inf.ty.clone(),
                                inf.repeated,
                                inf.map.is_some(),
                            );
                            if irep || imap || matches!(ity, Pt::Msg(_)) {
                                self.note(&format!(
                                    "non-scalar element field {iname} at {}",
                                    disp2(path, &fname)
                                ));
                                continue;
                            }
                            let Ok(ikey) = crate::jsonschema::kaiv_key(&iname) else {
                                self.note(&format!("unrepresentable field name: {iname:?}"));
                                continue;
                            };
                            if let Some(anno) = self.scalar_annotation(&ity) {
                                let anno = Self::nullable(anno);
                                self.body.push_str(&format!("{anno}\n"));
                            }
                            self.body.push_str(&format!("{ikey}?=\n"));
                        }
                        self.body.push_str("[]\n");
                    }
                    ref t => {
                        if let Some(anno) = self.scalar_annotation(t) {
                            self.body.push_str(&format!("{anno}\n"));
                        }
                        self.body.push_str(&format!("{path}/@{key};=\n"));
                    }
                }
                continue;
            }
            match fty {
                Pt::Msg(m) => {
                    if visiting.contains(&m) {
                        self.note(&format!("recursive message at {}", disp2(path, &fname)));
                        continue;
                    }
                    visiting.push(m);
                    self.message_fields(m, &format!("{path}/{key}"), visiting)?;
                    visiting.pop();
                }
                ref t => {
                    if let Some(anno) = self.scalar_annotation(t) {
                        let anno = Self::nullable(anno);
                        self.body.push_str(&format!("{anno}\n"));
                    }
                    self.body.push_str(&format!("{}?=\n", lhs(path, &key)));
                }
            }
        }
        Ok(())
    }
}

fn lhs(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}::{key}")
    }
}

fn disp2(path: &str, name: &str) -> String {
    if path.is_empty() {
        format!("root/{name}")
    } else {
        format!("{path}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = r#"
        syntax = "proto3";
        package acme;

        // A service config.
        message Config {
            string host = 1;
            int32 port = 2;
            bool active = 3;
            double ratio = 4;
            bytes blob = 5;
            repeated string tags = 6;
            repeated int64 counts = 7;
            Limits limits = 8;
            map<string, int64> quotas = 9;
            Level level = 10;
            sint32 delta = 11;
            fixed32 crc = 12;
            oneof answer {
                string text = 13;
                int64 code = 14;
            }
            message Limits {
                int64 rps = 1;
                int64 burst = 2;
            }
            enum Level {
                LOW = 0;
                MID = 1;
                HIGH = 2;
            }
        }
    "#;

    fn build(authored: &str) -> String {
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        crate::denorm::denormalize(&raiv).unwrap()
    }

    fn roundtrip(src: &[u8]) -> Vec<u8> {
        export(&build(&import(src, SCHEMA, None).unwrap()), SCHEMA, None).unwrap()
    }

    /// Hand-encode the reference message.
    fn sample() -> Vec<u8> {
        let mut b = Vec::new();
        tag(1, 2, &mut b);
        wvarint(5, &mut b);
        b.extend(b"web01");
        tag(2, 0, &mut b);
        wvarint(8443, &mut b);
        tag(3, 0, &mut b);
        wvarint(1, &mut b);
        tag(4, 1, &mut b);
        b.extend(2.5f64.to_le_bytes());
        tag(5, 2, &mut b);
        wvarint(3, &mut b);
        b.extend([0x00, 0xff, 0x10]);
        for t in [&b"prod"[..], b"eu"] {
            tag(6, 2, &mut b);
            wvarint(t.len() as u64, &mut b);
            b.extend(t);
        }
        tag(7, 2, &mut b); // packed
        wvarint(3, &mut b);
        wvarint(1, &mut b);
        wvarint(2, &mut b);
        wvarint(3, &mut b);
        let mut limits = Vec::new();
        tag(1, 0, &mut limits);
        wvarint(500, &mut limits);
        tag(2, 0, &mut limits);
        wvarint(900, &mut limits);
        tag(8, 2, &mut b);
        wvarint(limits.len() as u64, &mut b);
        b.extend(limits);
        let mut entry = Vec::new();
        tag(1, 2, &mut entry);
        wvarint(4, &mut entry);
        entry.extend(b"api2");
        tag(2, 0, &mut entry);
        wvarint(100, &mut entry);
        tag(9, 2, &mut b);
        wvarint(entry.len() as u64, &mut b);
        b.extend(entry);
        tag(10, 0, &mut b);
        wvarint(2, &mut b); // HIGH
        tag(11, 0, &mut b);
        wvarint(9, &mut b); // sint32 -5
        tag(12, 5, &mut b);
        b.extend(0xdeadbeefu32.to_le_bytes());
        tag(13, 2, &mut b);
        wvarint(2, &mut b);
        b.extend(b"ok");
        b
    }

    #[test]
    fn import_typing_and_natives() {
        let out = import(&sample(), SCHEMA, None).unwrap();
        assert!(out.contains("host=web01\n"));
        assert!(out.contains("!int\nport=8443\n"));
        assert!(out.contains("!bool\nactive=true\n"));
        assert!(out.contains("!float\nratio=2.5\n"));
        assert!(out.contains(&format!(
            "&bin\nblob={}\n",
            json::b64url_encode(&[0x00, 0xff, 0x10])
        )));
        assert!(out.contains("/@tags;=prod;eu\n"));
        assert!(out.contains("!int\n/@counts;=1;2;3\n"));
        assert!(out.contains("!int\n/limits:=rps=500|burst=900\n"));
        assert!(out.contains("!int\n/quotas:=api2=100\n"));
        assert!(out.contains("level=HIGH\n"));
        assert!(out.contains("!int\ndelta=-5\n"));
        assert!(out.contains("!int\ncrc=3735928559\n"));
        assert!(out.contains("text=ok\n"));
        // Byte-identical round trip (packed repeated, schema order).
        assert_eq!(roundtrip(&sample()), sample());
    }

    #[test]
    fn unpacked_ints_and_unknown_fields_and_last_wins() {
        let mut b = Vec::new();
        // counts unpacked despite proto3 packed default.
        for v in [1u64, 2, 3] {
            tag(7, 0, &mut b);
            wvarint(v, &mut b);
        }
        // Unknown field numbers: every wire type skips.
        tag(99, 0, &mut b);
        wvarint(7, &mut b);
        tag(98, 2, &mut b);
        wvarint(2, &mut b);
        b.extend(b"zz");
        tag(97, 5, &mut b);
        b.extend(1u32.to_le_bytes());
        tag(96, 1, &mut b);
        b.extend(1u64.to_le_bytes());
        // Singular field twice: the last value wins.
        tag(2, 0, &mut b);
        wvarint(1, &mut b);
        tag(2, 0, &mut b);
        wvarint(2, &mut b);
        let out = import(&b, SCHEMA, None).unwrap();
        assert!(out.contains("!int\n/@counts;=1;2;3\n"));
        assert!(out.contains("!int\nport=2\n"));
        assert!(!out.contains("zz"));
    }

    #[test]
    fn recursive_messages_decode() {
        let schema = r#"
            syntax = "proto3";
            message Node { Node next = 1; int32 v = 2; }
        "#;
        let mut inner = Vec::new();
        tag(2, 0, &mut inner);
        wvarint(2, &mut inner);
        let mut b = Vec::new();
        tag(1, 2, &mut b);
        wvarint(inner.len() as u64, &mut b);
        b.extend(&inner);
        tag(2, 0, &mut b);
        wvarint(1, &mut b);
        let out = import(&b, schema, None).unwrap();
        assert!(out.contains("!int\n/next:=v=2\n"));
        assert!(out.contains("!int\nv=1\n"));
    }

    #[test]
    fn int_map_keys_and_unknown_enums() {
        let schema = r#"
            syntax = "proto3";
            message M {
                map<int32, string> names = 1;
                E e = 2;
                enum E { A = 0; }
            }
        "#;
        let mut entry = Vec::new();
        tag(1, 0, &mut entry);
        wvarint(7, &mut entry);
        tag(2, 2, &mut entry);
        wvarint(3, &mut entry);
        entry.extend(b"sev");
        let mut b = Vec::new();
        tag(1, 2, &mut b);
        wvarint(entry.len() as u64, &mut b);
        b.extend(&entry);
        tag(2, 0, &mut b);
        wvarint(5, &mut b); // no symbol
        let out = import(&b, schema, None).unwrap();
        assert!(out.contains("/names::\"7\"=sev\n"));
        assert!(out.contains("!int\ne=5\n"));
        // The int key re-parses on export; the unknown enum number
        // re-encodes as itself.
        let back = export(&build(&out), schema, None).unwrap();
        let again = import(&back, schema, None).unwrap();
        assert_eq!(out, again);
    }

    #[test]
    fn negative_int32_and_nonfinite() {
        let schema = r#"
            syntax = "proto3";
            message M { int32 a = 1; double d = 2; float f = 3; }
        "#;
        let mut b = Vec::new();
        tag(1, 0, &mut b);
        wvarint((-3i64) as u64, &mut b); // 10-byte varint
        tag(2, 1, &mut b);
        b.extend(f64::INFINITY.to_le_bytes());
        tag(3, 5, &mut b);
        b.extend(f32::NAN.to_le_bytes());
        let out = import(&b, schema, None).unwrap();
        assert!(out.contains("!int\na=-3\n"));
        assert!(out.contains("&inf\nd=inf\n"));
        assert!(out.contains("&nan\nf=nan\n"));
        let back = export(&build(&out), schema, None).unwrap();
        let again = import(&back, schema, None).unwrap();
        assert_eq!(out, again);
    }

    #[test]
    fn cross_format_json_to_proto() {
        let authored = crate::json::import(
            br#"{"host":"eu1","port":80,"tags":["a"],"limits":{"rps":1},"quotas":{"x":2}}"#,
        )
        .unwrap();
        let bytes = export(&build(&authored), SCHEMA, Some("Config")).unwrap();
        let out = import(&bytes, SCHEMA, Some("acme.Config")).unwrap();
        assert!(out.contains("host=eu1\n"));
        assert!(out.contains("!int\nport=80\n"));
        assert!(out.contains("/@tags;=a\n"));
        assert!(out.contains("!int\n/limits:=rps=1\n"));
        assert!(out.contains("!int\n/quotas:=x=2\n"));
    }

    #[test]
    fn export_rejects_bad_shapes() {
        let e = |doc: &str| export(&build(doc), SCHEMA, None).unwrap_err().to_string();
        assert!(e(".!kaiv 1\n\nnope=1\n").contains("no field named nope"));
        assert!(e(".!kaiv 1\n!null\nhost=\n").contains("null"));
        // An untyped (string) value against an int field.
        assert!(e(".!kaiv 1\n\nport=abc\n").contains("does not match"));
        assert!(e(".!kaiv 1\n\nlevel=NADA\n").contains("unknown enum symbol"));
        // 2^31 overflows int32.
        assert!(e(".!kaiv 1\n!int\nport=2147483648\n").contains("32-bit"));
    }

    #[test]
    fn schema_parse_rejects() {
        assert!(parse_proto("import \"other.proto\";")
            .err()
            .unwrap()
            .to_string()
            .contains("import"));
        assert!(parse_proto("message M { int32 a = 1; int32 b = 1; }")
            .err()
            .unwrap()
            .to_string()
            .contains("duplicate field number"));
        assert!(parse_proto("message M { Unknown u = 1; }")
            .err()
            .unwrap()
            .to_string()
            .contains("unresolved type"));
        assert!(parse_proto("message M { map<Other, int32> m = 1; }").is_err());
        // Two top-level messages need an explicit pick.
        let reg = parse_proto("message A { int32 x = 1; } message B { int32 x = 1; }").unwrap();
        assert!(pick_message(&reg, None)
            .unwrap_err()
            .to_string()
            .contains("--message"));
        assert!(pick_message(&reg, Some("B")).is_ok());
    }

    #[test]
    fn schema_convert_core_mapping() {
        let saiv = import_schema(SCHEMA, None, "acme/config").unwrap();
        assert!(saiv.starts_with(".!kaivschema 1 acme/config\n"));
        assert!(saiv.contains(".!types std/enc\n"));
        assert!(saiv.contains(".!types std/num\n"));
        assert!(saiv.contains("host?=\n"));
        assert!(saiv.contains("!null|int[-2147483648,2147483647]\nport?=\n"));
        assert!(saiv.contains("!null|bool\nactive?=\n"));
        assert!(saiv.contains("!null|float|std/num/inf|std/num/nan\nratio?=\n"));
        assert!(saiv.contains("&bin\nblob?=\n"));
        assert!(saiv.contains("/@tags;=\n"));
        assert!(saiv.contains("!int[-9223372036854775808,9223372036854775807]\n/@counts;=\n"));
        assert!(saiv.contains("!null|int[-9223372036854775808,9223372036854775807]\n/limits::rps?=\n"));
        assert!(saiv.contains("!map<int>\nquotas?=\n"));
        assert!(saiv.contains("!null|int|str{LOW,MID,HIGH}\nlevel?=\n"));
        assert!(saiv.contains("!null|int[-2147483648,2147483647]\ndelta?=\n"));
        assert!(saiv.contains("!null|int[0,4294967295]\ncrc?=\n"));
        // oneof members are ordinary optional fields.
        assert!(saiv.contains("text?=\n"));
        // The schema compiles, and a document decoded from the wire
        // by the DATA converter validates against it — after the
        // Denormalizer materializes the wire-omitted optional fields
        // (strict-lockstep parallel scan).
        let csaiv = crate::compile_schema(saiv.as_bytes()).unwrap();
        let sc = crate::parse_csaiv(&csaiv).unwrap();
        let r = crate::Resolver::offline();
        r.preload("acme/config", "csaiv", csaiv.into_bytes());
        let authored = import(&sample(), SCHEMA, None)
            .unwrap()
            .replacen(".!kaiv 1\n", ".!kaiv 1\n.!schema:acme/config\n", 1);
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize_with(&raiv, &r).unwrap();
        assert_eq!(crate::validate(&daiv, &sc), Ok(()));
        // The wire ranges are enforced (host materialized so the
        // strict-lockstep scan reaches the port constraint).
        assert_eq!(
            crate::validate(
                ".!kaiv 1\n!str'::host=\n!int'::port=99999999999\n",
                &sc
            ),
            Err(crate::AppError::ConstraintViolation)
        );
    }

    #[test]
    fn schema_convert_recursion_drops() {
        let schema = r#"
            syntax = "proto3";
            message Node { Node next = 1; repeated Node kids = 2; int32 v = 3; }
        "#;
        let saiv = import_schema(schema, None, "t").unwrap();
        assert!(saiv.contains("// dropped: recursive message at root/next\n"));
        assert!(saiv.contains("// dropped: recursive message at root/kids\n"));
        assert!(saiv.contains("!null|int[-2147483648,2147483647]\nv?=\n"));
        crate::compile_schema(saiv.as_bytes()).unwrap();
    }

    #[test]
    fn proto2_labels_and_options_tolerated() {
        let schema = r#"
            syntax = "proto2";
            option java_package = "com.acme";
            message M {
                required string name = 1;
                optional int32 n = 2 [default = 5];
                repeated string t = 3 [packed = true];
                reserved 4, 5;
                reserved "old";
            }
        "#;
        let mut b = Vec::new();
        tag(1, 2, &mut b);
        wvarint(2, &mut b);
        b.extend(b"ok");
        let out = import(&b, schema, None).unwrap();
        assert!(out.contains("name=ok\n"));
    }
}
