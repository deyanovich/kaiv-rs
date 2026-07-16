//! CBOR import/export (`--features cbor`, RFC 8949) — a thin adapter
//! over the value hub and the family's first binary format; the
//! decoder/encoder pair is hand-rolled, zero dependencies.
//!
//! Mapping: the root item must be a map; text strings, arrays, and
//! maps land on their tree natives; byte strings ride the typed
//! channel as `std/enc/bin` (base64url) and decode back to byte
//! strings on export; tag 0 datetime strings ride as
//! `std/time/datetime` and re-emit tagged; non-finite floats are the
//! `std/num` markers (`&inf`, `&nan`), like the YAML/TOML pair.
//! Integers are exact at any width: beyond ±2^64 they travel as
//! decimal tokens and re-encode as bignums (tags 2/3).
//!
//! Fidelity is semantic. Number tokens normalize (a half-width 1.5
//! re-encodes as the shortest float that preserves the value —
//! RFC 8949 preferred serialization: definite lengths, minimal-width
//! heads); indefinite-length items are accepted and come back
//! definite. Known edges: `undefined` converges on null; tags other
//! than 0/2/3 are dropped (the tagged item imports as itself);
//! non-text scalar map keys stringify (`1` becomes `"1"`, and
//! exports as a text key); duplicate map keys are rejected
//! (RFC 8949 §5.6); non-scalar map keys are unsupported.

use crate::error::PipelineError;
use crate::json::{
    self, be_bytes_to_decimal, decimal_to_be_bytes, decrement, float_val, increment, node_to_val,
    Val,
};

const MAX_DEPTH: usize = 512;

fn err(msg: impl Into<String>) -> PipelineError {
    PipelineError::Other(msg.into())
}

pub fn import(input: &[u8]) -> Result<String, PipelineError> {
    let mut d = D { b: input, i: 0 };
    let root = d.item(0)?;
    if d.i != input.len() {
        return Err(err("trailing bytes after the CBOR item"));
    }
    let Val::Obj(members) = root else {
        return Err(err("root must be a CBOR map"));
    };
    json::import_val(&members, false)
}

// ------------------------------------------------------------- decode

struct D<'a> {
    b: &'a [u8],
    i: usize,
}

impl D<'_> {
    fn u8(&mut self) -> Result<u8, PipelineError> {
        let v = *self
            .b
            .get(self.i)
            .ok_or_else(|| err("truncated CBOR input"))?;
        self.i += 1;
        Ok(v)
    }

    fn take(&mut self, n: u64) -> Result<&[u8], PipelineError> {
        let n = usize::try_from(n).map_err(|_| err("CBOR length overflows"))?;
        let end = self
            .i
            .checked_add(n)
            .filter(|e| *e <= self.b.len())
            .ok_or_else(|| err("truncated CBOR input"))?;
        let out = &self.b[self.i..end];
        self.i = end;
        Ok(out)
    }

    /// The head's argument for additional information `ai` (never 31).
    fn arg(&mut self, ai: u8) -> Result<u64, PipelineError> {
        Ok(match ai {
            0..=23 => ai as u64,
            24 => self.u8()? as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().expect("2 bytes")) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().expect("8 bytes")),
            _ => return Err(err("reserved additional information in CBOR head")),
        })
    }

    /// Definite length, or None for indefinite (`ai` 31).
    fn len(&mut self, ai: u8) -> Result<Option<u64>, PipelineError> {
        if ai == 31 {
            Ok(None)
        } else {
            self.arg(ai).map(Some)
        }
    }

    /// Consume a break byte if one is next.
    fn eat_break(&mut self) -> Result<bool, PipelineError> {
        if self.b.get(self.i) == Some(&0xff) {
            self.i += 1;
            return Ok(true);
        }
        if self.i >= self.b.len() {
            return Err(err("truncated CBOR input"));
        }
        Ok(false)
    }

    /// A byte/text string body (`major` 2 or 3): definite, or
    /// indefinite as a concatenation of definite same-major chunks.
    fn string_body(&mut self, major: u8, ai: u8) -> Result<Vec<u8>, PipelineError> {
        match self.len(ai)? {
            Some(n) => Ok(self.take(n)?.to_vec()),
            None => {
                let mut out = Vec::new();
                loop {
                    let ib = self.u8()?;
                    if ib == 0xff {
                        return Ok(out);
                    }
                    if ib >> 5 != major {
                        return Err(err("indefinite-length string with mixed chunk types"));
                    }
                    let n = self
                        .len(ib & 0x1f)?
                        .ok_or_else(|| err("nested indefinite-length string chunk"))?;
                    out.extend_from_slice(self.take(n)?);
                }
            }
        }
    }

    /// The byte-string content of a bignum tag.
    fn bignum_bytes(&mut self) -> Result<Vec<u8>, PipelineError> {
        let ib = self.u8()?;
        if ib >> 5 != 2 {
            return Err(err("bignum tag content must be a byte string"));
        }
        self.string_body(2, ib & 0x1f)
    }

    fn map_pair(
        &mut self,
        depth: usize,
        members: &mut Vec<(String, Val)>,
        seen: &mut std::collections::BTreeSet<String>,
    ) -> Result<(), PipelineError> {
        // A type-tagged discriminator distinguishes a genuine same-type
        // duplicate (RFC 8949 §5.6) from a cross-type stringification
        // collision (int 1 vs text "1"), which are distinct CBOR keys
        // but both coerce to the text key kaiv objects require. Both
        // checks live in one set (typed `s:`/`n:`/`b:` entries plus a
        // coerced `c:` entry) so the scan stays O(log n) per key.
        let (typed, key) = match self.item(depth + 1)? {
            Val::Str(s) => (format!("s:{s}"), s),
            Val::Num(n) => (format!("n:{n}"), n),
            Val::Bool(b) => (format!("b:{b}"), b.to_string()),
            _ => return Err(err("unsupported CBOR map key type")),
        };
        if !seen.insert(typed) {
            return Err(err(format!("duplicate map key: {key}")));
        }
        if !seen.insert(format!("c:{key}")) {
            return Err(err(format!(
                "CBOR keys collide after text coercion (kaiv object keys are text): {key}"
            )));
        }
        let v = self.item(depth + 1)?;
        members.push((key, v));
        Ok(())
    }

    fn item(&mut self, depth: usize) -> Result<Val, PipelineError> {
        if depth > MAX_DEPTH {
            return Err(err("CBOR nesting too deep"));
        }
        let ib = self.u8()?;
        let (major, ai) = (ib >> 5, ib & 0x1f);
        Ok(match major {
            0 => Val::Num(self.arg(ai)?.to_string()),
            1 => Val::Num((-1i128 - self.arg(ai)? as i128).to_string()),
            2 => Val::Typed {
                lib: "std/enc".to_string(),
                name: "bin".to_string(),
                text: json::b64url_encode(&self.string_body(2, ai)?),
            },
            3 => Val::Str(
                String::from_utf8(self.string_body(3, ai)?)
                    .map_err(|_| err("CBOR text string is not UTF-8"))?,
            ),
            4 => {
                let mut items = Vec::new();
                match self.len(ai)? {
                    Some(n) => {
                        for _ in 0..n {
                            items.push(self.item(depth + 1)?);
                        }
                    }
                    None => {
                        while !self.eat_break()? {
                            items.push(self.item(depth + 1)?);
                        }
                    }
                }
                Val::Arr(items)
            }
            5 => {
                let mut members = Vec::new();
                let mut seen = std::collections::BTreeSet::new();
                match self.len(ai)? {
                    Some(n) => {
                        for _ in 0..n {
                            self.map_pair(depth, &mut members, &mut seen)?;
                        }
                    }
                    None => {
                        while !self.eat_break()? {
                            self.map_pair(depth, &mut members, &mut seen)?;
                        }
                    }
                }
                Val::Obj(members)
            }
            6 => match self.arg(ai)? {
                // Standard datetime string.
                0 => match self.item(depth + 1)? {
                    Val::Str(s) => Val::Typed {
                        lib: "std/time".to_string(),
                        name: "datetime".to_string(),
                        text: s,
                    },
                    _ => return Err(err("tag 0 content must be a text string")),
                },
                // Unsigned/negative bignum: exact decimal tokens.
                2 => Val::Num(be_bytes_to_decimal(&self.bignum_bytes()?)),
                3 => {
                    let mut bytes = self.bignum_bytes()?;
                    increment(&mut bytes);
                    Val::Num(format!("-{}", be_bytes_to_decimal(&bytes)))
                }
                // Any other tag is dropped; the item imports as itself.
                _ => self.item(depth + 1)?,
            },
            7 => match ai {
                20 => Val::Bool(false),
                21 => Val::Bool(true),
                // undefined converges on null.
                22 | 23 => Val::Null,
                25 => float_val(f16_to_f64(u16::from_be_bytes(
                    self.take(2)?.try_into().expect("2 bytes"),
                ))),
                26 => {
                    float_val(f32::from_be_bytes(self.take(4)?.try_into().expect("4 bytes")) as f64)
                }
                27 => float_val(f64::from_be_bytes(
                    self.take(8)?.try_into().expect("8 bytes"),
                )),
                31 => return Err(err("unexpected break code")),
                _ => {
                    if ai == 24 {
                        self.u8()?;
                    }
                    return Err(err("unsupported CBOR simple value"));
                }
            },
            _ => unreachable!("major is 3 bits"),
        })
    }
}

fn f16_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1f64 } else { 1f64 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let man = (h & 0x3ff) as f64;
    match exp {
        0 => sign * man * (-24f64).exp2(),
        0x1f => {
            if man == 0.0 {
                sign * f64::INFINITY
            } else {
                f64::NAN
            }
        }
        _ => sign * (1.0 + man / 1024.0) * f64::from(exp - 15).exp2(),
    }
}

// --------------------------------------------------------------- export

pub fn export(canonical: &str) -> Result<Vec<u8>, PipelineError> {
    let root = node_to_val(&json::tree(canonical)?)?;
    let mut out = Vec::new();
    emit(&root, &mut out, 0)?;
    Ok(out)
}

/// Minimal-width head (RFC 8949 preferred serialization).
fn head(major: u8, v: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    if v < 24 {
        out.push(m | v as u8);
    } else if v <= 0xff {
        out.push(m | 24);
        out.push(v as u8);
    } else if v <= 0xffff {
        out.push(m | 25);
        out.extend((v as u16).to_be_bytes());
    } else if v <= 0xffff_ffff {
        out.push(m | 26);
        out.extend((v as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend(v.to_be_bytes());
    }
}

fn emit(v: &Val, out: &mut Vec<u8>, depth: usize) -> Result<(), PipelineError> {
    if depth > MAX_DEPTH {
        return Err(err("nesting too deep for CBOR export"));
    }
    match v {
        Val::Null => out.push(0xf6),
        Val::Bool(false) => out.push(0xf4),
        Val::Bool(true) => out.push(0xf5),
        Val::Num(raw) => emit_num(raw, out)?,
        Val::Str(s) => {
            head(3, s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        Val::Typed { lib, name, text } if lib == "std/enc" && name == "bin" => {
            let b = json::b64url_decode(text).ok_or_else(|| err("invalid base64url payload"))?;
            head(2, b.len() as u64, out);
            out.extend_from_slice(&b);
        }
        // Full datetimes re-emit under tag 0; date/time/localdatetime
        // are not RFC 3339 date-times, so they stay plain text.
        Val::Typed { lib, name, text } if lib == "std/time" && name == "datetime" => {
            out.push(0xc0);
            head(3, text.len() as u64, out);
            out.extend_from_slice(text.as_bytes());
        }
        Val::Typed { lib, text, .. } if lib == "std/num" => {
            out.push(0xf9);
            out.extend(match text.as_str() {
                "inf" => [0x7c, 0x00],
                "-inf" => [0xfc, 0x00],
                _ => [0x7e, 0x00],
            });
        }
        Val::Typed { text, .. } => {
            head(3, text.len() as u64, out);
            out.extend_from_slice(text.as_bytes());
        }
        Val::Arr(items) => {
            head(4, items.len() as u64, out);
            for item in items {
                emit(item, out, depth + 1)?;
            }
        }
        Val::Obj(members) => {
            head(5, members.len() as u64, out);
            for (k, mv) in members {
                head(3, k.len() as u64, out);
                out.extend_from_slice(k.as_bytes());
                emit(mv, out, depth + 1)?;
            }
        }
    }
    Ok(())
}

/// A number token: integer-shaped tokens are exact — minimal-width
/// ints within ±2^64, bignums (tags 2/3) beyond; anything else is a
/// float, encoded at the shortest width that preserves the value.
fn emit_num(raw: &str, out: &mut Vec<u8>) -> Result<(), PipelineError> {
    let neg = raw.starts_with('-');
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    let int_shaped = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    if !int_shaped {
        let f: f64 = raw.parse().map_err(|_| err(format!("bad number: {raw}")))?;
        emit_float(f, out);
        return Ok(());
    }
    if let Ok(m) = digits.parse::<u128>() {
        if !neg && m <= u64::MAX as u128 {
            head(0, m as u64, out);
            return Ok(());
        }
        if neg && m == 0 {
            head(0, 0, out);
            return Ok(());
        }
        if neg && m <= u64::MAX as u128 + 1 {
            head(1, (m - 1) as u64, out);
            return Ok(());
        }
    }
    let mut bytes = decimal_to_be_bytes(digits);
    if neg {
        out.push(0xc3);
        decrement(&mut bytes);
        if bytes.first() == Some(&0) {
            bytes.remove(0);
        }
    } else {
        out.push(0xc2);
    }
    head(2, bytes.len() as u64, out);
    out.extend_from_slice(&bytes);
    Ok(())
}

fn emit_float(f: f64, out: &mut Vec<u8>) {
    if f.is_nan() {
        out.extend([0xf9, 0x7e, 0x00]);
    } else if let Some(h) = f64_to_f16(f) {
        out.push(0xf9);
        out.extend(h.to_be_bytes());
    } else {
        let s = f as f32;
        if f64::from(s) == f {
            out.push(0xfa);
            out.extend(s.to_bits().to_be_bytes());
        } else {
            out.push(0xfb);
            out.extend(f.to_bits().to_be_bytes());
        }
    }
}

/// The half-width encoding of `f` when it is exact (normals,
/// subnormals, ±0, ±inf); None otherwise.
fn f64_to_f16(f: f64) -> Option<u16> {
    let s = f as f32;
    if f64::from(s) != f {
        return None;
    }
    let bits = s.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let e = ((bits >> 23) & 0xff) as i32;
    let man = bits & 0x007f_ffff;
    if e == 0xff {
        return (man == 0).then_some(sign | 0x7c00); // ±inf
    }
    if man == 0 && e == 0 {
        return Some(sign); // ±0
    }
    if e == 0 {
        return None; // f32 subnormal: far below the f16 range
    }
    let exp = e - 127;
    if (-14..=15).contains(&exp) {
        (man & 0x1fff == 0).then(|| sign | (((exp + 15) as u16) << 10) | (man >> 13) as u16)
    } else if (-24..=-15).contains(&exp) {
        // f16 subnormal: an integer multiple of 2^-24.
        let full = 0x0080_0000u32 | man;
        let shift = 23 - (exp + 24) as u32;
        (full & ((1u32 << shift) - 1) == 0).then(|| sign | (full >> shift) as u16)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_type_key_coercion_reported_accurately() {
        // {1:"x","1":"y"} — distinct CBOR keys colliding as text keys.
        let bytes = [0xa2, 0x01, 0x61, 0x78, 0x61, 0x31, 0x61, 0x79];
        let e = format!("{}", import(&bytes).unwrap_err());
        assert!(e.contains("collide"), "{e}");
    }

    #[test]
    fn genuine_duplicate_key_still_errors() {
        // {1:x,1:y} — two int-1 keys.
        let bytes = [0xa2, 0x01, 0x61, 0x78, 0x01, 0x61, 0x79];
        let e = format!("{}", import(&bytes).unwrap_err());
        assert!(e.contains("duplicate map key"), "{e}");
    }

    fn roundtrip(src: &[u8]) -> Vec<u8> {
        let authored = import(src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        export(&daiv).unwrap()
    }

    /// `{"key": <item bytes>}` for single-member tests.
    fn doc(key: &str, item: &[u8]) -> Vec<u8> {
        let mut out = vec![0xa1, 0x60 | key.len() as u8];
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(item);
        out
    }

    #[test]
    fn import_typing_and_natives() {
        let mut src = vec![0xa5];
        for (k, v) in [
            ("host", &b"\x65web01"[..]),
            ("port", b"\x19\x1f\x90"),
            ("ratio", b"\xf9\x3e\x00"),
            ("on", b"\xf5"),
            ("note", b"\xf6"),
        ] {
            src.push(0x60 | k.len() as u8);
            src.extend_from_slice(k.as_bytes());
            src.extend_from_slice(v);
        }
        let out = import(&src).unwrap();
        assert!(out.contains("host=web01\n"));
        assert!(out.contains("!int\nport=8080\n"));
        assert!(out.contains("!float\nratio=1.5\n"));
        assert!(out.contains("!bool\non=true\n"));
        assert!(out.contains("!null\nnote=\n"));
        assert_eq!(roundtrip(&src), src);
    }

    #[test]
    fn byte_strings_are_std_enc_bin() {
        let src = doc("blob", &[0x43, 0x00, 0xff, 0x10]);
        let out = import(&src).unwrap();
        assert!(out.contains(".!types std/enc\n"));
        let b64 = json::b64url_encode(&[0x00, 0xff, 0x10]);
        assert!(out.contains(&format!("&bin\nblob={b64}\n")));
        assert_eq!(roundtrip(&src), src);
    }

    #[test]
    fn tag0_datetimes_are_std_time() {
        let mut item = vec![0xc0, 0x74];
        item.extend_from_slice(b"2026-07-03T21:00:00Z");
        let src = doc("when", &item);
        let out = import(&src).unwrap();
        assert!(out.contains(".!types std/time\n"));
        assert!(out.contains("&datetime\nwhen=2026-07-03T21:00:00Z\n"));
        assert_eq!(roundtrip(&src), src);
    }

    #[test]
    fn integers_exact_at_any_width() {
        let mut src = vec![0xa4];
        // max: u64::MAX; min: -2^64; big: 2^64 (tag 2); neg: -2^64-2
        // (tag 3, magnitude bytes 2^64+1).
        for (k, v) in [
            (
                "max",
                &[0x1b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff][..],
            ),
            (
                "min",
                &[0x3b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff],
            ),
            ("big", &[0xc2, 0x49, 1, 0, 0, 0, 0, 0, 0, 0, 0]),
            ("neg", &[0xc3, 0x49, 1, 0, 0, 0, 0, 0, 0, 0, 1]),
        ] {
            src.push(0x60 | k.len() as u8);
            src.extend_from_slice(k.as_bytes());
            src.extend_from_slice(v);
        }
        let out = import(&src).unwrap();
        assert!(out.contains("!int\nmax=18446744073709551615\n"));
        assert!(out.contains("!int\nmin=-18446744073709551616\n"));
        assert!(out.contains("!int\nbig=18446744073709551616\n"));
        assert!(out.contains("!int\nneg=-18446744073709551618\n"));
        assert_eq!(roundtrip(&src), src);
    }

    #[test]
    fn nonfinite_floats_are_std_num() {
        let mut src = vec![0xa3];
        for (k, v) in [
            ("a", [0xf9, 0x7c, 0x00]),
            ("b", [0xf9, 0xfc, 0x00]),
            ("c", [0xf9, 0x7e, 0x00]),
        ] {
            src.push(0x60 | k.len() as u8);
            src.extend_from_slice(k.as_bytes());
            src.extend_from_slice(&v);
        }
        let out = import(&src).unwrap();
        assert!(out.contains(".!types std/num\n"));
        assert!(out.contains("&inf\na=inf\n"));
        assert!(out.contains("&inf\nb=-inf\n"));
        assert!(out.contains("&nan\nc=nan\n"));
        assert_eq!(roundtrip(&src), src);
    }

    #[test]
    fn floats_reencode_shortest() {
        // 0.1 only fits f64; 2^24 fits f32; 1.5 fits f16.
        let src = doc("x", &{
            let mut v = vec![0xfb];
            v.extend(0.1f64.to_bits().to_be_bytes());
            v
        });
        assert_eq!(roundtrip(&src), src);
        let src = doc("x", &[0xfa, 0x4b, 0x80, 0x00, 0x00]);
        assert_eq!(roundtrip(&src), src);
        let src = doc("x", &[0xfb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0]); // 1.5 as f64
        assert_eq!(roundtrip(&src), doc("x", &[0xf9, 0x3e, 0x00]));
    }

    #[test]
    fn indefinite_lengths_normalize() {
        // {"s": (_ "a", "b"), "b": (_ h'01', h'0203'), "l": [_ 1, 2]}
        let src: Vec<u8> = vec![
            0xbf, // map, indefinite
            0x61, b's', 0x7f, 0x61, b'a', 0x61, b'b', 0xff, //
            0x61, b'b', 0x5f, 0x41, 0x01, 0x42, 0x02, 0x03, 0xff, //
            0x61, b'l', 0x9f, 0x01, 0x02, 0xff, //
            0xff,
        ];
        let out = import(&src).unwrap();
        assert!(out.contains("s=ab\n"));
        assert!(out.contains(&format!("&bin\nb={}\n", json::b64url_encode(&[1, 2, 3]))));
        assert!(out.contains("!int\n/@l;=1;2\n"));
        let definite: Vec<u8> = vec![
            0xa3, 0x61, b's', 0x62, b'a', b'b', 0x61, b'b', 0x43, 0x01, 0x02, 0x03, 0x61, b'l',
            0x82, 0x01, 0x02,
        ];
        assert_eq!(roundtrip(&src), definite);
    }

    #[test]
    fn scalar_keys_stringify() {
        let out = import(&[0xa1, 0x01, 0x02]).unwrap();
        assert!(out.contains("!int\n\"1\"=2\n"));
        // The key comes back as a text string: normalization, not
        // round-trip.
        assert_eq!(roundtrip(&[0xa1, 0x01, 0x02]), vec![0xa1, 0x61, b'1', 0x02]);
    }

    #[test]
    fn undefined_and_unknown_tags_degrade() {
        let out = import(&doc("u", &[0xf7])).unwrap();
        assert!(out.contains("!null\nu=\n"));
        // Tag 32 (URI) drops; the text string imports as itself.
        let out = import(&doc("u", &[0xd8, 0x20, 0x61, b'x'])).unwrap();
        assert!(out.contains("u=x\n"));
    }

    #[test]
    fn semantic_roundtrip() {
        let mut src = vec![0xa4];
        for (k, v) in [
            ("name", &b"\x63eu1"[..]),
            ("tags", b"\x82\x61a\x61b"),
            ("limits", b"\xa1\x63rps\x19\x01\xf4"),
            ("servers", b"\x82\xa1\x64host\x61a\xa1\x64host\x61b"),
        ] {
            src.push(0x60 | k.len() as u8);
            src.extend_from_slice(k.as_bytes());
            src.extend_from_slice(v);
        }
        assert_eq!(roundtrip(&src), src);
    }

    #[test]
    fn cross_format() {
        // JSON -> kaiv -> CBOR: preferred serialization bytes.
        let authored = crate::json::import(br#"{"a":[1,2],"b":"x"}"#).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        assert_eq!(
            export(&daiv).unwrap(),
            vec![0xa2, 0x61, b'a', 0x82, 0x01, 0x02, 0x61, b'b', 0x61, b'x']
        );
        // CBOR -> kaiv -> JSON: byte strings degrade to b64url text.
        let src = doc("blob", &[0x42, 0x01, 0xff]);
        let authored = import(&src).unwrap();
        let raiv = crate::compile(authored.as_bytes()).unwrap();
        let daiv = crate::denorm::denormalize(&raiv).unwrap();
        let json = crate::json::export(&daiv).unwrap();
        assert_eq!(json.trim_end(), r#"{"blob":"Af8"}"#);
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(import(&[0x01]).is_err()); // root not a map
        assert!(import(&[0xa0, 0x00]).is_err()); // trailing bytes
        assert!(import(&[0xa1, 0x61, b'a']).is_err()); // truncated
        assert!(import(&[0xff]).is_err()); // lone break
        assert!(import(&doc("k", &[0x1c])).is_err()); // reserved ai
        assert!(import(&doc("k", &[0xf8, 0xff])).is_err()); // simple 255
        assert!(import(&doc("k", &[0xf8])).is_err()); // truncated simple
                                                      // duplicate keys
        assert!(import(&[0xa2, 0x61, b'a', 0x01, 0x61, b'a', 0x02]).is_err());
        // invalid UTF-8 text string
        assert!(import(&doc("k", &[0x61, 0xff])).is_err());
        // map key is a container
        assert!(import(&[0xa1, 0x80, 0x01]).is_err());
    }
}
