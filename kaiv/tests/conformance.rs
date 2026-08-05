//! Runs the conformance tree — the executable definition of
//! correct. The tree is vendored into this repo under
//! `tests/conformance-vectors/` so a fresh clone can run it with
//! no sibling checkout, and so a run is pinned to an identifiable
//! vector set (`VECTORS` names the spec commit it came from).
//! Refresh it with `scripts/refresh-vectors.sh`; the spec repo
//! stays authoritative.
//!
//! `KAIV_CONFORMANCE_DIR` still overrides, for testing against a
//! working spec checkout without vendoring first.

use std::fs;
use std::path::{Path, PathBuf};

fn conformance_dir() -> PathBuf {
    // An empty value counts as unset: `KAIV_CONFORMANCE_DIR=` in a
    // CI env file would otherwise resolve to the empty path and
    // fail every vector with a confusing "not found at ".
    match std::env::var("KAIV_CONFORMANCE_DIR") {
        Ok(d) if !d.is_empty() => return PathBuf::from(d),
        _ => {}
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance-vectors")
}

/// The conformance tree lives in the spec repo, not this one, so a
/// clone without it as a sibling cannot run these tests. Say that
/// once and plainly — six `read_dir` panics naming a path that was
/// never going to exist explain nothing.
fn require_conformance_dir() -> PathBuf {
    let d = conformance_dir();
    if d.is_dir() {
        return d;
    }
    panic!(
        "conformance vectors not found at {}\n\n\
         The tree is vendored into this repo and should be present in\n\
         any clone. If it is missing, restore it with:\n\n    \
         scripts/refresh-vectors.sh [<spec-repo>]\n\n\
         Or point KAIV_CONFORMANCE_DIR at a spec checkout's\n\
         kaiv/conformance directory.",
        d.display()
    )
}

/// A vector directory may carry its own `kaiv.kaiv` (plus a local
/// registry tree) — the offline Layer 1/2 resolution surface. This is
/// also the config-bootstrap test: the harness parses the config with
/// the core pipeline before resolving anything. A `MODE` marker file
/// names a resolution mode the runner must enable (per the
/// conformance README): `registry-strict` or `registry-canonical`.
fn resolver_for(dir: &Path) -> kaiv::Resolver {
    let cfg = dir.join("kaiv.kaiv");
    let mut config = if cfg.exists() {
        kaiv::Config::load(&cfg).unwrap()
    } else {
        kaiv::Config::default()
    };
    match fs::read_to_string(dir.join("MODE")) {
        Ok(m) => match m.trim() {
            "registry-strict" => config.strict_registry = true,
            "registry-canonical" => config.canonical_registry = true,
            other => panic!("{}: unknown MODE {other:?}", dir.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("{}: cannot read MODE: {e}", dir.display()),
    }
    kaiv::Resolver::new(config)
}

/// Is this build's collation backend the full reference (ICU, every
/// locale and `-u-` override the spec recognizes)? Level 3 vectors
/// run unconditionally under it; other builds gate on the LEVEL
/// marker below.
const FULL_COLLATION: bool = cfg!(feature = "collation-icu");

/// The `LEVEL` marker file (conformance README): one line naming the
/// minimum Level the vector requires. Only `3` exists today; an
/// unknown value fails loudly rather than mis-gating the vector.
fn requires_level3(dir: &Path) -> bool {
    match fs::read_to_string(dir.join("LEVEL")) {
        Ok(s) => match s.trim() {
            "3" => true,
            other => panic!("{}: unknown LEVEL {other:?}", dir.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => panic!("{}: cannot read LEVEL: {e}", dir.display()),
    }
}

/// Level gating for a vector directory, per the conformance README:
/// under the full ICU backend every vector runs as written
/// (`Run`); a partial or absent backend runs a LEVEL-3 vector's
/// validate cases against `partial.expected` when the vector
/// carries it (the D-7 honest-partial rule: reject exactly, never
/// order differently), and skips the vector otherwise.
enum Gate {
    Run,
    Partial(String),
    Skip,
}

fn gate(dir: &Path) -> Gate {
    if !requires_level3(dir) || FULL_COLLATION {
        return Gate::Run;
    }
    match fs::read_to_string(dir.join("partial.expected")) {
        Ok(s) => Gate::Partial(s.trim().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Gate::Skip,
        Err(e) => panic!("{}: cannot read partial.expected: {e}", dir.display()),
    }
}

/// Guard against a fifth or mis-named category directory silently going
/// unexercised (e.g. `compile-errors`).
/// The vendored tree must be able to name where it came from —
/// that provenance is the whole difference between a pinned vector
/// set and a directory someone copied once. A tree without it
/// cannot be traced back to a spec commit, so refuse to run.
#[test]
fn the_vendored_tree_records_its_provenance() {
    let dir = require_conformance_dir();
    let marker = dir.join("VECTORS");
    if !marker.is_file() {
        // An explicit KAIV_CONFORMANCE_DIR points at a live spec
        // checkout, which carries no marker; that is the
        // documented override, not a vendored tree.
        assert!(
            std::env::var("KAIV_CONFORMANCE_DIR").is_ok_and(|v| !v.is_empty()),
            "vendored tree at {} has no VECTORS marker — \
             re-run scripts/refresh-vectors.sh",
            dir.display()
        );
        return;
    }
    let text = fs::read_to_string(&marker).expect("VECTORS is readable");
    let commit = text
        .lines()
        .find_map(|l| l.strip_prefix("commit: "))
        .expect("VECTORS names a commit");
    assert_eq!(commit.len(), 40, "commit is a full sha: {commit:?}");
    assert!(
        commit.bytes().all(|b| b.is_ascii_hexdigit()),
        "commit is hex: {commit:?}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("describe: ")),
        "VECTORS names a describe"
    );
}

#[test]
fn all_conformance_categories_are_known() {
    let known = ["valid", "invalid", "schema", "compile-error"];
    for entry in fs::read_dir(require_conformance_dir())
        .unwrap()
        .filter_map(|e| e.ok())
    {
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
        .unwrap_or_else(|_| {
            require_conformance_dir();
            panic!("cannot read {}", p.display())
        })
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
        if requires_level3(&dir) && !FULL_COLLATION {
            continue;
        }
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let input = fs::read(dir.join("input.kaiv")).unwrap();
        // An error vector (expected.error in place of expected.daiv):
        // the build pipeline must fail with the named error — the
        // denorm-error surface (UnitConversionError,
        // DelegationSchemaError, …).
        let epath = dir.join("expected.error");
        if epath.is_file() {
            let want = fs::read_to_string(&epath).unwrap().trim().to_string();
            let resolver = resolver_for(&dir);
            let r = kaiv::compile_with(&input, &resolver)
                .and_then(|raiv| kaiv::denormalize_with(&raiv, &resolver));
            match r {
                Ok(_) => failures.push(format!("{name}: pipeline OK, want {want}")),
                Err(e) => match pipeline_error_name(&e) {
                    Some(got) if got == want => {}
                    Some(got) => failures.push(format!("{name}: got {got}, want {want}")),
                    None => failures.push(format!("{name}: unnamed error {e}, want {want}")),
                },
            }
            continue;
        }
        let expected_daiv = fs::read_to_string(dir.join("expected.daiv")).unwrap();
        // A missing expected.raiv means "same as .daiv except the
        // first line reads .!raiv" (per the README); any OTHER read
        // error is a real problem, not a silent fallback.
        let expected_raiv = match fs::read_to_string(dir.join("expected.raiv")) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => expected_daiv
                .strip_prefix(".!daiv")
                .map(|rest| format!(".!raiv{rest}"))
                .unwrap_or_else(|| panic!("{name}: expected.daiv does not open with .!daiv")),
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
        let partial = match gate(&dir) {
            Gate::Run => None,
            Gate::Partial(e) => Some(e),
            Gate::Skip => continue,
        };
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
        // Compile-only vectors (no validate/ dir) are legal — e.g.
        // the delegation surface before its sub-scan lands (D-10).
        let mut cases: Vec<PathBuf> = match fs::read_dir(&vdir) {
            Err(_) => Vec::new(),
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "daiv"))
                .collect(),
        };
        cases.sort();
        // Every `*.expected` must have a paired `*.daiv`, and the dir
        // must hold at least one case — otherwise a mis-named or missing
        // file would silently pass the vector.
        for entry in fs::read_dir(&vdir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if p.extension().is_some_and(|x| x == "expected") && !p.with_extension("daiv").is_file()
            {
                failures.push(format!(
                    "{name}/{}: .expected has no paired .daiv",
                    p.file_stem().unwrap().to_string_lossy()
                ));
            }
        }
        assert!(
            !cases.is_empty() || !dir.join("validate").is_dir(),
            "{name}: validate/ has no .daiv cases"
        );
        for case in cases {
            let cname = case.file_stem().unwrap().to_string_lossy().to_string();
            let daiv = fs::read_to_string(&case).unwrap();
            // An honest-partial (or absent) collation backend expects
            // the vector's partial outcome for every case in place of
            // the per-case expectation (the D-7 rule).
            let want = match &partial {
                Some(e) => e.clone(),
                None => fs::read_to_string(case.with_extension("expected"))
                    .unwrap()
                    .trim()
                    .to_string(),
            };
            let got = match kaiv::validate(&daiv, &schema) {
                Ok(()) => "pass".to_string(),
                Err(e) => e.error.name().to_string(),
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
        kaiv::PipelineError::App(a) => Some(a.error.name()),
        kaiv::PipelineError::Other(_) => None,
        other => other.name(),
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
        if requires_level3(&dir) && !FULL_COLLATION {
            continue;
        }
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
        if requires_level3(&dir) && !FULL_COLLATION {
            continue;
        }
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

#[test]
fn fmt_preserves_compilation() {
    // `kaiv fmt` is semantics-neutral: for every valid vector the
    // formatted source compiles to the identical .raiv, and the
    // formatter is idempotent.
    let mut failures = Vec::new();
    for dir in subdirs(&conformance_dir().join("valid")) {
        if requires_level3(&dir) && !FULL_COLLATION {
            continue;
        }
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        if dir.join("expected.error").is_file() {
            continue; // error vectors carry no golden .raiv
        }
        let input = fs::read_to_string(dir.join("input.kaiv")).unwrap();
        let expected_raiv = {
            let daiv = fs::read_to_string(dir.join("expected.daiv")).unwrap();
            match fs::read_to_string(dir.join("expected.raiv")) {
                Ok(s) => s,
                Err(_) => daiv
                    .strip_prefix(".!daiv")
                    .map(|rest| format!(".!raiv{rest}"))
                    .unwrap(),
            }
        };
        let resolver = resolver_for(&dir);
        let fmted = match kaiv::format_data(&input) {
            Ok(f) => f,
            Err(e) => {
                failures.push(format!("{name}: fmt failed: {e:?}"));
                continue;
            }
        };
        match kaiv::format_data(&fmted) {
            Ok(again) if again == fmted => {}
            Ok(again) => failures.push(format!(
                "{name}: fmt not idempotent\n--- once ---\n{fmted}--- twice ---\n{again}"
            )),
            Err(e) => failures.push(format!("{name}: refmt failed: {e:?}")),
        }
        match kaiv::compile_with(fmted.as_bytes(), &resolver) {
            Ok(raiv) => {
                if raiv != expected_raiv {
                    failures.push(format!(
                        "{name}: fmt changed semantics\n--- fmt ---\n{fmted}--- got ---\n{raiv}--- want ---\n{expected_raiv}"
                    ));
                }
            }
            Err(e) => failures.push(format!(
                "{name}: fmt output does not compile: {e:?}\n--- fmt ---\n{fmted}"
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn unbuild_round_trips() {
    // Lifting a canonical .daiv to authored kaiv and rebuilding it
    // reproduces the identical .daiv.
    let mut failures = Vec::new();
    for dir in subdirs(&conformance_dir().join("valid")) {
        if requires_level3(&dir) && !FULL_COLLATION {
            continue;
        }
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        if dir.join("expected.error").is_file() {
            continue; // error vectors carry no golden .daiv
        }
        let daiv = fs::read_to_string(dir.join("expected.daiv")).unwrap();
        let resolver = resolver_for(&dir);
        let unbuilt = match kaiv::unbuild(&daiv) {
            Ok(l) => l,
            Err(e) => {
                failures.push(format!("{name}: unbuild failed: {e:?}"));
                continue;
            }
        };
        let rebuilt = kaiv::compile_with(unbuilt.as_bytes(), &resolver)
            .and_then(|r| kaiv::denormalize_with(&r, &resolver));
        match rebuilt {
            Ok(d2) if d2 == daiv => {}
            Ok(d2) => failures.push(format!(
                "{name}: unbuild round-trip drifted\n--- unbuilt ---\n{unbuilt}--- rebuilt ---\n{d2}--- want ---\n{daiv}"
            )),
            Err(e) => failures.push(format!(
                "{name}: unbuilt form does not build: {e:?}\n--- unbuilt ---\n{unbuilt}"
            )),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
