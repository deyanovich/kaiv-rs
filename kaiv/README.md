# kaiv

Reference implementation of the **kaiv** format — an immutable
structural type system for data at rest. Levels 0–3: lexer,
compiler, denormalizer, schema compiler, validator, plus
converters for a dozen neighbouring formats.

```toml
[dependencies]
kaiv = "0.12"
```

## The pipeline

Authored `.kaiv` compiles to the relational canonical `.raiv`,
which denormalizes to `.daiv` — one fully-qualified line per
scalar, and the form every consumer reads. A `.saiv` schema
compiles to `.csaiv` and validates a `.daiv`:

```text
.kaiv --[compile]--> .raiv --[denormalize]--> .daiv
.saiv --[compile_schema]--> .csaiv
.daiv + .csaiv --[validate]--> pass / fail
```

```rust
let raiv = kaiv::compile(b".!kaiv 1\n!int\nport=8443\n")?;
let daiv = kaiv::denormalize(&raiv)?;
assert_eq!(daiv, ".!daiv\n!int'::port=8443\n");

let csaiv = kaiv::compile_schema(b".!saiv acme/svc\n\n!int\nport=\n")?;
let schema = kaiv::parse_csaiv(&csaiv)?;
kaiv::validate(&daiv, &schema)?;
# Ok::<(), kaiv::PipelineError>(())
```

Every failure is a `PipelineError` whose `name()` is the spec's
error string — the same string the conformance vectors pin:

```rust
let e = kaiv::compile(b".!kaiv 1\nport=8443").unwrap_err();
assert_eq!(e.name(), Some("MISSING_FINAL_EOL_ERROR"));
```

## Reading and writing documents

`Doc::parse` reads a canonical document by relative namepath
(`::field`, `/ns`, `/@arr`); `DaivBuilder` and `KaivBuilder` emit
one without going through text. With the `serde` feature, any
`Serialize` type writes straight to canonical lines and any
`Deserialize` type reads back out of a `Doc`.

## Converters

Each converter module has `import` and `export` and sits behind
the feature of its name: `json`, `yaml`, `toml`, `xml`, `cbor`,
`avro`, `proto`, `asn1`. Schema converters — `jsonschema`,
`proto`, `avro`, `graphql`, `xsd` — translate foreign schemas
into `.saiv` under a sound-weakening contract: every constraint
emitted is implied by the source, and anything kaiv cannot
express is dropped with a comment rather than approximated.

## Features

`default = ["collation-icu"]` — Level 3 locale collation via
ICU4X, CLDR 48. `collation-colligo` is a lighter alternative
(wasm-friendly, exact context-free tiers, honest rejection
elsewhere); enabling both resolves to ICU. With neither you get a
lean Level 0–2 runtime where `..lex[locale]` rejects rather than
silently falling back.

`net` is **opt-in**: without it the crate makes no outbound
request and an `http(s)` registry base is a
`SchemaResolutionError`. The Level 0–2 core has no dependencies
at all.

## Stability

Pre-1.0, following Cargo's pre-1.0 semver reading: a breaking
change bumps the minor version. The public API is everything
reachable from the crate root on [docs.rs](https://docs.rs/kaiv).
The error enums are `#[non_exhaustive]` — match them with a
wildcard arm. **MSRV: Rust 1.85.**

The executable definition of "correct" is the conformance tree in
the spec repository, which the test suite runs.

## License

Licensed under either of [Apache License, Version
2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
