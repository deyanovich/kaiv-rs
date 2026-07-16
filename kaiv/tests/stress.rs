//! Property-based stress tests over the whole converter matrix.
//!
//! Deterministic (seeded xorshift, no wall clock); the iteration
//! count scales with `KAIV_STRESS_ITERS` (default keeps `cargo test`
//! fast; set it to a few thousand for a heavy run). Properties:
//!
//! 1. Whatever a converter exports, its importer accepts.
//! 2. Export → import → export is byte-stable (one conversion lands
//!    in the format's normal form).
//! 3. Random conversion chains converge (the type lattice only
//!    degrades, so repeated application must reach a fixed point).
//! 4. For generated .proto schemas, wire-decoded documents validate
//!    against the schema-converted `.saiv` (data/schema agreement).
//! 5. `infer` produces a schema its example document validates
//!    against, for arbitrary documents.
//! 6. No importer panics on mutated or garbage input.
#![cfg(all(
    feature = "yaml",
    feature = "toml",
    feature = "xml",
    feature = "cbor",
    feature = "avro",
    feature = "proto",
    feature = "asn1"
))]

use std::panic::{catch_unwind, AssertUnwindSafe};

fn iters(base: u64) -> u64 {
    std::env::var("KAIV_STRESS_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(base)
}

// ------------------------------------------------------------------ rng

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

// ------------------------------------------------------ JSON generators

const STRINGS: &[&str] = &[
    "",
    "plain",
    "with space",
    "line1\nline2",
    "tab\there",
    "quote\"inside",
    "back\\slash",
    "unicode é 日本 😀",
    "$deref",
    "trailing ",
    " leading",
    "null",
    "true",
    "123",
    "1e5",
    "2026-07-05T10:00:00Z",
    "a;b|c=d",
    "<tag>&amp;</tag>",
    "-----BEGIN X-----",
    "yes",
    "~tilde",
    "#comment",
];

const INTS: &[&str] = &[
    "0",
    "1",
    "-1",
    "42",
    "2147483647",
    "-2147483648",
    "9223372036854775807",
    "-9223372036854775808",
    "18446744073709551615",
    "18446744073709551616",
    "-18446744073709551616",
    "999999999999999999999999999999",
];

const FLOATS: &[&str] = &[
    "0.1",
    "1.5",
    "-2.75",
    "1e10",
    "1e-7",
    "3.141592653589793",
    "100.0",
    "-0.5",
    "1e300",
    "2.5e-300",
    "1e309",
];

const KEYS: &[&str] = &[
    "name", "port", "value", "data", "x", "y1", "item", "config", "host", "on", "_id",
];

const WEIRD_KEYS: &[&str] = &[
    "a b", "x:y", "@attr", "#text", "re", "min", "max", "1key", "ключ", "k\"q", "a/b", "a=b",
];

fn json_string(s: &str, out: &mut String) {
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
}

fn gen_key(r: &mut Rng, out: &mut String) {
    if r.chance(12) {
        json_string(r.pick(WEIRD_KEYS), out);
    } else {
        let base = r.pick(KEYS);
        if r.chance(50) {
            json_string(&format!("{base}{}", r.below(100)), out);
        } else {
            json_string(base, out);
        }
    }
}

fn gen_value(r: &mut Rng, depth: u64, out: &mut String) {
    let top = if depth >= 4 { 6 } else { 9 };
    match r.below(top) {
        0 => out.push_str("null"),
        1 => out.push_str(if r.chance(50) { "true" } else { "false" }),
        2 => out.push_str(r.pick(INTS)),
        3 => out.push_str(r.pick(FLOATS)),
        4 | 5 => json_string(r.pick(STRINGS), out),
        6 | 7 => gen_object(r, depth + 1, out),
        _ => {
            out.push('[');
            let n = r.below(5);
            for i in 0..n {
                if i > 0 {
                    out.push(',');
                }
                gen_value(r, depth + 1, out);
            }
            out.push(']');
        }
    }
}

fn gen_object(r: &mut Rng, depth: u64, out: &mut String) {
    out.push('{');
    let n = if depth == 0 {
        1 + r.below(6)
    } else {
        r.below(5)
    };
    let mut keys = Vec::new();
    let mut first = true;
    for _ in 0..n {
        let mut k = String::new();
        gen_key(r, &mut k);
        if keys.contains(&k) {
            continue;
        }
        keys.push(k.clone());
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(&k);
        out.push(':');
        gen_value(r, depth, out);
    }
    out.push('}');
}

fn gen_doc(r: &mut Rng) -> String {
    let mut out = String::new();
    gen_object(r, 0, &mut out);
    out
}

// ----------------------------------------------------- format dispatch

const FORMATS: &[&str] = &["json", "yaml", "toml", "xml", "cbor", "avro", "asn1"];

fn fmt_export(fmt: &str, daiv: &str) -> Result<Vec<u8>, String> {
    let text = |r: Result<String, kaiv::PipelineError>| r.map(String::into_bytes);
    match fmt {
        "json" => text(kaiv::json::export(daiv)),
        "yaml" => text(kaiv::yaml::export(daiv)),
        "toml" => text(kaiv::toml::export(daiv)),
        "xml" => text(kaiv::xml::export(daiv)),
        "cbor" => kaiv::cbor::export(daiv),
        "avro" => kaiv::avro::export(daiv),
        "asn1" => kaiv::asn1::export(daiv),
        _ => unreachable!(),
    }
    .map_err(|e| e.to_string())
}

fn fmt_import(fmt: &str, data: &[u8]) -> Result<String, String> {
    match fmt {
        "json" => kaiv::json::import(data),
        "yaml" => kaiv::yaml::import(data),
        "toml" => kaiv::toml::import(data),
        "xml" => kaiv::xml::import(data),
        "cbor" => kaiv::cbor::import(data),
        "avro" => kaiv::avro::import(data),
        "asn1" => kaiv::asn1::import(data),
        _ => unreachable!(),
    }
    .map_err(|e| e.to_string())
}

fn build(authored: &str) -> Result<String, String> {
    let raiv = kaiv::compile(authored.as_bytes()).map_err(|e| e.to_string())?;
    kaiv::denormalize(&raiv).map_err(|e| e.to_string())
}

/// Import a random JSON doc into canonical form (None when the doc is
/// legitimately unrepresentable, e.g. an empty key).
fn daiv_of(doc: &str) -> Option<String> {
    let authored = kaiv::json::import(doc.as_bytes()).ok()?;
    Some(build(&authored).unwrap_or_else(|e| {
        panic!("authored output of the JSON importer failed to compile: {e}\ndoc: {doc}\nauthored: {authored}")
    }))
}

// ------------------------------------------------------------ property 1+2

#[test]
fn export_reimports_and_is_stable() {
    let n = iters(60);
    let (mut converted, mut rejected) = (0u64, 0u64);
    for seed in 0..n {
        let mut r = Rng::new(seed);
        let doc = gen_doc(&mut r);
        let Some(daiv) = daiv_of(&doc) else {
            rejected += 1;
            continue;
        };
        for fmt in FORMATS {
            let b1 = match fmt_export(fmt, &daiv) {
                Ok(b) => b,
                Err(_) => {
                    rejected += 1;
                    continue; // documented unrepresentability
                }
            };
            // Property 1: an importer accepts its exporter's output.
            let a2 = fmt_import(fmt, &b1).unwrap_or_else(|e| {
                panic!(
                    "seed {seed} {fmt}: own export failed to reimport: {e}\ndoc: {doc}\nexport: {:?}",
                    String::from_utf8_lossy(&b1)
                )
            });
            let d2 = build(&a2).unwrap_or_else(|e| {
                panic!("seed {seed} {fmt}: reimported doc failed to compile: {e}\ndoc: {doc}")
            });
            // Property 2: one round trip reaches the format's normal
            // form (the first export may still normalize once — XML
            // trims text whitespace, for example — but the second
            // must be a fixed point).
            let b2 = fmt_export(fmt, &d2)
                .unwrap_or_else(|e| panic!("seed {seed} {fmt}: re-export failed: {e}\ndoc: {doc}"));
            let a3 = fmt_import(fmt, &b2).unwrap_or_else(|e| {
                panic!("seed {seed} {fmt}: second reimport failed: {e}\ndoc: {doc}")
            });
            let d3 = build(&a3).unwrap();
            let b3 = fmt_export(fmt, &d3).unwrap();
            assert_eq!(
                b2,
                b3,
                "seed {seed} {fmt}: export not stable\ndoc: {doc}\nsecond: {:?}\nthird:  {:?}",
                String::from_utf8_lossy(&b2),
                String::from_utf8_lossy(&b3)
            );
            converted += 1;
        }
    }
    eprintln!("stability: {converted} conversions stable, {rejected} clean rejections");
}

// ------------------------------------------------------------ property 3

#[test]
fn random_chains_converge() {
    let n = iters(40);
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0xc4a1);
        let doc = gen_doc(&mut r);
        let Some(d0) = daiv_of(&doc) else { continue };
        let chain: Vec<&str> = (0..3).map(|_| r.pick(FORMATS)).collect();
        let apply = |daiv: &str| -> String {
            let mut cur = daiv.to_string();
            for fmt in &chain {
                let Ok(bytes) = fmt_export(fmt, &cur) else {
                    continue; // unrepresentable hop: skip, keep going
                };
                let authored = fmt_import(fmt, &bytes).unwrap_or_else(|e| {
                    panic!("seed {seed} chain {chain:?} {fmt}: reimport failed: {e}\ndoc: {doc}")
                });
                cur = build(&authored).unwrap_or_else(|e| {
                    panic!("seed {seed} chain {chain:?} {fmt}: compile failed: {e}")
                });
            }
            cur
        };
        let d1 = apply(&d0);
        let d2 = apply(&d1);
        let d3 = apply(&d2);
        assert_eq!(
            fmt_export("json", &d2).ok(),
            fmt_export("json", &d3).ok(),
            "seed {seed}: chain {chain:?} did not converge\ndoc: {doc}\nafter 2: {d2}\nafter 3: {d3}"
        );
    }
}

/// YAML holds everything the JSON model holds, so its round trip
/// must preserve the document exactly — including strings shaped
/// like booleans or numbers, which the emitter must quote.
#[test]
fn yaml_round_trip_is_lossless() {
    let n = iters(80);
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0x7a31);
        let doc = gen_doc(&mut r);
        let Some(d0) = daiv_of(&doc) else { continue };
        let j0 = fmt_export("json", &d0).unwrap();
        let y = fmt_export("yaml", &d0)
            .unwrap_or_else(|e| panic!("seed {seed}: yaml export failed: {e}\ndoc: {doc}"));
        let a2 = fmt_import("yaml", &y).unwrap_or_else(|e| {
            panic!(
                "seed {seed}: yaml reimport failed: {e}\ndoc: {doc}\nyaml:\n{}",
                String::from_utf8_lossy(&y)
            )
        });
        let d2 = build(&a2).unwrap();
        let j2 = fmt_export("json", &d2).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&j0),
            String::from_utf8_lossy(&j2),
            "seed {seed}: yaml round trip lost data\ndoc: {doc}\nyaml:\n{}",
            String::from_utf8_lossy(&y)
        );
    }
}

// ------------------------------------------------------------ property 4

/// Strings that stay flat scalar lines in authored kaiv. Non-flat
/// strings (EOL/NUL, leading `$`) ride the `std/enc/json` embed
/// channel, which schema converters' string fields do not admit — a
/// DOCUMENTED LIMITATION of the converted-schema contract (see the
/// jsonschema module doc): a multiline proto string decodes as
/// `!std/enc/json` and fails `str`-typed validation unless the field
/// is hand-widened to `!str|std/enc/json`. The schema/data agreement
/// property therefore tests flat strings only.
const FLAT_STRINGS: &[&str] = &[
    "",
    "plain",
    "with space",
    "unicode é 日本 😀",
    "null",
    "123",
    "a;b|c=d",
    "<tag>&amp;</tag>",
    "trailing ",
];

/// A random .proto message plus a JSON document shaped to fit it.
fn gen_proto_pair(r: &mut Rng) -> (String, String) {
    let mut schema = String::from("syntax = \"proto3\";\nmessage Top {\n");
    let mut nested = String::new();
    let mut doc = String::from("{");
    let mut first = true;
    let nfields = 1 + r.below(7);
    for i in 0..nfields {
        let fname = format!("f{i}");
        let mut field = |ty: &str, value: String, schema: &mut String, doc: &mut String| {
            schema.push_str(&format!("  {ty} {fname} = {};\n", i + 1));
            if !first {
                doc.push(',');
            }
            first = false;
            doc.push_str(&format!("\"{fname}\":{value}"));
        };
        match r.below(12) {
            0 => field(
                "int32",
                format!("{}", r.next() as i32),
                &mut schema,
                &mut doc,
            ),
            1 => field(
                "int64",
                format!("{}", r.next() as i64),
                &mut schema,
                &mut doc,
            ),
            2 => field(
                "uint32",
                format!("{}", r.next() as u32),
                &mut schema,
                &mut doc,
            ),
            3 => field("uint64", format!("{}", r.next()), &mut schema, &mut doc),
            4 => field(
                "sint64",
                format!("{}", r.next() as i64),
                &mut schema,
                &mut doc,
            ),
            5 => field(
                "fixed32",
                format!("{}", r.next() as u32),
                &mut schema,
                &mut doc,
            ),
            6 => field("bool", (r.chance(50)).to_string(), &mut schema, &mut doc),
            7 => {
                let mut s = String::new();
                json_string(r.pick(FLAT_STRINGS), &mut s);
                field("string", s, &mut schema, &mut doc);
            }
            8 => field(
                "double",
                r.pick(&["0.5", "1.5", "-2.25", "100.0"]).to_string(),
                &mut schema,
                &mut doc,
            ),
            9 => {
                // Enum with a couple of symbols; value picks one.
                let ename = format!("E{i}");
                nested.push_str(&format!("  enum {ename} {{ A{i} = 0; B{i} = 1; }}\n"));
                field(
                    &ename,
                    format!("\"{}{i}\"", if r.chance(50) { "A" } else { "B" }),
                    &mut schema,
                    &mut doc,
                );
            }
            10 => {
                // Repeated int64.
                let m = r.below(4);
                let vals: Vec<String> = (0..m).map(|_| (r.next() as i64).to_string()).collect();
                field(
                    "repeated int64",
                    format!("[{}]", vals.join(",")),
                    &mut schema,
                    &mut doc,
                );
            }
            _ => {
                // Nested message with one int field.
                let mname = format!("M{i}");
                nested.push_str(&format!("  message {mname} {{ int64 v = 1; }}\n"));
                field(
                    &mname,
                    format!("{{\"v\":{}}}", r.next() as i64),
                    &mut schema,
                    &mut doc,
                );
            }
        }
    }
    schema.push_str(&nested);
    schema.push_str("}\n");
    doc.push('}');
    (schema, doc)
}

#[test]
fn proto_data_validates_against_converted_schema() {
    let n = iters(60);
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0x9670);
        let (schema, doc) = gen_proto_pair(&mut r);
        let ctx = || format!("seed {seed}\nschema:\n{schema}\ndoc: {doc}");
        let daiv = daiv_of(&doc).unwrap_or_else(|| panic!("doc rejected: {}", ctx()));
        let wire = kaiv::proto::export(&daiv, &schema, None)
            .unwrap_or_else(|e| panic!("proto export failed: {e}\n{}", ctx()));
        let back = kaiv::proto::import(&wire, &schema, None)
            .unwrap_or_else(|e| panic!("proto reimport failed: {e}\n{}", ctx()));
        let d2 = build(&back).unwrap_or_else(|e| panic!("compile failed: {e}\n{}", ctx()));
        // Wire stability.
        let wire2 = kaiv::proto::export(&d2, &schema, None)
            .unwrap_or_else(|e| panic!("re-export failed: {e}\n{}", ctx()));
        assert_eq!(wire, wire2, "wire not stable\n{}", ctx());
        // Data/schema agreement: the decoded document validates
        // against the schema-converted .saiv.
        let saiv = kaiv::proto::import_schema(&schema, None, "stress/top")
            .unwrap_or_else(|e| panic!("schema convert failed: {e}\n{}", ctx()));
        let csaiv = kaiv::compile_schema(saiv.as_bytes()).unwrap_or_else(|e| {
            panic!(
                "converted schema failed to compile: {e}\n{}\nsaiv:\n{saiv}",
                ctx()
            )
        });
        let sc = kaiv::parse_csaiv(&csaiv).unwrap();
        if let Err(e) = kaiv::validate(&d2, &sc) {
            panic!(
                "wire-decoded document does not validate: {e:?}\n{}\nsaiv:\n{saiv}\ndaiv:\n{d2}",
                ctx()
            );
        }
    }
}

// ------------------------------------------------------------ property 5

#[test]
fn inferred_schema_accepts_its_example() {
    let n = iters(80);
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0x1f3e);
        let doc = gen_doc(&mut r);
        let Some(daiv) = daiv_of(&doc) else { continue };
        let saiv = kaiv::infer::infer(&daiv, "stress").unwrap_or_else(|e| {
            panic!("seed {seed}: infer failed: {e}\ndoc: {doc}\ndaiv:\n{daiv}")
        });
        let csaiv = kaiv::compile_schema(saiv.as_bytes()).unwrap_or_else(|e| {
            panic!("seed {seed}: inferred schema failed to compile: {e}\ndoc: {doc}\nsaiv:\n{saiv}")
        });
        let sc = kaiv::parse_csaiv(&csaiv).unwrap();
        if let Err(e) = kaiv::validate(&daiv, &sc) {
            panic!(
                "seed {seed}: example does not validate against its inferred schema: {e:?}\ndoc: {doc}\nsaiv:\n{saiv}\ndaiv:\n{daiv}"
            );
        }
    }
}

// ------------------------------------------------------------ property 6

#[test]
fn importers_never_panic_on_mutated_input() {
    let n = iters(60);
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0xfa22);
        let doc = gen_doc(&mut r);
        let Some(daiv) = daiv_of(&doc) else { continue };
        for fmt in FORMATS {
            let Ok(mut bytes) = fmt_export(fmt, &daiv) else {
                continue;
            };
            // A few random mutations: flips, truncation, insertions.
            for _ in 0..1 + r.below(6) {
                if bytes.is_empty() {
                    break;
                }
                match r.below(4) {
                    0 => {
                        let i = r.below(bytes.len() as u64) as usize;
                        bytes[i] ^= (r.next() as u8) | 1;
                    }
                    1 => {
                        bytes.truncate(r.below(bytes.len() as u64 + 1) as usize);
                    }
                    2 => {
                        let i = r.below(bytes.len() as u64 + 1) as usize;
                        bytes.insert(i, r.next() as u8);
                    }
                    _ => {
                        let i = r.below(bytes.len() as u64) as usize;
                        bytes[i] = r.next() as u8;
                    }
                }
            }
            let res = catch_unwind(AssertUnwindSafe(|| {
                let _ = fmt_import(fmt, &bytes);
            }));
            assert!(
                res.is_ok(),
                "seed {seed} {fmt}: importer PANICKED on mutated input: {bytes:?}"
            );
        }
        // Pure garbage too.
        let mut garbage = vec![0u8; (1 + r.below(64)) as usize];
        for b in garbage.iter_mut() {
            *b = r.next() as u8;
        }
        for fmt in FORMATS {
            let res = catch_unwind(AssertUnwindSafe(|| {
                let _ = fmt_import(fmt, &garbage);
            }));
            assert!(
                res.is_ok(),
                "seed {seed} {fmt}: importer PANICKED on garbage: {garbage:?}"
            );
        }
    }
}

#[test]
fn proto_import_never_panics_on_mutated_wire() {
    let n = iters(60);
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0x9915);
        let (schema, doc) = gen_proto_pair(&mut r);
        let Some(daiv) = daiv_of(&doc) else { continue };
        let Ok(mut bytes) = kaiv::proto::export(&daiv, &schema, None) else {
            continue;
        };
        for _ in 0..1 + r.below(6) {
            if bytes.is_empty() {
                break;
            }
            match r.below(4) {
                0 => {
                    let i = r.below(bytes.len() as u64) as usize;
                    bytes[i] ^= (r.next() as u8) | 1;
                }
                1 => bytes.truncate(r.below(bytes.len() as u64 + 1) as usize),
                2 => {
                    let i = r.below(bytes.len() as u64 + 1) as usize;
                    bytes.insert(i, r.next() as u8);
                }
                _ => {
                    let i = r.below(bytes.len() as u64) as usize;
                    bytes[i] = r.next() as u8;
                }
            }
        }
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = kaiv::proto::import(&bytes, &schema, None);
        }));
        assert!(
            res.is_ok(),
            "seed {seed}: proto import PANICKED on mutated wire: {bytes:?}"
        );
        let mut garbage = vec![0u8; (1 + r.below(64)) as usize];
        for b in garbage.iter_mut() {
            *b = r.next() as u8;
        }
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = kaiv::proto::import(&garbage, &schema, None);
        }));
        assert!(
            res.is_ok(),
            "seed {seed}: proto import PANICKED on garbage: {garbage:?}"
        );
    }
}

#[test]
fn core_pipeline_never_panics_on_garbage() {
    let n = iters(80);
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0x5c1d);
        // Random byte buffers (the CLI's untrusted-stdin surface).
        let mut garbage = vec![0u8; (1 + r.below(128)) as usize];
        for b in garbage.iter_mut() {
            *b = r.next() as u8;
        }
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = kaiv::lex(&garbage, kaiv::FileKind::Data);
            let _ = kaiv::compile(&garbage);
        }));
        assert!(
            res.is_ok(),
            "seed {seed}: core lex/compile PANICKED on garbage: {garbage:?}"
        );
        // Mutated valid .daiv.
        let doc = gen_doc(&mut r);
        if let Some(daiv) = daiv_of(&doc) {
            let mut bytes = daiv.into_bytes();
            for _ in 0..1 + r.below(6) {
                if bytes.is_empty() {
                    break;
                }
                let i = r.below(bytes.len() as u64) as usize;
                bytes[i] = r.next() as u8;
            }
            let res = catch_unwind(AssertUnwindSafe(|| {
                let _ = kaiv::lex(&bytes, kaiv::FileKind::Data);
                let _ = kaiv::compile(&bytes);
            }));
            assert!(
                res.is_ok(),
                "seed {seed}: core lex/compile PANICKED on mutated daiv: {bytes:?}"
            );
        }
    }
}

#[test]
fn schema_parsers_never_panic_on_mutated_input() {
    let n = iters(60);
    let proto_seed = "syntax = \"proto3\";\nmessage T { int32 a = 1; repeated string b = 2; map<string,int64> m = 3; oneof o { bool c = 4; } enum E { X = 0; } E e = 5; }";
    let sdl_seed = "\"d\" type T { a: Int! b: [String!] c: E } enum E { X Y } scalar S union U = T";
    let xsd_seed = "<xs:schema xmlns:xs=\"http://www.w3.org/2001/XMLSchema\"><xs:element name=\"t\"><xs:complexType><xs:sequence><xs:element name=\"a\" type=\"xs:int\"/></xs:sequence></xs:complexType></xs:element></xs:schema>";
    let avsc_seed = r#"{"type":"record","name":"t","fields":[{"name":"a","type":["null","long"]},{"name":"b","type":{"type":"array","items":"string"}}]}"#;
    let jsch_seed = r#"{"type":"object","required":["a"],"properties":{"a":{"type":"integer","minimum":0},"b":{"type":"array","items":{"type":"string"}}}}"#;
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0x5c47);
        for (which, base) in [
            ("proto", proto_seed),
            ("graphql", sdl_seed),
            ("xsd", xsd_seed),
            ("avro", avsc_seed),
            ("jsonschema", jsch_seed),
        ] {
            let mut bytes = base.as_bytes().to_vec();
            for _ in 0..1 + r.below(8) {
                if bytes.is_empty() {
                    break;
                }
                match r.below(3) {
                    0 => {
                        let i = r.below(bytes.len() as u64) as usize;
                        bytes[i] = r.next() as u8;
                    }
                    1 => {
                        bytes.truncate(r.below(bytes.len() as u64 + 1) as usize);
                    }
                    _ => {
                        let i = r.below(bytes.len() as u64 + 1) as usize;
                        bytes.insert(i, r.next() as u8);
                    }
                }
            }
            let res = catch_unwind(AssertUnwindSafe(|| match which {
                "proto" => {
                    if let Ok(t) = std::str::from_utf8(&bytes) {
                        let _ = kaiv::proto::import_schema(t, None, "s");
                    }
                }
                "graphql" => {
                    let _ = kaiv::graphql::import_schema(&bytes, None, "s");
                }
                "xsd" => {
                    let _ = kaiv::xsd::import_schema(&bytes, None, "s");
                }
                "avro" => {
                    let _ = kaiv::avro::import_schema(&bytes, "s");
                }
                _ => {
                    let _ = kaiv::jsonschema::import(&bytes, "s");
                }
            }));
            assert!(
                res.is_ok(),
                "seed {seed} {which}: schema parser PANICKED on: {:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }
}

// -------------------------------------------- format-specific generators

/// XML-shaped documents: a single root, XML-name keys, attributes and
/// text members.
#[test]
fn xml_shaped_docs_are_stable() {
    let n = iters(60);
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0x3197);
        let mut doc = String::from("{\"root\":");
        gen_xmlish(&mut r, 0, &mut doc);
        doc.push('}');
        let Some(daiv) = daiv_of(&doc) else { continue };
        let Ok(b1) = fmt_export("xml", &daiv) else {
            continue;
        };
        let a2 = fmt_import("xml", &b1).unwrap_or_else(|e| {
            panic!(
                "seed {seed}: xml reimport failed: {e}\ndoc: {doc}\nxml: {}",
                String::from_utf8_lossy(&b1)
            )
        });
        let d2 = build(&a2).unwrap();
        let b2 = fmt_export("xml", &d2).unwrap();
        let a3 = fmt_import("xml", &b2).unwrap();
        let d3 = build(&a3).unwrap();
        let b3 = fmt_export("xml", &d3).unwrap();
        assert_eq!(
            b2,
            b3,
            "seed {seed}: xml not stable\ndoc: {doc}\nsecond:\n{}\nthird:\n{}",
            String::from_utf8_lossy(&b2),
            String::from_utf8_lossy(&b3)
        );
    }
}

fn gen_xmlish(r: &mut Rng, depth: u64, out: &mut String) {
    out.push('{');
    let mut first = true;
    let n = 1 + r.below(4);
    let mut used = Vec::new();
    for _ in 0..n {
        let key = match r.below(6) {
            0 if depth > 0 => format!("@{}", r.pick(KEYS)),
            1 if depth > 0 => "#text".to_string(),
            _ => format!("{}{}", r.pick(KEYS), r.below(20)),
        };
        if used.contains(&key) {
            continue;
        }
        used.push(key.clone());
        if !first {
            out.push(',');
        }
        first = false;
        json_string(&key, out);
        out.push(':');
        if key.starts_with('@') || key == "#text" {
            json_string(r.pick(STRINGS), out);
        } else {
            match r.below(5) {
                0 if depth < 3 => gen_xmlish(r, depth + 1, out),
                1 => {
                    // Repeated elements.
                    out.push('[');
                    let m = 1 + r.below(3);
                    for i in 0..m {
                        if i > 0 {
                            out.push(',');
                        }
                        if r.chance(50) && depth < 3 {
                            gen_xmlish(r, depth + 1, out);
                        } else {
                            json_string(r.pick(STRINGS), out);
                        }
                    }
                    out.push(']');
                }
                2 => out.push_str("null"),
                _ => json_string(r.pick(STRINGS), out),
            }
        }
    }
    out.push('}');
}

/// ASN.1-shaped documents: wrapper namespaces all the way down.
#[test]
fn asn1_shaped_docs_are_stable() {
    let n = iters(60);
    for seed in 0..n {
        let mut r = Rng::new(seed ^ 0xde01);
        let mut doc = String::from("{\"seq\":");
        gen_asn1_seq(&mut r, 0, &mut doc);
        doc.push('}');
        let Some(daiv) = daiv_of(&doc) else { continue };
        let b1 = fmt_export("asn1", &daiv).unwrap_or_else(|e| {
            panic!("seed {seed}: asn1 export failed: {e}\ndoc: {doc}\ndaiv:\n{daiv}")
        });
        let a2 = fmt_import("asn1", &b1).unwrap_or_else(|e| {
            panic!("seed {seed}: asn1 reimport failed: {e}\ndoc: {doc}\nder: {b1:02x?}")
        });
        let d2 = build(&a2).unwrap();
        let b2 = fmt_export("asn1", &d2).unwrap();
        assert_eq!(
            b1, b2,
            "seed {seed}: asn1 not stable\ndoc: {doc}\nfirst: {b1:02x?}\nsecond: {b2:02x?}"
        );
    }
}

fn gen_asn1_seq(r: &mut Rng, depth: u64, out: &mut String) {
    out.push('[');
    let n = r.below(5);
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        gen_asn1_value(r, depth, out);
    }
    out.push(']');
}

fn gen_asn1_value(r: &mut Rng, depth: u64, out: &mut String) {
    let top = if depth >= 3 { 7 } else { 10 };
    match r.below(top) {
        0 => out.push_str(r.pick(INTS)),
        1 => out.push_str(if r.chance(50) { "true" } else { "false" }),
        2 => out.push_str("null"),
        3 | 4 => json_string(r.pick(STRINGS), out),
        5 => {
            out.push_str("{\"oid\":\"");
            out.push_str(&format!(
                "{}.{}.{}",
                r.below(3),
                r.below(40),
                r.below(100000)
            ));
            out.push_str("\"}");
        }
        6 => {
            out.push_str("{\"bits\":\"");
            let m = r.below(20);
            for _ in 0..m {
                out.push(if r.chance(50) { '1' } else { '0' });
            }
            out.push_str("\"}");
        }
        7 => {
            out.push_str("{\"seq\":");
            gen_asn1_seq(r, depth + 1, out);
            out.push('}');
        }
        8 => {
            out.push_str("{\"set\":");
            gen_asn1_seq(r, depth + 1, out);
            out.push('}');
        }
        _ => {
            out.push_str(&format!("{{\"c{}\":", r.below(5)));
            gen_asn1_seq(r, depth + 1, out);
            out.push('}');
        }
    }
}
