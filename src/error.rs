//! Error types and error reporting.
//!
//! Define error types for different phases of the execution, together with functions to generate a
//! [codespan](https://crates.io/crates/codespan-reporting) diagnostic from them.
use std::fmt::Write;

use codespan::{FileId, Files};
use codespan_reporting::diagnostic::{Diagnostic, Label, LabelStyle};
use lalrpop_util::ErrorRecovery;

use crate::{
    eval::callstack::CallStack,
    identifier::Ident,
    label,
    label::ty_path,
    parser,
    parser::{
        error::{LexicalError, ParseError as InternalParseError},
        lexer::Token,
        utils::mk_span,
    },
    position::{RawSpan, TermPos},
    repl,
    serialize::ExportFormat,
    term::RichTerm,
    types::Types,
};

/// A general error occurring during either parsing or evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    EvalError(EvalError),
    TypecheckError(TypecheckError),
    ParseErrors(ParseErrors),
    ImportError(ImportError),
    SerializationError(SerializationError),
    IOError(IOError),
    ReplError(ReplError),
}

/// An error occurring during evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum EvalError {
    /// A blame occurred: a contract have been broken somewhere.
    BlameError(label::Label, CallStack),
    /// A field required by a record contract is missing a definition.
    MissingFieldDef(Option<label::Label>, CallStack),
    /// Mismatch between the expected type and the actual type of an expression.
    TypeError(
        /* expected type */ String,
        /* operation */ String,
        /* position of the original unevaluated expression */ TermPos,
        /* evaluated expression */ RichTerm,
    ),
    /// A term which is not a function has been applied to an argument.
    NotAFunc(
        /* term */ RichTerm,
        /* arg */ RichTerm,
        /* app position */ TermPos,
    ),
    /// A field access, or another record operation requiring the existence of a specific field,
    /// has been performed on a record missing that field.
    FieldMissing(
        /* field identifier */ String,
        /* operator */ String,
        RichTerm,
        TermPos,
    ),
    /// Too few arguments were provided to a builtin function.
    NotEnoughArgs(
        /* required arg count */ usize,
        /* primitive */ String,
        TermPos,
    ),
    /// Attempted to merge incompatible values: for example, tried to merge two distinct default
    /// values into one record field.
    MergeIncompatibleArgs(
        /* left operand */ RichTerm,
        /* right operand */ RichTerm,
        /* original merge */ TermPos,
    ),
    /// An unbound identifier was referenced.
    UnboundIdentifier(Ident, TermPos),
    /// A thunk was entered during its own update.
    InfiniteRecursion(CallStack, TermPos),
    /// A serialization error occurred during a call to the builtin `serialize`.
    SerializationError(SerializationError),
    /// A parse error occurred during a call to the builtin `deserialize`.
    DeserializationError(
        String,  /* format */
        String,  /* error message */
        TermPos, /* position of the call to deserialize */
    ),
    /// An unexpected internal error.
    InternalError(String, TermPos),
    /// Errors occurring rarely enough to not deserve a dedicated variant.
    Other(String, TermPos),
}

/// An error occurring during the static typechecking phase.
#[derive(Debug, PartialEq, Clone)]
pub enum TypecheckError {
    /// An unbound identifier was referenced.
    UnboundIdentifier(Ident, TermPos),
    /// An ill-formed type, such as a non-row type appearing in a row.
    IllformedType(Types),
    /// A specific row was expected to be in the type of an expression, but was not.
    MissingRow(
        Ident,
        /* the expected type */ Types,
        /* the inferred/annotated type */ Types,
        TermPos,
    ),
    /// A dynamic tail was expected to be in the type of an expression, but was not.
    MissingDynTail(
        /* the expected type */ Types,
        /* the inferred/annotated type */ Types,
        TermPos,
    ),
    /// A specific row was not expected to be in the type of an expression.
    ExtraRow(
        Ident,
        /* the expected type */ Types,
        /* the inferred/annotated type */ Types,
        TermPos,
    ),
    /// A additional dynamic tail was not expected to be in the type of an expression.
    ExtraDynTail(
        /* the expected type */ Types,
        /* the inferred/annotated type */ Types,
        TermPos,
    ),

    /// An unbound type variable was referenced.
    UnboundTypeVariable(Ident, TermPos),
    /// The actual (inferred or annotated) type of an expression is incompatible with its expected
    /// type.
    TypeMismatch(
        /* the expected type */ Types,
        /* the actual type */ Types,
        TermPos,
    ),
    /// Two incompatible kind (enum vs record) have been deduced for the same identifier of a row type.
    RowKindMismatch(
        Ident,
        /* the expected type */ Option<Types>,
        /* the actual type */ Option<Types>,
        TermPos,
    ),
    /// Two incompatible types have been deduced for the same identifier in a row type.
    RowMismatch(
        Ident,
        /* the expected row type (whole) */ Types,
        /* the actual row type (whole) */ Types,
        /* error at the given row */ Box<TypecheckError>,
        TermPos,
    ),
    /// Two incompatible types have been deduced for the same identifier of a row type.
    ///
    /// This is similar to `RowKindMismatch` but occurs in a slightly different situation. Consider a a
    /// unification variable `t`, which is a placeholder to be filled by a concrete type later in
    /// the typechecking phase.  If `t` appears as the tail of a row type, i.e. the type of some
    /// expression is inferred to be `{ field: Type | t}`, then `t` must not be unified later with
    /// a type including a different declaration for field, such as `field: Type2`.
    ///
    /// A [constraint](../typecheck/type.RowConstr.html) is added accordingly, and if this
    /// constraint is violated (that is if `t` does end up being unified with a type of the form
    /// `{ .., field: Type2, .. }`), `RowConflict` is raised.  We do not have access to the
    /// original `field: Type` declaration, as opposed to `RowKindMismatch`, which corresponds to the
    /// direct failure to unify `{ .. , x: T1, .. }` and `{ .., x: T2, .. }`.
    RowConflict(
        Ident,
        /* the second type assignment which violates the constraint */ Option<Types>,
        /* the expected type of the subexpression */ Types,
        /* the actual type of the subexpression */ Types,
        TermPos,
    ),
    /// Type mismatch on a subtype of an an arrow type.
    ///
    /// The unification of two arrow types requires the unification of the domain and the codomain
    /// (and recursively so, if they are themselves arrow types). When the unification of a subtype
    /// fails, we want to report which part of the arrow types is problematic, and why, rather than
    /// a generic `TypeMismatch`. Indeed, failing to unify two arrow types is a common type error
    /// which deserves a good reporting, that can be caused e.g. by applying a function to an
    /// argument of a wrong type in some cases:
    ///
    /// ```text
    /// let id_mono = fun x => x in let _ign = id_mono true in id_mono 0 : Num
    /// ```
    ///
    /// This specific error stores additionally the [type path](../label/ty_path/index.html) that
    /// identifies the subtype where unification failed and the corresponding error.
    ArrowTypeMismatch(
        /* the expected arrow type */ Types,
        /* the actual arrow type */ Types,
        /* the path to the incompatible subtypes */ ty_path::Path,
        /* the error on the subtype unification */ Box<TypecheckError>,
        TermPos,
    ),
}

#[derive(Debug, PartialEq, Clone, Default)]
pub struct ParseErrors {
    pub errors: Vec<ParseError>,
}

impl ParseErrors {
    pub fn new(errors: Vec<ParseError>) -> ParseErrors {
        ParseErrors { errors }
    }

    pub fn errors(self) -> Option<Vec<ParseError>> {
        if self.errors.is_empty() {
            None
        } else {
            Some(self.errors)
        }
    }

    pub fn no_errors(&self) -> bool {
        self.errors.is_empty()
    }

    pub const fn none() -> ParseErrors {
        ParseErrors { errors: Vec::new() }
    }

    pub fn from_recoverable<'a>(
        errs: Vec<ErrorRecovery<usize, Token<'a>, parser::error::ParseError>>,
        file_id: FileId,
    ) -> Self {
        ParseErrors {
            errors: errs
                .into_iter()
                .map(|e| ParseError::from_lalrpop(e.error, file_id))
                .collect(),
        }
    }
}

impl From<ParseError> for ParseErrors {
    fn from(e: ParseError) -> ParseErrors {
        ParseErrors { errors: vec![e] }
    }
}

impl From<Vec<ParseError>> for ParseErrors {
    fn from(errors: Vec<ParseError>) -> ParseErrors {
        ParseErrors { errors }
    }
}

impl ToDiagnostic<FileId> for ParseErrors {
    fn to_diagnostic(
        &self,
        files: &mut Files<String>,
        contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>> {
        self.errors
            .iter()
            .map(|e| e.to_diagnostic(files, contract_id))
            .flatten()
            .collect()
    }
}

/// An error occurring during parsing.
#[derive(Debug, PartialEq, Clone)]
pub enum ParseError {
    /// Unexpected end of file.
    UnexpectedEOF(FileId, /* tokens expected by the parser */ Vec<String>),
    /// Unexpected token.
    UnexpectedToken(
        RawSpan,
        /* tokens expected by the parser */ Vec<String>,
    ),
    /// Superfluous, unexpected token.
    ExtraToken(RawSpan),
    /// A closing brace '}' does not match an opening brace '{'. This rather precise error is detected by the because
    /// of how interpolated strings are lexed.
    UnmatchedCloseBrace(RawSpan),
    /// Invalid escape sequence in a string literal.
    InvalidEscapeSequence(RawSpan),
    /// Invalid ASCII escape code in a string literal.
    InvalidAsciiEscapeCode(RawSpan),
    /// Error when parsing an external format such as JSON, YAML, etc.
    ExternalFormatError(
        String, /* format */
        String, /* error message */
        Option<RawSpan>,
    ),
    /// Unbound type variable
    UnboundTypeVariables(Vec<Ident>, RawSpan),
    /// Illegal record literal in the uniterm syntax. In practice, this is a record with a
    /// polymorphic tail that contains a construct that wasn't permitted inside a record type in
    /// the original syntax. Typically, a field assignment:
    ///
    /// ```nickel
    /// forall a. {foo : Num; a} # allowed
    /// forall a. {foo : Num = 1; a} # InvalidUniRecord error: giving a value to foo is forbidden
    /// ```
    ///
    /// See [RFC002](../../rfcs/002-merge-types-terms-syntax.md) for more details.
    InvalidUniRecord(
        RawSpan, /* illegal (in conjunction with a tail) construct position */
        RawSpan, /* tail position */
        RawSpan, /* whole record position */
    ),
}

/// An error occurring during the resolution of an import.
#[derive(Debug, PartialEq, Clone)]
pub enum ImportError {
    /// An IO error occurred during an import.
    IOError(
        /* imported file */ String,
        /* error message */ String,
        /* import position */ TermPos,
    ),
    /// A parse error occurred during an import.
    ParseErrors(
        /* error */ ParseErrors,
        /* import position */ TermPos,
    ),
}

/// An error occurred during serialization.
#[derive(Debug, PartialEq, Clone)]
pub enum SerializationError {
    /// Encountered a null value for a format that doesn't support them.
    UnsupportedNull(ExportFormat, RichTerm),
    /// Tried exporting something else than a `Str` to raw format.
    NotAString(RichTerm),
    /// A term contains constructs that cannot be serialized.
    NonSerializable(RichTerm),
    Other(String),
}

/// A general I/O error, occurring when reading a source file or writing an export.
#[derive(Debug, PartialEq, Clone)]
pub struct IOError(pub String);

/// An error occurring during an REPL session.
#[derive(Debug, PartialEq, Clone)]
pub enum ReplError {
    UnknownCommand(String),
    MissingArg {
        cmd: repl::command::CommandType,
        msg_opt: Option<String>,
    },
}

impl From<EvalError> for Error {
    fn from(error: EvalError) -> Error {
        Error::EvalError(error)
    }
}

impl From<ParseError> for Error {
    fn from(error: ParseError) -> Error {
        Error::ParseErrors(ParseErrors {
            errors: vec![error],
        })
    }
}

impl From<ParseErrors> for Error {
    fn from(errors: ParseErrors) -> Error {
        Error::ParseErrors(errors)
    }
}

impl From<TypecheckError> for Error {
    fn from(error: TypecheckError) -> Error {
        Error::TypecheckError(error)
    }
}

impl From<ImportError> for Error {
    fn from(error: ImportError) -> Error {
        Error::ImportError(error)
    }
}

impl From<SerializationError> for Error {
    fn from(error: SerializationError) -> Error {
        Error::SerializationError(error)
    }
}

impl From<IOError> for Error {
    fn from(error: IOError) -> Error {
        Error::IOError(error)
    }
}

impl From<std::io::Error> for IOError {
    fn from(error: std::io::Error) -> IOError {
        IOError(error.to_string())
    }
}

impl From<SerializationError> for EvalError {
    fn from(error: SerializationError) -> EvalError {
        EvalError::SerializationError(error)
    }
}

/// Return an escaped version of a string. Used to sanitize strings before inclusion in error
/// messages, which can contain ASCII code sequences, and in particular ANSI escape codes, that
/// could alter Nickel's error messages.
pub fn escape(s: &str) -> String {
    String::from_utf8(
        s.bytes()
            .flat_map(std::ascii::escape_default)
            .collect::<Vec<u8>>(),
    )
    .expect("escape(): converting from a string should give back a valid UTF8 string")
}

impl From<ReplError> for Error {
    fn from(error: ReplError) -> Error {
        Error::ReplError(error)
    }
}

impl ParseError {
    pub fn from_lalrpop<T>(
        error: lalrpop_util::ParseError<usize, T, InternalParseError>,
        file_id: FileId,
    ) -> ParseError {
        match error {
            lalrpop_util::ParseError::InvalidToken { location } => {
                ParseError::UnexpectedToken(mk_span(file_id, location, location + 1), Vec::new())
            }
            lalrpop_util::ParseError::UnrecognizedToken {
                token: (start, _, end),
                expected,
            } => ParseError::UnexpectedToken(mk_span(file_id, start, end), expected),
            lalrpop_util::ParseError::UnrecognizedEOF { expected, .. } => {
                ParseError::UnexpectedEOF(file_id, expected)
            }
            lalrpop_util::ParseError::ExtraToken {
                token: (start, _, end),
            } => ParseError::ExtraToken(mk_span(file_id, start, end)),
            lalrpop_util::ParseError::User { error } => match error {
                InternalParseError::Lexical(LexicalError::Generic(start, end)) => {
                    ParseError::UnexpectedToken(mk_span(file_id, start, end), Vec::new())
                }
                InternalParseError::Lexical(LexicalError::UnmatchedCloseBrace(location)) => {
                    ParseError::UnmatchedCloseBrace(mk_span(file_id, location, location + 1))
                }
                InternalParseError::Lexical(LexicalError::InvalidEscapeSequence(location)) => {
                    ParseError::InvalidEscapeSequence(mk_span(file_id, location, location + 1))
                }
                InternalParseError::Lexical(LexicalError::InvalidAsciiEscapeCode(location)) => {
                    ParseError::InvalidAsciiEscapeCode(mk_span(file_id, location, location + 2))
                }
                InternalParseError::UnboundTypeVariables(idents, span) => {
                    ParseError::UnboundTypeVariables(idents, span)
                }
                InternalParseError::InvalidUniRecord(illegal_pos, tail_pos, pos) => {
                    ParseError::InvalidUniRecord(illegal_pos, tail_pos, pos)
                }
            },
        }
    }

    pub fn from_serde_json(
        error: serde_json::Error,
        file_id: FileId,
        files: &Files<String>,
    ) -> Self {
        use codespan::ByteOffset;

        // error.line() should start at `1` according to the documentation, but in practice, it may
        // be 0 for the error `json parse error: data did not match any variant of untagged enum
        // Term`. Although this error should not happen, if it does, it's better to get a message
        // than a panic message `subtract with overflow`.
        let line_span = if error.line() == 0 {
            None
        } else {
            files.line_span(file_id, (error.line() - 1) as u32).ok()
        };

        let start = line_span.map(|ls| ls.start() + ByteOffset::from(error.column() as i64 - 1));
        ParseError::ExternalFormatError(
            String::from("json"),
            error.to_string(),
            start.map(|start| RawSpan {
                src_id: file_id,
                start,
                end: start + ByteOffset::from(1),
            }),
        )
    }

    pub fn from_serde_yaml(error: serde_yaml::Error, file_id: FileId) -> Self {
        use codespan::{ByteIndex, ByteOffset};

        let start = error
            .location()
            .map(|loc| loc.index() as u32)
            .map(ByteIndex::from);
        ParseError::ExternalFormatError(
            String::from("yaml"),
            error.to_string(),
            start.map(|start| RawSpan {
                src_id: file_id,
                start,
                end: start + ByteOffset::from(1),
            }),
        )
    }

    pub fn from_toml(error: toml::de::Error, file_id: FileId, files: &Files<String>) -> Self {
        use codespan::ByteOffset;

        let start = error.line_col().and_then(|(line, col)| {
            Some(files.line_span(file_id, line as u32).ok()?.start() + ByteOffset::from(col as i64))
        });
        ParseError::ExternalFormatError(
            String::from("toml"),
            error.to_string(),
            start.map(|start| RawSpan {
                src_id: file_id,
                start,
                end: start + ByteOffset::from(1),
            }),
        )
    }
}

pub const INTERNAL_ERROR_MSG: &str =
    "This error should not happen. This is likely a bug in the Nickel interpreter. Please consider\
 reporting it at https://github.com/tweag/nickel/issues with the above error message.";

/// A trait for converting an error to a diagnostic.
pub trait ToDiagnostic<FileId> {
    /// Convert an error to a list of printable formatted diagnostic.
    ///
    /// # Arguments
    ///
    /// - `files`: to know why it takes a mutable reference to `Files<String>`, see
    ///   [`label_alt`](fn.label_alt.html).
    /// - `contract_id` is required to format the callstack when reporting blame errors. For some
    ///   errors (such as [`ParseError`](./enum.ParseError.html)), contracts may not have been loaded
    ///   yet, hence the optional. See also [`process_callstack`](fn.process_callstack.html).
    ///
    /// # Return
    ///
    /// Return a list of diagnostics. Most errors generate only one, but showing the callstack
    /// ordered requires to sidestep a limitation of codespan. The current solution is to generate
    /// one diagnostic per callstack element. See [this
    /// issue](https://github.com/brendanzab/codespan/issues/285).
    fn to_diagnostic(
        &self,
        files: &mut Files<String>,
        contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>>;
}

// Helpers for the creation of codespan `Label`s

/// Create a primary label from a span.
fn primary(span: &RawSpan) -> Label<FileId> {
    Label::primary(span.src_id, span.start.to_usize()..span.end.to_usize())
}

/// Create a secondary label from a span.
fn secondary(span: &RawSpan) -> Label<FileId> {
    Label::secondary(span.src_id, span.start.to_usize()..span.end.to_usize())
}

/// Create a label from an optional span, or fallback to annotating the alternative snippet
/// `alt_term` if the span is `None`.
///
/// When `span_opt` is `None`, the code snippet `alt_term` is added to `files` under a special
/// name and is referred to instead.
///
/// This is useful because during evaluation, some terms are the results of computations. They
/// correspond to nothing in the original source, and thus have a position set to `None`(e.g. the
/// result of `let x = 1 + 1 in x`).  In such cases it may still be valuable to print the term (or
/// a terse representation) in the error diagnostic rather than nothing, because if you have let `x
/// = 1 + 1 in` and then 100 lines later, `x arg` - causing a `NotAFunc` error - it may be helpful
/// to know that `x` holds the value `2`.
///
/// For example, if one wants to report an error on a record, `alt_term` may be defined to `{ ...  }`.
/// Then, if this record has no position (`span_opt` is `None`), the error will be reported as:
///
/// ```text
/// error: some error
///   -- <unknown> (generated by evaluation):1:2
///   |
/// 1 | { ... }
///     ^^^^^^^ some annotation
/// ```
///
/// The reason for the mutable reference to `files` is that codespan do no let you annotate
/// something that is not in `files`: you can't provide a raw snippet, you need to provide a
/// `FileId` referring to a file. This leaves the following possibilities:
///
/// 1. Do nothing: just elude annotations which refer to the term
/// 2. Print the term and the annotation as a note together with the diagnostic. Notes are
///    additional text placed at the end of diagnostic. What you lose:
///     - pretty formatting of annotations for such snippets
///     - style consistency: the style of the error now depends on the term being from the source
///     or a byproduct of evaluation
/// 3. Add the term to files, take 1: pass a reference to files so that the code building the
///    diagnostic can itself add arbitrary snippets if necessary, and get back their `FileId`. This
///    is what is done here.
/// 4. Add the term to files, take 2: make a wrapper around the `Files` and `FileId` structures of
///    codespan which handle source mapping. `FileId` could be something like
///    `Either<codespan::FileId, CustomId = u32>` so that `to_diagnostic` could construct and use these
///    separate ids, and return the corresponding snippets to be added together with the
///    diagnostic without modifying external state. Or even have `FileId = Either<codespan::FileId`,
///    `LoneCode = String or (Id, String)>` so we don't have to return the additional list of
///    snippets. This adds some boilerplate, that we wanted to avoid, but this stays on the
///    reasonable side of being an alternative.
fn label_alt(
    span_opt: Option<RawSpan>,
    alt_term: String,
    style: LabelStyle,
    files: &mut Files<String>,
) -> Label<FileId> {
    match span_opt {
        Some(span) => Label::new(
            style,
            span.src_id,
            span.start.to_usize()..span.end.to_usize(),
        ),
        None => {
            let range = 0..alt_term.len();
            Label::new(
                style,
                files.add("<unknown> (generated by evaluation)", alt_term),
                range,
            )
        }
    }
}

/// Create a secondary label from an optional span, or fallback to annotating the alternative snippet
/// `alt_term` if the span is `None`.
///
/// See [`label_alt`](fn.label_alt.html).
fn primary_alt(
    span_opt: Option<RawSpan>,
    alt_term: String,
    files: &mut Files<String>,
) -> Label<FileId> {
    label_alt(span_opt, alt_term, LabelStyle::Primary, files)
}

/// Create a primary label from a term, or fallback to annotating the shallow representation of this term
/// if its span is `None`.
///
/// See [`label_alt`](fn.label_alt.html).
fn primary_term(term: &RichTerm, files: &mut Files<String>) -> Label<FileId> {
    primary_alt(term.pos.into_opt(), term.as_ref().shallow_repr(), files)
}

/// Create a secondary label from an optional span, or fallback to annotating the alternative snippet
/// `alt_term` if the span is `None`.
///
/// See [`label_alt`](fn.label_alt.html).
fn secondary_alt(span_opt: TermPos, alt_term: String, files: &mut Files<String>) -> Label<FileId> {
    label_alt(span_opt.into_opt(), alt_term, LabelStyle::Secondary, files)
}

/// Create a secondary label from a term, or fallback to annotating the shallow representation of this term
/// if its span is `None`.
///
/// See [`label_alt`](fn.label_alt.html).
fn secondary_term(term: &RichTerm, files: &mut Files<String>) -> Label<FileId> {
    secondary_alt(term.pos, term.as_ref().shallow_repr(), files)
}

/// Generate a codespan label that describes the [type path](../label/enum.TyPath.html) of a
/// (Nickel) label, and notes to hint at the situation that may have caused the corresponding
/// error.
fn report_ty_path(l: &label::Label, files: &mut Files<String>) -> (Label<FileId>, Vec<String>) {
    let end_note = String::from("Note: this is an illustrative example. The actual error may involve deeper nested functions calls.");

    let (msg, notes) = if l.path.is_empty() {
        (String::from("expected type"), Vec::new())
    } else if ty_path::has_no_arrow(&l.path) {
        match l.path.last() {
            Some(ty_path::Elem::Array) => (String::from("expected array element type"), Vec::new()),
            Some(ty_path::Elem::Field(_)) => (String::from("expected field type"), Vec::new()),
            _ => unreachable!(),
        }
    }
    // If the path is only composed of codomains, polarity is necessarily true and the cause of the
    // blame is the return value of the function
    else if ty_path::is_only_codom(&l.path) {
        (
            String::from("expected return type"),
            vec![
                String::from(
                    "This error may happen in the following situation:
1. A function `f` is bound by a contract: e.g. `Bool -> Num`.
2. `f` returns a value of the wrong type: e.g. `f = fun c => \"string\"` while `Num` is expected.",
                ),
                String::from(
                    "Either change the contract accordingly, or change the return value of `f`",
                ),
            ],
        )
    } else {
        // We ignore the `Field` and `Array` elements of the path, since they do not impact
        // polarity, and only consider "higher-order" elements to customize error messages.
        let last = l
            .path
            .iter()
            .filter(|elt| matches!(*elt, ty_path::Elem::Domain | ty_path::Elem::Codomain))
            .last()
            .unwrap();
        match last {
            ty_path::Elem::Domain if l.polarity => {
                (String::from("expected type of an argument of an inner call"),
                 vec![
                     String::from("This error may happen in the following situation:
1. A function `f` is bound by a contract: e.g. `(Str -> Str) -> Str)`.
2. `f` takes another function `g` as an argument: e.g. `f = fun g => g 0`.
3. `f` calls `g` with an argument that does not respect the contract: e.g. `g 0` while `Str -> Str` is expected."),
                     String::from("Either change the contract accordingly, or call `g` with a `Str` argument."),
                     end_note,
                 ])
            }
            ty_path::Elem::Codomain if l.polarity => {
                (String::from("expected return type of a sub-function passed as an argument of an inner call"),
                 vec![
                     String::from("This error may happen in the following situation:
1. A function `f` is bound by a contract: e.g. `((Num -> Num) -> Num) -> Num)`.
2. `f` take another function `g` as an argument: e.g. `f = fun g => g (fun x => true)`.
3. `g` itself takes a function as an argument.
4. `f` passes a function that does not respect the contract to `g`: e.g. `g (fun x => true)` (expected to be of type `Num -> Num`)."),
                     String::from("Either change the contract accordingly, or call `g` with a function that returns a value of type `Num`."),
                     end_note,
                 ])
            }
            ty_path::Elem::Domain => {
                (String::from("expected type of the argument provided by the caller"),
                 vec![
                     String::from("This error may happen in the following situation:
1. A function `f` is bound by a contract: e.g. `Num -> Num`.
2. `f` is called with an argument of the wrong type: e.g. `f false`."),
                     String::from("Either change the contract accordingly, or call `f` with an argument of the right type."),
                     end_note,
                 ])
            }
            ty_path::Elem::Codomain => {
                (String::from("expected return type of a function provided by the caller"),
                 vec![
                     String::from("This error may happen in the following situation:
1. A function `f` is bound by a contract: e.g. `(Num -> Num) -> Num`.
2. `f` takes another function `g` as an argument: e.g. `f = fun g => g 0`.
3. `f` is called by with an argument `g` that does not respect the contract: e.g. `f (fun x => false)`."),
                     String::from("Either change the contract accordingly, or call `f` with a function that returns a value of the right type."),
                     end_note,
                 ])
            }
            _ => panic!(),
        }
    };

    let (start, end) = ty_path::span(l.path.iter().peekable(), &l.types);
    let label = Label::new(
        LabelStyle::Secondary,
        files.add("", format!("{}", l.types)),
        start..end,
    )
    .with_message(msg);
    (label, notes)
}

/// Return a note diagnostic showing where a contract was bound.
fn blame_label_note(l: &label::Label) -> Diagnostic<FileId> {
    Diagnostic::note().with_labels(vec![Label::primary(
        l.span.src_id,
        l.span.start.to_usize()..l.span.end.to_usize(),
    )
    .with_message("bound here")])
}

impl ToDiagnostic<FileId> for Error {
    fn to_diagnostic(
        &self,
        files: &mut Files<String>,
        contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>> {
        match self {
            Error::ParseErrors(errs) => errs
                .errors
                .iter()
                .map(|e| e.to_diagnostic(files, contract_id))
                .flatten()
                .collect(),
            Error::TypecheckError(err) => err.to_diagnostic(files, contract_id),
            Error::EvalError(err) => err.to_diagnostic(files, contract_id),
            Error::ImportError(err) => err.to_diagnostic(files, contract_id),
            Error::SerializationError(err) => err.to_diagnostic(files, contract_id),
            Error::IOError(err) => err.to_diagnostic(files, contract_id),
            Error::ReplError(err) => err.to_diagnostic(files, contract_id),
        }
    }
}

impl ToDiagnostic<FileId> for EvalError {
    fn to_diagnostic(
        &self,
        files: &mut Files<String>,
        contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>> {
        match self {
            EvalError::BlameError(l, call_stack) => {
                let mut msg = String::new();

                // Writing in a string should not raise an error, hence the fearless `unwrap()`
                if ty_path::has_no_arrow(&l.path) {
                    // An empty path or a path that contains only fields necessarily corresponds to
                    // a positive blame
                    assert!(l.polarity);
                    write!(&mut msg, "contract broken by a value").unwrap();
                } else if l.polarity {
                    write!(&mut msg, "contract broken by a function").unwrap();
                } else {
                    write!(&mut msg, "contract broken by the caller").unwrap();
                }

                if !l.tag.is_empty() {
                    write!(&mut msg, ": {}", &escape(&l.tag)).unwrap();
                }

                let (path_label, notes) = report_ty_path(l, files);
                let mut labels = vec![path_label];

                if let Some(ref arg_pos) = l.arg_pos.into_opt() {
                    // In some cases, if the blame error is located in an argument or return value
                    // of an higher order functions for example, the original argument position can
                    // point to the builtin implementation contract like `func` or `record`, so
                    // there's no good reason to show it. Note than even in that case, the
                    // information contained in the argument thunk can still be useful.
                    if contract_id
                        .map(|ctrs_id| arg_pos.src_id != ctrs_id)
                        .unwrap_or(true)
                    {
                        labels.push(primary(arg_pos).with_message("applied to this expression"));
                    }
                }

                // If we have a reference to the thunk that was being tested, we can try to show
                // more information about the final, evaluated value that is responsible for the
                // blame.
                if let Some(ref thunk) = l.arg_thunk {
                    let mut val = thunk.get_owned().body;

                    match (val.pos, l.arg_pos.as_opt_ref(), contract_id) {
                        // Avoid showing a position inside builtin contracts, it's rarely
                        // informative.
                        (TermPos::Original(val_pos), _, Some(c_id)) if val_pos.src_id == c_id => {
                            val.pos = TermPos::None;
                            labels.push(
                                secondary_term(&val, files).with_message("evaluated to this value"),
                            );
                        }
                        // Do not show the same thing twice: if arg_pos and val_pos are the same,
                        // the first label "applied to this value" is sufficient.
                        (TermPos::Original(ref val_pos), Some(arg_pos), _)
                            if val_pos == arg_pos => {}
                        (TermPos::Original(ref val_pos), ..) => labels
                            .push(secondary(val_pos).with_message("evaluated to this expression")),
                        // If the final thunk is a direct reduct of the original value, rather
                        // print the actual value than referring to the same position as
                        // before.
                        (TermPos::Inherited(ref val_pos), Some(arg_pos), _)
                            if val_pos == arg_pos =>
                        {
                            val.pos = TermPos::None;
                            labels.push(
                                secondary_term(&val, files).with_message("evaluated to this value"),
                            );
                        }
                        // Finally, if the parameter reduced to a value which originates from a
                        // different expression, show both the expression and the value.
                        (TermPos::Inherited(ref val_pos), ..) => {
                            labels.push(
                                secondary(val_pos).with_message("evaluated to this expression"),
                            );
                            val.pos = TermPos::None;
                            labels.push(
                                secondary_term(&val, files).with_message("evaluated to this value"),
                            );
                        }
                        (TermPos::None, ..) => labels.push(
                            secondary_term(&val, files).with_message("evaluated to this value"),
                        ),
                    }
                }

                let mut diagnostics = vec![Diagnostic::error()
                    .with_message(msg)
                    .with_labels(labels)
                    .with_notes(notes)];

                diagnostics.push(blame_label_note(&l));

                if ty_path::is_only_codom(&l.path) {
                } else if let Some(id) = contract_id {
                    let (calls, curr_call) = call_stack.group_by_calls(id);
                    let diag_curr_call = curr_call.map(|cdescr| {
                        let name = cdescr
                            .head
                            .map(|ident| ident.to_string())
                            .unwrap_or_else(|| String::from("<func>"));
                        Diagnostic::note().with_labels(vec![primary(&cdescr.span)
                            .with_message(format!("While calling to {}", name))])
                    });
                    let diags = calls.into_iter().enumerate().map(|(i, cdescr)| {
                        let name = cdescr
                            .head
                            .map(|ident| ident.to_string())
                            .unwrap_or_else(|| String::from("<func>"));
                        Diagnostic::note().with_labels(vec![secondary(&cdescr.span)
                            .with_message(format!("({}) calling {}", i + 1, name))])
                    });

                    diagnostics.extend(diag_curr_call);
                    diagnostics.extend(diags);
                }

                diagnostics
            }
            EvalError::MissingFieldDef(label, callstack) => {
                use crate::eval::callstack::StackElem;

                // The following code determines what was the last accessed record field by looking
                // at the call stack. Because of recursive records though, the fields may actually
                // be accessed via a variable:
                //
                // ```
                //  {
                //    foo | Dyn
                //        | doc "Oops, undefined :(",
                //    bar = 1 + foo,
                //  }.bar
                //  ```
                //
                // Here, the missing field doesn't correspond to a field access, but to a variable
                // occurrence `foo`. Thus, we take the last non-generated identifier accessed
                // (either variable or field) as the name of the missing field.
                let mut field: Option<String> = None;
                let mut pos_record = TermPos::None;
                let mut pos_access: Option<TermPos> = None;

                for elt in callstack.as_ref().iter().rev() {
                    match elt {
                        StackElem::Var { id, pos, .. } if !id.is_generated() && field.is_none() => {
                            field = Some(id.to_string());
                            pos_access = Some(*pos);
                        }
                        StackElem::Field {
                            id,
                            pos_record: pos_rec,
                            pos_access: pos_acc,
                            ..
                        } => {
                            field.get_or_insert(id.to_string());
                            pos_access.get_or_insert(*pos_acc);
                            pos_record = *pos_rec;
                            break;
                        }
                        _ => (),
                    }
                }

                let mut labels = vec![];

                if let Some(span) = pos_record.into_opt() {
                    labels.push(primary(&span).with_message("in this record"));
                }

                if let Some(span) = pos_access.map(TermPos::into_opt).flatten() {
                    labels.push(secondary(&span).with_message("accessed here"));
                }

                let mut diags = vec![Diagnostic::error()
                    .with_message(format!(
                        "missing definition for `{}`",
                        field.unwrap_or(String::from("?"))
                    ))
                    .with_labels(labels)
                    .with_notes(vec![])];

                if let Some(label) = label {
                    diags.push(blame_label_note(label));
                }

                diags
            }
            EvalError::TypeError(expd, msg, orig_pos_opt, t) => {
                let label = format!(
                    "this expression has type {}, but {} was expected",
                    t.term
                        .type_of()
                        .unwrap_or_else(|| String::from("<unevaluated>")),
                    expd,
                );

                let labels = match orig_pos_opt {
                    TermPos::Original(pos) | TermPos::Inherited(pos) if orig_pos_opt != &t.pos => {
                        vec![
                            primary(pos).with_message(label),
                            secondary_term(t, files).with_message("evaluated to this"),
                        ]
                    }
                    _ => vec![primary_term(t, files).with_message(label)],
                };

                vec![Diagnostic::error()
                    .with_message("type error")
                    .with_labels(labels)
                    .with_notes(vec![msg.clone()])]
            }
            EvalError::NotAFunc(t, arg, pos_opt) => vec![Diagnostic::error()
                .with_message("not a function")
                .with_labels(vec![
                    primary_term(t, files)
                        .with_message("this term is applied, but it is not a function"),
                    secondary_alt(
                        *pos_opt,
                        format!(
                            "({}) ({})",
                            (*t.term).shallow_repr(),
                            (*arg.term).shallow_repr()
                        ),
                        files,
                    )
                    .with_message("applied here"),
                ])],
            EvalError::FieldMissing(field, op, t, span_opt) => {
                let mut labels = Vec::new();
                let mut notes = Vec::new();
                let field = escape(field);

                if let Some(span) = span_opt.into_opt() {
                    labels.push(
                        Label::primary(span.src_id, span.start.to_usize()..span.end.to_usize())
                            .with_message(format!("this requires field {} to exist", field)),
                    );
                } else {
                    notes.push(format!(
                        "field {} was required by the operator {}",
                        field, op
                    ));
                }

                if let Some(span) = t.pos.as_opt_ref() {
                    labels.push(
                        secondary(span).with_message(format!("field {} is missing here", field)),
                    );
                }

                vec![Diagnostic::error()
                    .with_message("missing field")
                    .with_labels(labels)]
            }
            EvalError::NotEnoughArgs(count, op, span_opt) => {
                let mut labels = Vec::new();
                let mut notes = Vec::new();
                let msg = format!(
                    "{} expects {} arguments, but not enough were provided",
                    op, count
                );

                if let Some(span) = span_opt.into_opt() {
                    labels.push(
                        Label::primary(span.src_id, span.start.to_usize()..span.end.to_usize())
                            .with_message(msg),
                    );
                } else {
                    notes.push(msg);
                }

                vec![Diagnostic::error()
                    .with_message("not enough arguments")
                    .with_labels(labels)
                    .with_notes(notes)]
            }
            EvalError::MergeIncompatibleArgs(t1, t2, span_opt) => {
                let mut labels = vec![
                    primary_term(t1, files).with_message("cannot merge this expression"),
                    primary_term(t2, files).with_message("with this expression"),
                ];

                if let TermPos::Original(span) | TermPos::Inherited(span) = span_opt {
                    labels.push(secondary(span).with_message("merged here"));
                }

                vec![Diagnostic::error()
                    .with_message("non mergeable terms")
                    .with_labels(labels)]
            }
            EvalError::UnboundIdentifier(ident, span_opt) => vec![Diagnostic::error()
                .with_message("unbound identifier")
                .with_labels(vec![primary_alt(
                    span_opt.into_opt(),
                    ident.to_string(),
                    files,
                )
                .with_message("this identifier is unbound")])],
            EvalError::InfiniteRecursion(_call_stack, span_opt) => {
                let labels = span_opt
                    .as_opt_ref()
                    .map(|span| vec![primary(span).with_message("recursive reference")])
                    .unwrap_or_default();

                vec![Diagnostic::error()
                    .with_message("infinite recursion")
                    .with_labels(labels)]
            }
            EvalError::Other(msg, span_opt) => {
                let labels = span_opt
                    .as_opt_ref()
                    .map(|span| vec![primary(span).with_message("here")])
                    .unwrap_or_default();

                vec![Diagnostic::error().with_message(msg).with_labels(labels)]
            }
            EvalError::InternalError(msg, span_opt) => {
                let labels = span_opt
                    .as_opt_ref()
                    .map(|span| vec![primary(span).with_message("here")])
                    .unwrap_or_default();

                vec![Diagnostic::error()
                    .with_message(format!("internal error: {}", msg))
                    .with_labels(labels)
                    .with_notes(vec![String::from(INTERNAL_ERROR_MSG)])]
            }
            EvalError::SerializationError(err) => err.to_diagnostic(files, contract_id),
            EvalError::DeserializationError(format, msg, span_opt) => {
                let labels = span_opt
                    .as_opt_ref()
                    .map(|span| vec![primary(span).with_message("here")])
                    .unwrap_or_default();

                vec![Diagnostic::error()
                    .with_message(format!("{} parse error: {}", format, msg))
                    .with_labels(labels)
                    .with_notes(vec![String::from(INTERNAL_ERROR_MSG)])]
            }
        }
    }
}

impl ToDiagnostic<FileId> for ParseError {
    fn to_diagnostic(
        &self,
        files: &mut Files<String>,
        _contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>> {
        let diagnostic = match self {
            ParseError::UnexpectedEOF(file_id, _expected) => {
                let end = files.source_span(*file_id).end();
                Diagnostic::error()
                    .with_message(format!(
                        "unexpected end of file when parsing {}",
                        files.name(*file_id).to_string_lossy()
                    ))
                    .with_labels(vec![primary(&RawSpan {
                        start: end,
                        end,
                        src_id: *file_id,
                    })])
            }
            ParseError::UnexpectedToken(span, _expected) => Diagnostic::error()
                .with_message("unexpected token")
                .with_labels(vec![primary(span)]),
            ParseError::ExtraToken(span) => Diagnostic::error()
                .with_message("superfluous unexpected token")
                .with_labels(vec![primary(span)]),
            ParseError::UnmatchedCloseBrace(span) => Diagnostic::error()
                .with_message("unmatched closing brace \'}\'")
                .with_labels(vec![primary(span)]),
            ParseError::InvalidEscapeSequence(span) => Diagnostic::error()
                .with_message("invalid escape sequence")
                .with_labels(vec![primary(span)]),
            ParseError::InvalidAsciiEscapeCode(span) => Diagnostic::error()
                .with_message("invalid ascii escape code")
                .with_labels(vec![primary(span)]),
            ParseError::ExternalFormatError(format, msg, span_opt) => {
                let labels = span_opt
                    .as_ref()
                    .map(|span| vec![primary(span)])
                    .unwrap_or_default();

                Diagnostic::error()
                    .with_message(format!("{} parse error: {}", format, msg))
                    .with_labels(labels)
            }
            ParseError::UnboundTypeVariables(idents, span) => Diagnostic::error()
                .with_message(format!(
                    "unbound type variable(s): {}",
                    idents
                        .iter()
                        .map(|x| format!("`{}`", x))
                        .collect::<Vec<_>>()
                        .join(",")
                ))
                .with_labels(vec![primary(span)]),
            ParseError::InvalidUniRecord(illegal_span, tail_span, span) => Diagnostic::error()
                .with_message(format!("invalid record literal"))
                .with_labels(vec![
                    primary(span),
                    secondary(illegal_span).with_message("can't use this record construct"),
                    secondary(tail_span).with_message("in presence of a tail"),
                ])
                .with_notes(vec![
                    String::from("Using a polymorphic tail in a record `{ ..; a}` requires the rest of the record to be only composed of type annotations, of the form `<field>: <type>`."),
                    String::from("Value assignements, such as `<field> = <expr>`, metadata, etc. are forbidden."),
                ]),
        };

        vec![diagnostic]
    }
}

impl ToDiagnostic<FileId> for TypecheckError {
    fn to_diagnostic(
        &self,
        files: &mut Files<String>,
        contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>> {
        fn mk_expr_label(span_opt: &TermPos) -> Vec<Label<FileId>> {
            span_opt
                .as_opt_ref()
                .map(|span| vec![primary(span).with_message("this expression")])
                .unwrap_or_default()
        }

        match self {
            TypecheckError::UnboundIdentifier(ident, pos_opt) =>
            // Use the same diagnostic as `EvalError::UnboundIdentifier` for consistency.
                {
                    EvalError::UnboundIdentifier(ident.clone(), *pos_opt)
                        .to_diagnostic(files, contract_id)
                }
            TypecheckError::IllformedType(ty) => {
                let ty_fmted = format!("{}", ty);
                let len = ty_fmted.len();

                let label = Label::new(LabelStyle::Secondary, files.add("", ty_fmted), 0..len)
                    .with_message("ill-formed type");

                vec![Diagnostic::error()
                    .with_message("ill-formed type")
                    .with_labels(vec![label])]
            }
            TypecheckError::MissingRow(ident, expd, actual, span_opt) =>
                vec![Diagnostic::error()
                    .with_message(format!("type error: missing row `{}`", ident))
                    .with_labels(mk_expr_label(span_opt))
                    .with_notes(vec![
                        format!("The type of the expression was expected to be `{}` which contains the field `{}`", expd, ident),
                        format!("The type of the expression was inferred to be `{}`, which does not contain the field `{}`", actual, ident),
                    ])]
            ,
            TypecheckError::MissingDynTail(expd, actual, span_opt) =>
                vec![Diagnostic::error()
                    .with_message(String::from("type error: missing dynamic tail `| Dyn`"))
                    .with_labels(mk_expr_label(span_opt))
                    .with_notes(vec![
                        format!("The type of the expression was expected to be `{}` which contains the tail `| Dyn`", expd),
                        format!("The type of the expression was inferred to be `{}`, which does not contain the tail `| Dyn`", actual),
                    ])]
            ,

            TypecheckError::ExtraRow(ident, expd, actual, span_opt) =>
                vec![Diagnostic::error()
                    .with_message(format!("type error: extra row `{}`", ident))
                    .with_labels(mk_expr_label(span_opt))
                    .with_notes(vec![
                        format!("The type of the expression was expected to be `{}`, which does not contain the field `{}`", expd, ident),
                        format!("The type of the expression was inferred to be `{}`, which contains the extra field `{}`", actual, ident),
                    ])]
            ,
            TypecheckError::ExtraDynTail(expd, actual, span_opt) =>
                vec![Diagnostic::error()
                    .with_message(String::from("type error: extra dynamic tail `| Dyn`"))
                    .with_labels(mk_expr_label(span_opt))
                    .with_notes(vec![
                        format!("The type of the expression was expected to be `{}`, which does not contain the tail `| Dyn`", expd),
                        format!("The type of the expression was inferred to be `{}`, which contains the extra tail `| Dyn`", actual),
                    ])]
            ,

            TypecheckError::UnboundTypeVariable(ident, span_opt) =>
                vec![Diagnostic::error()
                    .with_message(String::from("unbound type variable"))
                    .with_labels(vec![primary_alt(span_opt.into_opt(), ident.to_string(), files).with_message("this type variable is unbound")])
                    .with_notes(vec![
                        format!("Maybe you forgot to put a `forall {}.` somewhere in the enclosing type ?", ident),
                    ])]
            ,
            TypecheckError::TypeMismatch(expd, actual, span_opt) =>
                vec![
                    Diagnostic::error()
                        .with_message("incompatible types")
                        .with_labels(mk_expr_label(span_opt))
                        .with_notes(vec![
                            format!("The type of the expression was expected to be `{}`", expd),
                            format!("The type of the expression was inferred to be `{}`", actual),
                            String::from("These types are not compatible"),
                        ])]
            ,
            TypecheckError::RowKindMismatch(ident, expd, actual, span_opt) => {
                let (expd_str, actual_str) = match (expd, actual) {
                    (Some(_), None) => ("an enum type", "a record type"),
                    (None, Some(_)) => ("a record type", "an enum type"),
                    _ => panic!("error::to_diagnostic()::RowKindMismatch: unexpected configuration for `expd` and `actual`"),
                };

                vec![
                    Diagnostic::error()
                        .with_message("incompatible row kinds")
                        .with_labels(mk_expr_label(span_opt))
                        .with_notes(vec![
                            format!("The row type of `{}` was expected to be `{}`, but was inferred to be `{}`", ident, expd_str, actual_str),
                            String::from("Enum row types and record row types are not compatible"),
                        ])]
            }
            TypecheckError::RowMismatch(ident, expd, actual, err_, span_opt) => {
                // If the unification error is on a nested field, we will have a succession of
                // `RowMismatch` errors wrapping the underlying error. In this case, instead of
                // showing a cascade of similar error messages, we determine the full path of the
                // nested field (e.g. `pkg.subpkg1.meta.url`) and only show once the row mismatch
                // error followed by the underlying error.
                let mut err = (*err_).clone();
                let mut path = vec![ident.clone()];

                while let TypecheckError::RowMismatch(id_next, _, _, next, _) = *err {
                    path.push(id_next);
                    err = next;
                }

                let path_str: Vec<String> = path.clone().into_iter().map(|ident| format!("{}", ident)).collect();
                let field = path_str.join(".");

                let note1 = match expd.row_find_path(path.as_slice()) {
                    Some(ty) => format!("The type of the expression was expected to have the row `{}: {}`", field, ty),
                    None => format!("The type of the expression was expected to be `{}`", expd)
                };

                let note2 = match actual.row_find_path(path.as_slice()) {
                    Some(ty) => format!("The type of the expression was inferred to have the row `{}: {}`", field, ty),
                    None => format!("The type of the expression was inferred to be `{}`", actual)
                };

                let mut diags = vec![Diagnostic::error()
                    .with_message("incompatible rows declaration")
                    .with_labels(mk_expr_label(span_opt))
                    .with_notes(vec![
                        note1,
                        note2,
                        format!("Could not match the two declaration of `{}`", field),
                    ])
                ];

                // We generate a diagnostic for the underlying error, but append a prefix to the
                // error message to make it clear that this is not a separated error but a more
                // precise description of why the unification of a row failed.
                diags.extend((*err).to_diagnostic(files, contract_id).into_iter()
                    .map(|mut diag| {
                        diag.message = format!("While typing field `{}`: {}", field, diag.message);
                        diag
                    }));
                diags
            }
            TypecheckError::RowConflict(ident, conflict, _expd, _actual, span_opt) => {
                vec![
                    Diagnostic::error()
                        .with_message("multiple rows declaration")
                        .with_labels(mk_expr_label(span_opt))
                        .with_notes(vec![
                            format!("The type of the expression was inferred to have the row `{}: {}`", ident, conflict.as_ref().cloned().unwrap()),
                            format!("But this type appears inside another row type, which already has a declaration for the field `{}`", ident),
                            String::from("A type cannot have two conflicting declaration for the same row"),
                        ])]
            }
            TypecheckError::ArrowTypeMismatch(expd, actual, path, err, span_opt) => {
                let (expd_start, expd_end) = ty_path::span(path.iter().peekable(), expd);
                let (actual_start, actual_end) = ty_path::span(path.iter().peekable(), actual);

                let mut labels = vec![
                    Label::secondary(
                        files.add("", format!("{}", expd)),
                        expd_start..expd_end,
                    )
                        .with_message("this part of the expected type"),
                    Label::secondary(
                        files.add("", format!("{}", actual)),
                        actual_start..actual_end,
                    )
                        .with_message("does not match this part of the inferred type"),
                ];
                labels.extend(mk_expr_label(span_opt));

                let mut diags = vec![Diagnostic::error()
                    .with_message("function types mismatch")
                    .with_labels(labels)
                    .with_notes(vec![
                        format!("The type of the expression was expected to be `{}`", expd),
                        format!("The type of the expression was inferred to be `{}`", actual),
                        String::from("Could not match the two function types"),
                    ])
                ];

                // We generate a diagnostic for the underlying error, but append a prefix to the
                // error message to make it clear that this is not a separated error but a more
                // precise description of why the unification of the row failed.
                match err.as_ref() {
                    // If the underlying error is a type mismatch, printing won't add any useful
                    // information, so we just ignore it.
                    TypecheckError::TypeMismatch(_, _, _) => (),
                    err => {
                        diags.extend(err.to_diagnostic(files, contract_id).into_iter()
                            .map(|mut diag| {
                                diag.message = format!("While matching function types: {}", diag.message);
                                diag
                            }));
                    }
                }

                diags
            }
        }
    }
}

impl ToDiagnostic<FileId> for ImportError {
    fn to_diagnostic(
        &self,
        files: &mut Files<String>,
        contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>> {
        match self {
            ImportError::IOError(path, error, span_opt) => {
                let labels = span_opt
                    .as_opt_ref()
                    .map(|span| vec![secondary(span).with_message("imported here")])
                    .unwrap_or_default();

                vec![Diagnostic::error()
                    .with_message(format!("import of {} failed: {}", path, error))
                    .with_labels(labels)]
            }
            ImportError::ParseErrors(error, span_opt) => {
                let mut diagnostic: Vec<Diagnostic<FileId>> = error
                    .errors
                    .iter()
                    .map(|e| e.to_diagnostic(files, contract_id))
                    .flatten()
                    .collect();

                if let Some(span) = span_opt.as_opt_ref() {
                    diagnostic[0]
                        .labels
                        .push(secondary(span).with_message("imported here"));
                }

                diagnostic
            }
        }
    }
}

impl ToDiagnostic<FileId> for SerializationError {
    fn to_diagnostic(
        &self,
        files: &mut Files<String>,
        _contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>> {
        match self {
            SerializationError::NotAString(rt) => vec![Diagnostic::error()
                .with_message(format!(
                    "raw export only supports `Str`, got {}",
                    rt.as_ref()
                        .type_of()
                        .unwrap_or_else(|| String::from("<unevaluated>"))
                ))
                .with_labels(vec![primary_term(rt, files)])],
            SerializationError::UnsupportedNull(format, rt) => vec![Diagnostic::error()
                .with_message(format!("{} doesn't support null values", format))
                .with_labels(vec![primary_term(rt, files)])],
            SerializationError::NonSerializable(rt) => vec![Diagnostic::error()
                .with_message("non serializable term")
                .with_labels(vec![primary_term(rt, files)])],
            SerializationError::Other(msg) => vec![Diagnostic::error()
                .with_message("error during serialization")
                .with_notes(vec![msg.clone()])],
        }
    }
}

impl ToDiagnostic<FileId> for IOError {
    fn to_diagnostic(
        &self,
        _files: &mut Files<String>,
        _contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>> {
        match self {
            IOError(msg) => vec![Diagnostic::error().with_message(msg.clone())],
        }
    }
}

impl ToDiagnostic<FileId> for ReplError {
    fn to_diagnostic(
        &self,
        _files: &mut Files<String>,
        _contract_id: Option<FileId>,
    ) -> Vec<Diagnostic<FileId>> {
        match self {
            ReplError::UnknownCommand(s) => vec![Diagnostic::error()
                .with_message(format!("unknown command `{}`", s))
                .with_notes(vec![String::from(
                    "type `:?` or `:help` for a list of available commands.",
                )])],
            ReplError::MissingArg { cmd, msg_opt } => {
                let mut notes = msg_opt
                    .as_ref()
                    .map(|msg| vec![msg.clone()])
                    .unwrap_or_default();
                notes.push(format!(
                    "type `:? {}` or `:help {}` for more information.",
                    cmd, cmd
                ));

                vec![Diagnostic::error()
                    .with_message(format!("{}: missing argument", cmd))
                    .with_notes(notes)]
            }
        }
    }
}
