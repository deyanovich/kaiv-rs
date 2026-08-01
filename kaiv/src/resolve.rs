//! Type-library resolution, Layers 1–4 (SPEC.md § Type Registry
//! Resolution): document-level `.!registry` overrides, then `kaiv.kaiv`
//! build-time configuration, then — behind the default-on `net`
//! feature — the hosted registries (redirect aliasing + the Layer 4
//! default hosts). Without `net`, a lookup that would need the network
//! is a `SchemaResolutionError`. `std/core` is embedded and never
//! resolved.
//!
//! Every resolution records a [`ResolutionEvent`] (drained via
//! [`Resolver::take_resolutions`]), and under strict mode
//! (`Config::strict_registry`) a Layer 1 win is refused with
//! `RegistryStrictError` — an untrusted document must not choose
//! where its own contract comes from. Canonical mode
//! (`Config::canonical_registry`) goes further: Layers 1–2 are
//! ignored and everything resolves through the Layer 4 defaults.

use crate::config::Config;
use crate::error::{AppError, PipelineError};
use crate::faiv::{parse_faiv, UnitLib};
use crate::taiv::{embedded, parse_taiv, TypeDef, TypeLib};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

/// Which layer won an artifact resolution. `Layer2` covers both the
/// `kaiv.kaiv` file and `KAIV_REGISTRY_*` environment overrides
/// (merged into one map before resolution, so indistinguishable
/// here). Layer 3 redirect aliasing happens transparently inside the
/// HTTP fetch and is not reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionLayer {
    /// Host-injected bytes ([`Resolver::preload`]); outrank every base.
    Preload,
    /// Document-level `.!registry` declaration.
    Layer1,
    /// Build-time configuration (`kaiv.kaiv` / environment).
    Layer2,
    /// Default registry host.
    Layer4,
}

/// Provenance record for one resolved artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionEvent {
    /// Library path as referenced (e.g. `hub/invoice`).
    pub lib: String,
    /// Artifact extension (`taiv`, `faiv`, `saiv`, `csaiv`, `maiv`).
    pub ext: String,
    pub layer: ResolutionLayer,
    /// The matched base (URL or filesystem path; empty for `Preload`).
    pub base: String,
    /// Full URL or filesystem path actually read (empty for `Preload`).
    pub location: String,
}

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
    /// Provenance log: one event per artifact resolution (including
    /// the Layer 1 event behind a strict-mode refusal).
    events: RefCell<Vec<ResolutionEvent>>,
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

    /// Drain the provenance log: one [`ResolutionEvent`] per artifact
    /// resolution so far, in resolution order.
    pub fn take_resolutions(&self) -> Vec<ResolutionEvent> {
        std::mem::take(&mut *self.events.borrow_mut())
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

    /// The alias → primary-name map of an imported unit library,
    /// chains followed (canonical lines never carry an alias —
    /// SPEC.md § Unit Definition Files).
    pub fn unit_aliases(
        &self,
        lib: &str,
        layer1: &[(String, String)],
    ) -> Result<std::collections::BTreeMap<String, String>, PipelineError> {
        self.unit_names(lib, layer1)?; // ensure cached
        let cache = self.unit_cache.borrow();
        let parsed = &cache[lib];
        let mut map = std::collections::BTreeMap::new();
        for (name, def) in &parsed.units {
            let Some(mut target) = def.alias_of.clone() else {
                continue;
            };
            let mut hops = 0;
            while let Some(next) = parsed.units.get(&target).and_then(|d| d.alias_of.clone()) {
                if hops >= 8 {
                    break;
                }
                target = next;
                hops += 1;
            }
            map.insert(name.clone(), target);
        }
        Ok(map)
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
            self.events.borrow_mut().push(ResolutionEvent {
                lib: lib.to_string(),
                ext: ext.to_string(),
                layer: ResolutionLayer::Preload,
                base: String::new(),
                location: String::new(),
            });
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

    /// The base layer cascade for `{lib}.{ext}`, without reading:
    /// Layer 1 (`.!registry`) wins over Layer 2 (`kaiv.kaiv`, whose
    /// `default` key wins over the Layer 4 default host). Canonical
    /// mode consults Layer 4 alone — no declaration, configuration
    /// file, or environment variable can redirect resolution. Strict
    /// mode inverts Layers 1–2: the consumer's configuration shadows
    /// the document's declaration for prefixes it covers, so that
    /// vendoring via `kaiv.kaiv` is the sanctioned way to resolve a
    /// document whose uncovered Layer 1 win would be refused.
    fn select_base<'a>(
        &'a self,
        prefix: &str,
        layer1: &'a [(String, String)],
        ext: &str,
    ) -> Option<(&'a str, ResolutionLayer)> {
        if self.config.canonical_registry {
            return layer4_default(ext).map(|b| (b, ResolutionLayer::Layer4));
        }
        let l1 = || {
            layer1
                .iter()
                .find(|(p, _)| p == prefix)
                .map(|(_, b)| (b.as_str(), ResolutionLayer::Layer1))
        };
        let l2 = || {
            self.config
                .base_for(prefix)
                .map(|b| (b, ResolutionLayer::Layer2))
        };
        let base = if self.config.strict_registry {
            l2().or_else(l1)
        } else {
            l1().or_else(l2)
        };
        base.or_else(|| layer4_default(ext).map(|b| (b, ResolutionLayer::Layer4)))
    }

    /// Locate and read `{base}/{lib}.{ext}` via the base layers.
    fn read_artifact_base(
        &self,
        lib: &str,
        layer1: &[(String, String)],
        ext: &str,
    ) -> Result<Vec<u8>, PipelineError> {
        let prefix = lib.split('/').next().unwrap_or(lib);
        let (base, layer) = self
            .select_base(prefix, layer1, ext)
            .ok_or(PipelineError::App(AppError::SchemaResolution))?;
        let mut event = ResolutionEvent {
            lib: lib.to_string(),
            ext: ext.to_string(),
            layer,
            base: base.to_string(),
            location: String::new(),
        };
        if layer == ResolutionLayer::Layer1 && self.config.strict_registry {
            // Strict mode: the document must not choose where its own
            // contract comes from. Record the would-be resolution and
            // refuse before any read or fetch.
            self.events.borrow_mut().push(event);
            return Err(PipelineError::App(AppError::RegistryStrict));
        }
        if base.starts_with("http://") || base.starts_with("https://") {
            #[cfg(feature = "net")]
            {
                let url = format!("{}/{lib}.{ext}", base.trim_end_matches('/'));
                let root = self
                    .config
                    .cache_dir
                    .clone()
                    .or_else(crate::net::default_cache_root);
                let bytes = crate::net::fetch(&url, root.as_deref(), crate::net::env_offline())?;
                event.location = url;
                self.events.borrow_mut().push(event);
                return Ok(bytes);
            }
            // Without the `net` feature, network resolution is
            // unimplemented (embedded/offline builds).
            #[cfg(not(feature = "net"))]
            {
                let _ = event;
                return Err(PipelineError::App(AppError::SchemaResolution));
            }
        }
        let mut path = PathBuf::from(base);
        if path.is_relative() {
            if let Some(dir) = &self.config.base_dir {
                path = dir.join(path);
            }
        }
        path.push(format!("{lib}.{ext}"));
        let bytes =
            std::fs::read(&path).map_err(|_| PipelineError::App(AppError::SchemaResolution))?;
        event.location = path.display().to_string();
        self.events.borrow_mut().push(event);
        Ok(bytes)
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

    const ACME_NET: &[u8] = b".!taiv acme/net\n\n{tcp,udp}\n&proto=tcp\n";

    /// A resolver whose every base lookup is a filesystem miss —
    /// keeps these tests off the Layer 4 network hosts.
    fn dead_end() -> Resolver {
        let mut config = Config::default();
        config
            .registries
            .insert("default".into(), "/nonexistent/kaiv-test".into());
        Resolver::new(config)
    }

    /// A temp directory holding `acme/net.taiv`, as a Layer 1/2 base.
    fn temp_lib(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kaiv-resolve-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("acme")).unwrap();
        std::fs::write(dir.join("acme/net.taiv"), ACME_NET).unwrap();
        dir
    }

    #[test]
    fn preload_wins_over_bases() {
        let r = dead_end();
        r.preload("acme/net", "taiv", ACME_NET.to_vec());
        assert!(r.contains("acme/net", "proto", &[]).unwrap());
        assert!(!r.contains("acme/net", "absent", &[]).unwrap());
        let events = r.take_resolutions();
        assert_eq!(events.len(), 1); // second lookup hits the lib cache
        assert_eq!(events[0].layer, ResolutionLayer::Preload);
    }

    #[test]
    fn layer1_resolution_is_reported() {
        let dir = temp_lib("layer1");
        let r = dead_end();
        let layer1 = vec![("acme".to_string(), dir.display().to_string())];
        assert!(r.contains("acme/net", "proto", &layer1).unwrap());
        let events = r.take_resolutions();
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!((ev.lib.as_str(), ev.ext.as_str()), ("acme/net", "taiv"));
        assert_eq!(ev.layer, ResolutionLayer::Layer1);
        assert_eq!(ev.base, dir.display().to_string());
        assert!(ev.location.ends_with("net.taiv"));
        // Drained: a second take starts empty.
        assert!(r.take_resolutions().is_empty());
    }

    #[test]
    fn strict_mode_refuses_layer1_before_reading() {
        let r = Resolver::new(Config {
            strict_registry: true,
            ..Config::default()
        });
        // The base is a dead end: a read attempt would surface
        // SchemaResolutionError, so RegistryStrictError proves the
        // refusal came before any read.
        let layer1 = vec![("acme".to_string(), "/nonexistent/kaiv-test".to_string())];
        match r.contains("acme/net", "proto", &layer1) {
            Err(PipelineError::App(AppError::RegistryStrict)) => {}
            other => panic!("expected RegistryStrictError, got {other:?}"),
        }
        let events = r.take_resolutions();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].layer, ResolutionLayer::Layer1);
        assert!(events[0].location.is_empty());
    }

    #[test]
    fn strict_mode_leaves_dormant_layer1_and_layer2_alone() {
        let dir = temp_lib("dormant");
        let mut config = Config {
            strict_registry: true,
            ..Config::default()
        };
        config
            .registries
            .insert("acme".into(), dir.display().to_string());
        let r = Resolver::new(config);
        // The document's .!registry names a different prefix — a
        // dormant declaration must not trip strict mode.
        let layer1 = vec![("other".to_string(), "/nonexistent/kaiv-test".to_string())];
        assert!(r.contains("acme/net", "proto", &layer1).unwrap());
        let events = r.take_resolutions();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].layer, ResolutionLayer::Layer2);
    }

    #[test]
    fn strict_mode_lets_vendored_layer2_shadow_layer1() {
        let dir = temp_lib("vendored");
        let mut config = Config {
            strict_registry: true,
            ..Config::default()
        };
        config
            .registries
            .insert("acme".into(), dir.display().to_string());
        let r = Resolver::new(config);
        // The document declares the same prefix the consumer has
        // vendored — the consumer's configuration shadows it, so
        // strict mode has nothing to refuse.
        let layer1 = vec![("acme".to_string(), "/nonexistent/kaiv-test".to_string())];
        assert!(r.contains("acme/net", "proto", &layer1).unwrap());
        let events = r.take_resolutions();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].layer, ResolutionLayer::Layer2);
    }

    #[test]
    fn canonical_mode_ignores_layers_1_and_2() {
        let mut config = Config {
            canonical_registry: true,
            ..Config::default()
        };
        config.registries.insert("acme".into(), "/layer2".into());
        let r = Resolver::new(config);
        let layer1 = vec![("acme".to_string(), "/layer1".to_string())];
        // Both overrides present; canonical mode still selects the
        // Layer 4 default host.
        let (base, layer) = r.select_base("acme", &layer1, "taiv").unwrap();
        assert_eq!(layer, ResolutionLayer::Layer4);
        assert_eq!(base, "https://t.kaiv.io");
        // Preload (host-injected, not an override) still wins.
        r.preload("acme/net", "taiv", ACME_NET.to_vec());
        assert!(r.contains("acme/net", "proto", &layer1).unwrap());
        assert_eq!(r.take_resolutions()[0].layer, ResolutionLayer::Preload);
    }

    #[test]
    fn canonical_subsumes_strict() {
        let r = Resolver::new(Config {
            canonical_registry: true,
            strict_registry: true,
            ..Config::default()
        });
        let layer1 = vec![("acme".to_string(), "/layer1".to_string())];
        // Layer 1 never wins under canonical mode, so strict mode
        // has nothing to refuse.
        let (_, layer) = r.select_base("acme", &layer1, "csaiv").unwrap();
        assert_eq!(layer, ResolutionLayer::Layer4);
    }

    #[test]
    fn strict_mode_allows_preload() {
        let r = Resolver::new(Config {
            strict_registry: true,
            ..Config::default()
        });
        r.preload("acme/net", "taiv", ACME_NET.to_vec());
        // Preload outranks Layer 1, so a registry-gate host keeps
        // working under strict mode.
        let layer1 = vec![("acme".to_string(), "/nonexistent/kaiv-test".to_string())];
        assert!(r.contains("acme/net", "proto", &layer1).unwrap());
        assert_eq!(r.take_resolutions()[0].layer, ResolutionLayer::Preload);
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
