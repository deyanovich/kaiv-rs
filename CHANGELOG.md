CHANGELOG
=========

All notable changes to kaiv-rs will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
but note that pre-1.0 releases may not adhere strictly to all
guidelines.

[0.11.0] - 2026-08-01
---------------------

### Changed

- **The fmt/unbuild split: the formatter formats, `unbuild`
  inverts `build`.** The library renames `fmt::lift` to
  `fmt::unbuild` (breaking) — the old name collided with the
  spec's *head-type lift*, an unrelated Denormalizer concept.
  `kaiv fmt` is now a formatter with the full social contract:
  authoring surfaces only, **in place by default** on a file
  argument (stdin still prints; `-w` remains as a compatible
  spelling of the default; `--check` unchanged); pointed at a
  canonical `.daiv`/`.raiv` it refuses — under
  byte-deterministic emission a canonical stream has exactly
  one spelling — and directs to the new verb. `kaiv unbuild
  [file]` renders canonical `.daiv`/`.raiv` as idiomatic
  authored `.kaiv` on stdout (a view: sugar the compiler
  resolved away — comments, variables, references — does not
  come back; `.raiv` preserves more, authored units included).
  kaiv-wasm gains the matching `unbuild` export.

[0.10.0] - 2026-07-31
---------------------

The design-review release: implements the eight rulings
(D-10 through D-15 plus the two conformance-surface strays)
logged against the format spec's 1.0-draft.13 review.

### Added

- **Schema delegation — the discriminated schema set (D-10).**
  A `.saiv` can declare an ostensive parent block over a set of
  member schemas (`(/ns schema:A|B)`); the compiled `.csaiv`
  carries a `[schema::A|B]` delegation line, and the document
  picks its member with a scoped `.!schema:/ns ID` discriminant
  declaration (mandatory: a delegated namespace without one is
  a `DelegationSchemaError`, as is a discriminant outside the
  declared set). The composite validation contract merges the
  parent and the selected member; the member's own header
  governs inside its prefix — `strict` and the
  `.!provenance` level apply per delegated namespace, not
  from the parent.
- **Per-alternative units in unions (D-11).** Each union
  alternative carries its own unit annotation
  (`!null|float:km`); the discriminant match honors the active
  alternative's unit on schema and data lines alike (the old
  blanket rejection of units on unions is gone).
- **Authored-unit conversion, exact decimal only (D-14).** A
  data line authored in a different unit of the same dimension
  as the field's declared unit converts at build time in the
  Denormalizer: value × the exact ratio of the two factors,
  over i128 decimals — no floating point anywhere in the
  pipeline. An inexact (non-terminating) or type-violating
  result is a build-time `UnitConversionError`. Factors resolve
  from the built-in tables — SI and the information units
  (`b`/`B` with decimal and IEC prefixes; `4 KiB` under a
  declared `:B` head becomes exactly `4096`) — and from
  imported `.faiv` definitions, aliases and compound custom
  dimensions included. Currencies never convert. `.raiv` keeps
  the authored unit; only the deployment artifact converts.
- **The elided-type unit annotation `!:unit` (D-15).** An
  authored annotation may elide the type and carry only the
  unit: under a schema-defined field it inherits the declared
  head type (then flows through D-14 conversion); on a
  schemaless or undefined field it resolves to `float`.
- **`.faiv` alias canonicalization.** Unit aliases
  (`&AU=au`) are authoring-side spellings only: the compiler
  rewrites them to their primary names at annotation
  acceptance, so canonical lines never carry an alias.

### Changed

- **`!str` retention by intent; the `/^/` lex-saver (D-12).**
  The schema compiler retains an authored `!str` head as a
  nominal type commitment; a headless field stays elided. The
  vacuous pattern `/^/` is the lex-saving spelling for
  constraining a headless field without a retained head.
- **`std/core` `b64` tightened to the quad form (D-13).** The
  pattern now rejects the impossible length class
  (`4n+1` residues): `^([A-Za-z0-9_-]{4})*([A-Za-z0-9_-]{2,3})?$`.
- **`FORMAT_KIND` gates the REQUIRED-header authored kinds.**
  A missing or mismatched format declaration on the
  identity-carrying kinds is the spec-named `FORMAT_KIND`
  lex error, not an ad-hoc message.
- **Conformance runner: build-error vectors.** A `valid/`
  vector may carry `expected.error` instead of expected
  outputs — the pipeline must fail with the named error
  (`UnitConversionError`, `DelegationSchemaError`, …); the
  fmt/lift sweeps skip them.

[0.9.0] - 2026-07-29
--------------------

### Changed

- **The declaration version is optional for every kind — the
  bare form is canonical family-wide.** The identity-carrying
  kinds (`.!saiv`/`.!csaiv`/`.!taiv`/`.!faiv`/`.!msaiv`) no
  longer require the version token: the declaration is split by
  token shape — a digit-first first token is the version slot
  (validated as before, `UNSUPPORTED_VERSION` on a non-1
  major), any other first token is the identity, with version 1
  implicit (an identity is alpha-first by grammar, so the two
  can never collide). `.!saiv acme/x strict` is now the
  canonical spelling; `.!saiv 1 acme/x strict` remains
  equivalent authored input. The schema compiler normalizes a
  version-1 `.!saiv` header to a bare `.!csaiv` header
  (an explicit non-1 version is preserved), the schema
  importers/inference (`import-json-schema`, `import-xsd`,
  `import-proto`, `import-avro`, `import-graphql`, `infer`)
  emit the bare form, `kaiv fmt` strips a version-1 token from
  every declaration kind (previously `.!kaiv` only), and the
  embedded `std/*` type libraries open bare. Tracks the spec's
  § Format Declaration change (1.0.0-alpha.10); conformance
  vectors swept, with new `schema/043-versioned-saiv-header`
  and `compile-error/017-unsupported-version` vectors.

[0.8.0] - 2026-07-26
--------------------

### Added

- **serde support (`--features serde`): kaiv as a serde data
  format.** Any `#[derive(Serialize)]` type serializes straight
  to canonical `.daiv` lines (`serde::serialize_into` — struct
  fields to `::field` scalars and `/field` namespaces,
  `Vec<scalar>` to vector lines, `Vec<struct>` to element runs,
  `None` to `!null`, multi-line strings to `!text` with the
  `std/enc` embed fallback), and any `#[derive(Deserialize)]`
  type reads back out of a parsed `Doc` (`serde::from_doc` /
  `from_view`) — deserialization driven by the TARGET type, so
  untyped authored scalars decode into typed fields, while
  self-describing targets read the line annotations. A `Heads`
  table supplies per-field wire types where the name is the
  semantics (union discriminants, `!text` under text unions) —
  serde sees only the Rust type. Empty collections have no wire
  representation (kaiv collections are never required): give
  collection fields `#[serde(default)]`. This supersedes the
  planned derive macro — serde's derive IS the derive, and a
  server can go typed-struct → canonical lines → wire with no
  intermediate value tree.

[0.7.0] - 2026-07-26
--------------------

### Added

- **`kaiv::doc` — read-side access to canonical `.daiv`**
  (`Doc`, `View`, `Typed`, `FromDaiv`): the mirror of
  `builder`. Parse a `.daiv` stream once into an addressable
  document, walk it with namepath-scoped views (`value` /
  `typed` / `is_null` / `required`; `scalars` for `{arr}::N`
  element lines, `elements` for `{arr}/N` element groups), and
  decode consumer types via the `FromDaiv` trait — a server
  answering typed requests reads
  `MyParams::from_daiv(&doc.root().view("/params"))` while
  `DaivBuilder` writes the reply, with no JSON detour on
  either side.

- **Quoted names as interior path segments** (spec § Quoted
  Names, the D-4 ruling). Quoted names are legal as any namepath
  segment — between `/` steps, after `@`, after `::` — through
  the whole pipeline: the canonical § 2.3.4 examples
  (`!str'/app/"dark-mode"::enabled=true`,
  `!int'/@"x-servers"/0::"retry-count"=3`) round-trip
  byte-exactly. The path splits are quote-aware end to end (a
  quoted name may contain `/`, `::`, or `@` literally), the
  MUST-iff rule holds in every segment position (authored
  over-quoting of a bare-able name normalizes away; non-bare
  names must be quoted — `INVALID_KEY_ERROR` otherwise), and a
  quoted all-digit segment stays an ordinary name, never an
  array index. `fk-path` remains bare-segment-only, a documented
  subset boundary.
- **`..time` compares RFC 3339 instants** (the D-4 ruling).
  Range evaluation under the `..time` span is offset-aware
  temporal comparison — `2025-12-31T23:00:00+01:00` equals
  `2026-01-01T00:00:00+02:00` — via a constant-memory RFC 3339
  parse covering the four `std/time` shapes; fractional seconds
  compare numerically. Off-shape operands keep byte order. Enum
  membership stays byte equality (§ 2.6.11 scopes span orderings
  to ranges; the § 5.3 equality extension is `..lex[locale]`
  only).
- **Map key constraints — the ostensive block surface** (the D-2
  ruling). A `.saiv` declares a keyed map as a `(/name)` block
  mirroring the data form: optional `min=`/`max=` entry-count
  bounds on the header, a type-annotation line for entry values,
  and a `/regex/` key line (the key grammar). Lowers to the
  § 7.5 compiled form (`[key::…]` clause + bounds on the
  collection line, then the entry line); the Validator enforces
  the key grammar per entry (`ConstraintViolationError`, matched
  on the name's content) and the entry count
  (`CardinalityViolationError`). Literal key lines inside map
  blocks are out of scope and reject statically
  (`INVALID_CONSTRAINT_ERROR`). The flat `!map<T>` form remains
  the unkeyed map.
- **Level 3 strict unknown-input semantics** (the D-8 ruling):
  an ill-formed `..lex[tag]` locale is now
  `INVALID_CONSTRAINT_ERROR` at schema-compile time (new
  internal BCP 47 well-formedness check, backend-independent —
  previously deferred to validation); a well-formed tag whose
  primary language CLDR does not know refuses to resolve under
  the ICU backend instead of silently falling back to root
  (likely-subtags detection); an unrecognized `-u-` key (or any
  non-`u` extension) is `CollationUnsupportedError`; explicit
  `..lex[und]` means root collation on both backends (colligo
  previously refused it).
- **Conformance runner: Level gating and resolution modes.** The
  runner honors the tree's `LEVEL` marker (Level 3 vectors run
  as written under ICU; an honest-partial build substitutes
  `partial.expected` or skips) and the `MODE` marker
  (`registry-strict` / `registry-canonical`).
- **Resolution provenance events and strict/canonical registry
  modes.** The Resolver records a `ResolutionEvent` per resolved
  artifact (drained via `take_resolutions`) and honors two
  off-by-default `Config` toggles: `strict_registry`
  (`--registry-strict` / `KAIV_REGISTRY_STRICT` — refuses a
  Layer 1 `.!registry` win with `RegistryStrictError` before any
  fetch) and `canonical_registry` (`--registry-canonical` /
  `KAIV_REGISTRY_CANONICAL` — Layers 1–2 ignored, everything
  resolves through the Layer 4 default hosts). The CLI grows the
  two flags plus `-v`/`--verbose` resolution reporting.

- **Builder: schema declarations and the served form.**
  `DaivBuilder::declare_schema` emits `.!schema:<ref>` header
  lines in declaration order — composition order is composed
  field order, so a server emitting an envelope sandwich
  declares head, data, tail in sequence — and `finish_daiv`
  renders the document under the canonical `.!daiv` declaration
  (every builder line is already canonical, so the artifact is
  directly `Doc::parse`-able and `validate`-able). `KaivBuilder`
  mirrors `declare_schema`.

- **Multi-line values through the builder and the doc reader.**
  `DaivBuilder::leaf_text` emits multi-line text as the core
  `!text` type (newlines become the `|:|` separator on a flat
  canonical line; a literal `|:|` in the content is refused
  toward the embed route, CRLF toward normalization) and
  `DaivBuilder::leaf_embed` emits base64url `std/enc/<kind>`
  payloads for everything the flat line cannot carry —
  `KaivBuilder` mirrors both. On the read side,
  `doc::View::text` resolves `|:|` back to newlines (accepting
  plain `str` as the coercion's read half) and `View::embed`
  decodes `std/enc` payloads to `(kind, bytes)`. The base64url
  helpers move to a feature-free core module (`kaiv::b64`);
  `json::b64url_{encode,decode}` remain as re-exports.

- **Exporters resolve custom heads** (spec § Exporters Resolve
  Custom Heads). New `export_with(canonical, &Resolver)` on every
  exporter (json / yaml / toml / xml / cbor / avro / asn1 /
  proto — all through the shared tree), interpreting custom type
  heads by, in order: the `.taiv` chain via the resolver (a
  `..num` lowering exports as a number, a `{true,false}` enum as
  a boolean), then the document's declared `.!schema` — the
  vendored `.csaiv` a validating device already holds carries
  every field's lowered base kind, so offline export needs no new
  artifact. Best-effort and total: an uninterpretable head, or a
  value that does not fit the resolved kind, exports as its
  string — resolution refines, it cannot reject. The CLI's
  `export` resolves by default; `--no-resolve` keeps the
  uninterpreted forms. Plain `export()` is unchanged.

### Fixed

- **Scalar-array cardinality counted zero elements.** A
  `[min=N]`/`[max=M]` header on a scalar array (elements are
  `{arr}::N` value lines, not field groups) was checked
  against the namespace-array element counter only, so every
  non-empty batch failed `min` and no batch could fail `max`.
  The Pass 1 counter now also counts scalar element lines
  (conformance `schema/042-scalar-array-cardinality`).

### Changed

- **Head-type retention** (spec § The Schema Compiler). The
  compiled schema now retains each field's authored head type
  name as its leading item (`!int`, `!std/time/datetime`,
  `!acme/net/port` — the most derived authored name; the
  identity `str` stays elided) instead of lowering every
  non-str/text/unit/union name away. On top of it, the
  schema-aware Denormalizer generalizes the old str→text retype
  to a full **type lift**: untyped (`!str`) data lines on typed
  fields are emitted into `.daiv` under the head type, and
  materialized defaults are typed with it — so `.daiv` states
  every value's type on the line and exporters regain native
  int/bool/float/datetime fidelity with no schema lookup
  downstream of the build (the reviewer-reported
  everything-exports-as-strings gap). Validation is now nominal
  only where the name IS the semantics — unions, `null`, `text`
  (guarded coercion), `std/enc` embeds, the explicit `!str`
  identity head — and structural elsewhere (`!int` under
  `!std/net/port` validates; so does the pre-lift `!str` form).
  `.csaiv` artifacts regenerate; the item grammar is unchanged
  (the head is one more token from the existing vocabulary).

- **`kaiv fmt` always emits a format declaration**: a bare
  `.!kaiv` is inserted (after any shebang) when the input
  carries none — the canonical authored form declares itself.

[0.6.0] - 2026-07-18
--------------------

### Added

- **`kaiv fmt` — the standard formatter for authoring files.**
  Normalizes an authored `.kaiv` into the standard style with an
  optimizing pretty-printer: for every group of fields it chooses
  among the three equivalent syntaxes — a flat namepath line for a
  single field, the inline assignment (`/path:=a=1|b=2`,
  `/@arr+:=...`) for small all-string groups within 72 columns,
  and the `(...)`/`[...]` block form for anything larger, typed,
  or commented. Array runs render uniformly (one element needing
  the block form puts them all in blocks). Authored blank lines
  act as grouping hints; comments, variables, splats, and every
  value byte are preserved, fields are never reordered, and
  `compile(fmt(x)) == compile(x)` holds across the conformance
  tree (enforced by a new conformance-level property test,
  alongside fmt idempotency). A canonical `.daiv`/`.raiv` input is
  instead *lifted* into idiomatic authored kaiv — the human view
  of a machine artifact (round-trip enforced: rebuilding the
  lifted form reproduces the identical `.daiv`). Schema-side
  authoring kinds (`.saiv`/`.taiv`/`.faiv`/`.maiv`) get light
  whitespace/blank-line normalization only. CLI:
  `kaiv fmt [file] [-w] [--check]` (stdin default; `-w` in-place;
  `--check` for CI). Library: `format_data`, `format_plain`,
  `lift` in a new `fmt` module.
- **`KaivBuilder`** — the authored-form sibling of `DaivBuilder`:
  the identical value-level API (declarations, typed leaves, units,
  provenance), with `finish()` rendering through the formatter's
  lift, so generated documents destined for human eyes come out in
  idiomatic authored kaiv (grouped blocks, inline assignments,
  implicit `str`, `$` re-escaped).
- **Mappings (`.maiv`) implemented** — SPEC.md § Mappings, end to
  end: parser (bare `.!maiv` header; a mapping's identity is its
  `.!source`/`.!target` endpoint pair), publish-time validation
  (namepath existence, override admissibility, and the static
  required-field completeness check — new `IncompleteMappingError`),
  the streaming mapper (source `.daiv` → target `.daiv`, assembled
  in target schema order, target defaults materialized, `.!schema`
  declaration emitted), `/*` array wildcards, `|constant` and
  `|!null` overrides, and composition as a namepath join with the
  `.!via` trail recorded by edge identity. CLI: `kaiv mapping
  validate | apply | compose`. Registry addressing is derived
  and self-evident —
  `s.kaiv.io/{source}/mapto/{target}/{version}.maiv` published
  by the source owner, `{target}/mapfrom/{source}/{version}.maiv`
  by the target owner — the direction marker makes the address
  read as a sentence and records who vouches for the edge;
  same-namespace short forms, mandatory edition segment,
  `mapto`/`mapfrom` reserved on the registries.
- **Information units built in** — the bit `b` and byte `B`
  (= 8 b) join the curated unit set as a base dimension of
  their own, with the SI decimal multiples (`kB`…`PB`,
  `kb`…`Pb`, ×1000) and the IEC binary multiples
  (`KiB`…`PiB`, `Kib`…`Pib`, ×1024) — IEC prefixes restricted
  to information units, multiplying prefixes only. The telecom
  rate spellings `bps kbps Mbps Gbps Tbps` are accepted as
  input and canonicalize to the decimal-bit compounds
  (`Mb/s`, never mebibits). **`KB`/`Kb` are rejected as
  ambiguous at the unit grammar level** (JEDEC 1024 vs SI-read
  1000), with a teaching error naming both resolutions — no
  context, including a custom `.faiv`, can claim the spelling.
- **The `!text` type** — a seventh `std/core` canonical
  shorthand: multi-line text in readable form, the value's
  lines joined by the fixed `|:|` separator. Interpretation
  happens only at the application/export layer; within kaiv the
  value is verbatim. Importers now emit `!text` for LF-only
  multi-line strings (readable, greppable) instead of the
  base64 `std/enc` embedding — content carrying a CR or a
  literal `|:|` still falls back to `std/enc`. In schemas,
  `!text` is retained as a type item in the compiled `.csaiv`,
  with the str→text coercion: a plain `!str` line satisfies a
  `!text` field when its value carries no literal `|:|` (a
  `|:|`-carrying value is `DelimiterCollisionError` — the
  retype would reinterpret it), and the schema-aware
  Denormalizer retypes coerced lines to `!text` in the `.daiv`.
  The reverse direction stays a `TypeMismatchError`.
- **Empty-stdin guard** — the stdin-reading commands reject
  empty standard input with a pipeline-failure hint (an
  upstream `kaiv import` failure would otherwise let `build`
  exit 0 on nothing); an intentionally empty document still
  builds when passed as a file.
- **`kaiv validate` reads stdin** — omit the data argument, or
  pass `-` when also naming a schema file: `kaiv build x.kaiv |
  kaiv validate`. Piped `.!raiv` input is denormalized
  (schema-aware) before validation.

### Changed

- **Per-kind format declarations.** The declaration keyword now
  mirrors the file extension — `.!kaiv`/`.!raiv`/`.!daiv` for
  data and `.!maiv` for mappings (version optional; the bare
  form is canonical and means version 1),
  `.!saiv`/`.!csaiv`/`.!taiv`/`.!faiv` (version required) for
  the identity-carrying kinds. The old
  `.!kaivschema`/`.!kaivtype`/`.!kaivunit`/`.!kaivmap`/
  `.!kaivmetaschema` names are gone. The declaration is
  optional in authored `.kaiv` (absence means version 1 — any
  well-formed `.env` file is valid kaiv); the pipeline rewrites
  the keyword at each stage and canonical consumers require the
  matching kind (new `FORMAT_KIND_ERROR`).
- **`std/enc` `txt` renamed to `plain`** (`&plain`,
  `!std/enc/plain`) to remove the near-collision with `!text`.
- The `canonical_input` `.raiv` path (CLI) and the gate's
  `.kaiv`/`.raiv` deposit paths denormalize schema-aware, so
  absent optional fields are materialized before validation.

- **`kaiv login` / `kaiv whoami` / `kaiv logout`** — identity
  against the kaiv registries (idaiv), on the literium model:
  the passwordless email-link device authorization grant (an
  emailed one-time link approves the device; the first sign-in
  creates the account), a rotating refresh token stored at
  `~/.config/kaiv/credentials` (0600) with access tokens minted
  on demand and rotation persisted before use, and best-effort
  server-side revocation on logout. `KAIV_ID_URL` overrides the
  identity host (default `https://id.kaiv.io` during the
  alpha).


[0.5.0] - 2026-07-17
--------------------

### Added

- **A second collation backend: `collation-colligo`.** Level 3
  locale collation now selects between two mutually exclusive
  backends. The default is unchanged in behavior from 0.4.0:
  `collation-icu` (full-fidelity ICU4X — every locale and `-u-`
  strength/case/variable override the spec recognizes; the
  `collation` feature name remains as an alias). The new opt-in
  `collation-colligo` builds on the colligo collator instead — a
  fraction of the dependency weight (~100 KB vs ~1.3 MB of data),
  wasm-friendly, derived from the same pinned reference line
  (CLDR 48.2 / UCA 17.0.0) and verified against the ICU oracle.
  Under colligo only exact-fidelity locales resolve: approximate
  or deliberately divergent tailorings (`fr-CA`, `ru`),
  unsupported scripts (`ja`), and any tag carrying a BCP 47
  extension report `CollationUnsupportedError` instead of
  collating — rejecting is always conforming at Level 3, silently
  disagreeing is not. Comparisons run at the pinned tertiary
  defaults with UCA lowercase-first case order.
  `collate::CLDR_VERSION` is "48" under either backend. Enabling
  both backends is a compile error; enabling neither leaves the
  built-in byte order only, with `..lex[locale]` rejecting as
  before.
- kaiv-wasm (unpublished) enables `collation-colligo`: Level 3 in
  the browser at 1.9 MB total, where the ICU4X data would have
  tripled the module.


[0.4.0] - 2026-07-17
--------------------

### Added

- **The `std/net` library** — network identifiers, embedded like
  the other std libraries and pinned to their defining documents:
  `uri` (RFC 3986 generic syntax, exactly — full IPv6/IPvFuture
  IP-literals, percent-encoding, every path form), `url` (the
  same grammar with a required authority: the everyday
  `scheme://` subset; `mailto:`/`urn:` forms are uri-only),
  `email` (the WHATWG HTML valid-e-mail-address pattern — the
  practical interchange subset, not full RFC 5322), `hostname`
  (RFC 1123, the 253-octet total carried as a length constraint),
  and `port` (`int[0,65535]`).
- **The `std/math` library** — `complex`: one-spelling `a±bi`
  with both components always present, the separator sign
  carrying the imaginary sign, the `i` suffix mandatory, and
  float-grammar components (Gaussian integers expressible).
  Deliberately no numeric span: the complex numbers admit no
  total order compatible with their arithmetic, so ordering
  stays deterministic byte order and ranges are inert.
- **The `\xHH` regex escape** (spec addition): exactly two hex
  digits naming an ASCII character (00-7F), valid as an atom and
  as a character-class member or range endpoint. The one letter
  escape beyond `\d` — unambiguous in every source dialect — and
  the only way a pattern matches the `'` delimiter its body
  cannot contain literally (RFC 3986 sub-delims, the e-mail
  local-part alphabet).
- `kaiv version` (also `--version`/`-V`).

### Changed

- **Validation errors carry the failure site.** `validate`
  returns `AppErrorAt` — the pinned spec error name plus the
  1-based `.daiv` line and a context string naming the field,
  value, and schema constraint involved
  (`ConstraintViolationError: /server::port=99999 (type !int)
  violates !int[1,65535] (line 2)`). Duplicates name the field;
  missing fields name the declared field and where the scan
  stopped; uniqueness names the colliding tuple and key;
  referential integrity names the unmatched value and target;
  cardinality names counts and bounds. The bare names are
  unchanged — conformance still compares them.
- **XSD import roots fields under the element's namespace**,
  matching the XML data converter, so data imported from XML
  validates against the schema imported from its own XSD without
  reshaping. (Converted-schema namepaths gain the root segment —
  regenerate any stored conversions.)
- Schema converters preserve more instead of dropping: JSON
  Schema `format: uri`/`email`/`hostname` and XSD `xs:anyURI`
  land as the `std/net` named types (joining the existing
  date-time family); a bare `enum`/`const` with no `type`
  keyword infers its scalar kind from homogeneous members; a
  restriction on a bounded XSD integer base keeps the base's
  implicit range (`xs:positiveInteger` + `maxInclusive 9999` is
  `[1,9999]`, not `[,9999]`).

### Fixed

- **The schema compiler enforces the pattern dialect.** An
  out-of-dialect pattern (`\w+`, `[\x80]`) was compiled into the
  `.csaiv` unchecked and surfaced at validate time as a data-side
  `ConstraintViolationError`; every compiled field's patterns —
  union alternatives included — now pass through the regex engine
  at schema compile, and a bad pattern is the schema author's
  `INVALID_CONSTRAINT_ERROR`, as the spec always said.
- `kaiv` with no arguments printed a panic instead of the usage
  text (empty-args slice).


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
