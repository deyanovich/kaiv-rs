# kaiv-cli

Command-line toolchain for the [kaiv format](https://kaiv.io/) —
installs a `kaiv` binary wrapping the [`kaiv`
library](https://crates.io/crates/kaiv) (Levels 0–2).

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
kaiv import   [--FORMAT] [file]   foreign format -> authored .kaiv
kaiv export   --FORMAT [file]     canonical kaiv -> foreign format
kaiv infer    [--name ID] [file]  infer an authored .saiv from data
kaiv import-schema [--name] [f]   foreign schema -> authored .saiv
                                  (JSON Schema, .proto, .avsc,
                                  GraphQL SDL, .xsd)
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

Type, schema, and unit resolution is configured by the nearest
`kaiv.kaiv` (itself a kaiv file) found from the working directory
upward, plus `KAIV_REGISTRY_*` environment overrides.

## Example

```
$ echo '{"host":"a.example","port":8443}' | kaiv import --json
.!kaiv 1

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
