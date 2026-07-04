//! The Denormalizer: `.raiv` → `.daiv`. Resolves `$field` /
//! `$path::field` references left-to-right against the field table —
//! and nothing else (SPEC.md § Architectural Impact). No forward
//! references.

use crate::error::PipelineError;
use std::collections::HashMap;

pub fn denormalize(raiv: &str) -> Result<String, PipelineError> {
    let mut table: HashMap<String, String> = HashMap::new();
    let mut out = String::new();
    for line in raiv.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let eol = &line[body.len()..];
        if let Some(tick) = body.find('\'') {
            if let Some(eq_rel) = body[tick..].find('=') {
                let eq = tick + eq_rel;
                let namepath = &body[tick + 1..eq];
                let value = &body[eq + 1..];
                let resolved = if let Some(r) = value.strip_prefix('$') {
                    if r.starts_with('.') {
                        return Err(PipelineError::Other(format!(
                            "unresolved variable in .raiv: {value}"
                        )));
                    }
                    let target = canonical_ref(r);
                    table.get(&target).cloned().ok_or_else(|| {
                        PipelineError::Other(format!(
                            "forward or dangling field reference: {value}"
                        ))
                    })?
                } else {
                    value.to_string()
                };
                table.insert(namepath.to_string(), resolved.clone());
                out.push_str(&body[..eq + 1]);
                out.push_str(&resolved);
                out.push_str(eol);
                continue;
            }
        }
        out.push_str(line);
    }
    Ok(out)
}

/// `server/api::host` → `/server/api::host`; `field` → `::field`.
fn canonical_ref(r: &str) -> String {
    if r.contains("::") {
        if r.starts_with('/') {
            r.to_string()
        } else {
            format!("/{r}")
        }
    } else {
        format!("::{r}")
    }
}
