# kaiv-rs

Reference implementation of the **kaiv** format (Levels 0–2) in
Rust. A cargo workspace with two crates:

- **`kaiv`** — the library. Zero dependencies; the certifiable
  reference pipeline.
- **`kaiv-cli`** — the `kaiv` command-line binary, a thin shell
  over the library. Kept out of the library crate so its (future)
  dependencies never reach library consumers.

kaiv is an immutable structural type system for data at rest; the
specification and design documents live in the sibling `spec`
repository. The library implements the build pipeline:

```
.kaiv --[Compiler]--> .raiv --[Denormalizer]--> .daiv
.saiv --[schema compiler]--> .csaiv
.daiv + .csaiv --[Validator]--> pass / fail
```

## CLI

```
kaiv compile  <file.kaiv>       authored -> relational canonical (.raiv)
kaiv denorm   <file.raiv>       relational -> denormalized (.daiv)
kaiv build    <file.kaiv>       authored -> .daiv (compile + denorm)
kaiv schema   <file.saiv>       authored schema -> compiled (.csaiv)
kaiv validate <data> <schema>   .daiv/.kaiv against .csaiv/.saiv -> pass/error
kaiv unit     <expr>            canonicalize a unit expression
kaiv infer    [--name ID] [f]   infer an authored .saiv from an example
                                document (kaiv or any import format); the
                                example validates against the result
kaiv import-schema [--name] [f] JSON Schema -> authored .saiv, as a sound
                                weakening: inexpressible constraints are
                                dropped with // comments, never invented
kaiv import   [--FMT] [file]    JSON/YAML/TOML -> authored .kaiv: structures
                                import natively (inline ;= := +:= forms where
                                homogeneous and short); only empty containers,
                                anonymous nested arrays, and non-flat strings
                                embed as std/enc/json. Format from the
                                extension (.json .yaml .yml .toml) or the
                                option (required for stdin); --flat embeds
                                all containers (json only)
kaiv export   --FMT [file]      kaiv -> JSON/YAML/TOML (.kaiv built first;
                                .daiv as is)
```

The nearest `kaiv.kaiv` up from the working directory configures
registry resolution (SPEC.md § Layer 2) — the toolchain's own
configuration is a kaiv document — with `KAIV_REGISTRY_*`
environment variables overriding. Exit 0 on success/pass, 1
otherwise; output on stdout, diagnostics on stderr.

## Library modules

| Module | Role |
|---|---|
| `lexer` | Six-rule line classifier + document checks (SPEC.md § Parsing Requirements). Eager model: whole text validated before any token is emitted. |
| `anno` | Constraint grammar, both surface forms (annotation position and constraint-line position), including the alternative-delimiter pattern form `re{sep}…{sep}` (lowered to canonical `/…/`). |
| `rex` | Backtracking matcher for the pinned finite-state regex dialect. |
| `unit` | Compound-unit canonicalization (base-name-sorted factors) and built-in-set membership. |
| `taiv` / `faiv` | Type-library (`.taiv`) and unit-definition (`.faiv`) parsing; `std/core` and `std/enc` ship embedded as real `.taiv` files. |
| `config` / `resolve` | `kaiv.kaiv` Layer 2 configuration (the format bootstrap) and Layer 1/2 registry resolution over filesystem bases. |
| `compiler` | `.kaiv` → `.raiv`: variables, sugar (`+=`, `;=`, `:=`, `+:=`, blocks, maps), `&name` resolution via `.!types`, unit membership via `.!units`. |
| `denorm` | `.raiv` → `.daiv`: `$field` reference resolution, nothing else. |
| `schema` | `.saiv` → `.csaiv`: transitive named-type lowering, constrained-union groups, map entry lines, `;=` vector declarations and `[/@name …]` element blocks with Level 2 table headers (unique/ref/min/max collection constraint lines), `.!schema` inheritance (flat, `/ns`-encapsulated, `/@arr` element-wise; redeclaration narrows in place), `=`/`?=` operators, strict-modifier passthrough. |
| `infer` | Canonical kaiv → an authored `.saiv` the example validates against: types from annotations, `{int,float}` widening, null unions, `;=` vectors, `[/@name]` blocks with per-element-field optionality. |
| `jsonschema` (feature `json`) | JSON Schema → authored `.saiv` as a sound weakening: types/unions, pattern (dialect-checked, `/` escaped), length/range (`exclusiveMin/Max` exact for integers), enum/const, required/defaults, `format` date-time→`std/time`, nested objects, typed maps, scalar vectors and struct-array blocks (`minItems`/`maxItems` graduate to table-header cardinality), local `$ref` inlining; everything else drops with a comment. |
| `validator` | Parallel scan of `.daiv` against `.csaiv`, plus the Level 2 post-scan pass (uniqueness, referential integrity, cardinality). |
| `json` (feature `json`) | JSON import/export with a hand-rolled parser: number tokens and nested containers stay raw source slices, so a compact import/export roundtrip is byte-identical. The library's default build stays zero-dependency; format features are additive. |
| `yaml` / `toml` (features) | Thin adapters over the JSON hub: each converts its parse tree to compact JSON text and reuses the JSON importer/exporter, so every emission rule is shared by construction. Fidelity is semantic (formatting and comments do not survive); TOML datetimes import as strings, and TOML cannot represent null. Deps (`yaml-rust2`, `toml`) gated behind their features. |

## Conformance

The executable definition of "correct" is the conformance tree in
the spec repo (`spec/kaiv/conformance/`). `cargo test` runs it from
`../../spec/kaiv/conformance` (relative to the `kaiv` crate) by
default; override with `KAIV_CONFORMANCE_DIR`.

## Scope and known limits (seed)

Levels 0–2. Type, schema, and unit resolution is
offline (Layers 1–2: `.!registry` + `kaiv.kaiv` over filesystem
bases); an `http(s)` base is a `SchemaResolutionError` — the hosted
Layers 3–4 are not implemented. Also not yet implemented, by design
at this stage: Level 3 locale collation
(`..lex[locale]` values), Level 4 entirely,
map key-pattern clauses, quoted names as interior path
segments, and `..time`/`..ver` span semantics beyond canonical-form
string comparison. Each unimplemented path fails loudly rather than
guessing.

## License

Licensed under either of [Apache License, Version
2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
