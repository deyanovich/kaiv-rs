//! The `kaiv` CLI — thin shell over the library pipeline. Zero
//! dependencies; hand-rolled argument handling.
//!
//! Configuration: the nearest `kaiv.kaiv` up from the working
//! directory (SPEC.md § Layer 2), overlaid with KAIV_REGISTRY_* /
//! KAIV_REGISTRY environment variables.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
kaiv — reference toolchain for the kaiv format (Levels 0-3)

USAGE:
    kaiv [--offline] <COMMAND> [ARGS]

    --offline: resolve registries from the local cache only
    (equivalent to KAIV_OFFLINE=1; cache root: KAIV_CACHE_DIR,
    else ~/.cache/kaiv, overridable per-project via /cache::dir
    in kaiv.kaiv)

COMMANDS:
    compile  [file.kaiv]              authored -> relational canonical (.raiv)
    denorm   [file.raiv]              relational -> denormalized (.daiv)
    build    [file.kaiv]              authored -> .daiv (compile + denorm)
    schema   [file.saiv]              authored schema -> compiled (.csaiv)
                                      (these four read stdin when no file
                                      is given)
    validate <data> [schema]          validate data against a schema;
                                      data: .daiv (or .kaiv, built first)
                                      schema: .csaiv (or .saiv, compiled
                                      first); omitted = resolve the
                                      document's .!schema declarations
                                      from the registries
    unit     <expr>                   canonicalize a unit expression
    import   [--FORMAT] [--flat] [f]  foreign format -> authored .kaiv;
                                      formats: --json --yaml --toml
                                      --xml --cbor --avro --proto
                                      --asn1, inferred from the file
                                      extension (.json .yaml .yml
                                      .toml .xml .cbor .avro .pb
                                      .binpb .der .pem .crt .cer;
                                      PEM/DER auto-import as ASN.1),
                                      the option required
                                      for stdin. proto wire data is
                                      not self-describing: pass
                                      --schema <file.proto> (and
                                      --message <Name> when the
                                      schema has several top-level
                                      messages).
                                      Structures import natively;
                                      only empty containers, anonymous
                                      nested arrays, and non-flat
                                      strings embed as std/enc types.
                                      --flat embeds all containers
                                      (json only)
    export   --FORMAT [file]          kaiv -> foreign format (--json
                                      --yaml --toml --xml --cbor
                                      --avro --proto --asn1; the binary
                                      formats write raw bytes to
                                      stdout; --proto needs --schema
                                      / --message like import);
                                      .kaiv is built first, .daiv
                                      used as is, .raiv denormalized
                                      first, stdin sniffed
    infer    [--name ID] [file]       infer an authored .saiv schema
                                      from an example document (kaiv
                                      or any import format); the
                                      example validates against it
    import-schema [--name ID] [file]  foreign schema -> authored .saiv,
                                      a sound weakening: constraints
                                      kaiv cannot express are dropped
                                      with // comments, never invented.
                                      Formats: --json (JSON Schema)
                                      --proto --avro (.avsc) --graphql
                                      --xsd, inferred from the
                                      extension (.json .proto .avsc
                                      .graphql .graphqls .gql .xsd);
                                      --message picks the root
                                      message/type/element when the
                                      schema declares several
    help                              this text
    version                           print the version

Output goes to stdout; diagnostics to stderr. Exit 0 on success/pass,
1 on any error or validation failure.

The nearest kaiv.kaiv up from the working directory configures
registry resolution (KAIV_REGISTRY_* environment variables override).";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Global flag: --offline = cache-only network resolution (the
    // library reads KAIV_OFFLINE; registry artifacts are immutable,
    // so a warm cache is a complete resolution surface).
    if let Some(i) = args.iter().position(|a| a == "--offline") {
        args.remove(i);
        std::env::set_var("KAIV_OFFLINE", "1");
    }
    match run(&args) {
        Ok(out) => {
            use std::io::Write;
            if std::io::stdout().write_all(&out).is_err() {
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("kaiv: {msg}");
            ExitCode::FAILURE
        }
    }
}

/// Text commands go through `run_text`; the binary formats (CBOR,
/// Avro, proto) export raw bytes.
fn run(args: &[String]) -> Result<Vec<u8>, String> {
    if args.first().map(String::as_str) == Some("export") {
        let a = parse_fmt_args(&args[1..])?;
        if let Some(f @ ("cbor" | "avro" | "proto" | "asn1")) = a.fmt.as_deref() {
            if a.flat {
                return Err("--flat is an import option".into());
            }
            let canonical =
                canonical_input(a.path.as_deref(), a.schema.as_deref(), a.message.as_deref())?;
            return match f {
                "cbor" => kaiv::cbor::export(&canonical).map_err(|e| e.to_string()),
                "avro" => kaiv::avro::export(&canonical).map_err(|e| e.to_string()),
                "asn1" => kaiv::asn1::export(&canonical).map_err(|e| e.to_string()),
                _ => {
                    let schema = proto_schema(&a)?;
                    kaiv::proto::export(&canonical, &schema, a.message.as_deref())
                        .map_err(|e| e.to_string())
                }
            };
        }
    }
    run_text(args).map(String::into_bytes)
}

fn run_text(args: &[String]) -> Result<String, String> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match (cmd, args.get(1..).unwrap_or(&[])) {
        ("compile", rest) if rest.len() <= 1 => {
            let r = resolver()?;
            let data = read_input(rest.first().map(String::as_str))?;
            kaiv::compile_with(&data, &r).map_err(|e| e.to_string())
        }
        ("denorm", rest) if rest.len() <= 1 => {
            // Schema-aware: when the document declares a .!schema,
            // absent optional fields are materialized from the
            // resolved .csaiv (SPEC.md § Default Values).
            let r = resolver()?;
            let data = read_input(rest.first().map(String::as_str))?;
            let raiv = String::from_utf8(data).map_err(|e| e.to_string())?;
            kaiv::denormalize_with(&raiv, &r).map_err(|e| e.to_string())
        }
        ("build", rest) if rest.len() <= 1 => {
            let r = resolver()?;
            let data = read_input(rest.first().map(String::as_str))?;
            let raiv = kaiv::compile_with(&data, &r).map_err(|e| e.to_string())?;
            kaiv::denormalize_with(&raiv, &r).map_err(|e| e.to_string())
        }
        ("schema", rest) if rest.len() <= 1 => {
            let r = resolver()?;
            let data = read_input(rest.first().map(String::as_str))?;
            kaiv::compile_schema_with(&data, &r).map_err(|e| e.to_string())
        }
        ("validate", [data, schema]) => {
            reject_raiv(data)?;
            let r = resolver()?;
            let csaiv = if schema.ends_with(".csaiv") {
                String::from_utf8(read(schema)?).map_err(|e| e.to_string())?
            } else {
                kaiv::compile_schema_with(&read(schema)?, &r).map_err(|e| e.to_string())?
            };
            let compiled = kaiv::parse_csaiv(&csaiv).map_err(|e| e.to_string())?;
            let daiv = canonical_input(Some(data), None, None)?;
            match kaiv::validate(&daiv, &compiled) {
                Ok(()) => Ok("pass\n".into()),
                Err(e) => Err(e.to_string()),
            }
        }
        ("validate", [data]) => {
            reject_raiv(data)?;
            // No schema argument: resolve the document's .!schema
            // declarations through the registries (SPEC.md § Schema
            // Composition; canonical hosts by default).
            let r = resolver()?;
            let daiv = canonical_input(Some(data), None, None)?;
            let compiled = kaiv::schema_for_daiv(&daiv, &r)
                .map_err(|e| e.to_string())?
                .ok_or("document declares no .!schema; pass a schema file")?;
            match kaiv::validate(&daiv, &compiled) {
                Ok(()) => Ok("pass\n".into()),
                Err(e) => Err(e.to_string()),
            }
        }
        ("infer", rest) => {
            let mut name = None;
            let mut path = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--name" => name = Some(it.next().ok_or("--name needs a value")?.clone()),
                    f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
                    p if path.is_none() => path = Some(p.to_string()),
                    p => return Err(format!("unexpected argument: {p}")),
                }
            }
            let name = name.unwrap_or_else(|| {
                path.as_deref()
                    .and_then(|p| std::path::Path::new(p).file_stem())
                    .and_then(|s| s.to_str())
                    .map(|s| {
                        let cleaned: String = s
                            .chars()
                            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                            .collect();
                        if cleaned.starts_with(|c: char| c.is_ascii_alphabetic()) {
                            cleaned
                        } else {
                            format!("inferred_{cleaned}")
                        }
                    })
                    .unwrap_or_else(|| "inferred".to_string())
            });
            let canonical = canonical_input(path.as_deref(), None, None)?;
            kaiv::infer::infer(&canonical, &name).map_err(|e| e.to_string())
        }
        ("import-schema", rest) => {
            let mut name = None;
            let mut message = None;
            let mut fmt: Option<&str> = None;
            let mut path = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--name" => name = Some(it.next().ok_or("--name needs a value")?.clone()),
                    "--message" => {
                        message = Some(it.next().ok_or("--message needs a value")?.clone())
                    }
                    "--json" => fmt = Some("json"),
                    "--proto" => fmt = Some("proto"),
                    "--avro" => fmt = Some("avro"),
                    "--graphql" => fmt = Some("graphql"),
                    "--xsd" => fmt = Some("xsd"),
                    f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
                    p if path.is_none() => path = Some(p.to_string()),
                    p => return Err(format!("unexpected argument: {p}")),
                }
            }
            let fmt = fmt
                .or_else(|| match path.as_deref() {
                    Some(p) if p.ends_with(".proto") => Some("proto"),
                    Some(p) if p.ends_with(".avsc") => Some("avro"),
                    Some(p)
                        if p.ends_with(".graphql")
                            || p.ends_with(".graphqls")
                            || p.ends_with(".gql") =>
                    {
                        Some("graphql")
                    }
                    Some(p) if p.ends_with(".xsd") => Some("xsd"),
                    Some(p) if p.ends_with(".json") => Some("json"),
                    _ => None,
                })
                .unwrap_or("json");
            let name = name.unwrap_or_else(|| "imported".to_string());
            let data = read_input(path.as_deref())?;
            match fmt {
                "proto" => {
                    let text = String::from_utf8(data).map_err(|e| e.to_string())?;
                    kaiv::proto::import_schema(&text, message.as_deref(), &name)
                        .map_err(|e| e.to_string())
                }
                "avro" => kaiv::avro::import_schema(&data, &name).map_err(|e| e.to_string()),
                "graphql" => kaiv::graphql::import_schema(&data, message.as_deref(), &name)
                    .map_err(|e| e.to_string()),
                "xsd" => kaiv::xsd::import_schema(&data, message.as_deref(), &name)
                    .map_err(|e| e.to_string()),
                _ => kaiv::jsonschema::import(&data, &name).map_err(|e| e.to_string()),
            }
        }
        ("unit", [expr]) => kaiv::unit::canonicalize(expr)
            .map(|c| format!("{c}\n"))
            .ok_or_else(|| format!("invalid unit expression: {expr}")),
        ("import", rest) => {
            let a = parse_fmt_args(rest)?;
            let fmt = match (&a.fmt, &a.path) {
                (Some(f), _) => f.clone(),
                (None, Some(p)) => match ext_format(p) {
                    Some(f) => f.to_string(),
                    None => return Err(format!("cannot infer format from {p}")),
                },
                (None, None) => {
                    return Err("stdin import requires a format option (e.g. --json)".into())
                }
            };
            let data = read_input(a.path.as_deref())?;
            match (fmt.as_str(), a.flat) {
                ("json", false) => kaiv::json::import(&data).map_err(|e| e.to_string()),
                ("json", true) => kaiv::json::import_flat(&data).map_err(|e| e.to_string()),
                ("yaml", false) => kaiv::yaml::import(&data).map_err(|e| e.to_string()),
                ("toml", false) => kaiv::toml::import(&data).map_err(|e| e.to_string()),
                ("xml", false) => kaiv::xml::import(&data).map_err(|e| e.to_string()),
                ("cbor", false) => kaiv::cbor::import(&data).map_err(|e| e.to_string()),
                ("avro", false) => kaiv::avro::import(&data).map_err(|e| e.to_string()),
                ("proto", false) => {
                    let schema = proto_schema(&a)?;
                    kaiv::proto::import(&data, &schema, a.message.as_deref())
                        .map_err(|e| e.to_string())
                }
                ("asn1", false) => kaiv::asn1::import(&data).map_err(|e| e.to_string()),
                (f @ ("yaml" | "toml" | "xml" | "cbor" | "avro" | "proto" | "asn1"), true) => {
                    Err(format!("--flat is json-only (got --{f})"))
                }
                (other, _) => Err(format!("unsupported import format: {other}")),
            }
        }
        ("export", rest) => {
            let a = parse_fmt_args(rest)?;
            if a.flat {
                return Err("--flat is an import option".into());
            }
            let fmt = a
                .fmt
                .clone()
                .ok_or("export requires a format option (e.g. --json)")?;
            let canonical =
                canonical_input(a.path.as_deref(), a.schema.as_deref(), a.message.as_deref())?;
            match fmt.as_str() {
                "json" => kaiv::json::export(&canonical).map_err(|e| e.to_string()),
                "yaml" => kaiv::yaml::export(&canonical).map_err(|e| e.to_string()),
                "toml" => kaiv::toml::export(&canonical).map_err(|e| e.to_string()),
                "xml" => kaiv::xml::export(&canonical).map_err(|e| e.to_string()),
                other => Err(format!("unsupported export format: {other}")),
            }
        }
        ("help" | "--help" | "-h", _) => Ok(format!("{USAGE}\n")),
        ("version" | "--version" | "-V", _) => {
            Ok(format!("kaiv {}\n", env!("CARGO_PKG_VERSION")))
        }
        // Known commands with the wrong argument count get a specific
        // message rather than "unknown command".
        (cmd @ ("compile" | "denorm" | "build" | "schema"), _) => Err(format!(
            "{cmd} takes at most one file (got extra arguments); see `kaiv help`"
        )),
        ("unit", []) => Err("unit needs an expression: kaiv unit <expr>".into()),
        ("unit", _) => Err("unit takes exactly one expression".into()),
        ("validate", _) => Err("validate takes <data> [schema]".into()),
        (cmd, _) => Err(format!(
            "unknown or malformed command: {cmd} (try `kaiv help`)"
        )),
    }
}

/// `validate` is scoped to `.daiv`/`.kaiv`; a relational `.raiv` must be
/// denormalized first, so reject it rather than validate the
/// un-materialized form.
fn reject_raiv(data: &str) -> Result<(), String> {
    if data.to_ascii_lowercase().ends_with(".raiv") {
        return Err(
            "validate needs .daiv or .kaiv (relational .raiv must be denormalized first)".into(),
        );
    }
    Ok(())
}

fn read(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))
}

#[derive(Default)]
struct FmtArgs {
    fmt: Option<String>,
    flat: bool,
    path: Option<String>,
    schema: Option<String>,
    message: Option<String>,
}

/// `[--FORMAT] [--flat] [--schema F] [--message M] [path]` for
/// import/export.
fn parse_fmt_args(rest: &[String]) -> Result<FmtArgs, String> {
    let mut a = FmtArgs::default();
    let mut it = rest.iter();
    while let Some(t) = it.next() {
        match t.as_str() {
            "--json" => a.fmt = Some("json".to_string()),
            "--yaml" => a.fmt = Some("yaml".to_string()),
            "--toml" => a.fmt = Some("toml".to_string()),
            "--xml" => a.fmt = Some("xml".to_string()),
            "--cbor" => a.fmt = Some("cbor".to_string()),
            "--avro" => a.fmt = Some("avro".to_string()),
            "--proto" => a.fmt = Some("proto".to_string()),
            "--asn1" => a.fmt = Some("asn1".to_string()),
            "--flat" => a.flat = true,
            "--schema" => a.schema = Some(it.next().ok_or("--schema needs a path")?.clone()),
            "--message" => a.message = Some(it.next().ok_or("--message needs a name")?.clone()),
            f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
            p if a.path.is_none() => a.path = Some(p.to_string()),
            p => return Err(format!("unexpected argument: {p}")),
        }
    }
    Ok(a)
}

/// The `.proto` schema text a proto conversion needs.
fn proto_schema(a: &FmtArgs) -> Result<String, String> {
    let p = a
        .schema
        .as_deref()
        .ok_or("proto needs --schema <file.proto>")?;
    String::from_utf8(read(p)?).map_err(|e| e.to_string())
}

/// Canonical kaiv text from a path or stdin: foreign formats import
/// first, authored kaiv builds, canonical passes through. `schema`
/// and `message` apply when the input is proto wire data.
fn canonical_input(
    path: Option<&str>,
    schema: Option<&str>,
    message: Option<&str>,
) -> Result<String, String> {
    let data = read_input(path)?;
    let r = resolver()?;
    let authored = match path.and_then(ext_format) {
        Some("json") => Some(kaiv::json::import(&data).map_err(|e| e.to_string())?),
        Some("yaml") => Some(kaiv::yaml::import(&data).map_err(|e| e.to_string())?),
        Some("toml") => Some(kaiv::toml::import(&data).map_err(|e| e.to_string())?),
        Some("xml") => Some(kaiv::xml::import(&data).map_err(|e| e.to_string())?),
        Some("cbor") => Some(kaiv::cbor::import(&data).map_err(|e| e.to_string())?),
        Some("avro") => Some(kaiv::avro::import(&data).map_err(|e| e.to_string())?),
        Some("proto") => {
            let sp = schema.ok_or("proto input needs --schema <file.proto>")?;
            let stext = String::from_utf8(read(sp)?).map_err(|e| e.to_string())?;
            Some(kaiv::proto::import(&data, &stext, message).map_err(|e| e.to_string())?)
        }
        Some("asn1") => Some(kaiv::asn1::import(&data).map_err(|e| e.to_string())?),
        _ => None,
    };
    let text = match authored {
        Some(a) => a,
        None => {
            let t = String::from_utf8(data).map_err(|e| e.to_string())?;
            if is_canonical(&t, path) {
                // Relational .raiv still carries `$field` references
                // and `$$` doublings; consumers (exporters, infer) are
                // defined over .daiv, so resolve first. A .daiv (or
                // sniffed canonical stdin) is verbatim already.
                if path.is_some_and(|p| p.to_ascii_lowercase().ends_with(".raiv")) {
                    return kaiv::denormalize(&t).map_err(|e| e.to_string());
                }
                return Ok(t);
            }
            t
        }
    };
    let raiv = kaiv::compile_with(text.as_bytes(), &r).map_err(|e| e.to_string())?;
    kaiv::denormalize_with(&raiv, &r).map_err(|e| e.to_string())
}

/// Import format from a file extension.
fn ext_format(path: &str) -> Option<&'static str> {
    let p = path.to_ascii_lowercase();
    if p.ends_with(".json") {
        Some("json")
    } else if p.ends_with(".yaml") || p.ends_with(".yml") {
        Some("yaml")
    } else if p.ends_with(".toml") {
        Some("toml")
    } else if p.ends_with(".xml") {
        Some("xml")
    } else if p.ends_with(".cbor") {
        Some("cbor")
    } else if p.ends_with(".avro") {
        Some("avro")
    } else if p.ends_with(".pb") || p.ends_with(".binpb") {
        Some("proto")
    } else if p.ends_with(".der")
        || p.ends_with(".pem")
        || p.ends_with(".crt")
        || p.ends_with(".cer")
    {
        Some("asn1")
    } else {
        None
    }
}

fn read_input(path: Option<&str>) -> Result<Vec<u8>, String> {
    match path {
        Some(p) => read(p),
        None => {
            use std::io::Read;
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| format!("cannot read stdin: {e}"))?;
            Ok(buf)
        }
    }
}

/// Is this kaiv text canonical (`.daiv`/`.raiv`) or authored?
/// Extension wins; stdin is sniffed: the first substantive line of a
/// canonical file has a `'` delimiter before its first `=`.
fn is_canonical(text: &str, path: Option<&str>) -> bool {
    if let Some(p) = path {
        let lp = p.to_ascii_lowercase();
        if lp.ends_with(".daiv") || lp.ends_with(".raiv") {
            return true;
        }
        if lp.ends_with(".kaiv") {
            return false;
        }
    }
    for raw in text.lines() {
        let s = raw.trim_start_matches([' ', '\t']);
        if s.is_empty()
            || s.starts_with('#')
            || s.starts_with("//")
            || s.starts_with(".!")
            || s.starts_with(".?")
        {
            continue;
        }
        // Find the first `'` and `=` OUTSIDE quoted names (`""`-doubling
        // aware): an apostrophe inside a quoted name is literal, so
        // `"it's"=on` is authored, not canonical.
        let (mut tick, mut eq) = (None, None);
        let b = s.as_bytes();
        let mut in_quote = false;
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'"' => {
                    if in_quote && b.get(i + 1) == Some(&b'"') {
                        i += 1;
                    } else {
                        in_quote = !in_quote;
                    }
                }
                b'\'' if !in_quote && tick.is_none() => tick = Some(i),
                b'=' if !in_quote && eq.is_none() => eq = Some(i),
                _ => {}
            }
            i += 1;
        }
        return match (tick, eq) {
            (Some(t), Some(e)) => t < e,
            (Some(_), None) => true,
            _ => false,
        };
    }
    false
}

/// Resolver from the nearest `kaiv.kaiv` up from the working
/// directory, with the environment overlaid.
fn resolver() -> Result<kaiv::Resolver, String> {
    let mut config = match find_config(&std::env::current_dir().map_err(|e| e.to_string())?) {
        Some(path) => kaiv::Config::load(&path).map_err(|e| e.to_string())?,
        None => kaiv::Config::default(),
    };
    config.apply_env();
    Ok(kaiv::Resolver::new(config))
}

fn find_config(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join("kaiv.kaiv");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(args: &[&str]) -> Result<String, String> {
        run_text(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn is_canonical_is_quote_aware_and_case_insensitive() {
        // Apostrophe inside a quoted name is literal -> authored.
        assert!(!is_canonical("\"it's\"=on", None));
        assert!(!is_canonical("[/\"o'brien\"]", None));
        // Genuine canonical line.
        assert!(is_canonical("!str'::x=1", None));
        // Extension check is case-insensitive.
        assert!(is_canonical("", Some("DATA.DAIV")));
        assert!(!is_canonical("", Some("DOC.KAIV")));
    }

    #[test]
    fn extension_inference_is_case_insensitive() {
        assert_eq!(ext_format("X.JSON"), Some("json"));
        assert_eq!(ext_format("cert.PEM"), Some("asn1"));
        assert_eq!(ext_format("m.PB"), Some("proto"));
    }

    #[test]
    fn wrong_arity_names_the_command() {
        let e = rt(&["compile", "a.kaiv", "b.kaiv"]).unwrap_err();
        assert!(e.contains("takes at most one file"), "{e}");
        assert!(!e.contains("unknown"), "{e}");
        let e = rt(&["unit"]).unwrap_err();
        assert!(e.contains("needs an expression"), "{e}");
    }

    #[test]
    fn validate_rejects_raiv() {
        assert!(reject_raiv("doc.raiv").is_err());
        assert!(reject_raiv("DOC.RAIV").is_err());
        assert!(reject_raiv("doc.daiv").is_ok());
    }
}
