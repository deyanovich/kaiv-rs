//! kaiv — reference implementation of the kaiv format, Levels 0–2.
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
pub mod compiler;
pub mod config;
pub mod denorm;
pub mod error;
pub mod faiv;
pub mod infer;
#[cfg(feature = "json")]
pub mod json;
#[cfg(feature = "json")]
pub mod jsonschema;
pub mod lexer;
pub mod resolve;
pub mod rex;
pub mod schema;
pub mod table;
pub mod taiv;
#[cfg(feature = "toml")]
pub mod toml;
pub mod unit;
pub mod validator;
#[cfg(feature = "yaml")]
pub mod yaml;

pub use compiler::{compile, compile_with};
pub use config::Config;
pub use denorm::denormalize;
pub use error::{AppError, LexError, LexErrorAt, PipelineError};
pub use lexer::{lex, FileKind};
pub use resolve::Resolver;
pub use schema::{compile_schema, compile_schema_with};
pub use validator::{parse_csaiv, validate, CompiledSchema, ProvenanceLevel};
