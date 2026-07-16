CHANGELOG
=========

All notable changes to kaiv-rs will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
but note that pre-1.0 releases may not adhere strictly to all
guidelines.

[0.3.0] - 2026-07-16
--------------------

### Added

- **Level 3: collation.** `..lex[locale]` span orderings are now
  evaluated, not just rejected: range constraints compare and enum
  membership tests for equality under the tag's locale collation
  (UCA/CLDR via ICU4X), at the spec's pinned defaults — tertiary
  strength, non-ignorable variables — with the recognized BCP 47
  `-u-` overrides (`ks`, `ka`, `kc`, `co`) honored. A tag that is
  malformed or carries an unrecognized override raises
  `CollationUnsupportedError` up front. Gated behind the new
  default-on `collation` feature (the CLDR data adds ~1.3 MB to
  the binary); without it the runtime stays L0-2 and keeps
  rejecting. The embedded data is the ICU4X 2.1 line's CLDR 48 —
  the spec's pinned reference version (advanced from CLDR 46 in
  the same revision) — reported as `collate::CLDR_VERSION` per
  the spec's conformance-metadata rule.

- **Anonymous refinement: bare constraint lines in `.saiv`.** A
  metadata line of value-constraint items above a field definition
  (`/^[a-z]+$/ #[1,8]` then `name=`) now refines the field's
  implicit `str` type — the `.taiv` definition shape applied to a
  field, lowered to a bare constraint group exactly as `!str` plus
  the same items would be (spec § Anonymous Refinement, added in
  the same revision). Previously the schema compiler silently
  dropped such lines, compiling a weaker contract than authored.
  Rule-6 metadata lines the schema compiler cannot interpret — a
  type-reference item on a bare line, a `?` provenance list, a
  stray no-`=` line — now reject loudly instead of dropping.
  Conformance vector `schema/021-bare-constraint-line` pins the
  new semantics.

- The spec's new named Compiler/Schema-compiler errors:
  `UndefinedReferenceError`, `VariableContextError`,
  `DelimiterCollisionError`, `SchemaInheritanceCycleError`
  (real chain-based cycle detection), and
  `SchemaOptionalWithoutDefaultError` — replacing ad-hoc
  `Other`-string errors, each pinned by a conformance vector.

- Newly specified authored forms: namespace-variable splat
  (`/path:=$/.name` and standalone `$/.name` lines inside
  blocks), map assignment (`/ns/path={}` and inline entries —
  also fixes a doubled-`/` bug in map entry emission), and the
  flat space-separated `.!schema hub/x` declaration form.
  `@.a+=$@.b` now splices the referenced array instead of
  joining it into one value.

- `kaiv validate <data>` with the schema argument omitted now
  resolves the document's `.!schema` declarations through the
  registries (via `schema_for_daiv`, allOf-composed), so a
  canonical document validates against its declared schemas
  with zero configuration.

- Registry-gate entry points, for hosts embedding the pipeline
  without filesystem or network access (the pyloros `hook-kaiv`
  Worker): `Resolver::preload`/`take_missing` (an in-memory
  artifact seam consulted before every base layer, with failed
  lookups recorded for a fetch-and-retry loop),
  `Resolver::csaiv_bytes`, `schema::check_type_lib` (a `.taiv`
  publish check — every definition lowers, so unresolvable or
  cyclic references are caught at publish time), and
  `validator::schema_for_daiv` (resolves a canonical document's
  `.!schema` declarations into one merged `CompiledSchema`,
  allOf-composed, honoring namespace/array qualifiers). Existing
  resolution behavior is unchanged.

- `builder::DaivBuilder` — programmatic construction of canonical
  `.daiv` documents from typed values, with per-leaf provenance
  (`?source@timestamp#dpid`) and component validation, so built
  output always lexes. The first value-level (rather than
  text-level) API; added for the quarb engine's `.daiv` emission.

- Network registry resolution (Layers 3–4) behind the new `net`
  feature, **enabled by default** — disable via `default-features =
  false` for embedded/offline builds. `http(s)` registry bases fetch
  `{base}/{lib}.{ext}` with redirect aliasing (Layer 3) and fall
  back to the default hosts `ktaiv.com`/`ksaiv.com`/`kfaiv.com`
  (Layer 4). Artifacts are immutable eternalinks, cached without
  revalidation under `$XDG_CACHE_HOME/kaiv` (`KAIV_CACHE_DIR` env or
  `/cache::dir` in `kaiv.kaiv` override). `KAIV_OFFLINE=1` / the CLI
  `--offline` flag resolve from the cache only. Adds the crate's
  first default dependency: `ureq` (rustls, minimal features).
- XML import/export behind the new `xml` feature (zero new
  dependencies — the well-formed-subset parser is hand-rolled, no
  DTD support), wired into the CLI as `--xml` / the `.xml`
  extension. Mapping: the root element is the single top-level
  namespace, attributes are `@name` members, text beside attributes
  is `#text`, repeated same-name siblings group into arrays, an
  empty element is `!null`, and element text stays untyped (no
  sniffing). Namespaces stay verbatim (`soap:Body` is a literal
  key). Mixed content embeds the element verbatim as `std/enc/xml`
  and splices back on export; any kaiv document exports (multiple
  top-level members wrap in `<root>`).
- CBOR import/export behind the new `cbor` feature (RFC 8949,
  zero new dependencies — hand-rolled decoder/encoder), wired into
  the CLI as `--cbor` / the `.cbor` extension; the family's first
  binary format (`export --cbor` writes binary to stdout). Byte
  strings ride the typed channel as `std/enc/bin` (base64url) and
  decode back to byte strings; tag 0 datetime strings ride as
  `std/time/datetime` and re-emit tagged; non-finite floats are the
  `std/num` markers. Integers are exact at any width — beyond
  ±2^64 they travel as decimal tokens and re-encode as bignums
  (tags 2/3). Output is RFC 8949 preferred serialization (definite
  lengths, minimal-width heads, shortest exact float);
  indefinite-length input is accepted. Edges: `undefined`
  converges on null, tags other than 0/2/3 drop, non-text scalar
  map keys stringify, duplicate map keys are rejected.
- Avro import/export behind the new `avro` feature (Object
  Container Files, zero new dependencies — the embedded JSON
  schema is read with the crate's own parser), wired into the CLI
  as `--avro` / the `.avro` extension. Import decodes against the
  embedded schema: bytes/fixed ride as `std/enc/bin`, enum symbols
  as strings, non-finite floats as `std/num` markers, the decimal
  logical type decodes to an exact token at its scale via the
  bignum arithmetic, and the time logical types (`date`, `time-*`,
  `timestamp-*`, `local-timestamp-*`) map onto `std/time` tokens
  (micros resolution on export; offsets normalize to UTC). Both
  the null and deflate codecs are read — the RFC 1951 inflate
  decoder is hand-rolled too. A single-record file becomes the
  document root; anything else lands under a top-level `records`
  array.
  Export *infers* an Avro schema from the tree — records unify
  field-wise (missing fields become unions with null), int-shaped
  numbers are `long` or `decimal` (scale 0) beyond i64, mixed
  scalar kinds become unions — and writes a deterministic
  single-record null-codec OCF. Edges: snappy is rejected and
  export always writes the null codec; logical types outside
  decimal and the time family decode as their underlying
  primitive; mixed time kinds cannot unify; enums degrade to
  strings on re-export; recursive schemas are rejected; field
  names must be valid Avro names on export. Interop verified
  against fastavro in both directions, including deflate input
  and the logical types.
- Protocol Buffers import/export behind the new `proto` feature
  (hand-rolled, zero new dependencies). Unlike every other
  converter the wire format is not self-describing, so both
  directions take a `.proto` schema: `--schema <file.proto>` plus
  `--message <Name>` when the schema has several top-level
  messages (extensions `.pb`/`.binpb` infer the format). The
  schema parser covers proto3 (proto2 labels tolerated): nested
  and recursive messages, enums, oneof, maps, packages, lexical
  name resolution; `import` statements are rejected (schemas must
  be self-contained). Import decodes nested messages as
  namespaces, repeated fields as arrays (packed and unpacked),
  maps with stringified keys, enum numbers as symbol names, bytes
  as `std/enc/bin`, non-finite floats as `std/num`; absent fields
  are omitted (proto3 cannot distinguish absence from default)
  and unknown field numbers are skipped, like every protobuf
  decoder. Export encodes in schema order, packing numeric
  repeated fields; present members always serialize, even at
  default values; null members are an error (proto3 has no null).
  Interop verified against protoc: wire round trips are
  byte-identical to protoc's own encoding.
- ASN.1 BER/DER import and DER export behind the new `asn1`
  feature (hand-rolled, zero new dependencies), wired into the CLI
  as `--asn1` / the `.der` `.pem` `.crt` `.cer` extensions.
  Schema-less: BER/DER is structurally self-describing, so
  universal tags drive the mapping — no ASN.1 module needed. PEM
  armor strips automatically on import. ASN.1 structures are
  positional, so every element imports as a single-member wrapper
  namespace naming its type (`seq`, `set`, `int`, `oid`, `bits`,
  `utc`/`gentime` riding `std/time`, `printable`/`ia5`/… for the
  string types, `c0`/`c0p` and friends for tagged elements,
  `u9p`/`u8c` fallbacks for the rest). INTEGERs are exact at any
  width; times convert to RFC 3339-shaped tokens (UTCTime century
  rule applied). DER input round-trips byte-identically — verified
  on an openssl-generated X.509 certificate, which openssl parses
  back after the round trip; BER-only forms (indefinite lengths,
  non-minimal encodings) normalize to DER. Edges: REAL rides the
  bytes fallback, constructed string encodings are rejected, and a
  multi-member namespace has no DER form (names cannot ride the
  wire without a schema).
- Schema converters: `.proto`, Avro Schema (`.avsc`), GraphQL SDL,
  and XSD now convert to authored `.saiv`, joining the JSON Schema
  converter under the same sound-weakening contract — every
  emitted constraint is implied by the source, and what kaiv
  cannot express drops with a `//` comment, never invented. The
  proto and Avro converters reuse the schema parsers already built
  for their data converters; the XSD converter parses the schema
  document with the crate's own XML parser; the GraphQL converter
  adds a hand-rolled SDL parser behind the new `graphql` feature
  (`xsd` feature for XSD). Highlights: exact wire-range bounds on
  sized integers everywhere; open proto3 enums emit
  `!int|str{…}` while closed Avro/GraphQL enums emit `!str{…}`;
  Avro time logical types and XSD date/dateTime/time ride
  `std/time`; XSD restriction facets map like JSON Schema
  constraints (pattern, enumeration, ranges, lengths) and
  attributes emit as `@name` fields matching the XML data
  converter; optionality never leaks through optional XSD
  ancestors. In every case the schema-converter output compiles,
  and where a data converter shares the source schema, the
  documents it decodes validate against the converted schema.
  `kaiv import-schema` grows `--proto`/`--avro`/`--graphql`/`--xsd`
  with extension inference and `--message` root picking. Documented
  limitation (all schema converters, JSON Schema included): the
  contract holds for documents whose strings are flat — non-flat
  strings (EOL/NUL, leading `$`) ride the data converters'
  `std/enc/json` embed channel, which `str`-typed fields do not
  admit; validating such a document against a converted schema
  reports a type mismatch. Data conversion itself is unaffected;
  widen affected fields by hand to `!str|std/enc/json`.

### Changed

- **Materialized `.daiv`.** The Denormalizer is schema-aware:
  `denormalize_with` resolves the document's declared schema and
  materializes absent optional fields into `.daiv` — the
  resolved default when applicable, else `!null` — per element
  inside namespace arrays, in schema order; an absent required
  field is a build-time `RequiredFieldSchemaError`. The
  Validator's parallel scan is a strict lockstep walk that no
  longer branches on the `?=` optional marker (only
  empty-collection element lines skip; counts are enforced by
  Pass-1 cardinality). The schema compiler rejects an optional
  field with no applicable default and no null alternative
  (`SchemaOptionalWithoutDefaultError`). The format converters
  (jsonschema/proto/xsd) emit `!null|T` unions for optional
  constrained fields accordingly.

- Array-extend/append targets require the `@` sigil: a
  sigil-less `labels;=…`/`labels+=…` left side is an
  `INVALID_KEY_ERROR` (grammar `array-path`), instead of
  compiling to canonical lines no schema could address.

- Layer 4 default hosts now point at the live canonical
  registries `t.kaiv.io`/`s.kaiv.io`/`f.kaiv.io` (the registry
  gate deployment). The `k*aiv.com` production domains named by
  the spec take over when those zones go live —
  `resolve::layer4_default` is the single switch point.

### Fixed

- **Non-ASCII values no longer corrupt.** The Compiler's and
  Denormalizer's value resolvers re-encoded every byte ≥ 0x80 as
  a Latin-1 codepoint (one mojibake layer per stage), so any
  non-ASCII value came out of the pipeline double-encoded. Byte
  runs are now copied as string slices. Root cause of the six
  failing stress tests (`yaml_round_trip_is_lossless` and
  friends), which all pass again; the all-ASCII conformance tree
  had masked it, and the new `valid/034-utf8-values` vector pins
  the fix.

- L0-2 hardening pass (closing the conformance-audit gaps). The
  variable/reference subsystem: array variables (`@.name+=` /
  `@.name;=` / `$@.name`) now work instead of leaking a hidden
  `.`-name into `.daiv`; field references honor the active
  namespace-block prefix; the `$$` escape and mid-value
  interpolation are implemented (a scanner in both compiler and
  denormalizer); a namespace-block `schema:` annotation is a loud
  error rather than silently dropped. Spans and units: `..ver`
  ranges compare segment-wise numeric (not lexical); locale
  collation (`..lex[locale]`) raises `CollationUnsupportedError`;
  unit byte-comparison checks both directions and unit membership;
  a unit on a non-numeric type is a compile error; duplicate
  `.saiv` fields raise `SchemaDuplicateKeyError`; non-bare inline
  map keys are quoted; a first-line `#!` is a shebang, not a
  comment.

- Export mis-parsed arrays under keys needing quotes: the canonical
  `@"name"` segment (e.g. from importing `{"m:Item": [...]}`) was
  read as a literal field named `@name` with the indices as object
  keys, so JSON/YAML/TOML export returned
  `{"@m:Item":{"0":...,"1":...}}` instead of the array. The array
  marker is now recognized as an unquoted leading `@` whatever the
  quoting of the name after it.
- TOML export silently rounded integers beyond i64 through f64
  (`18446744073709551616` became `18446744073709552000.0`). It now
  refuses with an error, like the existing null rejection — TOML
  has no wider integer, and a silently corrupted value is worse
  than no conversion.
- Five bugs found by the new property-based stress harness
  (`tests/stress.rs`, seeded and deterministic; iteration count
  scales with `KAIV_STRESS_ITERS` — the suite holds at 100k seeds):
  - `float_token` used `Display` for whole doubles beyond 1e16,
    which prints hundreds of bare digits and silently turned float
    values into integer-shaped tokens across every binary decoder
    (a CBOR/Avro/proto double `1e300` re-exported as a bignum).
    Such values now take the exponent form.
  - `infer` widened `{int,float}` to `float` even inside unions —
    but unions are tagged (the data line's type selects the
    alternative by name), so `[false, -1, 1e-7]` inferred a schema
    that rejected its own example. The widening now applies only
    when the set collapses to a plain `!float`.
  - `infer` emitted element-block fields in first-seen order; the
    validator's per-element scan is ordered, so arrays whose
    elements order fields differently inferred schemas that failed
    on their own example. Block fields now take a deterministic
    topological order over every element's observed order, and
    arrays with cyclically inconsistent orders demote to skipped.
  - The validator treated any `/` in an element field name as
    deeper structure, so a quoted flat field like `"a/b"` inside a
    table block was skipped and its required declaration reported
    missing. Quoted fields are terminal.
  - The schema compiler left bare-shaped quoted names (`"re"`,
    quoted in authored schemas to clear the reserved-word rule)
    quoted in element-block namepaths, so they never matched the
    data side's bare spelling. Block fields now canonicalize like
    every other segment.
  - Also: ASN.1 PEM detection matched `-----BEGIN ` as a substring
    anywhere, so raw DER *containing* that text as a string value
    was mis-read as PEM; detection now requires valid UTF-8 and a
    line-anchored armor.


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
