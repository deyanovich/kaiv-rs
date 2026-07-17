//! Browser playground entry point: the kaiv pipeline, converters,
//! and validator over pasted text, entirely client-side.
//!
//! Every function returns a JSON envelope and never throws:
//! `{"ok":true,"output":"..."}` or `{"ok":false,"error":"..."}`.
//! Binary export formats (cbor, avro, asn1) return the bytes as a
//! hex dump in `output` plus base64url in `b64`. Registry
//! resolution is the offline resolver: the six embedded std
//! libraries resolve, anything else reports its normal
//! resolution error.

use wasm_bindgen::prelude::*;

fn ok(output: String) -> String {
    serde_none(&[("ok", "true".into()), ("output", quote(&output))])
}
fn ok_bin(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes
        .chunks(16)
        .enumerate()
        .map(|(i, row)| {
            let hexes: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
            let ascii: String = row
                .iter()
                .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                .collect();
            format!("{:08x}: {:<47} {}", i * 16, hexes.join(" "), ascii)
        })
        .collect();
    let b64 = b64url(bytes);
    serde_none(&[
        ("ok", "true".into()),
        ("output", quote(&hex.join("\n"))),
        ("b64", quote(&b64)),
    ])
}
fn err(msg: impl std::fmt::Display) -> String {
    serde_none(&[("ok", "false".into()), ("error", quote(&msg.to_string()))])
}

/// Minimal JSON object writer (the crate is dependency-light on
/// purpose; the envelope is flat strings).
fn serde_none(fields: &[(&str, String)]) -> String {
    let body: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("\"{k}\":{v}"))
        .collect();
    format!("{{{}}}", body.join(","))
}
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn b64url(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(A[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(A[n as usize & 63] as char);
        }
    }
    out
}

/// The toolchain version shown in the playground footer.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Authored `.kaiv` -> canonical `.daiv` (compile + denorm).
#[wasm_bindgen]
pub fn build(input: &str) -> String {
    let r = kaiv::Resolver::offline();
    match kaiv::compile_with(input.as_bytes(), &r).and_then(|raiv| kaiv::denorm::denormalize_with(&raiv, &r))
    {
        Ok(daiv) => ok(daiv),
        Err(e) => err(e),
    }
}

/// Foreign text format -> authored `.kaiv`.
/// Formats: json | yaml | toml | xml.
#[wasm_bindgen(js_name = importFrom)]
pub fn import(format: &str, input: &str) -> String {
    let bytes = input.as_bytes();
    let res = match format {
        "json" => kaiv::json::import(bytes),
        "yaml" => kaiv::yaml::import(bytes),
        "toml" => kaiv::toml::import(bytes),
        "xml" => kaiv::xml::import(bytes),
        other => return err(format!("unsupported import format: {other}")),
    };
    match res {
        Ok(k) => ok(k),
        Err(e) => err(e),
    }
}

/// Canonical `.daiv` -> foreign format. Text formats return text;
/// cbor/avro/asn1 return a hex dump plus base64url.
#[wasm_bindgen(js_name = exportTo)]
pub fn export(format: &str, daiv: &str) -> String {
    match format {
        "json" => kaiv::json::export(daiv).map(ok).unwrap_or_else(err),
        "yaml" => kaiv::yaml::export(daiv).map(ok).unwrap_or_else(err),
        "toml" => kaiv::toml::export(daiv).map(ok).unwrap_or_else(err),
        "xml" => kaiv::xml::export(daiv).map(ok).unwrap_or_else(err),
        "cbor" => kaiv::cbor::export(daiv).map(|b| ok_bin(&b)).unwrap_or_else(err),
        "avro" => kaiv::avro::export(daiv).map(|b| ok_bin(&b)).unwrap_or_else(err),
        "asn1" => kaiv::asn1::export(daiv).map(|b| ok_bin(&b)).unwrap_or_else(err),
        other => err(format!("unsupported export format: {other}")),
    }
}

/// Validate canonical data against an authored `.saiv` schema.
#[wasm_bindgen]
pub fn validate(daiv: &str, saiv: &str) -> String {
    let r = kaiv::Resolver::offline();
    let csaiv = match kaiv::compile_schema_with(saiv.as_bytes(), &r) {
        Ok(c) => c,
        Err(e) => return err(format!("schema: {e}")),
    };
    let compiled = match kaiv::parse_csaiv(&csaiv) {
        Ok(c) => c,
        Err(e) => return err(format!("schema: {e}")),
    };
    match kaiv::validate(daiv, &compiled) {
        Ok(()) => ok("pass".into()),
        Err(e) => err(e),
    }
}

/// Infer an authored `.saiv` schema from a `.kaiv`/`.daiv` document.
#[wasm_bindgen]
pub fn infer(input: &str, name: &str) -> String {
    let r = kaiv::Resolver::offline();
    let daiv = match kaiv::compile_with(input.as_bytes(), &r)
        .and_then(|raiv| kaiv::denorm::denormalize_with(&raiv, &r))
    {
        Ok(d) => d,
        Err(e) => return err(e),
    };
    match kaiv::infer::infer(&daiv, name) {
        Ok(s) => ok(s),
        Err(e) => err(e),
    }
}

/// Foreign schema -> authored `.saiv` (sound weakening).
/// Formats: jsonschema | xsd | proto | avsc | graphql.
/// `message` picks the root when the schema declares several
/// (empty = sole/default).
#[wasm_bindgen(js_name = importSchema)]
pub fn import_schema(format: &str, input: &str, name: &str, message: &str) -> String {
    let bytes = input.as_bytes();
    let msg = if message.is_empty() { None } else { Some(message) };
    let res = match format {
        "jsonschema" => kaiv::jsonschema::import(bytes, name),
        "xsd" => kaiv::xsd::import_schema(bytes, msg, name),
        "proto" => kaiv::proto::import_schema(input, msg, name),
        "avsc" => kaiv::avro::import_schema(bytes, name),
        "graphql" => kaiv::graphql::import_schema(bytes, msg, name),
        other => return err(format!("unsupported schema format: {other}")),
    };
    match res {
        Ok(s) => ok(s),
        Err(e) => err(e),
    }
}
