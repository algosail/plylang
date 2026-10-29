use std::collections::HashSet;

use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::codes::Error;
use crate::lexer::tokenize;
use crate::paths::ParseMode;
use crate::span::Span;
use crate::token::TokenKind;
use crate::LANGUAGE_VERSION;

pub fn parse(source: &str, mode: ParseMode) -> Result<Module, Diagnostic> {
    let tokens = tokenize(source)?;
    Parser {
        tokens,
        pos: 0,
        mode,
        group_depth: 0,
        last_span: Span::new(0, 0),
    }
    .parse_module()
}

struct Parser {
    tokens: Vec<crate::token::Token>,
    pos: usize,
    mode: ParseMode,
    group_depth: u32,
    last_span: Span,
}

impl Parser {
    fn parse_module(&mut self) -> Result<Module, Diagnostic> {
        self.skip_newlines();
        if self.is_eof() {
            if self.mode == ParseMode::Interface {
                return Err(self.error(Error::ExpectedLanguageDirective));
            }
            return Ok(Module {
                span: Span::new(0, 0),
                language: None,
                imports: Vec::new(),
                items: Vec::new(),
            });
        }
        let start = self.cur().span;
        let language = if self.is(TokenKind::Language) {
            if self.mode != ParseMode::Interface {
                return Err(self.error(Error::LanguageDirectiveInSource));
            }
            let version = self.parse_language()?;
            self.skip_newlines();
            Some(version)
        } else if self.mode == ParseMode::Interface {
            return Err(self.error(Error::ExpectedLanguageDirective));
        } else {
            None
        };
        let imports = self.parse_imports()?;
        let items = self.parse_items()?;
        let module = Module {
            span: start.merge(self.last_span),
            language,
            imports,
            items,
        };
        check_duplicates(&module)?;
        Ok(module)
    }

    fn parse_language(&mut self) -> Result<u32, Diagnostic> {
        self.bump();
        // `language 1` is a bare number. `1n` is accepted too. A fraction is not.
        let lexeme = match &self.cur().kind {
            TokenKind::Int(lexeme) => lexeme.clone(),
            TokenKind::Number(lexeme) if lexeme.chars().all(|ch| ch.is_ascii_digit()) => {
                lexeme.clone()
            }
            _ => return Err(self.error(Error::ExpectedLanguageVersion)),
        };
        let version = lexeme
            .parse::<u32>()
            .map_err(|_| self.error(Error::ExpectedLanguageVersion))?;
        if version == 0 {
            return Err(self.error(Error::ExpectedLanguageVersion));
        }
        if version > LANGUAGE_VERSION {
            return Err(self.error(Error::LanguageTooNew { version, compiler: LANGUAGE_VERSION }));
        }
        self.bump();
        Ok(version)
    }

    fn parse_imports(&mut self) -> Result<Vec<Import>, Diagnostic> {
        let mut imports = Vec::new();
        loop {
            self.skip_newlines();
            if self.is(TokenKind::Doc(String::new())) {
                let saved = self.pos;
                let doc = self.take_doc();
                if self.is(TokenKind::Import) && !doc.is_empty() {
                    return Err(self.error(Error::DocNotOnDeclaration));
                }
                if self.is(TokenKind::Import) {
                    continue;
                }
                self.pos = saved;
                break;
            }
            if !self.is(TokenKind::Import) {
                break;
            }
            self.expect_at_column_0()?;
            imports.push(self.parse_import()?);
        }
        Ok(imports)
    }

    fn parse_import(&mut self) -> Result<Import, Diagnostic> {
        let start = self.cur().span;
        self.bump();
        let TokenKind::String(path) = self.cur().kind.clone() else {
            return Err(self.error(Error::ExpectedImportPath));
        };
        let path = path.clone();
        self.bump();
        self.expect(&TokenKind::LParen, Error::ExpectedLParen)?;
        let mut names = Vec::new();
        let mut all = false;
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RParen) {
                break;
            }
            if self.eat(&TokenKind::All) {
                all = true;
            } else if matches!(self.cur().kind, TokenKind::Ident(_)) {
                names.push(self.expect_ident(Error::ExpectedImportedName)?);
            } else {
                return Err(self.error(Error::ExpectedImportedName));
            }
            self.skip_newlines();
            if self.eat(&TokenKind::Comma) {
                continue;
            }
            self.expect(&TokenKind::RParen, Error::ExpectedRParen)?;
            break;
        }
        Ok(Import {
            span: start.merge(self.last_span),
            path,
            names,
            all,
        })
    }

    fn parse_items(&mut self) -> Result<Vec<Item>, Diagnostic> {
        let mut items = Vec::new();
        loop {
            let doc = self.take_doc();
            self.skip_newlines();
            if self.is_eof() {
                break;
            }
            if self.is(TokenKind::Import) {
                if !doc.is_empty() {
                    return Err(self.error(Error::DocNotOnDeclaration));
                }
                return Err(self.error(Error::ImportAfterDeclaration));
            }
            let mut item = self.parse_item()?;
            match &mut item {
                Item::Data(data) => data.doc = doc,
                Item::Synonym(synonym) => synonym.doc = doc,
                Item::Def(def) => def.doc = doc,
                Item::Class(class) => class.doc = doc,
                Item::Instance(_) => {}
            }
            items.push(item);
        }
        Ok(items)
    }

    /// `..` lines with no blank line between them or before the next declaration.
    /// A blank line throws the block away. A normal `.` comment does not.
    fn take_doc(&mut self) -> String {
        while self.is(TokenKind::Newline) {
            self.bump();
        }
        let mut lines = Vec::new();
        while self.is(TokenKind::Doc(String::new())) {
            let TokenKind::Doc(text) = self.bump() else {
                break;
            };
            lines.push(text);
            let mut breaks = 0;
            while self.is(TokenKind::Newline) {
                breaks += 1;
                self.bump();
            }
            if self.is(TokenKind::Doc(String::new())) {
                if breaks >= 2 {
                    lines.clear();
                }
                continue;
            }
            if breaks >= 2 {
                return String::new();
            }
            break;
        }
        if lines.iter().all(|line| line.is_empty()) {
            String::new()
        } else {
            lines.join("\n")
        }
    }

    fn parse_item(&mut self) -> Result<Item, Diagnostic> {
        self.expect_at_column_0()?;
        if self.eat(&TokenKind::Data) {
            return self.parse_data().map(Item::Data);
        }
        if self.eat(&TokenKind::Type) {
            return self.parse_synonym().map(Item::Synonym);
        }
        if self.eat(&TokenKind::Class) {
            return self.parse_class().map(Item::Class);
        }
        if self.eat(&TokenKind::Instance) {
            return self.parse_instance().map(Item::Instance);
        }
        if self.is(TokenKind::Colon) {
            return self.parse_def().map(Item::Def);
        }
        Err(self.error(Error::ExpectedDeclaration))
    }

    fn parse_data(&mut self) -> Result<Data, Diagnostic> {
        let start = self.last_span;
        // `; foreign Name params` has no constructors. The value comes from JS.
        let foreign = self.eat(&TokenKind::Foreign);
        let name_span = self.cur().span;
        let name = self.expect_upper(Error::ExpectedTypeName)?;
        let mut params = Vec::new();
        while self.is_lower_ident() && !self.cur().line_start {
            params.push(self.expect_lower(Error::ExpectedTypeVariable)?);
        }
        if foreign {
            self.skip_newlines();
            if self.is_eof()
                || self.is_item_start()
                || self.is(TokenKind::Doc(String::new()))
            {
                return Ok(Data {
                    span: start.merge(self.last_span),
                    name_span,
                    name,
                    params,
                    ctors: Vec::new(),
                    foreign: true,
                    newtype: false,
                    doc: String::new(),
                });
            }
            return Err(self.error(Error::ForeignType));
        }
        self.skip_newlines();
        let newtype = if self.eat(&TokenKind::EqEq) {
            true
        } else {
            self.expect(&TokenKind::Eq, Error::ExpectedEq)?;
            false
        };
        let mut ctors = Vec::new();
        loop {
            self.skip_newlines();
            if self.is_eof() || self.is_item_start() {
                break;
            }
            ctors.push(self.parse_ctor()?);
            self.skip_newlines();
            if self.eat(&TokenKind::Pipe) {
                continue;
            }
            break;
        }
        if ctors.is_empty() {
            return Err(self.error(Error::ExpectedConstructor));
        }
        if newtype && ctors.len() != 1 {
            return Err(self.error_at(start, Error::NewtypeOneConstructor));
        }
        if newtype && ctors[0].fields.len() != 1 {
            return Err(self.error_at(ctors[0].span, Error::NewtypeOneField));
        }
        Ok(Data {
            span: start.merge(self.last_span),
            name_span,
            name,
            params,
            ctors,
            foreign: false,
            newtype,
            doc: String::new(),
        })
    }

    fn parse_synonym(&mut self) -> Result<Synonym, Diagnostic> {
        let start = self.last_span;
        let name_span = self.cur().span;
        let name = self.expect_upper(Error::ExpectedTypeName)?;
        let mut params = Vec::new();
        let mut seen = HashSet::new();
        while self.is_lower_ident() && !self.cur().line_start {
            let param = self.expect_lower(Error::ExpectedTypeVariable)?;
            if !seen.insert(param.clone()) {
                return Err(self.error_at(self.last_span, Error::DuplicateTypeVariable(param.to_string())));
            }
            params.push(param);
        }
        self.skip_newlines();
        let rhs = self.parse_type_item()?;
        Ok(Synonym {
            span: start.merge(self.last_span),
            name_span,
            name,
            params,
            rhs,
            doc: String::new(),
        })
    }

    fn parse_ctor(&mut self) -> Result<Ctor, Diagnostic> {
        let start = self.cur().span;
        let name = self.expect_upper(Error::ExpectedConstructor)?;
        if self.named_fields_ahead() {
            let fields = self.parse_named_ctor_fields()?;
            if fields.len() < 2 {
                return Err(self.error_at(start, Error::SingleFieldUnnamed));
            }
            return Ok(Ctor {
                span: start.merge(self.last_span),
                name,
                fields,
            });
        }
        let mut fields = Vec::new();
        if !self.stops_ctor_field() && self.starts_type_item() {
            fields.push(CtorField {
                name: None,
                ty: self.parse_type_item()?,
            });
            if !self.stops_ctor_field() && self.starts_type_item() {
                return Err(self.error_at(start, Error::SeveralFieldsNeedNames));
            }
        }
        Ok(Ctor {
            span: start.merge(self.last_span),
            name,
            fields,
        })
    }

    /// `frs : a, snd : b`. A newline may sit before the first name and after a comma.
    fn parse_named_ctor_fields(&mut self) -> Result<Vec<CtorField>, Diagnostic> {
        let mut fields: Vec<CtorField> = Vec::new();
        loop {
            self.skip_newlines();
            if !self.named_field_here() {
                break;
            }
            let name_span = self.cur().span;
            let name = self.expect_lower(Error::ExpectedFieldName)?;
            if fields.iter().any(|field| field.name.as_deref() == Some(name.as_str())) {
                return Err(self.error_at(name_span, Error::DuplicateFieldName));
            }
            self.expect(&TokenKind::Colon, Error::ExpectedColon)?;
            let ty = self.parse_type_item()?;
            fields.push(CtorField { name: Some(name), ty });
            self.skip_newlines();
            if self.eat(&TokenKind::Comma) {
                continue;
            }
            if self.named_field_here() {
                continue;
            }
            break;
        }
        Ok(fields)
    }

    fn named_field_here(&self) -> bool {
        self.is_lower_ident()
            && self
                .tokens
                .get(self.pos + 1)
                .is_some_and(|token| matches!(token.kind, TokenKind::Colon))
    }

    /// A named field may start on the next line. A positional field may not.
    fn named_fields_ahead(&self) -> bool {
        let mut index = self.pos;
        loop {
            let Some(token) = self.tokens.get(index) else {
                return false;
            };
            if matches!(token.kind, TokenKind::Newline) {
                index += 1;
                continue;
            }
            let TokenKind::Ident(name) = &token.kind else {
                return false;
            };
            if !is_lower(name) {
                return false;
            }
            return matches!(
                self.tokens.get(index + 1).map(|token| &token.kind),
                Some(TokenKind::Colon)
            );
        }
    }

    fn stops_ctor_field(&self) -> bool {
        if self.is_eof() || self.is(TokenKind::Pipe) || self.is_item_start() {
            return true;
        }
        if self.group_depth == 0 && (self.is(TokenKind::Newline) || self.cur().line_start) {
            return true;
        }
        false
    }

    fn parse_def(&mut self) -> Result<Def, Diagnostic> {
        self.skip_newlines();
        let start = self.cur().span;
        self.expect(&TokenKind::Colon, Error::ExpectedColon)?;
        // `foreign` and `private` sit after `:` and before the name.
        // Each once, either order. A repeat is not a name.
        let mut vis = Vis::PUBLIC;
        loop {
            if !vis.foreign && self.eat(&TokenKind::Foreign) {
                vis.foreign = true;
            } else if !vis.private && self.eat(&TokenKind::Private) {
                vis.private = true;
            } else {
                break;
            }
        }
        let name_span = self.cur().span;
        let name = self.expect_ident(Error::ExpectedDefinitionName)?;
        self.expect(&TokenKind::LParen, Error::ExpectedLParen)?;
        self.group_depth += 1;
        let scheme = self.parse_scheme()?;
        self.expect(&TokenKind::RParen, Error::ExpectedRParen)?;
        self.group_depth -= 1;
        // Words after `)` on this line are the body. Equations start on the next line.
        let same_line = self.parse_same_line_terms()?;
        let (body, clauses) = if vis.is_foreign() {
            if !same_line.is_empty() {
                return Err(self.error(Error::ForeignHasBody));
            }
            self.reject_following_body(Error::ForeignHasBody)?;
            (Vec::new(), Vec::new())
        } else if self.mode == ParseMode::Interface {
            if !same_line.is_empty() {
                return Err(self.error(Error::InterfaceHasBody));
            }
            self.reject_following_body(Error::InterfaceHasBody)?;
            (Vec::new(), Vec::new())
        } else if !same_line.is_empty() {
            let mut body = same_line;
            body.extend(self.parse_body()?);
            (body, Vec::new())
        // A first body line of patterns and `->` is equations, one bare name included.
        } else if self.clause_block_starts() {
            let clauses = self.parse_clauses()?;
            if clauses.is_empty() {
                return Err(self.error_at(name_span, Error::MissingBody));
            }
            (Vec::new(), clauses)
        } else {
            let body = self.parse_body()?;
            if body.is_empty() {
                return Err(self.error_at(name_span, Error::MissingBody));
            }
            (body, Vec::new())
        };
        Ok(Def {
            span: start.merge(self.last_span),
            name_span,
            vis,
            name,
            scheme,
            body,
            clauses,
            doc: String::new(),
        })
    }

    /// Terms after the scheme on the declaration line. `->` here is not an equation.
    fn parse_same_line_terms(&mut self) -> Result<Vec<Term>, Diagnostic> {
        let mut terms = Vec::new();
        loop {
            if self.is_eof() || self.is(TokenKind::Newline) || self.cur().line_start {
                break;
            }
            if self.is(TokenKind::Arrow) {
                return Err(self.error(Error::ExpectedTerm));
            }
            if !self.starts_term() {
                break;
            }
            terms.push(self.parse_term()?);
        }
        Ok(terms)
    }

    /// The first body line is an equation header: one or more patterns, then `->`.
    /// One bare lowercase name counts: `n -> n`.
    fn clause_block_starts(&self) -> bool {
        let mut index = self.skip_newline_index(self.pos);
        if self.tokens.get(index).is_none_or(|token| {
            matches!(token.kind, TokenKind::Eof) || self.item_start_at(index)
        }) {
            return false;
        }
        let mut count = 0;
        loop {
            let Some(token) = self.tokens.get(index) else {
                return false;
            };
            if matches!(token.kind, TokenKind::Eof | TokenKind::Newline) {
                return false;
            }
            if matches!(token.kind, TokenKind::Arrow) {
                return count > 0 && !token.line_start;
            }
            let Some(next) = self.skip_one_pattern(index) else {
                // An unclosed `<` or `{` is still a pattern. The pattern parser
                // reports the missing closer; a term would blame the binder.
                return matches!(token.kind, TokenKind::LBrace | TokenKind::LAngle);
            };
            count += 1;
            index = next;
        }
    }

    fn skip_newline_index(&self, mut index: usize) -> usize {
        while matches!(
            self.tokens.get(index).map(|token| &token.kind),
            Some(TokenKind::Newline)
        ) {
            index += 1;
        }
        index
    }

    fn item_start_at(&self, index: usize) -> bool {
        let Some(token) = self.tokens.get(index) else {
            return false;
        };
        if token.col != 0 || !token.line_start {
            return false;
        }
        if matches!(
            token.kind,
            TokenKind::Data
                | TokenKind::Type
                | TokenKind::Class
                | TokenKind::Instance
                | TokenKind::Colon
        ) {
            return true;
        }
        false
    }

    /// One pattern, without consuming.
    fn skip_one_pattern(&self, index: usize) -> Option<usize> {
        let token = self.tokens.get(index)?;
        match &token.kind {
            TokenKind::Ident(name) if name == "_" || is_lower(name) || is_upper(name) => {
                Some(index + 1)
            }
            TokenKind::True | TokenKind::False | TokenKind::Int(_) | TokenKind::Number(_) | TokenKind::String(_) => {
                Some(index + 1)
            }
            TokenKind::LParen => self.skip_balanced(index, &TokenKind::LParen, &TokenKind::RParen),
            TokenKind::LAngle => self.skip_balanced(index, &TokenKind::LAngle, &TokenKind::RAngle),
            TokenKind::LBrace => self.skip_balanced(index, &TokenKind::LBrace, &TokenKind::RBrace),
            _ => None,
        }
    }

    /// From the opener at `index`, the token after the matching closer.
    fn skip_balanced(&self, index: usize, open: &TokenKind, close: &TokenKind) -> Option<usize> {
        let mut depth = 0u32;
        let mut cursor = index;
        while cursor < self.tokens.len() {
            let kind = &self.tokens[cursor].kind;
            if kind == open {
                depth += 1;
            } else if kind == close {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return Some(cursor + 1);
                }
            } else if matches!(kind, TokenKind::Eof) {
                return None;
            }
            cursor += 1;
        }
        None
    }

    fn parse_clauses(&mut self) -> Result<Vec<Clause>, Diagnostic> {
        self.skip_newlines();
        let column = self.cur().col;
        let mut clauses = Vec::new();
        loop {
            self.skip_newlines();
            if self.is_eof() || self.is_item_start() {
                break;
            }
            let token = self.cur();
            if !clauses.is_empty() && token.line_start && token.col != column {
                break;
            }
            clauses.push(self.parse_clause(column)?);
        }
        Ok(clauses)
    }

    fn parse_clause(&mut self, column: u32) -> Result<Clause, Diagnostic> {
        let start = self.cur().span;
        let mut patterns = Vec::new();
        loop {
            if self.is(TokenKind::Arrow) {
                break;
            }
            if self.is(TokenKind::Newline) || (!patterns.is_empty() && self.cur().line_start) {
                return Err(self.error(Error::ExpectedArrow));
            }
            if !self.starts_pattern() {
                return Err(self.error(Error::ExpectedPattern));
            }
            patterns.push(self.parse_slot_pattern()?);
        }
        if patterns.is_empty() {
            return Err(self.error(Error::ExpectedPattern));
        }
        self.expect(&TokenKind::Arrow, Error::ExpectedArrow)?;
        let body = self.parse_arm_body(column)?;
        let end = body.last().map(|term| term.span).unwrap_or(self.last_span);
        Ok(Clause {
            span: start.merge(end),
            patterns,
            body,
        })
    }

    fn parse_slot_pattern(&mut self) -> Result<Pattern, Diagnostic> {
        match self.cur().kind.clone() {
            TokenKind::Ident(name) if name == "_" => {
                self.bump();
                Ok(Pattern::Wild)
            }
            TokenKind::Ident(name) if is_lower(&name) => {
                self.bump();
                Ok(Pattern::Bind(name))
            }
            TokenKind::Ident(name) if is_upper(&name) => {
                self.bump();
                Ok(Pattern::Unpack(name))
            }
            TokenKind::True => {
                self.bump();
                Ok(Pattern::Lit(Lit::Bool(true)))
            }
            TokenKind::False => {
                self.bump();
                Ok(Pattern::Lit(Lit::Bool(false)))
            }
            TokenKind::Int(lexeme) => {
                self.bump();
                Ok(Pattern::Lit(Lit::Int(lexeme)))
            }
            TokenKind::Number(lexeme) => {
                self.bump();
                Ok(Pattern::Lit(Lit::Number(lexeme)))
            }
            TokenKind::String(value) => {
                self.bump();
                Ok(Pattern::Lit(Lit::String(value)))
            }
            TokenKind::LParen => self.parse_paren_pattern(),
            TokenKind::LAngle => self.parse_array_pattern(),
            TokenKind::LBrace => self.parse_record_pattern(),
            _ => Err(self.error(Error::ExpectedPattern)),
        }
    }

    /// `{ name : n }` is one slot. The right of `:` is a pattern.
    /// `{}` binds nothing. `|` is not a tail here.
    fn parse_record_pattern(&mut self) -> Result<Pattern, Diagnostic> {
        self.bump();
        self.group_depth += 1;
        let mut fields = Vec::new();
        let mut seen = HashSet::new();
        loop {
            self.skip_newlines();
            if self.is(TokenKind::RBrace) {
                break;
            }
            if self.is(TokenKind::Pipe) {
                return Err(self.error(Error::ExpectedFieldName));
            }
            if self.is_eof() {
                return Err(self.error(Error::ExpectedRBrace));
            }
            let name_span = self.cur().span;
            let name = self.expect_ident(Error::ExpectedFieldName)?;
            if !seen.insert(name.clone()) {
                return Err(self.error_at(name_span, Error::DuplicateFieldName));
            }
            self.expect(&TokenKind::Colon, Error::ExpectedColon)?;
            self.skip_newlines();
            if !self.starts_pattern() {
                return Err(self.error(Error::ExpectedPattern));
            }
            fields.push((name, self.parse_slot_pattern()?));
            self.skip_newlines();
            if self.is(TokenKind::Pipe) {
                return Err(self.error(Error::ExpectedFieldName));
            }
            if self.eat(&TokenKind::Comma) {
                continue;
            }
            break;
        }
        self.skip_newlines();
        if self.is(TokenKind::Pipe) {
            return Err(self.error(Error::ExpectedFieldName));
        }
        self.expect(&TokenKind::RBrace, Error::ExpectedRBrace)?;
        self.group_depth -= 1;
        Ok(Pattern::Record(fields))
    }

    /// `< 0n 1n a b _ >` is one slot of exactly that length. `<>` is empty.
    /// Index 0 is the left element. `|` is not a tail.
    fn parse_array_pattern(&mut self) -> Result<Pattern, Diagnostic> {
        let start = self.cur().span;
        self.bump();
        self.group_depth += 1;
        let mut elements = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RAngle) {
                self.group_depth -= 1;
                break;
            }
            if self.is_eof() {
                return Err(self.error_at(start, Error::UnterminatedArray));
            }
            if !self.starts_pattern() {
                return Err(self.error(Error::ExpectedPattern));
            }
            elements.push(self.parse_slot_pattern()?);
        }
        Ok(Pattern::Array(elements))
    }

    /// `(Just x)` is one slot. A bare name inside is a field pattern, so
    /// `(Just Just)` unpacks the inner constructor and `(Just)` has no fields.
    fn parse_paren_pattern(&mut self) -> Result<Pattern, Diagnostic> {
        self.bump();
        self.group_depth += 1;
        self.skip_newlines();
        if !self.is_upper_ident() {
            self.group_depth -= 1;
            return Err(self.error(Error::ExpectedConstructor));
        }
        let name = self.expect_upper(Error::ExpectedConstructor)?;
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RParen) {
                self.group_depth -= 1;
                break;
            }
            if self.is_eof() {
                return Err(self.error(Error::ExpectedRParen));
            }
            if !self.starts_pattern() {
                return Err(self.error(Error::ExpectedPattern));
            }
            fields.push(self.parse_slot_pattern()?);
        }
        Ok(Pattern::Ctor(name, fields))
    }

    fn starts_pattern(&self) -> bool {
        match &self.cur().kind {
            TokenKind::True
            | TokenKind::False
            | TokenKind::Int(_)
            | TokenKind::Number(_)
            | TokenKind::String(_)
            | TokenKind::LParen
            | TokenKind::LAngle
            | TokenKind::LBrace => true,
            TokenKind::Ident(name) => name == "_" || is_lower(name) || is_upper(name),
            _ => false,
        }
    }

    fn parse_class(&mut self) -> Result<Class, Diagnostic> {
        let start = self.last_span;
        // `& Comparable a, Show a => Sortable a`, or `& Sortable a` when there is no superclass.
        let first_span = self.cur().span;
        let first_name = self.expect_upper(Error::ExpectedClassName)?;
        let first_args = self.parse_class_args()?;
        self.skip_newlines();
        let (name, name_span, params, supers) = if self.is(TokenKind::Comma) || self.is(TokenKind::FatArrow)
        {
            if first_args.is_empty() {
                return Err(self.error(Error::ExpectedClassArgument));
            }
            let mut supers = vec![Constraint {
                class_name: first_name,
                args: first_args,
            }];
            while self.eat(&TokenKind::Comma) {
                self.skip_newlines();
                let class_name = self.expect_upper(Error::ExpectedClassName)?;
                let args = self.parse_class_args()?;
                if args.is_empty() {
                    return Err(self.error(Error::ExpectedClassArgument));
                }
                supers.push(Constraint { class_name, args });
                self.skip_newlines();
            }
            self.expect(&TokenKind::FatArrow, Error::ExpectedFatArrow)?;
            self.skip_newlines();
            let name_span = self.cur().span;
            let name = self.expect_upper(Error::ExpectedClassName)?;
            let params = self.parse_class_params()?;
            (name, name_span, params, supers)
        } else {
            (first_name, first_span, self.params_from_args(first_args)?, Vec::new())
        };
        let mut methods = Vec::new();
        loop {
            self.skip_newlines();
            if self.stops_method_list() {
                break;
            }
            let name_span = self.cur().span;
            let method = self.expect_ident(Error::ExpectedMethodName)?;
            self.expect(&TokenKind::LParen, Error::ExpectedLParen)?;
            self.group_depth += 1;
            let scheme = self.parse_scheme()?;
            self.expect(&TokenKind::RParen, Error::ExpectedRParen)?;
            self.group_depth -= 1;
            methods.push(MethodSig {
                name: method,
                name_span,
                scheme,
            });
        }
        Ok(Class {
            span: start.merge(self.last_span),
            name_span,
            name,
            supers,
            params,
            methods,
            doc: String::new(),
        })
    }

    /// A newline ends the header. The method on the next line is not another argument.
    /// A grouped type still wraps its own lines inside `parse_type_item`.
    fn parse_class_args(&mut self) -> Result<Vec<Type>, Diagnostic> {
        let mut args = Vec::new();
        loop {
            if self.stops_class_arg() {
                break;
            }
            args.push(self.parse_type_item()?);
        }
        Ok(args)
    }

    fn stops_class_arg(&self) -> bool {
        self.is(TokenKind::Comma)
            || self.is(TokenKind::FatArrow)
            || self.is(TokenKind::Newline)
            || self.is_eof()
            || !self.starts_type_item()
    }

    fn parse_class_params(&mut self) -> Result<Vec<String>, Diagnostic> {
        let mut params = Vec::new();
        while self.is_lower_ident() && !self.cur().line_start {
            params.push(self.expect_lower(Error::ExpectedTypeVariable)?);
        }
        Ok(params)
    }

    fn params_from_args(&self, args: Vec<Type>) -> Result<Vec<String>, Diagnostic> {
        let mut params = Vec::new();
        for arg in args {
            match arg {
                Type::Var(name) => params.push(name),
                _ => return Err(self.error_at(self.last_span, Error::ExpectedTypeVariable)),
            }
        }
        Ok(params)
    }

    fn parse_instance(&mut self) -> Result<Instance, Diagnostic> {
        let start = self.last_span;
        let mut class_names = vec![self.expect_upper(Error::ExpectedClassName)?];
        while self.eat(&TokenKind::Comma) {
            self.skip_newlines();
            class_names.push(self.expect_upper(Error::ExpectedClassName)?);
        }
        let mut args = Vec::new();
        while !self.is(TokenKind::Newline) && !self.is_eof() {
            if self.starts_type_item() {
                args.push(self.parse_type_item()?);
            } else {
                break;
            }
        }
        self.skip_newlines();
        let mut methods = Vec::new();
        loop {
            self.skip_newlines();
            if self.stops_method_list() {
                break;
            }
            let name_span = self.cur().span;
            let name = self.expect_ident(Error::ExpectedMethodName)?;
            self.expect(&TokenKind::Eq, Error::ExpectedEq)?;
            let word = self.expect_ident(Error::ExpectedWord)?;
            methods.push(MethodBind {
                name,
                name_span,
                word,
            });
        }
        Ok(Instance {
            span: start.merge(self.last_span),
            class_names,
            args,
            methods,
        })
    }

    fn parse_scheme(&mut self) -> Result<Scheme, Diagnostic> {
        self.skip_newlines();
        let mut vars = Vec::new();
        if self.binder_ahead() {
            loop {
                vars.push(self.expect_lower(Error::ExpectedTypeVariable)?);
                self.skip_newlines();
                if self.eat(&TokenKind::Dot) {
                    break;
                }
                if !self.is_lower_ident() {
                    return Err(self.error(Error::ExpectedDot));
                }
            }
        }
        let mut constraints = Vec::new();
        loop {
            self.skip_newlines();
            if !self.constraint_ahead() {
                break;
            }
            loop {
                let class_name = self.expect_upper(Error::ExpectedClassName)?;
                let mut args = Vec::new();
                loop {
                    self.skip_newlines();
                    if self.is(TokenKind::FatArrow) || self.is(TokenKind::Comma) {
                        break;
                    }
                    args.push(self.parse_type_item()?);
                }
                if args.is_empty() {
                    return Err(self.error(Error::ExpectedClassArgument));
                }
                constraints.push(Constraint { class_name, args });
                if self.eat(&TokenKind::Comma) {
                    self.skip_newlines();
                    continue;
                }
                break;
            }
            self.expect(&TokenKind::FatArrow, Error::ExpectedFatArrow)?;
        }
        let input = self.parse_input_stack()?;
        self.expect(&TokenKind::StackArrow, Error::ExpectedStackArrow)?;
        let output = self.parse_output_stack()?;
        Ok(Scheme {
            vars,
            constraints,
            effect: StackEffect {
                input,
                output,
                effects: None,
            },
        })
    }

    /// `a b.` at the start of a scheme. A bare `.` is the empty stack tail.
    fn binder_ahead(&self) -> bool {
        let mut index = self.pos;
        let mut saw_var = false;
        while index < self.tokens.len() {
            match &self.tokens[index].kind {
                TokenKind::Newline => index += 1,
                TokenKind::Ident(name) if is_lower(name) => {
                    saw_var = true;
                    index += 1;
                }
                TokenKind::Dot if saw_var => return true,
                _ => return false,
            }
        }
        false
    }

    fn constraint_ahead(&self) -> bool {
        let mut index = self.pos;
        let mut depth = self.group_depth;
        while index < self.tokens.len() {
            match &self.tokens[index].kind {
                TokenKind::Eof => return false,
                TokenKind::FatArrow if depth == self.group_depth => return true,
                TokenKind::StackArrow if depth == self.group_depth => return false,
                TokenKind::LParen | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBrace => {
                    if depth == self.group_depth {
                        return false;
                    }
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
            index += 1;
        }
        false
    }

    fn parse_input_stack(&mut self) -> Result<Stack, Diagnostic> {
        self.skip_newlines();
        let tail = self.parse_tail()?;
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            if self.is_tail_token() {
                return Err(self.error(Error::StackTailNotBottom));
            }
            if !self.starts_type_item() {
                break;
            }
            items.push(self.parse_type_item()?);
        }
        Ok(Stack { tail, items })
    }

    fn parse_output_stack(&mut self) -> Result<Stack, Diagnostic> {
        if self.group_depth > 0 {
            self.skip_newlines();
        }
        if self.output_stops() {
            return Ok(Stack {
                tail: StackTail::Implicit,
                items: Vec::new(),
            });
        }
        let tail = self.parse_tail()?;
        let mut items = Vec::new();
        loop {
            if self.group_depth > 0 {
                self.skip_newlines();
            }
            if self.output_stops() {
                break;
            }
            if self.is_tail_token() {
                return Err(self.error(Error::StackTailNotBottom));
            }
            if !self.starts_type_item() {
                break;
            }
            items.push(self.parse_type_item()?);
        }
        Ok(Stack { tail, items })
    }

    fn output_stops(&self) -> bool {
        if self.is_eof() {
            return true;
        }
        if self.group_depth == 0 && self.is(TokenKind::Newline) {
            return true;
        }
        if self.group_depth > 0 && (self.is(TokenKind::RParen) || self.is(TokenKind::RBrace)) {
            return true;
        }
        false
    }

    fn parse_tail(&mut self) -> Result<StackTail, Diagnostic> {
        if self.group_depth > 0 {
            self.skip_newlines();
        }
        if self.eat(&TokenKind::Dot) {
            return Ok(StackTail::Empty);
        }
        if let TokenKind::Tail(name) = &self.cur().kind {
            let name = name.clone();
            self.bump();
            return Ok(StackTail::Var(name));
        }
        Ok(StackTail::Implicit)
    }

    /// One stack slot. A name does not take arguments; application is `(Maybe a)`.
    fn parse_type_item(&mut self) -> Result<Type, Diagnostic> {
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Result<Type, Diagnostic> {
        self.skip_grouped_newlines();
        if self.is_lower_ident() {
            return Ok(Type::Var(self.expect_lower(Error::ExpectedType)?));
        }
        if self.is_upper_ident() {
            return Ok(Type::Ctor(self.expect_upper(Error::ExpectedType)?));
        }
        if self.eat_group(&TokenKind::LParen) {
            return self.parse_paren();
        }
        if self.eat_group(&TokenKind::LBrace) {
            return Ok(Type::Record(self.parse_record_body()?));
        }
        Err(self.error(Error::ExpectedType))
    }

    fn parse_paren(&mut self) -> Result<Type, Diagnostic> {
        if self.stack_effect_ahead() {
            let input = self.parse_input_stack()?;
            self.expect(&TokenKind::StackArrow, Error::ExpectedStackArrow)?;
            let output = self.parse_output_stack()?;
            self.expect(&TokenKind::RParen, Error::ExpectedRParen)?;
            self.group_depth -= 1;
            return Ok(Type::Fun(Box::new(StackEffect {
                input,
                output,
                effects: None,
            })));
        }
        self.skip_newlines();
        if self.is(TokenKind::RParen) || self.is_tail_token() || !self.starts_type_item() {
            return Err(self.error(Error::ExpectedStackArrow));
        }
        let head = self.parse_type_item()?;
        let mut args = Vec::new();
        loop {
            self.skip_newlines();
            if self.is(TokenKind::RParen) || self.is_eof() {
                break;
            }
            if self.is_tail_token() {
                return Err(self.error(Error::ExpectedStackArrow));
            }
            if !self.starts_atom() {
                break;
            }
            args.push(self.parse_atom()?);
        }
        self.expect(&TokenKind::RParen, Error::ExpectedRParen)?;
        self.group_depth -= 1;
        Ok(apply(head, args))
    }

    /// True when `--` appears at the current parenthesis depth before its `)`.
    fn stack_effect_ahead(&self) -> bool {
        let mut index = self.pos;
        let mut depth = self.group_depth;
        while index < self.tokens.len() {
            match &self.tokens[index].kind {
                TokenKind::Eof => return false,
                TokenKind::StackArrow if depth == self.group_depth => return true,
                TokenKind::RParen | TokenKind::RBrace if depth == self.group_depth => return false,
                TokenKind::LParen | TokenKind::LBrace => depth += 1,
                TokenKind::RParen | TokenKind::RBrace => depth = depth.saturating_sub(1),
                _ => {}
            }
            index += 1;
        }
        false
    }

    fn parse_record_body(&mut self) -> Result<RecordRow, Diagnostic> {
        // `{` already consumed and group_depth incremented.
        let mut fields = Vec::new();
        let mut tail = None;
        loop {
            self.skip_newlines();
            if self.is(TokenKind::RBrace) {
                break;
            }
            if self.eat(&TokenKind::Pipe) {
                tail = Some(self.parse_record_tail()?);
                break;
            }
            let name = self.expect_ident(Error::ExpectedFieldName)?;
            self.expect(&TokenKind::Colon, Error::ExpectedColon)?;
            let ty = self.parse_type_item()?;
            fields.push((name, ty));
            self.skip_newlines();
            if self.eat(&TokenKind::Comma) {
                continue;
            }
            break;
        }
        self.skip_newlines();
        if tail.is_none() && self.eat(&TokenKind::Pipe) {
            tail = Some(self.parse_record_tail()?);
            self.skip_newlines();
        }
        self.expect(&TokenKind::RBrace, Error::ExpectedRBrace)?;
        self.group_depth -= 1;
        Ok(RecordRow { fields, tail })
    }

    /// `| r` is a row variable. `| Person`, `| (Person a)`, and `| { ... }` splice a type.
    /// A bare lowercase name is never a type. One tail: the next token is `}`.
    fn parse_record_tail(&mut self) -> Result<RecordTail, Diagnostic> {
        self.skip_newlines();
        if self.is_lower_ident() {
            return Ok(RecordTail::Var(self.expect_lower(Error::ExpectedRowVariable)?));
        }
        if self.starts_type_item() {
            return Ok(RecordTail::Type(Box::new(self.parse_type_item()?)));
        }
        Err(self.error(Error::ExpectedRowVariable))
    }

    fn parse_body(&mut self) -> Result<Vec<Term>, Diagnostic> {
        let mut terms = Vec::new();
        loop {
            self.skip_newlines();
            if self.is_eof() || self.is_item_start() || !self.starts_term() {
                break;
            }
            terms.push(self.parse_term()?);
        }
        if self.is(TokenKind::Arrow) {
            return Err(self.error(Error::ExpectedTerm));
        }
        Ok(terms)
    }

    fn reject_following_body(&mut self, error: Error) -> Result<(), Diagnostic> {
        let checkpoint = self.pos;
        self.skip_newlines();
        if self.starts_term() && !self.is_item_start() {
            return Err(self.error(error));
        }
        self.pos = checkpoint;
        Ok(())
    }

    fn parse_term(&mut self) -> Result<Term, Diagnostic> {
        let start = self.cur().span;
        let kind = self.cur().kind.clone();
        match kind {
            TokenKind::LBrack => {
                self.bump();
                self.parse_quote(start)
            }
            TokenKind::LAngle => {
                self.bump();
                self.group_depth += 1;
                self.parse_array(start)
            }
            TokenKind::Field(name) => {
                self.bump();
                Ok(self.term(start, TermNode::Field(name)))
            }
            TokenKind::FieldSet(name) => {
                self.bump();
                Ok(self.term(start, TermNode::FieldSet(name)))
            }
            TokenKind::FieldInsert(name) => {
                self.bump();
                Ok(self.term(start, TermNode::FieldInsert(name)))
            }
            TokenKind::LBrace => {
                self.bump();
                self.group_depth += 1;
                self.parse_record_literal(start)
            }
            TokenKind::True => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::Bool(true))))
            }
            TokenKind::False => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::Bool(false))))
            }
            TokenKind::Int(lexeme) => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::Int(lexeme))))
            }
            TokenKind::Number(lexeme) => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::Number(lexeme))))
            }
            TokenKind::String(value) => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::String(value))))
            }
            TokenKind::Ident(name) => {
                self.bump();
                Ok(self.term(start, TermNode::Word(name)))
            }
            _ => Err(self.error(Error::ExpectedTerm)),
        }
    }

    /// `<` is already consumed and `group_depth` includes it.
    /// `< 1 2 3 >` is an array. `<>` is empty. A word inside is `expected element`.
    fn parse_array(&mut self, start: Span) -> Result<Term, Diagnostic> {
        let mut elements = Vec::new();
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RAngle) {
                break;
            }
            if self.is_eof() {
                return Err(self.error_at(start, Error::UnterminatedArray));
            }
            elements.push(self.parse_element()?);
        }
        self.group_depth -= 1;
        Ok(self.term(start.merge(self.last_span), TermNode::Array(elements)))
    }

    /// `{` is already consumed and `group_depth` includes it.
    /// `{ name: "Ada" }` is a record. `{}` is an empty record, not an array.
    fn parse_record_literal(&mut self, start: Span) -> Result<Term, Diagnostic> {
        let mut fields = Vec::new();
        let mut seen = HashSet::new();
        loop {
            self.skip_newlines();
            if self.is(TokenKind::RBrace) {
                break;
            }
            let name_span = self.cur().span;
            let name = self.expect_ident(Error::ExpectedFieldName)?;
            if !seen.insert(name.clone()) {
                return Err(self.error_at(name_span, Error::DuplicateFieldName));
            }
            self.expect(&TokenKind::Colon, Error::ExpectedColon)?;
            let value = self.parse_element()?;
            fields.push((name, value));
            self.skip_newlines();
            if self.eat(&TokenKind::Comma) {
                continue;
            }
            break;
        }
        self.skip_newlines();
        self.expect(&TokenKind::RBrace, Error::ExpectedRBrace)?;
        self.group_depth -= 1;
        Ok(self.term(start.merge(self.last_span), TermNode::Record(fields)))
    }

    /// A literal, a quotation, an array, or a record. Not a word and not a stack program.
    fn parse_element(&mut self) -> Result<Term, Diagnostic> {
        self.skip_newlines();
        let start = self.cur().span;
        match self.cur().kind.clone() {
            TokenKind::True => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::Bool(true))))
            }
            TokenKind::False => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::Bool(false))))
            }
            TokenKind::Int(lexeme) => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::Int(lexeme))))
            }
            TokenKind::Number(lexeme) => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::Number(lexeme))))
            }
            TokenKind::String(value) => {
                self.bump();
                Ok(self.term(start, TermNode::Lit(Lit::String(value))))
            }
            TokenKind::LBrack => {
                self.bump();
                self.parse_quote(start)
            }
            TokenKind::LAngle => {
                self.bump();
                self.group_depth += 1;
                self.parse_array(start)
            }
            TokenKind::LBrace => {
                self.bump();
                self.group_depth += 1;
                self.parse_record_literal(start)
            }
            _ => Err(self.error(Error::ExpectedElement)),
        }
    }

    fn parse_quote(&mut self, start: Span) -> Result<Term, Diagnostic> {
        let mut terms = Vec::new();
        // A quotation opens like one equation arm: `acc f ->`. One bare name counts.
        self.skip_newlines();
        if self.quote_bind_starts() {
            terms.push(self.parse_quote_bind()?);
        }
        loop {
            self.skip_newlines();
            if self.eat(&TokenKind::RBrack) {
                break;
            }
            if self.is_eof() {
                return Err(self.error_at(start, Error::UnterminatedQuotation));
            }
            if !self.starts_term() {
                return Err(self.error(Error::ExpectedTerm));
            }
            terms.push(self.parse_term()?);
        }
        Ok(self.term(start.merge(self.last_span), TermNode::Quote(terms)))
    }

    /// Names and `->` on the first line of a quotation, before any other term.
    fn quote_bind_starts(&self) -> bool {
        let mut index = self.pos;
        let mut count = 0;
        loop {
            let Some(token) = self.tokens.get(index) else {
                return false;
            };
            match &token.kind {
                TokenKind::Eof | TokenKind::Newline => return false,
                TokenKind::Arrow => return count > 0 && !token.line_start,
                TokenKind::Ident(name) if is_lower(name) => {
                    if count > 0 && token.line_start {
                        return false;
                    }
                    count += 1;
                    index += 1;
                }
                _ => return false,
            }
        }
    }

    /// `acc f ->` at the opening of a quotation. Left is deep, right is top.
    fn parse_quote_bind(&mut self) -> Result<Term, Diagnostic> {
        let start = self.cur().span;
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        while self.is_lower_ident() && (names.is_empty() || !self.cur().line_start) {
            let name = self.expect_ident(Error::ExpectedName)?;
            if !seen.insert(name.clone()) {
                return Err(self.error_at(self.last_span, Error::DuplicateDefinition(name.to_string())));
            }
            names.push(name);
        }
        self.expect(&TokenKind::Arrow, Error::ExpectedArrow)?;
        Ok(self.term(start.merge(self.last_span), TermNode::Bind(names)))
    }

    fn parse_arm_body(&mut self, arm_col: u32) -> Result<Vec<Term>, Diagnostic> {
        let mut terms = Vec::new();
        loop {
            if self.is(TokenKind::Newline) || self.is_eof() {
                let checkpoint = self.pos;
                self.skip_newlines();
                if self.is_eof() || self.is(TokenKind::RBrack) {
                    break;
                }
                let token = self.cur();
                if token.line_start && token.col <= arm_col {
                    break;
                }
                if !self.starts_term() {
                    if self.is(TokenKind::Arrow) {
                        return Err(self.error(Error::ExpectedTerm));
                    }
                    self.pos = checkpoint;
                    break;
                }
                continue;
            }
            if self.cur().line_start && self.cur().col <= arm_col {
                break;
            }
            if self.starts_term() {
                terms.push(self.parse_term()?);
            } else if self.is(TokenKind::Arrow) {
                return Err(self.error(Error::ExpectedTerm));
            } else {
                break;
            }
        }
        Ok(terms)
    }

    fn term(&self, span: Span, node: TermNode) -> Term {
        Term { span, node }
    }

    fn cur(&self) -> &crate::token::Token {
        &self.tokens[self.pos]
    }

    fn bump(&mut self) -> TokenKind {
        let token = self.cur().clone();
        self.last_span = token.span;
        if !matches!(token.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        token.kind
    }

    fn is(&self, kind: TokenKind) -> bool {
        std::mem::discriminant(&self.cur().kind) == std::mem::discriminant(&kind)
    }

    fn is_eof(&self) -> bool {
        matches!(self.cur().kind, TokenKind::Eof)
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.is(kind.clone()) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn eat_group(&mut self, kind: &TokenKind) -> bool {
        if self.eat(kind) {
            self.group_depth += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, error: Error) -> Result<(), Diagnostic> {
        if self.eat(kind) {
            Ok(())
        } else {
            Err(self.error(error))
        }
    }

    fn expect_at_column_0(&self) -> Result<(), Diagnostic> {
        let token = self.cur();
        if token.col == 0 && token.line_start {
            Ok(())
        } else {
            Err(self.error(Error::DeclarationColumn))
        }
    }

    fn is_item_start(&self) -> bool {
        self.item_start_at(self.pos)
    }

    /// A column-0 `..` documents the next declaration, so it is not a method name.
    fn stops_method_list(&self) -> bool {
        self.is_eof() || self.is_item_start() || self.is(TokenKind::Doc(String::new()))
    }

    fn skip_newlines(&mut self) {
        while self.is(TokenKind::Newline) {
            self.bump();
        }
    }

    fn skip_grouped_newlines(&mut self) {
        if self.group_depth > 0 {
            self.skip_newlines();
        }
    }

    fn error(&self, error: Error) -> Diagnostic {
        self.error_at(self.cur().span, error)
    }

    fn error_at(&self, span: Span, error: Error) -> Diagnostic {
        Diagnostic::new(span, error)
    }

    fn ident_text(&self) -> Option<&str> {
        match &self.cur().kind {
            TokenKind::Ident(name) => Some(name),
            _ => None,
        }
    }

    fn is_lower_ident(&self) -> bool {
        self.ident_text().is_some_and(|name| is_lower(name))
    }

    fn is_upper_ident(&self) -> bool {
        self.ident_text().is_some_and(|name| is_upper(name))
    }

    fn expect_ident(&mut self, error: Error) -> Result<String, Diagnostic> {
        let Some(name) = self.ident_text().map(str::to_string) else {
            return Err(self.error(error));
        };
        self.bump();
        Ok(name)
    }

    fn expect_upper(&mut self, error: Error) -> Result<String, Diagnostic> {
        if self.ident_text().is_some_and(|name| !is_upper(name)) {
            return Err(self.error(Error::TypeConstructorCase));
        }
        self.expect_ident(error)
    }

    fn expect_lower(&mut self, error: Error) -> Result<String, Diagnostic> {
        if self.ident_text().is_some_and(|name| !is_lower(name)) {
            return Err(self.error(Error::TypeVariableCase));
        }
        self.expect_ident(error)
    }

    fn is_tail_token(&self) -> bool {
        matches!(self.cur().kind, TokenKind::Dot | TokenKind::Tail(_))
    }

    fn starts_type_item(&self) -> bool {
        self.is_lower_ident()
            || self.is_upper_ident()
            || self.is(TokenKind::LParen)
            || self.is(TokenKind::LBrace)
    }

    fn starts_atom(&self) -> bool {
        self.starts_type_item()
    }

    fn starts_term(&self) -> bool {
        matches!(
            self.cur().kind,
            TokenKind::Ident(_)
                | TokenKind::Int(_)
                | TokenKind::Number(_)
                | TokenKind::String(_)
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Field(_)
                | TokenKind::FieldSet(_)
                | TokenKind::FieldInsert(_)
                | TokenKind::LBrack
                | TokenKind::LAngle
                | TokenKind::LBrace
        )
    }
}

fn apply(head: Type, args: Vec<Type>) -> Type {
    if args.is_empty() {
        head
    } else {
        Type::App(Box::new(head), args)
    }
}

fn is_upper(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_uppercase())
}

fn is_lower(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_lowercase())
}

fn check_duplicates(module: &Module) -> Result<(), Diagnostic> {
    let mut types = HashSet::new();
    let mut words = HashSet::new();
    let mut classes = HashSet::new();
    for item in &module.items {
        match item {
            Item::Data(data) => {
                claim(&mut types, &data.name, data.name_span)?;
                for ctor in &data.ctors {
                    claim(&mut words, &ctor.name, ctor.span)?;
                }
            }
            Item::Synonym(synonym) => claim(&mut types, &synonym.name, synonym.name_span)?,
            Item::Def(def) => claim(&mut words, &def.name, def.name_span)?,
            Item::Class(class) => claim(&mut classes, &class.name, class.name_span)?,
            Item::Instance(_) => {}
        }
    }
    Ok(())
}

fn claim(seen: &mut HashSet<String>, name: &str, span: Span) -> Result<(), Diagnostic> {
    if !seen.insert(name.to_string()) {
        Err(Diagnostic::new(span, Error::DuplicateDefinition(name.to_string())))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::diagnostic::Diagnostic;
    use crate::paths::ParseMode;

    fn ok(source: &str) -> String {
        parse(source, ParseMode::Source)
            .unwrap_or_else(|error| panic!("{}", render(source, &error)))
            .to_string()
    }

    fn err(source: &str, mode: ParseMode) -> String {
        parse(source, mode)
            .expect_err("expected a diagnostic")
            .message
    }

    fn render(source: &str, error: &Diagnostic) -> String {
        error.render("test.ply", source)
    }

    #[test]
    fn parses_the_maybe_example() {
        let source = "\
; Maybe a = Nothing | Just a

: fromMaybe ( a (Maybe a) -- a )
  Nothing ->
  Just -> nip

: mapMaybe ( (Maybe a) (a -- b) -- (Maybe b) )
  Nothing q -> q drop Nothing
  (Just x) q -> x q call Just
";
        let module = parse(source, ParseMode::Source)
            .unwrap_or_else(|error| panic!("{}", error.render("Maybe.ply", source)));
        let text = module.to_string();
        assert!(
            text.contains(": fromMaybe ( a (Maybe a) -- a )"),
            "{text}"
        );
        assert!(text.contains("Nothing ->\n  Just -> nip"), "{text}");
        assert!(
            text.contains(": mapMaybe ( (Maybe a) (a -- b) -- (Maybe b) )"),
            "{text}"
        );
        assert!(text.contains("Nothing q -> q drop Nothing"), "{text}");
        assert!(text.contains("(Just x) q -> x q call Just"), "{text}");
    }

    #[test]
    fn parses_named_constructor_fields() {
        let text = ok("; Pair a b = Pair frs : a, snd : b\n");
        assert!(text.contains("Pair frs : a, snd : b"), "{text}");
        let wrapped = ok("; Entry = Entry\n  id : Num,\n  at : Num\n");
        assert!(wrapped.contains("Entry id : Num, at : Num"), "{wrapped}");
        assert_eq!(
            err("; Pair a b = Pair a b\n", ParseMode::Source),
            "a constructor with several fields names each one"
        );
        assert_eq!(
            err("; Box a = Box value : a\n", ParseMode::Source),
            "a single field has no name"
        );
        assert_eq!(
            err("; Option a = None | Some value : a\n", ParseMode::Source),
            "a single field has no name"
        );
    }

    #[test]
    fn brackets_are_arrays() {
        let array = ok(": arr ( -- (Array Num) )\n  < 1 2 3 >\n");
        assert!(array.contains("< 1 2 3 >"), "{array}");
        assert_eq!(ok(&array), array);
        let empty = ok(": none ( -- (Array Int) )\n  <>\n");
        assert!(empty.contains("<>"), "{empty}");
        assert_eq!(ok(&empty), empty);
        let read = ok("; Person = Person name : Str, age : Num\n: get ( Person -- Str )\n  name>>\n");
        assert!(read.contains("name>>"), "{read}");
        assert_eq!(
            err(": bad ( -- (Array Num) )\n  < 1 dup >\n", ParseMode::Source),
            "expected element"
        );
        assert_eq!(
            err(": bad ( -- Str )\n  { name: \"Ada\" }\n", ParseMode::Source),
            "unexpected character"
        );
        assert_eq!(
            err("; Person = Person name : Str, name : Int\n", ParseMode::Source),
            "duplicate field name"
        );
    }

    #[test]
    fn the_declaration_dot_is_not_the_empty_tail() {
        let both = ok(": just ( a. . a -- . a )\n  drop\n");
        assert!(both.contains(": just ( a. . a -- . a )"), "{both}");
        let mono = ok(": only ( . Int -- . Int )\n  drop\n");
        assert!(mono.contains(": only ( . Int -- . Int )"), "{mono}");
    }

    #[test]
    fn spaces_separate_slots_and_parentheses_apply() {
        let text = ok(": add ( Int Int -- Int )\n  iadd\n");
        assert!(text.contains(": add ( Int Int -- Int )"), "{text}");
        assert_eq!(ok(&text), text);
        let wrapped = ok(": add ( (Int) (Int) -- Int )\n  iadd\n");
        assert!(wrapped.contains(": add ( Int Int -- Int )"), "{wrapped}");
        let data = ok("; Pair = Mk frs : Int, snd : Int\n");
        assert!(data.contains("; Pair = Mk frs : Int, snd : Int"), "{data}");
        assert_eq!(ok(&data), data);
        let applied = ok(": just ( a. (Maybe a) Int -- a )\n  drop\n");
        assert!(applied.contains("(Maybe a) Int -- a"), "{applied}");
    }

    #[test]
    fn two_stack_variables_stay_two_items() {
        let text = ok(": swap2 ( a b. ..s a b -- ..s b a )\n  swap\n");
        assert!(text.contains("a b. ..s a b -- ..s b a"), "{text}");
    }

    #[test]
    fn a_name_inside_the_scheme_is_a_type() {
        let text = ok(": f ( Int -- Int dup )\n  iadd\n");
        assert!(text.contains(": f ( Int -- Int dup )\n"), "{text}");
        assert!(text.contains("iadd"), "{text}");
        let applied = ok(": f ( Int -- (Int dup) )\n  drop\n");
        assert!(applied.contains(": f ( Int -- (Int dup) )\n"), "{applied}");
    }

    #[test]
    fn a_word_after_the_scheme_is_the_body() {
        let source = ": double ( Int -- Int ) dup iadd\n";
        let module = parse(source, ParseMode::Source).unwrap_or_else(|error| panic!("{}", error.message));
        let crate::ast::Item::Def(def) = &module.items[0] else {
            panic!("def");
        };
        assert!(def.clauses.is_empty());
        assert!(matches!(def.body[0].node, crate::ast::TermNode::Word(ref name) if name == "dup"));
        assert!(matches!(def.body[1].node, crate::ast::TermNode::Word(ref name) if name == "iadd"));
        let text = ok(source);
        assert!(text.contains(": double ( Int -- Int )\n  dup iadd"), "{text}");
        assert_eq!(
            err(": fact ( Int -- Int ) 0n -> 1n\n", ParseMode::Source),
            "expected term"
        );
        assert_eq!(
            err("double : Int -- Int\n  dup iadd\n", ParseMode::Source),
            "expected declaration"
        );
        assert_eq!(
            err("def : double ( Int -- Int ) dup iadd\n", ParseMode::Source),
            "expected declaration"
        );
    }

    #[test]
    fn comments_do_not_eat_the_stack_arrow() {
        let text = ok(". keep the signature\n: double ( Int -- Int )\n  dup\n");
        assert!(text.contains(": double ( Int -- Int )\n  dup"), "{text}");
        assert!(!text.contains("keep"), "{text}");
    }

    #[test]
    fn a_doc_comment_sticks_to_the_next_declaration() {
        let text = ok(".. Copy the top.\n..\n.. Again.\n: double ( Int -- Int )\n  .. note\n  dup\n");
        assert!(
            text.starts_with(".. Copy the top.\n..\n.. Again.\n: double"),
            "{text}"
        );
        assert!(!text.contains("note"), "{text}");
        let detached = ok(".. gone\n\n: double ( Int -- Int )\n  dup\n");
        assert!(!detached.contains("gone"), "{detached}");
        let before_import = ok(".. gone\n\n~ \"Data/Maybe\" (Just)\n\n: id ( Int -- Int )\n  dup\n");
        assert!(before_import.contains("~ \"Data/Maybe\" (Just)"), "{before_import}");
        assert!(!before_import.contains("gone"), "{before_import}");
        assert_eq!(
            err(
                ".. before the import\n~ \"Data/Maybe\" (Just)\n",
                ParseMode::Source
            ),
            "doc comment must sit above a declaration"
        );
    }

    #[test]
    fn a_doc_comment_after_an_instance_documents_the_next_word() {
        let text = ok("# Eq Int\n  eq = eqInt\n.. Next.\n: id ( Int -- Int )\n  dup\n");
        assert!(
            text.contains("# Eq Int\n  eq = eqInt\n.. Next.\n: id"),
            "{text}"
        );
        let spaced = ok(
            "# Eq Int\n  eq = eqInt\n\n.. Next.\n: id ( Int -- Int )\n  dup\n",
        );
        assert!(spaced.contains(".. Next.\n: id"), "{spaced}");
        let class = ok(
            "& Eq a\n  eq ( a a -- Bool )\n.. Next.\n: id ( Int -- Int )\n  dup\n",
        );
        assert!(
            class.contains("& Eq a\n  eq ( a a -- Bool )\n.. Next.\n: id"),
            "{class}"
        );
        let above = ok(".. Equality.\n& Eq a\n  eq ( a a -- Bool )\n");
        assert!(above.starts_with(".. Equality.\n& Eq a"), "{above}");
        let lost = ok(".. lost\n# Eq Int\n  eq = eqInt\n");
        assert!(!lost.contains("lost"), "{lost}");
        assert!(lost.starts_with("# Eq Int"), "{lost}");
    }

    #[test]
    fn interface_has_no_body() {
        let source = "language 1\n; Maybe a = Nothing | Just a\n\n: fromMaybe ( a. a (Maybe a) -- a )\n";
        let module = parse(source, ParseMode::Interface)
            .unwrap_or_else(|error| panic!("{}", error.render("Maybe.d.ply", source)));
        assert_eq!(module.language, Some(1));
        assert!(module.to_string().starts_with("language 1\n; Maybe a"));
        assert_eq!(
            err(source, ParseMode::Source),
            "language directive is only valid in an interface"
        );
    }

    #[test]
    fn a_foreign_type_has_no_constructors() {
        let text = ok(".. A cell.\n; foreign Ref a\n");
        assert!(text.starts_with(".. A cell.\n; foreign Ref a"), "{text}");
        assert!(!text.contains('='), "{text}");
        assert_eq!(ok(&text), text);
        let bare = ok("; foreign Gate\n");
        assert_eq!(bare, "; foreign Gate");
        let next = ok("; foreign Hub a\n.. Next.\n: id ( (Hub a) -- (Hub a) )\n  dup\n");
        assert!(
            next.contains("; foreign Hub a\n.. Next.\n: id ( (Hub a) -- (Hub a) )"),
            "{next}"
        );
        let source = "language 1\n; foreign Hub a\n";
        let module = parse(source, ParseMode::Interface)
            .unwrap_or_else(|error| panic!("{}", error.render("Hub.d.ply", source)));
        assert!(module.to_string().contains("language 1\n; foreign Hub a"), "{}", module);
        assert_eq!(
            err("; foreign Hub a = Listeners\n", ParseMode::Source),
            "foreign type has no constructors"
        );
        assert_eq!(
            err("; foreign Age == Years Num\n", ParseMode::Source),
            "foreign type has no constructors"
        );
        assert_eq!(
            err("; foreign Hub a\n  Listeners\n", ParseMode::Source),
            "foreign type has no constructors"
        );
        assert_eq!(err("; Hub a =\n", ParseMode::Source), "expected constructor");
    }

    #[test]
    fn diagnostics_match_the_spec() {
        assert_eq!(
            err(": f ( Int -- Int )\n", ParseMode::Source),
            "missing body"
        );
        assert_eq!(
            err(
                "language 1\n: f ( Int -- Int )\n  dup\n",
                ParseMode::Interface
            ),
            "interface declaration has a body"
        );
        assert_eq!(
            err(
                ": f ( Int -- Int )\n  dup\n: f ( Int -- Int )\n  drop\n",
                ParseMode::Source
            ),
            "duplicate definition f"
        );
        assert_eq!(
            err("language 2\n", ParseMode::Interface),
            "language version 2 is newer than this compiler (1)"
        );
        assert!(parse("", ParseMode::Source).is_ok());
        assert_eq!(err("", ParseMode::Interface), "expected language directive");
    }

    #[test]
    fn dip_keeps_the_tail_inside_parentheses() {
        let text = ok(": dip ( a. ..s (..s -- ..t) a -- ..t a )\n  dip\n");
        assert!(text.contains("a. ..s (..s -- ..t) a -- ..t a"), "{text}");
    }

    #[test]
    fn parentheses_apply_type_variables() {
        let text = ok("& Functor f\n  map ( a b. (a -- b) (f a) -- (f b) )\n");
        assert!(
            text.contains("map ( a b. (a -- b) (f a) -- (f b) )"),
            "{text}"
        );
        assert!(!text.contains("map :"), "{text}");
        let bare = ok(": map ( a b. (a -- b) (f a) -- f b )\n  call\n");
        assert!(bare.contains("-- f b )"), "{bare}");
        assert!(!bare.contains("-- (f b)"), "{bare}");
    }

    #[test]
    fn application_of_a_quotation_is_parenthesized() {
        let bare = ok(": f ( Maybe (a -- b) -- Int )\n  drop\n");
        assert!(bare.contains(": f ( Maybe (a -- b) -- Int )"), "{bare}");
        assert!(!bare.contains("(Maybe (a -- b))"), "{bare}");
        let one = ok(": f ( (Maybe (a -- b)) -- Int )\n  drop\n");
        assert!(one.contains(": f ( (Maybe (a -- b)) -- Int )"), "{one}");
    }

    #[test]
    fn empty_parentheses_need_a_stack_arrow() {
        assert_eq!(
            err(": f ( () -- Int )\n  drop\n", ParseMode::Source),
            "expected '--'"
        );
        assert_eq!(
            err(
                ": f ( (..s a) -- Int )\n  drop\n",
                ParseMode::Source
            ),
            "expected '--'"
        );
    }

    #[test]
    fn import_all_prints() {
        let text = ok("~ \"Data/Either\" (..)\n\n: getName ( Str -- Str )\n  dup\n");
        assert!(text.contains("~ \"Data/Either\" (..)"), "{text}");
        assert_eq!(
            err(
                ": print ( Str -- ! { console } )\n  drop\n",
                ParseMode::Source
            ),
            "unexpected character"
        );
    }

    #[test]
    fn a_class_method_has_no_colon() {
        let text = ok("& Eq a\n  eq ( a a -- Bool )\n");
        assert!(text.contains("& Eq a\n  eq ( a a -- Bool )"), "{text}");
        assert!(!text.contains("eq :"), "{text}");
        assert_eq!(
            err(
                "& Eq a\n  eq : a a -- Bool\n",
                ParseMode::Source
            ),
            "expected '('"
        );
        let neutral = ok("& Neutral a\n  empty ( -- a )\n");
        assert!(neutral.contains("empty ( -- a )"), "{neutral}");
    }

    #[test]
    fn a_colon_then_a_name_then_a_scheme_is_a_definition() {
        let text = ok(": id ( a -- a )\n  dup\n\n: foreign nzero ( Num -- Bool )\n");
        assert!(text.contains(": id ( a -- a )\n  dup"), "{text}");
        assert!(!text.contains("a."), "{text}");
        assert!(text.contains(": foreign nzero ( Num -- Bool )"), "{text}");
        assert_eq!(
            err("foreign : nzero ( Num -- Bool )\n", ParseMode::Source),
            "expected declaration"
        );
        let both = ok(": foreign private nzero ( Num -- Bool )\n");
        assert!(both.contains(": foreign private nzero ( Num -- Bool )"), "{both}");
        let swapped = ok(": private foreign nzero ( Num -- Bool )\n");
        assert!(
            swapped.contains(": foreign private nzero ( Num -- Bool )"),
            "{swapped}"
        );
        let iface_source = "language 1\n: private foreign nzero ( Num -- Bool )\n";
        let iface = parse(iface_source, ParseMode::Interface)
            .unwrap_or_else(|error| panic!("{}", render(iface_source, &error)))
            .to_string();
        assert!(
            iface.contains(": foreign private nzero ( Num -- Bool )"),
            "{iface}"
        );
        let hidden_source = "language 1\n: private hidden ( Int -- Int )\n";
        let hidden_iface = parse(hidden_source, ParseMode::Interface)
            .unwrap_or_else(|error| panic!("{}", render(hidden_source, &error)))
            .to_string();
        assert!(
            hidden_iface.contains(": private hidden ( Int -- Int )"),
            "{hidden_iface}"
        );
        assert_eq!(
            err(": foreign foreign nzero ( Num -- Bool )\n", ParseMode::Source),
            "expected definition name"
        );
        assert_eq!(
            err(": private private nzero ( Num -- Bool )\n", ParseMode::Source),
            "expected definition name"
        );
        assert_eq!(
            err(
                ": foreign private nzero ( Num -- Bool )\n  dup\n",
                ParseMode::Source
            ),
            "foreign declaration has a body"
        );
        assert_eq!(
            err("def : foreign nzero ( Num -- Bool )\n", ParseMode::Source),
            "expected declaration"
        );
        let hidden = ok(": private hidden ( Int -- Int )\n  dup\n");
        assert!(hidden.contains(": private hidden ( Int -- Int )"), "{hidden}");
        let source = ": fact ( Int -- Int )\n  0n -> 1n\n  n -> n dup 1n isub fact imul\n: double ( Int -- Int ) dup iadd\n";
        let module = parse(source, ParseMode::Source).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(module.items.len(), 2, "{module}");
        let crate::ast::Item::Def(fact) = &module.items[0] else {
            panic!("fact");
        };
        assert!(fact.body.is_empty());
        assert_eq!(fact.clauses.len(), 2);
        let crate::ast::Item::Def(double) = &module.items[1] else {
            panic!("double");
        };
        assert!(double.clauses.is_empty());
        assert_eq!(double.body.len(), 2);
        let printed = ok(source);
        assert!(printed.contains(": fact ( Int -- Int )\n  0n -> 1n"), "{printed}");
        assert!(printed.contains(": double ( Int -- Int )\n  dup iadd"), "{printed}");
    }

    #[test]
    fn equations_include_one_name() {
        let text = ok(": toString ( Bool -- Str )\n  true -> \"true\"\n  false -> \"false\"\n");
        assert!(text.contains("true -> \"true\""), "{text}");
        assert!(text.contains("false -> \"false\""), "{text}");
        let nested = ok(
            ": flatten ( a. (Maybe (Maybe a)) -- (Maybe a) )\n  Nothing -> Nothing\n  (Just Nothing) -> Nothing\n  (Just (Just x)) -> x Just\n",
        );
        assert!(nested.contains("(Just Nothing) -> Nothing"), "{nested}");
        assert!(nested.contains("(Just (Just x)) -> x Just"), "{nested}");
        let split = ok(": f ( (Maybe Int) -- Int )\n  (Just\n    x) -> x\n  Nothing -> 0n\n");
        assert!(split.contains("(Just x) -> x"), "{split}");
        let two = ok(
            ": a ( Bool -- Str )\n  true -> \"true\"\n  false -> \"false\"\n: b ( Int -- Int )\n  0n\n",
        );
        assert!(two.contains(": b ( Int -- Int )\n  0n"), "{two}");

        let source = ": id ( a. a -- a )\n  n -> n\n";
        let module = parse(source, ParseMode::Source).unwrap_or_else(|error| panic!("{}", error.message));
        let crate::ast::Item::Def(def) = &module.items[0] else {
            panic!("def");
        };
        assert!(def.body.is_empty());
        assert!(matches!(
            def.clauses[0].patterns.as_slice(),
            [crate::ast::Pattern::Bind(name)] if name == "n"
        ));
        assert!(matches!(def.clauses[0].body[0].node, crate::ast::TermNode::Word(ref name) if name == "n"));
        assert_eq!(
            err(": bad ( Int -- Int )\n  -> n\n", ParseMode::Source),
            "expected term"
        );
        assert_eq!(
            err(": bad ( Int -- Int )\n  [ -> n ]\n", ParseMode::Source),
            "expected term"
        );

        let quoted = ": rev ( a b. a b -- b a )\n  [ x y -> y x ] call\n";
        let module = parse(quoted, ParseMode::Source).unwrap_or_else(|error| panic!("{}", error.message));
        let crate::ast::Item::Def(def) = &module.items[0] else {
            panic!("def");
        };
        let crate::ast::TermNode::Quote(terms) = &def.body[0].node else {
            panic!("quote");
        };
        assert!(matches!(&terms[0].node, crate::ast::TermNode::Bind(names) if names == &["x".to_string(), "y".to_string()]));
        assert!(matches!(&terms[1].node, crate::ast::TermNode::Word(name) if name == "y"));
        assert!(matches!(&terms[2].node, crate::ast::TermNode::Word(name) if name == "x"));
        let printed = ok(": rev ( a b. a b -- b a )\n  [ x y -> y x ] call\n");
        assert!(printed.contains("[ x y -> y x ]"), "{printed}");
        assert_eq!(
            err(": bad ( Int -- Int )\n  [ x x -> x ]\n", ParseMode::Source),
            "duplicate definition x"
        );

        assert_eq!(
            err(
                ": bad ( Bool -- Str )\n  true -> \"true\"\n  drop\n",
                ParseMode::Source
            ),
            "expected '->'"
        );
        assert_eq!(
            err(
                ": bad ( Bool -- Str )\n  true -> \"true\"\n  >>age\n",
                ParseMode::Source
            ),
            "expected pattern"
        );
    }

    #[test]
    fn an_array_pattern_has_a_fixed_length() {
        let text = ok(
            ": isEmpty ( (Array a) -- Bool )\n  <> -> true\n  _ -> false\n",
        );
        assert!(text.contains("<> -> true"), "{text}");
        let split = ok(
            ": pick ( (Array Int) -- Int )\n  < 0n\n    a > -> a\n  _ -> 0n\n",
        );
        assert!(split.contains("< 0n a > -> a"), "{split}");
        let nested = ok(
            ": inner ( (Array (Maybe Int)) -- Int )\n  < (Just x) > -> x\n  < < > > -> 0n\n  _ -> 0n\n",
        );
        assert!(nested.contains("< (Just x) > -> x"), "{nested}");
        assert!(nested.contains("< <> > -> 0n"), "{nested}");
        assert_eq!(
            err(
                ": bad ( (Array Int) -- Int )\n  < 0n | r > -> 0n\n",
                ParseMode::Source
            ),
            "expected pattern"
        );
        assert_eq!(
            err(
                ": bad ( (Array Int) -- Int )\n  < 0n\n",
                ParseMode::Source
            ),
            "unterminated array"
        );
    }

    #[test]
    fn an_instance_prints_every_class_on_one_head() {
        let text = ok(
            "# Comparable, Sortable (Box a)\n  eq = eqBox\n  compare = cmpBox\n",
        );
        assert!(
            text.contains("# Comparable, Sortable (Box a)\n  eq = eqBox\n  compare = cmpBox"),
            "{text}"
        );
    }

    #[test]
    fn a_superclass_and_a_constraint_comma_print() {
        let class = ok(
            "& Comparable a => Sortable a\n  compare ( a a -- Ordering )\n",
        );
        assert!(
            class.contains("& Comparable a => Sortable a\n  compare ( a a -- Ordering )"),
            "{class}"
        );
        let several = ok(
            "& Comparable a, Show a => Sortable a\n  compare ( a a -- Bool )\n",
        );
        assert!(
            several.contains("& Comparable a, Show a => Sortable a"),
            "{several}"
        );
        let comma = ok(": f ( Liftable m, Chainable m => a -- a )\n  dup\n");
        assert!(comma.contains("Liftable m => Chainable m =>"), "{comma}");
        let arrows = ok(": f ( Liftable m => Chainable m => a -- a )\n  dup\n");
        assert!(arrows.contains("Liftable m => Chainable m =>"), "{arrows}");
        let product = ok(": f ( Eq Person => Person -- Bool )\n  drop\n");
        assert!(product.contains("Eq Person => Person -- Bool"), "{product}");
        let both = ok(": f ( Eq Person, Ord a => a -- a )\n  dup\n");
        assert!(both.contains("Eq Person => Ord a =>"), "{both}");
        assert_eq!(
            err("& Comparable => Sortable a\n  compare ( a a -- Bool )\n", ParseMode::Source),
            "expected class argument"
        );
        assert_eq!(
            err(
                "& Sortable a <= Comparable a\n  compare ( a a -- Ordering )\n",
                ParseMode::Source
            ),
            "expected method name"
        );
        assert_eq!(err(": f ( Eq a, => a -- a )\n  dup\n", ParseMode::Source), "expected class name");
    }
}
