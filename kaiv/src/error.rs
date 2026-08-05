//! Error catalog per SPEC.md § Errors. Names match the spec strings
//! exactly; the conformance runner compares against them.

use std::fmt;

/// Lexer errors (SPEC.md § Lexer Errors), in priority order: when one
/// line raises several, the lowest discriminant wins.
///
/// Non-exhaustive: the catalog tracks the spec, which still has
/// levels to land, so a match on this type needs a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum LexError {
    /// Text begins with the UTF-8 BOM `EF BB BF`.
    Bom,
    /// Input contains invalid UTF-8 sequences.
    InvalidUtf8,
    /// A standalone CR not part of a CRLF, or a NUL.
    InvalidCharacter,
    /// The final non-empty line lacks its EOL terminator.
    MissingFinalEol,
    /// The version after a format-declaration keyword is not
    /// `N`, `N.N`, or `N.N.N`.
    InvalidVersion,
    /// The version is well-formed but not one this
    /// implementation supports.
    UnsupportedVersion,
    /// A stream consumed as a canonical kind (.raiv/.daiv/.csaiv)
    /// does not open with the matching format declaration (SPEC.md
    /// § Format Declaration).
    FormatKind,
    /// A data line starts with `=`: the key is empty or pure
    /// whitespace.
    EmptyKey,
    /// A line that should carry data has no `=`.
    MissingOperator,
    /// A bare key — or any unquoted namepath segment — is not a
    /// `bare-name` (a leading digit, a `-`, a `.`), or a quoted
    /// key is empty or misquotes `"`.
    InvalidKey,
    /// A `.!` line does not open with a keyword from the
    /// declaration inventory.
    InvalidDirective,
    /// A constraint clause matches none of the constraint
    /// productions, or a unit annotation names a unit in
    /// neither the built-in set nor an imported `.faiv`.
    InvalidConstraint,
}

impl LexError {
    /// The spec's error name, exactly as the conformance vectors pin
    /// it (`"BOM_ERROR"`, …).
    pub fn name(self) -> &'static str {
        match self {
            LexError::Bom => "BOM_ERROR",
            LexError::InvalidUtf8 => "INVALID_UTF8_ERROR",
            LexError::InvalidCharacter => "INVALID_CHARACTER_ERROR",
            LexError::MissingFinalEol => "MISSING_FINAL_EOL_ERROR",
            LexError::InvalidVersion => "INVALID_VERSION_ERROR",
            LexError::UnsupportedVersion => "UNSUPPORTED_VERSION_ERROR",
            LexError::FormatKind => "FORMAT_KIND_ERROR",
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

impl std::error::Error for LexError {}

/// A lexer error with the 1-based line it was detected on
/// (0 = whole-document errors reported without a line number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexErrorAt {
    /// The spec error raised.
    pub error: LexError,
    /// 1-based line in the input; 0 when the failure is not tied to
    /// one line.
    pub line: usize,
}

impl fmt::Display for LexErrorAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)?;
        if self.line > 0 {
            write!(f, " (line {})", self.line)?;
        }
        Ok(())
    }
}

impl std::error::Error for LexErrorAt {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Application errors (SPEC.md § Application Errors).
///
/// Non-exhaustive, for the same reason as [`LexError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AppError {
    /// A metadata annotation in authored `.kaiv` is not followed
    /// by a data line before the next blank line, comment, or
    /// end of input.
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
    /// A document declaring `.!verbatim` contains reference
    /// machinery in a positional form — a variable-definition line,
    /// a standalone splat line, or a whole-value splice — or the
    /// declaration itself is misused: arguments, a repeat, or a
    /// position after content (SPEC.md § Verbatim Documents).
    VerbatimContext,
    /// A compound-form value collides with that form's delimiter:
    /// `|` in a `:=`/`+:=` pair value, `;` in `;=` data, `:` or `;`
    /// in an inline map entry (SPEC.md § Errors).
    DelimiterCollision,
    /// A `.saiv` text defines the same field twice.
    SchemaDuplicateKey,
    /// A `.!schema` inheritance chain among `.saiv` files revisits a
    /// schema already in the chain.
    SchemaInheritanceCycle,
    /// A parent schema both delegates a namespace and declares
    /// fields under it, or declares an empty/duplicate member set
    /// (SPEC.md § Namespace-Scoped Schemas, D-10).
    SchemaDelegation,
    /// A document under a delegating schema carries no scoped
    /// declaration for the namespace, or one outside the set.
    DelegationSchema,
    /// An optional field whose resolved default is inapplicable and
    /// whose type does not admit `!null` — the Denormalizer would
    /// have nothing to materialize for an absent instance (SPEC.md
    /// § Default Values).
    SchemaOptionalWithoutDefault,
    /// A `.!schema`, `.!types`, `.!source`, `.!target` or `.!via`
    /// reference cannot be retrieved from its URL or registry.
    SchemaResolution,
    /// Strict resolution mode: a document-level `.!registry`
    /// declaration would determine the base of a resolved artifact
    /// (SPEC.md § Type Registry Resolution, strict mode).
    RegistryStrict,
    /// A field the schema declares required (`=`) is absent — from
    /// the authored data at build time, or from the `.daiv` at
    /// validation time.
    RequiredFieldSchema,
    /// The data carries two or more entries for one
    /// schema-declared field.
    DuplicateKeySchema,
    /// The data carries an entry for a field a strict schema does
    /// not define.
    UndefinedFieldStrictSchema,
    /// A data line violates the schema's `.!provenance` requirement
    /// level — missing source or timestamp under `required` /
    /// `source`, or any provenance at all under `none`.
    ProvenanceSchema,
    /// A canonical consumer received a stream without the matching
    /// format declaration (SPEC.md § Format Declaration). Mirrors
    /// `LexError::FormatKind` for the Validator's error channel;
    /// both render the same spec name.
    FormatKind,
    /// The data line's type annotation fails the field's nominal
    /// requirement: no union alternative matches the discriminant,
    /// or a `!null` / `std/enc` line meets a head admitting neither.
    TypeMismatch,
    /// An authored same-dimension unit cannot convert exactly into
    /// the field's declared unit (SPEC.md § Authored-Unit
    /// Conversion, D-14).
    UnitConversion,
    /// A value fails a pattern, range, enumeration, or length
    /// constraint.
    ConstraintViolation,
    /// A field declared `[unique::field]` (Level 2) has duplicate
    /// values across array elements.
    UniquenessViolation,
    /// A `.maiv` leaves a required target-schema field unproduced
    /// (SPEC.md § Publish-Time Validation) — checked statically.
    IncompleteMapping,
    /// A field declared `[ref::field=/@path]` (Level 2) holds a
    /// value absent from the referenced field set.
    ReferentialIntegrity,
    /// An array's element count violates `min=N` or `max=M`.
    CardinalityViolation,
    /// A `..lex[locale]` constraint cannot be evaluated exactly:
    /// a Level 0–2 runtime configured to reject rather than fall
    /// back to bare `..lex`, or a Level 3 validator whose backend
    /// does not cover the locale.
    CollationUnsupported,
}

impl AppError {
    /// The spec's error name, exactly as the conformance vectors pin
    /// it (`"TypeMismatchError"`, …).
    pub fn name(self) -> &'static str {
        match self {
            AppError::MetadataWithoutTarget => "MetadataWithoutTargetError",
            AppError::UndefinedReference => "UndefinedReferenceError",
            AppError::VariableContext => "VariableContextError",
            AppError::VerbatimContext => "VerbatimContextError",
            AppError::DelimiterCollision => "DelimiterCollisionError",
            AppError::SchemaDuplicateKey => "SchemaDuplicateKeyError",
            AppError::SchemaInheritanceCycle => "SchemaInheritanceCycleError",
            AppError::SchemaDelegation => "SchemaDelegationError",
            AppError::DelegationSchema => "DelegationSchemaError",
            AppError::SchemaOptionalWithoutDefault => "SchemaOptionalWithoutDefaultError",
            AppError::SchemaResolution => "SchemaResolutionError",
            AppError::RegistryStrict => "RegistryStrictError",
            AppError::RequiredFieldSchema => "RequiredFieldSchemaError",
            AppError::DuplicateKeySchema => "DuplicateKeySchemaError",
            AppError::UndefinedFieldStrictSchema => "UndefinedFieldStrictSchemaError",
            AppError::ProvenanceSchema => "ProvenanceSchemaError",
            AppError::FormatKind => "FORMAT_KIND_ERROR",
            AppError::TypeMismatch => "TypeMismatchError",
            AppError::UnitConversion => "UnitConversionError",
            AppError::ConstraintViolation => "ConstraintViolationError",
            AppError::UniquenessViolation => "UniquenessViolationError",
            AppError::IncompleteMapping => "IncompleteMappingError",
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

impl std::error::Error for AppError {}

/// An application error with the context the Validator attaches at
/// the failure site. The bare [`AppError`] name stays the pinned
/// spec string (conformance compares it); `line` and `context` are
/// presentation — which `.daiv` line and which field/value/constraint
/// were involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppErrorAt {
    /// The spec error raised.
    pub error: AppError,
    /// 1-based line in the `.daiv` input; 0 when the failure is not
    /// tied to one data line (e.g. a field missing at end of input).
    pub line: usize,
    /// Human-readable site description; empty when none applies.
    pub context: String,
}

impl AppErrorAt {
    /// The error with no site attached — for failures raised where no
    /// data line is in hand.
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

impl std::error::Error for AppErrorAt {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Any failure along the build pipeline.
///
/// Non-exhaustive: match with a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PipelineError {
    /// A lexer error, with its line.
    Lex(LexErrorAt),
    /// An application error, with whatever site the raising stage
    /// knew (often none — see [`AppErrorAt::bare`]).
    App(AppErrorAt),
    /// Compiler-internal malformation with context (a condition the
    /// spec assigns to no named error).
    Other(String),
}

impl PipelineError {
    /// An application error raised with no site attached.
    pub fn app(error: AppError) -> Self {
        PipelineError::App(AppErrorAt::bare(error))
    }

    /// The spec error name behind this failure, when it has one.
    /// `Other` carries no spec error and yields `None`.
    pub fn name(&self) -> Option<&'static str> {
        match self {
            PipelineError::Lex(e) => Some(e.error.name()),
            PipelineError::App(e) => Some(e.error.name()),
            _ => None,
        }
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipelineError::Lex(e) => e.fmt(f),
            PipelineError::App(e) => e.fmt(f),
            PipelineError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for PipelineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PipelineError::Lex(e) => Some(e),
            PipelineError::App(e) => Some(e),
            _ => None,
        }
    }
}

impl From<LexErrorAt> for PipelineError {
    fn from(e: LexErrorAt) -> Self {
        PipelineError::Lex(e)
    }
}

impl From<AppErrorAt> for PipelineError {
    fn from(e: AppErrorAt) -> Self {
        PipelineError::App(e)
    }
}

impl From<LexError> for PipelineError {
    fn from(error: LexError) -> Self {
        PipelineError::Lex(LexErrorAt { error, line: 0 })
    }
}

impl From<AppError> for PipelineError {
    fn from(error: AppError) -> Self {
        PipelineError::app(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names are the spec's, and the conformance vectors compare
    /// against them verbatim — a typo in one is a silently wrong
    /// error report for any category no vector happens to cover.
    #[test]
    fn every_error_name_matches_the_spec() {
        let lex = [
            (LexError::Bom, "BOM_ERROR"),
            (LexError::InvalidUtf8, "INVALID_UTF8_ERROR"),
            (LexError::InvalidCharacter, "INVALID_CHARACTER_ERROR"),
            (LexError::MissingFinalEol, "MISSING_FINAL_EOL_ERROR"),
            (LexError::InvalidVersion, "INVALID_VERSION_ERROR"),
            (LexError::UnsupportedVersion, "UNSUPPORTED_VERSION_ERROR"),
            (LexError::FormatKind, "FORMAT_KIND_ERROR"),
            (LexError::EmptyKey, "EMPTY_KEY_ERROR"),
            (LexError::MissingOperator, "MISSING_OPERATOR_ERROR"),
            (LexError::InvalidKey, "INVALID_KEY_ERROR"),
            (LexError::InvalidDirective, "INVALID_DIRECTIVE_ERROR"),
            (LexError::InvalidConstraint, "INVALID_CONSTRAINT_ERROR"),
        ];
        for (e, name) in lex {
            assert_eq!(e.name(), name);
            assert_eq!(e.to_string(), name);
        }

        let app = [
            (
                AppError::MetadataWithoutTarget,
                "MetadataWithoutTargetError",
            ),
            (AppError::UndefinedReference, "UndefinedReferenceError"),
            (AppError::VariableContext, "VariableContextError"),
            (AppError::VerbatimContext, "VerbatimContextError"),
            (AppError::DelimiterCollision, "DelimiterCollisionError"),
            (AppError::SchemaDuplicateKey, "SchemaDuplicateKeyError"),
            (
                AppError::SchemaInheritanceCycle,
                "SchemaInheritanceCycleError",
            ),
            (AppError::SchemaDelegation, "SchemaDelegationError"),
            (AppError::DelegationSchema, "DelegationSchemaError"),
            (
                AppError::SchemaOptionalWithoutDefault,
                "SchemaOptionalWithoutDefaultError",
            ),
            (AppError::SchemaResolution, "SchemaResolutionError"),
            (AppError::RegistryStrict, "RegistryStrictError"),
            (AppError::RequiredFieldSchema, "RequiredFieldSchemaError"),
            (AppError::DuplicateKeySchema, "DuplicateKeySchemaError"),
            (
                AppError::UndefinedFieldStrictSchema,
                "UndefinedFieldStrictSchemaError",
            ),
            (AppError::ProvenanceSchema, "ProvenanceSchemaError"),
            // Deliberately SCREAMING_CASE: it mirrors the lexer error
            // of the same name, and the spec pins that spelling.
            (AppError::FormatKind, "FORMAT_KIND_ERROR"),
            (AppError::TypeMismatch, "TypeMismatchError"),
            (AppError::UnitConversion, "UnitConversionError"),
            (AppError::ConstraintViolation, "ConstraintViolationError"),
            (AppError::UniquenessViolation, "UniquenessViolationError"),
            (AppError::IncompleteMapping, "IncompleteMappingError"),
            (AppError::ReferentialIntegrity, "ReferentialIntegrityError"),
            (AppError::CardinalityViolation, "CardinalityViolationError"),
            (AppError::CollationUnsupported, "CollationUnsupportedError"),
        ];
        for (e, name) in app {
            assert_eq!(e.name(), name);
            assert_eq!(e.to_string(), name);
        }
    }

    /// Lexer priority is the discriminant order: the lowest-numbered
    /// error a line raises is the one reported.
    #[test]
    fn lex_errors_order_by_priority() {
        assert!(LexError::Bom < LexError::InvalidUtf8);
        assert!(LexError::InvalidUtf8 < LexError::InvalidCharacter);
        assert!(LexError::EmptyKey < LexError::InvalidConstraint);
    }

    #[test]
    fn display_carries_the_site() {
        let at = AppErrorAt {
            error: AppError::TypeMismatch,
            line: 7,
            context: "field /a::b".into(),
        };
        assert_eq!(at.to_string(), "TypeMismatchError: field /a::b (line 7)");
        assert_eq!(
            AppErrorAt::bare(AppError::TypeMismatch).to_string(),
            "TypeMismatchError"
        );
        let lex = LexErrorAt {
            error: LexError::EmptyKey,
            line: 3,
        };
        assert_eq!(lex.to_string(), "EMPTY_KEY_ERROR (line 3)");
        // A pipeline error renders exactly as the error it wraps.
        assert_eq!(PipelineError::from(lex).to_string(), lex.to_string());
        assert_eq!(PipelineError::from(at.clone()).to_string(), at.to_string());
    }

    /// The reason for the `std::error::Error` impls: `?` into a
    /// boxed error, and a `source()` chain down to the spec error.
    #[test]
    fn errors_interoperate_as_std_errors() {
        fn boxed() -> Result<(), Box<dyn std::error::Error>> {
            Err(PipelineError::app(AppError::TypeMismatch))?;
            Ok(())
        }
        let e = boxed().unwrap_err();
        assert_eq!(e.to_string(), "TypeMismatchError");
        let src = std::error::Error::source(&*e).expect("App wraps AppErrorAt");
        assert_eq!(src.to_string(), "TypeMismatchError");
        assert!(std::error::Error::source(src).is_some()); // → AppError
    }

    #[test]
    fn name_reports_the_spec_error_behind_a_pipeline_failure() {
        assert_eq!(
            PipelineError::app(AppError::UnitConversion).name(),
            Some("UnitConversionError")
        );
        assert_eq!(PipelineError::from(LexError::Bom).name(), Some("BOM_ERROR"));
        assert_eq!(PipelineError::Other("internal".into()).name(), None);
    }
}
