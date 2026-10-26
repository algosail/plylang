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
    /// `/name` reads a record field.
    Field(String),
    /// `=name` replaces a record field. The record stays below the new value.
    FieldSet(String),
    /// `+name` inserts a record field. The record stays below the new value.
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
    LBrace,
    RBrace,
    Import,
    Language,
    Data,
    /// Transparent type synonym.
    Type,
    Foreign,
    Private,
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
        "private" => TokenKind::Private,
        "true" => TokenKind::True,
        "false" => TokenKind::False,
        _ => return None,
    })
}
