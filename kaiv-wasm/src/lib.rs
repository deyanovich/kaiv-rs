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

/// A resolver seeded with host-supplied artifacts: three parallel
/// arrays of (lib, ext, text). The browser drives the resolve
/// loop — run, drain the misses from the envelope, fetch them
/// from the registries, retry with a longer preload list.
fn resolver_with(libs: Vec<String>, exts: Vec<String>, texts: Vec<String>) -> kaiv::Resolver {
    let r = kaiv::Resolver::offline();
    for ((lib, ext), text) in libs.iter().zip(exts.iter()).zip(texts.iter()) {
        r.preload(lib, ext, text.as_bytes().to_vec());
    }
    r
}

/// Error envelope carrying unresolved dependencies:
/// `{"ok":false,"error":…,"missing":[["lib","ext"],…]}`. An empty
/// `missing` means the error is final — no fetch will help.
fn err_missing(r: &kaiv::Resolver, e: impl std::fmt::Display) -> String {
    let missing = r.take_missing();
    if missing.is_empty() {
        return err(e);
    }
    let pairs: Vec<String> = missing
        .iter()
        .map(|(l, x)| format!("[{},{}]", quote(l), quote(x)))
        .collect();
    format!(
        "{{\"ok\":false,\"error\":{},\"missing\":[{}]}}",
        quote(&e.to_string()),
        pairs.join(",")
    )
}

/// `build` with host preloads and a missing-deps envelope: the
/// registry-resolving build loop's engine.
#[wasm_bindgen(js_name = buildR)]
pub fn build_r(input: &str, libs: Vec<String>, exts: Vec<String>, texts: Vec<String>) -> String {
    let r = resolver_with(libs, exts, texts);
    match kaiv::compile_with(input.as_bytes(), &r)
        .and_then(|raiv| kaiv::denorm::denormalize_with(&raiv, &r))
    {
        Ok(daiv) => ok(daiv),
        Err(e) => err_missing(&r, e),
    }
}

/// `compileSchema` with host preloads and the missing envelope.
#[wasm_bindgen(js_name = compileSchemaR)]
pub fn compile_schema_r(
    saiv: &str,
    libs: Vec<String>,
    exts: Vec<String>,
    texts: Vec<String>,
) -> String {
    let r = resolver_with(libs, exts, texts);
    match kaiv::compile_schema_with(saiv.as_bytes(), &r) {
        Ok(csaiv) => ok(csaiv),
        Err(e) => err_missing(&r, e),
    }
}

/// `infer` with host preloads and the missing envelope (the
/// example document may import registry types or units).
#[wasm_bindgen(js_name = inferR)]
pub fn infer_r(
    input: &str,
    name: &str,
    libs: Vec<String>,
    exts: Vec<String>,
    texts: Vec<String>,
) -> String {
    let r = resolver_with(libs, exts, texts);
    let daiv = match kaiv::compile_with(input.as_bytes(), &r)
        .and_then(|raiv| kaiv::denorm::denormalize_with(&raiv, &r))
    {
        Ok(d) => d,
        Err(e) => return err_missing(&r, e),
    };
    match kaiv::infer::infer(&daiv, name) {
        Ok(s) => ok(s),
        Err(e) => err_missing(&r, e),
    }
}

/// `validate` with host preloads and the missing envelope: the
/// schema compiles through the same resolver, so registry types,
/// units, and inherited schemas all participate.
#[wasm_bindgen(js_name = validateR)]
pub fn validate_r(
    daiv: &str,
    saiv: &str,
    libs: Vec<String>,
    exts: Vec<String>,
    texts: Vec<String>,
) -> String {
    let r = resolver_with(libs, exts, texts);
    let csaiv = match kaiv::compile_schema_with(saiv.as_bytes(), &r) {
        Ok(c) => c,
        Err(e) => return err_missing(&r, format!("schema: {e}")),
    };
    let compiled = match kaiv::parse_csaiv(&csaiv) {
        Ok(c) => c,
        Err(e) => return err(format!("schema: {e}")),
    };
    match kaiv::validate(daiv, &compiled) {
        Ok(()) => ok("pass".into()),
        Err(e) => err_missing(&r, e),
    }
}

/// Compile an authored `.saiv` to its `.csaiv` — the form the
/// registry gate byte-checks against a sibling deposit and the
/// validator resolves (offline: `.!types`/`.!units` beyond the
/// embedded std libraries do not resolve here).
#[wasm_bindgen(js_name = compileSchema)]
pub fn compile_schema(saiv: &str) -> String {
    let r = kaiv::Resolver::offline();
    match kaiv::compile_schema_with(saiv.as_bytes(), &r) {
        Ok(csaiv) => ok(csaiv),
        Err(e) => err(e),
    }
}

/// BLAKE3 hex of the input — kdaiv's content-address for
/// `.kaiv`/`.daiv` deposits.
#[wasm_bindgen(js_name = b3hex)]
pub fn b3hex(input: &str) -> String {
    ok(blake3::hash(input.as_bytes()).to_hex().to_string())
}

/// Format an authored `.kaiv` stream with the standard formatter
/// (`kaiv fmt`): grouped namespace blocks, idiomatic sugar.
#[wasm_bindgen]
pub fn fmt(input: &str) -> String {
    match kaiv::format_data(input) {
        Ok(t) => ok(t),
        Err(e) => err(e),
    }
}

/// Canonical `.daiv`/`.raiv` -> idiomatic authored `.kaiv`: the
/// inverse direction of `build`, and a view — sugar the compiler
/// resolved away (comments, variables, references) does not come
/// back; `.raiv` preserves more, authored units included.
#[wasm_bindgen]
pub fn unbuild(input: &str) -> String {
    match kaiv::unbuild(input) {
        Ok(t) => ok(t),
        Err(e) => err(e),
    }
}

/// Authored `.kaiv` -> canonical `.daiv`, with an in-memory
/// `.saiv` preloaded into the resolver so a `.!schema:` reference
/// resolves without a registry — the playground's schema pane fed
/// straight to the pipeline (schema-declared defaults materialize,
/// required-field checks apply).
#[wasm_bindgen(js_name = buildWith)]
pub fn build_with(input: &str, schema_name: &str, saiv: &str) -> String {
    let r = kaiv::Resolver::offline();
    if !schema_name.is_empty() && !saiv.is_empty() {
        // Validation resolves the compiled form; inheritance the
        // source form. Feed both from the one authored schema.
        r.preload(schema_name, "saiv", saiv.as_bytes().to_vec());
        match kaiv::compile_schema_with(saiv.as_bytes(), &r) {
            Ok(csaiv) => r.preload(schema_name, "csaiv", csaiv.into_bytes()),
            Err(e) => return err(e),
        }
    }
    match kaiv::compile_with(input.as_bytes(), &r)
        .and_then(|raiv| kaiv::denorm::denormalize_with(&raiv, &r))
    {
        Ok(daiv) => ok(daiv),
        Err(e) => err(e),
    }
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
