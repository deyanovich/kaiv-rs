//! Layer 2 build-time configuration: `kaiv.kaiv` (SPEC.md § Layer 2).
//! The config file is the format's own bootstrap — a kaiv document
//! restricted to the Level 0 scalar subset, parsed by the core
//! pipeline before any type resolution exists, so the configuration
//! that drives resolution never needs resolution itself.

use crate::error::PipelineError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Build-time resolution configuration: the Layer 2 registry map
/// and the resolution-mode switches. Load one from a `kaiv.kaiv`
/// file with [`Config::load`].
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Registry prefix → base (URL or filesystem path). The reserved
    /// key `default` overrides the Layer 4 default for unmatched
    /// prefixes.
    pub registries: BTreeMap<String, String>,
    /// Directory containing `kaiv.kaiv` — relative bases resolve here.
    pub base_dir: Option<PathBuf>,
    /// Network-cache root (`/cache::dir`, relative to `base_dir`).
    /// None → `KAIV_CACHE_DIR` / XDG default.
    pub cache_dir: Option<PathBuf>,
    /// Strict resolution mode (SPEC.md § Type Registry Resolution):
    /// a document-level `.!registry` declaration that would determine
    /// an artifact's base raises `RegistryStrictError` instead of
    /// resolving. Set via `KAIV_REGISTRY_STRICT`.
    pub strict_registry: bool,
    /// Canonical resolution mode: Layers 1–2 are ignored entirely
    /// and every artifact resolves through the Layer 4 default
    /// registry — no declaration, configuration file, or environment
    /// variable can redirect resolution. Subsumes strict mode (a
    /// Layer 1 declaration is dormant by construction). Set via
    /// `KAIV_REGISTRY_CANONICAL`.
    pub canonical_registry: bool,
}

impl Config {
    /// Parse `kaiv.kaiv` text via the core Level 0 pipeline.
    pub fn parse(text: &[u8], base_dir: Option<PathBuf>) -> Result<Self, PipelineError> {
        let raiv = crate::compile(text)?;
        let daiv = crate::denorm::denormalize(&raiv)?;
        let mut registries = BTreeMap::new();
        let mut cache_dir = None;
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
            } else if np == "/cache::dir" && !v.is_empty() {
                let mut p = PathBuf::from(v);
                if p.is_relative() {
                    if let Some(dir) = &base_dir {
                        p = dir.join(p);
                    }
                }
                cache_dir = Some(p);
            }
        }
        Ok(Config {
            registries,
            base_dir,
            cache_dir,
            strict_registry: false,
            canonical_registry: false,
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
            if k == "KAIV_REGISTRY_STRICT" || k == "KAIV_REGISTRY_CANONICAL" {
                // Mode toggles, not prefix overrides (same
                // truthiness as KAIV_OFFLINE).
                if !v.is_empty() && v != "0" {
                    if k == "KAIV_REGISTRY_STRICT" {
                        self.strict_registry = true;
                    } else {
                        self.canonical_registry = true;
                    }
                }
            } else if let Some(prefix) = k.strip_prefix("KAIV_REGISTRY_") {
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

    #[test]
    fn env_strict_toggle_is_not_a_prefix_override() {
        std::env::set_var("KAIV_REGISTRY_STRICT", "1");
        std::env::set_var("KAIV_REGISTRY_CANONICAL", "1");
        let mut c = Config::default();
        c.apply_env();
        std::env::remove_var("KAIV_REGISTRY_STRICT");
        std::env::remove_var("KAIV_REGISTRY_CANONICAL");
        assert!(c.strict_registry);
        assert!(c.canonical_registry);
        // The toggles must not be misread as KAIV_REGISTRY_{PREFIX}.
        assert!(!c.registries.contains_key("strict"));
        assert!(!c.registries.contains_key("canonical"));
    }
}
