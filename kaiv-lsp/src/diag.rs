//! Per-file diagnostics: dispatch a document through the pipeline
//! stage its extension calls for and map the first error to an LSP
//! `Diagnostic`. Errors carry a 1-based line (0 = whole document) and
//! a stable spec error name — no column information, so ranges span
//! the whole line.

use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use std::path::{Path, PathBuf};

/// Diagnostics for a document, or `None` when the extension carries
/// no pipeline to run (`.qaiv`, unknown). `Some(vec![])` means the
/// document is clean and stale diagnostics must be cleared.
pub fn check(uri: &str, text: &str) -> Option<Vec<Diagnostic>> {
    let ext = extension(uri)?;
    let resolver = resolver_for(uri_to_path(uri).as_deref());
    let r = &resolver;
    let result: Result<(), Diagnostic> = match ext.as_str() {
        "kaiv" => kaiv::compile_with(text.as_bytes(), r)
            .and_then(|raiv| kaiv::denormalize_with(&raiv, r))
            .map(drop)
            .map_err(pipeline_diag),
        "raiv" => kaiv::lexer::expect_kind(text, "raiv")
            .map_err(lex_diag)
            .and_then(|_| {
                kaiv::denormalize_with(text, r)
                    .map(drop)
                    .map_err(pipeline_diag)
            }),
        "daiv" => kaiv::lexer::expect_kind(text, "daiv")
            .map_err(lex_diag)
            .and_then(|_| {
                kaiv::lex(text.as_bytes(), kaiv::FileKind::Data)
                    .map(drop)
                    .map_err(lex_diag)
            }),
        "saiv" => kaiv::compile_schema_with(text.as_bytes(), r)
            .map(drop)
            .map_err(pipeline_diag),
        "csaiv" => kaiv::parse_csaiv(text).map(drop).map_err(pipeline_diag),
        "taiv" => kaiv::check_type_lib(text.as_bytes(), r)
            .map(drop)
            .map_err(pipeline_diag),
        "faiv" => kaiv::lex(text.as_bytes(), kaiv::FileKind::UnitLib)
            .map(drop)
            .map_err(lex_diag),
        "maiv" => kaiv::lex(text.as_bytes(), kaiv::FileKind::Mapping)
            .map(drop)
            .map_err(lex_diag),
        "msaiv" => kaiv::lex(text.as_bytes(), kaiv::FileKind::Schema)
            .map(drop)
            .map_err(lex_diag),
        _ => return None,
    };
    Some(match result {
        Ok(()) => Vec::new(),
        Err(d) => vec![d],
    })
}

fn extension(uri: &str) -> Option<String> {
    let name = uri.rsplit(['/', '\\']).next()?;
    let (_, ext) = name.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

/// `file://` URI → filesystem path (percent-decoded). Non-file URIs
/// yield `None`; diagnostics still run, just with the offline
/// resolver.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mirror the CLI: the nearest `kaiv.kaiv` up from the document's
/// directory configures resolution; otherwise resolve offline.
fn resolver_for(path: Option<&Path>) -> kaiv::Resolver {
    let mut dir = path.and_then(Path::parent);
    while let Some(d) = dir {
        let candidate = d.join("kaiv.kaiv");
        if candidate.is_file() {
            if let Ok(config) = kaiv::Config::load(&candidate) {
                return kaiv::Resolver::new(config);
            }
        }
        dir = d.parent();
    }
    kaiv::Resolver::offline()
}

fn pipeline_diag(e: kaiv::PipelineError) -> Diagnostic {
    match e {
        kaiv::PipelineError::Lex(l) => lex_diag(l),
        kaiv::PipelineError::App(a) => diagnostic(0, Some(a.name()), a.name().to_string()),
        kaiv::PipelineError::Other(s) => diagnostic(0, None, s),
    }
}

fn lex_diag(e: kaiv::LexErrorAt) -> Diagnostic {
    diagnostic(e.line, Some(e.error.name()), e.error.name().to_string())
}

#[allow(dead_code)] // wired in when cross-file validation lands
fn app_diag(e: kaiv::AppErrorAt) -> Diagnostic {
    let mut message = e.error.name().to_string();
    if !e.context.is_empty() {
        message.push_str(": ");
        message.push_str(&e.context);
    }
    diagnostic(e.line, Some(e.error.name()), message)
}

fn diagnostic(line: usize, code: Option<&str>, mut message: String) -> Diagnostic {
    if line == 0 {
        message.push_str(" (whole document)");
    }
    Diagnostic {
        range: line_range(line),
        severity: Some(DiagnosticSeverity::ERROR),
        code: code.map(|c| NumberOrString::String(c.to_string())),
        source: Some("kaiv".to_string()),
        message,
        ..Default::default()
    }
}

/// Whole-line range for a 1-based line; clients clamp the past-EOL
/// end character. Line 0 (whole-document) anchors at the file start.
fn line_range(line: usize) -> Range {
    if line >= 1 {
        let l = (line - 1) as u32;
        Range::new(Position::new(l, 0), Position::new(l, u32::MAX))
    } else {
        Range::new(Position::new(0, 0), Position::new(0, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_document_is_clean() {
        let d = check("file:///tmp/a.kaiv", "host=localhost\n").unwrap();
        assert!(d.is_empty());
    }

    #[test]
    fn missing_final_eol_is_whole_document() {
        let d = check("file:///tmp/a.kaiv", "host=localhost").unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].code,
            Some(NumberOrString::String("MISSING_FINAL_EOL_ERROR".into()))
        );
        assert_eq!(d[0].range.start, Position::new(0, 0));
        assert_eq!(d[0].range.end, Position::new(0, 0));
        assert!(d[0].message.contains("whole document"));
    }

    #[test]
    fn line_errors_are_one_based() {
        let d = check("file:///tmp/a.kaiv", "host=x\n!int[1;2]\n").unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(
            d[0].code,
            Some(NumberOrString::String("INVALID_CONSTRAINT_ERROR".into()))
        );
        // 1-based line 2 → 0-based line 1, whole-line range.
        assert_eq!(d[0].range.start, Position::new(1, 0));
        assert_eq!(d[0].range.end.line, 1);
    }

    #[test]
    fn daiv_requires_format_declaration() {
        let d = check("file:///tmp/a.daiv", "host=x\n").unwrap();
        assert_eq!(
            d[0].code,
            Some(NumberOrString::String("FORMAT_KIND_ERROR".into()))
        );
    }

    #[test]
    fn schema_pipeline_runs_for_saiv() {
        let d = check(
            "file:///tmp/s.saiv",
            ".!saiv 1 acme/x\n!str\nhost=\n",
        )
        .unwrap();
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn qaiv_has_no_pipeline() {
        assert!(check("file:///tmp/q.qaiv", "anything\n").is_none());
    }

    #[test]
    fn percent_decoding_paths() {
        assert_eq!(
            uri_to_path("file:///a%20b/c.kaiv"),
            Some(PathBuf::from("/a b/c.kaiv"))
        );
    }
}
