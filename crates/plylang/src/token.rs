use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// True when only whitespace separates this token from the start of the line.
    pub line_start: bool,
    pub col: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Newline,
    Ident(String),
    Int(String),
    Number(String),
    String(String),
    /// `..name`
    Tail(String),
    /// `name>>` reads a field.
    Field(String),
    /// `>>name` replaces a field and returns a new value.
    FieldSet(String),
    /// Kept so error numbers stay put. The lexer does not produce this token.
    #[allow(dead_code)]
    FieldInsert(String),
    /// `..` in an import list.
    All,
    /// `.`
    Dot,
    /// `--`
    StackArrow,
    /// `->` separates a pattern from its body.
    Arrow,
    /// `=>` in a scheme, and between superclasses and the declared class.
    FatArrow,
    Colon,
    Eq,
    /// `==` on a `;` type. One constructor, one field, erased in JavaScript.
    EqEq,
    Pipe,
    Comma,
    LParen,
    RParen,
    /// `[` opens a quotation.
    LBrack,
    /// `]` closes a quotation.
    RBrack,
    /// `<` opens an array.
    LAngle,
    /// `>` closes an array.
    RAngle,
    /// `@` opens the module's name list.
    Export,
    /// Kept so error numbers stay put. The lexer does not produce this token.
    LBrace,
    /// Kept so error numbers stay put. The lexer does not produce this token.
    RBrace,
    Import,
    Language,
    Data,
    /// Transparent type synonym.
    Type,
    Foreign,
    Class,
    Instance,
    True,
    False,
    /// A `..` line at column 0, outside parentheses. One leading space is already stripped.
    Doc(String),
    Eof,
}

pub fn keyword(name: &str) -> Option<TokenKind> {
    Some(match name {
        "language" => TokenKind::Language,
        "foreign" => TokenKind::Foreign,
        "true" => TokenKind::True,
        "false" => TokenKind::False,
        _ => return None,
    })
}
