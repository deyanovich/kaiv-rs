//! ASN.1 BER/DER import and DER export (`--features asn1`) —
//! hand-rolled, zero dependencies, schema-less: BER/DER is
//! structurally self-describing (every element carries a tag and
//! length), so universal tags drive the mapping and no ASN.1 module
//! is needed. PEM armor is stripped automatically on import; export
//! writes raw DER.
//!
//! ASN.1 structures are positional and unnamed — field names live in
//! schemas, not on the wire — so every element imports as a
//! single-member wrapper namespace naming its type: `{"seq": […]}`,
//! `{"set": […]}`, `{"int": n}`, `{"bool": b}`, `{"null": }`,
//! `{"utf8": s}` (and `printable`/`ia5`/`numeric`/`visible`/`t61`/
//! `general`/`graphic`/`videotex`/`bmp`/`universal` for the other
//! string types), `{"octets": &bin}`, `{"oid": "2.5.4.3"}`,
//! `{"reloid": …}`, `{"enum": n}`, `{"bits": &bin}` (whole bytes) or
//! `{"bits": "0110…"}` (a bit-character string when trailing bits are
//! unused), `{"utc": &datetime}` / `{"gentime": &datetime|
//! &localdatetime}` riding the `std/time` channel. Tagged elements
//! are `{"c0": [children]}` / `{"c0p": &bin}` (context), `a…`
//! (application), `x…` (private); unrecognized universal tags fall
//! back to `{"u9p": &bin}` / `{"u8c": […]}` — total coverage, nothing
//! rejected but constructed strings and groups of trailing garbage.
//!
//! Fidelity: DER input round-trips byte-identically. BER-only forms
//! normalize to DER on re-export (indefinite lengths become definite,
//! non-minimal integers and lengths become minimal, BOOLEAN true
//! becomes 0xFF, offset times shift to UTC-with-Z and GeneralizedTime
//! fractions drop trailing zeros); SET element order is preserved as
//! read, not re-sorted. The one deliberate BER carve-out: a zoneless
//! (local) GeneralizedTime is emitted for `std/time/localdatetime`,
//! which is zone-free by definition — converting it would invent a
//! timezone. Known edges: REAL has no native mapping (it rides the
//! `u9p` bytes fallback); constructed string encodings are rejected
//! (DER forbids them); export accepts bare values for convenience
//! (Bool, integer tokens, strings as UTF8String, `&bin` as OCTET
//! STRING, arrays as SEQUENCE, `std/time` datetimes as
//! GeneralizedTime) but a multi-member namespace has no DER form —
//! names cannot ride the wire without a schema.

use crate::error::PipelineError;
use crate::json::{self, node_to_val, token_to_twos_complement, twos_complement_token, Val};

const MAX_DEPTH: usize = 512;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

pub fn import(input: &[u8]) -> Result<String, PipelineError> {
    let pem;
    let der = if is_pem(input) {
        pem = pem_decode(input)?;
        &pem[..]
    } else {
        input
    };
    let mut r = R { b: der, i: 0 };
    let root = element(&mut r, 0)?;
    if r.i != der.len() {
        return Err(err("trailing bytes after the ASN.1 element"));
    }
    let members = match root {
        Val::Obj(ms) => ms,
        other => vec![(bare_name(&other).to_string(), other)],
    };
    json::import_val(&members, false)
}

pub fn export(canonical: &str) -> Result<Vec<u8>, PipelineError> {
    let root = node_to_val(&json::tree(canonical)?)?;
    let mut out = Vec::new();
    encode(&root, &mut out, 0)?;
    Ok(out)
}

/// The wrapper key a bare value takes at the document root.
fn bare_name(v: &Val) -> &'static str {
    match v {
        Val::Arr(_) => "seq",
        Val::Num(_) => "int",
        Val::Bool(_) => "bool",
        Val::Null => "null",
        Val::Str(_) => "utf8",
        Val::Typed { .. } => "octets",
        Val::Obj(_) => unreachable!("wrappers are objects"),
    }
}

// ------------------------------------------------------------------ PEM

/// PEM is text with an armor line; raw DER routinely *contains*
/// `-----BEGIN ` as string content, so the check demands valid UTF-8
/// and a line that starts with the armor (not a substring anywhere).
fn is_pem(input: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(input) else {
        return false;
    };
    text.lines().any(|l| l.trim().starts_with("-----BEGIN "))
}

/// The first PEM block's payload (banner text before it is fine).
fn pem_decode(input: &[u8]) -> Result<Vec<u8>, PipelineError> {
    let text = std::str::from_utf8(input).map_err(|_| err("PEM input is not UTF-8"))?;
    let mut body = String::new();
    let mut inside = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN ") {
            if inside {
                return Err(err("nested PEM BEGIN"));
            }
            inside = true;
        } else if line.starts_with("-----END ") {
            return b64std_decode(&body).ok_or_else(|| err("invalid base64 in PEM body"));
        } else if inside {
            body.push_str(line);
        }
    }
    Err(err("unterminated PEM block"))
}

/// Standard base64 (RFC 4648 §4), `=` padding, whitespace-free input.
fn b64std_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let b = s.trim_end_matches('=').as_bytes();
    if b.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() * 3 / 4);
    for chunk in b.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
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

// --------------------------------------------------------------- decode

struct R<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> R<'a> {
    fn u8(&mut self) -> Result<u8, PipelineError> {
        let v = *self
            .b
            .get(self.i)
            .ok_or_else(|| err("truncated ASN.1 input"))?;
        self.i += 1;
        Ok(v)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], PipelineError> {
        let end = self
            .i
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .ok_or_else(|| err("truncated ASN.1 input"))?;
        let out = &self.b[self.i..end];
        self.i = end;
        Ok(out)
    }

    /// (class 0-3, constructed, tag number), high tag numbers included.
    fn tag(&mut self) -> Result<(u8, bool, u32), PipelineError> {
        let first = self.u8()?;
        let class = first >> 6;
        let constructed = first & 0x20 != 0;
        let mut number = u32::from(first & 0x1f);
        if number == 31 {
            number = 0;
            loop {
                let b = self.u8()?;
                number = number
                    .checked_mul(128)
                    .and_then(|n| n.checked_add(u32::from(b & 0x7f)))
                    .ok_or_else(|| err("ASN.1 tag number overflows"))?;
                if b & 0x80 == 0 {
                    break;
                }
            }
        }
        Ok((class, constructed, number))
    }

    /// Definite length, or None for indefinite.
    fn length(&mut self) -> Result<Option<usize>, PipelineError> {
        let first = self.u8()?;
        if first < 0x80 {
            return Ok(Some(first as usize));
        }
        if first == 0x80 {
            return Ok(None);
        }
        let n = (first & 0x7f) as usize;
        if n > std::mem::size_of::<usize>() {
            return Err(err("ASN.1 length too large"));
        }
        let mut len = 0usize;
        for &b in self.take(n)? {
            len = (len << 8) | b as usize;
        }
        Ok(Some(len))
    }

    /// The children of a constructed element: definite length or
    /// indefinite (until end-of-contents).
    fn children(&mut self, len: Option<usize>, depth: usize) -> Result<Vec<Val>, PipelineError> {
        let mut kids = Vec::new();
        match len {
            Some(n) => {
                let body = self.take(n)?;
                let mut sub = R { b: body, i: 0 };
                while sub.i < body.len() {
                    kids.push(element(&mut sub, depth + 1)?);
                }
            }
            None => loop {
                if self.b[self.i..].starts_with(&[0x00, 0x00]) {
                    self.i += 2;
                    break;
                }
                if self.i >= self.b.len() {
                    return Err(err("unterminated indefinite-length element"));
                }
                kids.push(element(self, depth + 1)?);
            },
        }
        Ok(kids)
    }

    /// Primitive content (must be definite).
    fn content(&mut self, len: Option<usize>) -> Result<&'a [u8], PipelineError> {
        let n = len.ok_or_else(|| err("indefinite length on a primitive element"))?;
        self.take(n)
    }
}

fn element(r: &mut R, depth: usize) -> Result<Val, PipelineError> {
    if depth > MAX_DEPTH {
        return Err(err("ASN.1 nesting too deep"));
    }
    let (class, constructed, number) = r.tag()?;
    let len = r.length()?;
    if class != 0 {
        let prefix = match class {
            1 => "a",
            2 => "c",
            _ => "x",
        };
        return Ok(if constructed {
            wrap(
                format!("{prefix}{number}"),
                Val::Arr(r.children(len, depth)?),
            )
        } else {
            wrap(format!("{prefix}{number}p"), bin(r.content(len)?))
        });
    }
    if constructed {
        return Ok(match number {
            // SEQUENCE wraps like everything else: a bare array
            // inside an array would be anonymous, and anonymous
            // nested arrays embed rather than staying native.
            16 => wrap("seq".to_string(), Val::Arr(r.children(len, depth)?)),
            17 => wrap("set".to_string(), Val::Arr(r.children(len, depth)?)),
            3 | 4 | 12 | 18..=30 => {
                return Err(err(
                    "constructed string/time encodings are not supported (DER forbids them)",
                ))
            }
            n => wrap(format!("u{n}c"), Val::Arr(r.children(len, depth)?)),
        });
    }
    let c = r.content(len)?;
    Ok(match number {
        1 => {
            if c.len() != 1 {
                return Err(err("BOOLEAN content must be one byte"));
            }
            Val::Bool(c[0] != 0)
        }
        2 | 10 => {
            if c.is_empty() {
                return Err(err("empty INTEGER content"));
            }
            let n = Val::Num(twos_complement_token(c, 0));
            if number == 2 {
                n
            } else {
                wrap("enum".to_string(), n)
            }
        }
        3 => bits_val(c)?,
        4 => bin(c),
        5 => {
            if !c.is_empty() {
                return Err(err("NULL content must be empty"));
            }
            Val::Null
        }
        6 => wrap("oid".to_string(), Val::Str(oid_string(c, false)?)),
        13 => wrap("reloid".to_string(), Val::Str(oid_string(c, true)?)),
        12 => Val::Str(utf8(c)?),
        19 => wrap("printable".to_string(), Val::Str(utf8(c)?)),
        22 => wrap("ia5".to_string(), Val::Str(utf8(c)?)),
        18 => wrap("numeric".to_string(), Val::Str(utf8(c)?)),
        26 => wrap("visible".to_string(), Val::Str(utf8(c)?)),
        20 => wrap("t61".to_string(), Val::Str(utf8(c)?)),
        27 => wrap("general".to_string(), Val::Str(utf8(c)?)),
        25 => wrap("graphic".to_string(), Val::Str(utf8(c)?)),
        21 => wrap("videotex".to_string(), Val::Str(utf8(c)?)),
        30 => wrap("bmp".to_string(), Val::Str(utf16be(c)?)),
        28 => wrap("universal".to_string(), Val::Str(utf32be(c)?)),
        23 => wrap("utc".to_string(), time_typed(utc_to_token(c)?)),
        24 => {
            let (token, local) = gen_to_token(c)?;
            wrap(
                "gentime".to_string(),
                if local {
                    local_typed(token)
                } else {
                    time_typed(token)
                },
            )
        }
        16 | 17 => return Err(err("SEQUENCE/SET must be constructed")),
        n => wrap(format!("u{n}p"), bin(c)),
    })
}

fn wrap(key: String, v: Val) -> Val {
    Val::Obj(vec![(key, v)])
}

fn bin(bytes: &[u8]) -> Val {
    Val::Typed {
        lib: "std/enc".to_string(),
        name: "bin".to_string(),
        text: json::b64url_encode(bytes),
    }
}

fn time_typed(token: String) -> Val {
    Val::Typed {
        lib: "std/time".to_string(),
        name: "datetime".to_string(),
        text: token,
    }
}

fn local_typed(token: String) -> Val {
    Val::Typed {
        lib: "std/time".to_string(),
        name: "localdatetime".to_string(),
        text: token,
    }
}

fn utf8(c: &[u8]) -> Result<String, PipelineError> {
    String::from_utf8(c.to_vec()).map_err(|_| err("ASN.1 string content is not UTF-8"))
}

fn utf16be(c: &[u8]) -> Result<String, PipelineError> {
    if c.len() % 2 != 0 {
        return Err(err("BMPString content has odd length"));
    }
    let units: Vec<u16> = c
        .chunks(2)
        .map(|p| u16::from_be_bytes([p[0], p[1]]))
        .collect();
    String::from_utf16(&units).map_err(|_| err("invalid BMPString content"))
}

fn utf32be(c: &[u8]) -> Result<String, PipelineError> {
    if c.len() % 4 != 0 {
        return Err(err("UniversalString content length is not a multiple of 4"));
    }
    c.chunks(4)
        .map(|p| {
            char::from_u32(u32::from_be_bytes([p[0], p[1], p[2], p[3]]))
                .ok_or_else(|| err("invalid UniversalString content"))
        })
        .collect()
}

/// BIT STRING: whole bytes ride as `&bin`; trailing unused bits force
/// the exact bit-character form.
fn bits_val(c: &[u8]) -> Result<Val, PipelineError> {
    let (&unused, data) = c
        .split_first()
        .ok_or_else(|| err("empty BIT STRING content"))?;
    if unused > 7 || (data.is_empty() && unused != 0) {
        return Err(err("invalid BIT STRING unused-bits count"));
    }
    if unused == 0 {
        return Ok(wrap("bits".to_string(), bin(data)));
    }
    let total = data.len() * 8 - unused as usize;
    let mut s = String::with_capacity(total);
    for i in 0..total {
        s.push(if data[i / 8] >> (7 - i % 8) & 1 == 1 {
            '1'
        } else {
            '0'
        });
    }
    Ok(wrap("bits".to_string(), Val::Str(s)))
}

fn oid_string(c: &[u8], relative: bool) -> Result<String, PipelineError> {
    let mut arcs: Vec<u128> = Vec::new();
    let mut cur = 0u128;
    let mut in_arc = false;
    for &b in c {
        cur = cur
            .checked_mul(128)
            .and_then(|v| v.checked_add(u128::from(b & 0x7f)))
            .ok_or_else(|| err("OID arc overflows"))?;
        in_arc = b & 0x80 != 0;
        if !in_arc {
            arcs.push(cur);
            cur = 0;
        }
    }
    if in_arc || arcs.is_empty() {
        return Err(err("malformed OID content"));
    }
    if !relative {
        let first = arcs.remove(0);
        let (x, y) = match first {
            0..=39 => (0, first),
            40..=79 => (1, first - 40),
            _ => (2, first - 80),
        };
        arcs.insert(0, y);
        arcs.insert(0, x);
    }
    Ok(arcs
        .iter()
        .map(|a| a.to_string())
        .collect::<Vec<_>>()
        .join("."))
}

// ---------------------------------------------------------------- times

/// UTCTime → RFC 3339-shaped token: `YYMMDDHHMM[SS](Z|±HHMM)`, the
/// X.509 century rule (00-49 → 20xx, 50-99 → 19xx).
fn utc_to_token(c: &[u8]) -> Result<String, PipelineError> {
    let s = std::str::from_utf8(c).map_err(|_| err("UTCTime is not ASCII"))?;
    let bad = || err(format!("malformed UTCTime: {s}"));
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    let (sec, zone_at) = match digits {
        10 => ("00", 10),
        12 => (&s[10..12], 12),
        _ => return Err(bad()),
    };
    let century = if &s[..2] < "50" { "20" } else { "19" };
    Ok(format!(
        "{century}{}-{}-{}T{}:{}:{sec}{}",
        &s[..2],
        &s[2..4],
        &s[4..6],
        &s[6..8],
        &s[8..10],
        zone_token(&s[zone_at..]).ok_or_else(bad)?
    ))
}

/// GeneralizedTime → token + is-local: `YYYYMMDDHH[MM[SS]][.f](zone?)`.
fn gen_to_token(c: &[u8]) -> Result<(String, bool), PipelineError> {
    let s = std::str::from_utf8(c).map_err(|_| err("GeneralizedTime is not ASCII"))?;
    let bad = || err(format!("malformed GeneralizedTime: {s}"));
    let digits = s.bytes().take_while(|b| b.is_ascii_digit()).count();
    let (mi, sec, mut rest) = match digits {
        10 => ("00".to_string(), "00".to_string(), &s[10..]),
        12 => (s[10..12].to_string(), "00".to_string(), &s[12..]),
        14 => (s[10..12].to_string(), s[12..14].to_string(), &s[14..]),
        _ => return Err(bad()),
    };
    let mut frac = String::new();
    if rest.starts_with(['.', ',']) {
        if digits != 14 {
            return Err(err(format!(
                "fractional GeneralizedTime without full seconds is not supported: {s}"
            )));
        }
        let n = rest[1..].bytes().take_while(|b| b.is_ascii_digit()).count();
        if n == 0 {
            return Err(bad());
        }
        frac = format!(".{}", &rest[1..1 + n]);
        rest = &rest[1 + n..];
    }
    let (zone, local) = if rest.is_empty() {
        (String::new(), true)
    } else {
        (zone_token(rest).ok_or_else(bad)?, false)
    };
    Ok((
        format!(
            "{}-{}-{}T{}:{mi}:{sec}{frac}{zone}",
            &s[..4],
            &s[4..6],
            &s[6..8],
            &s[8..10]
        ),
        local,
    ))
}

/// `Z` or `±HHMM`/`±HH` → the token zone suffix.
fn zone_token(z: &str) -> Option<String> {
    match z.as_bytes() {
        b"Z" => Some("Z".to_string()),
        [s @ (b'+' | b'-'), h1, h2] if h1.is_ascii_digit() && h2.is_ascii_digit() => {
            Some(format!("{}{}{}:00", *s as char, *h1 as char, *h2 as char))
        }
        [s @ (b'+' | b'-'), rest @ ..]
            if rest.len() == 4 && rest.iter().all(u8::is_ascii_digit) =>
        {
            Some(format!(
                "{}{}:{}",
                *s as char,
                std::str::from_utf8(&rest[..2]).expect("ascii"),
                std::str::from_utf8(&rest[2..]).expect("ascii")
            ))
        }
        _ => None,
    }
}

/// A datetime token → (`YYYYMMDDHHMMSS`, fraction digits, zone) DER
/// pieces; zone is None for local tokens.
fn token_pieces(t: &str) -> Result<(String, String, Option<String>), PipelineError> {
    let bad = || err(format!("malformed datetime token: {t}"));
    let b = t.as_bytes();
    if b.len() < 19 || !matches!(b[10], b'T' | b't' | b' ') {
        return Err(bad());
    }
    // Byte 19 may straddle a multibyte char (a valid token is ASCII
    // through the time field, so this only rejects malformed input).
    if !t.is_char_boundary(19) {
        return Err(bad());
    }
    let date = &t[..10];
    let time = &t[11..19];
    if date.as_bytes()[4] != b'-'
        || date.as_bytes()[7] != b'-'
        || time.as_bytes()[2] != b':'
        || time.as_bytes()[5] != b':'
    {
        return Err(bad());
    }
    let compact = format!(
        "{}{}{}{}{}{}",
        &date[..4],
        &date[5..7],
        &date[8..10],
        &time[..2],
        &time[3..5],
        &time[6..8]
    );
    if !compact.bytes().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let mut rest = &t[19..];
    let mut frac = String::new();
    if rest.starts_with('.') {
        let n = rest[1..].bytes().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return Err(bad());
        }
        frac = rest[..1 + n].to_string();
        rest = &rest[1 + n..];
    }
    let zone = match rest.as_bytes() {
        [] => None,
        [b'Z' | b'z'] => Some("Z".to_string()),
        [s @ (b'+' | b'-'), h1, h2, b':', m1, m2]
            if [h1, h2, m1, m2].iter().all(|c| c.is_ascii_digit()) =>
        {
            Some(format!(
                "{}{}{}{}{}",
                *s as char, *h1 as char, *h2 as char, *m1 as char, *m2 as char
            ))
        }
        _ => return Err(bad()),
    };
    Ok((compact, frac, zone))
}

// --------------------------------------------------------------- encode

fn encode(v: &Val, out: &mut Vec<u8>, depth: usize) -> Result<(), PipelineError> {
    if depth > MAX_DEPTH {
        return Err(err("nesting too deep for ASN.1 export"));
    }
    match v {
        Val::Bool(b) => wrap_tlv(0, false, 1, &[if *b { 0xff } else { 0x00 }], out),
        Val::Null => wrap_tlv(0, false, 5, &[], out),
        Val::Num(raw) => wrap_tlv(0, false, 2, &token_to_twos_complement(raw)?, out),
        Val::Str(s) => wrap_tlv(0, false, 12, s.as_bytes(), out),
        Val::Typed { lib, name, text } if lib == "std/enc" && name == "bin" => {
            let b = json::b64url_decode(text).ok_or_else(|| err("invalid base64url payload"))?;
            wrap_tlv(0, false, 4, &b, out);
        }
        Val::Typed { lib, name, text } if lib == "std/time" => match name.as_str() {
            "datetime" | "localdatetime" => encode_gentime(text, out)?,
            other => return Err(err(format!("std/time/{other} has no ASN.1 mapping"))),
        },
        Val::Typed { .. } => return Err(err("this typed value has no ASN.1 mapping")),
        Val::Arr(items) => encode_constructed(0, 16, items, out, depth)?,
        Val::Obj(ms) => {
            let [(key, inner)] = ms.as_slice() else {
                return Err(err(
                    "a multi-member namespace has no DER form (ASN.1 fields are positional)",
                ));
            };
            encode_wrapper(key, inner, out, depth)?;
        }
    }
    Ok(())
}

fn encode_wrapper(
    key: &str,
    v: &Val,
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<(), PipelineError> {
    let bad = |what: &str| err(format!("{key} expects {what}"));
    match key {
        "seq" | "set" => {
            let Val::Arr(items) = v else {
                return Err(bad("an array"));
            };
            encode_constructed(0, if key == "seq" { 16 } else { 17 }, items, out, depth)?;
        }
        "int" | "enum" => {
            let Val::Num(raw) = v else {
                return Err(bad("an integer"));
            };
            wrap_tlv(
                0,
                false,
                if key == "int" { 2 } else { 10 },
                &token_to_twos_complement(raw)?,
                out,
            );
        }
        "bool" => {
            let Val::Bool(b) = v else {
                return Err(bad("a boolean"));
            };
            wrap_tlv(0, false, 1, &[if *b { 0xff } else { 0x00 }], out);
        }
        "null" => {
            if !matches!(v, Val::Null) {
                return Err(bad("null"));
            }
            wrap_tlv(0, false, 5, &[], out);
        }
        "octets" => match v {
            Val::Typed { lib, name, text } if lib == "std/enc" && name == "bin" => {
                let b =
                    json::b64url_decode(text).ok_or_else(|| err("invalid base64url payload"))?;
                wrap_tlv(0, false, 4, &b, out);
            }
            Val::Str(s) => wrap_tlv(0, false, 4, s.as_bytes(), out),
            _ => return Err(bad("binary content")),
        },
        "oid" | "reloid" => {
            let Val::Str(s) = v else {
                return Err(bad("a dotted OID string"));
            };
            let content = oid_bytes(s, key == "reloid")?;
            wrap_tlv(0, false, if key == "oid" { 6 } else { 13 }, &content, out);
        }
        "bits" => {
            let content = match v {
                Val::Typed { lib, name, text } if lib == "std/enc" && name == "bin" => {
                    let b = json::b64url_decode(text)
                        .ok_or_else(|| err("invalid base64url payload"))?;
                    let mut c = vec![0u8];
                    c.extend_from_slice(&b);
                    c
                }
                Val::Str(s) if s.bytes().all(|c| matches!(c, b'0' | b'1')) => {
                    let unused = (8 - s.len() % 8) % 8;
                    let mut c = vec![unused as u8];
                    c.extend(s.as_bytes().chunks(8).map(|chunk| {
                        chunk
                            .iter()
                            .enumerate()
                            .fold(0u8, |acc, (i, b)| acc | ((b - b'0') << (7 - i)))
                    }));
                    c
                }
                _ => return Err(bad("binary content or a 0/1 string")),
            };
            wrap_tlv(0, false, 3, &content, out);
        }
        "utf8" => wrap_str(v, 12, out, bad)?,
        "printable" => wrap_str(v, 19, out, bad)?,
        "ia5" => wrap_str(v, 22, out, bad)?,
        "numeric" => wrap_str(v, 18, out, bad)?,
        "visible" => wrap_str(v, 26, out, bad)?,
        "t61" => wrap_str(v, 20, out, bad)?,
        "general" => wrap_str(v, 27, out, bad)?,
        "graphic" => wrap_str(v, 25, out, bad)?,
        "videotex" => wrap_str(v, 21, out, bad)?,
        "bmp" => {
            let Val::Str(s) = v else {
                return Err(bad("a string"));
            };
            let content: Vec<u8> = s.encode_utf16().flat_map(u16::to_be_bytes).collect();
            wrap_tlv(0, false, 30, &content, out);
        }
        "universal" => {
            let Val::Str(s) = v else {
                return Err(bad("a string"));
            };
            let content: Vec<u8> = s.chars().flat_map(|c| (c as u32).to_be_bytes()).collect();
            wrap_tlv(0, false, 28, &content, out);
        }
        "utc" => encode_utctime(token_text(v).ok_or_else(|| bad("a datetime"))?, out)?,
        "gentime" => encode_gentime(token_text(v).ok_or_else(|| bad("a datetime"))?, out)?,
        _ => {
            let (class, number, kind) = parse_tag_key(key)
                .ok_or_else(|| err(format!("unknown ASN.1 wrapper key: {key}")))?;
            match kind {
                TagKind::Constructed => {
                    let Val::Arr(items) = v else {
                        return Err(bad("an array of children"));
                    };
                    encode_constructed(class, number, items, out, depth)?;
                }
                TagKind::Primitive => match v {
                    Val::Typed { lib, name, text } if lib == "std/enc" && name == "bin" => {
                        let b = json::b64url_decode(text)
                            .ok_or_else(|| err("invalid base64url payload"))?;
                        wrap_tlv(class, false, number, &b, out);
                    }
                    Val::Str(s) => wrap_tlv(class, false, number, s.as_bytes(), out),
                    _ => return Err(bad("binary content")),
                },
            }
        }
    }
    Ok(())
}

fn token_text(v: &Val) -> Option<&str> {
    match v {
        Val::Typed { lib, name, text }
            if lib == "std/time" && matches!(name.as_str(), "datetime" | "localdatetime") =>
        {
            Some(text)
        }
        Val::Str(s) => Some(s),
        _ => None,
    }
}

fn wrap_str(
    v: &Val,
    number: u32,
    out: &mut Vec<u8>,
    bad: impl Fn(&str) -> PipelineError,
) -> Result<(), PipelineError> {
    let Val::Str(s) = v else {
        return Err(bad("a string"));
    };
    wrap_tlv(0, false, number, s.as_bytes(), out);
    Ok(())
}

enum TagKind {
    Constructed,
    Primitive,
}

/// `c0`/`c0p`, `a…` (application), `x…` (private), and the
/// unrecognized-universal fallbacks `u9p`/`u8c`.
fn parse_tag_key(key: &str) -> Option<(u8, u32, TagKind)> {
    let (class, rest) = match key.as_bytes().first()? {
        b'c' => (2u8, &key[1..]),
        b'a' => (1, &key[1..]),
        b'x' => (3, &key[1..]),
        b'u' => (0, &key[1..]),
        _ => return None,
    };
    let universal = class == 0;
    let (digits, kind) = if let Some(d) = rest.strip_suffix('p') {
        (d, TagKind::Primitive)
    } else if universal {
        (rest.strip_suffix('c')?, TagKind::Constructed)
    } else {
        (rest, TagKind::Constructed)
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((class, digits.parse().ok()?, kind))
}

fn encode_constructed(
    class: u8,
    number: u32,
    items: &[Val],
    out: &mut Vec<u8>,
    depth: usize,
) -> Result<(), PipelineError> {
    let mut body = Vec::new();
    for item in items {
        encode(item, &mut body, depth + 1)?;
    }
    wrap_tlv(class, true, number, &body, out);
    Ok(())
}

fn encode_utctime(token: &str, out: &mut Vec<u8>) -> Result<(), PipelineError> {
    let (compact, frac, zone) = token_pieces(token)?;
    if !frac.is_empty() {
        return Err(err("UTCTime cannot carry fractional seconds"));
    }
    let zone = zone.ok_or_else(|| err("UTCTime needs a zone (Z or ±HH:MM)"))?;
    // DER (X.690 §11.8) admits only the UTC form: shift an offset
    // reading to UTC and emit `Z`.
    let compact = if zone == "Z" {
        compact
    } else {
        compact_to_utc(&compact, &zone)?
    };
    let year: u32 = compact[..4].parse().expect("digits");
    if !(1950..=2049).contains(&year) {
        return Err(err(format!("UTCTime year out of range: {year}")));
    }
    let content = format!("{}Z", &compact[2..]);
    wrap_tlv(0, false, 23, content.as_bytes(), out);
    Ok(())
}

fn encode_gentime(token: &str, out: &mut Vec<u8>) -> Result<(), PipelineError> {
    let (compact, mut frac, zone) = token_pieces(token)?;
    // DER (X.690 §11.7) forbids trailing fractional zeros.
    while frac.ends_with('0') {
        frac.pop();
    }
    if frac == "." {
        frac.clear();
    }
    // Offset readings shift to UTC-with-Z; a zoneless token stays the
    // local form (the documented localdatetime carve-out).
    let content = match zone.as_deref() {
        None => format!("{compact}{frac}"),
        Some("Z") => format!("{compact}{frac}Z"),
        Some(z) => format!("{}{frac}Z", compact_to_utc(&compact, z)?),
    };
    wrap_tlv(0, false, 24, content.as_bytes(), out);
    Ok(())
}

/// Shift a wall-clock `YYYYMMDDHHMMSS` reading by its `±HHMM` offset
/// to UTC. Offsets are whole minutes, so seconds pass through.
fn compact_to_utc(compact: &str, zone: &str) -> Result<String, PipelineError> {
    let num = |s: &str| s.parse::<i64>().expect("digits");
    let (y, mo, d) = (
        num(&compact[..4]),
        num(&compact[4..6]),
        num(&compact[6..8]),
    );
    let (h, mi) = (num(&compact[8..10]), num(&compact[10..12]));
    let sec = &compact[12..14];
    let sign = if zone.starts_with('-') { -1i64 } else { 1 };
    let (oh, om) = (num(&zone[1..3]), num(&zone[3..5]));
    if oh > 23 || om > 59 {
        return Err(err(format!("bad zone offset in datetime: {zone}")));
    }
    let total = days_from_civil(y, mo, d) * 24 * 60 + h * 60 + mi - sign * (oh * 60 + om);
    let (days, rem) = (total.div_euclid(24 * 60), total.rem_euclid(24 * 60));
    let (uy, umo, ud) = civil_from_days(days);
    if !(0..=9999).contains(&uy) {
        return Err(err("datetime out of range after the UTC shift"));
    }
    Ok(format!(
        "{uy:04}{umo:02}{ud:02}{:02}{:02}{sec}",
        rem / 60,
        rem % 60
    ))
}

/// Days since 1970-01-01 of a proleptic-Gregorian civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Civil date of a days-since-1970-01-01 count.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn oid_bytes(s: &str, relative: bool) -> Result<Vec<u8>, PipelineError> {
    let bad = || err(format!("malformed OID: {s}"));
    let mut arcs: Vec<u128> = Vec::new();
    for part in s.split('.') {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        arcs.push(part.parse().map_err(|_| bad())?);
    }
    if !relative {
        if arcs.len() < 2 {
            return Err(bad());
        }
        let x = arcs.remove(0);
        let y = arcs.remove(0);
        if x > 2 || (x < 2 && y > 39) {
            return Err(bad());
        }
        arcs.insert(0, x * 40 + y);
    }
    let mut out = Vec::new();
    for arc in arcs {
        let mut tmp = [0u8; 19];
        let mut n = 0;
        let mut v = arc;
        loop {
            tmp[n] = (v & 0x7f) as u8;
            v >>= 7;
            n += 1;
            if v == 0 {
                break;
            }
        }
        for i in (0..n).rev() {
            out.push(tmp[i] | if i > 0 { 0x80 } else { 0 });
        }
    }
    Ok(out)
}

/// Tag + definite minimal length + content (DER).
fn wrap_tlv(class: u8, constructed: bool, number: u32, content: &[u8], out: &mut Vec<u8>) {
    let head = (class << 6) | if constructed { 0x20 } else { 0 };
    if number < 31 {
        out.push(head | number as u8);
    } else {
        out.push(head | 0x1f);
        let mut tmp = [0u8; 5];
        let mut n = 0;
        let mut v = number;
        loop {
            tmp[n] = (v & 0x7f) as u8;
            v >>= 7;
            n += 1;
            if v == 0 {
                break;
            }
        }
        for i in (0..n).rev() {
            out.push(tmp[i] | if i > 0 { 0x80 } else { 0 });
        }
    }
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_pieces_rejects_multibyte_without_panic() {
        assert!(token_pieces("0000000000T0000000é").is_err());
    }

    fn build(authored: &str) -> String {
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        crate::denorm::denormalize(&raiv).unwrap()
    }

    fn roundtrip(src: &[u8]) -> Vec<u8> {
        export(&build(&import(src).unwrap())).unwrap()
    }

    /// A SEQUENCE exercising every mapped universal type plus tagged
    /// elements.
    fn sample() -> Vec<u8> {
        let mut body = Vec::new();
        body.extend([0x02, 0x02, 0x20, 0xfb]); // INTEGER 8443
        body.extend([0x01, 0x01, 0xff]); // BOOLEAN true
        body.extend([0x05, 0x00]); // NULL
        body.extend([0x0c, 0x05]);
        body.extend(b"web01"); // UTF8String
        body.extend([0x04, 0x03, 0x00, 0xff, 0x10]); // OCTET STRING
        body.extend([0x13, 0x02]);
        body.extend(b"US"); // PrintableString
        body.extend([0x06, 0x03, 0x55, 0x04, 0x03]); // OID 2.5.4.3
        body.extend([0x03, 0x03, 0x00, 0xde, 0xad]); // BIT STRING, unused 0
        body.extend([0x03, 0x02, 0x04, 0xb0]); // BIT STRING "1011"
        body.extend([0x17, 0x0d]);
        body.extend(b"260705100000Z"); // UTCTime
        body.extend([0x18, 0x0f]);
        body.extend(b"20260705100000Z"); // GeneralizedTime
        body.extend([0x0a, 0x01, 0x03]); // ENUMERATED 3
        body.extend([0x31, 0x03, 0x02, 0x01, 0x01]); // SET { INTEGER 1 }
        body.extend([0xa0, 0x03, 0x02, 0x01, 0x02]); // [0] { INTEGER 2 }
        body.extend([0x81, 0x02, 0xaa, 0xbb]); // [1] primitive
        let mut out = vec![0x30, body.len() as u8];
        out.extend(body);
        out
    }

    #[test]
    fn import_typing_and_natives() {
        let out = import(&sample()).unwrap();
        assert!(out.contains("!int\n/@seq::0=8443\n"));
        assert!(out.contains("!bool\n/@seq::1=true\n"));
        assert!(out.contains("!null\n/@seq::2=\n"));
        assert!(out.contains("/@seq::3=web01\n"));
        assert!(out.contains(&format!(
            "&bin\n/@seq::4={}\n",
            json::b64url_encode(&[0x00, 0xff, 0x10])
        )));
        assert!(out.contains("/@seq/5:=printable=US\n"));
        assert!(out.contains("/@seq/6:=oid=2.5.4.3\n"));
        assert!(out.contains(&format!(
            "&bin\n/@seq/7:=bits={}\n",
            json::b64url_encode(&[0xde, 0xad])
        )));
        assert!(out.contains("/@seq/8:=bits=1011\n"));
        assert!(out.contains("&datetime\n/@seq/9:=utc=2026-07-05T10:00:00Z\n"));
        assert!(out.contains("&datetime\n/@seq/10:=gentime=2026-07-05T10:00:00Z\n"));
        assert!(out.contains("!int\n/@seq/11:=enum=3\n"));
        assert!(out.contains("!int\n/@seq/12/@set;=1\n"));
        assert!(out.contains("!int\n/@seq/13/@c0;=2\n"));
        assert!(out.contains(&format!(
            "&bin\n/@seq/14:=c1p={}\n",
            json::b64url_encode(&[0xaa, 0xbb])
        )));
        assert_eq!(roundtrip(&sample()), sample());
    }

    #[test]
    fn integers_exact_at_any_width() {
        for (der, token) in [
            (&[0x02u8, 0x01, 0x00][..], "0"),
            (&[0x02, 0x01, 0xff], "-1"),
            (&[0x02, 0x02, 0x00, 0x80], "128"),
            (&[0x02, 0x01, 0x80], "-128"),
            (
                &[0x02, 0x09, 0x01, 0, 0, 0, 0, 0, 0, 0, 0],
                "18446744073709551616",
            ),
        ] {
            let out = import(der).unwrap();
            assert!(out.contains(&format!("int={token}\n")), "{token}");
            assert_eq!(roundtrip(der), der, "{token}");
        }
    }

    #[test]
    fn long_lengths_high_tags_and_indefinite() {
        // Long-form length: OCTET STRING of 200 bytes.
        let mut der = vec![0x04, 0x81, 200];
        der.extend(vec![0x5a; 200]);
        assert_eq!(roundtrip(&der), der);
        // High tag number: private constructed [1000], empty.
        let der = vec![0xff, 0x87, 0x68, 0x00];
        let out = import(&der).unwrap();
        assert!(out.contains("x1000"));
        assert_eq!(roundtrip(&der), der);
        // Indefinite length (BER) normalizes to definite (DER).
        let ber = vec![0x30, 0x80, 0x02, 0x01, 0x05, 0x00, 0x00];
        assert_eq!(roundtrip(&ber), vec![0x30, 0x03, 0x02, 0x01, 0x05]);
    }

    #[test]
    fn pem_armor_strips() {
        let der = sample();
        // Standard base64 with line wrapping and a text banner.
        let b64 = {
            const A: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut s = String::new();
            for chunk in der.chunks(3) {
                let b = [
                    chunk[0],
                    *chunk.get(1).unwrap_or(&0),
                    *chunk.get(2).unwrap_or(&0),
                ];
                let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
                for (i, shift) in [18u32, 12, 6, 0].iter().enumerate() {
                    if i <= chunk.len() {
                        s.push(A[(n >> shift) as usize & 63] as char);
                    } else {
                        s.push('=');
                    }
                }
            }
            s
        };
        let mut pem = String::from("Subject: test banner\n\n-----BEGIN THING-----\n");
        for line in b64.as_bytes().chunks(64) {
            pem.push_str(std::str::from_utf8(line).unwrap());
            pem.push('\n');
        }
        pem.push_str("-----END THING-----\n");
        assert_eq!(import(pem.as_bytes()).unwrap(), import(&der).unwrap());
    }

    #[test]
    fn times_and_zones() {
        // UTCTime with offset; GeneralizedTime local and fractional.
        let mut body = Vec::new();
        body.extend([0x17, 0x11]);
        body.extend(b"991231235959+0230");
        body.extend([0x18, 0x0e]);
        body.extend(b"20260705100000"); // local, no zone
        body.extend([0x18, 0x12]);
        body.extend(b"20260705100000.25Z");
        let mut der = vec![0x30, body.len() as u8];
        der.extend(&body);
        let out = import(&der).unwrap();
        assert!(out.contains("&datetime\n/@seq+:=utc=1999-12-31T23:59:59+02:30\n"));
        assert!(out.contains("&localdatetime\n/@seq+:=gentime=2026-07-05T10:00:00\n"));
        assert!(out.contains("&datetime\n/@seq+:=gentime=2026-07-05T10:00:00.25Z\n"));
        // Re-export DER-normalizes the BER offset form to UTC-with-Z;
        // the zoneless GeneralizedTime is the documented localdatetime
        // carve-out and the fractional form is already DER.
        let mut nbody = Vec::new();
        nbody.extend([0x17, 0x0d]);
        nbody.extend(b"991231212959Z");
        nbody.extend([0x18, 0x0e]);
        nbody.extend(b"20260705100000");
        nbody.extend([0x18, 0x12]);
        nbody.extend(b"20260705100000.25Z");
        let mut nder = vec![0x30, nbody.len() as u8];
        nder.extend(&nbody);
        assert_eq!(roundtrip(&der), nder);
        // Stable after one normalization.
        assert_eq!(roundtrip(&nder), nder);
    }

    #[test]
    fn utc_shift_crosses_civil_boundaries() {
        // Year rollover backward (+01:00 offset) and forward (-01:00).
        let mut body = Vec::new();
        body.extend([0x18, 0x13]);
        body.extend(b"20260101003000+0100");
        body.extend([0x18, 0x13]);
        body.extend(b"20261231233000-0100");
        // Trailing fractional zeros are BER; DER trims them.
        body.extend([0x18, 0x13]);
        body.extend(b"20260705100000.250Z");
        let mut der = vec![0x30, body.len() as u8];
        der.extend(&body);
        let out = roundtrip(&der);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("20251231233000Z"), "{text}");
        assert!(text.contains("20270101003000Z"), "{text}");
        assert!(text.contains("20260705100000.25Z"), "{text}");
        assert!(!text.contains(".250"), "{text}");
    }

    #[test]
    fn cross_format_json_to_der() {
        let authored =
            crate::json::import(br#"{"seq":[1,"x",{"oid":"1.2.840.113549"},{"c0":[true]}]}"#)
                .unwrap();
        let der = export(&build(&authored)).unwrap();
        assert_eq!(
            der,
            vec![
                0x30, 0x13, 0x02, 0x01, 0x01, 0x0c, 0x01, b'x', 0x06, 0x06, 0x2a, 0x86, 0x48, 0x86,
                0xf7, 0x0d, 0xa0, 0x03, 0x01, 0x01, 0xff
            ]
        );
        let back = import(&der).unwrap();
        assert!(back.contains("!int\n/@seq::0=1\n"));
        assert!(back.contains("/@seq/2:=oid=1.2.840.113549\n"));
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(import(&[0x30]).is_err()); // truncated
        assert!(import(&[0x02, 0x01, 0x05, 0x00]).is_err()); // trailing
        assert!(import(&[0x02, 0x00]).is_err()); // empty INTEGER
        assert!(import(&[0x01, 0x02, 0x00, 0x00]).is_err()); // fat BOOLEAN
        assert!(import(&[0x24, 0x02, 0x04, 0x00]).is_err()); // constructed OCTET STRING
        assert!(import(&[0x30, 0x02, 0x05, 0x01]).is_err()); // NULL with content
        assert!(import(&[0x03, 0x02, 0x08, 0x00]).is_err()); // unused > 7
        assert!(import(b"-----BEGIN X-----\n!!!\n-----END X-----\n").is_err());
    }

    #[test]
    fn export_rejects_unrepresentable() {
        let e = |doc: &str| export(&build(doc)).unwrap_err().to_string();
        assert!(e(".!kaiv 1\n\na=1\nb=2\n").contains("positional"));
        assert!(e(".!kaiv 1\n!float\nint=1.5\n").contains("integer token"));
        assert!(e(".!kaiv 1\n\nwat=1\n").contains("unknown ASN.1 wrapper"));
        // UTCTime range and fraction rules.
        assert!(e(".!kaiv 1\n\nutc=2060-01-01T00:00:00Z\n").contains("out of range"));
        assert!(e(".!kaiv 1\n\nutc=2020-01-01T00:00:00.5Z\n").contains("fractional"));
    }
}
