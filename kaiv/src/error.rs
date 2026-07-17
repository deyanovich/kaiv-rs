//! Error catalog per SPEC.md § Errors. Names match the spec strings
//! exactly; the conformance runner compares against them.

use std::fmt;

/// Lexer errors (SPEC.md § Lexer Errors), in priority order: when one
/// line raises several, the lowest discriminant wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LexError {
    Bom,
    InvalidUtf8,
    InvalidCharacter,
    MissingFinalEol,
    InvalidVersion,
    UnsupportedVersion,
    EmptyKey,
    MissingOperator,
    InvalidKey,
    InvalidDirective,
    InvalidConstraint,
}

impl LexError {
    pub fn name(self) -> &'static str {
        match self {
            LexError::Bom => "BOM_ERROR",
            LexError::InvalidUtf8 => "INVALID_UTF8_ERROR",
            LexError::InvalidCharacter => "INVALID_CHARACTER_ERROR",
            LexError::MissingFinalEol => "MISSING_FINAL_EOL_ERROR",
            LexError::InvalidVersion => "INVALID_VERSION_ERROR",
            LexError::UnsupportedVersion => "UNSUPPORTED_VERSION_ERROR",
            LexError::EmptyKey => "EMPTY_KEY_ERROR",
            LexError::MissingOperator => "MISSING_OPERATOR_ERROR",
            LexError::InvalidKey => "INVALID_KEY_ERROR",
            LexError::InvalidDirective => "INVALID_DIRECTIVE_ERROR",
            LexError::InvalidConstraint => "INVALID_CONSTRAINT_ERROR",
        }
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A lexer error with the 1-based line it was detected on
/// (0 = whole-document errors reported without a line number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexErrorAt {
    pub error: LexError,
    pub line: usize,
}

/// Application errors (SPEC.md § Application Errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppError {
    MetadataWithoutTarget,
    /// A value references an undefined hidden variable or data field,
    /// or contains a `$` that begins neither a well-formed reference
    /// nor the `$$` doubling (SPEC.md § Errors).
    UndefinedReference,
    /// A container-variable reference appears where its expansion
    /// cannot be placed: `$/.name` outside the two splat positions,
    /// or `$@.name` in a scalar position (SPEC.md
    /// § Namespace-Variable Splat).
    VariableContext,
    /// A compound-form value collides with that form's delimiter:
    /// `|` in a `:=`/`+:=` pair value, `;` in `;=` data, `:` or `;`
    /// in an inline map entry (SPEC.md § Errors).
    DelimiterCollision,
    SchemaDuplicateKey,
    /// A `.!schema` inheritance chain among `.saiv` files revisits a
    /// schema already in the chain.
    SchemaInheritanceCycle,
    /// An optional field whose resolved default is inapplicable and
    /// whose type does not admit `!null` — the Denormalizer would
    /// have nothing to materialize for an absent instance (SPEC.md
    /// § Default Values).
    SchemaOptionalWithoutDefault,
    SchemaResolution,
    RequiredFieldSchema,
    DuplicateKeySchema,
    UndefinedFieldStrictSchema,
    ProvenanceSchema,
    TypeMismatch,
    ConstraintViolation,
    UniquenessViolation,
    ReferentialIntegrity,
    CardinalityViolation,
    CollationUnsupported,
}

impl AppError {
    pub fn name(self) -> &'static str {
        match self {
            AppError::MetadataWithoutTarget => "MetadataWithoutTargetError",
            AppError::UndefinedReference => "UndefinedReferenceError",
            AppError::VariableContext => "VariableContextError",
            AppError::DelimiterCollision => "DelimiterCollisionError",
            AppError::SchemaDuplicateKey => "SchemaDuplicateKeyError",
            AppError::SchemaInheritanceCycle => "SchemaInheritanceCycleError",
            AppError::SchemaOptionalWithoutDefault => "SchemaOptionalWithoutDefaultError",
            AppError::SchemaResolution => "SchemaResolutionError",
            AppError::RequiredFieldSchema => "RequiredFieldSchemaError",
            AppError::DuplicateKeySchema => "DuplicateKeySchemaError",
            AppError::UndefinedFieldStrictSchema => "UndefinedFieldStrictSchemaError",
            AppError::ProvenanceSchema => "ProvenanceSchemaError",
            AppError::TypeMismatch => "TypeMismatchError",
            AppError::ConstraintViolation => "ConstraintViolationError",
            AppError::UniquenessViolation => "UniquenessViolationError",
            AppError::ReferentialIntegrity => "ReferentialIntegrityError",
            AppError::CardinalityViolation => "CardinalityViolationError",
            AppError::CollationUnsupported => "CollationUnsupportedError",
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An application error with the context the Validator attaches at
/// the failure site. The bare [`AppError`] name stays the pinned
/// spec string (conformance compares it); `line` and `context` are
/// presentation — which `.daiv` line and which field/value/constraint
/// were involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppErrorAt {
    pub error: AppError,
    /// 1-based line in the `.daiv` input; 0 when the failure is not
    /// tied to one data line (e.g. a field missing at end of input).
    pub line: usize,
    /// Human-readable site description; empty when none applies.
    pub context: String,
}

impl AppErrorAt {
    pub fn bare(error: AppError) -> Self {
        Self {
            error,
            line: 0,
            context: String::new(),
        }
    }
}

impl fmt::Display for AppErrorAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)?;
        if !self.context.is_empty() {
            write!(f, ": {}", self.context)?;
        }
        if self.line > 0 {
            write!(f, " (line {})", self.line)?;
        }
        Ok(())
    }
}

/// Any failure along the build pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    Lex(LexErrorAt),
    App(AppError),
    /// Compiler-internal malformation with context (a condition the
    /// spec assigns to no named error).
    Other(String),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::Lex(e) if e.line == 0 => e.error.fmt(f),
            PipelineError::Lex(e) => write!(f, "{} (line {})", e.error, e.line),
            PipelineError::App(e) => e.fmt(f),
            PipelineError::Other(s) => f.write_str(s),
        }
    }
}
