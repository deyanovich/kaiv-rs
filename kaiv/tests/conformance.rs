//! Runs the conformance tree from the spec repo. Location: the
//! `KAIV_CONFORMANCE_DIR` env var, or `../../spec/kaiv/conformance`
//! relative to this crate.

use std::fs;
use std::path::{Path, PathBuf};

fn conformance_dir() -> PathBuf {
    if let Ok(d) = std::env::var("KAIV_CONFORMANCE_DIR") {
        return PathBuf::from(d);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/kaiv/conformance")
}

/// A vector directory may carry its own `kaiv.kaiv` (plus a local
/// registry tree) — the offline Layer 1/2 resolution surface. This is
/// also the config-bootstrap test: the harness parses the config with
/// the core pipeline before resolving anything.
fn resolver_for(dir: &Path) -> kaiv::Resolver {
    let cfg = dir.join("kaiv.kaiv");
    if cfg.exists() {
        kaiv::Resolver::new(kaiv::Config::load(&cfg).unwrap())
    } else {
        kaiv::Resolver::offline()
    }
}

/// Guard against a fifth or mis-named category directory silently going
/// unexercised (e.g. `compile-errors`).
#[test]
fn all_conformance_categories_are_known() {
    let known = ["valid", "invalid", "schema", "compile-error"];
    for entry in fs::read_dir(conformance_dir()).unwrap().filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                known.contains(&name.as_str()),
                "unknown conformance category directory: {name}"
            );
        }
    }
}

fn subdirs(p: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(p)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    v.sort();
    assert!(!v.is_empty(), "no vectors found under {}", p.display());
    v
}

#[test]
fn valid_vectors() {
    let mut failures = Vec::new();
    for dir in subdirs(&conformance_dir().join("valid")) {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let input = fs::read(dir.join("input.kaiv")).unwrap();
        let expected_daiv = fs::read_to_string(dir.join("expected.daiv")).unwrap();
        // A missing expected.raiv means "same as .daiv" (per the README);
        // any OTHER read error is a real problem, not a silent fallback.
        let expected_raiv = match fs::read_to_string(dir.join("expected.raiv")) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => expected_daiv.clone(),
            Err(e) => panic!("{name}: cannot read expected.raiv: {e}"),
        };

        let resolver = resolver_for(&dir);
        match kaiv::compile_with(&input, &resolver) {
            Ok(raiv) => {
                if raiv != expected_raiv {
                    failures.push(format!(
                        "{name}: raiv mismatch\n--- got ---\n{raiv}--- want ---\n{expected_raiv}"
                    ));
                    continue;
                }
                match kaiv::denormalize_with(&raiv, &resolver) {
                    Ok(daiv) => {
                        if daiv != expected_daiv {
                            failures.push(format!(
                                "{name}: daiv mismatch\n--- got ---\n{daiv}--- want ---\n{expected_daiv}"
                            ));
                        }
                    }
                    Err(e) => failures.push(format!("{name}: denormalize error: {e}")),
                }
            }
            Err(e) => failures.push(format!("{name}: compile error: {e}")),
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn schema_vectors() {
    let mut failures = Vec::new();
    for dir in subdirs(&conformance_dir().join("schema")) {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let saiv = fs::read(dir.join("schema.saiv")).unwrap();
        let expected_csaiv = fs::read_to_string(dir.join("expected.csaiv")).unwrap();

        let resolver = resolver_for(&dir);
        let csaiv = match kaiv::compile_schema_with(&saiv, &resolver) {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("{name}: schema compile error: {e}"));
                continue;
            }
        };
        if csaiv != expected_csaiv {
            failures.push(format!(
                "{name}: csaiv mismatch\n--- got ---\n{csaiv}--- want ---\n{expected_csaiv}"
            ));
            continue;
        }
        let schema = match kaiv::parse_csaiv(&csaiv) {
            Ok(s) => s,
            Err(e) => {
                failures.push(format!("{name}: csaiv parse error: {e}"));
                continue;
            }
        };
        let vdir = dir.join("validate");
        let mut cases: Vec<PathBuf> = fs::read_dir(&vdir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "daiv"))
            .collect();
        cases.sort();
        // Every `*.expected` must have a paired `*.daiv`, and the dir
        // must hold at least one case — otherwise a mis-named or missing
        // file would silently pass the vector.
        for entry in fs::read_dir(&vdir).unwrap().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().is_some_and(|x| x == "expected") && !p.with_extension("daiv").is_file() {
                failures.push(format!(
                    "{name}/{}: .expected has no paired .daiv",
                    p.file_stem().unwrap().to_string_lossy()
                ));
            }
        }
        assert!(!cases.is_empty(), "{name}: validate/ has no .daiv cases");
        for case in cases {
            let cname = case.file_stem().unwrap().to_string_lossy().to_string();
            let daiv = fs::read_to_string(&case).unwrap();
            let want = fs::read_to_string(case.with_extension("expected"))
                .unwrap()
                .trim()
                .to_string();
            let got = match kaiv::validate(&daiv, &schema) {
                Ok(()) => "pass".to_string(),
                Err(e) => e.name().to_string(),
            };
            if got != want {
                failures.push(format!("{name}/{cname}: got {got}, want {want}"));
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

/// The pinned error name a pipeline error carries, if any (lexer or
/// application errors; compiler-internal `Other`s are unnamed).
fn pipeline_error_name(e: &kaiv::PipelineError) -> Option<&'static str> {
    match e {
        kaiv::PipelineError::Lex(l) => Some(l.error.name()),
        kaiv::PipelineError::App(a) => Some(a.name()),
        kaiv::PipelineError::Other(_) => None,
    }
}

/// Compile-time application errors (past the Lexer): a `.kaiv` runs
/// the data pipeline, a `.saiv` the schema compiler, each expected to
/// fail with the pinned error name in `expected.error`.
#[test]
fn compile_error_vectors() {
    // Required like the other three categories: subdirs() fails loudly
    // if compile-error/ is absent or empty.
    let root = conformance_dir().join("compile-error");
    let mut failures = Vec::new();
    for dir in subdirs(&root) {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let want = fs::read_to_string(dir.join("expected.error"))
            .unwrap()
            .trim()
            .to_string();
        let resolver = resolver_for(&dir);
        let result = if dir.join("input.saiv").exists() {
            let saiv = fs::read(dir.join("input.saiv")).unwrap();
            kaiv::compile_schema_with(&saiv, &resolver).map(|_| ())
        } else {
            // A `.kaiv` runs the full build: Compiler then
            // Denormalizer (schema-aware materialization included),
            // so build-time errors like RequiredFieldSchemaError are
            // reachable.
            let input = fs::read(dir.join("input.kaiv")).unwrap();
            kaiv::compile_with(&input, &resolver)
                .and_then(|raiv| kaiv::denormalize_with(&raiv, &resolver))
                .map(|_| ())
        };
        match result {
            Ok(()) => failures.push(format!("{name}: compiled OK, want {want}")),
            Err(e) => match pipeline_error_name(&e) {
                Some(got) if got == want => {}
                Some(got) => failures.push(format!("{name}: got {got}, want {want}")),
                None => failures.push(format!("{name}: unnamed error {e}, want {want}")),
            },
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}

#[test]
fn invalid_vectors() {
    let mut failures = Vec::new();
    for dir in subdirs(&conformance_dir().join("invalid")) {
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let (path, kind) = if dir.join("input.kaiv").exists() {
            (dir.join("input.kaiv"), kaiv::FileKind::Data)
        } else {
            (dir.join("input.saiv"), kaiv::FileKind::Schema)
        };
        let input = fs::read(&path).unwrap();
        let want = fs::read_to_string(dir.join("expected.error"))
            .unwrap()
            .trim()
            .to_string();
        match kaiv::lex(&input, kind) {
            Ok(_) => failures.push(format!("{name}: lexed OK, want {want}")),
            Err(e) => {
                let got = e.error.name();
                if got != want {
                    failures.push(format!("{name}: got {got}, want {want}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}
