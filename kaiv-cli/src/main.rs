//! The `kaiv` CLI — thin shell over the library pipeline.
//! Hand-rolled argument handling; the only direct dependency is
//! ureq (already in the tree via the library's `net` feature),
//! which the `login` flow drives.
//!
//! Configuration: the nearest `kaiv.kaiv` up from the working
//! directory (SPEC.md § Layer 2), overlaid with KAIV_REGISTRY_* /
//! KAIV_REGISTRY environment variables.

mod account;

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
    validate [data] [schema]          validate data against a schema;
                                      data: .daiv (or .kaiv, built
                                      first); stdin when omitted or `-`
                                      (sniffed; `-` is required when
                                      passing a schema);
                                      schema: .csaiv (or .saiv, compiled
                                      first); omitted = resolve the
                                      document's .!schema declarations
                                      from the registries
    unit     <expr>                   canonicalize a unit expression
    fmt      [file] [-w] [--check]    format an authoring file into the
                                      standard style (stdin when the
                                      file is omitted or `-`); a
                                      canonical .daiv/.raiv input is
                                      rendered as idiomatic authored
                                      .kaiv instead (a view - sugar the
                                      compiler resolved away does not
                                      come back); -w rewrites the file
                                      in place, --check exits 1 if the
                                      file is not already formatted
                                      (both authoring files only)
    mapping  validate <map.maiv>      check a mapping against its two
                                      schemas (namepaths, overrides,
                                      required-field completeness)
    mapping  apply <map.maiv> [data]  source document -> target .daiv
                                      (data: .daiv or authored .kaiv,
                                      built first; stdin when omitted
                                      or `-`)
    mapping  compose <a> <b>          compose two mappings (a: B<-A,
                                      b: C<-B) into C<-A; the .!via
                                      trail records each hop by its
                                      edge identity {source}/{target}.
                                      All three resolve schemas from
                                      the mapping's .!source/.!target
                                      via the registries;
                                      --source-schema/--target-schema
                                      override with local files
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
    login    [email]                  sign in to the kaiv registries
                                      (idaiv): an emailed one-time
                                      link approves this device; the
                                      first sign-in creates the
                                      account. KAIV_ID_URL overrides
                                      the identity host (default
                                      https://id.kaiv.io during the
                                      alpha)
    whoami                            the signed-in account
    logout                            revoke and forget the stored
                                      session
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
        ("validate", rest) if rest.len() <= 2 => {
            // Data comes from a file, or stdin when the argument is
            // omitted or `-` (matching compile/denorm/build/schema).
            // A bare `kaiv validate <file>` is always the data form;
            // to validate stdin against a schema file, write
            // `kaiv validate - <schema>`.
            let data = match rest.first().map(String::as_str) {
                None | Some("-") => None,
                Some(p) => Some(p),
            };
            let schema = rest.get(1).map(String::as_str);
            if let Some(p) = data {
                reject_raiv(p)?;
            }
            let r = resolver()?;
            // Stdin is sniffed: authored builds, .!daiv passes
            // through, .!raiv denormalizes (schema-aware) first.
            let daiv = canonical_input(data, None, None)?;
            let compiled = match schema {
                Some(s) => {
                    let csaiv = if s.ends_with(".csaiv") {
                        String::from_utf8(read(s)?).map_err(|e| e.to_string())?
                    } else {
                        kaiv::compile_schema_with(&read(s)?, &r).map_err(|e| e.to_string())?
                    };
                    kaiv::parse_csaiv(&csaiv).map_err(|e| e.to_string())?
                }
                // No schema argument: resolve the document's .!schema
                // declarations through the registries (SPEC.md
                // § Schema Composition; canonical hosts by default).
                None => kaiv::schema_for_daiv(&daiv, &r)
                    .map_err(|e| e.to_string())?
                    .ok_or("document declares no .!schema; pass a schema file")?,
            };
            match kaiv::validate(&daiv, &compiled) {
                Ok(()) => Ok("pass\n".into()),
                Err(e) => Err(e.to_string()),
            }
        }
        ("mapping", rest) => mapping_cmd(rest),
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
        ("login", rest) if rest.len() <= 1 => {
            let email = match rest.first() {
                Some(e) => e.clone(),
                None => prompt("email: ")?,
            };
            if email.is_empty() || !email.contains('@') {
                return Err("that does not look like an email address".into());
            }
            let grant = account::begin_login(&email)?;
            eprintln!(
                "A sign-in link is on its way to {email}.\n\
                 Open it only if the code in the message is:\n\n\
                 \x20   {}\n\n\
                 Waiting for the link to be opened (Ctrl-C to give up)…",
                grant.user_code
            );
            let credentials = account::wait_for_approval(&grant)?;
            Ok(format!(
                "Signed in as {} ({}).\n",
                credentials.email, credentials.issuer
            ))
        }
        ("whoami", []) => {
            let mut credentials = account::load()?
                .ok_or("not signed in — run `kaiv login`")?;
            let (id, email, handle) = account::whoami(&mut credentials)?;
            let handle = handle.map(|h| format!(" ({h})")).unwrap_or_default();
            Ok(format!("{email}{handle}\nid: {id}\nissuer: {}\n", credentials.issuer))
        }
        ("logout", []) => match account::load()? {
            Some(credentials) => {
                account::revoke(&credentials);
                account::erase()?;
                Ok(format!("Signed out of {}.\n", credentials.issuer))
            }
            None => Ok("No stored session.\n".into()),
        },
        ("fmt", rest) => fmt_cmd(rest),
        ("unit", [expr]) => kaiv::unit::canonicalize(expr)
            .map(|c| format!("{c}\n"))
            .ok_or_else(|| {
                // The one famously ambiguous spelling gets a teaching
                // rejection instead of a shrug.
                kaiv::unit::ambiguity_hint(expr)
                    .unwrap_or_else(|| format!("invalid unit expression: {expr}"))
            }),
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
        ("login", _) => Err("login takes at most an email address".into()),
        ("whoami", _) => Err("whoami takes no arguments".into()),
        ("logout", _) => Err("logout takes no arguments".into()),
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

/// `kaiv mapping` subcommands (ARCHITECTURE.md §14.9): validate,
/// apply, compose. Schemas resolve from the mapping's `.!source` /
/// `.!target` references through the registries; `--source-schema` /
/// `--target-schema` override with local files (.saiv compiled,
/// .csaiv used as-is).
fn mapping_cmd(rest: &[String]) -> Result<String, String> {
    let sub = rest.first().map(String::as_str).unwrap_or("");
    let mut source_schema = None;
    let mut target_schema = None;
    let mut pos: Vec<&str> = Vec::new();
    let mut it = rest.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--source-schema" => {
                source_schema = Some(it.next().ok_or("--source-schema needs a value")?.as_str())
            }
            "--target-schema" => {
                target_schema = Some(it.next().ok_or("--target-schema needs a value")?.as_str())
            }
            f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
            p => pos.push(p),
        }
    }
    match sub {
        "validate" => {
            let map = pos.first().ok_or("mapping validate needs a .maiv file")?;
            let m = kaiv::maiv::parse_maiv(&read(map)?).map_err(|e| e.to_string())?;
            let r = resolver()?;
            let src = schema_from(source_schema, &m.source, &r)?;
            let tgt = schema_from(target_schema, &m.target, &r)?;
            kaiv::maiv::validate_maiv(&m, &src, &tgt).map_err(|e| e.to_string())?;
            Ok("ok\n".into())
        }
        "apply" => {
            let map = pos.first().ok_or("mapping apply needs a .maiv file")?;
            let data = match pos.get(1).copied() {
                None | Some("-") => None,
                p => p,
            };
            let m = kaiv::maiv::parse_maiv(&read(map)?).map_err(|e| e.to_string())?;
            let r = resolver()?;
            let src = schema_from(source_schema, &m.source, &r)?;
            let tgt = schema_from(target_schema, &m.target, &r)?;
            // The mapper validates the mapping before applying it
            // (ARCHITECTURE.md §14.1).
            kaiv::maiv::validate_maiv(&m, &src, &tgt).map_err(|e| e.to_string())?;
            // Authored source data builds first; canonical passes
            // through (same sniffing as every other consumer).
            let daiv = canonical_input(data, None, None)?;
            kaiv::maiv::apply(&m, &daiv, &tgt).map_err(|e| e.to_string())
        }
        "compose" => {
            let (a, b) = match pos.as_slice() {
                [a, b] => (a, b),
                _ => return Err("mapping compose needs two .maiv files".into()),
            };
            let ma = kaiv::maiv::parse_maiv(&read(a)?).map_err(|e| e.to_string())?;
            let mb = kaiv::maiv::parse_maiv(&read(b)?).map_err(|e| e.to_string())?;
            let c = kaiv::maiv::compose(&ma, &mb).map_err(|e| e.to_string())?;
            Ok(c.render())
        }
        other => Err(format!(
            "unknown mapping subcommand: {other:?} (validate | apply | compose)"
        )),
    }
}

/// A compiled schema from an override file (.saiv compiled, .csaiv
/// as-is) or from the registries via the mapping's own reference.
fn schema_from(
    flag: Option<&str>,
    reference: &str,
    r: &kaiv::Resolver,
) -> Result<kaiv::CompiledSchema, String> {
    let csaiv = match flag {
        Some(p) if p.to_ascii_lowercase().ends_with(".csaiv") => {
            String::from_utf8(read(p)?).map_err(|e| e.to_string())?
        }
        Some(p) => kaiv::compile_schema_with(&read(p)?, r).map_err(|e| e.to_string())?,
        None => {
            if reference.contains("://") {
                return Err(format!(
                    "URL schema reference {reference} needs a local override \
                     (--source-schema / --target-schema)"
                ));
            }
            String::from_utf8(r.csaiv_bytes(reference, &[]).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?
        }
    };
    kaiv::parse_csaiv(&csaiv).map_err(|e| e.to_string())
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
                // defined over .daiv, so resolve first — recognized by
                // its `.!raiv` declaration or extension. A .daiv is
                // verbatim already.
                if opens_with_kind(&t, "raiv")
                    || path.is_some_and(|p| p.to_ascii_lowercase().ends_with(".raiv"))
                {
                    // Schema-aware: materializes absent optional
                    // fields, so the result is a complete .daiv.
                    return kaiv::denorm::denormalize_with(&t, &r).map_err(|e| e.to_string());
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

/// One line from stdin, prompted on stderr (stdout stays clean
/// for command output).
fn prompt(question: &str) -> Result<String, String> {
    use std::io::{BufRead, Write};
    eprint!("{question}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("read stdin: {e}"))?;
    Ok(line.trim().to_string())
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
            // An empty document is valid kaiv, but empty *stdin* is
            // almost always an upstream failure in a pipeline (the
            // shell reports the last command's status, so a dead
            // `kaiv import | kaiv build` would otherwise exit 0).
            // Fail loudly; an intentionally empty document can be
            // passed as a file.
            if buf.is_empty() {
                return Err(
                    "empty input on stdin (upstream failure in a pipeline?); \
                     pass an empty file to process an empty document"
                        .into(),
                );
            }
            Ok(buf)
        }
    }
}

/// `kaiv fmt`: normalize an authoring file into the standard style,
/// or render a canonical stream as idiomatic authored kaiv.
fn fmt_cmd(rest: &[String]) -> Result<String, String> {
    let mut path: Option<String> = None;
    let mut write = false;
    let mut check = false;
    for a in rest {
        match a.as_str() {
            "-w" | "--write" => write = true,
            "--check" => check = true,
            "-" => path = None,
            f if f.starts_with('-') => return Err(format!("unknown option: {f}")),
            p if path.is_none() => path = Some(p.to_string()),
            p => return Err(format!("unexpected argument: {p}")),
        }
    }
    if write && check {
        return Err("-w and --check are mutually exclusive".into());
    }
    let data = read_input(path.as_deref())?;
    let text = String::from_utf8(data).map_err(|e| e.to_string())?;

    // The declaration decides the kind; the extension breaks a tie;
    // undeclared stdin is sniffed like `validate` does.
    let ext = path
        .as_deref()
        .map(|p| p.to_ascii_lowercase())
        .unwrap_or_default();
    for k in ["csaiv", "msaiv"] {
        if opens_with_kind(&text, k) || ext.ends_with(&format!(".{k}")) {
            return Err(format!(
                ".{k} is a compiled artifact, not an authoring surface;                  fmt formats what humans write"
            ));
        }
    }
    let canonical = opens_with_kind(&text, "daiv")
        || opens_with_kind(&text, "raiv")
        || ((ext.ends_with(".daiv") || ext.ends_with(".raiv"))
            && !opens_with_kind(&text, "kaiv"));
    if canonical {
        if write {
            return Err(
                "-w would replace a canonical file with authored kaiv;                  redirect the output instead"
                    .into(),
            );
        }
        if check {
            return Err("--check applies to authoring files only".into());
        }
        return kaiv::lift(&text).map_err(|e| e.to_string());
    }
    let plain_kind = [
        ("saiv", kaiv::FileKind::Schema),
        ("taiv", kaiv::FileKind::TypeLib),
        ("faiv", kaiv::FileKind::UnitLib),
        ("maiv", kaiv::FileKind::Mapping),
    ]
    .into_iter()
    .find(|(k, _)| opens_with_kind(&text, k) || ext.ends_with(&format!(".{k}")));
    let out = match plain_kind {
        Some((_, k)) => kaiv::format_plain(&text, k).map_err(|e| e.to_string())?,
        None => kaiv::format_data(&text).map_err(|e| e.to_string())?,
    };
    if check {
        if out != text {
            return Err(match path.as_deref() {
                Some(p) => format!("{p} is not formatted (run: kaiv fmt -w {p})"),
                None => "input is not formatted".into(),
            });
        }
        return Ok(String::new());
    }
    if write {
        let Some(p) = path.as_deref() else {
            return Err("-w needs a file (stdin has nowhere to write back)".into());
        };
        if out != text {
            std::fs::write(p, &out).map_err(|e| format!("cannot write {p}: {e}"))?;
        }
        return Ok(String::new());
    }
    Ok(out)
}

/// Whether the text opens with the format declaration for `kind` —
/// bare or versioned, on the first line (or after a shebang). The
/// declaration keyword mirrors the file kind (SPEC.md § Format
/// Declaration), so this identifies canonical streams without any
/// filename context.
fn opens_with_kind(text: &str, kind: &str) -> bool {
    let mut lines = text.lines();
    let mut first = lines.next().unwrap_or("");
    if first.starts_with("#!") {
        first = lines.next().unwrap_or("");
    }
    first
        .trim_start_matches([' ', '\t'])
        .strip_prefix(".!")
        .and_then(|r| r.strip_prefix(kind))
        .is_some_and(|r| r.is_empty() || r.starts_with([' ', '\t']))
}

/// Is this kaiv text canonical (`.daiv`/`.raiv`) or authored?
/// The format declaration is authoritative (canonical files always
/// carry their kind's declaration); then the extension; stdin without
/// a declaration is sniffed: the first substantive line of a
/// canonical file has a `'` delimiter before its first `=`.
fn is_canonical(text: &str, path: Option<&str>) -> bool {
    if opens_with_kind(text, "daiv") || opens_with_kind(text, "raiv") {
        return true;
    }
    if opens_with_kind(text, "kaiv") {
        return false;
    }
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
