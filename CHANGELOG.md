CHANGELOG
=========

All notable changes to kaiv-rs will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
but note that pre-1.0 releases may not adhere strictly to all
guidelines.


[0.2.0] - 2026-07-05
--------------------

Complete rewrite. `kaiv` 0.1.0 was the Era 1 "kv format" CLI tool;
0.2.0 is the reference implementation of the current kaiv format
(spec `v1.0.0-alpha` line), reorganized as a workspace: the `kaiv`
library (zero dependencies in its core; `json`/`yaml`/`toml` adapters
feature-gated) and the `kaiv-cli` binary.

### Library

- Full Level 0–2 pipeline: Lexer (six-rule classifier, document
  checks), Compiler (`.kaiv` → `.raiv`), Denormalizer (`.raiv` →
  `.daiv`), schema compiler (`.saiv` → `.csaiv`), Validator
  (parallel scan + Level 2 post-scan: uniqueness, referential
  integrity, cardinality).
- Type system: named types over the constraint triple, tagged
  unions, maps, units (canonicalized compound expressions,
  currencies, custom `.faiv` libraries), default-value cascade,
  provenance requirements, schema inheritance (`.!schema`), Level 2
  table declarations, `re{sep}…{sep}` pattern literals.
- Embedded `std/core`, `std/enc`, `std/time`, `std/num` libraries;
  offline registry resolution (`.!registry` + `kaiv.kaiv`).
- Import/export: JSON (byte-identical compact roundtrips), YAML,
  TOML (all four datetime flavors); schema inference (`infer`);
  JSON Schema import as a sound weakening.
- Conformance: `cargo test` runs the spec repo's vector tree
  (development setup; the tree is not part of the package).

### CLI (`kaiv-cli`, new crate)

- `compile`, `denorm`, `build`, `schema`, `validate`, `unit`,
  `import`, `export`, `infer`, `import-schema`; stdin fallbacks and
  extension-based format inference.
