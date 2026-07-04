//! Layer 2 build-time configuration: `kaiv.kaiv` (SPEC.md § Layer 2).
//! The config file is the format's own bootstrap — a kaiv document
//! restricted to the Level 0 scalar subset, parsed by the core
//! pipeline before any type resolution exists, so the configuration
//! that drives resolution never needs resolution itself.

use crate::error::PipelineError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Registry prefix → base (URL or filesystem path). The reserved
    /// key `default` overrides the Layer 4 default for unmatched
    /// prefixes.
    pub registries: BTreeMap<String, String>,
    /// Directory containing `kaiv.kaiv` — relative bases resolve here.
    pub base_dir: Option<PathBuf>,
}

impl Config {
    /// Parse `kaiv.kaiv` text via the core Level 0 pipeline.
    pub fn parse(text: &[u8], base_dir: Option<PathBuf>) -> Result<Self, PipelineError> {
        let raiv = crate::compile(text)?;
        let daiv = crate::denorm::denormalize(&raiv)?;
        let mut registries = BTreeMap::new();
        for line in daiv.lines() {
            // Canonical: !str'/registries::name=value
            let Some(tick) = line.find('\'') else {
                continue;
            };
            let rest = &line[tick + 1..];
            let Some((np, v)) = rest.split_once('=') else {
                continue;
            };
            if let Some(name) = np.strip_prefix("/registries::") {
                registries.insert(unquote(name), v.to_string());
            }
        }
        Ok(Config {
            registries,
            base_dir,
        })
    }

    /// Load a `kaiv.kaiv` file; relative bases resolve against its
    /// directory.
    pub fn load(path: &Path) -> Result<Self, PipelineError> {
        let bytes = std::fs::read(path)
            .map_err(|e| PipelineError::Other(format!("cannot read {}: {e}", path.display())))?;
        Self::parse(&bytes, path.parent().map(Path::to_path_buf))
    }

    /// Overlay `KAIV_REGISTRY_{PREFIX}` / `KAIV_REGISTRY` environment
    /// variables — they override the file (SPEC.md § Layer 2). Not
    /// applied automatically: conformance runs stay deterministic.
    pub fn apply_env(&mut self) {
        for (k, v) in std::env::vars() {
            if let Some(prefix) = k.strip_prefix("KAIV_REGISTRY_") {
                self.registries.insert(prefix.to_lowercase(), v);
            } else if k == "KAIV_REGISTRY" {
                self.registries.insert("default".into(), v);
            }
        }
    }

    /// Base for a library-path prefix: exact entry, else `default`.
    pub fn base_for(&self, prefix: &str) -> Option<&str> {
        self.registries
            .get(prefix)
            .or_else(|| self.registries.get("default"))
            .map(String::as_str)
    }
}

/// Strip a quoted name's quotes and undo `""` doubling; bare names
/// pass through. (`"acme-corp"` → `acme-corp`.)
fn unquote(name: &str) -> String {
    match name.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_parse() {
        let text = b"# kaiv.kaiv\n.!kaiv 1\n\n/registries::acme=https://types.acme.com\n/registries::\"acme-corp\"=./types\n/registries::default=https://ktaiv.com\n";
        let c = Config::parse(text, None).unwrap();
        assert_eq!(
            c.registries.get("acme").map(String::as_str),
            Some("https://types.acme.com")
        );
        assert_eq!(
            c.registries.get("acme-corp").map(String::as_str),
            Some("./types")
        );
        assert_eq!(c.base_for("acme"), Some("https://types.acme.com"));
        assert_eq!(c.base_for("unknown"), Some("https://ktaiv.com"));
    }
}
