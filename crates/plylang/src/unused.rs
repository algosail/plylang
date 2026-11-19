//! Imported names the module never mentions.
//!
//! A word, type, class, or place on a `~` line fades when this file does not
//! use it. A constructor is a word. A synonym is a type. A bare `~ "Path"`
//! brings instances and has no name to fade. An alias fades when no
//! `Alias:name` is written. A name in `@`, a signature, a pattern, a place
//! sigil, or a `?` line is used. `plylang check` does not see these notes.

use std::collections::HashSet;

use crate::ast::{
    Constraint, Exports, Import, Item, Module, Pattern, Scheme, Stack, Term, TermNode, Type,
};
use crate::check::{KnownModule, ResolvedImport};
use crate::diagnostic::Hint;

pub(crate) fn unused_imports(module: &Module, resolved: &[ResolvedImport<'_>]) -> Vec<Hint> {
    let uses = uses_of(module);
    let mut hints = Vec::new();
    for import in resolved {
        let Some(provider) = import.known.first() else {
            continue;
        };
        fade_import(import.import, provider, import.known, &uses, &mut hints);
    }
    hints
}

fn fade_import(
    import: &Import,
    provider: &KnownModule,
    known: &[KnownModule],
    uses: &HashSet<String>,
    hints: &mut Vec<Hint>,
) {
    if let Some(alias) = &import.alias {
        if !alias_is_used(alias, uses) {
            if let Some(span) = import.alias_span {
                hints.push(Hint {
                    span,
                    message: format!("unused import {alias}"),
                    related: None,
                    unnecessary: true,
                });
            }
        }
        return;
    }
    let mut seen = HashSet::new();
    for name in &import.names {
        if !seen.insert(name.local.clone()) || uses.contains(&name.local) {
            continue;
        }
        let Some(kind) = kind_of(&provider.origin, &name.remote, known, &mut HashSet::new()) else {
            continue;
        };
        hints.push(Hint {
            span: name.span,
            message: format!("unused {} {}", kind.label(), name.local),
            related: None,
            unnecessary: true,
        });
    }
}

fn alias_is_used(alias: &str, uses: &HashSet<String>) -> bool {
    let mut prefix = String::from(alias);
    prefix.push(':');
    uses.iter().any(|name| {
        name.strip_prefix(&prefix)
            .is_some_and(|rest| !rest.is_empty())
    })
}

#[derive(Clone, Copy)]
enum Kind {
    Word,
    Type,
    Class,
    Place,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Word => "word",
            Kind::Type => "type",
            Kind::Class => "class",
            Kind::Place => "place",
        }
    }
}

/// What `origin` publishes as `name`: a definition there, or a re-export.
fn kind_of(
    origin: &str,
    name: &str,
    known: &[KnownModule],
    stack: &mut HashSet<String>,
) -> Option<Kind> {
    if !stack.insert(origin.to_string()) {
        return None;
    }
    let known_module = known.iter().find(|item| item.origin == origin)?;
    let module = &known_module.module;
    if !on_surface(module, name) {
        return None;
    }
    if let Some(kind) = defined_kind(module, name) {
        return Some(kind);
    }
    for (index, child) in module.imports.iter().enumerate() {
        let Some(remote) = child.remote_of(name) else {
            continue;
        };
        let Some(dep) = known_module.deps.get(index) else {
            continue;
        };
        return kind_of(dep, remote, known, stack);
    }
    None
}

fn on_surface(module: &Module, name: &str) -> bool {
    match &module.exports {
        Exports::List(list) => list.has(name),
        Exports::Closed => false,
    }
}

fn defined_kind(module: &Module, name: &str) -> Option<Kind> {
    for item in &module.items {
        match item {
            Item::Data(data) if data.name == name => return Some(Kind::Type),
            Item::Synonym(synonym) if synonym.name == name => return Some(Kind::Type),
            Item::Def(def) if def.name == name => return Some(Kind::Word),
            Item::Place(place) if place.name == name => return Some(Kind::Place),
            Item::Class(class) if class.name == name => return Some(Kind::Class),
            _ => {}
        }
    }
    for item in &module.items {
        match item {
            Item::Data(data) if data.ctors.iter().any(|ctor| ctor.name == name) => {
                return Some(Kind::Word);
            }
            Item::Class(class) if class.methods.iter().any(|method| method.name == name) => {
                return Some(Kind::Word);
            }
            _ => {}
        }
    }
    None
}

fn uses_of(module: &Module) -> HashSet<String> {
    let mut uses = HashSet::new();
    if let Exports::List(list) = &module.exports {
        for name in &list.names {
            uses.insert(name.name.clone());
        }
    }
    for item in &module.items {
        match item {
            Item::Data(data) => {
                for ctor in &data.ctors {
                    for field in &ctor.fields {
                        walk_type(&field.ty, &mut uses);
                    }
                }
            }
            Item::Synonym(synonym) => walk_type(&synonym.rhs, &mut uses),
            Item::Def(def) => {
                walk_scheme(&def.scheme, &mut uses);
                walk_terms(&def.body, &mut uses);
                for clause in &def.clauses {
                    for pattern in &clause.patterns {
                        walk_pattern(pattern, &mut uses);
                    }
                    walk_terms(&clause.body, &mut uses);
                }
            }
            Item::Place(place) => {
                walk_type(&place.ty, &mut uses);
                walk_terms(&place.body, &mut uses);
            }
            Item::Class(class) => {
                for constraint in &class.supers {
                    walk_constraint(constraint, &mut uses);
                }
                for method in &class.methods {
                    walk_scheme(&method.scheme, &mut uses);
                }
            }
            Item::Instance(instance) => {
                for name in &instance.class_names {
                    uses.insert(name.clone());
                }
                for ty in &instance.args {
                    walk_type(ty, &mut uses);
                }
                for binding in &instance.methods {
                    uses.insert(binding.word.clone());
                }
            }
            Item::Probe(probe) => {
                walk_terms(&probe.picture, &mut uses);
                walk_terms(&probe.body, &mut uses);
            }
        }
    }
    uses
}

fn walk_scheme(scheme: &Scheme, uses: &mut HashSet<String>) {
    for constraint in &scheme.constraints {
        walk_constraint(constraint, uses);
    }
    walk_stack(&scheme.effect.input, uses);
    walk_stack(&scheme.effect.output, uses);
}

fn walk_constraint(constraint: &Constraint, uses: &mut HashSet<String>) {
    uses.insert(constraint.class_name.clone());
    for ty in &constraint.args {
        walk_type(ty, uses);
    }
}

fn walk_stack(stack: &Stack, uses: &mut HashSet<String>) {
    for ty in &stack.items {
        walk_type(ty, uses);
    }
}

fn walk_type(ty: &Type, uses: &mut HashSet<String>) {
    match ty {
        Type::Var(_) => {}
        Type::Ctor(name) => {
            uses.insert(name.clone());
        }
        Type::App(head, args) => {
            walk_type(head, uses);
            for arg in args {
                walk_type(arg, uses);
            }
        }
        Type::Fun(effect) => {
            walk_stack(&effect.input, uses);
            walk_stack(&effect.output, uses);
        }
    }
}

fn walk_terms(terms: &[Term], uses: &mut HashSet<String>) {
    for term in terms {
        match &term.node {
            TermNode::Word(name) | TermNode::PlaceSet(name) | TermNode::PlaceBind(name) => {
                uses.insert(name.clone());
            }
            TermNode::Quote(inner) | TermNode::Array(inner) => walk_terms(inner, uses),
            TermNode::Bind(_) | TermNode::Field(_) | TermNode::FieldSet(_) | TermNode::Lit(_) => {}
        }
    }
}

fn walk_pattern(pattern: &Pattern, uses: &mut HashSet<String>) {
    match pattern {
        Pattern::Unpack(name) | Pattern::Open(name) => {
            uses.insert(name.clone());
        }
        Pattern::Ctor(name, fields) => {
            uses.insert(name.clone());
            for field in fields {
                walk_pattern(field, uses);
            }
        }
        Pattern::Array(items) => {
            for item in items {
                walk_pattern(item, uses);
            }
        }
        Pattern::Wild | Pattern::Bind(_, _) | Pattern::Lit(_) | Pattern::Spill => {}
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::{buffer_notes, check_buffer};

    struct Temp(PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "plylang-unused-{label}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(path.join("src")).unwrap();
            fs::write(
                path.join("package.json"),
                "{\n  \"ply\": { \"language\": 1, \"source\": \"src\", \"out\": \"dist\" }\n}\n",
            )
            .unwrap();
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

    const HOST: &str = "\
@ answer, spare, Box, Show, base, Just, Age

: answer ( -- Num )
  1

: spare ( -- Num )
  1

; Box = Box

& Show a
  show ( a -- Str )

= base Num
  0

; Wrap = Just

- Age Num
";

    fn messages(source: &str, temp: &Temp) -> Vec<String> {
        temp.write("src/Host.ply", HOST);
        temp.write("src/App.ply", source);
        let path = temp.0.join("src/App.ply");
        let notes = buffer_notes(&path, source);
        notes
            .hints
            .iter()
            .map(|hint| {
                let text = &source[hint.span.start as usize..hint.span.end as usize];
                format!("{text}: {}", hint.message)
            })
            .collect()
    }

    #[test]
    fn fades_an_unused_word_type_class_and_place() {
        let temp = Temp::new("kinds");
        temp.write("src/Instances.ply", "@\n");
        let source = "\
@ kept
~ \"Host\" answer, spare : echo, Box, Show, base, Just, Age
~ \"Instances\"
~ \"Host\" : Host

: kept ( a. Show a => a -- a )
  dup drop

: boxed ( Box -- Box )
  dup drop
";
        let temp_messages = messages(source, &temp);
        assert_eq!(
            temp_messages,
            vec![
                "answer: unused word answer",
                "echo: unused word echo",
                "base: unused place base",
                "Just: unused word Just",
                "Age: unused type Age",
                "Host: unused import Host",
            ],
            "{temp_messages:?}"
        );
        let path = temp.0.join("src/App.ply");
        check_buffer(&path, source)
            .unwrap_or_else(|error| panic!("{}", error.render("App.ply", source)));
    }

    #[test]
    fn a_check_an_export_and_a_qualified_name_count_as_uses() {
        let temp = Temp::new("uses");
        let source = "\
@ answer
~ \"Host\" answer, spare, base, Just
~ \"Host\" : Host

: write ( Num -- )
  =base

: build ( -- Num )
  Just drop Host:answer

? \"spare\" < 1 > [
  spare
]
";
        let found = messages(source, &temp);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_type_error_still_fades_the_unused_import() {
        let temp = Temp::new("beside-error");
        let source = "\
@
~ \"Host\" spare

: bad ( Int -- Int )
  dup
";
        let path = temp.0.join("src/App.ply");
        temp.write("src/Host.ply", HOST);
        temp.write("src/App.ply", source);
        let notes = buffer_notes(&path, source);
        assert!(notes.error.is_some(), "the word still fails");
        assert_eq!(
            notes
                .hints
                .iter()
                .map(|hint| hint.message.as_str())
                .collect::<Vec<_>>(),
            vec!["unused word spare"]
        );
        assert!(check_buffer(&path, source).is_err());
    }

    #[test]
    fn a_reexported_type_fades_when_this_file_does_not_name_it() {
        let temp = Temp::new("reexport");
        temp.write("src/Data.ply", "@ Box\n\n; Box = Box\n");
        temp.write("src/Host.ply", "@ Box\n~ \"Data\" Box\n");
        let source = "@\n~ \"Host\" Box\n\n: main ( -- Unit )\n  unit\n";
        temp.write("src/App.ply", source);
        let notes = buffer_notes(&temp.0.join("src/App.ply"), source);
        assert!(notes.error.is_none(), "{:?}", notes.error);
        assert_eq!(
            notes
                .hints
                .iter()
                .map(|hint| hint.message.as_str())
                .collect::<Vec<_>>(),
            vec!["unused type Box"]
        );
        let host = buffer_notes(&temp.0.join("src/Host.ply"), "@ Box\n~ \"Data\" Box\n");
        assert!(host.hints.is_empty(), "{:?}", host.hints);
    }
}
