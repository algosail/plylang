//! Each diagnostic names an [`Error`]. The English sentence is formatted from that value.
//! `docs/Errors.md` has one section per code, and the printed message links to it.
//! `PL0000` is reserved for a message this compiler does not construct.

const ERROR_BASE: &str = "https://github.com/algosail/plylang/blob/main/docs/Errors.md#";

pub fn error_url(code: &str) -> String {
    format!("{ERROR_BASE}{}", code.to_ascii_lowercase())
}

/// A compiler diagnostic. The code is the identity. [`Self::message`] is the sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    UnexpectedCharacter,
    UnterminatedString,
    UnknownStringEscape,
    StrayDash,
    ExpectedExponentDigits,
    TypeVariableCase,
    ExpectedLanguageDirective,
    LanguageDirectiveInSource,
    ExpectedLanguageVersion,
    LanguageTooNew { version: u32, compiler: u32 },
    DocNotOnDeclaration,
    ImportAfterDeclaration,
    DeclarationColumn,
    ExpectedDeclaration,
    ExpectedImportPath,
    ExpectedImportedName,
    ExpectedLParen,
    ExpectedRParen,
    ExpectedTypeName,
    ExpectedTypeVariable,
    ExpectedEq,
    ExpectedConstructor,
    NewtypeOneConstructor,
    NewtypeOneField,
    DuplicateTypeVariable(String),
    ExpectedDefinitionName,
    ExpectedColon,
    ForeignHasBody,
    InterfaceHasBody,
    MissingBody,
    ExpectedTerm,
    ExpectedArrow,
    ExpectedPattern,
    ExpectedPatternCount(usize),
    ExpectedFieldName,
    ExpectedRBrace,
    DuplicateFieldName,
    ExpectedClassArgument,
    ExpectedClassName,
    ExpectedMethodName,
    ExpectedWord,
    ExpectedFatArrow,
    ExpectedDot,
    ExpectedStackArrow,
    StackTailNotBottom,
    ExpectedType,
    ExpectedRowVariable,
    ExpectedElement,
    ExpectedName,
    UnterminatedArray,
    UnterminatedQuotation,
    TypeConstructorCase,
    DuplicateDefinition(String),
    UndefinedWord(String),
    UndefinedType(String),
    CannotRedefineBuiltin(String),
    CyclicType(String),
    TypeVariableNotQuantified(String),
    TypeConstructorArity { name: String, count: usize },
    DuplicateField(String),
    TypeMismatch { expected: String, found: String },
    ExpectedRecord(String),
    ExpectedDataType(String),
    StackUnderflow,
    OccursCheck,
    KindMismatch { expected: String, found: String },
    DuplicateEffect(String),
    EffectMismatch { expected: String, found: String },
    EffectNotOpen { found: String },
    RecordHasNoField(String),
    RecordTailOpen,
    QuotationArityUnknown,
    RedundantPattern,
    NonExhaustive,
    NonExhaustiveExamples(String),
    PatternArity { name: String, count: usize },
    ConstructorNotOf { name: String, type_name: String },
    UndefinedClass(String),
    ClassNeedsParameter(String),
    ClassArity { name: String, count: usize },
    CyclicSuperclass(String),
    MethodNotOf { name: String, classes: String },
    MethodOfSeveral { name: String, owners: String },
    OrphanInstance,
    MissingMethod(String),
    InstanceWordPrivate(String),
    IncoherentInstance,
    NoInstance(String),
    AmbiguousConstraint(String),
    CyclicInstance(String),
    ImportNotExported { path: String, name: String },
    CannotResolveImport(String),
    ImportCycle(String),
    ModuleNotExported(String),
    ModuleSegment(String),
    EmptyModulePath,
    EmptyPathSegment,
    ExpectedPlyPath,
    PackageJsonInvalid,
    PackageJsonNoPly,
    PackageJsonSource,
    PackageJsonOut,
    PackageJsonLanguage,
    OutSameAsSource,
    PackageJsonForeign,
    ForeignSameDirectory,
    PackageJsonNoForeign,
    ForeignNotExported(String),
    ForeignArity {
        name: String,
        expected: usize,
        found: usize,
    },
    PackageJsonNotObject,
    SourceDirectoryNotFound,
    /// The operating-system text. The path is added by the caller.
    Os(String),
    Frozen,
    Arity,
    EntryWord,
    ForeignType,
}

impl Error {
    pub fn code(&self) -> &'static str {
        self.describe().0
    }

    pub fn message(&self) -> String {
        self.describe().1
    }

    fn describe(&self) -> (&'static str, String) {
        use Error::*;
        match self {
            UnexpectedCharacter => static_text("PL0001", "unexpected character"),
            UnterminatedString => static_text("PL0002", "unterminated string"),
            UnknownStringEscape => static_text("PL0003", "unknown string escape"),
            StrayDash => static_text("PL0004", "stray '-'"),
            ExpectedExponentDigits => static_text("PL0005", "expected exponent digits"),
            TypeVariableCase => {
                static_text("PL0006", "type variable must start with a lowercase letter")
            }
            ExpectedLanguageDirective => static_text("PL0007", "expected language directive"),
            LanguageDirectiveInSource => {
                static_text("PL0008", "language directive is only valid in an interface")
            }
            ExpectedLanguageVersion => static_text("PL0009", "expected language version"),
            LanguageTooNew { version, compiler } => (
                "PL0010",
                format!("language version {version} is newer than this compiler ({compiler})"),
            ),
            DocNotOnDeclaration => {
                static_text("PL0011", "doc comment must sit above a declaration")
            }
            ImportAfterDeclaration => static_text("PL0012", "import must come before declarations"),
            DeclarationColumn => static_text("PL0013", "declaration must start at column 0"),
            ExpectedDeclaration => static_text("PL0014", "expected declaration"),
            ExpectedImportPath => static_text("PL0015", "expected import path"),
            ExpectedImportedName => static_text("PL0016", "expected imported name"),
            ExpectedLParen => static_text("PL0017", "expected '('"),
            ExpectedRParen => static_text("PL0018", "expected ')'"),
            ExpectedTypeName => static_text("PL0019", "expected type name"),
            ExpectedTypeVariable => static_text("PL0020", "expected type variable"),
            ExpectedEq => static_text("PL0021", "expected '='"),
            ExpectedConstructor => static_text("PL0022", "expected constructor"),
            NewtypeOneConstructor => static_text("PL0023", "newtype expects one constructor"),
            NewtypeOneField => static_text("PL0024", "newtype expects one field"),
            DuplicateTypeVariable(name) => ("PL0025", format!("duplicate type variable {name}")),
            ExpectedDefinitionName => static_text("PL0026", "expected definition name"),
            ExpectedColon => static_text("PL0027", "expected ':'"),
            ForeignHasBody => static_text("PL0028", "foreign declaration has a body"),
            InterfaceHasBody => static_text("PL0029", "interface declaration has a body"),
            MissingBody => static_text("PL0030", "missing body"),
            ExpectedTerm => static_text("PL0031", "expected term"),
            ExpectedArrow => static_text("PL0032", "expected '->'"),
            ExpectedPattern => static_text("PL0033", "expected pattern"),
            ExpectedPatternCount(count) => (
                "PL0034",
                format!("expected {count} {}", noun(*count, "pattern", "patterns")),
            ),
            ExpectedFieldName => static_text("PL0035", "expected field name"),
            ExpectedRBrace => static_text("PL0036", "expected '}'"),
            DuplicateFieldName => static_text("PL0037", "duplicate field name"),
            ExpectedClassArgument => static_text("PL0038", "expected class argument"),
            ExpectedClassName => static_text("PL0039", "expected class name"),
            ExpectedMethodName => static_text("PL0040", "expected method name"),
            ExpectedWord => static_text("PL0041", "expected word"),
            ExpectedFatArrow => static_text("PL0042", "expected '=>'"),
            ExpectedDot => static_text("PL0043", "expected '.'"),
            ExpectedStackArrow => static_text("PL0044", "expected '--'"),
            StackTailNotBottom => static_text("PL0045", "stack tail must be the bottom"),
            ExpectedType => static_text("PL0046", "expected type"),
            ExpectedRowVariable => static_text("PL0047", "expected row variable"),
            ExpectedElement => static_text("PL0048", "expected element"),
            ExpectedName => static_text("PL0049", "expected name"),
            UnterminatedArray => static_text("PL0050", "unterminated array"),
            UnterminatedQuotation => static_text("PL0051", "unterminated quotation"),
            TypeConstructorCase => {
                static_text("PL0052", "type constructor must start with an uppercase letter")
            }
            DuplicateDefinition(name) => ("PL0053", format!("duplicate definition {name}")),
            UndefinedWord(name) => ("PL0054", format!("undefined word {name}")),
            UndefinedType(name) => ("PL0055", format!("undefined type {name}")),
            CannotRedefineBuiltin(name) => ("PL0056", format!("cannot redefine builtin {name}")),
            CyclicType(name) => ("PL0057", format!("cyclic type {name}")),
            TypeVariableNotQuantified(name) => {
                ("PL0058", format!("type variable {name} is not quantified"))
            }
            TypeConstructorArity { name, count } => (
                "PL0059",
                format!(
                    "type constructor {name} expects {count} {}",
                    noun(*count, "argument", "arguments")
                ),
            ),
            DuplicateField(name) => ("PL0060", format!("duplicate field {name}")),
            TypeMismatch { expected, found } => (
                "PL0061",
                format!("type mismatch: expected {expected}, found {found}"),
            ),
            ExpectedRecord(found) => (
                "PL0062",
                format!("type mismatch: expected a record, found {found}"),
            ),
            ExpectedDataType(found) => (
                "PL0063",
                format!("type mismatch: expected a data type, found {found}"),
            ),
            StackUnderflow => static_text("PL0064", "stack underflow"),
            OccursCheck => static_text("PL0065", "occurs check failed"),
            KindMismatch { expected, found } => (
                "PL0066",
                format!("kind mismatch: expected {expected}, found {found}"),
            ),
            DuplicateEffect(label) => ("PL0067", format!("duplicate effect {label}")),
            EffectMismatch { expected, found } => (
                "PL0068",
                format!("effect mismatch: expected {expected}, found {found}"),
            ),
            EffectNotOpen { found } => (
                "PL0069",
                format!("effect mismatch: expected {{ | _ }}, found {found}"),
            ),
            RecordHasNoField(name) => ("PL0070", format!("record has no field {name}")),
            RecordTailOpen => static_text("PL0071", "record tail must be closed"),
            QuotationArityUnknown => static_text("PL0072", "quotation arity is not known"),
            RedundantPattern => static_text("PL0073", "redundant pattern"),
            NonExhaustive => static_text("PL0074", "non-exhaustive match"),
            NonExhaustiveExamples(examples) => {
                ("PL0075", format!("non-exhaustive match: {examples}"))
            }
            PatternArity { name, count } => (
                "PL0076",
                format!(
                    "pattern {name} expects {count} {}",
                    noun(*count, "field", "fields")
                ),
            ),
            ConstructorNotOf { name, type_name } => (
                "PL0077",
                format!("constructor {name} is not a constructor of {type_name}"),
            ),
            UndefinedClass(name) => ("PL0078", format!("undefined class {name}")),
            ClassNeedsParameter(name) => ("PL0079", format!("class {name} expects a parameter")),
            ClassArity { name, count } => (
                "PL0080",
                format!(
                    "class {name} expects {count} {}",
                    noun(*count, "argument", "arguments")
                ),
            ),
            CyclicSuperclass(name) => ("PL0081", format!("cyclic superclass: {name}")),
            MethodNotOf { name, classes } => (
                "PL0082",
                format!("method {name} is not a method of {classes}"),
            ),
            MethodOfSeveral { name, owners } => {
                ("PL0083", format!("method {name} is a method of {owners}"))
            }
            OrphanInstance => static_text("PL0084", "orphan instance"),
            MissingMethod(name) => ("PL0085", format!("missing method {name}")),
            InstanceWordPrivate(name) => ("PL0086", format!("instance word {name} is private")),
            IncoherentInstance => static_text("PL0087", "incoherent instance"),
            NoInstance(constraint) => ("PL0088", format!("no instance for {constraint}")),
            AmbiguousConstraint(constraint) => {
                ("PL0089", format!("ambiguous constraint {constraint}"))
            }
            CyclicInstance(key) => ("PL0090", format!("cyclic instance: {key}")),
            ImportNotExported { path, name } => (
                "PL0091",
                format!("~ \"{path}\" does not export {name}"),
            ),
            CannotResolveImport(path) => ("PL0092", format!("cannot resolve ~ \"{path}\"")),
            ImportCycle(names) => ("PL0093", format!("import cycle: {names}")),
            ModuleNotExported(name) => ("PL0094", format!("module {name} is not exported")),
            ModuleSegment(segment) => (
                "PL0095",
                format!("module segment '{segment}' must start with an uppercase letter"),
            ),
            EmptyModulePath => static_text("PL0096", "empty module path"),
            EmptyPathSegment => static_text("PL0097", "empty path segment"),
            ExpectedPlyPath => static_text("PL0098", "expected a .ply or .d.ply path"),
            PackageJsonInvalid => static_text("PL0099", "package.json is not valid JSON"),
            PackageJsonNoPly => static_text("PL0100", "package.json has no \"ply\" field"),
            PackageJsonSource => static_text("PL0101", "package.json ply.source must be a string"),
            PackageJsonOut => static_text("PL0102", "package.json ply.out must be a string"),
            PackageJsonLanguage => {
                static_text("PL0103", "package.json ply.language must be a number")
            }
            OutSameAsSource => {
                static_text("PL0104", "ply.out must be a different directory from ply.source")
            }
            PackageJsonForeign => {
                static_text("PL0105", "package.json ply.foreign must be a string")
            }
            ForeignSameDirectory => static_text(
                "PL0106",
                "ply.foreign must be a different directory from ply.source and ply.out",
            ),
            PackageJsonNoForeign => {
                static_text("PL0107", "package.json has no \"ply.foreign\" field")
            }
            ForeignNotExported(name) => ("PL0108", format!("foreign word {name} is not exported")),
            ForeignArity {
                name,
                expected,
                found,
            } => (
                "PL0109",
                format!("foreign word {name} expects {expected} parameters, found {found}"),
            ),
            PackageJsonNotObject => static_text("PL0110", "package.json must be an object"),
            SourceDirectoryNotFound => static_text("PL0111", "source directory not found"),
            Os(detail) => ("PL0112", detail.clone()),
            Frozen => static_text("PL0113", "frozen"),
            Arity => static_text("PL0114", "arity"),
            EntryWord => static_text("PL0115", "word main must have scheme ( -- Unit )"),
            ForeignType => static_text("PL0116", "foreign type has no constructors"),
        }
    }
}

fn static_text(code: &'static str, message: &'static str) -> (&'static str, String) {
    (code, message.to_string())
}

fn noun(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::{error_url, Error};

    #[test]
    fn every_code_formats_its_sentence_and_has_a_section() {
        let page = include_str!("../../../docs/Errors.md");
        let samples = [
            (Error::UnexpectedCharacter, "unexpected character", "PL0001"),
            (Error::UnterminatedString, "unterminated string", "PL0002"),
            (Error::UnknownStringEscape, "unknown string escape", "PL0003"),
            (Error::StrayDash, "stray '-'", "PL0004"),
            (Error::ExpectedExponentDigits, "expected exponent digits", "PL0005"),
            (
                Error::TypeVariableCase,
                "type variable must start with a lowercase letter",
                "PL0006",
            ),
            (Error::ExpectedLanguageDirective, "expected language directive", "PL0007"),
            (
                Error::LanguageDirectiveInSource,
                "language directive is only valid in an interface",
                "PL0008",
            ),
            (Error::ExpectedLanguageVersion, "expected language version", "PL0009"),
            (
                Error::LanguageTooNew { version: 2, compiler: 1 },
                "language version 2 is newer than this compiler (1)",
                "PL0010",
            ),
            (
                Error::DocNotOnDeclaration,
                "doc comment must sit above a declaration",
                "PL0011",
            ),
            (Error::ImportAfterDeclaration, "import must come before declarations", "PL0012"),
            (Error::DeclarationColumn, "declaration must start at column 0", "PL0013"),
            (Error::ExpectedDeclaration, "expected declaration", "PL0014"),
            (Error::ExpectedImportPath, "expected import path", "PL0015"),
            (Error::ExpectedImportedName, "expected imported name", "PL0016"),
            (Error::ExpectedLParen, "expected '('", "PL0017"),
            (Error::ExpectedRParen, "expected ')'", "PL0018"),
            (Error::ExpectedTypeName, "expected type name", "PL0019"),
            (Error::ExpectedTypeVariable, "expected type variable", "PL0020"),
            (Error::ExpectedEq, "expected '='", "PL0021"),
            (Error::ExpectedConstructor, "expected constructor", "PL0022"),
            (Error::NewtypeOneConstructor, "newtype expects one constructor", "PL0023"),
            (Error::NewtypeOneField, "newtype expects one field", "PL0024"),
            (Error::DuplicateTypeVariable("a".into()), "duplicate type variable a", "PL0025"),
            (Error::ExpectedDefinitionName, "expected definition name", "PL0026"),
            (Error::ExpectedColon, "expected ':'", "PL0027"),
            (Error::ForeignHasBody, "foreign declaration has a body", "PL0028"),
            (Error::InterfaceHasBody, "interface declaration has a body", "PL0029"),
            (Error::MissingBody, "missing body", "PL0030"),
            (Error::ExpectedTerm, "expected term", "PL0031"),
            (Error::ExpectedArrow, "expected '->'", "PL0032"),
            (Error::ExpectedPattern, "expected pattern", "PL0033"),
            (Error::ExpectedPatternCount(2), "expected 2 patterns", "PL0034"),
            (Error::ExpectedFieldName, "expected field name", "PL0035"),
            (Error::ExpectedRBrace, "expected '}'", "PL0036"),
            (Error::DuplicateFieldName, "duplicate field name", "PL0037"),
            (Error::ExpectedClassArgument, "expected class argument", "PL0038"),
            (Error::ExpectedClassName, "expected class name", "PL0039"),
            (Error::ExpectedMethodName, "expected method name", "PL0040"),
            (Error::ExpectedWord, "expected word", "PL0041"),
            (Error::ExpectedFatArrow, "expected '=>'", "PL0042"),
            (Error::ExpectedDot, "expected '.'", "PL0043"),
            (Error::ExpectedStackArrow, "expected '--'", "PL0044"),
            (Error::StackTailNotBottom, "stack tail must be the bottom", "PL0045"),
            (Error::ExpectedType, "expected type", "PL0046"),
            (Error::ExpectedRowVariable, "expected row variable", "PL0047"),
            (Error::ExpectedElement, "expected element", "PL0048"),
            (Error::ExpectedName, "expected name", "PL0049"),
            (Error::UnterminatedArray, "unterminated array", "PL0050"),
            (Error::UnterminatedQuotation, "unterminated quotation", "PL0051"),
            (
                Error::TypeConstructorCase,
                "type constructor must start with an uppercase letter",
                "PL0052",
            ),
            (Error::DuplicateDefinition("f".into()), "duplicate definition f", "PL0053"),
            (Error::UndefinedWord("nope".into()), "undefined word nope", "PL0054"),
            (Error::UndefinedType("Maybe".into()), "undefined type Maybe", "PL0055"),
            (Error::CannotRedefineBuiltin("dup".into()), "cannot redefine builtin dup", "PL0056"),
            (Error::CyclicType("A".into()), "cyclic type A", "PL0057"),
            (
                Error::TypeVariableNotQuantified("a".into()),
                "type variable a is not quantified",
                "PL0058",
            ),
            (
                Error::TypeConstructorArity { name: "Maybe".into(), count: 1 },
                "type constructor Maybe expects 1 argument",
                "PL0059",
            ),
            (Error::DuplicateField("city".into()), "duplicate field city", "PL0060"),
            (
                Error::TypeMismatch { expected: "Int".into(), found: "Str".into() },
                "type mismatch: expected Int, found Str",
                "PL0061",
            ),
            (
                Error::ExpectedRecord("Int".into()),
                "type mismatch: expected a record, found Int",
                "PL0062",
            ),
            (
                Error::ExpectedDataType("Int".into()),
                "type mismatch: expected a data type, found Int",
                "PL0063",
            ),
            (Error::StackUnderflow, "stack underflow", "PL0064"),
            (Error::OccursCheck, "occurs check failed", "PL0065"),
            (
                Error::KindMismatch { expected: "Type".into(), found: "Type -> Type".into() },
                "kind mismatch: expected Type, found Type -> Type",
                "PL0066",
            ),
            (Error::DuplicateEffect("io".into()), "duplicate effect io", "PL0067"),
            (
                Error::EffectMismatch { expected: "{ io }".into(), found: "{ console }".into() },
                "effect mismatch: expected { io }, found { console }",
                "PL0068",
            ),
            (
                Error::EffectNotOpen { found: "{ console }".into() },
                "effect mismatch: expected { | _ }, found { console }",
                "PL0069",
            ),
            (Error::RecordHasNoField("age".into()), "record has no field age", "PL0070"),
            (Error::RecordTailOpen, "record tail must be closed", "PL0071"),
            (Error::QuotationArityUnknown, "quotation arity is not known", "PL0072"),
            (Error::RedundantPattern, "redundant pattern", "PL0073"),
            (Error::NonExhaustive, "non-exhaustive match", "PL0074"),
            (Error::NonExhaustiveExamples("Just".into()), "non-exhaustive match: Just", "PL0075"),
            (
                Error::PatternArity { name: "Just".into(), count: 1 },
                "pattern Just expects 1 field",
                "PL0076",
            ),
            (
                Error::ConstructorNotOf { name: "Left".into(), type_name: "Maybe".into() },
                "constructor Left is not a constructor of Maybe",
                "PL0077",
            ),
            (Error::UndefinedClass("Show".into()), "undefined class Show", "PL0078"),
            (Error::ClassNeedsParameter("Eq".into()), "class Eq expects a parameter", "PL0079"),
            (
                Error::ClassArity { name: "Eq".into(), count: 1 },
                "class Eq expects 1 argument",
                "PL0080",
            ),
            (Error::CyclicSuperclass("Eq".into()), "cyclic superclass: Eq", "PL0081"),
            (
                Error::MethodNotOf { name: "eq".into(), classes: "Ord".into() },
                "method eq is not a method of Ord",
                "PL0082",
            ),
            (
                Error::MethodOfSeveral { name: "eq".into(), owners: "Eq and Also".into() },
                "method eq is a method of Eq and Also",
                "PL0083",
            ),
            (Error::OrphanInstance, "orphan instance", "PL0084"),
            (Error::MissingMethod("eq".into()), "missing method eq", "PL0085"),
            (
                Error::InstanceWordPrivate("eqInt".into()),
                "instance word eqInt is private",
                "PL0086",
            ),
            (Error::IncoherentInstance, "incoherent instance", "PL0087"),
            (Error::NoInstance("Eq Int".into()), "no instance for Eq Int", "PL0088"),
            (Error::AmbiguousConstraint("Eq _".into()), "ambiguous constraint Eq _", "PL0089"),
            (
                Error::CyclicInstance("Eq (Maybe Int)".into()),
                "cyclic instance: Eq (Maybe Int)",
                "PL0090",
            ),
            (
                Error::ImportNotExported { path: "Data/Maybe".into(), name: "hidden".into() },
                "~ \"Data/Maybe\" does not export hidden",
                "PL0091",
            ),
            (
                Error::CannotResolveImport("Data/Num".into()),
                "cannot resolve ~ \"Data/Num\"",
                "PL0092",
            ),
            (
                Error::ImportCycle("Data.A, Data.B, Data.A".into()),
                "import cycle: Data.A, Data.B, Data.A",
                "PL0093",
            ),
            (
                Error::ModuleNotExported("Data.Num".into()),
                "module Data.Num is not exported",
                "PL0094",
            ),
            (
                Error::ModuleSegment("data".into()),
                "module segment 'data' must start with an uppercase letter",
                "PL0095",
            ),
            (Error::EmptyModulePath, "empty module path", "PL0096"),
            (Error::EmptyPathSegment, "empty path segment", "PL0097"),
            (Error::ExpectedPlyPath, "expected a .ply or .d.ply path", "PL0098"),
            (Error::PackageJsonInvalid, "package.json is not valid JSON", "PL0099"),
            (Error::PackageJsonNoPly, "package.json has no \"ply\" field", "PL0100"),
            (Error::PackageJsonSource, "package.json ply.source must be a string", "PL0101"),
            (Error::PackageJsonOut, "package.json ply.out must be a string", "PL0102"),
            (Error::PackageJsonLanguage, "package.json ply.language must be a number", "PL0103"),
            (
                Error::OutSameAsSource,
                "ply.out must be a different directory from ply.source",
                "PL0104",
            ),
            (Error::PackageJsonForeign, "package.json ply.foreign must be a string", "PL0105"),
            (
                Error::ForeignSameDirectory,
                "ply.foreign must be a different directory from ply.source and ply.out",
                "PL0106",
            ),
            (Error::PackageJsonNoForeign, "package.json has no \"ply.foreign\" field", "PL0107"),
            (Error::ForeignNotExported("add".into()), "foreign word add is not exported", "PL0108"),
            (
                Error::ForeignArity { name: "add".into(), expected: 2, found: 1 },
                "foreign word add expects 2 parameters, found 1",
                "PL0109",
            ),
            (Error::PackageJsonNotObject, "package.json must be an object", "PL0110"),
            (Error::SourceDirectoryNotFound, "source directory not found", "PL0111"),
            (Error::Os("disk full".into()), "disk full", "PL0112"),
            (Error::Frozen, "frozen", "PL0113"),
            (Error::Arity, "arity", "PL0114"),
            (
                Error::EntryWord,
                "word main must have scheme ( -- Unit )",
                "PL0115",
            ),
            (
                Error::ForeignType,
                "foreign type has no constructors",
                "PL0116",
            ),
        ];
        assert_eq!(samples.len(), 116);
        for (error, message, code) in samples {
            assert_eq!(error.code(), code, "{message}");
            assert_eq!(error.message(), message);
            assert!(
                page.contains(&format!("## {code}\n")),
                "docs/Errors.md has no section {code}"
            );
            assert_eq!(
                error_url(code),
                format!(
                    "https://github.com/algosail/plylang/blob/main/docs/Errors.md#{}",
                    code.to_ascii_lowercase()
                )
            );
        }
        assert_eq!(
            Error::ExpectedPatternCount(1).message(),
            "expected 1 pattern"
        );
        assert_eq!(
            Error::TypeConstructorArity { name: "Pair".into(), count: 2 }.message(),
            "type constructor Pair expects 2 arguments"
        );
    }
}
