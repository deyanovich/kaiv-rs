//! Avro import/export (`--features avro`) — Object Container Files
//! (self-describing: the schema rides in the header as JSON, read
//! with the crate's own JSON parser), hand-rolled, zero dependencies.
//!
//! Import decodes against the embedded schema: records and maps land
//! as namespaces, arrays as arrays, enum symbols as strings;
//! bytes/fixed ride the typed channel as `std/enc/bin`; non-finite
//! floats are the `std/num` markers; the decimal logical type decodes
//! to an exact decimal token at its scale (bignum arithmetic — no
//! precision ceiling); the time logical types (`date`, `time-*`,
//! `timestamp-*`, `local-timestamp-*`) decode to `std/time` tokens.
//! Both the null and deflate codecs are read (the inflate decoder is
//! hand-rolled too). A file with a single record becomes the document
//! root; anything else (scalar schemas, zero or many items) lands
//! under a top-level `records` array — container framing is
//! normalized, not preserved.
//!
//! Export is schema-*inferring*: Avro cannot be written without a
//! schema, so one is synthesized from the tree — records field-wise
//! (a field missing from some array elements becomes a union with
//! null), int-shaped numbers as `long`, or as `decimal` (scale 0)
//! beyond i64 rather than silently rounding, float-shaped as
//! `double`, mixed scalar kinds as unions. Output is a single-record
//! null-codec OCF with a fixed sync marker (deterministic output).
//!
//! Known edges: snappy input is rejected (null/deflate only), and
//! export always writes the null codec; logical types other than
//! decimal and the time family decode as their underlying primitive;
//! `std/time` values export at micros resolution (a millis input
//! re-exports as micros; sub-microsecond fractions are an error);
//! datetime offsets normalize to UTC (the instant is preserved, the
//! offset spelling is not); mixed time kinds cannot unify; enums
//! degrade to strings on re-export; map keys and record fields merge
//! into one namespace notion; recursive schemas are rejected; field
//! names must be valid Avro names on export; a float cannot unify
//! with a beyond-i64 integer.

use crate::error::PipelineError;
use crate::json::{
    self, float_val, node_to_val, token_to_twos_complement, twos_complement_token, Val,
};

const MAGIC: &[u8] = b"Obj\x01";
/// Deterministic sync marker (16 bytes) — exports are reproducible.
const SYNC: &[u8; 16] = b"kaiv.avro.sync!!";
const MAX_DEPTH: usize = 512;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

// -------------------------------------------------------------- schema

#[derive(Clone, PartialEq)]
enum Sch {
    Null,
    Bool,
    Int,
    Long,
    Float,
    Double,
    Bytes,
    Str,
    Fixed(usize),
    /// The decimal logical type over bytes or fixed(size).
    Decimal {
        scale: usize,
        precision: usize,
        fixed: Option<usize>,
        /// Inference-only: every contributing value survives an f64
        /// round-trip, so the field may demote to `double` after the
        /// schema fold (`demote_safe_decimals`). Always false for
        /// parsed reader schemas and integer-shaped decimals.
        double_ok: bool,
    },
    /// A time logical type over int/long, mapped onto `std/time`.
    Time(Tl),
    Enum(Vec<String>),
    Array(Box<Sch>),
    Map(Box<Sch>),
    /// The name is assigned on export just before schema emission;
    /// import keeps the declared one (unused for decoding).
    Record(String, Vec<(String, Sch)>),
    Union(Vec<Sch>),
}

#[derive(Clone, Copy, PartialEq)]
enum Tl {
    Date,
    TimeMillis,
    TimeMicros,
    TsMillis,
    TsMicros,
    LocalTsMillis,
    LocalTsMicros,
}

/// Parse a schema from its JSON tree. Named types register after they
/// complete, so a self-reference (recursive schema) fails as an
/// unresolved name.
fn parse_schema(v: &Val, names: &mut Vec<(String, Sch)>) -> Result<Sch, PipelineError> {
    match v {
        Val::Str(s) => match s.as_str() {
            "null" => Ok(Sch::Null),
            "boolean" => Ok(Sch::Bool),
            "int" => Ok(Sch::Int),
            "long" => Ok(Sch::Long),
            "float" => Ok(Sch::Float),
            "double" => Ok(Sch::Double),
            "bytes" => Ok(Sch::Bytes),
            "string" => Ok(Sch::Str),
            name => names
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, s)| s.clone())
                .ok_or_else(|| err(format!("unresolved (or recursive) type reference: {name}"))),
        },
        Val::Arr(branches) => Ok(Sch::Union(
            branches
                .iter()
                .map(|b| parse_schema(b, names))
                .collect::<Result<_, _>>()?,
        )),
        Val::Obj(members) => {
            let get = |k: &str| members.iter().find(|(mk, _)| mk == k).map(|(_, mv)| mv);
            let Some(t) = get("type") else {
                return Err(err("schema object without a type"));
            };
            let Val::Str(ty) = t else {
                // The type attribute may itself be a schema.
                return parse_schema(t, names);
            };
            let logical = match get("logicalType") {
                Some(Val::Str(l)) => Some(l.as_str()),
                _ => None,
            };
            let decimal = logical == Some("decimal");
            let scale = || match get("scale") {
                Some(Val::Num(n)) => n
                    .parse::<usize>()
                    .map_err(|_| err(format!("bad decimal scale: {n}"))),
                None => Ok(0),
                _ => Err(err("bad decimal scale")),
            };
            let sch = match ty.as_str() {
                "record" | "error" => {
                    let Some(Val::Arr(fs)) = get("fields") else {
                        return Err(err("record schema without fields"));
                    };
                    let mut fields = Vec::new();
                    for f in fs {
                        let Val::Obj(fm) = f else {
                            return Err(err("record field is not an object"));
                        };
                        let fget = |k: &str| fm.iter().find(|(mk, _)| mk == k).map(|(_, mv)| mv);
                        let (Some(Val::Str(fname)), Some(ftype)) = (fget("name"), fget("type"))
                        else {
                            return Err(err("record field without name/type"));
                        };
                        fields.push((fname.clone(), parse_schema(ftype, names)?));
                    }
                    Sch::Record(name_of(get("name"))?, fields)
                }
                "enum" => {
                    let Some(Val::Arr(syms)) = get("symbols") else {
                        return Err(err("enum schema without symbols"));
                    };
                    let symbols = syms
                        .iter()
                        .map(|s| match s {
                            Val::Str(s) => Ok(s.clone()),
                            _ => Err(err("enum symbol is not a string")),
                        })
                        .collect::<Result<_, _>>()?;
                    Sch::Enum(symbols)
                }
                "fixed" => {
                    let Some(Val::Num(n)) = get("size") else {
                        return Err(err("fixed schema without size"));
                    };
                    let size = n
                        .parse::<usize>()
                        .map_err(|_| err(format!("bad fixed size: {n}")))?;
                    if decimal {
                        Sch::Decimal {
                            scale: scale()?,
                            precision: 0,
                            fixed: Some(size),
                            double_ok: false,
                        }
                    } else {
                        Sch::Fixed(size)
                    }
                }
                "array" => {
                    let items = get("items").ok_or_else(|| err("array schema without items"))?;
                    Sch::Array(Box::new(parse_schema(items, names)?))
                }
                "map" => {
                    let values = get("values").ok_or_else(|| err("map schema without values"))?;
                    Sch::Map(Box::new(parse_schema(values, names)?))
                }
                "bytes" if decimal => Sch::Decimal {
                    scale: scale()?,
                    precision: 0,
                    fixed: None,
                    double_ok: false,
                },
                // Time logical types over their required primitives.
                "int" if logical == Some("date") => Sch::Time(Tl::Date),
                "int" if logical == Some("time-millis") => Sch::Time(Tl::TimeMillis),
                "long" if logical == Some("time-micros") => Sch::Time(Tl::TimeMicros),
                "long" if logical == Some("timestamp-millis") => Sch::Time(Tl::TsMillis),
                "long" if logical == Some("timestamp-micros") => Sch::Time(Tl::TsMicros),
                "long" if logical == Some("local-timestamp-millis") => Sch::Time(Tl::LocalTsMillis),
                "long" if logical == Some("local-timestamp-micros") => Sch::Time(Tl::LocalTsMicros),
                // A primitive with attributes; other logical types
                // decode as their underlying primitive.
                _ => parse_schema(&Val::Str(ty.clone()), names)?,
            };
            // Register named types (short name and namespaced fullname).
            if matches!(ty.as_str(), "record" | "error" | "enum" | "fixed") {
                let name = name_of(get("name"))?;
                if let Some(Val::Str(ns)) = get("namespace") {
                    names.push((format!("{ns}.{name}"), sch.clone()));
                }
                names.push((name, sch.clone()));
            }
            Ok(sch)
        }
        _ => Err(err("unsupported schema JSON shape")),
    }
}

fn name_of(v: Option<&Val>) -> Result<String, PipelineError> {
    match v {
        Some(Val::Str(s)) => Ok(s.clone()),
        _ => Err(err("named schema without a name")),
    }
}

// -------------------------------------------------------------- import

pub fn import(input: &[u8]) -> Result<String, PipelineError> {
    let mut r = R { b: input, i: 0 };
    if r.take(4)? != MAGIC {
        return Err(err("not an Avro object container file (bad magic)"));
    }
    let mut schema_json: Option<Vec<u8>> = None;
    let mut codec = b"null".to_vec();
    loop {
        let mut n = r.long()?;
        if n == 0 {
            break;
        }
        if n < 0 {
            let _byte_size = r.long()?;
            n = -n;
        }
        for _ in 0..n {
            let key = r.lbytes()?;
            let val = r.lbytes()?;
            match key.as_slice() {
                b"avro.schema" => schema_json = Some(val),
                b"avro.codec" => codec = val,
                _ => {}
            }
        }
    }
    let sync: Vec<u8> = r.take(16)?.to_vec();
    let deflated = match codec.as_slice() {
        b"null" => false,
        b"deflate" => true,
        other => {
            return Err(err(format!(
                "unsupported Avro codec: {} (null and deflate only)",
                String::from_utf8_lossy(other)
            )))
        }
    };
    let sjson = schema_json.ok_or_else(|| err("missing avro.schema header"))?;
    let stext = std::str::from_utf8(&sjson).map_err(|_| err("avro.schema header is not UTF-8"))?;
    let sch = parse_schema(&json::parse_val(stext)?, &mut Vec::new())?;
    let mut items = Vec::new();
    while r.i < r.b.len() {
        let count = r.long()?;
        let size = r.len()?;
        let raw = r.take(size)?;
        let plain;
        let mut br = if deflated {
            plain = inflate(raw)?;
            R { b: &plain, i: 0 }
        } else {
            R { b: raw, i: 0 }
        };
        for _ in 0..count {
            items.push(decode(&mut br, &sch, 0)?);
        }
        if br.i != br.b.len() {
            return Err(err("block size does not match its content"));
        }
        if r.take(16)? != sync.as_slice() {
            return Err(err("sync marker mismatch"));
        }
    }
    let members = match <[Val; 1]>::try_from(items) {
        Ok([Val::Obj(ms)]) => ms,
        Ok([other]) => vec![("records".to_string(), Val::Arr(vec![other]))],
        Err(items) => vec![("records".to_string(), Val::Arr(items))],
    };
    json::import_val(&members, false)
}

struct R<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> R<'a> {
    fn u8(&mut self) -> Result<u8, PipelineError> {
        let v = *self
            .b
            .get(self.i)
            .ok_or_else(|| err("truncated Avro input"))?;
        self.i += 1;
        Ok(v)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PipelineError> {
        let end = self
            .i
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .ok_or_else(|| err("truncated Avro input"))?;
        let out = &self.b[self.i..end];
        self.i = end;
        Ok(out)
    }

    /// A zigzag varint long.
    fn long(&mut self) -> Result<i64, PipelineError> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.u8()?;
            if shift > 63 {
                return Err(err("varint too long"));
            }
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(((v >> 1) as i64) ^ -((v & 1) as i64));
            }
            shift += 7;
        }
    }

    /// A non-negative long used as a length.
    fn len(&mut self) -> Result<usize, PipelineError> {
        usize::try_from(self.long()?).map_err(|_| err("negative length"))
    }

    /// Length-prefixed bytes (the `bytes`/`string` wire shape).
    fn lbytes(&mut self) -> Result<Vec<u8>, PipelineError> {
        let n = self.len()?;
        Ok(self.take(n)?.to_vec())
    }

    /// Array/map block loop: `f` runs once per element.
    fn blocks(
        &mut self,
        mut f: impl FnMut(&mut Self) -> Result<(), PipelineError>,
    ) -> Result<(), PipelineError> {
        loop {
            let mut n = self.long()?;
            if n == 0 {
                return Ok(());
            }
            if n < 0 {
                let _byte_size = self.long()?;
                n = -n;
            }
            for _ in 0..n {
                f(self)?;
            }
        }
    }
}

fn decode(r: &mut R, sch: &Sch, depth: usize) -> Result<Val, PipelineError> {
    if depth > MAX_DEPTH {
        return Err(err("Avro nesting too deep"));
    }
    Ok(match sch {
        Sch::Null => Val::Null,
        Sch::Bool => match r.u8()? {
            0 => Val::Bool(false),
            1 => Val::Bool(true),
            b => return Err(err(format!("invalid boolean byte: {b}"))),
        },
        Sch::Int | Sch::Long => Val::Num(r.long()?.to_string()),
        Sch::Float => float_val(f32::from_le_bytes(r.take(4)?.try_into().expect("4 bytes")) as f64),
        Sch::Double => float_val(f64::from_le_bytes(r.take(8)?.try_into().expect("8 bytes"))),
        Sch::Bytes => bin(r.lbytes()?),
        Sch::Str => {
            Val::Str(String::from_utf8(r.lbytes()?).map_err(|_| err("Avro string is not UTF-8"))?)
        }
        Sch::Fixed(n) => bin(r.take(*n)?.to_vec()),
        Sch::Decimal { scale, fixed, .. } => {
            let bytes = match fixed {
                Some(n) => r.take(*n)?.to_vec(),
                None => r.lbytes()?,
            };
            Val::Num(twos_complement_token(&bytes, *scale))
        }
        Sch::Time(tl) => {
            let (name, text) = time_token(*tl, r.long()?)?;
            Val::Typed {
                lib: "std/time".to_string(),
                name: name.to_string(),
                text,
            }
        }
        Sch::Enum(symbols) => {
            let idx = r.len()?;
            Val::Str(
                symbols
                    .get(idx)
                    .ok_or_else(|| err(format!("enum index {idx} out of range")))?
                    .clone(),
            )
        }
        Sch::Array(el) => {
            let mut items = Vec::new();
            r.blocks(|r| {
                items.push(decode(r, el, depth + 1)?);
                Ok(())
            })?;
            Val::Arr(items)
        }
        Sch::Map(vt) => {
            let mut members: Vec<(String, Val)> = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            r.blocks(|r| {
                let key =
                    String::from_utf8(r.lbytes()?).map_err(|_| err("Avro map key is not UTF-8"))?;
                if !seen.insert(key.clone()) {
                    return Err(err(format!("duplicate map key: {key}")));
                }
                members.push((key, decode(r, vt, depth + 1)?));
                Ok(())
            })?;
            Val::Obj(members)
        }
        Sch::Record(_, fields) => {
            let mut members = Vec::with_capacity(fields.len());
            for (name, fsch) in fields {
                members.push((name.clone(), decode(r, fsch, depth + 1)?));
            }
            Val::Obj(members)
        }
        Sch::Union(branches) => {
            let idx = r.len()?;
            let branch = branches
                .get(idx)
                .ok_or_else(|| err(format!("union index {idx} out of range")))?;
            decode(r, branch, depth + 1)?
        }
    })
}

fn bin(bytes: Vec<u8>) -> Val {
    Val::Typed {
        lib: "std/enc".to_string(),
        name: "bin".to_string(),
        text: json::b64url_encode(&bytes),
    }
}

// --------------------------------------------------------------- export

pub fn export(canonical: &str) -> Result<Vec<u8>, PipelineError> {
    let root = node_to_val(&json::tree(canonical)?)?;
    let mut sch = infer(&root, 0)?;
    demote_safe_decimals(&mut sch);
    let mut used = std::collections::BTreeSet::new();
    name_records(&mut sch, "root", &mut used);
    let mut sjson = String::new();
    schema_json(&sch, &mut sjson)?;
    let mut payload = Vec::new();
    encode(&root, &sch, &mut payload, 0)?;
    let mut out = Vec::from(MAGIC);
    wlong(2, &mut out);
    wbytes(b"avro.schema", &mut out);
    wbytes(sjson.as_bytes(), &mut out);
    wbytes(b"avro.codec", &mut out);
    wbytes(b"null", &mut out);
    wlong(0, &mut out);
    out.extend_from_slice(SYNC);
    wlong(1, &mut out); // one record
    wlong(payload.len() as i64, &mut out);
    out.extend_from_slice(&payload);
    out.extend_from_slice(SYNC);
    Ok(out)
}

/// Infer the Avro schema of a value. Records are named later, in one
/// pass, so structurally equal trees infer equal schemas.
fn infer(v: &Val, depth: usize) -> Result<Sch, PipelineError> {
    if depth > MAX_DEPTH {
        return Err(err("nesting too deep for Avro export"));
    }
    Ok(match v {
        Val::Null => Sch::Null,
        Val::Bool(_) => Sch::Bool,
        Val::Num(raw) => num_schema(raw),
        Val::Str(_) => Sch::Str,
        Val::Typed { lib, name, .. } if lib == "std/enc" && name == "bin" => Sch::Bytes,
        Val::Typed { lib, .. } if lib == "std/num" => Sch::Double,
        Val::Typed { lib, name, .. } if lib == "std/time" => match name.as_str() {
            "date" => Sch::Time(Tl::Date),
            "time" => Sch::Time(Tl::TimeMicros),
            "datetime" => Sch::Time(Tl::TsMicros),
            "localdatetime" => Sch::Time(Tl::LocalTsMicros),
            _ => Sch::Str,
        },
        Val::Typed { .. } => Sch::Str,
        Val::Arr(items) => {
            let mut el = None;
            for item in items {
                let s = infer(item, depth + 1)?;
                el = Some(match el {
                    None => s,
                    Some(prev) => unify(prev, s)?,
                });
            }
            Sch::Array(Box::new(el.unwrap_or(Sch::Null)))
        }
        Val::Obj(members) => {
            let mut fields = Vec::with_capacity(members.len());
            for (k, mv) in members {
                if !avro_name_ok(k) {
                    return Err(err(format!("not a valid Avro field name: {k}")));
                }
                fields.push((k.clone(), infer(mv, depth + 1)?));
            }
            Sch::Record(String::new(), fields)
        }
    })
}

/// Integer-shaped tokens are `long`, or `decimal` (scale 0) beyond
/// i64 — never silently rounded. Fractional/exponent tokens infer as
/// exact `decimal` at their minimal scale; a field whose every value
/// survives an f64 round-trip is demoted back to `double` after the
/// fold (`demote_safe_decimals`), so ordinary data keeps its natural
/// Avro type and only genuinely heavy fields change representation —
/// mirroring the integer `long` → `decimal` precedent.
fn num_schema(raw: &str) -> Sch {
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        return if raw.parse::<i64>().is_ok() {
            Sch::Long
        } else {
            Sch::Decimal {
                scale: 0,
                precision: digits.len(),
                fixed: None,
                double_ok: false,
            }
        };
    }
    match to_decimal(raw) {
        Some((_, d, scale)) => Sch::Decimal {
            scale,
            precision: d.len().max(scale),
            fixed: None,
            double_ok: double_safe(raw),
        },
        // Unreachable through render_node's number gate; kept total.
        None => Sch::Double,
    }
}

/// Exact decimal reading of a JSON-number-shaped token: value =
/// ±digits × 10⁻ˢᶜᵃˡᵉ, minimal (no leading zeros, no trailing
/// fractional zeros; zero is `("0", 0)` with the sign dropped).
/// `None` for anything that is not a finite decimal numeral.
fn to_decimal(raw: &str) -> Option<(bool, String, usize)> {
    let (neg, rest) = match raw.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, raw),
    };
    let (mant, exp) = match rest.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.trim_start_matches('+').parse::<i64>().ok()?),
        None => (rest, 0),
    };
    let (int, frac) = match mant.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mant, ""),
    };
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    if !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut digits = format!("{int}{frac}");
    let mut scale = frac.len() as i64 - exp;
    let trimmed = digits.trim_start_matches('0');
    digits = trimmed.to_string();
    while scale > 0 && digits.ends_with('0') {
        digits.pop();
        scale -= 1;
    }
    if digits.is_empty() {
        return Some((false, "0".to_string(), 0));
    }
    while scale < 0 {
        digits.push('0');
        scale += 1;
    }
    Some((neg, digits, scale as usize))
}

/// Whether the token's decimal value survives the f64 trip: it parses
/// finite and the shortest re-rendering denotes the same decimal.
fn double_safe(raw: &str) -> bool {
    let Ok(f) = raw.parse::<f64>() else {
        return false;
    };
    if !f.is_finite() {
        return false;
    }
    to_decimal(&format!("{f}")) == to_decimal(raw)
}

/// A numeric token as an unscaled two's-complement integer at the
/// field's decimal scale (`1.5` at scale 2 → 150).
fn token_to_decimal_bytes(raw: &str, field_scale: usize) -> Result<Vec<u8>, PipelineError> {
    let (neg, mut digits, scale) =
        to_decimal(raw).ok_or_else(|| err(format!("not a finite number: {raw}")))?;
    if scale > field_scale {
        return Err(err(format!(
            "cannot represent {raw} exactly at decimal scale {field_scale}"
        )));
    }
    digits.extend(std::iter::repeat_n('0', field_scale - scale));
    let tok = if neg { format!("-{digits}") } else { digits };
    token_to_twos_complement(&tok)
}

/// Demote every all-values-double-safe decimal back to `double`
/// (post-fold; see `num_schema`).
fn demote_safe_decimals(sch: &mut Sch) {
    match sch {
        Sch::Decimal {
            double_ok: true, ..
        } => *sch = Sch::Double,
        Sch::Array(el) | Sch::Map(el) => demote_safe_decimals(el),
        Sch::Union(bs) => bs.iter_mut().for_each(demote_safe_decimals),
        Sch::Record(_, fs) => fs.iter_mut().for_each(|(_, s)| demote_safe_decimals(s)),
        _ => {}
    }
}

/// The least schema covering both. Records merge field-wise (a field
/// missing on one side becomes nullable), arrays merge element-wise,
/// numerics widen (`long` ⊂ `decimal`; `long` ⊂ `double`), and
/// distinct kinds form a union.
fn unify(a: Sch, b: Sch) -> Result<Sch, PipelineError> {
    if a == b {
        return Ok(a);
    }
    Ok(match (a, b) {
        (Sch::Union(u), x) => merge_union(u, x)?,
        (x, Sch::Union(u)) => merge_union(vec![x], Sch::Union(u))?,
        (Sch::Null, x) => merge_union(vec![Sch::Null], x)?,
        (x, Sch::Null) => merge_union(vec![x], Sch::Null)?,
        (Sch::Long, Sch::Double) | (Sch::Double, Sch::Long) => Sch::Double,
        (
            Sch::Long,
            Sch::Decimal {
                scale, precision, ..
            },
        )
        | (
            Sch::Decimal {
                scale, precision, ..
            },
            Sch::Long,
        ) => Sch::Decimal {
            scale,
            // Integer-digit capacity covers i64 (19 digits) and the
            // decimal side at the shared scale.
            precision: (precision - scale).max(19) + scale,
            fixed: None,
            // Value-blind: a long beyond 2^53 would not survive a
            // demotion to double, so a mixed field stays exact.
            double_ok: false,
        },
        (
            Sch::Decimal {
                scale: s1,
                precision: p1,
                double_ok: k1,
                ..
            },
            Sch::Decimal {
                scale: s2,
                precision: p2,
                double_ok: k2,
                ..
            },
        ) => {
            let scale = s1.max(s2);
            Sch::Decimal {
                scale,
                precision: (p1 - s1).max(p2 - s2) + scale,
                fixed: None,
                double_ok: k1 && k2,
            }
        }
        // During inference a Double arises only from non-finite
        // std/num markers; a double-safe decimal joins it losslessly.
        (Sch::Double, Sch::Decimal { double_ok: true, .. })
        | (Sch::Decimal { double_ok: true, .. }, Sch::Double) => Sch::Double,
        (Sch::Double, Sch::Decimal { .. }) | (Sch::Decimal { .. }, Sch::Double) => {
            return Err(err(
                "cannot unify a non-finite number with an exact decimal (Avro has no exact superset)",
            ))
        }
        (Sch::Time(_), Sch::Time(_)) => {
            return Err(err("cannot unify different time logical types"))
        }
        (Sch::Array(x), Sch::Array(y)) => Sch::Array(Box::new(unify(*x, *y)?)),
        (Sch::Record(_, f1), Sch::Record(_, f2)) => {
            let mut fields = f1;
            let mut in_second = std::collections::BTreeSet::new();
            for (k, s2) in f2 {
                in_second.insert(k.clone());
                match fields.iter().position(|(fk, _)| *fk == k) {
                    Some(pos) => {
                        let s1 = fields[pos].1.clone();
                        fields[pos].1 = unify(s1, s2)?;
                    }
                    // Present only on the second side: nullable.
                    None => fields.push((k, unify(s2, Sch::Null)?)),
                }
            }
            // Fields only on the first side are nullable too.
            for (k, s) in fields.iter_mut() {
                if !in_second.contains(k) {
                    let prev = std::mem::replace(s, Sch::Null);
                    *s = unify(prev, Sch::Null)?;
                }
            }
            Sch::Record(String::new(), fields)
        }
        (a, b) => Sch::Union(vec![a, b]),
    })
}

/// Fold `x` into union branches: same-kind branches unify in place,
/// new kinds append (Avro forbids duplicate kinds within a union).
fn merge_union(mut branches: Vec<Sch>, x: Sch) -> Result<Sch, PipelineError> {
    let xs = match x {
        Sch::Union(bs) => bs,
        other => vec![other],
    };
    for b in xs {
        match branches
            .iter()
            .position(|e| std::mem::discriminant(e) == std::mem::discriminant(&b))
        {
            Some(pos) => {
                let prev = branches[pos].clone();
                branches[pos] = unify(prev, b)?;
            }
            None => {
                // Numeric kinds share one slot.
                let numeric = |s: &Sch| matches!(s, Sch::Long | Sch::Double | Sch::Decimal { .. });
                if numeric(&b) {
                    if let Some(pos) = branches.iter().position(numeric) {
                        let prev = branches[pos].clone();
                        branches[pos] = unify(prev, b)?;
                        continue;
                    }
                }
                branches.push(b);
            }
        }
    }
    Ok(if branches.len() == 1 {
        branches.pop().expect("one branch")
    } else {
        Sch::Union(branches)
    })
}

/// Assign unique record names (field name at the point of use; the
/// root record is `root`).
fn name_records(sch: &mut Sch, hint: &str, used: &mut std::collections::BTreeSet<String>) {
    match sch {
        Sch::Record(name, fields) => {
            let mut cand = hint.to_string();
            let mut n = 2;
            while !used.insert(cand.clone()) {
                cand = format!("{hint}_{n}");
                n += 1;
            }
            *name = cand;
            for (k, f) in fields {
                name_records(f, k, used);
            }
        }
        Sch::Array(el) | Sch::Map(el) => name_records(el, hint, used),
        Sch::Union(branches) => {
            for b in branches {
                name_records(b, hint, used);
            }
        }
        _ => {}
    }
}

fn schema_json(sch: &Sch, out: &mut String) -> Result<(), PipelineError> {
    match sch {
        Sch::Null => out.push_str("\"null\""),
        Sch::Bool => out.push_str("\"boolean\""),
        Sch::Int => out.push_str("\"int\""),
        Sch::Long => out.push_str("\"long\""),
        Sch::Float => out.push_str("\"float\""),
        Sch::Double => out.push_str("\"double\""),
        Sch::Bytes => out.push_str("\"bytes\""),
        Sch::Str => out.push_str("\"string\""),
        Sch::Decimal {
            scale, precision, ..
        } => out.push_str(&format!(
            "{{\"type\":\"bytes\",\"logicalType\":\"decimal\",\"precision\":{precision},\"scale\":{scale}}}"
        )),
        Sch::Time(tl) => out.push_str(match tl {
            Tl::Date => "{\"type\":\"int\",\"logicalType\":\"date\"}",
            Tl::TimeMillis => "{\"type\":\"int\",\"logicalType\":\"time-millis\"}",
            Tl::TimeMicros => "{\"type\":\"long\",\"logicalType\":\"time-micros\"}",
            Tl::TsMillis => "{\"type\":\"long\",\"logicalType\":\"timestamp-millis\"}",
            Tl::TsMicros => "{\"type\":\"long\",\"logicalType\":\"timestamp-micros\"}",
            Tl::LocalTsMillis => "{\"type\":\"long\",\"logicalType\":\"local-timestamp-millis\"}",
            Tl::LocalTsMicros => "{\"type\":\"long\",\"logicalType\":\"local-timestamp-micros\"}",
        }),
        Sch::Array(el) => {
            out.push_str("{\"type\":\"array\",\"items\":");
            schema_json(el, out)?;
            out.push('}');
        }
        Sch::Record(name, fields) => {
            out.push_str(&format!("{{\"type\":\"record\",\"name\":\"{name}\",\"fields\":["));
            for (i, (k, f)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&format!("{{\"name\":\"{k}\",\"type\":"));
                schema_json(f, out)?;
                out.push('}');
            }
            out.push_str("]}");
        }
        Sch::Union(branches) => {
            out.push('[');
            for (i, b) in branches.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                schema_json(b, out)?;
            }
            out.push(']');
        }
        // Never inferred on export.
        Sch::Fixed(_) | Sch::Enum(_) | Sch::Map(_) => {
            return Err(err("internal: schema kind not inferred on export"))
        }
    }
    Ok(())
}

fn encode(v: &Val, sch: &Sch, out: &mut Vec<u8>, depth: usize) -> Result<(), PipelineError> {
    if depth > MAX_DEPTH {
        return Err(err("nesting too deep for Avro export"));
    }
    match (v, sch) {
        (_, Sch::Union(branches)) => {
            let idx = branches
                .iter()
                .position(|b| fits(v, b))
                .ok_or_else(|| err("internal: no union branch fits the value"))?;
            wlong(idx as i64, out);
            encode(v, &branches[idx], out, depth)?;
        }
        (Val::Null, Sch::Null) => {}
        (Val::Bool(b), Sch::Bool) => out.push(*b as u8),
        (Val::Num(raw), Sch::Long) => wlong(
            raw.parse::<i64>()
                .map_err(|_| err(format!("bad long: {raw}")))?,
            out,
        ),
        (Val::Num(raw), Sch::Double) => {
            let f: f64 = raw.parse().map_err(|_| err(format!("bad number: {raw}")))?;
            out.extend(f.to_le_bytes());
        }
        (Val::Num(raw), Sch::Decimal { scale, .. }) => {
            let bytes = token_to_decimal_bytes(raw, *scale)?;
            wbytes(&bytes, out);
        }
        (Val::Str(s), Sch::Str) => wbytes(s.as_bytes(), out),
        (Val::Typed { lib, name, text }, Sch::Bytes) if lib == "std/enc" && name == "bin" => {
            let b = json::b64url_decode(text).ok_or_else(|| err("invalid base64url payload"))?;
            wbytes(&b, out);
        }
        (Val::Typed { lib, text, .. }, Sch::Double) if lib == "std/num" => {
            let f = match text.as_str() {
                "inf" => f64::INFINITY,
                "-inf" => f64::NEG_INFINITY,
                _ => f64::NAN,
            };
            out.extend(f.to_le_bytes());
        }
        (Val::Typed { lib, name, text }, Sch::Time(tl)) if lib == "std/time" => {
            wlong(time_value(*tl, name, text)?, out)
        }
        (Val::Typed { text, .. }, Sch::Str) => wbytes(text.as_bytes(), out),
        (Val::Arr(items), Sch::Array(el)) => {
            if !items.is_empty() {
                wlong(items.len() as i64, out);
                for item in items {
                    encode(item, el, out, depth + 1)?;
                }
            }
            wlong(0, out);
        }
        (Val::Obj(members), Sch::Record(_, fields)) => {
            for (fname, fsch) in fields {
                match members.iter().find(|(k, _)| k == fname) {
                    Some((_, mv)) => encode(mv, fsch, out, depth + 1)?,
                    None => encode(&Val::Null, fsch, out, depth + 1)?,
                }
            }
        }
        _ => return Err(err("internal: value does not match its inferred schema")),
    }
    Ok(())
}

/// Does the value select this (unified) union branch? Branches are
/// kind-distinct, so a shallow kind check suffices.
fn fits(v: &Val, s: &Sch) -> bool {
    match (v, s) {
        (Val::Null, Sch::Null) => true,
        (Val::Bool(_), Sch::Bool) => true,
        (Val::Num(_), Sch::Long | Sch::Double | Sch::Decimal { .. }) => true,
        (Val::Str(_), Sch::Str) => true,
        (Val::Typed { lib, name, .. }, Sch::Bytes) => lib == "std/enc" && name == "bin",
        (Val::Typed { lib, .. }, Sch::Double) => lib == "std/num",
        (Val::Typed { lib, .. }, Sch::Time(_)) => lib == "std/time",
        (Val::Typed { lib, name, .. }, Sch::Str) => {
            !(lib == "std/enc" && name == "bin") && lib != "std/num" && {
                lib != "std/time"
                    || !matches!(
                        name.as_str(),
                        "date" | "time" | "datetime" | "localdatetime"
                    )
            }
        }
        (Val::Arr(_), Sch::Array(_)) => true,
        (Val::Obj(_), Sch::Record(..)) => true,
        _ => false,
    }
}

// ---------------------------------------------------------- time tokens

/// A decoded time logical value → (`std/time` type name, token).
fn time_token(tl: Tl, v: i64) -> Result<(&'static str, String), PipelineError> {
    Ok(match tl {
        Tl::Date => ("date", date_string(v)),
        Tl::TimeMillis | Tl::TimeMicros => {
            let us = if matches!(tl, Tl::TimeMillis) {
                v.checked_mul(1000)
            } else {
                Some(v)
            }
            .filter(|us| (0..86_400_000_000).contains(us))
            .ok_or_else(|| err(format!("time-of-day out of range: {v}")))?;
            ("time", time_string(us as u64))
        }
        Tl::TsMillis | Tl::TsMicros | Tl::LocalTsMillis | Tl::LocalTsMicros => {
            let us = if matches!(tl, Tl::TsMillis | Tl::LocalTsMillis) {
                v.checked_mul(1000)
                    .ok_or_else(|| err(format!("timestamp out of range: {v}")))?
            } else {
                v
            };
            let days = us.div_euclid(86_400_000_000);
            let tod = us.rem_euclid(86_400_000_000) as u64;
            let local = matches!(tl, Tl::LocalTsMillis | Tl::LocalTsMicros);
            let text = format!(
                "{}T{}{}",
                date_string(days),
                time_string(tod),
                if local { "" } else { "Z" }
            );
            (if local { "localdatetime" } else { "datetime" }, text)
        }
    })
}

/// A `std/time` token → the wire value of the given logical type.
fn time_value(tl: Tl, name: &str, text: &str) -> Result<i64, PipelineError> {
    let us = match (tl, name) {
        (Tl::Date, "date") => return parse_date(text),
        (Tl::TimeMillis | Tl::TimeMicros, "time") => parse_time(text)? as i64,
        (Tl::TsMillis | Tl::TsMicros, "datetime") => parse_datetime(text, true)?,
        (Tl::LocalTsMillis | Tl::LocalTsMicros, "localdatetime") => parse_datetime(text, false)?,
        _ => {
            return Err(err(format!(
                "std/time/{name} does not fit this Avro time logical type"
            )))
        }
    };
    if matches!(tl, Tl::TimeMillis | Tl::TsMillis | Tl::LocalTsMillis) {
        if us % 1000 != 0 {
            return Err(err(format!(
                "sub-millisecond value in a millis logical type: {text}"
            )));
        }
        Ok(us / 1000)
    } else {
        Ok(us)
    }
}

fn date_string(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn time_string(us: u64) -> String {
    let (h, rem) = (us / 3_600_000_000, us % 3_600_000_000);
    let (mi, rem) = (rem / 60_000_000, rem % 60_000_000);
    let (s, frac) = (rem / 1_000_000, rem % 1_000_000);
    let f = if frac == 0 {
        String::new()
    } else if frac % 1000 == 0 {
        format!(".{:03}", frac / 1000)
    } else {
        format!(".{frac:06}")
    };
    format!("{h:02}:{mi:02}:{s:02}{f}")
}

/// `YYYY-MM-DD` → days since the epoch; the round trip through
/// `civil_from_days` validates month lengths and leap days.
fn parse_date(s: &str) -> Result<i64, PipelineError> {
    let bad = || err(format!("invalid date token: {s}"));
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return Err(bad());
    }
    let y: i64 = s[..4].parse().map_err(|_| bad())?;
    let m: u32 = s[5..7].parse().map_err(|_| bad())?;
    let d: u32 = s[8..10].parse().map_err(|_| bad())?;
    let days = days_from_civil(y, m, d);
    if civil_from_days(days) != (y, m, d) {
        return Err(bad());
    }
    Ok(days)
}

/// `HH:MM:SS[.frac]` → microseconds of day.
fn parse_time(s: &str) -> Result<u64, PipelineError> {
    let bad = || err(format!("invalid time token: {s}"));
    let (hms, frac) = match s.find('.') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };
    let b = hms.as_bytes();
    if b.len() != 8 || b[2] != b':' || b[5] != b':' {
        return Err(bad());
    }
    let h: u64 = hms[..2].parse().map_err(|_| bad())?;
    let mi: u64 = hms[3..5].parse().map_err(|_| bad())?;
    let sec: u64 = hms[6..8].parse().map_err(|_| bad())?;
    if h > 23 || mi > 59 || sec > 59 {
        return Err(bad());
    }
    let mut us = 0u64;
    if !frac.is_empty() {
        if !frac.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        if frac.len() > 6 && frac[6..].bytes().any(|c| c != b'0') {
            return Err(err(format!(
                "sub-microsecond fraction cannot ride an Avro time logical type: {s}"
            )));
        }
        let head = &frac[..frac.len().min(6)];
        us = head.parse::<u64>().map_err(|_| bad())? * 10u64.pow(6 - head.len() as u32);
    }
    Ok((h * 3600 + mi * 60 + sec) * 1_000_000 + us)
}

/// `date T time [Z|±HH:MM]` → epoch microseconds (UTC when the offset
/// applies; the offset is required exactly when `with_offset`).
fn parse_datetime(s: &str, with_offset: bool) -> Result<i64, PipelineError> {
    let bad = || err(format!("invalid datetime token: {s}"));
    if s.len() < 11 || !s.is_char_boundary(10) {
        return Err(bad());
    }
    let (date, rest) = s.split_at(10);
    if !matches!(rest.as_bytes()[0], b'T' | b't' | b' ') {
        return Err(bad());
    }
    let rest = &rest[1..];
    let (time_part, offset_us) = if !with_offset {
        (rest, 0)
    } else if let Some(t) = rest.strip_suffix(['Z', 'z']) {
        (t, 0)
    } else {
        let n = rest.len();
        if n < 6 || !rest.is_char_boundary(n - 6) {
            return Err(bad());
        }
        let (t, off) = rest.split_at(n - 6);
        let b = off.as_bytes();
        if !matches!(b[0], b'+' | b'-') || b[3] != b':' {
            return Err(bad());
        }
        let oh: i64 = off[1..3].parse().map_err(|_| bad())?;
        let om: i64 = off[4..6].parse().map_err(|_| bad())?;
        if oh > 23 || om > 59 {
            return Err(bad());
        }
        let sign = if b[0] == b'-' { -1 } else { 1 };
        (t, sign * (oh * 3600 + om * 60) * 1_000_000)
    };
    let days = parse_date(date)?;
    let tod = parse_time(time_part)? as i64;
    Ok(days * 86_400_000_000 + tod - offset_us)
}

/// Days since 1970-01-01 → (year, month, day) — Hinnant's
/// `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// (year, month, day) → days since 1970-01-01 — Hinnant's
/// `days_from_civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// -------------------------------------------------------------- inflate

/// Raw DEFLATE (RFC 1951) decompression — the Avro `deflate` codec
/// has no zlib wrapper and no checksum. Decode-only: export always
/// writes the null codec.
fn inflate(data: &[u8]) -> Result<Vec<u8>, PipelineError> {
    let mut s = Inf {
        b: data,
        pos: 0,
        bit: 0,
        out: Vec::new(),
    };
    loop {
        let last = s.bits(1)?;
        match s.bits(2)? {
            0 => s.stored()?,
            1 => {
                let (lit, dist) = fixed_tables();
                s.codes(&lit, &dist)?;
            }
            2 => {
                let (lit, dist) = s.dynamic_tables()?;
                s.codes(&lit, &dist)?;
            }
            _ => return Err(err("invalid DEFLATE block type")),
        }
        if last == 1 {
            return Ok(s.out);
        }
    }
}

struct Inf<'a> {
    b: &'a [u8],
    pos: usize,
    bit: u32,
    out: Vec<u8>,
}

impl Inf<'_> {
    /// `n` bits, LSB first.
    fn bits(&mut self, n: u32) -> Result<u32, PipelineError> {
        let mut v = 0u32;
        for i in 0..n {
            let byte = *self
                .b
                .get(self.pos)
                .ok_or_else(|| err("truncated DEFLATE stream"))?;
            v |= u32::from((byte >> self.bit) & 1) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Ok(v)
    }

    fn stored(&mut self) -> Result<(), PipelineError> {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
        if self.pos + 4 > self.b.len() {
            return Err(err("truncated DEFLATE stream"));
        }
        let len = u16::from_le_bytes([self.b[self.pos], self.b[self.pos + 1]]);
        let nlen = u16::from_le_bytes([self.b[self.pos + 2], self.b[self.pos + 3]]);
        if nlen != !len {
            return Err(err("stored DEFLATE block length check failed"));
        }
        self.pos += 4;
        let end = self.pos + len as usize;
        if end > self.b.len() {
            return Err(err("truncated DEFLATE stream"));
        }
        self.out.extend_from_slice(&self.b[self.pos..end]);
        self.pos = end;
        Ok(())
    }

    /// Canonical-Huffman decode, one bit at a time (Mark Adler's
    /// `puff` shape — compact and clearly correct).
    fn decode(&mut self, h: &Huff) -> Result<u16, PipelineError> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= self.bits(1)? as i32;
            let count = i32::from(h.count[len]);
            if code - first < count {
                return Ok(h.symbol[(index + (code - first)) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(err("invalid Huffman code"))
    }

    fn dynamic_tables(&mut self) -> Result<(Huff, Huff), PipelineError> {
        const ORDER: [usize; 19] = [
            16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
        ];
        let hlit = self.bits(5)? as usize + 257;
        let hdist = self.bits(5)? as usize + 1;
        let hclen = self.bits(4)? as usize + 4;
        let mut cl = [0u8; 19];
        for &o in ORDER.iter().take(hclen) {
            cl[o] = self.bits(3)? as u8;
        }
        let clh = Huff::new(&cl);
        let mut lengths = vec![0u8; hlit + hdist];
        let mut i = 0;
        while i < lengths.len() {
            let sym = self.decode(&clh)?;
            let (value, rep) = match sym {
                0..=15 => (sym as u8, 1),
                16 => {
                    if i == 0 {
                        return Err(err("DEFLATE repeat with no previous length"));
                    }
                    (lengths[i - 1], 3 + self.bits(2)? as usize)
                }
                17 => (0, 3 + self.bits(3)? as usize),
                _ => (0, 11 + self.bits(7)? as usize),
            };
            if i + rep > lengths.len() {
                return Err(err("DEFLATE code lengths overflow their table"));
            }
            lengths[i..i + rep].fill(value);
            i += rep;
        }
        Ok((Huff::new(&lengths[..hlit]), Huff::new(&lengths[hlit..])))
    }

    fn codes(&mut self, lit: &Huff, dist: &Huff) -> Result<(), PipelineError> {
        const LEN_BASE: [u16; 29] = [
            3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99,
            115, 131, 163, 195, 227, 258,
        ];
        const LEN_EXTRA: [u32; 29] = [
            0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
        ];
        const DIST_BASE: [u16; 30] = [
            1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025,
            1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
        ];
        const DIST_EXTRA: [u32; 30] = [
            0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12,
            12, 13, 13,
        ];
        loop {
            let sym = self.decode(lit)?;
            match sym {
                0..=255 => self.out.push(sym as u8),
                256 => return Ok(()),
                257..=285 => {
                    let i = (sym - 257) as usize;
                    let len = LEN_BASE[i] as usize + self.bits(LEN_EXTRA[i])? as usize;
                    let dsym = self.decode(dist)? as usize;
                    if dsym >= 30 {
                        return Err(err("invalid DEFLATE distance code"));
                    }
                    let d = DIST_BASE[dsym] as usize + self.bits(DIST_EXTRA[dsym])? as usize;
                    if d > self.out.len() {
                        return Err(err("DEFLATE distance reaches before the output start"));
                    }
                    let start = self.out.len() - d;
                    for j in 0..len {
                        let byte = self.out[start + j];
                        self.out.push(byte);
                    }
                }
                _ => return Err(err("invalid DEFLATE literal/length code")),
            }
        }
    }
}

/// The fixed literal/length and distance tables (RFC 1951 §3.2.6).
fn fixed_tables() -> (Huff, Huff) {
    let mut lit = [0u8; 288];
    lit[..144].fill(8);
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    lit[280..].fill(8);
    (Huff::new(&lit), Huff::new(&[5u8; 30]))
}

/// Canonical Huffman decoding table: symbol counts per code length
/// plus the symbols sorted by (length, symbol).
struct Huff {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Huff {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[(l & 15) as usize] += 1;
        }
        count[0] = 0;
        let mut offs = [0u16; 16];
        for len in 1..16 {
            offs[len] = offs[len - 1] + count[len - 1];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Huff { count, symbol }
    }
}

fn avro_name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

fn wlong(v: i64, out: &mut Vec<u8>) {
    let mut z = ((v << 1) ^ (v >> 63)) as u64;
    loop {
        if z < 0x80 {
            out.push(z as u8);
            return;
        }
        out.push((z & 0x7f) as u8 | 0x80);
        z >>= 7;
    }
}

fn wbytes(b: &[u8], out: &mut Vec<u8>) {
    wlong(b.len() as i64, out);
    out.extend_from_slice(b);
}

// ------------------------------------------------------ schema convert

/// Avro Schema (`.avsc` JSON) → authored `.saiv`, a sound weakening:
/// every emitted constraint is implied by the source, and what kaiv
/// cannot express drops with a `//` comment. The root must be a
/// record. int/long carry their exact wire ranges; float/double emit
/// the extended-reals union (Avro floats admit non-finite values);
/// bytes/fixed ride `&bin` (the fixed size is noted, not enforced);
/// enums are closed, so they emit `!str{…}` without the int
/// alternative; the decimal logical type emits `!int` (scale 0) or
/// `!float`; the time logical types ride `std/time`. Record fields
/// are always present on the wire, so fields are required — a union
/// with null keeps the field required and adds the `null`
/// alternative. Nested records become namespaces, arrays vectors or
/// element blocks, maps typed maps; a union with a non-scalar branch
/// has no kaiv spelling and the field is omitted with a note.
/// The contract holds for flat strings: see the `jsonschema`
/// module doc for the shared `std/enc/json` embed-channel
/// limitation.
pub fn import_schema(input: &[u8], name: &str) -> Result<String, PipelineError> {
    let text = std::str::from_utf8(input).map_err(|_| err("input is not valid UTF-8"))?;
    let sch = parse_schema(&json::parse_val(text)?, &mut Vec::new())?;
    let Sch::Record(_, fields) = sch else {
        return Err(err("the root Avro schema must be a record"));
    };
    let mut ctx = SchemaCtx {
        body: String::new(),
        imports: std::collections::BTreeSet::new(),
    };
    ctx.record_fields(&fields, "", 0)?;
    let mut out = format!(".!kaivschema 1 {name}\n");
    for lib in &ctx.imports {
        out.push_str(&format!(".!types {lib}\n"));
    }
    out.push('\n');
    out.push_str(&ctx.body);
    Ok(out)
}

struct SchemaCtx {
    body: String,
    imports: std::collections::BTreeSet<&'static str>,
}

impl SchemaCtx {
    fn note(&mut self, msg: &str) {
        self.body.push_str(&format!("// dropped: {msg}\n"));
    }

    /// The union-member spelling of a scalar schema (without the
    /// leading `!`), or None when the branch has no scalar spelling.
    fn member(&mut self, sch: &Sch) -> Option<String> {
        Some(match sch {
            Sch::Null => "null".to_string(),
            Sch::Bool => "bool".to_string(),
            Sch::Int => "int[-2147483648,2147483647]".to_string(),
            Sch::Long => "int[-9223372036854775808,9223372036854775807]".to_string(),
            Sch::Float | Sch::Double => {
                self.imports.insert("std/num");
                "float|std/num/inf|std/num/nan".to_string()
            }
            Sch::Bytes | Sch::Fixed(_) => {
                self.imports.insert("std/enc");
                "std/enc/bin".to_string()
            }
            Sch::Str => "str".to_string(),
            Sch::Decimal { scale: 0, .. } => "int".to_string(),
            Sch::Decimal { .. } => "float".to_string(),
            Sch::Enum(syms) if !syms.is_empty() => format!("str{{{}}}", syms.join(",")),
            Sch::Enum(_) => "str".to_string(),
            Sch::Time(tl) => {
                self.imports.insert("std/time");
                match tl {
                    Tl::Date => "std/time/date",
                    Tl::TimeMillis | Tl::TimeMicros => "std/time/time",
                    Tl::TsMillis | Tl::TsMicros => "std/time/datetime",
                    Tl::LocalTsMillis | Tl::LocalTsMicros => "std/time/localdatetime",
                }
                .to_string()
            }
            Sch::Record(..) | Sch::Array(_) | Sch::Map(_) | Sch::Union(_) => return None,
        })
    }

    /// The annotation line for a scalar field, or None for a plain
    /// string. `&name` is preferred over the qualified spelling when
    /// the type stands alone.
    fn annotation(&mut self, sch: &Sch) -> Result<Option<String>, PipelineError> {
        Ok(match sch {
            Sch::Str => None,
            Sch::Bytes | Sch::Fixed(_) => {
                if let Sch::Fixed(n) = sch {
                    self.note(&format!("fixed size {n} (length is not enforced)"));
                }
                self.imports.insert("std/enc");
                Some("&bin".to_string())
            }
            Sch::Time(_) => {
                let m = self.member(sch).expect("time is scalar");
                Some(format!("&{}", m.rsplit('/').next().expect("qualified")))
            }
            Sch::Union(branches) => {
                let mut members = Vec::with_capacity(branches.len());
                for b in branches {
                    match self.member(b) {
                        Some(m) => members.push(m),
                        None => return Err(err("non-scalar union branch")),
                    }
                }
                Some(format!("!{}", members.join("|")))
            }
            other => Some(format!("!{}", self.member(other).expect("scalar"))),
        })
    }

    fn record_fields(
        &mut self,
        fields: &[(String, Sch)],
        path: &str,
        depth: usize,
    ) -> Result<(), PipelineError> {
        if depth > 32 {
            return Err(err("record nesting too deep"));
        }
        for (fname, fsch) in fields {
            let Ok(key) = crate::jsonschema::kaiv_key(fname) else {
                self.note(&format!("unrepresentable field name: {fname:?}"));
                continue;
            };
            match fsch {
                Sch::Record(_, inner) => {
                    self.record_fields(inner, &format!("{path}/{key}"), depth + 1)?;
                }
                Sch::Array(el) => match el.as_ref() {
                    Sch::Record(_, inner) => {
                        self.body.push_str(&format!("[{path}/@{key}]\n"));
                        for (iname, isch) in inner {
                            let Ok(ikey) = crate::jsonschema::kaiv_key(iname) else {
                                self.note(&format!("unrepresentable field name: {iname:?}"));
                                continue;
                            };
                            match self.annotation(isch) {
                                Ok(anno) => {
                                    if let Some(a) = anno {
                                        self.body.push_str(&format!("{a}\n"));
                                    }
                                    self.body.push_str(&format!("{ikey}=\n"));
                                }
                                Err(_) => self.note(&format!(
                                    "non-scalar element field {iname} at {}",
                                    disp2(path, fname)
                                )),
                            }
                        }
                        self.body.push_str("[]\n");
                    }
                    el => match self.annotation(el) {
                        Ok(anno) => {
                            if let Some(a) = anno {
                                self.body.push_str(&format!("{a}\n"));
                            }
                            self.body.push_str(&format!("{path}/@{key};=\n"));
                        }
                        Err(_) => self.note(&format!(
                            "array element type at {} has no kaiv spelling",
                            disp2(path, fname)
                        )),
                    },
                },
                Sch::Map(v) => {
                    let core = match v.as_ref() {
                        Sch::Bool => "bool",
                        Sch::Str => "str",
                        Sch::Float | Sch::Double => "float",
                        Sch::Int | Sch::Long => "int",
                        _ => {
                            self.note(&format!(
                                "map value type at {} has no kaiv spelling",
                                disp2(path, fname)
                            ));
                            continue;
                        }
                    };
                    self.body
                        .push_str(&format!("!map<{core}>\n{}=\n", lhs(path, &key)));
                }
                scalar => match self.annotation(scalar) {
                    Ok(anno) => {
                        if let Some(a) = anno {
                            self.body.push_str(&format!("{a}\n"));
                        }
                        // Record fields are always present on the wire.
                        self.body.push_str(&format!("{}=\n", lhs(path, &key)));
                    }
                    Err(_) => self.note(&format!(
                        "union with a non-scalar branch at {}",
                        disp2(path, fname)
                    )),
                },
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

    #[test]
    fn parse_datetime_rejects_multibyte_without_panic() {
        assert!(parse_datetime("aéééééXXXX", true).is_err());
        assert!(parse_datetime("2026-07-05T10:00:00é:00", true).is_err());
    }

    /// A null-codec OCF around already-encoded record payloads.
    fn ocf(schema: &str, records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::from(MAGIC);
        wlong(2, &mut out);
        wbytes(b"avro.schema", &mut out);
        wbytes(schema.as_bytes(), &mut out);
        wbytes(b"avro.codec", &mut out);
        wbytes(b"null", &mut out);
        wlong(0, &mut out);
        out.extend_from_slice(SYNC);
        let payload: Vec<u8> = records.concat();
        wlong(records.len() as i64, &mut out);
        wlong(payload.len() as i64, &mut out);
        out.extend_from_slice(&payload);
        out.extend_from_slice(SYNC);
        out
    }

    fn build(authored: &str) -> String {
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        crate::denorm::denormalize(&raiv).unwrap()
    }

    fn roundtrip(src: &[u8]) -> Vec<u8> {
        export(&build(&import(src).unwrap())).unwrap()
    }

    #[test]
    fn import_typing_and_natives() {
        let schema = r#"{"type":"record","name":"r","fields":[
            {"name":"host","type":"string"},
            {"name":"port","type":"long"},
            {"name":"ratio","type":"double"},
            {"name":"on","type":"boolean"},
            {"name":"note","type":["null","string"]}]}"#;
        let mut rec = Vec::new();
        wbytes(b"web01", &mut rec);
        wlong(8080, &mut rec);
        rec.extend(1.5f64.to_le_bytes());
        rec.push(1);
        wlong(0, &mut rec); // union: null branch
        let src = ocf(schema, &[rec]);
        let out = import(&src).unwrap();
        assert!(out.contains("host=web01\n"));
        assert!(out.contains("!int\nport=8080\n"));
        assert!(out.contains("!float\nratio=1.5\n"));
        assert!(out.contains("!bool\non=true\n"));
        assert!(out.contains("!null\nnote=\n"));
        // Idempotence: export(import(x)) is a fixed point.
        let once = roundtrip(&src);
        assert_eq!(roundtrip(&once), once);
    }

    #[test]
    fn bytes_enum_fixed_and_map() {
        let schema = r#"{"type":"record","name":"r","fields":[
            {"name":"blob","type":"bytes"},
            {"name":"level","type":{"type":"enum","name":"lvl","symbols":["low","mid","high"]}},
            {"name":"mac","type":{"type":"fixed","name":"m6","size":3}},
            {"name":"limits","type":{"type":"map","values":"long"}}]}"#;
        let mut rec = Vec::new();
        wbytes(&[0x00, 0xff, 0x10], &mut rec);
        wlong(2, &mut rec); // "high"
        rec.extend([0xde, 0xad, 0xbe]);
        wlong(2, &mut rec); // map block of 2
        wbytes(b"rps", &mut rec);
        wlong(500, &mut rec);
        wbytes(b"burst", &mut rec);
        wlong(900, &mut rec);
        wlong(0, &mut rec);
        let src = ocf(schema, &[rec]);
        let out = import(&src).unwrap();
        assert!(out.contains(&format!(
            "&bin\nblob={}\n",
            json::b64url_encode(&[0x00, 0xff, 0x10])
        )));
        assert!(out.contains("level=high\n"));
        assert!(out.contains(&format!(
            "&bin\nmac={}\n",
            json::b64url_encode(&[0xde, 0xad, 0xbe])
        )));
        assert!(out.contains("!int\n/limits:=rps=500|burst=900\n"));
    }

    #[test]
    fn decimals_are_exact_tokens() {
        let schema = r#"{"type":"record","name":"r","fields":[
            {"name":"price","type":{"type":"bytes","logicalType":"decimal","precision":9,"scale":2}},
            {"name":"debt","type":{"type":"fixed","name":"d2","size":2,"logicalType":"decimal","precision":4,"scale":2}},
            {"name":"tiny","type":{"type":"bytes","logicalType":"decimal","precision":3,"scale":3}}]}"#;
        let mut rec = Vec::new();
        wbytes(&[0x30, 0x39], &mut rec); // 12345, scale 2
        rec.extend([0xff, 0x85]); // -123, scale 2 (fixed 2)
        wbytes(&[0x07], &mut rec); // 7, scale 3
        let src = ocf(schema, &[rec]);
        let out = import(&src).unwrap();
        assert!(out.contains("!float\nprice=123.45\n"));
        assert!(out.contains("!float\ndebt=-1.23\n"));
        assert!(out.contains("!float\ntiny=0.007\n"));
    }

    #[test]
    fn big_integers_export_as_decimal() {
        let authored =
            crate::json::import(br#"{"big":18446744073709551616,"neg":-18446744073709551616}"#)
                .unwrap();
        let bytes = export(&build(&authored)).unwrap();
        let sjson = String::from_utf8_lossy(&bytes);
        assert!(sjson.contains("\"logicalType\":\"decimal\",\"precision\":20,\"scale\":0"));
        let back = import(&bytes).unwrap();
        assert!(back.contains("!int\nbig=18446744073709551616\n"));
        assert!(back.contains("!int\nneg=-18446744073709551616\n"));
    }

    #[test]
    fn high_precision_fractionals_export_as_decimal() {
        // 22 significant digits: silently lossy as double before; now
        // an exact decimal, mirroring the big-integer precedent.
        let daiv = ".!kaiv 1\n!float'::amount=12345678901234567890.12\n";
        let bytes = export(daiv).unwrap();
        let sjson = String::from_utf8_lossy(&bytes);
        assert!(
            sjson.contains("\"logicalType\":\"decimal\",\"precision\":22,\"scale\":2"),
            "{sjson}"
        );
        let back = import(&bytes).unwrap();
        assert!(back.contains("amount=12345678901234567890.12\n"), "{back}");
    }

    #[test]
    fn double_safe_fractionals_stay_double() {
        let daiv = ".!kaiv 1\n!float'::pi=3.14159\n!float'::half=0.5\n";
        let bytes = export(daiv).unwrap();
        let sjson = String::from_utf8_lossy(&bytes);
        assert!(!sjson.contains("decimal"), "{sjson}");
        let back = import(&bytes).unwrap();
        assert!(back.contains("pi=3.14159\n"), "{back}");
        assert!(back.contains("half=0.5\n"), "{back}");
    }

    #[test]
    fn mixed_scale_decimal_field_unifies() {
        // One heavy value lowers the whole array to decimal; lighter
        // values scale up exactly (value-exact, spelling-normalized).
        let daiv = ".!kaiv 1\n!float'/@xs::0=1.5\n!float'/@xs::1=12345678901234567890.12\n";
        let bytes = export(daiv).unwrap();
        let sjson = String::from_utf8_lossy(&bytes);
        assert!(
            sjson.contains("\"logicalType\":\"decimal\",\"precision\":22,\"scale\":2"),
            "{sjson}"
        );
        let back = import(&bytes).unwrap();
        assert!(back.contains("1.50"), "{back}");
        assert!(back.contains("12345678901234567890.12"), "{back}");
    }

    #[test]
    fn overflowing_exponent_exports_exactly() {
        // `1e400` saturates f64 to inf; as an exact decimal it is a
        // 401-digit integer — exported exactly instead of as inf.
        let daiv = ".!kaiv 1\n!float'::x=1e400\n";
        let bytes = export(daiv).unwrap();
        let back = import(&bytes).unwrap();
        let expected = format!("x=1{}\n", "0".repeat(400));
        assert!(back.contains(&expected), "{back}");
    }

    #[test]
    fn nonfinite_doubles_are_std_num() {
        let daiv = ".!kaiv 1\n!std/num/inf'::f=inf\n!std/num/inf'::g=-inf\n!std/num/nan'::n=nan\n";
        let bytes = export(daiv).unwrap();
        let out = import(&bytes).unwrap();
        assert!(out.contains("&inf\nf=inf\n"));
        assert!(out.contains("&inf\ng=-inf\n"));
        assert!(out.contains("&nan\nn=nan\n"));
    }

    #[test]
    fn multiple_records_wrap_in_records_array() {
        let schema = r#"{"type":"record","name":"r","fields":[{"name":"host","type":"string"}]}"#;
        let mut r1 = Vec::new();
        wbytes(b"a", &mut r1);
        let mut r2 = Vec::new();
        wbytes(b"b", &mut r2);
        let src = ocf(schema, &[r1, r2]);
        let out = import(&src).unwrap();
        assert!(out.contains("/@records+:=host=a\n"));
        assert!(out.contains("/@records+:=host=b\n"));
    }

    #[test]
    fn scalar_schema_wraps_too() {
        let mut r1 = Vec::new();
        wlong(7, &mut r1);
        let src = ocf("\"long\"", &[r1]);
        let out = import(&src).unwrap();
        assert!(out.contains("!int\n/@records;=7\n"));
    }

    #[test]
    fn export_unifies_ragged_records() {
        let authored = crate::json::import(br#"{"rows":[{"a":1},{"a":null,"b":"x"}]}"#).unwrap();
        let bytes = export(&build(&authored)).unwrap();
        let sjson = String::from_utf8_lossy(&bytes);
        // a: long that must also hold null; b: missing on one side.
        assert!(sjson.contains(r#"{"name":"a","type":["long","null"]}"#));
        assert!(sjson.contains(r#"{"name":"b","type":["string","null"]}"#));
        let back = import(&bytes).unwrap();
        assert!(back.contains("!int\n/@rows/0::a=1\n"));
        assert!(back.contains("!null\n/@rows/1::a=\n"));
        assert!(back.contains("/@rows/1::b=x\n"));
        // The first row's missing b materializes as null.
        assert!(back.contains("!null\n/@rows/0::b=\n"));
    }

    #[test]
    fn cross_format_and_idempotence() {
        let authored = crate::json::import(
            br#"{"name":"eu1","port":8443,"ratio":2.5,"tags":["a","b"],"limits":{"rps":500},"servers":[{"host":"a","port":1},{"host":"b","port":2}]}"#,
        )
        .unwrap();
        let daiv = build(&authored);
        let bytes = export(&daiv).unwrap();
        let daiv2 = build(&import(&bytes).unwrap());
        let json2 = crate::json::export(&daiv2).unwrap();
        assert_eq!(
            json2.trim_end(),
            r#"{"name":"eu1","port":8443,"ratio":2.5,"tags":["a","b"],"limits":{"rps":500},"servers":[{"host":"a","port":1},{"host":"b","port":2}]}"#
        );
        assert_eq!(export(&daiv2).unwrap(), bytes);
    }

    #[test]
    fn schema_convert_core_mapping() {
        let avsc = br#"{"type":"record","name":"event","fields":[
            {"name":"id","type":"long"},
            {"name":"seq","type":"int"},
            {"name":"host","type":"string"},
            {"name":"on","type":"boolean"},
            {"name":"payload","type":"bytes"},
            {"name":"ratio","type":"double"},
            {"name":"level","type":{"type":"enum","name":"lvl","symbols":["low","high"]}},
            {"name":"mac","type":{"type":"fixed","name":"m3","size":3}},
            {"name":"price","type":{"type":"bytes","logicalType":"decimal","precision":9,"scale":2}},
            {"name":"day","type":{"type":"int","logicalType":"date"}},
            {"name":"when","type":{"type":"long","logicalType":"timestamp-micros"}},
            {"name":"note","type":["null","string"]},
            {"name":"tags","type":{"type":"array","items":"string"}},
            {"name":"rows","type":{"type":"array","items":{"type":"record","name":"row","fields":[{"name":"a","type":"long"}]}}},
            {"name":"attrs","type":{"type":"map","values":"long"}},
            {"name":"limits","type":{"type":"record","name":"lim","fields":[{"name":"rps","type":"long"}]}},
            {"name":"blob","type":["null",{"type":"array","items":"string"}]}
        ]}"#;
        let saiv = import_schema(avsc, "acme/event").unwrap();
        assert!(saiv.starts_with(".!kaivschema 1 acme/event\n"));
        assert!(saiv.contains("!int[-9223372036854775808,9223372036854775807]\nid=\n"));
        assert!(saiv.contains("!int[-2147483648,2147483647]\nseq=\n"));
        assert!(saiv.contains("host=\n"));
        assert!(saiv.contains("!bool\non=\n"));
        assert!(saiv.contains("&bin\npayload=\n"));
        assert!(saiv.contains("!float|std/num/inf|std/num/nan\nratio=\n"));
        assert!(saiv.contains("!str{low,high}\nlevel=\n"));
        assert!(saiv.contains("// dropped: fixed size 3"));
        assert!(saiv.contains("&bin\nmac=\n"));
        assert!(saiv.contains("!float\nprice=\n"));
        assert!(saiv.contains("&date\nday=\n"));
        assert!(saiv.contains("&datetime\nwhen=\n"));
        assert!(saiv.contains("!null|str\nnote=\n"));
        assert!(saiv.contains("/@tags;=\n"));
        assert!(saiv.contains("[/@rows]\n!int[-9223372036854775808,9223372036854775807]\na=\n[]\n"));
        assert!(saiv.contains("!map<int>\nattrs=\n"));
        assert!(saiv.contains("!int[-9223372036854775808,9223372036854775807]\n/limits::rps=\n"));
        assert!(saiv.contains("// dropped: union with a non-scalar branch at root/blob"));
        // The schema compiles, and a document decoded from the wire
        // by the DATA converter validates against it.
        let csaiv = crate::compile_schema(saiv.as_bytes()).unwrap();
        let sc = crate::parse_csaiv(&csaiv).unwrap();
        let mut rec = Vec::new();
        wlong(1, &mut rec); // id
        wlong(2, &mut rec); // seq
        wbytes(b"h", &mut rec); // host
        rec.push(1); // on
        wbytes(&[0x01], &mut rec); // payload
        rec.extend(2.5f64.to_le_bytes()); // ratio
        wlong(1, &mut rec); // level = high
        rec.extend([0xde, 0xad, 0xbe]); // mac
        wbytes(&[0x30, 0x39], &mut rec); // price = 123.45
        wlong(20639, &mut rec); // day
        wlong(1_783_245_600_000_000, &mut rec); // when
        wlong(0, &mut rec); // note: null branch
        wlong(1, &mut rec); // tags block
        wbytes(b"x", &mut rec);
        wlong(0, &mut rec);
        wlong(1, &mut rec); // rows block
        wlong(7, &mut rec); // row { a: 7 }
        wlong(0, &mut rec);
        wlong(1, &mut rec); // attrs block
        wbytes(b"k", &mut rec);
        wlong(9, &mut rec);
        wlong(0, &mut rec);
        wlong(5, &mut rec); // limits.rps
        wlong(0, &mut rec); // blob: null branch
        let schema_json = std::str::from_utf8(avsc).unwrap();
        let daiv = build(&import(&ocf(schema_json, &[rec])).unwrap());
        assert_eq!(crate::validate(&daiv, &sc), Ok(()));
        // A bad enum symbol breaks validity (fields are required, so
        // the wire-shaped document is the reference; flip one value).
        let bad = daiv.replace("level=high", "level=nope");
        assert_eq!(
            crate::validate(&bad, &sc),
            Err(crate::AppError::ConstraintViolation)
        );
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(import(b"NotAvro").is_err());
        // Unsupported codec.
        let mut out = Vec::from(MAGIC);
        wlong(2, &mut out);
        wbytes(b"avro.schema", &mut out);
        wbytes(b"\"long\"", &mut out);
        wbytes(b"avro.codec", &mut out);
        wbytes(b"snappy", &mut out);
        wlong(0, &mut out);
        out.extend_from_slice(SYNC);
        assert!(import(&out).unwrap_err().to_string().contains("codec"));
        // Recursive schema.
        let rec = r#"{"type":"record","name":"n","fields":[{"name":"next","type":["null","n"]}]}"#;
        let src = ocf(rec, &[vec![0]]);
        assert!(import(&src).unwrap_err().to_string().contains("unresolved"));
        // Union index out of range.
        let schema =
            r#"{"type":"record","name":"r","fields":[{"name":"x","type":["null","long"]}]}"#;
        let mut r1 = Vec::new();
        wlong(5, &mut r1);
        assert!(import(&ocf(schema, &[r1])).is_err());
        // Sync marker mismatch.
        let mut r1 = Vec::new();
        wbytes(b"a", &mut r1);
        let mut src = ocf(
            r#"{"type":"record","name":"r","fields":[{"name":"h","type":"string"}]}"#,
            &[r1],
        );
        let n = src.len();
        src[n - 1] ^= 0xff;
        assert!(import(&src).unwrap_err().to_string().contains("sync"));
    }

    #[test]
    fn inflate_stored_and_dynamic() {
        // Single stored block.
        let mut src = vec![0x01, 0x03, 0x00, 0xfc, 0xff];
        src.extend_from_slice(b"abc");
        assert_eq!(inflate(&src).unwrap(), b"abc");
        // zlib -9 raw stream (dynamic Huffman + LZ77 back-references).
        let deflated: [u8; 30] = [
            203, 78, 204, 44, 83, 72, 73, 77, 203, 73, 44, 73, 85, 40, 75, 77, 46, 201, 47, 178,
            82, 72, 76, 74, 38, 136, 24, 254, 3, 0,
        ];
        let payload: Vec<u8> = [
            &b"kaiv deflate vector: "[..],
            &b"abc".repeat(12),
            &[0x00, 0xff],
        ]
        .concat();
        assert_eq!(inflate(&deflated).unwrap(), payload);
        assert!(inflate(&[0x07]).is_err()); // block type 3
        assert!(inflate(&[0x01, 0x03, 0x00, 0x00, 0x00, b'a']).is_err()); // bad nlen
    }

    #[test]
    fn deflate_codec_imports() {
        let schema = r#"{"type":"record","name":"r","fields":[{"name":"host","type":"string"}]}"#;
        let mut rec = Vec::new();
        wbytes(b"web01", &mut rec);
        // Wrap the block payload in a stored DEFLATE block.
        let mut deflated = vec![0x01];
        deflated.extend((rec.len() as u16).to_le_bytes());
        deflated.extend((!(rec.len() as u16)).to_le_bytes());
        deflated.extend_from_slice(&rec);
        let mut src = Vec::from(MAGIC);
        wlong(2, &mut src);
        wbytes(b"avro.schema", &mut src);
        wbytes(schema.as_bytes(), &mut src);
        wbytes(b"avro.codec", &mut src);
        wbytes(b"deflate", &mut src);
        wlong(0, &mut src);
        src.extend_from_slice(SYNC);
        wlong(1, &mut src);
        wlong(deflated.len() as i64, &mut src);
        src.extend_from_slice(&deflated);
        src.extend_from_slice(SYNC);
        let out = import(&src).unwrap();
        assert!(out.contains("host=web01\n"));
    }

    #[test]
    fn time_logical_types_are_std_time() {
        let schema = r#"{"type":"record","name":"r","fields":[
            {"name":"day","type":{"type":"int","logicalType":"date"}},
            {"name":"at","type":{"type":"long","logicalType":"time-micros"}},
            {"name":"tick","type":{"type":"int","logicalType":"time-millis"}},
            {"name":"when","type":{"type":"long","logicalType":"timestamp-micros"}},
            {"name":"seen","type":{"type":"long","logicalType":"timestamp-millis"}},
            {"name":"local","type":{"type":"long","logicalType":"local-timestamp-micros"}},
            {"name":"eve","type":{"type":"int","logicalType":"date"}}]}"#;
        let mut rec = Vec::new();
        wlong(20639, &mut rec); // 2026-07-05
        wlong(37_800_250_000, &mut rec); // 10:30:00.25
        wlong(3_723_004, &mut rec); // 01:02:03.004
        wlong(1_783_245_600_000_000, &mut rec); // 2026-07-05T10:00:00Z
        wlong(1_783_245_600_500, &mut rec); // …T10:00:00.5Z in millis
        wlong(1_783_245_600_000_000, &mut rec);
        wlong(-1, &mut rec); // 1969-12-31
        let src = ocf(schema, &[rec]);
        let out = import(&src).unwrap();
        assert!(out.contains(".!types std/time\n"));
        assert!(out.contains("&date\nday=2026-07-05\n"));
        assert!(out.contains("&time\nat=10:30:00.250\n"));
        assert!(out.contains("&time\ntick=01:02:03.004\n"));
        assert!(out.contains("&datetime\nwhen=2026-07-05T10:00:00Z\n"));
        assert!(out.contains("&datetime\nseen=2026-07-05T10:00:00.500Z\n"));
        assert!(out.contains("&localdatetime\nlocal=2026-07-05T10:00:00\n"));
        assert!(out.contains("&date\neve=1969-12-31\n"));
        // Millis inputs re-export at micros; the round trip is then a
        // fixed point.
        let once = roundtrip(&src);
        let sjson = String::from_utf8_lossy(&once);
        assert!(sjson.contains("\"logicalType\":\"timestamp-micros\""));
        assert!(roundtrip(&once) == once);
    }

    #[cfg(feature = "toml")]
    #[test]
    fn cross_format_toml_datetimes() {
        // TOML datetimes ride std/time into Avro time logical types;
        // offsets normalize to UTC with the instant preserved.
        let authored = crate::toml::import(
            b"when = 2026-07-05T12:00:00+02:00\nday = 2026-07-05\nat = 10:30:00.25\n",
        )
        .unwrap();
        let bytes = export(&build(&authored)).unwrap();
        let back = import(&bytes).unwrap();
        assert!(back.contains("&datetime\nwhen=2026-07-05T10:00:00Z\n"));
        assert!(back.contains("&date\nday=2026-07-05\n"));
        assert!(back.contains("&time\nat=10:30:00.250\n"));
    }

    #[cfg(feature = "toml")]
    #[test]
    fn mixed_time_kinds_cannot_unify() {
        let authored = crate::toml::import(b"xs = [2026-07-05, 2026-07-05T10:00:00Z]\n").unwrap();
        assert!(export(&build(&authored))
            .unwrap_err()
            .to_string()
            .contains("time logical types"));
    }

    #[test]
    fn rejects_unexportable_shapes() {
        // Invalid Avro field name.
        let daiv = ".!kaiv 1\n!str'::\"@x\"=1\n";
        assert!(export(daiv).unwrap_err().to_string().contains("field name"));
        // A non-finite marker cannot join an exact decimal.
        let daiv = ".!kaiv 1\n!std/num/nan'/@xs::0=nan\n!float'/@xs::1=1e400\n";
        assert!(export(daiv).unwrap_err().to_string().contains("non-finite"));
    }

    #[test]
    fn float_and_big_integer_unify_as_exact_decimal() {
        // Formerly a hard error ("no exact superset"): a double-safe
        // float and a beyond-i64 integer now share a decimal field.
        let authored = crate::json::import(br#"{"xs":[1.5,18446744073709551616]}"#).unwrap();
        let bytes = export(&build(&authored)).unwrap();
        let back = import(&bytes).unwrap();
        assert!(back.contains("1.5"), "{back}");
        assert!(back.contains("18446744073709551616.0"), "{back}");
    }
}
