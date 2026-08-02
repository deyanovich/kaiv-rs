# kaiv-rs

Reference implementation of the **kaiv** format (Levels 0–3) in
Rust. A cargo workspace with four crates:

- **`kaiv`** — the library. The certifiable reference pipeline;
  the Levels 0–2 core is zero-dependency, Level 3 collation rides
  ICU4X behind the default-on `collation-icu` feature (with a
  lightweight `collation-colligo` alternative).
- **`kaiv-cli`** — the `kaiv` command-line binary, a thin shell
  over the library. Kept out of the library crate so its (future)
  dependencies never reach library consumers.
- **`kaiv-lsp`** — a diagnostics-only language server over the
  pipeline (stdio, synchronous): each open document runs the
  stage its extension calls for and the first error publishes as
  a whole-line diagnostic carrying the spec error name. Consumed
  by the editor plugins in the sibling `editors` and `kaiv-vim`
  repos; install with `cargo install --path kaiv-lsp`.
- **`kaiv-wasm`** — the browser playground entry point behind
  the sibling `demo` repo (demo.kaiv.io). Built for the browser
  via wasm-pack, or `cargo build -p kaiv-wasm`.

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
kaiv fmt      [file] [--check]  format an authoring file into the standard
                                style, in place (stdin prints to stdout);
                                --check exits 1 if it is not already
                                formatted. Canonical .daiv/.raiv have
                                exactly one spelling, so fmt refuses them
                                and points at unbuild
kaiv unbuild  [file]            canonical .daiv/.raiv -> authored .kaiv,
                                build's inverse direction. A view, not a
                                round trip: sugar the compiler resolved
                                away (comments, variables, references) does
                                not come back; .raiv preserves more,
                                authored units included
kaiv mapping  validate <m.maiv> check a mapping against its two schemas
                                (namepaths, overrides, required-field
                                completeness)
kaiv mapping  apply <m> [data]  source document -> target .daiv
kaiv mapping  compose <a> <b>   compose two mappings (a: B<-A, b: C<-B)
                                into C<-A; the .!via trail records each hop
kaiv login    [email]           sign in to the kaiv registries (idaiv): an
                                emailed one-time link approves this device
kaiv whoami                     the signed-in account
kaiv logout                     revoke and forget the stored session
kaiv infer    [--name ID] [f]   infer an authored .saiv from an example
                                document (kaiv or any import format); the
                                example validates against the result
kaiv import-schema [--name] [f] foreign schema -> authored .saiv, as a sound
                                weakening: inexpressible constraints are
                                dropped with // comments, never invented.
                                Formats: JSON Schema, .proto, Avro Schema
                                (.avsc), GraphQL SDL, XSD — from the flag
                                (--json --proto --avro --graphql --xsd) or
                                the extension; --message picks the root
                                message/type/element when several exist
kaiv import   [--FMT] [file]    foreign format -> authored .kaiv: structures
                                import natively (inline ;= := +:= forms where
                                homogeneous and short); only empty containers,
                                anonymous nested arrays, non-flat strings,
                                XML mixed content, and binary byte strings
                                embed as std/enc types. Formats: JSON, YAML,
                                TOML, XML, CBOR, Avro, Protocol Buffers
                                (--proto needs --schema <file.proto>, plus
                                --message <Name> for multi-message schemas),
                                and ASN.1 BER/DER (--asn1; PEM auto-strips);
                                inferred from the extension (.json .yaml .yml
                                .toml .xml .cbor .avro .pb .binpb .der .pem
                                .crt .cer), the option required for stdin;
                                --flat embeds all containers (json only)
kaiv export   --FMT [file]      kaiv -> any of the same formats (.kaiv built
                                first; .daiv as is; the binary formats write
                                raw bytes; --proto needs --schema/--message
                                like import)
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
| `unit` | Compound-unit canonicalization (base-name-sorted factors), built-in-set membership, and exact same-dimension conversion. |
| `error` | The spec's error catalog: `LexError` / `AppError` and their positioned forms, each `name()` the exact string the conformance vectors pin. |
| `builder` | `DaivBuilder` / `KaivBuilder`: emit canonical or authored kaiv programmatically, without going through text. |
| `doc` | Reader over a parsed `.daiv`: `Doc::parse`, then `View` lookups by relative namepath (`::field`, `/ns`, `/@arr`), plus the `FromDaiv` trait. |
| `fmt` | `format_data` / `format_plain` (authored files into the standard style) and `unbuild` (canonical back to authored — the inverse direction of build, not a round trip). |
| `maiv` | `.maiv` mappings: parse, static publish-time validation against source and target schemas, application, and composition. |
| `serde` (feature `serde`) | kaiv as a serde data format: `Serialize` straight to canonical lines, `Deserialize` back out of a `Doc` view. |
| `collate` (features `collation-icu` / `collation-colligo`) | Level 3 locale-aware collation behind one backend or the other. |
| `taiv` / `faiv` | Type-library (`.taiv`) and unit-definition (`.faiv`) parsing; `std/core` and `std/enc` ship embedded as real `.taiv` files. |
| `config` / `resolve` / `net` | `kaiv.kaiv` Layer 2 configuration (the format bootstrap) and Layer 1–4 registry resolution: filesystem bases, and — behind the opt-in `net` feature — `http(s)` fetching with redirect aliasing, Layer 4 default hosts, and an immutable on-disk cache. |
| `compiler` | `.kaiv` → `.raiv`: variables, sugar (`+=`, `;=`, `:=`, `+:=`, blocks, maps), `&name` resolution via `.!types`, unit membership via `.!units`. |
| `denorm` | `.raiv` → `.daiv`: `$field` reference resolution, nothing else. |
| `schema` | `.saiv` → `.csaiv`: transitive named-type lowering, constrained-union groups, map entry lines and keyed map blocks (lowered to the `[key::…]` collection clause + entry-count bounds), `;=` vector declarations and `[/@name …]` element blocks with Level 2 table headers (unique/ref/min/max collection constraint lines), `.!schema` inheritance (flat, `/ns`-encapsulated, `/@arr` element-wise; redeclaration narrows in place), `=`/`?=` operators, strict-modifier passthrough. |
| `infer` | Canonical kaiv → an authored `.saiv` the example validates against: types from annotations, `{int,float}` widening, null unions, `;=` vectors, `[/@name]` blocks with per-element-field optionality. |
| `jsonschema` (feature `json`) | JSON Schema → authored `.saiv` as a sound weakening: types/unions, pattern (dialect-checked, `/` escaped), length/range (`exclusiveMin/Max` exact for integers), enum/const, required/defaults, `format` date-time→`std/time`, nested objects, typed maps, scalar vectors and struct-array blocks (`minItems`/`maxItems` graduate to table-header cardinality), local `$ref` inlining; everything else drops with a comment. |
| schema converters (`proto`/`avro`/`graphql`/`xsd` features) | `.proto`, Avro Schema, GraphQL SDL, and XSD → authored `.saiv`, under the same sound-weakening contract. The proto/Avro converters reuse their data converters' schema parsers; XSD parses with the crate's XML parser; GraphQL adds a hand-rolled SDL parser. Sized integers carry exact wire ranges; open proto3 enums emit `!int|str{…}`, closed Avro/GraphQL enums `!str{…}`; Avro/XSD time types ride `std/time`; XSD facets map like JSON Schema constraints and attributes emit as `@name` fields; recursion and inexpressible shapes drop with notes. Where a data converter shares the source schema, decoded documents validate against the converted schema — for flat strings: non-flat strings (EOL/NUL, leading `$`) ride the `std/enc/json` embed channel, which `str`-typed fields do not admit; widen such fields by hand to `!str\|std/enc/json` (a documented limitation, not a data-conversion loss). |
| `validator` | Parallel scan of `.daiv` against `.csaiv`, plus the Level 2 post-scan pass (uniqueness, referential integrity, cardinality, map entry bounds and key grammar). |
| `json` (feature `json`) | JSON import/export with a hand-rolled parser: number tokens and nested containers stay raw source slices, so a compact import/export roundtrip is byte-identical. The library's default build stays zero-dependency; format features are additive. |
| `yaml` / `toml` / `xml` / `cbor` / `avro` (features) | Thin adapters over the value hub: each converts its parse tree to the shared interchange tree and reuses the shared emission engine, so every rule (inline forms, embedding, the line budget) is format-agnostic by construction. Fidelity is semantic (formatting and comments do not survive). TOML datetimes ride the typed channel as `std/time` named types; TOML cannot represent null or integers beyond i64. XML maps attributes to `@name` members, text-beside-attributes to `#text`, repeated siblings to arrays; mixed content embeds verbatim as `std/enc/xml` and splices back; the well-formed-subset parser is hand-rolled (zero deps, no DTD). CBOR (RFC 8949) and Avro (Object Container Files) are the binary formats, both hand-rolled and zero-dep: byte strings ride as `std/enc/bin`, datetimes as `std/time` (CBOR tag 0; the Avro date/time/timestamp logical types, micros on export), integers are exact at any width (CBOR bignums; Avro decimal logical type), CBOR output is preferred serialization, Avro reads the null and deflate codecs (hand-rolled inflate) and export infers a schema from the tree (field-wise record unification with null-unions). Deps (`yaml-rust2`, `toml`) gated behind their features. |
| `proto` (feature) | Protocol Buffers, hand-rolled and zero-dep, schema-driven in both directions (the wire format is not self-describing): a proto3 `.proto` parser (nested/recursive messages, enums, oneof, maps, packages; `import` statements rejected) plus wire decode/encode against a chosen message. Nested messages are namespaces, repeated fields arrays (packed and unpacked), maps stringify their keys, enum numbers become symbol names, bytes ride as `std/enc/bin`. Absent fields stay absent (proto3 cannot tell absence from default); unknown field numbers skip, like every protobuf decoder; null is an export error (proto3 has none). Wire round trips are byte-identical to protoc's encoding. |
| `asn1` (feature) | ASN.1 BER/DER import and DER export, hand-rolled and zero-dep, schema-less: universal tags drive the mapping, PEM armor strips automatically. Structures are positional, so every element is a single-member wrapper namespace naming its type (`seq`/`set`/`int`/`oid`/`bits`/`utc`/`printable`/`c0`/… with `u9p`-style fallbacks — total coverage). INTEGERs exact at any width; UTCTime/GeneralizedTime ride `std/time`. DER round-trips byte-identically (verified on openssl-generated X.509 certificates); BER-only forms normalize to DER. A generic multi-member namespace has no DER form — ASN.1 field names live in schemas, not on the wire. |

## Conformance

The executable definition of "correct" is the conformance tree in
the spec repo (`spec/kaiv/conformance/`). `cargo test` runs it from
`../../spec/kaiv/conformance` (relative to the `kaiv` crate) by
default; override with `KAIV_CONFORMANCE_DIR`. Level 3 vectors
carry a `LEVEL` marker: under the default ICU backend they run as
written; a build without it substitutes the vector's
`partial.expected` outcome (the honest-partial rule) or skips the
vector.

## Running the tests

Run `cargo test` at the **workspace root**: kaiv-cli unifies the
converter features there, so the full matrix — unit tests, the
conformance tree, and the converter stress suite — runs. A bare
`cargo test -p kaiv` fails **by design**: the stress suite is
gated on the converter features and would otherwise compile to
zero tests, silently indistinguishable from success, so a
sentinel test trips with a directive instead (per-package CI
should run the workspace root, or `cargo test -p kaiv --features
json,yaml,toml,xml,cbor,avro,proto,asn1,graphql,xsd`). Library-
only iteration: `cargo test -p kaiv --lib`.

## Stability

The crate is pre-1.0 and follows Cargo's pre-1.0 semver reading: a
breaking change bumps the minor version. `kaiv` and `kaiv-cli`
version together; `kaiv-lsp` tracks its own line.

The public API is everything reachable from the crate root on
[docs.rs](https://docs.rs/kaiv). Anything not reachable from there
is an implementation detail and may change in a patch release.

The error enums (`LexError`, `AppError`, `PipelineError`) are
`#[non_exhaustive]`: match them with a wildcard arm. The catalog is
complete against the current spec, but Level 4 is unimplemented and
will add to it.

**MSRV: Rust 1.85** for the library, the CLI and the LSP; raising it
is a minor-version change, and CI builds against it. The optional
`collation-colligo` backend needs 1.88 (its dependency uses let
chains), so `kaiv-wasm`, the only crate that selects it, declares
that instead.

Feature notes: `default = ["collation-icu"]`. `net` is opt-in — the
library does no network I/O unless you ask for it. The two collation
backends are additive; with both enabled ICU wins, so unrelated
crates in one dependency graph can each pick one.

## Scope and known limits (seed)

Levels 0–3. Type, schema, and unit resolution covers all four
layers: `.!registry` (Layer 1) and `kaiv.kaiv` (Layer 2) over
filesystem or `http(s)` bases, redirect aliasing (Layer 3), and the
canonical default hosts `t.kaiv.io`/`s.kaiv.io`/`f.kaiv.io`
(Layer 4; the `k*aiv.com` production domains take over when those
zones go live). Network resolution is the `net` feature — opt-in,
so a default build makes no outbound request and an `http(s)` base
is a `SchemaResolutionError`; the CLI enables it. Fetched artifacts are
immutable eternalinks, cached without revalidation under
`~/.cache/kaiv` (`KAIV_CACHE_DIR` / `/cache::dir` in `kaiv.kaiv`
override); `KAIV_OFFLINE=1` or `kaiv --offline` resolves from the
cache only. Level 3 locale collation (`..lex[locale]`) has
two backends: `collation-icu` — on by default,
full-fidelity ICU4X, every locale and `-u-` override the spec
recognizes (the CLDR data grows the binary by roughly 1.3 MB) —
and the opt-in `collation-colligo`, a lightweight context-free
backend ([colligo](https://crates.io/crates/colligo), ~100 KB)
that serves its exactly-reproducible locales and honestly rejects
the rest (`fr-CA`, `ja`, `-u-` extensions →
`CollationUnsupportedError`). Enabling both resolves to ICU. With
neither
(`default-features = false`) you get a lean Level 0–2 runtime
where `..lex[locale]` is a `CollationUnsupportedError` and only
the built-in byte order remains — never a silent byte-order
fallback. The embedded data is CLDR 48
(`kaiv::collate::CLDR_VERSION`) under either backend, matching
the spec's pinned reference version. Unknown Level 3 input is
strict under either backend: an ill-formed `..lex[tag]` locale is
rejected statically at schema compile, a well-formed tag whose
language CLDR does not know — or carrying an unrecognized `-u-`
key — is `CollationUnsupportedError`, and root collation is
selected only by an explicit `..lex[und]`, never by silent
fallback. Span semantics are full-fidelity: `..ver` compares
segment-numerically (1.10.0 > 1.9.0), `..time` compares RFC 3339
*instants* (offset-aware, constant-memory parse). Map key-pattern
clauses and quoted names as interior path segments are
implemented. Not yet implemented, by design at this stage:
Level 4 entirely. Each unimplemented path fails loudly rather
than guessing.

## License

Licensed under either of [Apache License, Version
2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
