//! Type-library resolution, Layers 1–4 (SPEC.md § Type Registry
//! Resolution): document-level `.!registry` overrides, then `kaiv.kaiv`
//! build-time configuration, then — behind the default-on `net`
//! feature — the hosted registries (redirect aliasing + the Layer 4
//! default hosts). Without `net`, a lookup that would need the network
//! is a `SchemaResolutionError`. `std/core` is embedded and never
//! resolved.

use crate::config::Config;
use crate::error::{AppError, PipelineError};
use crate::faiv::{parse_faiv, UnitLib};
use crate::taiv::{embedded, parse_taiv, TypeDef, TypeLib};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

#[derive(Default)]
pub struct Resolver {
    pub config: Config,
    cache: RefCell<HashMap<String, TypeLib>>,
    unit_cache: RefCell<HashMap<String, UnitLib>>,
    /// Preloaded artifact bytes, `(lib, ext)` → bytes; consulted
    /// before any Layer 1/2/4 base. Lets an embedding host (e.g. a
    /// registry gate) feed dependencies without filesystem or
    /// network access.
    sources: RefCell<HashMap<(String, String), Vec<u8>>>,
    /// Artifacts a lookup failed to obtain, for the host's
    /// fetch-and-retry loop.
    missing: RefCell<BTreeSet<(String, String)>>,
}

impl Resolver {
    pub fn new(config: Config) -> Self {
        Resolver {
            config,
            ..Resolver::default()
        }
    }

    /// Core-only resolver: no configuration, only embedded `std/core`.
    pub fn offline() -> Self {
        Self::default()
    }

    /// Supply `{lib}.{ext}` bytes ahead of resolution. Preloaded
    /// sources win over every base layer, so a host can satisfy
    /// dependency lookups from memory alone.
    pub fn preload(&self, lib: &str, ext: &str, bytes: Vec<u8>) {
        self.sources
            .borrow_mut()
            .insert((lib.to_string(), ext.to_string()), bytes);
    }

    /// Drain the `(lib, ext)` pairs whose resolution has failed so
    /// far. A host fetches these, `preload`s them, and retries.
    pub fn take_missing(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.missing.borrow_mut())
            .into_iter()
            .collect()
    }

    /// Load and cache `lib` if needed. `layer1` is the document's
    /// `.!registry` overrides (prefix → base), checked before the
    /// Layer 2 configuration. No borrow is held across the load, so
    /// lookups may recurse (transitive lowering).
    fn ensure(&self, lib: &str, layer1: &[(String, String)]) -> Result<(), PipelineError> {
        if embedded(lib).is_some() || self.cache.borrow().contains_key(lib) {
            return Ok(());
        }
        let loaded = self.load(lib, layer1)?;
        self.cache.borrow_mut().insert(lib.to_string(), loaded);
        Ok(())
    }

    /// Does `lib` define `name`?
    pub fn contains(
        &self,
        lib: &str,
        name: &str,
        layer1: &[(String, String)],
    ) -> Result<bool, PipelineError> {
        self.ensure(lib, layer1)?;
        if let Some(l) = embedded(lib) {
            return Ok(l.types.contains_key(name));
        }
        Ok(self.cache.borrow()[lib].types.contains_key(name))
    }

    /// The (unlowered) definition of `lib`'s type `name` — constraint
    /// items plus the type's default.
    pub fn def(
        &self,
        lib: &str,
        name: &str,
        layer1: &[(String, String)],
    ) -> Result<TypeDef, PipelineError> {
        self.ensure(lib, layer1)?;
        let cloned = if let Some(l) = embedded(lib) {
            l.types.get(name).cloned()
        } else {
            self.cache.borrow()[lib].types.get(name).cloned()
        };
        cloned.ok_or(PipelineError::App(AppError::SchemaResolution))
    }

    fn load(&self, lib: &str, layer1: &[(String, String)]) -> Result<TypeLib, PipelineError> {
        let bytes = self.read_artifact(lib, layer1, "taiv")?;
        let parsed = parse_taiv(&bytes)?;
        if parsed.library != lib {
            // The file's .!taiv identity must match the requested path.
            return Err(PipelineError::App(AppError::SchemaResolution));
        }
        Ok(parsed)
    }

    /// The unit names (definitions and aliases) of a `.faiv` library
    /// (SPEC.md § Unit Definition Files), for `.!units` imports.
    pub fn unit_names(
        &self,
        lib: &str,
        layer1: &[(String, String)],
    ) -> Result<BTreeSet<String>, PipelineError> {
        if !self.unit_cache.borrow().contains_key(lib) {
            let bytes = self.read_artifact(lib, layer1, "faiv")?;
            let parsed = parse_faiv(&bytes)?;
            if parsed.library != lib {
                // The file's .!faiv identity must match the path.
                return Err(PipelineError::App(AppError::SchemaResolution));
            }
            self.unit_cache.borrow_mut().insert(lib.to_string(), parsed);
        }
        Ok(self.unit_cache.borrow()[lib]
            .units
            .keys()
            .cloned()
            .collect())
    }

    /// The unit definitions of a `.faiv` library, for consumers
    /// that need the conversion factors themselves (e.g. a query
    /// engine scaling custom units to base via
    /// [`crate::unit::scale_with`]). Same load path and cache as
    /// [`Self::unit_names`].
    pub fn unit_defs(
        &self,
        lib: &str,
        layer1: &[(String, String)],
    ) -> Result<BTreeMap<String, crate::faiv::UnitDef>, PipelineError> {
        self.unit_names(lib, layer1)?; // load + cache
        Ok(self.unit_cache.borrow()[lib].units.clone())
    }

    /// Locate and read `{base}/{lib}.{ext}` via Layer 1 (`.!registry`)
    /// then Layer 2 (`kaiv.kaiv`); filesystem bases only.
    /// Read a schema source (`.saiv`) for `.!schema` inheritance
    /// resolution (SPEC.md § Encapsulated Hub Schema Extension).
    pub fn schema_bytes(
        &self,
        lib: &str,
        layer1: &[(String, String)],
    ) -> Result<Vec<u8>, PipelineError> {
        self.read_artifact(lib, layer1, "saiv")
    }

    /// Read a compiled schema (`.csaiv`) for `.!schema` validation
    /// of a canonical document.
    pub fn csaiv_bytes(
        &self,
        lib: &str,
        layer1: &[(String, String)],
    ) -> Result<Vec<u8>, PipelineError> {
        self.read_artifact(lib, layer1, "csaiv")
    }

    fn read_artifact(
        &self,
        lib: &str,
        layer1: &[(String, String)],
        ext: &str,
    ) -> Result<Vec<u8>, PipelineError> {
        if let Some(bytes) = self
            .sources
            .borrow()
            .get(&(lib.to_string(), ext.to_string()))
        {
            return Ok(bytes.clone());
        }
        let read = self.read_artifact_base(lib, layer1, ext);
        if read.is_err() {
            self.missing
                .borrow_mut()
                .insert((lib.to_string(), ext.to_string()));
        }
        read
    }

    /// Locate and read `{base}/{lib}.{ext}` via the base layers.
    fn read_artifact_base(
        &self,
        lib: &str,
        layer1: &[(String, String)],
        ext: &str,
    ) -> Result<Vec<u8>, PipelineError> {
        let prefix = lib.split('/').next().unwrap_or(lib);
        // Layer 1 (.!registry) wins over Layer 2 (kaiv.kaiv).
        // Layer 1 (.!registry) wins over Layer 2 (kaiv.kaiv, whose
        // `default` key wins over the Layer 4 default host).
        let base = layer1
            .iter()
            .find(|(p, _)| p == prefix)
            .map(|(_, b)| b.as_str())
            .or_else(|| self.config.base_for(prefix))
            .or_else(|| layer4_default(ext))
            .ok_or(PipelineError::App(AppError::SchemaResolution))?;
        if base.starts_with("http://") || base.starts_with("https://") {
            #[cfg(feature = "net")]
            {
                let url = format!("{}/{lib}.{ext}", base.trim_end_matches('/'));
                let root = self
                    .config
                    .cache_dir
                    .clone()
                    .or_else(crate::net::default_cache_root);
                return crate::net::fetch(&url, root.as_deref(), crate::net::env_offline());
            }
            // Without the `net` feature, network resolution is
            // unimplemented (embedded/offline builds).
            #[cfg(not(feature = "net"))]
            return Err(PipelineError::App(AppError::SchemaResolution));
        }
        let mut path = PathBuf::from(base);
        if path.is_relative() {
            if let Some(dir) = &self.config.base_dir {
                path = dir.join(path);
            }
        }
        path.push(format!("{lib}.{ext}"));
        std::fs::read(&path).map_err(|_| PipelineError::App(AppError::SchemaResolution))
    }
}

/// Layer 4 default registry hosts, by artifact kind (SPEC.md
/// § Layer 4). Reached only when no Layer 1/2 entry matches.
/// The kaiv.io subdomains are the live canonical hosts; the
/// SPEC's `k*aiv.com` production domains take over when those
/// zones go live (these constants are the single switch point).
fn layer4_default(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "taiv" => "https://t.kaiv.io",
        "faiv" => "https://f.kaiv.io",
        // Mappings live on the schema registry — they are edges
        // between schemas (SPEC.md § Mappings).
        "saiv" | "csaiv" | "maiv" => "https://s.kaiv.io",
        _ => return None,
    })
}

/// Resolve an authored `&name` against the document's `.!types`
/// imports, in declaration order: `std/core` first (short canonical
/// form), then each import. Found in none → `SchemaResolutionError`;
/// found in several → ambiguity error.
pub fn resolve_named(
    name: &str,
    imports: &[String],
    resolver: &Resolver,
    layer1: &[(String, String)],
) -> Result<String, PipelineError> {
    if crate::anno::CORE_TYPES.contains(&name) {
        return Ok(name.to_string()); // std/core keeps the short form
    }
    let mut found: Option<&str> = None;
    for lib in imports {
        if resolver.contains(lib, name, layer1)? {
            if let Some(prev) = found {
                return Err(PipelineError::Other(format!(
                    "ambiguous named type &{name}: defined in {prev} and {lib}"
                )));
            }
            found = Some(lib);
        }
    }
    match found {
        Some(lib) => Ok(format!("{lib}/{name}")),
        None => Err(PipelineError::App(AppError::SchemaResolution)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACME_NET: &[u8] = b".!taiv 1 acme/net\n\n{tcp,udp}\n&proto=tcp\n";

    /// A resolver whose every base lookup is a filesystem miss —
    /// keeps these tests off the Layer 4 network hosts.
    fn dead_end() -> Resolver {
        let mut config = Config::default();
        config
            .registries
            .insert("default".into(), "/nonexistent/kaiv-test".into());
        Resolver::new(config)
    }

    #[test]
    fn preload_wins_over_bases() {
        let r = dead_end();
        r.preload("acme/net", "taiv", ACME_NET.to_vec());
        assert!(r.contains("acme/net", "proto", &[]).unwrap());
        assert!(!r.contains("acme/net", "absent", &[]).unwrap());
    }

    #[test]
    fn failed_lookups_are_recorded() {
        let r = dead_end();
        assert!(r.contains("acme/net", "proto", &[]).is_err());
        assert!(r.unit_names("astro/units", &[]).is_err());
        let missing = r.take_missing();
        assert_eq!(
            missing,
            vec![
                ("acme/net".to_string(), "taiv".to_string()),
                ("astro/units".to_string(), "faiv".to_string()),
            ]
        );
        // Drained: a second take starts empty.
        assert!(r.take_missing().is_empty());
    }

    #[test]
    fn preload_then_retry_succeeds() {
        let r = dead_end();
        assert!(r.def("acme/net", "proto", &[]).is_err());
        for (lib, ext) in r.take_missing() {
            assert_eq!((lib.as_str(), ext.as_str()), ("acme/net", "taiv"));
            r.preload(&lib, &ext, ACME_NET.to_vec());
        }
        let def = r.def("acme/net", "proto", &[]).unwrap();
        assert_eq!(def.default, "tcp");
    }
}
