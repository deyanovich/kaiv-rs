//! kaiv — reference implementation of the kaiv format, Levels 0–3
//! (Level 3 collation behind the default `collation` feature).
//!
//! Pipeline (SPEC.md, ARCHITECTURE.md §7):
//!
//! ```text
//! .kaiv --[compiler]--> .raiv --[denorm]--> .daiv --[validator + .csaiv]--> pass/fail
//! .saiv --[schema]----> .csaiv
//! ```
//!
//! The executable definition of "correct" is the conformance tree in
//! the spec repo (`spec/kaiv/conformance/`); `tests/conformance.rs`
//! runs it. The Lexer implements the eager parsing model: the whole
//! text is validated before any token is emitted, and no tokens are
//! emitted on error.

pub mod anno;
#[cfg(feature = "asn1")]
pub mod asn1;
#[cfg(feature = "avro")]
pub mod avro;
pub mod builder;
#[cfg(feature = "cbor")]
pub mod cbor;
#[cfg(feature = "collation")]
pub mod collate;
pub mod compiler;
pub mod config;
pub mod denorm;
pub mod error;
pub mod faiv;
#[cfg(feature = "graphql")]
pub mod graphql;
pub mod infer;
#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "json")]
pub mod jsonschema;
pub mod lexer;
#[cfg(feature = "net")]
mod net;
#[cfg(feature = "proto")]
pub mod proto;
pub mod resolve;
pub mod rex;
pub mod schema;
pub mod table;
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

pub use builder::{DaivBuilder, Provenance};
pub use compiler::{compile, compile_with};
pub use config::Config;
pub use denorm::{denormalize, denormalize_with};
pub use error::{AppError, AppErrorAt, LexError, LexErrorAt, PipelineError};
pub use lexer::{lex, FileKind};
pub use resolve::Resolver;
pub use schema::{check_type_lib, compile_schema, compile_schema_with};
pub use validator::{parse_csaiv, schema_for_daiv, validate, CompiledSchema, ProvenanceLevel};
