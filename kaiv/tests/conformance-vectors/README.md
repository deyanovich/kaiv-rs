# kaiv conformance vectors (Levels 0–3)

> This repository is the vectors alone, extracted from the kaiv
> spec repository with their history. It is versioned
> independently of the specification document: pin a tag (`v1`,
> …) to state which vector set an implementation conforms to.

Golden-file test vectors for the Level 0–3 surface of
[the kaiv specification](https://kaiv.io/kaiv/spec/latest). Every vector is derived from a worked
example or normative statement in the spec; the table at the
bottom maps each vector to its source section. An input that
cannot be turned into a vector without guessing indicates a
spec gap — report it rather than inventing behavior.

This is the conformance suite that STRATEGY.md §3.3 defines as a
first-class release artifact. It is normative in the same sense
as the spec text: a conforming implementation passes every
vector. Level 2 (Tables) — `unique`/`ref`/`min`/`max` clauses,
independent constraint groups, and the O(N) Pass 2 — is covered
(schema/015, schema/019, schema/020). Level 3 (Collation) is
covered by the `LEVEL`-marked vectors (schema/025–032); see
Pinned assumptions, item 7. Level 4 remains out of scope by
design.

## Layout

```
conformance/
├── valid/NNN-name/       end-to-end golden triples
│   ├── input.kaiv            authored input
│   ├── expected.raiv         Compiler output (see rule below)
│   └── expected.daiv         Denormalizer output
├── schema/NNN-name/      schema compilation + validation
│   ├── schema.saiv           authored schema
│   ├── expected.csaiv        schema-compiler output
│   └── validate/             cases against expected.csaiv
│       ├── name.daiv             input document
│       └── name.expected         "pass" or an error name
├── invalid/NNN-name/     lexer-error cases
│   ├── input.kaiv|.saiv      malformed input
│   └── expected.error        error name from SPEC.md § Errors
└── compile-error/NNN-name/  post-lex compile errors
    ├── input.kaiv|.saiv      well-lexed but invalid input
    └── expected.error        application error name raised by the
                              Compiler / schema compiler
```

**Missing `expected.raiv` rule:** when a `valid/` vector has no
`expected.raiv`, it is identical to `expected.daiv` except that
the first line reads `.!raiv` instead of `.!daiv` (each
canonical kind opens with its own format declaration — SPEC.md
§ Format Declaration). Only vectors where the Denormalizer
changes the body carry a distinct `expected.raiv`: `$field`
references (preserved in `.raiv`, resolved in `.daiv`), schema
materialization (absent optional fields appear in `.daiv` only —
SPEC.md § Default Values), the head-type lift (untyped lines
carry `!str` in `.raiv` and the field's retained head type in
`.daiv` — SPEC.md § Null Semantics), and the `.!verbatim`
declaration (carried in `.raiv`, discharged in `.daiv` —
SPEC.md § Verbatim Documents).

## Runner contract

- `valid/`: run input.kaiv through Compiler and Denormalizer
  (schema-aware: when the input declares a `.!schema`, the
  Denormalizer resolves the compiled `.csaiv` through the
  vector's local registry and materializes absent optional
  fields); outputs must match the expected files byte for byte.
  A vector carrying `expected.error` in place of the golden
  files is a **build-error vector**: the pipeline must fail with
  the named error (the denorm-error surface —
  `UnitConversionError`, `DelegationSchemaError`, …).
- `schema/`: compile schema.saiv; output must match
  expected.csaiv byte for byte. Then validate each
  `validate/*.daiv` against expected.csaiv; the result must
  match the paired `.expected` file — the literal word `pass`,
  or the error name raised.
- `compile-error/`: run the full build — a `.kaiv` input goes
  through Compiler and schema-aware Denormalizer, a `.saiv`
  input through the schema compiler. The build must fail, and
  the application error's name must equal the contents of
  `expected.error` (one name, one line).
- `invalid/`: lex the input; the highest-priority error raised
  must equal the contents of `expected.error` (one name, one
  line). Priority order is the § Lexer Errors table.

Marker files a vector may carry at its root, all optional:

- `LEVEL` — one line naming the minimum Level the vector
  requires (`3`). A runner built without that Level's features
  skips the vector. Absent means Levels 0–2.
- `partial.expected` — for `LEVEL` 3 vectors: the error name
  (one name, one line) an honest-partial collation backend
  raises for every validate case in place of the per-case
  `.expected` outcome (the D-7 rule: a backend either evaluates
  exactly per SPEC.md §5 or rejects that constraint with
  `CollationUnsupportedError`; ordering differently is
  non-conformant). A `LEVEL` 3 vector without
  `partial.expected` uses only collations every conforming
  backend must either honor exactly or skip via `LEVEL`.
- `MODE` — resolution mode the runner must enable for the
  vector: `registry-strict` (compile-error/012). Absent means
  default resolution.

## Pinned assumptions

Behaviors the spec states normatively but only softly (SHOULD)
or by example are pinned here so vectors are byte-exact:

1. **Line endings** are LF throughout, and every file ends with
   a final EOL (SPEC.md § EOL) — except vectors that test the
   absence of one.
2. **Untyped authored lines** canonicalize to `!str` (the
   identity type) — stated normatively in SPEC.md
   § Unannotated scalars canonicalize to `!str`. Vectors that
   resolve named types stay self-contained and offline: each
   carries its own `kaiv.kaiv` and a local registry tree
   (`types/…/*.taiv`), exercising resolution Layer 2
   (`kaiv.kaiv`) with no network. (Layer 1 `.!registry`
   overrides are vectored in valid/045 and compile-error/012 —
   both carry a minimal `kaiv.kaiv` solely to anchor relative
   bases at the vector directory; the network Layers 3–4 are
   not vectored.)
3. **Declarations survive** into `.raiv`/`.daiv` (`.!schema`,
   `.!registry`, `.?id` — per SPEC.md § Named Types in Data
   Files canonical example and ARCHITECTURE.md §9), and the
   format declaration is rewritten to the output kind's keyword
   at each stage: Compiler emits `.!raiv`, Denormalizer emits
   `.!daiv`, schema compiler emits `.!csaiv` — materialized
   even when the authored source declares nothing, and
   normalized to the bare (versionless) form for the data
   kinds. The declaration is optional in authored `.kaiv` only
   (absent means authored `.kaiv` version 1 — the `.env`
   compatibility rule); canonical consumers require the
   matching kind declaration (`FORMAT_KIND_ERROR`). SPEC.md
   § Format Declaration is normative; `.!registry` survival
   remains SHOULD-strength, pinned here.
4. **Comments and blank lines are not emitted** into canonical
   output (matching every canonical block in SPEC.md worked
   examples). Inputs here avoid them except where classification
   itself is under test.
5. **`UNSUPPORTED_VERSION_ERROR` vectors assume a 1.x
   implementation** (any 1.y, y ≥ 0, is supported; 99 is not).
6. **Conformance runs are offline; network retrieval MUST NOT
   be attempted.** Every resolution a vector needs is satisfied
   by its own `kaiv.kaiv` and local registry tree (Layers 1–2);
   an `http(s)` schema reference or registry base raises
   `SchemaResolutionError` (compile-error/003 depends on this).
7. **Level 3 conformance runs build with the ICU backend at
   CLDR 48** — the spec's pinned reference data and options
   (SPEC.md §5.3). Level 3 vectors carry a `LEVEL` file
   (contents `3`); a Level 0–2 runner skips them. An
   honest-partial backend (D-7) substitutes the vector's
   `partial.expected` outcome, where present, for every
   validate case. Vector values avoid characters whose
   collation weights are unstable across recent CLDR versions
   (they use É/é, ß, ö, ASCII digits and case — stable under
   root and the exercised tailorings), so a backend on an
   adjacent CLDR version agrees on these vectors even though
   only CLDR 48 results are conformant by definition.

## Vector index

| Vector | SPEC.md source |
|---|---|
| valid/001-empty-document | § Empty Documents |
| valid/002-minimal-scalar | § The Data Model, § Format Declaration |
| valid/003-typed-scalars | § The `std/core` Standard Library |
| valid/004-namespaced-fields | § Namespaced Field Keys |
| valid/005-struct-assignment | § Structs |
| valid/006-scalar-arrays | § Arrays (`+=`/`;=` intermix) |
| valid/007-namespace-arrays | § Arrays Are Namespaces with Integer Fields (`+:=`) |
| valid/008-section-blocks | § Arrays Are Namespaces with Integer Fields (blocks) |
| valid/009-variables | § Worked Example — Variables |
| valid/010-field-references | § Worked Example — Field References |
| valid/011-namespace-block | § Namespace Blocks — Basic Example |
| valid/012-map | § Map Type |
| valid/013-quoted-names | § Quoted Names |
| valid/014-null-vs-empty | § Null Semantics |
| valid/015-provenance | § Provenance |
| valid/016-units | § Units (negative-exponent folding, `m*s^-1` → `m/s`) |
| valid/017-value-preservation | § Value Preservation |
| valid/018-whitespace | § Whitespace Handling |
| valid/019-comments | § The Six Rules (comment / doc-comment stripping) |
| valid/020-mixed-array | § Mixed Arrays |
| valid/021-crlf | § EOL (CRLF tolerance; canonical output is LF) |
| valid/022-section-multi-array | § Section Block Semantics (new section-open closes the current array) |
| valid/023-compound-units | § Canonical form (factor reorder, cancellation to `1`), § Currencies |
| valid/024-named-types | § Type Library Files, § Layer 2 (`kaiv.kaiv` + local registry tree), `&name` → `!lib/path/name` |
| valid/025-custom-units | § Unit Definition Files (`.faiv`), § Referencing custom units (`.!units`; custom currency); the `&AU=au` alias canonicalizes to the primary name — canonical lines never carry an alias |
| valid/026-std-enc | § The `std/enc` Encoding Library (`&json` → `!std/enc/json`, embedded resolution) |
| valid/027-typed-expansion | § Metadata Annotations (one annotation types a whole `:=`/`;=` expansion) |
| valid/028-std-time | § The `std/time` Time Library (`&datetime` → `!std/time/datetime`, `..time` lowering) |
| valid/029-std-num | § The `std/num` Numeric Markers Library (`&inf`/`&nan` resolution) |
| valid/030-quoted-normalization | § When to Quote (quoted bare-able names normalize to bare — one canonical representation) |
| valid/031-array-variables | § Variables, Array-variable splices (`$@.name` as the whole `+=`/`;=` right side, element-wise) |
| valid/032-dollar-interpolation | § Variables (`$.name` mid-value interpolation), § Value Preservation (the `$$` doubling) |
| valid/033-scoped-field-reference | § Field References (block-prefix qualification; `.raiv` preserves, `.daiv` resolves) |
| valid/034-utf8-values | § UTF-8 Processing, § Value Preservation (multibyte values, non-ASCII quoted name, non-ASCII through `$.var` and `$field` resolution) |
| valid/035-ns-var-splat | § Namespace-Variable Splat (`:=$/.name` right side; standalone `$/.name` line in a block) |
| valid/036-map-assign | § Map Type (map-assign-line: `/ns/path=k:v;…` entries, `={}` empty map) |
| valid/037-materialized-defaults | § Default Values, § Null Semantics — Materialization of Absent Fields (absent optional, resolved default) |
| valid/038-materialized-null | § Null Semantics — Materialization of Absent Fields (absent nullable optional → `!null'::field=`); § Declarations (flat `.!schema hub/x` space form) |
| valid/039-env-file | § Format Declaration (declaration optional in authored `.kaiv`: an env-style `KEY=value` file with `#` comments builds as-is; output materializes `.!daiv`) |
| valid/040-versioned-declaration | § Format Declaration (`.!kaiv 1.0` names version 1; the output declaration is the bare kind form `.!daiv`) |
| valid/041-text-type | § The text Type (`&text`/`!text` canonical shorthand; `|:|`-separated multi-line value passes through verbatim) |
| valid/042-info-units | § Built-in Units, Information units (`GiB` and `MiB/s` canonical; the `Mbps` telecom alias canonicalizes to `Mb/s`) |
| valid/043-provenance-partial | § Provenance Syntax (partial shapes — source-only, `?src@ts`, `?src#dpid`, full triple — and a multi-source provenance list, all surviving to `.daiv`) |
| valid/044-provenance-id-decl | § Provenance Syntax (`.?id uri` declarations mapping source IDs to URIs, surviving into canonical output — Pinned assumptions, item 3) |
| valid/045-registry-layer1 | § Type Registry Resolution, Layer 1 (in-document `.!registry prefix=./path` resolving a `.!types` import fully offline; the declaration survives into canonical output) |
| valid/046-allof-schemas | § Schema Composition (`allOf`) (two `.!schema` declarations on one document; each compiled schema contributes — materialization draws the second schema's default) |
| valid/047-nested-arrays | § Nesting Depth (depth 2: scalar array per element, nested namespace-array element fields via indexed section-open; depth 3: `/@cube/0/@planes/0/@points`) |
| valid/049-shebang | § Shebang Lines (first physical line `#!…` is a file-level directive, not a comment; not part of the kaiv text, so canonical output opens with the format declaration) |
| valid/050-quoted-interior | § Quoted Names, Examples in Canonical Form (quoted names as interior namepath segments: between `/` steps via a namespace block, after `@` on a section block, after `::`; the `""` doubling; authored with blocks per § When to Quote) |
| valid/051-unit-conversion | § Authored-Unit Conversion, D-14 (authored `!float:km` / `!int:min` under declared `:m` / `:s` heads: `.raiv` preserves the authored units, `.daiv` carries the declared units with exactly converted values — `42.5` → `42500`, `5` → `300`) |
| valid/052-elided-unit | § Elided-Type Unit Annotation, D-15 (`!:km` under a declared `!float:m` head inherits the head and converts; `!:h` on an undefined field under the relaxed schema resolves to `float`, the authored unit standing; `.raiv` preserves the `!:` form) |
| valid/053-delegation | § Namespace-Scoped Schemas, § Canonical Form, D-10 (the x509 shape end-to-end: `(/parameters schema:crypto/rsa-params)` emits the scoped-declaration discriminant into the header, the selected member composes under the prefix with membership checked, and the block's lines flatten and validate against it) |
| valid/054-inexact-conversion | § Authored-Unit Conversion, D-14 (build-error vector: authored `!float:m` under a declared `:yd` head divides by 0.9144 — the exact result is non-terminating, so the build fails with `UnitConversionError` rather than rounding) |
| valid/055-bad-delegation | § Delegated Namespaces in the Compiled Schema, D-10 (build-error vector: the block selects `crypto/dsa-params` — resolvable, but outside the declared set — and the build fails with `DelegationSchemaError`) |
| valid/056-info-conversion | § Authored-Unit Conversion, § Built-in Units (information units convert exactly: authored `!int:KiB` value `4` under a declared `:B` head becomes `4096`; `.raiv` keeps the authored `KiB`) |
| valid/057-custom-conversion | § Authored-Unit Conversion, § Unit Definition Files (custom `.faiv` units convert through their declared factors: authored `2 au` under a declared `:m` head becomes `299195741400`, and the compound `au/s` converts to `m/s`; `.raiv` keeps the authored units) |
| valid/058-instant-compact-deprecated | § Provenance (the deprecated compact 16-character instant `20250115T093000Z` is accepted on input and canonicalizes to the dashed 20-character form; producers never emit compact, and it is removed at 1.0) |
| valid/059-verbatim | § Verbatim Documents (`.!verbatim`: every `$` in a value is literal — no doubling, no references; the declaration is carried into `.raiv` and discharged in `.daiv`, whose bytes match the escaped-authored equivalent's) |
| valid/051-head-lift | § Unannotated Scalars Canonicalize to !str, § Null Semantics (the schema-aware Denormalizer lifts untyped lines to the field's retained head type — `!int`, unit-carrying `!float:km`; an explicit `!str` head never lifts; `.raiv` keeps the authored `!str`) |
| schema/001-server-config | § Named Types in Schemas, § Compiled Schema, § Parallel Scan Validation; § Format Declaration (`FORMAT_KIND_ERROR`: undeclared / wrong-kind canonical input) |
| schema/002-strict | § Errors (strict modifier) |
| schema/003-constraints | § Constraints (enum, range lowering), § Tagged unions (TypeMismatch on a `!str` field), § Errors |
| schema/004-length | § Length Constraints; § The Schema Compiler (`!str` before a leading `#` item) |
| schema/005-map | § Map Type, § Maps in the Compiled Schema (entry run, empty map) |
| schema/006-named-types | § Type Registry Resolution (Layer 2), § The Schema Compiler (transitive `.taiv` lowering) |
| schema/007-pattern-equals | § The Six Rules (rule-6 priority: `=` inside a pattern) |
| schema/008-nullable | § Tagged unions (per-alternative groups; null empty-payload; narrowing) |
| schema/009-std-time | § The `std/time` Time Library (RFC 3339 lowering, `..time` span) |
| schema/010-extended-float | § The `std/num` Numeric Markers Library (extended-real union `!float\|std/num/inf`) |
| schema/011-arrays | § Table Declarations in the Compiled Schema (authored `;=` vectors, constraint-free blocks, element runs) |
| schema/012-defaults | § Default Values (type defaults, the applicability cascade, `.csaiv` carrying the resolved default) |
| schema/013-provenance | § Requiring Provenance in Schemas (`.!provenance:required` propagation + enforcement) |
| schema/014-units | § Validation: units do not convert (retained `!type:unit` token); § Length Constraints (b64 byte→char translation) |
| schema/015-tables | § Table Declaration Syntax; § Validation Pass 2 (compound unique, foreign keys, cardinality) |
| schema/016-inheritance | § Encapsulated Hub Schema Extension (flat, `/ns`, and `/@arr` forms; redeclaration narrows in place) |
| schema/017-re-literal | § Constraint types (alternative-delimiter patterns `re{sep}…{sep}`, lowered to the canonical `/…/` form) |
| schema/018-relaxed-interleave | § Validator Pseudocode (undefined fields do not consume the schema pointer; defined-field order still enforced) |
| schema/019-table-groups | § Table Declaration Syntax (independent `\|` constraint groups; FK combined with unique) |
| schema/020-optional-unique | § Table Declaration Syntax (optional-unique: materialized empty values participate in uniqueness) |
| schema/021-bare-constraint-line | § Anonymous Refinement (bare constraint lines: implicit `str` narrowed by the items) |
| schema/022-hex-escape | § Constraint types (`\x27` hex escape names the apostrophe in a pattern body) |
| schema/023-std-net-math | § The `std/net` and `std/math` Libraries (`&email`/`&complex` lowering through `.!types` imports) |
| schema/024-text | § The text Type (`!text` retained in `.csaiv`; str→text coercion — plain lines pass and retype, `|:|`-carrying str values are `DelimiterCollisionError`, other types `TypeMismatchError`) |
| schema/025-lex-locale-range | § The Problem: `..lex` Is Byte Order, § Syntax: `..lex[locale]` (accented value inside a `..lex[fr]` range; the same value outside the identical bare-`..lex` range) |
| schema/026-lex-locale-enum | § Reference Collation: CLDR Version and Strength (collation governs enum equality: an NFD value matches an NFC member; a case-distinct value fails at tertiary strength) |
| schema/027-lex-locale-de | § Syntax: `..lex[locale]` (German: ß primary-equal to ss lands inside a `[strasse,strassf]` boundary; bare `..lex` contrast on the sibling field) |
| schema/028-lex-locale-taiv | § In Type Definitions (`.taiv`), § In Compiled Schema (`.csaiv`) (a LOCAL `.taiv` named type carrying `..lex[fr-CA]`, transitively lowered; range narrowing at use; `partial.expected` — fr-CA is outside exact-tier partial backends) |
| schema/029-lex-strength-override | § Reference Collation: CLDR Version and Strength (`-u-ks-level1` primary strength: accented value passes an unaccented enum; `partial.expected` per D-7 — partial backends reject overrides) |
| schema/030-lex-named-collation | § Reference Collation: CLDR Version and Strength (`de-DE-u-co-phonebk`, the spec's worked example: ö-as-oe inside a range where standard de-DE falls outside; `partial.expected`) |
| schema/031-lex-shifted | § Reference Collation: CLDR Version and Strength (`-u-ka-shifted`: punctuation ignorable at the pinned tertiary strength; default non-ignorable contrast on the sibling field; `partial.expected`) |
| schema/032-lex-root-defaults | § Reference Collation: CLDR Version and Strength (pinned root defaults under a tailored-but-default locale: no numeric reordering — "10" in a [1,5] range where "9" is out — and tertiary case sensitivity) |
| schema/033-ver-span | § Span Orderings (`..ver` dotted-numeric ordering: 1.9 inside [1.2,1.10], 1.11 outside); § Formal Grammar (the authored `\/` escape in a pattern body, preserved into `.csaiv`) |
| schema/034-taiv-units | § Units on Named Types (a LOCAL `.taiv` type defined `!float[0,]:km`; the unit is part of the type's identity and same-dimension narrowing to `:m` at use is legal; units byte-compare — a `:km` line against the `:m` field is `TypeMismatchError`) |
| schema/035-provenance-levels | § Requiring Provenance in Schemas (`.!provenance:source` propagated into `.csaiv`; source-only and full-triple pass, absent source fails) |
| schema/036-lex-unresolvable-tag | § Reference Collation: CLDR Version and Strength (a well-formed tag naming a language CLDR does not know compiles, but every conforming backend refuses it at validation with `CollationUnsupportedError` — root fallback is never silent; the D-8 ruling) |
| schema/037-lex-und-root | § Reference Collation: CLDR Version and Strength (`..lex[und]` is the explicit, legal request for CLDR root collation — range evaluation under the root order; the D-8 ruling) |
| schema/040-time-offsets | § Span Orderings (`..time` range evaluation compares RFC 3339 *instants*, offset-aware: a cross-offset spelling of an in-window instant passes; a value byte order would accept — same date, offset shifting it out of the window — fails) |
| schema/041-map-keys | § Maps in the Compiled Schema, Key constraints (the ostensive map block `(/name min=1 max=40)` + value annotation + `/regex/` key line, lowered to the `[key::…]` collection line and entry line; entry keys must match — quoted keys on their content — and entry counts obey the bounds) |
| schema/042-scalar-array-cardinality | § Cardinality Constraints (a `[min=1 max=3]` header on a scalar array — elements are `{arr}::N` value lines, counted by the Pass 1 cardinality counter; an empty document fails `min`, four elements fail `max`) |
| schema/043-versioned-saiv-header | § Format Declaration (`.!saiv 1.0 ID strict` names version 1 — the versioned identity header remains equivalent authored input; the emitted `.csaiv` header is the bare canonical form with the strict modifier preserved) |
| schema/044-b64-length | § std/core, D-13 (the quad-form `b64` pattern: encoded lengths ≡ 0/2/3 mod 4 pass — `abcd`, `abcdefg` — and the impossible ≡ 1 class fails the type itself, isolated from any length constraint) |
| schema/045-identity-declaration | § The Schema Compiler, § Anonymous Refinement, D-12 (retention by intent: authored `!str` is the nominal identity declaration — an `!int` assertion is `TypeMismatchError`; a bare-refinement or unannotated field is headless — assertions tolerated, the constraint governs; the unannotated field's lex-saver is the vacuous `/^/`) |
| schema/046-union-units | § Tagged Unions, § Units, D-11 (the nullable-quantity pattern `!null\|float:km`: unit glued to the compiled alternative; discriminant is name plus canonical unit — `!float:m` and unit-less `!float` are `TypeMismatchError`; `!null` carries and demands none) |
| schema/047-delegation | § Namespace-Scoped Schemas, § Delegated Namespaces in the Compiled Schema, D-10 (the ostensive parent block `(/parameters schema:A\|B)` + empty body compiles to the delegation line `/parameters [schema::A\|B]` — the namespace's entire compiled presence; compile-only until the sub-scan lands) |
| compile-error/001-metadata-without-target | § Errors (`MetadataWithoutTargetError`) |
| compile-error/002-duplicate-schema-key | § Schema Compilation Errors (`SchemaDuplicateKeyError`) |
| compile-error/003-url-schema-reference | § Type Registry Resolution (offline runs: an `http(s)` reference is `SchemaResolutionError`) |
| compile-error/004-required-field-absent | § Null Semantics — Materialization of Absent Fields (required-absent is a build-time `RequiredFieldSchemaError`) |
| compile-error/005-optional-without-default | § Default Values (`SchemaOptionalWithoutDefaultError`) |
| compile-error/006-undefined-reference | § Errors (`UndefinedReferenceError`: undefined `$.name`) |
| compile-error/007-variable-context | § Namespace-Variable Splat (`VariableContextError`: array variable in scalar position) |
| compile-error/008-delimiter-collision | § Errors (`DelimiterCollisionError`: `\|` in a `:=` pair value) |
| compile-error/009-schema-inheritance-cycle | § Encapsulated Hub Schema Extension (`SchemaInheritanceCycleError`) |
| compile-error/011-provenance-static | § Requiring Provenance in Schemas (`.!provenance:required` combined with an optional field is statically unsatisfiable — `ProvenanceSchemaError` at schema compile) |
| compile-error/012-registry-strict | § Type Registry Resolution, Trust model and strict resolution (`RegistryStrictError`: strict mode refuses a Layer 1 `.!registry` base before retrieval; carries the `MODE` marker `registry-strict`) |
| compile-error/015-map-literal-key | § Maps in the Compiled Schema, Key constraints (a literal key line — a required named entry — inside a map block is outside the pattern-key surface; the schema compiler rejects it statically as INVALID_CONSTRAINT) |
| compile-error/016-lex-malformed-tag | § Reference Collation: CLDR Version and Strength (a `..lex[tag]` locale failing BCP 47 well-formedness is INVALID_CONSTRAINT at schema-compile time — static and backend-independent, split by kind from the validation-time `CollationUnsupportedError`; the D-8 ruling) |
| compile-error/017-unsupported-version | § Format Declaration, § Errors (a digit-first first token on an identity-carrying declaration is the version slot; a well-formed major other than 1 — `.!saiv 99 acme/x` — is UNSUPPORTED_VERSION_ERROR, never an identity misread) |
| compile-error/020-verbatim-context | § Verbatim Documents (`VerbatimContextError`: a variable definition under `.!verbatim` — the machinery is banned positionally) |
| compile-error/021-verbatim-splat | § Verbatim Documents (`VerbatimContextError`: a standalone namespace-variable splat line under `.!verbatim`) |
| invalid/001-bom | § BOM handling |
| invalid/002-invalid-utf8 | § UTF-8 Processing |
| invalid/003-nul-byte | § Forbidden characters |
| invalid/004-bare-cr | § Forbidden characters |
| invalid/005-missing-final-eol | § EOL |
| invalid/006-invalid-version | § Format Declaration |
| invalid/007-unsupported-version | § Lexer Errors |
| invalid/008-empty-key | § Lexer Errors |
| invalid/009-missing-operator | § Lexer Errors |
| invalid/010-invalid-key | § Whitespace Handling |
| invalid/011-invalid-directive | § Declaration Inventory |
| invalid/012-invalid-constraint | § Lexer Errors |
| invalid/013-invalid-bare-name | § Lexer Errors (INVALID_KEY: hyphen in a bare segment) |
| invalid/014-empty-quoted-name | § Quoted Names, § Lexer Errors |
| invalid/015-unknown-unit | § Built-in units (no `.!units` imports: the namespace is closed and membership is lex-time-checked; with imports the Compiler checks) |
| invalid/016-pattern-apostrophe | § Formal Grammar (`p-char` excludes `'`) |
| invalid/017-old-struct-operator | § Formal Grammar (the retired `::=` operator fails loudly as INVALID_KEY) |
| invalid/018-bad-table-header | § Table Declaration Syntax (malformed header clause is INVALID_CONSTRAINT) |
| invalid/019-reserved-re | § Constraint types (bare `re` reserved in schema name position; quote to use) |
| invalid/020-unterminated-re | § Constraint types (unterminated `re{sep}` literal is INVALID_CONSTRAINT) |
| invalid/021-array-without-sigil | § Formal Grammar (`array-path` requires the `@` sigil on `+=`/`;=` left sides; INVALID_KEY) |
| invalid/022-leading-zero-index | § Section Block Semantics, Canonical index spelling (`00` is not a canonical index; INVALID_KEY) |
| invalid/023-struct-without-slash | § Formal Grammar (`struct-line` requires the leading `/` on its `ns-path`; INVALID_KEY) |
| invalid/024-kb-ambiguity | § Built-in Units, Information units (`KB` rejected as ambiguous at the unit grammar level) |
| invalid/026-error-priority | § Errors (one line violating two rules — no `=`, invalid bare name — reports the highest-priority error: `MISSING_OPERATOR_ERROR` outranks `INVALID_KEY_ERROR` in the § Lexer Errors order) |
| invalid/030-unquoted-dash-segment | § Quoted Names, When to Quote; § Errors (an unquoted interior namepath segment outside the bare-name grammar — a hyphen — is INVALID_KEY; non-bare names MUST be quoted) |
| invalid/031-empty-quoted-segment | § Quoted Names, Quoted Name Rules (a quoted name MUST contain at least one character — an empty quoted interior segment is INVALID_KEY) |

## Known coverage gaps

Normative behaviors specified but **not yet vectored**, tracked so
the suite is not mistaken for full coverage:

- **Network resolution** (Layers 3–4: registry redirects, the
  ktaiv.com/ksaiv.com defaults): vectors cover the offline Layers
  1–2 only (`.!registry`, `kaiv.kaiv`, local registry trees) —
  see Pinned assumptions, item 6.
- **Map key constraints** (`[key::/pat/]` collection clause) and
  entry-count bounds: no authored `.saiv` syntax is defined yet, so
  neither the schema compiler nor the Validator covers them.
- **Provenance**: partial shapes and `.?id` survival are now
  covered (valid/043, valid/044), `:source` and the static
  `required`-with-optional rejection too (schema/035,
  compile-error/011). Still unvectored: the `:none` prohibition
  level, and the malformed-provenance lexer surface (which
  named error a malformed `#dpid` or source-less qualifier
  raises is not pinned by SPEC.md § Errors). (Array variables
  `$@.` and the `$$` escape are covered — valid/031,
  valid/032.)
- **Namespace-scoped sub-schema delegation** (§3.5.3 `schema:`
  block annotation, DFA composition): no vectors yet — the
  surface is now specified (D-10: ostensive parent block
  `(/ns schema:A|B)` + empty body, compiled `[schema::A|B]`
  delegation line, mandatory scoped `.!schema:/ns A`
  discriminant in canonical output; SPEC.md § Delegated
  Namespaces in the Compiled Schema). **Landed**:
  `schema/047-delegation` (parent block → delegation line),
  `compile-error/018-delegation-inline` (inline fields →
  `SchemaDelegationError`), and `valid/053-delegation` — the
  end-to-end triple: the data block's annotation becomes the
  mandatory scoped-declaration discriminant in the header, the
  schema resolution composes the selected member under the
  prefix (membership checked — absent or non-member selection is
  `DelegationSchemaError`), and materialization/validation ride
  the composite. The member's own
  `strict`/`.!provenance` modifiers govern inside the namespace
  (kaiv-rs governing tests pin both). The
  discriminant error path is vectored: `valid/055-bad-delegation`
  (a resolvable selection outside the declared set is
  `DelegationSchemaError`).
- **Per-alternative units in unions** (D-11, SPEC.md § Tagged
  Unions): **landed** — `!null|float:km` (the nullable-quantity
  pattern) compiles with the unit glued to the alternative
  (`float:km(…)`); the discriminant match is name plus canonical
  unit. `schema/046-union-units` vectors compile, both pass
  variants, and the wrong-unit/missing-unit mismatches.
- **`!str` retention by intent** (D-12, SPEC.md § The Schema
  Compiler): **landed** — authored `!str` is always retained (the
  nominal identity declaration, items or not); unannotated fields
  and anonymous refinements carry no head, with the vacuous
  pattern `/^/` as the lex-saver where a leading item is needed.
  Fixtures migrated (schema/003, 007, 012, 013, 016, 017, 018);
  `schema/045-identity-declaration` vectors the
  nominal/structural split.
- **Tightened `b64` pattern** (D-13, SPEC.md § std/core):
  **landed** — the quad-form pattern replaces alphabet-only;
  `schema/014` migrated, and `schema/044-b64-length` vectors the
  impossible mod-4 class in isolation.
- **Unit immutability + authored-unit conversion** (D-14, SPEC.md
  § Units on Named Types, § Authored-Unit Conversion): **landed**
  for built-in units — `valid/051-unit-conversion` is the golden
  triple (`42.5 km` → `42500 m`, `5 min` → `300 s`; `.raiv` keeps
  the authored units). Exact decimal arithmetic; an inexact or
  type-violating result is `UnitConversionError`
  (`valid/054-inexact-conversion`); information units convert
  (`valid/056-info-conversion` — `4 KiB` → `4096 B`, exact);
  and custom (`.faiv`) units convert through their declared
  factors, aliases and compound dimensions included
  (`valid/057-custom-conversion` — `2 au` → `299195741400 m`).
  No unit conversion remains unimplemented; currencies never
  convert by design.
- **Elided-type unit annotation** (D-15, SPEC.md § Elided-Type
  Unit Annotation): **landed** — `valid/052-elided-unit` pins
  both resolutions: `!:km` under a declared `!float:m` head
  inherits and converts (`42.5` → `42500`), and `!:h` on an
  undefined field resolves to `float` with the authored unit
  standing; `.raiv` preserves the elided form, `.daiv` never
  carries it (a hand-made `.daiv` with `!:` is a
  `TypeMismatchError`).
- **Level 2 collection constraints** are covered: `unique`/`ref`/
  `min`/`max` and the O(N) Pass 2 in schema/015; independent `|`
  constraint groups and FK-combined-with-unique in schema/019;
  optional-unique omitted-field semantics in schema/020; the
  compound-key encoding boundary in schema/015 (which now also
  carries the exactly-min / exactly-max cardinality boundary
  passes).

## Out of suite scope, by design

Declared exclusions — normative surface this suite deliberately
does not golden-test, so their absence is not a coverage gap:

- **Mappings (`.maiv`, SPEC.md ch. 8)**: the mapping surface
  (header declarations, mapping lines, execution, publish-time
  validation) is exercised by registry E2E tooling, not by
  golden files here.
- **Validator-side multi-violation priority**: when one data
  text violates several schema constraints, which application
  error is reported first is unpinned by design (unlike the
  lexer's § Errors priority order, which invalid/026 pins).
  Validate cases therefore never construct inputs with two
  simultaneous violation kinds.
- **CLDR-version metadata reporting** (SPEC.md §5.3's MUST for
  non-CLDR-48 builds): an implementation-reporting requirement
  with no golden-file representation; checked by implementation
  tests, not vectors.
