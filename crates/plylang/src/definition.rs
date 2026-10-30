//! Go to definition. `line` and `character` are the 0-based UTF-16 position LSP sends.
//! A builtin has no file. This buffer is the current module. Another module is the file on disk.

use std::path::{Path, PathBuf};

use crate::ast::{Clause, Import, Item, Module, Term, TermNode};
use crate::build::{load_surface, provider_file, read_module};
use crate::surface::{self, Found};
use crate::complete::{is_import_path, name_space_at, NameSpace};
use crate::lexer::tokenize;
use crate::parser::parse;
use crate::paths::mode_from_path;
use crate::span::{byte_offset, Span};
use crate::token::{Token, TokenKind};

pub struct Definition {
    pub path: PathBuf,
    pub span: Span,
}

#[derive(Clone, Copy)]
enum Target {
    Type,
    Value,
}

/// Where `name` under the cursor is declared. `None` when the name is a builtin or unknown.
pub fn definition(source: &str, path: &Path, line: u32, character: u32) -> Option<Definition> {
    let offset = byte_offset(source, line, character);
    let tokens = tokenize(source).ok()?;
    let path_text = path.to_string_lossy();
    let module = parse(source, mode_from_path(&path_text)).ok()?;
    let index = tokens.iter().position(|token| {
        token.span.start <= offset
            && offset < token.span.end
            && matches!(token.kind, TokenKind::Ident(_) | TokenKind::String(_))
    })?;
    match &tokens[index].kind {
        TokenKind::String(import_path) => import_definition(path, import_path, &tokens, index),
        TokenKind::Ident(name) => {
            let space = name_space_at(&tokens, index);
            resolve(path, &module, &tokens, offset, name, space)
        }
        _ => None,
    }
}

fn import_definition(
    path: &Path,
    import_path: &str,
    tokens: &[Token],
    index: usize,
) -> Option<Definition> {
    if !is_import_path(tokens, index) {
        return None;
    }
    let file = provider_file(path, import_path)?;
    let module = read_module(&file)?;
    Some(Definition {
        path: file,
        span: module_anchor(&module),
    })
}

/// First declaration in the provider. An instance has no name of its own.
fn module_anchor(module: &Module) -> Span {
    for item in &module.items {
        match item {
            Item::Data(data) => return data.name_span,
            Item::Synonym(synonym) => return synonym.name_span,
            Item::Def(def) => return def.name_span,
            Item::Class(class) => return class.name_span,
            Item::Instance(instance) => return instance.span,
        }
    }
    Span::new(0, 0)
}

fn resolve(
    path: &Path,
    module: &Module,
    tokens: &[Token],
    offset: u32,
    name: &str,
    space: NameSpace,
) -> Option<Definition> {
    match space {
        NameSpace::Type => {
            local_type(path, module, name).or_else(|| imported(path, module, name, Target::Type))
        }
        NameSpace::Value => local_value(path, module, tokens, offset, name)
            .or_else(|| imported(path, module, name, Target::Value))
            .or_else(|| local_type(path, module, name))
            .or_else(|| imported(path, module, name, Target::Type)),
        NameSpace::Either if is_upper(name) => local_type(path, module, name)
            .or_else(|| imported(path, module, name, Target::Type))
            .or_else(|| local_value(path, module, tokens, offset, name))
            .or_else(|| imported(path, module, name, Target::Value)),
        NameSpace::Either => local_value(path, module, tokens, offset, name)
            .or_else(|| imported(path, module, name, Target::Value))
            .or_else(|| local_type(path, module, name))
            .or_else(|| imported(path, module, name, Target::Type)),
    }
}

fn local_value(
    path: &Path,
    module: &Module,
    tokens: &[Token],
    offset: u32,
    name: &str,
) -> Option<Definition> {
    local_binder(module, tokens, offset, name)
        .or_else(|| def_span(module, name))
        .or_else(|| ctor_span(module, name))
        .or_else(|| method_span(module, name))
        .map(|span| here(path, span))
}

fn local_type(path: &Path, module: &Module, name: &str) -> Option<Definition> {
    data_span(module, name)
        .or_else(|| synonym_span(module, name))
        .or_else(|| class_span(module, name))
        .map(|span| here(path, span))
}

fn here(path: &Path, span: Span) -> Definition {
    Definition {
        path: path.to_path_buf(),
        span,
    }
}

fn imported(from: &Path, module: &Module, name: &str, target: Target) -> Option<Definition> {
    for import in &module.imports {
        let Some(file) = provider_file(from, &import.path) else {
            continue;
        };
        let Some(provider) = read_module(&file) else {
            continue;
        };
        if !import_offers(import, name) {
            continue;
        }
        let mut load = |start: &Path, path: &str| load_surface(start, path);
        let Some((decl, found)) = surface::lookup(&provider, &file, name, &mut load) else {
            continue;
        };
        if let Some(span) = found_span(&found, name, target) {
            return Some(Definition { path: decl, span });
        }
    }
    None
}

fn import_offers(import: &Import, name: &str) -> bool {
    import.all || import.names.iter().any(|imported| imported == name)
}

fn found_span(found: &Found, name: &str, target: Target) -> Option<Span> {
    match (found, target) {
        (Found::Def(def), Target::Value) if def.name == name => Some(def.name_span),
        (Found::Data(data), Target::Type) if data.name == name => Some(data.name_span),
        (Found::Data(data), Target::Value) => data
            .ctors
            .iter()
            .find(|ctor| ctor.name == name)
            .map(|ctor| ctor.span),
        (Found::Synonym(synonym), Target::Type) if synonym.name == name => Some(synonym.name_span),
        (Found::Class(class), Target::Type) if class.name == name => Some(class.name_span),
        (Found::Class(class), Target::Value) => class
            .methods
            .iter()
            .find(|method| method.name == name)
            .map(|method| method.name_span),
        _ => None,
    }
}

fn def_span(module: &Module, name: &str) -> Option<Span> {
    module.items.iter().find_map(|item| match item {
        Item::Def(def) if def.name == name => Some(def.name_span),
        _ => None,
    })
}

fn data_span(module: &Module, name: &str) -> Option<Span> {
    module.items.iter().find_map(|item| match item {
        Item::Data(data) if data.name == name => Some(data.name_span),
        _ => None,
    })
}

fn synonym_span(module: &Module, name: &str) -> Option<Span> {
    module.items.iter().find_map(|item| match item {
        Item::Synonym(synonym) if synonym.name == name => Some(synonym.name_span),
        _ => None,
    })
}

fn class_span(module: &Module, name: &str) -> Option<Span> {
    module.items.iter().find_map(|item| match item {
        Item::Class(class) if class.name == name => Some(class.name_span),
        _ => None,
    })
}

fn ctor_span(module: &Module, name: &str) -> Option<Span> {
    module.items.iter().find_map(|item| match item {
        Item::Data(data) => data
            .ctors
            .iter()
            .find(|ctor| ctor.name == name)
            .map(|ctor| ctor.span),
        _ => None,
    })
}

fn method_span(module: &Module, name: &str) -> Option<Span> {
    module.items.iter().find_map(|item| match item {
        Item::Class(class) => class
            .methods
            .iter()
            .find(|method| method.name == name)
            .map(|method| method.name_span),
        _ => None,
    })
}

/// A quotation binder inside the cursor wins over the equation that contains it.
fn local_binder(module: &Module, tokens: &[Token], offset: u32, name: &str) -> Option<Span> {
    if !is_lower(name) {
        return None;
    }
    for item in &module.items {
        let Item::Def(def) = item else {
            continue;
        };
        if def.span.start > offset || offset >= def.span.end {
            continue;
        }
        if let Some(span) = binders_in(&def.body, tokens, offset, name) {
            return Some(span);
        }
        for clause in &def.clauses {
            if let Some(span) = binders_in(&clause.body, tokens, offset, name) {
                return Some(span);
            }
            if clause.span.start <= offset && offset < clause.span.end {
                if let Some(span) = clause_binder(clause, tokens, name) {
                    return Some(span);
                }
            }
        }
    }
    None
}

fn binders_in(terms: &[Term], tokens: &[Token], offset: u32, name: &str) -> Option<Span> {
    for term in terms {
        if let Some(span) = binder_in(term, tokens, offset, name) {
            return Some(span);
        }
    }
    None
}

fn binder_in(term: &Term, tokens: &[Token], offset: u32, name: &str) -> Option<Span> {
    match &term.node {
        TermNode::Quote(inner) => {
            if term.span.start <= offset && offset < term.span.end {
                if let Some(span) = binders_in(inner, tokens, offset, name) {
                    return Some(span);
                }
                if let Some(bind) = inner.iter().find(|term| match &term.node {
                    TermNode::Bind(names) => names.iter().any(|bound| bound == name),
                    _ => false,
                }) {
                    return Some(ident_in(tokens, bind.span, name));
                }
            }
            None
        }
        TermNode::Array(inner) => binders_in(inner, tokens, offset, name),
        TermNode::Record(fields) => fields
            .iter()
            .find_map(|(_, value)| binder_in(value, tokens, offset, name)),
        _ => None,
    }
}

fn clause_binder(clause: &Clause, tokens: &[Token], name: &str) -> Option<Span> {
    let arrow = tokens.iter().find(|token| {
        matches!(token.kind, TokenKind::Arrow)
            && token.span.start >= clause.span.start
            && token.span.end <= clause.span.end
    })?;
    tokens
        .iter()
        .find(|token| {
            matches!(&token.kind, TokenKind::Ident(found) if found == name)
                && token.span.start >= clause.span.start
                && token.span.end <= arrow.span.start
        })
        .map(|token| token.span)
}

fn ident_in(tokens: &[Token], within: Span, name: &str) -> Span {
    tokens
        .iter()
        .find(|token| {
            token.span.start >= within.start
                && token.span.end <= within.end
                && matches!(&token.kind, TokenKind::Ident(found) if found == name)
        })
        .map(|token| token.span)
        .unwrap_or(within)
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::definition;

    #[test]
    fn jumps_to_a_word() {
        let source = ": double ( Int -- Int )\n  dup drop\n\n: use ( Int -- Int )\n  double\n";
        let declared = jump(source, at(source, "double"));
        let used = jump(source, at_offset(source, source.rfind("double").unwrap()));
        assert_eq!(declared.span, used.span);
        assert_eq!(declared.path, PathBuf::from("M.ply"));
        assert_eq!(slice(source, declared.span), "double");
        assert!(definition(source, Path::new("M.ply"), at(source, "dup").0, at(source, "dup").1).is_none());
        assert!(definition(source, Path::new("M.ply"), at(source, "Int").0, at(source, "Int").1).is_none());
    }

    #[test]
    fn splits_a_type_and_its_constructor() {
        let source = "; Maybe a = Nothing | Just a\n\n: none ( -- (Maybe Int) )\n  Nothing\n";
        let declared = jump(source, at(source, "Maybe"));
        let scheme = jump(source, at_offset(source, source.find("(Maybe").unwrap() + 1));
        assert_eq!(declared.span, scheme.span);
        assert_eq!(slice(source, scheme.span), "Maybe");
        let ctor = jump(source, at(source, "Nothing"));
        let used = jump(source, at_offset(source, source.rfind("Nothing").unwrap()));
        assert_eq!(ctor.span, used.span);
        assert_eq!(slice(source, used.span), "Nothing");
        assert_ne!(scheme.span, used.span);

        let source = "; Box a = Box\n\n: pass ( a. a -- (Box a) )\n  Box\n";
        let type_name = jump(source, at(source, "Box"));
        let in_scheme = jump(source, at_offset(source, source.find("(Box").unwrap() + 1));
        let ctor = jump(source, at_offset(source, source.find("= Box").unwrap() + 2));
        let used = jump(source, at_offset(source, source.rfind("Box").unwrap()));
        assert_eq!(type_name.span, in_scheme.span);
        assert_eq!(slice(source, type_name.span), "Box");
        assert_eq!(ctor.span, used.span);
        assert_ne!(type_name.span, ctor.span);
    }

    #[test]
    fn jumps_to_a_foreign_type() {
        let source = "; foreign Hub a\n\n: pass ( (Hub a) -- (Hub a) )\n  hub -> hub\n";
        let declared = jump(source, at(source, "Hub"));
        let used = jump(source, at_offset(source, source.find("(Hub").unwrap() + 1));
        assert_eq!(declared.span, used.span);
        assert_eq!(slice(source, used.span), "Hub");
    }

    #[test]
    fn jumps_to_a_synonym() {
        let source = "; Person = Person name : Str, age : Num\n- Human Person\n\n: older ( Human -- Human )\n  dup name>>\n";
        let declared = jump(source, at(source, "Human"));
        let used = jump(source, at(source, "Human --"));
        assert_eq!(declared.span, used.span);
        assert_eq!(slice(source, used.span), "Human");
        let field = at(source, "name>>");
        assert!(definition(source, Path::new("M.ply"), field.0, field.1).is_none());
    }

    #[test]
    fn jumps_to_a_binder() {
        let source = ": pass ( Int -- Int )\n  n -> n\n";
        let used = jump(source, at_offset(source, source.rfind('n').unwrap()));
        assert_eq!(used.span.start as usize, source.find("n ->").unwrap());
        assert_eq!(slice(source, used.span), "n");

        let source = ": pass ( Int Int -- Int )\n  [ n m -> m ]\n";
        let used = jump(source, at_offset(source, source.rfind('m').unwrap()));
        assert_eq!(used.span.start as usize, source.find("m ->").unwrap());
        assert_eq!(slice(source, used.span), "m");

        let source = ": pass ( Int -- Int )\n  n -> [ n -> n ]\n";
        let used = jump(source, at_offset(source, source.rfind('n').unwrap()));
        assert_eq!(used.span.start as usize, source.find("[ n").unwrap() + 2);
        assert_ne!(used.span.start as usize, source.find("n ->").unwrap());
    }

    #[test]
    fn a_type_variable_stays_in_the_scheme() {
        let source = ": a ( Int -- Int )\n  dup drop\n\n: pass ( a. a -- a )\n  drop\n";
        let word = jump(source, at_offset(source, source.find(": a").unwrap() + 2));
        assert_eq!(slice(source, word.span), "a");
        assert_eq!(word.span.start as usize, source.find(": a").unwrap() + 2);
        let scheme_at = source.find("( a. a -- a )").unwrap();
        let scheme = "( a. a -- a )";
        for (index, ch) in scheme.char_indices() {
            if ch == 'a' {
                let (line, character) = at_offset(source, scheme_at + index);
                assert!(definition(source, Path::new("M.ply"), line, character).is_none());
            }
        }
    }

    #[test]
    fn a_word_wins_over_a_method() {
        let source = "& Eq a\n  eq ( a a -- Bool )\n\n: eq ( Int Int -- Bool )\n  drop drop true\n\n: use ( Int Int -- Bool )\n  eq\n";
        let class = jump(source, at(source, "Eq"));
        assert_eq!(slice(source, class.span), "Eq");
        let used = jump(source, at_offset(source, source.rfind("eq").unwrap()));
        assert_eq!(used.span.start as usize, source.find(": eq").unwrap() + 2);
        assert_ne!(used.span.start as usize, source.find("eq (").unwrap());
    }

    #[test]
    fn an_instance_binding_jumps_to_the_method() {
        let source = "& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqInt\n\n: use ( Int Int -- Bool )\n  eq\n";
        let method = source.find("eq (").unwrap();
        let used = jump(source, at_offset(source, source.rfind("\n  eq").unwrap() + 3));
        let bound = jump(source, at(source, "eq ="));
        assert_eq!(used.span.start as usize, method);
        assert_eq!(bound.span.start as usize, method);
        let word = source.find("eqInt").unwrap();
        let impl_use = jump(source, at_offset(source, source.rfind("eqInt").unwrap()));
        assert_eq!(impl_use.span.start as usize, word);
        assert_eq!(slice(source, impl_use.span), "eqInt");
    }

    #[test]
    fn an_import_jumps_to_the_provider() {
        let temp = Temp::new("goto");
        temp.write(
            "package.json",
            "{\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 }\n}\n",
        );
        let provider = "\
@ Hub, add

; foreign Hub a

: add ( Int Int -- Int )
  drop

: hidden ( Int -- Int )
  dup drop
";
        temp.write("src/Data/Num.ply", provider);
        let app = temp.0.join("src/App.ply");
        let source = "~ \"Data/Num\" (add)\n\n: twice ( Int -- Int )\n  dup add\n";
        let line = source.lines().nth(3).unwrap();
        let found = definition(&source, &app, 3, line.find("add").unwrap() as u32).unwrap();
        assert_eq!(found.path, temp.0.join("src/Data/Num.ply"));
        assert_eq!(slice(provider, found.span), "add");
        assert_ne!(found.span.start as usize, provider.find("hidden").unwrap());

        let path_at = at_offset(source, source.find("Data/Num").unwrap());
        let anchor = definition(&source, &app, path_at.0, path_at.1).unwrap();
        assert_eq!(anchor.path, found.path);
        assert_eq!(slice(provider, anchor.span), "Hub");

        let hub = "~ \"Data/Num\" (Hub)\n\n: pass ( (Hub a) -- (Hub a) )\n  hub -> hub\n";
        let hub_at = at_offset(hub, hub.find("(Hub").unwrap() + 1);
        let hub_def = definition(&hub, &app, hub_at.0, hub_at.1).unwrap();
        assert_eq!(hub_def.path, found.path);
        assert_eq!(hub_def.span, anchor.span);

        let all = "~ \"Data/Num\" (..)\n\n: use ( Int -- Int )\n  add\n";
        let all_add = definition(&all, &app, 3, 2).unwrap();
        assert_eq!(slice(provider, all_add.span), "add");
        let hidden = "~ \"Data/Num\" (..)\n\n: use ( Int -- Int )\n  hidden\n";
        assert!(definition(&hidden, &app, 3, 2).is_none());
        let empty = "~ \"Data/Num\" ()\n\n: use ( Int -- Int )\n  add\n";
        assert!(definition(&empty, &app, 3, 2).is_none());
    }

    #[test]
    fn a_public_import_jumps_to_the_declaring_file() {
        let temp = Temp::new("facade");
        temp.write(
            "package.json",
            "{\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 }\n}\n",
        );
        let lib = "\
@ answer, eqNum

.. A number.
: answer ( -- Num )
  1

.. Equal numbers.
: eqNum ( Num Num -- Bool )
  drop drop true

& Eq a
  eq ( a a -- Bool )

# Eq Num
  eq = eqNum
";
        temp.write("src/Lib.ply", lib);
        temp.write("src/Door.ply", "@ answer, eqNum\n~ \"Lib\" (answer, eqNum)\n");
        let app = temp.0.join("src/App.ply");
        let source = "~ \"Door\" (..)\n\n: use ( Num Num -- Bool )\n  eqNum\n";
        let found = definition(&source, &app, 3, 2).unwrap();
        assert_eq!(found.path, temp.0.join("src/Lib.ply"));
        assert_eq!(slice(lib, found.span), "eqNum");
        let answer = "~ \"Door\" (answer)\n\n: use ( -- Num )\n  answer\n";
        let found = definition(&answer, &app, 3, 2).unwrap();
        assert_eq!(found.path, temp.0.join("src/Lib.ply"));
        assert_eq!(slice(lib, found.span), "answer");
    }

    #[test]
    fn a_package_export_prefers_source() {
        let temp = Temp::new("vendor");
        let root = temp.0.join("node_modules/@algosail/nums");
        temp.write(
            "node_modules/@algosail/nums/package.json",
            "{\n  \"name\": \"@algosail/nums\",\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 },\n  \"exports\": { \"./Data/Num\": { \"ply\": \"./dist/Data/Num.d.ply\" } }\n}\n",
        );
        let source_text = "@ add\n\n: add ( Int Int -- Int )\n  drop\n";
        temp.write("node_modules/@algosail/nums/src/Data/Num.ply", source_text);
        let interface = "language 1\n\n@ add\n\n.. From the interface.\n\n: add ( Int Int -- Int )\n";
        temp.write(
            "node_modules/@algosail/nums/dist/Data/Num.d.ply",
            interface,
        );
        let from = temp.0.join("App.ply");
        let source = "~ \"@algosail/nums/Data/Num\" (add)\n\n: twice ( Int -- Int )\n  dup add\n";
        let found = definition(&source, &from, 3, 6).unwrap();
        assert_eq!(found.path, root.join("src/Data/Num.ply"));
        assert_eq!(found.span.start, 9);
        assert_eq!(slice(source_text, found.span), "add");

        fs::remove_file(root.join("src/Data/Num.ply")).unwrap();
        let found = definition(&source, &from, 3, 6).unwrap();
        assert_eq!(found.path, root.join("dist/Data/Num.d.ply"));
        assert_eq!(slice(interface, found.span), "add");
        assert_ne!(found.span.start, 2);
    }

    fn jump(source: &str, at: (u32, u32)) -> super::Definition {
        definition(source, Path::new("M.ply"), at.0, at.1).unwrap()
    }

    fn slice(source: &str, span: crate::span::Span) -> &str {
        &source[span.start as usize..span.end as usize]
    }

    fn at(source: &str, needle: &str) -> (u32, u32) {
        at_offset(source, source.find(needle).unwrap())
    }

    fn at_offset(source: &str, offset: usize) -> (u32, u32) {
        let line = source[..offset].bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = source[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0);
        let character = source[line_start..offset].encode_utf16().count() as u32;
        (line, character)
    }

    struct Temp(PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "plylang-{label}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, text).unwrap();
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
