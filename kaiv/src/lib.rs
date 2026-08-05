//! kaiv — reference implementation of the kaiv format, Levels 0–3
//! (Level 3 collation behind the default `collation-icu` feature).
//!
//! Pipeline (SPEC.md, ARCHITECTURE.md §7):
//!
//! ```text
//! .kaiv --[compiler]--> .raiv --[denorm]--> .daiv --[validator + .csaiv]--> pass/fail
//! .saiv --[schema]----> .csaiv
//! ```
//!
//! Authored `.kaiv` compiles to the canonical `.raiv`, which
//! denormalizes to `.daiv` — one fully-qualified line per scalar,
//! which is the form every consumer reads. A `.saiv` schema compiles
//! to `.csaiv` and validates a `.daiv`:
//!
//! ```
//! # fn main() -> Result<(), kaiv::PipelineError> {
//! let raiv = kaiv::compile(b".!kaiv 1\n!int\nport=8443\n")?;
//! let daiv = kaiv::denormalize(&raiv)?;
//! assert_eq!(daiv, ".!daiv\n!int'::port=8443\n");
//!
//! let csaiv = kaiv::compile_schema(b".!saiv acme/svc\n\n!int\nport=\n")?;
//! let schema = kaiv::parse_csaiv(&csaiv)?;
//! kaiv::validate(&daiv, &schema)?;
//! # Ok(())
//! # }
//! ```
//!
//! Failure is always a [`PipelineError`], whose [`name`] is the
//! spec's error string — the same string the conformance vectors
//! pin:
//!
//! ```
//! let e = kaiv::compile(b".!kaiv 1\nport=8443").unwrap_err();
//! assert_eq!(e.name(), Some("MISSING_FINAL_EOL_ERROR"));
//! ```
//!
//! Documents in foreign formats convert through the same canonical
//! form: each converter module (`json`, `yaml`, `toml`, `xml`,
//! `cbor`, `avro`, `proto`, `asn1`) has `import` and `export`, and
//! each sits behind the feature of its name.
//!
//! The executable definition of "correct" is the conformance tree in
//! the conformance vectors, their own public repository
//! (<https://gitlab.com/kaiv-format/conformance>), vendored here
//! and run by `tests/conformance.rs`. The Lexer implements the eager parsing model: the whole
//! text is validated before any token is emitted, and no tokens are
//! emitted on error.
//!
//! # Stability
//!
//! See the README for the versioning policy and the MSRV. The public
//! API is everything reachable from this page; the error enums are
//! `#[non_exhaustive]`, so match them with a wildcard arm.
//!
//! [`name`]: PipelineError::name

// The crate contains no `unsafe` and is meant to keep it that way:
// it parses untrusted input for a living.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod anno;
#[cfg(feature = "asn1")]
pub mod asn1;
#[cfg(feature = "avro")]
pub mod avro;
// Internal: base64url for the std/enc channel. The encoded form
// is part of the format; this helper is not part of the API.
mod b64;
mod bcp47;
pub mod builder;
#[cfg(feature = "cbor")]
pub mod cbor;
/// Level 3 collation backend: ICU4X (`collation-icu`, the default —
/// full CLDR fidelity) or colligo (`collation-colligo` — lightweight,
/// context-free exact tiers, wasm-friendly). With neither, only the
/// default byte order is available and `..lex[locale]` spans reject.
///
/// Cargo features are additive — two crates in one dependency graph
/// may each pick a backend — so enabling both is legal and resolves
/// to ICU4X, the higher-fidelity of the two. Selecting colligo means
/// enabling it *without* `collation-icu` (which the default feature
/// set turns on, so `default-features = false`).
#[cfg(feature = "collation-icu")]
#[path = "collate_icu.rs"]
pub mod collate;
#[cfg(all(feature = "collation-colligo", not(feature = "collation-icu")))]
#[path = "collate_colligo.rs"]
pub mod collate;
pub mod compiler;
pub mod config;
pub mod denorm;
pub mod doc;
pub mod error;
pub mod faiv;
pub mod fmt;
#[cfg(feature = "graphql")]
pub mod graphql;
pub mod infer;
#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "json")]
pub mod jsonschema;
pub mod lexer;
pub mod maiv;
#[cfg(feature = "net")]
mod net;
#[cfg(feature = "proto")]
pub mod proto;
pub mod resolve;
// Internal: the constraint-pattern engine. Patterns are a schema
// surface, the matcher behind them is not.
mod rex;
pub mod schema;
#[cfg(feature = "serde")]
pub mod serde;
// Internal: Level 2 table-header parsing and the compiled-header
// serialization, both schema-compiler machinery.
mod table;
pub mod taiv;
#[cfg(feature = "toml")]
pub mod toml;
pub mod unit;
pub mod validator;
#[cfg(feature = "xml")]
pub mod xml;
#[cfg(feature = "xsd")]
pub mod xsd;
#[cfg(feature = "yaml")]
pub mod yaml;

pub use builder::{DaivBuilder, KaivBuilder, Provenance};
pub use compiler::{compile, compile_with};
pub use config::Config;
pub use denorm::{denormalize, denormalize_with};
pub use doc::{Doc, FromDaiv, Typed, View};
pub use error::{AppError, AppErrorAt, LexError, LexErrorAt, PipelineError};
pub use fmt::{format_data, format_plain, unbuild};
pub use lexer::{lex, FileKind};
pub use resolve::{ResolutionEvent, ResolutionLayer, Resolver};
pub use schema::{check_type_lib, compile_schema, compile_schema_with};
pub use validator::{parse_csaiv, schema_for_daiv, validate, CompiledSchema, ProvenanceLevel};
