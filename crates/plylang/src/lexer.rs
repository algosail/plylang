use crate::diagnostic::Diagnostic;
use crate::codes::Error;
use crate::span::Span;
use crate::token::{keyword, Token, TokenKind};

pub fn tokenize(source: &str) -> Result<Vec<Token>, Diagnostic> {
    let mut lexer = Lexer {
        source,
        index: 0,
        line_start_at: 0,
        at_line_start: true,
        group_depth: 0,
    };
    let mut tokens = Vec::new();
    loop {
        lexer.skip_ignored();
        if lexer.eof() {
            tokens.push(lexer.make(TokenKind::Eof, lexer.index, lexer.index));
            break;
        }
        let ch = lexer.peek().expect("eof checked");
        if ch == '\n' || ch == '\r' {
            let start = lexer.index;
            lexer.bump();
            if ch == '\r' && lexer.peek() == Some('\n') {
                lexer.bump();
            }
            let line_blank = lexer.source[lexer.line_start_at..start]
                .chars()
                .all(|ch| ch == ' ' || ch == '\t');
            lexer.line_start_at = lexer.index;
            lexer.at_line_start = true;
            let only_breaks = tokens.iter().all(|token| token.kind == TokenKind::Newline);
            let previous_break = tokens
                .last()
                .is_some_and(|token| token.kind == TokenKind::Newline);
            let doc_before = tokens.iter().rev().find(|token| token.kind != TokenKind::Newline).is_some_and(|token| {
                matches!(token.kind, TokenKind::Doc(_))
            });
            if !only_breaks && !previous_break
                || (previous_break && doc_before && line_blank)
            {
                tokens.push(lexer.make(TokenKind::Newline, start, lexer.index));
            }
            continue;
        }
        tokens.push(lexer.next_token()?);
    }
    Ok(tokens)
}

struct Lexer<'a> {
    source: &'a str,
    index: usize,
    line_start_at: usize,
    at_line_start: bool,
    /// Round parentheses. Outside them `.` is a comment. Inside, it is scheme syntax.
    group_depth: u32,
}

impl<'a> Lexer<'a> {
    fn eof(&self) -> bool {
        self.index >= self.source.len()
    }

    fn peek(&self) -> Option<char> {
        self.source[self.index..].chars().next()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        let mut index = self.index;
        for _ in 0..offset {
            let ch = self.source[index..].chars().next()?;
            index += ch.len_utf8();
        }
        self.source[index..].chars().next()
    }

    fn bump(&mut self) -> char {
        let ch = self.peek().expect("bump at eof");
        self.index += ch.len_utf8();
        ch
    }

    fn skip_ignored(&mut self) {
        loop {
            match self.peek() {
                Some(' ' | '\t') => {
                    self.bump();
                }
                Some('.')
                    if self.group_depth == 0
                        && self.peek_at(1) == Some('.')
                        && self.index == self.line_start_at =>
                {
                    break
                }
                Some('.') if self.group_depth == 0 => {
                    self.bump();
                    while let Some(ch) = self.peek() {
                        if ch == '\n' || ch == '\r' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn make(&mut self, kind: TokenKind, start: usize, end: usize) -> Token {
        let line_start = self.at_line_start;
        let col = start.saturating_sub(self.line_start_at) as u32;
        if !matches!(kind, TokenKind::Newline | TokenKind::Eof) {
            self.at_line_start = false;
        }
        Token {
            kind,
            span: Span::new(start, end),
            line_start,
            col,
        }
    }

    fn error(&self, start: usize, error: Error) -> Diagnostic {
        Diagnostic::new(
            Span::new(
                start,
                self.index
                    .max(start + 1)
                    .min(self.source.len().max(start + 1)),
            ),
            error,
        )
    }

    fn next_token(&mut self) -> Result<Token, Diagnostic> {
        let start = self.index;
        let ch = self.bump();
        let kind = match ch {
            '(' => {
                self.group_depth += 1;
                TokenKind::LParen
            }
            ')' => {
                self.group_depth = self.group_depth.saturating_sub(1);
                TokenKind::RParen
            }
            '[' => TokenKind::LBrack,
            ']' => TokenKind::RBrack,
            ':' => TokenKind::Colon,
            ',' => TokenKind::Comma,
            '|' => TokenKind::Pipe,
            '=' if self.peek() == Some('>') => {
                self.bump();
                TokenKind::FatArrow
            }
            '=' if self.peek() == Some('=') => {
                self.bump();
                TokenKind::EqEq
            }
            '=' => TokenKind::Eq,
            '<' => TokenKind::LAngle,
            // `>>name` replaces a field. `>` and `>>` without a name stay angles.
            '>' if self.peek() == Some('>') && self.peek_at(1).is_some_and(is_ident_start) => {
                self.bump();
                let name_at = self.index;
                while self.peek().is_some_and(is_ident_continue) {
                    self.bump();
                }
                let name = self.source[name_at..self.index].to_string();
                return Ok(self.make(TokenKind::FieldSet(name), start, self.index));
            }
            '>' => TokenKind::RAngle,
            // Declaration runes. `-` at column 0 is a synonym; other dashes stay in `dash`.
            '~' => TokenKind::Import,
            ';' => TokenKind::Data,
            '&' => TokenKind::Class,
            '#' => TokenKind::Instance,
            '.' if self.group_depth == 0 => return self.doc_line(start),
            '.' => return self.dot(start),
            '"' => return self.string(start),
            '-' => return self.dash(start),
            ch if ch.is_ascii_digit() => return self.number(start, false),
            ch if is_ident_start(ch) => {
                while self.peek().is_some_and(is_ident_continue) {
                    self.bump();
                }
                // `name>>` reads a field. A space before `>>` leaves the name and two angles.
                if self.peek() == Some('>') && self.peek_at(1) == Some('>') {
                    let text = self.source[start..self.index].to_string();
                    self.bump();
                    self.bump();
                    return Ok(self.make(TokenKind::Field(text), start, self.index));
                }
                let text = self.source[start..self.index].to_string();
                keyword(&text).unwrap_or(TokenKind::Ident(text))
            }
            _ => {
                return Err(self.error(start, Error::UnexpectedCharacter));
            }
        };
        Ok(self.make(kind, start, self.index))
    }

    /// `..` at column 0, outside parentheses. One leading space after the marker is not part of the text.
    fn doc_line(&mut self, start: usize) -> Result<Token, Diagnostic> {
        if self.peek() != Some('.') {
            return Err(self.error(start, Error::UnexpectedCharacter));
        }
        self.bump();
        if self.peek() == Some(' ') {
            self.bump();
        }
        let mut text = String::new();
        while let Some(ch) = self.peek() {
            if ch == '\n' || ch == '\r' {
                break;
            }
            text.push(self.bump());
        }
        Ok(self.make(TokenKind::Doc(text), start, self.index))
    }

    fn string(&mut self, start: usize) -> Result<Token, Diagnostic> {
        let mut value = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(Diagnostic::new(Span::new(start, self.index), Error::UnterminatedString));
            };
            self.bump();
            match ch {
                '"' => break,
                '\\' => {
                    let Some(escaped) = self.peek() else {
                        return Err(Diagnostic::new(Span::new(start, self.index), Error::UnterminatedString));
                    };
                    self.bump();
                    let mapped = match escaped {
                        '\\' => '\\',
                        '"' => '"',
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        _ => {
                            return Err(Diagnostic::new(
                                Span::new(self.index - escaped.len_utf8(), self.index),
                                Error::UnknownStringEscape,
                            ));
                        }
                    };
                    value.push(mapped);
                }
                other => value.push(other),
            }
        }
        Ok(self.make(TokenKind::String(value), start, self.index))
    }

    fn dot(&mut self, start: usize) -> Result<Token, Diagnostic> {
        if self.peek() == Some('.') {
            self.bump();
            if self.peek().is_some_and(is_ident_start) {
                let name_at = self.index;
                while self.peek().is_some_and(is_ident_continue) {
                    self.bump();
                }
                let name = self.source[name_at..self.index].to_string();
                if !name
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_lowercase())
                {
                    return Err(Diagnostic::new(
                        Span::new(start, self.index),
                        Error::TypeVariableCase,
                    ));
                }
                return Ok(self.make(TokenKind::Tail(name), start, self.index));
            }
            return Ok(self.make(TokenKind::All, start, self.index));
        }
        Ok(self.make(TokenKind::Dot, start, self.index))
    }

    fn dash(&mut self, start: usize) -> Result<Token, Diagnostic> {
        if self.peek() == Some('-') {
            self.bump();
            return Ok(self.make(TokenKind::StackArrow, start, self.index));
        }
        if self.peek() == Some('>') {
            self.bump();
            return Ok(self.make(TokenKind::Arrow, start, self.index));
        }
        if self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            return self.number(start, true);
        }
        // `- Age` at column 0 is a synonym. `a - b` stays a stray dash.
        if start == self.line_start_at {
            return Ok(self.make(TokenKind::Type, start, self.index));
        }
        Err(self.error(start, Error::StrayDash))
    }

    fn number(&mut self, start: usize, _negative: bool) -> Result<Token, Diagnostic> {
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
        }
        let mut float = false;
        if self.peek() == Some('.') && self.peek_at(1).is_some_and(|ch| ch.is_ascii_digit()) {
            float = true;
            self.bump();
            while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            float = true;
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if !self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                return Err(self.error(start, Error::ExpectedExponentDigits));
            }
            while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                self.bump();
            }
        }
        // `1n` is Int. `1name` stays the number `1` and the name `name`.
        // A fraction or an exponent is always Number: the suffix is not part of it.
        if !float && self.peek() == Some('n') && !self.peek_at(1).is_some_and(is_ident_continue) {
            let digits_end = self.index;
            self.bump();
            let lexeme = self.source[start..digits_end].to_string();
            return Ok(self.make(TokenKind::Int(lexeme), start, self.index));
        }
        let lexeme = self.source[start..self.index].to_string();
        Ok(self.make(TokenKind::Number(lexeme), start, self.index))
    }
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::tokenize;

    fn spell(source: &str) -> Vec<String> {
        tokenize(source)
            .unwrap_or_else(|error| panic!("{}", error.message))
            .into_iter()
            .map(|token| format!("{:?}", token.kind))
            .filter(|kind| kind != "Eof")
            .collect()
    }

    #[test]
    fn stack_arrow_is_not_a_comment() {
        let kinds = spell("Int -- Int . gone\n");
        assert_eq!(
            kinds,
            vec![
                "Ident(\"Int\")".to_string(),
                "StackArrow".to_string(),
                "Ident(\"Int\")".to_string(),
                "Newline".to_string(),
            ]
        );
        let quoted = &tokenize("\"keep . this\"").unwrap()[0];
        assert_eq!(
            quoted.kind,
            crate::token::TokenKind::String("keep . this".to_string())
        );
        let error = tokenize("// nope").unwrap_err();
        assert_eq!(error.message, "unexpected character");
        assert_eq!(
            spell("; Maybe\n"),
            vec!["Data".to_string(), "Ident(\"Maybe\")".to_string(), "Newline".to_string(),]
        );
        assert_eq!(
            spell(".. Copy the top.\n. note\n"),
            vec!["Doc(\"Copy the top.\")".to_string(), "Newline".to_string(),]
        );
        assert_eq!(
            spell(".. kept\n\ndef\n"),
            vec![
                "Doc(\"kept\")".to_string(),
                "Newline".to_string(),
                "Newline".to_string(),
                "Ident(\"def\")".to_string(),
                "Newline".to_string(),
            ]
        );
        assert_eq!(
            spell("1. hidden\n2"),
            vec![
                "Number(\"1\")".to_string(),
                "Newline".to_string(),
                "Number(\"2\")".to_string(),
            ]
        );
    }

    #[test]
    fn literals_and_tails() {
        let kinds = spell("-42 1.5 1e10 name>> (..s . ..)");
        assert!(kinds.iter().any(|kind| kind == "Number(\"-42\")"));
        assert!(kinds.iter().any(|kind| kind == "Number(\"1.5\")"));
        assert!(kinds.iter().any(|kind| kind == "Number(\"1e10\")"));
        assert_eq!(
            spell("(1.) 1n -1n 1name >>age"),
            vec![
                "LParen".to_string(),
                "Number(\"1\")".to_string(),
                "Dot".to_string(),
                "RParen".to_string(),
                "Int(\"1\")".to_string(),
                "Int(\"-1\")".to_string(),
                "Number(\"1\")".to_string(),
                "Ident(\"name\")".to_string(),
                "FieldSet(\"age\")".to_string(),
            ]
        );
        assert!(kinds.iter().any(|kind| kind == "Tail(\"s\")"));
        assert!(kinds.iter().any(|kind| kind == "Field(\"name\")"));
        assert_eq!(
            spell(">>age name>>"),
            vec![
                "FieldSet(\"age\")".to_string(),
                "Field(\"name\")".to_string(),
            ]
        );
        for source in ["+city", "+ city", "+", "+1", "+=", "++", "{ name: 1 }", "}"] {
            let error = tokenize(source).unwrap_err();
            assert_eq!(error.message, "unexpected character", "{source}");
        }
        assert!(kinds.iter().any(|kind| kind == "Dot"));
        assert!(kinds.iter().any(|kind| kind == "All"));
        let inside = spell("(\n..s\n)");
        assert!(inside.iter().any(|kind| kind == "Tail(\"s\")"), "{inside:?}");
        assert!(!inside.iter().any(|kind| kind.starts_with("Doc")), "{inside:?}");
        assert_eq!(
            spell("< < dup > > <= => >>age name>> eq = eqInt"),
            vec![
                "LAngle".to_string(),
                "LAngle".to_string(),
                "Ident(\"dup\")".to_string(),
                "RAngle".to_string(),
                "RAngle".to_string(),
                "LAngle".to_string(),
                "Eq".to_string(),
                "FatArrow".to_string(),
                "FieldSet(\"age\")".to_string(),
                "Field(\"name\")".to_string(),
                "Ident(\"eq\")".to_string(),
                "Eq".to_string(),
                "Ident(\"eqInt\")".to_string(),
            ]
        );
    }

    #[test]
    fn string_escapes_and_errors() {
        let token = &tokenize(r#""a\nb""#).unwrap()[0];
        assert_eq!(
            token.kind,
            crate::token::TokenKind::String("a\nb".to_string())
        );
        let error = tokenize("\"hi").unwrap_err();
        assert_eq!(error.message, "unterminated string");
        let error = tokenize(r#""\q""#).unwrap_err();
        assert_eq!(error.message, "unknown string escape");
        let error = tokenize("a - x").unwrap_err();
        assert_eq!(error.message, "stray '-'");
        assert_eq!(
            spell("x y -> y"),
            vec![
                "Ident(\"x\")".to_string(),
                "Ident(\"y\")".to_string(),
                "Arrow".to_string(),
                "Ident(\"y\")".to_string(),
            ]
        );
        assert_eq!(
            spell("::"),
            vec!["Colon".to_string(), "Colon".to_string(),]
        );
        assert_eq!(
            spell("- Age\n"),
            vec!["Type".to_string(), "Ident(\"Age\")".to_string(), "Newline".to_string(),]
        );
    }
}
