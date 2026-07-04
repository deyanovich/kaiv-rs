//! The `kaiv` CLI — thin shell over the library pipeline. Zero
//! dependencies; hand-rolled argument handling.
//!
//! Configuration: the nearest `kaiv.kaiv` up from the working
//! directory (SPEC.md § Layer 2), overlaid with KAIV_REGISTRY_* /
//! KAIV_REGISTRY environment variables.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
kaiv — reference toolchain for the kaiv format (Levels 0-2)

USAGE:
    kaiv <COMMAND> [ARGS]

COMMANDS:
    compile  [file.kaiv]              authored -> relational canonical (.raiv)
    denorm   [file.raiv]              relational -> denormalized (.daiv)
    build    [file.kaiv]              authored -> .daiv (compile + denorm)
    schema   [file.saiv]              authored schema -> compiled (.csaiv)
                                      (these four read stdin when no file
                                      is given)
    validate <data> <schema>          validate data against a schema;
                                      data: .daiv (or .kaiv, built first)
                                      schema: .csaiv (or .saiv, compiled first)
    unit     <expr>                   canonicalize a unit expression
    import   [--FORMAT] [--flat] [f]  foreign format -> authored .kaiv;
                                      formats: --json --yaml --toml,
                                      inferred from the file extension
                                      (.json .yaml .yml .toml), the
                                      option required for stdin.
                                      Structures import natively;
                                      only empty containers, anonymous
                                      nested arrays, and non-flat
                                      strings embed as std/enc types.
                                      --flat embeds all containers
                                      (json only)
    export   --FORMAT [file]          kaiv -> foreign format (--json
                                      --yaml --toml); .kaiv is built
                                      first, .daiv/.raiv used as is,
                                      stdin sniffed
    infer    [--name ID] [file]       infer an authored .saiv schema
                                      from an example document (kaiv
                                      or any import format); the
                                      example validates against it
    import-schema [--name ID] [file]  JSON Schema -> authored .saiv, a
                                      sound weakening: constraints kaiv
                                      cannot express are dropped with
                                      // comments, never invented
    help                              this text

Output goes to stdout; diagnostics to stderr. Exit 0 on success/pass,
1 on any error or validation failure.

The nearest kaiv.kaiv up from the working directory configures
registry resolution (KAIV_REGISTRY_* environment variables override).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("kaiv: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<String, String> {
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    match (cmd, &args[1..]) {
        ("compile", rest) if rest.len() <= 1 => {
            let r = resolver()?;
            let data = read_input(rest.first().map(String::as_str))?;
            kaiv::compile_with(&data, &r).map_err(|e| e.to_string())
        }
        ("denorm", rest) if rest.len() <= 1 => {
            let data = read_input(rest.first().map(String::as_str))?;
            let raiv = String::from_utf8(data).map_err(|e| e.to_string())?;
            kaiv::denormalize(&raiv).map_err(|e| e.to_string())
        }
        ("build", rest) if rest.len() <= 1 => {
            let r = resolver()?;
            let data = read_input(rest.first().map(String::as_str))?;
            let raiv = kaiv::compile_with(&data, &r).map_err(|e| e.to_string())?;
            kaiv::denormalize(&raiv).map_err(|e| e.to_string())
        }
        ("schema", rest) if rest.len() <= 1 => {
            let r = resolver()?;
            let data = read_input(rest.first().map(String::as_str))?;
            kaiv::compile_schema_with(&data, &r).map_err(|e| e.to_string())
        }
        ("validate", [data, schema]) => {
            let r = resolver()?;
            let csaiv = if schema.ends_with(".csaiv") {
                String::from_utf8(read(schema)?).map_err(|e| e.to_string())?
            } else {
                kaiv::compile_schema_with(&read(schema)?, &r).map_err(|e| e.to_string())?
            };
            let compiled = kaiv::parse_csaiv(&csaiv).map_err(|e| e.to_string())?;
            let daiv = canonical_input(Some(data))?;
            match kaiv::validate(&daiv, &compiled) {
                Ok(()) => Ok("pass\n".into()),
                Err(e) => Err(e.name().to_string()),
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
            let canonical = canonical_input(path.as_deref())?;
            kaiv::infer::infer(&canonical, &name).map_err(|e| e.to_string())
        }
        ("import-schema", rest) => {
            let mut name = None;
            let mut path = None;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--name" => name = Some(it.next().ok_or("--name needs a value")?.clone()),
                    "--json" => {} // the only schema format today
                    f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
                    p if path.is_none() => path = Some(p.to_string()),
                    p => return Err(format!("unexpected argument: {p}")),
                }
            }
            let name = name.unwrap_or_else(|| "imported".to_string());
            let data = read_input(path.as_deref())?;
            kaiv::jsonschema::import(&data, &name).map_err(|e| e.to_string())
        }
        ("unit", [expr]) => kaiv::unit::canonicalize(expr)
            .map(|c| format!("{c}\n"))
            .ok_or_else(|| format!("invalid unit expression: {expr}")),
        ("import", rest) => {
            let (fmt, flat, path) = parse_fmt_args(rest)?;
            let fmt = match (fmt, &path) {
                (Some(f), _) => f,
                (None, Some(p)) => match ext_format(p) {
                    Some(f) => f.to_string(),
                    None => return Err(format!("cannot infer format from {p}")),
                },
                (None, None) => {
                    return Err("stdin import requires a format option (e.g. --json)".into())
                }
            };
            let data = read_input(path.as_deref())?;
            match (fmt.as_str(), flat) {
                ("json", false) => kaiv::json::import(&data).map_err(|e| e.to_string()),
                ("json", true) => kaiv::json::import_flat(&data).map_err(|e| e.to_string()),
                ("yaml", false) => kaiv::yaml::import(&data).map_err(|e| e.to_string()),
                ("toml", false) => kaiv::toml::import(&data).map_err(|e| e.to_string()),
                (f @ ("yaml" | "toml"), true) => Err(format!("--flat is json-only (got --{f})")),
                (other, _) => Err(format!("unsupported import format: {other}")),
            }
        }
        ("export", rest) => {
            let (fmt, flat, path) = parse_fmt_args(rest)?;
            if flat {
                return Err("--flat is an import option".into());
            }
            let fmt = fmt.ok_or("export requires a format option (e.g. --json)")?;
            let canonical = canonical_input(path.as_deref())?;
            match fmt.as_str() {
                "json" => kaiv::json::export(&canonical).map_err(|e| e.to_string()),
                "yaml" => kaiv::yaml::export(&canonical).map_err(|e| e.to_string()),
                "toml" => kaiv::toml::export(&canonical).map_err(|e| e.to_string()),
                other => Err(format!("unsupported export format: {other}")),
            }
        }
        ("help" | "--help" | "-h", _) => Ok(format!("{USAGE}\n")),
        (cmd, _) => Err(format!(
            "unknown or malformed command: {cmd} (try `kaiv help`)"
        )),
    }
}

fn read(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))
}

/// `[--FORMAT] [--flat] [path]` for import/export.
fn parse_fmt_args(rest: &[String]) -> Result<(Option<String>, bool, Option<String>), String> {
    let mut fmt = None;
    let mut flat = false;
    let mut path = None;
    for a in rest {
        match a.as_str() {
            "--json" => fmt = Some("json".to_string()),
            "--yaml" => fmt = Some("yaml".to_string()),
            "--toml" => fmt = Some("toml".to_string()),
            "--flat" => flat = true,
            f if f.starts_with("--") => return Err(format!("unknown option: {f}")),
            p if path.is_none() => path = Some(p.to_string()),
            p => return Err(format!("unexpected argument: {p}")),
        }
    }
    Ok((fmt, flat, path))
}

/// Canonical kaiv text from a path or stdin: foreign formats import
/// first, authored kaiv builds, canonical passes through.
fn canonical_input(path: Option<&str>) -> Result<String, String> {
    let data = read_input(path)?;
    let r = resolver()?;
    let authored = match path.and_then(ext_format) {
        Some("json") => Some(kaiv::json::import(&data).map_err(|e| e.to_string())?),
        Some("yaml") => Some(kaiv::yaml::import(&data).map_err(|e| e.to_string())?),
        Some("toml") => Some(kaiv::toml::import(&data).map_err(|e| e.to_string())?),
        _ => None,
    };
    let text = match authored {
        Some(a) => a,
        None => {
            let t = String::from_utf8(data).map_err(|e| e.to_string())?;
            if is_canonical(&t, path) {
                return Ok(t);
            }
            t
        }
    };
    let raiv = kaiv::compile_with(text.as_bytes(), &r).map_err(|e| e.to_string())?;
    kaiv::denormalize(&raiv).map_err(|e| e.to_string())
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
        if p.ends_with(".daiv") || p.ends_with(".raiv") {
            return true;
        }
        if p.ends_with(".kaiv") {
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
        return match (s.find('\''), s.find('=')) {
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
