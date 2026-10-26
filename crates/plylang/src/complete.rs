//! Completions at the cursor: words, types, import paths, import names, and record fields.
//! A file that does not parse still completes from the lines above the cursor.

use std::path::Path;

use crate::ast::{
    method_line, scheme_string, type_string, Class, Ctor, Data, Def, Item, MethodSig, Module,
    Scheme, Stack, StackEffect, StackTail, Type,
};
use crate::build::{available_import_paths, load_provider, stack_sites};
use crate::builtin::{self, Lowering};
use crate::lexer::tokenize;
use crate::parser::parse;
use crate::paths::mode_from_path;
use crate::span::{byte_offset, Span};
use crate::token::{Token, TokenKind};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompletionKind {
    Keyword,
    Word,
    Constructor,
    Type,
    TypeVar,
    Module,
    Field,
    Class,
}

#[derive(Debug)]
pub struct Completion {
    pub label: String,
    pub insert: String,
    pub kind: CompletionKind,
    pub detail: String,
    pub documentation: String,
    pub replace: Span,
    pub sort: String,
}

/// `line` and `character` are the 0-based UTF-16 position LSP sends.
pub fn complete(source: &str, path: &Path, line: u32, character: u32) -> Vec<Completion> {
    let offset = byte_offset(source, line, character) as usize;
    let offset = offset.min(source.len());
    if in_comment(source, offset) {
        return Vec::new();
    }
    if let Some(mut items) = complete_fields(source, path, offset) {
        items.sort_by(|left, right| left.sort.cmp(&right.sort));
        return items;
    }
    let Ok(tokens) = tokenize(source) else {
        return unclosed_import(source, offset)
            .map(|replace| import_paths(path, replace, true))
            .unwrap_or_default();
    };
    let partial = tokens.iter().find(|token| is_partial(token, offset));
    let before = tokens_before(&tokens, offset, partial);
    let walked = walk(&before);
    let at_column_0 = at_decl_column(source, offset, partial);
    let replace = replace_span(source, offset, partial);
    let context = context_of(&walked, partial, at_column_0);
    let mut items = match context {
        Context::None => Vec::new(),
        Context::Keyword => keywords(path.to_string_lossy().ends_with(".d.ply"), replace),
        Context::Modifier { foreign, private } => modifiers(foreign, private, replace),
        Context::ImportPath { quoted } => import_paths(path, replace, quoted),
        Context::ImportName { path: import, seen, has_all } => {
            import_names(path, &import, &seen, has_all, replace)
        }
        Context::Type { vars } => {
            let module = parsed_module(source, path, offset);
            type_names(path, module.as_ref(), &vars, replace)
        }
        Context::Word => {
            let module = parsed_module(source, path, offset);
            let mut items = word_names(path, module.as_ref(), replace);
            for (name, detail) in reader_fields(path, source, offset) {
                let label = format!("/{name}");
                push(
                    &mut items,
                    entry(&label, &label, CompletionKind::Field, &detail, "", "1", replace),
                );
            }
            items
        }
    };
    items.sort_by(|left, right| left.sort.cmp(&right.sort));
    items
}

enum Context {
    None,
    Keyword,
    Modifier { foreign: bool, private: bool },
    ImportPath { quoted: bool },
    ImportName { path: String, seen: Vec<String>, has_all: bool },
    Type { vars: Vec<String> },
    Word,
}

fn context_of(walked: &Walk, partial: Option<&Token>, at_column_0: bool) -> Context {
    if matches!(partial.map(|token| &token.kind), Some(TokenKind::String(_)))
        && matches!(walked.state, Place::Import)
    {
        return Context::ImportPath { quoted: true };
    }
    match walked.state {
        Place::ImportList => Context::ImportName {
            path: walked.path.clone(),
            seen: walked.seen.clone(),
            has_all: walked.has_all,
        },
        Place::Import => Context::ImportPath { quoted: false },
        Place::Vis | Place::DefName => Context::Modifier {
            foreign: walked.saw_foreign,
            private: walked.saw_private,
        },
        Place::Signature => Context::Type { vars: walked.vars.clone() },
        Place::DataFields => Context::Type { vars: walked.params.clone() },
        Place::Body if !at_column_0 => Context::Word,
        Place::Top | Place::Body | Place::DataGap if at_column_0 => Context::Keyword,
        _ => Context::None,
    }
}

fn keywords(interface: bool, replace: Span) -> Vec<Completion> {
    let names: &[&str] = if interface {
        &["~", ";", "-", "&", "#", ":", "language"]
    } else {
        &["~", ";", "-", "&", "#", ":"]
    };
    names
        .iter()
        .map(|name| entry(*name, *name, CompletionKind::Keyword, "", "", "0", replace))
        .collect()
}

fn modifiers(saw_foreign: bool, saw_private: bool, replace: Span) -> Vec<Completion> {
    let mut names = Vec::new();
    if !saw_foreign {
        names.push("foreign");
    }
    if !saw_private {
        names.push("private");
    }
    names
        .into_iter()
        .map(|name| entry(name, name, CompletionKind::Keyword, "", "", "0", replace))
        .collect()
}

fn import_paths(from: &Path, replace: Span, quoted: bool) -> Vec<Completion> {
    available_import_paths(from)
        .into_iter()
        .map(|path| {
            let insert = if quoted { path.clone() } else { format!("\"{path}\"") };
            entry(&path, &insert, CompletionKind::Module, "", "", "1", replace)
        })
        .collect()
}

fn import_names(
    from: &Path,
    import: &str,
    seen: &[String],
    has_all: bool,
    replace: Span,
) -> Vec<Completion> {
    if has_all {
        return Vec::new();
    }
    let mut items = Vec::new();
    if let Some(provider) = load_provider(from, import) {
        for (name, kind, detail, doc) in provider_exports(&provider) {
            if seen.iter().any(|have| have == &name) {
                continue;
            }
            push(&mut items, entry(&name, &name, kind, &detail, &doc, "2", replace));
        }
    }
    if !seen.iter().any(|name| name == "..") {
        push(
            &mut items,
            entry("..", "..", CompletionKind::Keyword, "every public name", "", "9", replace),
        );
    }
    items
}

fn type_names(from: &Path, module: Option<&Module>, vars: &[String], replace: Span) -> Vec<Completion> {
    let mut items = Vec::new();
    for var in vars {
        push(
            &mut items,
            entry(var, var, CompletionKind::TypeVar, "", "", "1", replace),
        );
    }
    if let Some(module) = module {
        for item in &module.items {
            match item {
                Item::Data(data) => push(
                    &mut items,
                    entry(&data.name, &data.name, CompletionKind::Type, &type_detail(data), &data.doc, "1", replace),
                ),
                Item::Synonym(synonym) => push(
                    &mut items,
                    entry(
                        &synonym.name,
                        &synonym.name,
                        CompletionKind::Type,
                        &synonym_detail(synonym),
                        &synonym.doc,
                        "1",
                        replace,
                    ),
                ),
                Item::Class(class) => push(
                    &mut items,
                    entry(
                        &class.name,
                        &class.name,
                        CompletionKind::Class,
                        &class_detail(class),
                        &class.doc,
                        "1",
                        replace,
                    ),
                ),
                Item::Def(_) | Item::Instance(_) => {}
            }
        }
        for name in imported_types(from, module) {
            push(&mut items, name.with_replace(replace));
        }
    }
    for ty in builtin::TYPES {
        push(
            &mut items,
            entry(ty.name, ty.name, CompletionKind::Type, "", "", "3", replace),
        );
    }
    items
}

fn word_names(from: &Path, module: Option<&Module>, replace: Span) -> Vec<Completion> {
    let mut items = Vec::new();
    if let Some(module) = module {
        for item in &module.items {
            match item {
                Item::Def(def) => push(&mut items, def_entry(def, "1", replace)),
                Item::Data(data) => {
                    for ctor in &data.ctors {
                        push(&mut items, ctor_entry(data, ctor, "1", replace));
                    }
                }
                Item::Class(class) => {
                    for method in &class.methods {
                        push(&mut items, method_entry(class, method, "1", replace));
                    }
                }
                Item::Synonym(_) | Item::Instance(_) => {}
            }
        }
        for name in imported_words(from, module) {
            push(&mut items, name.with_replace(replace));
        }
    }
    for (name, kind, detail, doc) in builtin_words() {
        push(&mut items, entry(name, name, kind, detail, doc, "3", replace));
    }
    items
}

fn builtin_words() -> Vec<(&'static str, CompletionKind, &'static str, &'static str)> {
    let mut names = vec![
        ("true", CompletionKind::Keyword, "-- Bool", ""),
        ("false", CompletionKind::Keyword, "-- Bool", ""),
    ];
    for word in builtin::WORDS {
        let kind = match word.lowering {
            Lowering::Unit => CompletionKind::Constructor,
            _ => CompletionKind::Word,
        };
        names.push((word.name, kind, word.scheme, word.doc));
    }
    names
}

struct Named {
    label: String,
    kind: CompletionKind,
    detail: String,
    documentation: String,
    group: &'static str,
}

impl Named {
    fn with_replace(self, replace: Span) -> Completion {
        entry(
            &self.label,
            &self.label,
            self.kind,
            &self.detail,
            &self.documentation,
            self.group,
            replace,
        )
    }
}

fn imported_words(from: &Path, module: &Module) -> Vec<Named> {
    let mut names = Vec::new();
    for import in &module.imports {
        let Some(provider) = load_provider(from, &import.path) else {
            continue;
        };
        for name in selected_names(import, &provider) {
            if let Some(def) = find_def(&provider, &name) {
                if !def.vis.is_private() {
                    names.push(Named {
                        label: def.name.clone(),
                        kind: CompletionKind::Word,
                        detail: scheme_string(&def.scheme),
                        documentation: doc_text(&def.doc),
                        group: "2",
                    });
                }
            } else if let Some((data, ctor)) = find_ctor(&provider, &name) {
                names.push(Named {
                    label: ctor.name.clone(),
                    kind: CompletionKind::Constructor,
                    detail: ctor_detail(data, ctor),
                    documentation: doc_text(&data.doc),
                    group: "2",
                });
            } else if let Some((class, method)) = find_method(&provider, &name) {
                names.push(Named {
                    label: method.name.clone(),
                    kind: CompletionKind::Word,
                    detail: method_line(class, method),
                    documentation: doc_text(&class.doc),
                    group: "2",
                });
            }
        }
    }
    names
}

fn imported_types(from: &Path, module: &Module) -> Vec<Named> {
    let mut names = Vec::new();
    for import in &module.imports {
        let Some(provider) = load_provider(from, &import.path) else {
            continue;
        };
        for name in selected_names(import, &provider) {
            if let Some(data) = find_data(&provider, &name) {
                names.push(Named {
                    label: data.name.clone(),
                    kind: CompletionKind::Type,
                    detail: type_detail(data),
                    documentation: doc_text(&data.doc),
                    group: "2",
                });
            } else if let Some(class) = find_class(&provider, &name) {
                names.push(Named {
                    label: class.name.clone(),
                    kind: CompletionKind::Class,
                    detail: class_detail(class),
                    documentation: doc_text(&class.doc),
                    group: "2",
                });
            }
        }
    }
    names
}

fn selected_names(import: &crate::ast::Import, provider: &Module) -> Vec<String> {
    if import.all {
        provider_exports(provider).into_iter().map(|(name, _, _, _)| name).collect()
    } else {
        import.names.clone()
    }
}

fn provider_exports(module: &Module) -> Vec<(String, CompletionKind, String, String)> {
    let mut names = Vec::new();
    for item in &module.items {
        if let Item::Data(data) = item {
            names.push((
                data.name.clone(),
                CompletionKind::Type,
                type_detail(data),
                doc_text(&data.doc),
            ));
            for ctor in &data.ctors {
                names.push((
                    ctor.name.clone(),
                    CompletionKind::Constructor,
                    ctor_detail(data, ctor),
                    doc_text(&data.doc),
                ));
            }
        }
        if let Item::Synonym(synonym) = item {
            names.push((
                synonym.name.clone(),
                CompletionKind::Type,
                synonym_detail(synonym),
                doc_text(&synonym.doc),
            ));
        }
        if let Item::Class(class) = item {
            names.push((
                class.name.clone(),
                CompletionKind::Class,
                class_detail(class),
                doc_text(&class.doc),
            ));
            for method in &class.methods {
                names.push((
                    method.name.clone(),
                    CompletionKind::Word,
                    method_line(class, method),
                    doc_text(&class.doc),
                ));
            }
        }
    }
    for item in &module.items {
        if let Item::Def(def) = item {
            if !def.vis.is_private() {
                names.push((
                    def.name.clone(),
                    CompletionKind::Word,
                    scheme_string(&def.scheme),
                    doc_text(&def.doc),
                ));
            }
        }
    }
    names
}

fn method_entry(class: &Class, method: &MethodSig, group: &str, replace: Span) -> Completion {
    let line = method_line(class, method);
    let detail = line
        .split_once(" ( ")
        .and_then(|(_, rest)| rest.strip_suffix(" )"))
        .unwrap_or(&line);
    entry(
        &method.name,
        &method.name,
        CompletionKind::Word,
        detail,
        &class.doc,
        &format!("{group}{}", method.name),
        replace,
    )
}

fn class_detail(class: &Class) -> String {
    format!("& {} {}", class.name, class.params.join(" "))
}

fn find_class<'a>(module: &'a Module, name: &str) -> Option<&'a Class> {
    module.items.iter().find_map(|item| match item {
        Item::Class(class) if class.name == name => Some(class),
        _ => None,
    })
}

fn find_method<'a>(module: &'a Module, name: &str) -> Option<(&'a Class, &'a MethodSig)> {
    module.items.iter().find_map(|item| match item {
        Item::Class(class) => class
            .methods
            .iter()
            .find(|method| method.name == name)
            .map(|method| (class, method)),
        _ => None,
    })
}

fn def_entry(def: &Def, group: &str, replace: Span) -> Completion {
    entry(
        &def.name,
        &def.name,
        CompletionKind::Word,
        &scheme_string(&def.scheme),
        &def.doc,
        &format!("{group}{}", def.name),
        replace,
    )
}

fn ctor_entry(data: &Data, ctor: &Ctor, group: &str, replace: Span) -> Completion {
    entry(
        &ctor.name,
        &ctor.name,
        CompletionKind::Constructor,
        &ctor_detail(data, ctor),
        &data.doc,
        &format!("{group}{}", ctor.name),
        replace,
    )
}

fn synonym_detail(synonym: &crate::ast::Synonym) -> String {
    let mut out = format!("- {}", synonym.name);
    for param in &synonym.params {
        out.push(' ');
        out.push_str(param);
    }
    out.push(' ');
    out.push_str(&crate::ast::type_string(&synonym.rhs));
    out
}

fn type_detail(data: &Data) -> String {
    if data.params.is_empty() {
        String::new()
    } else {
        format!("{} {}", data.name, data.params.join(" "))
    }
}

fn ctor_detail(data: &Data, ctor: &Ctor) -> String {
    let result = if data.params.is_empty() {
        Type::Ctor(data.name.clone())
    } else {
        Type::App(
            Box::new(Type::Ctor(data.name.clone())),
            data.params.iter().map(|param| Type::Var(param.clone())).collect(),
        )
    };
    scheme_string(&Scheme {
        vars: data.params.clone(),
        constraints: Vec::new(),
        effect: StackEffect {
            input: Stack { tail: StackTail::Implicit, items: ctor.fields.clone() },
            output: Stack { tail: StackTail::Implicit, items: vec![result] },
            effects: None,
        },
    })
}

fn entry(
    label: &str,
    insert: &str,
    kind: CompletionKind,
    detail: &str,
    doc: &str,
    group: &str,
    replace: Span,
) -> Completion {
    Completion {
        label: label.to_string(),
        insert: insert.to_string(),
        kind,
        detail: detail.to_string(),
        documentation: doc_text(doc),
        replace,
        sort: format!("{group}{label}"),
    }
}

fn push(items: &mut Vec<Completion>, item: Completion) {
    if items.iter().any(|have| have.label == item.label) {
        return;
    }
    items.push(item);
}

fn doc_text(doc: &str) -> String {
    if doc.is_empty() {
        String::new()
    } else {
        doc.replace("```", "\\`\\`\\`")
    }
}

fn parsed_module(source: &str, path: &Path, offset: usize) -> Option<Module> {
    let mode = mode_from_path(&path.to_string_lossy());
    if let Ok(module) = parse(source, mode) {
        return Some(module);
    }
    let cut = source[..offset].rfind('\n')?;
    parse(&source[..=cut], mode).ok()
}

fn find_data<'a>(module: &'a Module, name: &str) -> Option<&'a Data> {
    module.items.iter().find_map(|item| match item {
        Item::Data(data) if data.name == name => Some(data),
        _ => None,
    })
}

fn find_def<'a>(module: &'a Module, name: &str) -> Option<&'a Def> {
    module.items.iter().find_map(|item| match item {
        Item::Def(def) if def.name == name => Some(def),
        _ => None,
    })
}

fn find_ctor<'a>(module: &'a Module, name: &str) -> Option<(&'a Data, &'a Ctor)> {
    module.items.iter().find_map(|item| match item {
        Item::Data(data) => data.ctors.iter().find(|ctor| ctor.name == name).map(|ctor| (data, ctor)),
        _ => None,
    })
}

struct Walk {
    state: Place,
    path: String,
    seen: Vec<String>,
    has_all: bool,
    vars: Vec<String>,
    params: Vec<String>,
    binders: bool,
    depth: i32,
    saw_foreign: bool,
    saw_private: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Top,
    Vis,
    Import,
    ImportString,
    ImportList,
    DefName,
    DefParen,
    Signature,
    Body,
    DataName,
    DataParams,
    DataCtor,
    DataFields,
    DataGap,
}

fn walk(tokens: &[&Token]) -> Walk {
    let mut walked = Walk {
        state: Place::Top,
        path: String::new(),
        seen: Vec::new(),
        has_all: false,
        vars: Vec::new(),
        params: Vec::new(),
        binders: true,
        depth: 0,
        saw_foreign: false,
        saw_private: false,
    };
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let next = tokens.get(index + 1);
        if resets(walked.state, token, next) {
            walked.state = Place::Top;
            walked.depth = 0;
            walked.saw_foreign = false;
            walked.saw_private = false;
        }
        step(&mut walked, token, next);
        index += 1;
    }
    walked
}

fn followed_by_colon(next: Option<&&Token>) -> bool {
    next.is_some_and(|token| matches!(token.kind, TokenKind::Colon))
}

fn resets(state: Place, token: &Token, _next: Option<&&Token>) -> bool {
    let declaration = is_item(token);
    if !declaration {
        return false;
    }
    matches!(
        state,
        Place::Body
            | Place::Signature
            | Place::ImportList
            | Place::ImportString
            | Place::DataCtor
            | Place::DataFields
            | Place::DataGap
    )
}

fn step(walked: &mut Walk, token: &Token, next: Option<&&Token>) {
    match walked.state {
        Place::Top => match &token.kind {
            TokenKind::Import => walked.state = Place::Import,
            TokenKind::Colon => {
                walked.saw_foreign = false;
                walked.saw_private = false;
                walked.state = Place::DefName;
            }
            TokenKind::Data => walked.state = Place::DataName,
            _ => {}
        },
        Place::Vis => match &token.kind {
            TokenKind::Ident(_) => walked.state = Place::DefParen,
            TokenKind::Foreign => walked.saw_foreign = true,
            TokenKind::Private => walked.saw_private = true,
            TokenKind::Newline => {
                walked.saw_foreign = false;
                walked.saw_private = false;
                walked.state = Place::Top;
            }
            _ => {}
        },
        Place::Import => match &token.kind {
            TokenKind::String(path) => {
                walked.path = path.clone();
                walked.state = Place::ImportString;
            }
            TokenKind::Newline => walked.state = Place::Top,
            _ => {}
        },
        Place::ImportString => match &token.kind {
            TokenKind::LParen => {
                walked.seen.clear();
                walked.has_all = false;
                walked.state = Place::ImportList;
            }
            TokenKind::Newline => walked.state = Place::Top,
            _ => {}
        },
        Place::ImportList => match &token.kind {
            TokenKind::RParen => walked.state = Place::Top,
            TokenKind::Ident(name) => walked.seen.push(name.clone()),
            TokenKind::All => walked.has_all = true,
            _ => {}
        },
        Place::DefName => match &token.kind {
            TokenKind::Ident(_) => walked.state = Place::DefParen,
            TokenKind::Foreign => {
                walked.saw_foreign = true;
                walked.state = Place::Vis;
            }
            TokenKind::Private => {
                walked.saw_private = true;
                walked.state = Place::Vis;
            }
            TokenKind::Newline => {
                walked.saw_foreign = false;
                walked.saw_private = false;
                walked.state = Place::Top;
            }
            _ => {}
        },
        Place::DefParen => match &token.kind {
            TokenKind::LParen => {
                walked.vars.clear();
                walked.binders = true;
                walked.depth = 0;
                walked.state = Place::Signature;
            }
            TokenKind::Newline => walked.state = Place::Top,
            _ => {}
        },
        Place::Signature => match &token.kind {
            TokenKind::LParen | TokenKind::LBrace => {
                walked.depth += 1;
                walked.binders = false;
            }
            TokenKind::RParen if walked.depth == 0 => walked.state = Place::Body,
            TokenKind::RParen | TokenKind::RBrace => {
                walked.depth = walked.depth.saturating_sub(1);
            }
            TokenKind::Dot | TokenKind::StackArrow if walked.depth == 0 => walked.binders = false,
            TokenKind::Ident(name) if is_var(name) => {
                let field = followed_by_colon(next);
                if !field && !walked.vars.iter().any(|var| var == name) {
                    walked.vars.push(name.clone());
                }
                if !(walked.binders && walked.depth == 0 && !field) {
                    walked.binders = false;
                }
            }
            TokenKind::Ident(_) => walked.binders = false,
            _ => {}
        },
        Place::Body => {}
        Place::DataName => match &token.kind {
            TokenKind::Ident(_) => walked.state = Place::DataParams,
            TokenKind::Newline => walked.state = Place::Top,
            _ => {}
        },
        Place::DataParams => match &token.kind {
            TokenKind::Ident(name) if is_var(name) && !token.line_start => {
                if !walked.params.iter().any(|param| param == name) {
                    walked.params.push(name.clone());
                }
            }
            TokenKind::Eq | TokenKind::EqEq => walked.state = Place::DataCtor,
            _ => {}
        },
        Place::DataCtor => match &token.kind {
            TokenKind::Ident(name) if is_upper(name) => {
                walked.depth = 0;
                walked.state = Place::DataFields;
            }
            TokenKind::Newline => walked.state = Place::DataGap,
            _ => {}
        },
        Place::DataFields => match &token.kind {
            TokenKind::LParen | TokenKind::LBrace => walked.depth += 1,
            TokenKind::RParen | TokenKind::RBrace => {
                walked.depth = walked.depth.saturating_sub(1);
            }
            TokenKind::Pipe if walked.depth == 0 => walked.state = Place::DataCtor,
            TokenKind::Newline if walked.depth == 0 => walked.state = Place::DataGap,
            _ => {}
        },
        Place::DataGap => match &token.kind {
            TokenKind::Pipe => walked.state = Place::DataCtor,
            _ => {}
        },
    }
}

fn is_item(token: &Token) -> bool {
    token.col == 0
        && token.line_start
        && matches!(
            token.kind,
            TokenKind::Import
                | TokenKind::Data
                | TokenKind::Type
                | TokenKind::Class
                | TokenKind::Instance
                | TokenKind::Colon
        )
}

fn is_partial(token: &Token, offset: usize) -> bool {
    let start = token.span.start as usize;
    let end = token.span.end as usize;
    if start >= end {
        return false;
    }
    match &token.kind {
        TokenKind::Ident(_) | TokenKind::Field(_) => start <= offset && offset <= end,
        TokenKind::String(_) => start < offset && offset < end,
        _ => false,
    }
}

fn tokens_before<'a>(tokens: &'a [Token], offset: usize, partial: Option<&Token>) -> Vec<&'a Token> {
    let mut before = Vec::new();
    for token in tokens {
        if matches!(token.kind, TokenKind::Eof) {
            continue;
        }
        if partial.is_some_and(|partial| std::ptr::eq(token, partial)) {
            break;
        }
        if token.span.start as usize >= offset || token.span.end as usize > offset {
            break;
        }
        before.push(token);
    }
    before
}

fn at_decl_column(source: &str, offset: usize, partial: Option<&Token>) -> bool {
    if let Some(token) = partial {
        return token.col == 0 && token.line_start;
    }
    let line_start = source[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0);
    offset == line_start
}

fn replace_span(source: &str, offset: usize, partial: Option<&Token>) -> Span {
    let Some(token) = partial else {
        return Span::new(offset, offset);
    };
    match &token.kind {
        TokenKind::Ident(_) | TokenKind::Field(_) => token.span,
        TokenKind::String(_) => string_interior(source, token),
        _ => Span::new(offset, offset),
    }
}

fn string_interior(source: &str, token: &Token) -> Span {
    let start = token.span.start as usize + 1;
    let mut end = token.span.end as usize;
    if end > start && source.as_bytes().get(end - 1) == Some(&b'"') {
        end -= 1;
    }
    Span::new(start.min(end), end)
}

/// A `.` comment outside parentheses, before the cursor.
fn in_comment(source: &str, offset: usize) -> bool {
    scan_line(source, offset).0
}

/// The cursor sits inside a string on this line.
fn in_string(source: &str, offset: usize) -> bool {
    scan_line(source, offset).1
}

/// `(in_comment, in_string)` for the source prefix ending at `offset`.
/// A `.` outside parentheses comments out the rest of its line. A dot between
/// digits is a decimal, and a dot inside `()` is scheme syntax.
fn scan_line(source: &str, offset: usize) -> (bool, bool) {
    let mut chars = source[..offset].chars().peekable();
    let mut in_string = false;
    let mut in_comment = false;
    let mut depth = 0u32;
    let mut prev_digit = false;
    while let Some(ch) = chars.next() {
        if ch == '\n' || ch == '\r' {
            in_comment = false;
            prev_digit = false;
            continue;
        }
        if in_comment {
            continue;
        }
        if in_string {
            if ch == '\\' {
                chars.next();
            } else if ch == '"' {
                in_string = false;
            }
            prev_digit = false;
            continue;
        }
        if ch == '"' {
            in_string = true;
            prev_digit = false;
            continue;
        }
        if ch == '(' {
            depth += 1;
            prev_digit = false;
            continue;
        }
        if ch == ')' {
            depth = depth.saturating_sub(1);
            prev_digit = false;
            continue;
        }
        if ch == '.' && depth == 0 {
            let decimal = prev_digit && chars.peek().is_some_and(|next| next.is_ascii_digit());
            if !decimal {
                in_comment = true;
                prev_digit = false;
                continue;
            }
        }
        prev_digit = ch.is_ascii_digit();
    }
    (in_comment, in_string)
}

struct FieldEdit {
    /// Byte index of `=` in `=name`.
    mark: usize,
    /// Byte index just after the field name, including characters after the cursor.
    name_end: usize,
    replace: Span,
}

fn complete_fields(source: &str, path: &Path, offset: usize) -> Option<Vec<Completion>> {
    let edit = field_edit(source, offset)?;
    if in_string(source, edit.mark) {
        return Some(Vec::new());
    }
    if !in_body(source, edit.mark) {
        return None;
    }
    let mut items = Vec::new();
    for (name, detail) in fields_at(path, source, &edit) {
        push(
            &mut items,
            entry(&name, &name, CompletionKind::Field, &detail, "", "1", edit.replace),
        );
    }
    Some(items)
}

/// `=` plus the field name around the cursor. The `=` itself stays put.
/// `/name` is a reader and is not completed here. `=>` is not a setter.
fn field_edit(source: &str, offset: usize) -> Option<FieldEdit> {
    let bytes = source.as_bytes();
    if offset > bytes.len() {
        return None;
    }
    let mut name_start = offset;
    while name_start > 0 && is_ident_byte(bytes[name_start - 1]) {
        name_start -= 1;
    }
    if name_start < offset && !is_ident_start_byte(bytes[name_start]) {
        return None;
    }
    // Cursor between `=` and `>` of `=>`.
    if name_start == offset && bytes.get(name_start) == Some(&b'>') {
        return None;
    }
    if name_start == 0 || bytes[name_start - 1] != b'=' {
        return None;
    }
    let mark = name_start - 1;
    let mut name_end = offset;
    while name_end < bytes.len() && is_ident_byte(bytes[name_end]) {
        name_end += 1;
    }
    Some(FieldEdit {
        mark,
        name_end,
        replace: Span::new(name_start, name_end),
    })
}

fn in_body(source: &str, hash: usize) -> bool {
    let Ok(tokens) = tokenize(&source[..hash]) else {
        return false;
    };
    let before = tokens_before(&tokens, hash, None);
    matches!(walk(&before).state, Place::Body)
}

/// Labels of the record on top of the stack. An unsolved top uses records written in this def.
fn fields_at(path: &Path, source: &str, edit: &FieldEdit) -> Vec<(String, String)> {
    let mut probe = String::new();
    probe.push_str(&source[..edit.mark]);
    probe.push(' ');
    let marker = probe.len();
    probe.push_str("dup");
    probe.push(' ');
    probe.push_str(&source[edit.name_end..]);
    if let Some(before) = stack_before(path, &probe, marker as u32) {
        // The new value is on top. The record is under it, or on top when the value is not written yet.
        let slot = under_top(&before).or_else(|| top_slot(&before));
        if let Some(top) = slot {
            if let Some(fields) = record_fields(top) {
                return fields;
            }
            if !is_open_stack(top) {
                return Vec::new();
            }
        }
    }
    signature_fields(path, source, edit.mark)
}

/// `/name` for the record on top, offered while a word or `/` reader is being typed.
/// A setter `=name` is handled by `field_edit` and yields nothing here.
fn reader_fields(path: &Path, source: &str, offset: usize) -> Vec<(String, String)> {
    let bytes = source.as_bytes();
    let end = offset.min(bytes.len());
    let mut name_start = end;
    while name_start > 0 && is_ident_byte(bytes[name_start - 1]) {
        name_start -= 1;
    }
    if name_start < end && !is_ident_start_byte(bytes[name_start]) {
        return Vec::new();
    }
    if name_start > 0 && bytes[name_start - 1] == b'=' {
        return Vec::new();
    }
    let mut name_end = end;
    while name_end < bytes.len() && is_ident_byte(bytes[name_end]) {
        name_end += 1;
    }
    // `/na` is one field token. The probe must not keep the slash, or it fails to lex.
    let mut cut = name_start;
    if cut > 0 && bytes[cut - 1] == b'/' {
        cut -= 1;
    }
    let mut probe = String::new();
    probe.push_str(&source[..cut]);
    let marker = probe.len();
    probe.push_str("dup ");
    probe.push_str(&source[name_end..]);
    let Some(before) = stack_before(path, &probe, marker as u32) else {
        return Vec::new();
    };
    let Some(top) = top_slot(&before) else {
        return Vec::new();
    };
    record_fields(top).unwrap_or_default()
}

fn stack_before(path: &Path, source: &str, marker: u32) -> Option<String> {
    stack_sites(path, source)
        .into_iter()
        .find(|site| site.span.start == marker)
        .map(|site| site.before)
}

fn signature_fields(path: &Path, source: &str, hash: usize) -> Vec<(String, String)> {
    let Some(module) = module_around(path, source, hash) else {
        return Vec::new();
    };
    let Some(def) = module.items.iter().rev().find_map(|item| match item {
        Item::Def(def) if def.span.start <= hash as u32 => Some(def),
        _ => None,
    }) else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    collect_scheme(&def.scheme, &mut fields);
    fields
}

fn module_around(path: &Path, source: &str, hash: usize) -> Option<Module> {
    let mode = mode_from_path(&path.to_string_lossy());
    if let Ok(module) = parse(source, mode) {
        return Some(module);
    }
    let line_start = source[..hash].rfind('\n').map(|index| index + 1).unwrap_or(0);
    let mut head = source[..line_start].to_string();
    if !head.ends_with('\n') {
        head.push('\n');
    }
    head.push_str("  dup\n");
    parse(&head, mode).ok()
}

fn collect_scheme(scheme: &Scheme, out: &mut Vec<(String, String)>) {
    collect_stack(&scheme.effect.input, out);
    collect_stack(&scheme.effect.output, out);
    for constraint in &scheme.constraints {
        for arg in &constraint.args {
            collect_type(arg, out);
        }
    }
}

fn collect_stack(stack: &Stack, out: &mut Vec<(String, String)>) {
    for item in &stack.items {
        collect_type(item, out);
    }
}

fn collect_type(ty: &Type, out: &mut Vec<(String, String)>) {
    match ty {
        Type::Record(row) => {
            for (name, field) in &row.fields {
                out.push((name.clone(), type_string(field)));
                collect_type(field, out);
            }
            if let Some(ty) = row.tail_ty() {
                collect_type(ty, out);
            }
        }
        Type::App(head, args) => {
            collect_type(head, out);
            for arg in args {
                collect_type(arg, out);
            }
        }
        Type::Fun(effect) => {
            collect_stack(&effect.input, out);
            collect_stack(&effect.output, out);
        }
        Type::Var(_) | Type::Ctor(_) => {}
    }
}

fn top_slot(stack: &str) -> Option<&str> {
    let bytes = stack.as_bytes();
    let mut depth = 0i32;
    let mut start = 0;
    let mut last = None;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'{' | b'(' => depth += 1,
            b'}' | b')' => depth -= 1,
            b' ' if depth == 0 => {
                if index > start {
                    last = Some(&stack[start..index]);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < bytes.len() {
        last = Some(&stack[start..]);
    }
    last.map(str::trim).filter(|slot| !slot.is_empty())
}

/// The slot under the top. `=name` reads the record there: the new value is on top.
fn under_top(stack: &str) -> Option<&str> {
    let bytes = stack.as_bytes();
    let mut depth = 0i32;
    let mut start = 0;
    let mut previous = None;
    let mut last = None;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'{' | b'(' => depth += 1,
            b'}' | b')' => depth -= 1,
            b' ' if depth == 0 => {
                if index > start {
                    previous = last;
                    last = Some(&stack[start..index]);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < bytes.len() {
        previous = last;
    }
    previous.map(str::trim).filter(|slot| !slot.is_empty())
}

fn record_fields(ty: &str) -> Option<Vec<(String, String)>> {
    let ty = ty.trim();
    if !ty.starts_with('{') || !ty.ends_with('}') {
        return None;
    }
    let inner = &ty[1..ty.len() - 1];
    let bytes = inner.as_bytes();
    let mut fields = Vec::new();
    let mut depth = 0i32;
    let mut part_start = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'(' => depth += 1,
            b'}' | b')' => depth -= 1,
            b'|' if depth == 0 => break,
            b',' if depth == 0 => {
                take_field(&inner[part_start..index], &mut fields);
                part_start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    take_field(&inner[part_start..index], &mut fields);
    Some(fields)
}

fn take_field(part: &str, out: &mut Vec<(String, String)>) {
    let part = part.trim();
    if part.is_empty() {
        return;
    }
    let Some(colon) = part.find(" : ") else {
        return;
    };
    let name = part[..colon].trim();
    if name.is_empty() || !name.bytes().all(is_ident_byte) {
        return;
    }
    out.push((name.to_string(), part[colon + 3..].trim().to_string()));
}

fn is_open_stack(top: &str) -> bool {
    top == "_" || top == ".._"
}

fn is_ident_start_byte(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn unclosed_import(source: &str, offset: usize) -> Option<Span> {
    let head = &source[..offset];
    let start = head.rfind('~')?;
    let after = &head[start + '~'.len_utf8()..];
    let trimmed = after.trim_start_matches([' ', '\t']);
    if !trimmed.starts_with('"') || trimmed[1..].contains('"') {
        return None;
    }
    let quote_at = head.len() - trimmed.len();
    Some(Span::new(quote_at + 1, offset))
}

fn is_var(name: &str) -> bool {
    name.chars().next().is_some_and(|ch| ch.is_ascii_lowercase())
}

fn is_upper(name: &str) -> bool {
    name.chars().next().is_some_and(|ch| ch.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{complete, CompletionKind};

    struct Cleanup(PathBuf);

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn at(source: &str, needle: &str) -> (u32, u32) {
        let offset = source.rfind(needle).unwrap() + needle.len();
        let line = source[..offset].bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = source[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0);
        let character = source[line_start..offset].encode_utf16().count() as u32;
        (line, character)
    }

    fn labels(source: &str, needle: &str) -> Vec<String> {
        let (line, character) = at(source, needle);
        complete(source, Path::new("M.ply"), line, character)
            .into_iter()
            .map(|item| item.label)
            .collect()
    }

    #[test]
    fn completes_words_in_the_body() {
        let source = ".. Copy.\n: double ( Int -- Int )\n  du";
        let (line, character) = at(source, "du");
        let items = complete(source, Path::new("M.ply"), line, character);
        let dup = items.iter().find(|item| item.label == "dup").unwrap();
        assert_eq!(dup.detail, "..s a -- ..s a a");
        assert_eq!(dup.kind, CompletionKind::Word);
        assert_eq!(dup.insert, "dup");
        assert_eq!(
            dup.documentation,
            "Copies the top value. On `x y` the result is `x y y`."
        );
        let double = items.iter().find(|item| item.label == "double").unwrap();
        assert_eq!(double.documentation, "Copy.");
        assert!(!items.iter().any(|item| item.label == "iadd"));
        assert!(items.iter().any(|item| item.label == "true"));
        assert!(!items.iter().any(|item| item.label == "match"));
        assert!(!items.iter().any(|item| item.label == "Int"));
    }

    #[test]
    fn completes_types_in_the_signature_and_constructor_fields() {
        let source = "; Maybe a = Nothing | Just a\n\n: use ( a. (M\n  drop\n";
        let types = labels(source, "(M");
        assert!(types.iter().any(|label| label == "Maybe"), "{types:?}");
        assert!(types.iter().any(|label| label == "a"), "{types:?}");
        assert!(types.iter().any(|label| label == "Int"), "{types:?}");
        assert!(!types.iter().any(|label| label == "dup"), "{types:?}");
        let fields = labels("; Maybe a = Just ", "Just ");
        assert!(fields.iter().any(|label| label == "a"), "{fields:?}");
        assert!(fields.iter().any(|label| label == "Int"), "{fields:?}");
        assert!(!fields.iter().any(|label| label == "Nothing"), "{fields:?}");
    }

    #[test]
    fn completes_a_class_and_its_method() {
        let source = ".. Equality.\n& Eq a\n  eq ( a a -- Bool )\n\n: use ( E Int -- Bool )\n  e";
        let (line, character) = at(source, "( E");
        let types = complete(source, Path::new("M.ply"), line, character);
        let eq_class = types.iter().find(|item| item.label == "Eq").expect("class Eq");
        assert_eq!(eq_class.documentation, "Equality.");
        let (line, character) = at(source, "\n  e");
        let items = complete(source, Path::new("M.ply"), line, character);
        let eq = items.iter().find(|item| item.label == "eq").expect("method eq");
        assert_eq!(eq.detail, "Eq a => a a -- Bool");
        assert_eq!(eq.kind, CompletionKind::Word);
        assert_eq!(eq.documentation, "Equality.");
    }

    #[test]
    fn completes_keywords_and_skips_comments() {
        let keywords = labels("de", "de");
        assert!(!keywords.iter().any(|label| label == "def"), "{keywords:?}");
        assert!(keywords.iter().any(|label| label == "~"), "{keywords:?}");
        assert!(keywords.iter().any(|label| label == ";"), "{keywords:?}");
        assert!(keywords.iter().any(|label| label == "-"), "{keywords:?}");
        assert!(keywords.iter().any(|label| label == "&"), "{keywords:?}");
        assert!(keywords.iter().any(|label| label == "#"), "{keywords:?}");
        assert!(keywords.iter().any(|label| label == ":"), "{keywords:?}");
        assert!(!keywords.iter().any(|label| label == "foreign"), "{keywords:?}");
        assert!(!keywords.iter().any(|label| label == "private"), "{keywords:?}");
        assert!(!keywords.iter().any(|label| label == "class"), "{keywords:?}");
        assert!(!keywords.iter().any(|label| label == "instance"), "{keywords:?}");
        assert!(!keywords.iter().any(|label| label == "import"), "{keywords:?}");
        assert!(!keywords.iter().any(|label| label == "dup"), "{keywords:?}");
        let after_colon = labels(": ", ": ");
        assert!(after_colon.iter().any(|label| label == "foreign"), "{after_colon:?}");
        assert!(after_colon.iter().any(|label| label == "private"), "{after_colon:?}");
        let after_foreign = labels(": foreign ", ": foreign ");
        assert_eq!(after_foreign, vec!["private".to_string()]);
        let after_private = labels(": private ", ": private ");
        assert_eq!(after_private, vec!["foreign".to_string()]);
        assert!(labels(": foreign private ", ": foreign private ").is_empty());
        assert!(labels(": private foreign ", ": private foreign ").is_empty());
        let again = ": foreign read ( -- Str )\n\n: ";
        let after_next = labels(again, "\n\n: ");
        assert!(after_next.iter().any(|label| label == "foreign"), "{after_next:?}");
        assert!(after_next.iter().any(|label| label == "private"), "{after_next:?}");
        let (line, character) = at(": ", ": ");
        let iface = complete(": ", Path::new("M.d.ply"), line, character);
        assert!(iface.iter().any(|item| item.label == "foreign"), "{iface:?}");
        assert!(iface.iter().any(|item| item.label == "private"), "{iface:?}");
        let (line, character) = at(": foreign ", ": foreign ");
        let iface_foreign = complete(": foreign ", Path::new("M.d.ply"), line, character);
        assert_eq!(
            iface_foreign.iter().map(|item| item.label.as_str()).collect::<Vec<_>>(),
            vec!["private"]
        );
        assert!(labels(": f ( Int -- Int )\n  dup . dr", ". dr").is_empty());
    }

    #[test]
    fn completes_an_import_from_the_package() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("plylang-complete-import-{nanos}"));
        let _cleanup = Cleanup(root.clone());
        fs::create_dir_all(root.join("src/Data")).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "@algosail/c",
  "ply": { "source": "src", "out": "dist", "language": 1 },
  "exports": {
    "./Data/Maybe": { "ply": "./dist/Data/Maybe.d.ply" },
    "./Try": { "ply": "./dist/Try.d.ply" }
  }
}"#,
        )
        .unwrap();
        fs::write(
            root.join("src/Data/Maybe.ply"),
            "\
; Maybe a = Nothing | Just a

: fromMaybe ( a (Maybe a) -- a )
  Nothing ->
  Just -> nip

: mapMaybe ( (Maybe a) (a -- b) -- (Maybe b) )
  Nothing q -> q drop Nothing
  (Just x) q -> x q call Just
",
        )
        .unwrap();
        let path = root.join("src/Try.ply");
        fs::write(&path, "").unwrap();
        let (line, character) = at("~ \"\"\n", "~ \"");
        let paths = complete("~ \"\"\n", &path, line, character);
        assert!(paths.iter().any(|item| item.label == "Data/Maybe"), "{paths:?}");
        assert!(paths.iter().all(|item| item.label != "Try"), "{paths:?}");
        assert_eq!(paths.iter().find(|item| item.label == "Data/Maybe").unwrap().insert, "Data/Maybe");

        let source = "~ \"Data/Maybe\" (J)\n";
        let (line, character) = at(source, "(J");
        let names = complete(source, &path, line, character);
        assert!(names.iter().any(|item| item.label == "Just"), "{names:?}");
        assert!(names.iter().any(|item| item.label == "fromMaybe"), "{names:?}");
        assert!(names.iter().any(|item| item.label == "Maybe"), "{names:?}");
        assert!(names.iter().any(|item| item.label == ".."), "{names:?}");

        let source = "~ \"Data/Maybe\" (Just)\n\n: use ( Int -- Int )\n  J";
        let (line, character) = at(source, "\n  J");
        let words = complete(source, &path, line, character);
        assert!(words.iter().any(|item| item.label == "Just"), "{words:?}");
        assert!(!words.iter().any(|item| item.label == "Nothing"), "{words:?}");
        assert!(words.iter().any(|item| item.label == "dup"), "{words:?}");

        let source = "~ \"Data/Maybe\" (Maybe)\n\n: use ( M\n  drop\n";
        let (line, character) = at(source, "( M");
        let types = complete(source, &path, line, character);
        assert!(types.iter().any(|item| item.label == "Maybe"), "{types:?}");
    }

    #[test]
    fn hides_a_private_word_and_keeps_its_doc_on_the_public_one() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("plylang-complete-{nanos}"));
        let _cleanup = Cleanup(root.clone());
        fs::create_dir_all(root.join("src/Data")).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{
  "name": "@algosail/c",
  "ply": { "source": "src", "out": "dist", "language": 1 },
  "exports": { "./Data/Num": { "ply": "./dist/Data/Num.d.ply" } }
}"#,
        )
        .unwrap();
        fs::write(
            root.join("src/Data/Num.ply"),
            ".. Add two integers.\n: add ( Int Int -- Int )\n  drop\n\n: private hidden ( Int -- Int )\n  dup drop\n",
        )
        .unwrap();
        let app = root.join("src/App.ply");
        fs::write(&app, "").unwrap();
        let source = "~ \"Data/Num\" ()\n";
        let (line, character) = at(source, "(");
        let names = complete(source, &app, line, character);
        assert!(names.iter().any(|item| item.label == "add"), "{names:?}");
        let add = names.iter().find(|item| item.label == "add").unwrap();
        assert_eq!(add.documentation, "Add two integers.");
        assert_eq!(add.detail, "Int Int -- Int");
        assert!(!names.iter().any(|item| item.label == "hidden"), "{names:?}");
    }

    #[test]
    fn completes_record_fields() {
        let source = ": get ( { age : Int, name : Str } -- { age : Int, name : Str } )\n  1 =";
        let (line, character) = at(source, "1 =");
        let items = complete(source, Path::new("M.ply"), line, character);
        assert_eq!(
            items.iter().map(|item| item.label.clone()).collect::<Vec<_>>(),
            vec!["age".to_string(), "name".to_string()]
        );
        let name = items.iter().find(|item| item.label == "name").unwrap();
        assert_eq!(name.insert, "name");
        assert_eq!(name.detail, "Str");
        assert_eq!(name.kind, CompletionKind::Field);
        assert_eq!(&source[name.replace.start as usize..name.replace.end as usize], "");

        let source = ": get ( { age : Int, name : Str } -- { age : Int, name : Str } )\n  1 =na";
        let (line, character) = at(source, "=na");
        let items = complete(source, Path::new("M.ply"), line, character);
        let name = items.iter().find(|item| item.label == "name").unwrap();
        assert_eq!(&source[name.replace.start as usize..name.replace.end as usize], "na");
        assert_eq!(name.insert, "name");

        let open = ": get ( a. { name : a | r } -- { name : a | r } )\n  1 =";
        let name = complete(open, Path::new("M.ply"), at(open, "1 =").0, at(open, "1 =").1)
            .into_iter()
            .find(|item| item.label == "name")
            .unwrap();
        assert_eq!(name.detail, "a");

        let reading = ": get ( { age : Int, name : Str } -- Str )\n  na";
        let reader = complete(reading, Path::new("M.ply"), at(reading, "na").0, at(reading, "na").1)
            .into_iter()
            .find(|item| item.label == "/name")
            .unwrap();
        assert_eq!(reader.insert, "/name");
        assert_eq!(reader.detail, "Str");
        assert_eq!(reader.kind, CompletionKind::Field);

        let slash = ": get ( { age : Int, name : Str } -- Str )\n  /na";
        let reader = complete(slash, Path::new("M.ply"), at(slash, "/na").0, at(slash, "/na").1)
            .into_iter()
            .find(|item| item.label == "/name")
            .unwrap();
        assert_eq!(reader.insert, "/name");
        assert_eq!(&slash[reader.replace.start as usize..reader.replace.end as usize], "/na");

        assert!(labels(": bad ( Int -- Int )\n  1 =", "1 =").is_empty());
        assert!(labels(": get ( { name : Str } -- Str )\n  /name =", "/name =").is_empty());
        assert_eq!(
            labels(": get ( { name : Str } -- Str )\n  [ = ] call\n", "[ ="),
            vec!["name".to_string()]
        );
        assert_eq!(
            labels(
                "; Person = Mk { name : Str, age : Int }\n\n: fromMk ( Person -- Str )\n  Mk -> 1 =",
                "Mk -> 1 ="
            ),
            vec!["age".to_string(), "name".to_string()]
        );
        assert!(labels(": get ( { name : Str } -- Str )\n  = ! age", "= !").is_empty());
    }
}
