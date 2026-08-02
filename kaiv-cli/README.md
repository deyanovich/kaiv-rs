# kaiv-cli

Command-line toolchain for the [kaiv format](https://kaiv.io/) —
installs a `kaiv` binary wrapping the [`kaiv`
library](https://crates.io/crates/kaiv) (Levels 0–3).

```
cargo install kaiv-cli
```

## Commands

```
kaiv compile  [file.kaiv]         authored -> relational canonical (.raiv)
kaiv denorm   [file.raiv]         relational -> denormalized (.daiv)
kaiv build    [file.kaiv]         authored -> .daiv (compile + denorm)
kaiv schema   [file.saiv]         authored schema -> compiled (.csaiv)
kaiv validate <data> <schema>     validate data against a schema
kaiv unit     <expr>              canonicalize a unit expression
kaiv fmt      [file] [--check]    format an authoring file into the
                                  standard style, in place
kaiv unbuild  [file]              canonical .daiv/.raiv -> authored
                                  .kaiv (build's inverse direction)
kaiv mapping  validate <m.maiv>   check a mapping against its schemas
kaiv mapping  apply <m> [data]    source document -> target .daiv
kaiv mapping  compose <a> <b>     compose two mappings into one
kaiv import   [--FORMAT] [file]   foreign format -> authored .kaiv
kaiv export   --FORMAT [file]     canonical kaiv -> foreign format
kaiv infer    [--name ID] [file]  infer an authored .saiv from data
kaiv import-schema [--name] [f]   foreign schema -> authored .saiv
                                  (JSON Schema, .proto, .avsc,
                                  GraphQL SDL, .xsd)
kaiv publish  <paths...>          publish artifacts to the kaiv
                                  registries (--batch for sets;
                                  --dry-run for the plan)
kaiv login    [email]             sign in to the kaiv registries
kaiv whoami                       the signed-in account
kaiv logout                       revoke and forget the session
```

Formats: `--json` `--yaml` `--toml` `--xml` `--cbor` `--avro`
`--proto` `--asn1`, inferred from the file extension (`.json`
`.yaml` `.yml` `.toml` `.xml` `.cbor` `.avro` `.pb` `.binpb` `.der`
`.pem` `.crt` `.cer`); the option is required for stdin. The binary
formats (cbor, avro, proto, asn1) write raw bytes to stdout on
export. Protocol Buffers wire data is not self-describing: pass
`--schema <file.proto>` (and `--message <Name>` when the schema has
several top-level messages). ASN.1 input may be raw BER/DER or
PEM-armored; export writes DER. The single-file commands read
stdin when no file is given. `validate` accepts authored or
foreign-format data and authored or compiled schemas, converting
as needed.

`fmt` is the standard formatter for what humans write: it picks,
per group of fields, the most readable of the three equivalent
syntaxes (flat namepath line, inline `:=`/`+:=` assignment within
72 columns, `(...)`/`[...]` block), honoring authored blank lines
as grouping hints and never touching semantics — values, order,
comments, and variables are preserved exactly. A named file is
rewritten in place; stdin prints to stdout; `--check` exits nonzero
when a file is not already formatted (for CI). Canonical
`.daiv`/`.raiv` have exactly one spelling, so `fmt` refuses them
and points at `unbuild`.

`unbuild` goes the other way: canonical `.daiv`/`.raiv` back to
authored `.kaiv`. It is build's inverse *direction*, not a round
trip — sugar the compiler resolved away (comments, variables,
references) does not come back, though `.raiv` preserves more than
`.daiv`, authored units included.

`mapping` works with `.maiv` files, which are edges between two
schemas. `validate` checks one statically (namepaths, overrides,
and whether every required target field is produced); `apply` runs
a source document through it; `compose` joins `B<-A` and `C<-B`
into `C<-A`, recording each hop in the `.!via` trail.

Type, schema, and unit resolution is configured by the nearest
`kaiv.kaiv` (itself a kaiv file) found from the working directory
upward, plus `KAIV_REGISTRY_*` environment overrides.

`login` is passwordless: an emailed one-time link approves the
device (compare the code the CLI prints against the one in the
mail), and the first sign-in creates the account. The stored
credential is a rotating refresh token at
`~/.config/kaiv/credentials` (mode 0600); access tokens are
minted on demand. `KAIV_ID_URL` overrides the identity host
(default `https://id.kaiv.io` during the alpha).

## Publishing

`kaiv publish` writes artifacts to the registries through the
synchronous validation gate: every deposit is checked by the
reference validator at the gate, and a refusal carries the
validator's own error message verbatim. Published artifacts are
write-once eternalinks — republishing identical bytes succeeds
idempotently, different bytes under a taken name are refused.

The registry is chosen by extension (`.taiv` types, `.saiv` /
`.csaiv` / `.maiv` schemas, `.faiv` units, `.kaiv` / `.raiv` /
`.daiv` documents), and the published address is
`{namespace}/{name}.{ext}`. Library artifacts carry their own
identity in the format declaration (a file declaring
`.!taiv acme/net` publishes `acme/net.taiv`); data documents
publish by basename under `--namespace`; `.daiv` is
content-addressed (the name is the BLAKE3 hex of the bytes);
`.maiv` takes an explicit `--as` address.

```
$ kaiv publish types/net.taiv
published acme/net.taiv (sha256 4f0c…)

$ kaiv publish schemas/ types/ --batch
published 5/5 artifact(s) in 1 round(s)
  ok         acme/net.taiv
  ...
```

`--batch` sends one SRCN pack per cella in registry order (t, f,
s, d) so a batch may carry its own dependency closure in any
order: records denied for a not-yet-published dependency are
retried in fixed-point rounds, on the gate within each pack and
by the client across packs. `--dry-run` prints the fully
resolved plan without sending anything.

Authentication is the `--token` flag, the `KAIV_TOKEN`
environment variable, or the stored `kaiv login` session, in
that order. Publishes target the alpha `{t,s,f,d}.kaiv.io`
hosts by default (`--gate` / `KAIV_GATE_URL` override); the
production `k*aiv.com` registries are refused unless
`--production` is passed.

## Example

```
$ echo '{"host":"a.example","port":8443}' | kaiv import --json
.!kaiv

host=a.example
!int
port=8443

$ kaiv infer --name acme/svc config.kaiv > svc.saiv
$ kaiv validate config.kaiv svc.saiv
pass
```

## License

Licensed under either of [Apache License, Version
2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
