//! Type-library resolution, Layers 1–2 (SPEC.md § Type Registry
//! Resolution): document-level `.!registry` overrides, then `kaiv.kaiv`
//! build-time configuration. Filesystem bases only — the hosted
//! Layers 3–4 (registry redirects, ktaiv.com default) are network
//! services this seed does not implement, so a lookup that would need
//! them is a `SchemaResolutionError`. `std/core` is embedded and never
//! resolved.

use crate::config::Config;
use crate::error::{AppError, PipelineError};
use crate::faiv::{parse_faiv, UnitLib};
use crate::taiv::{embedded, parse_taiv, TypeDef, TypeLib};
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

#[derive(Default)]
pub struct Resolver {
    pub config: Config,
    cache: RefCell<HashMap<String, TypeLib>>,
    unit_cache: RefCell<HashMap<String, UnitLib>>,
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
            // The file's .!kaivtype identity must match the requested path.
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
                // The file's .!kaivunit identity must match the path.
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

    fn read_artifact(
        &self,
        lib: &str,
        layer1: &[(String, String)],
        ext: &str,
    ) -> Result<Vec<u8>, PipelineError> {
        let prefix = lib.split('/').next().unwrap_or(lib);
        // Layer 1 (.!registry) wins over Layer 2 (kaiv.kaiv).
        let base = layer1
            .iter()
            .find(|(p, _)| p == prefix)
            .map(|(_, b)| b.as_str())
            .or_else(|| self.config.base_for(prefix))
            .ok_or(PipelineError::App(AppError::SchemaResolution))?;
        if base.starts_with("http://") || base.starts_with("https://") {
            // Layers 3-4 are network services, unimplemented offline.
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
