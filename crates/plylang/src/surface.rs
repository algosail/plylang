//! Public surface of a module.
//!
//! A module publishes the names in its `@` list and nothing else.
//! An import brings those names into one file. Repeating one in the list
//! publishes it again. Instances are not names: every import carries them.
//! A word bound by `#` stays in the file that defines it unless that name is
//! also in the list. A file with no list publishes nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ast::{Class, Data, Def, Exports, Instance, Item, Module, Place, Synonym, Type};

pub(crate) enum Found {
    Data(Data),
    Synonym(Synonym),
    Def(Def),
    Class(Class),
    Place(Place),
}

/// `true` when this file declares `name` as a type, constructor, synonym,
/// word, class, or method. The `@` list is not consulted.
pub(crate) fn defines(module: &Module, name: &str) -> bool {
    module.items.iter().any(|item| match item {
        Item::Data(data) => data.name == name || data.ctors.iter().any(|ctor| ctor.name == name),
        Item::Synonym(synonym) => synonym.name == name,
        Item::Def(def) => def.name == name,
        Item::Place(place) => place.name == name,
        Item::Class(class) => {
            class.name == name || class.methods.iter().any(|method| method.name == name)
        }
        Item::Instance(_) | Item::Probe(_) => false,
    })
}

/// The generated JavaScript exports this word.
pub(crate) fn js_exports_def(module: &Module, def: &Def) -> bool {
    match &module.exports {
        Exports::List(list) => list.has(&def.name) || local_instance_word(module, &def.name),
        Exports::Closed => true,
    }
}

fn local_instance_word(module: &Module, name: &str) -> bool {
    defines(module, name)
        && own_method_words(module).iter().any(|word| word == name)
        && module
            .items
            .iter()
            .any(|item| matches!(item, Item::Def(def) if def.name == name))
}

/// Names this module itself publishes. Re-exports are not included.
pub(crate) fn own_public_names(module: &Module) -> Vec<String> {
    match &module.exports {
        Exports::List(list) => {
            let mut names = Vec::new();
            let mut seen = HashSet::new();
            for item in &list.names {
                if defines(module, &item.name) {
                    push(&mut names, &mut seen, item.name.clone());
                }
            }
            names
        }
        Exports::Closed => Vec::new(),
    }
}

/// Constructor words and word names this module publishes as JavaScript values.
pub(crate) fn own_public_values(module: &Module) -> Vec<String> {
    match &module.exports {
        Exports::List(list) => {
            let mut names = Vec::new();
            for item in &module.items {
                if let Item::Data(data) = item {
                    for ctor in &data.ctors {
                        if list.has(&ctor.name) {
                            names.push(ctor.name.clone());
                        }
                    }
                }
            }
            for item in &module.items {
                if let Item::Def(def) = item {
                    if list.has(&def.name) {
                        names.push(def.name.clone());
                    }
                }
                if let Item::Place(place) = item {
                    if list.has(&place.name) {
                        names.push(place.name.clone());
                    }
                }
            }
            names
        }
        Exports::Closed => Vec::new(),
    }
}

/// Words named by `#` lines whose head an importer can name.
/// A head kept off `@` stays in this file, and so does its method word.
pub(crate) fn own_method_words(module: &Module) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for item in &module.items {
        if let Item::Instance(instance) = item {
            if instance_head_is_hidden(module, instance) {
                continue;
            }
            for binding in &instance.methods {
                push(&mut names, &mut seen, binding.word.clone());
            }
        }
    }
    names
}

/// The instance head names a type this file did not publish.
pub(crate) fn instance_head_is_hidden(module: &Module, instance: &Instance) -> bool {
    instance.args.iter().any(|ty| type_is_hidden(module, ty))
}

fn type_is_hidden(module: &Module, ty: &Type) -> bool {
    match ty {
        Type::Var(_) => false,
        Type::Ctor(name) => local_type_is_hidden(module, name),
        Type::App(head, args) => {
            type_is_hidden(module, head) || args.iter().any(|ty| type_is_hidden(module, ty))
        }
        Type::Fun(effect) => effect
            .input
            .items
            .iter()
            .chain(&effect.output.items)
            .any(|ty| type_is_hidden(module, ty)),
        Type::Record(row) => {
            row.fields.iter().any(|(_, ty)| type_is_hidden(module, ty))
                || row.tail_ty().is_some_and(|ty| type_is_hidden(module, ty))
        }
    }
}

fn local_type_is_hidden(module: &Module, name: &str) -> bool {
    let local = module.items.iter().any(|item| match item {
        Item::Data(data) => data.name == name,
        Item::Synonym(synonym) => synonym.name == name,
        _ => false,
    });
    let published = match &module.exports {
        Exports::List(list) => list.has(name),
        Exports::Closed => false,
    };
    local && !published
}

pub(crate) fn own_instances(module: &Module) -> Vec<Instance> {
    module
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Instance(instance) => Some(instance.clone()),
            _ => None,
        })
        .collect()
}

fn push(names: &mut Vec<String>, seen: &mut HashSet<String>, name: String) {
    if seen.insert(name.clone()) {
        names.push(name);
    }
}

type Load<'a> = dyn FnMut(&Path, &str) -> Option<(PathBuf, Module)> + 'a;

/// Names on this module's `@` list.
pub(crate) fn public_names(module: &Module) -> Vec<String> {
    match &module.exports {
        Exports::List(list) => {
            let mut names = Vec::new();
            let mut seen = HashSet::new();
            for item in &list.names {
                push(&mut names, &mut seen, item.name.clone());
            }
            names
        }
        Exports::Closed => Vec::new(),
    }
}

/// Instances written in this module and in the modules it imports.
/// A module is entered once. Its instances do not depend on the caller, and
/// dropping it from the path afterwards walked every import path again.
pub(crate) fn instances(module: &Module, from: &Path, load: &mut Load<'_>) -> Vec<Instance> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut entered = HashSet::new();
    collect_instances(module, from, load, &mut found, &mut seen, &mut entered);
    found
}

fn collect_instances(
    module: &Module,
    from: &Path,
    load: &mut Load<'_>,
    found: &mut Vec<Instance>,
    seen: &mut HashSet<String>,
    entered: &mut HashSet<String>,
) {
    if !entered.insert(from.to_string_lossy().to_string()) {
        return;
    }
    for instance in own_instances(module) {
        let label = instance.to_string();
        if seen.insert(label) {
            found.push(instance);
        }
    }
    for import in &module.imports {
        let Some((file, provider)) = load(from, &import.path) else {
            continue;
        };
        collect_instances(&provider, &file, load, found, seen, entered);
    }
}

/// The file that publishes `name`. An unlisted definition is not a result.
pub(crate) fn lookup(
    module: &Module,
    from: &Path,
    name: &str,
    load: &mut Load<'_>,
) -> Option<(PathBuf, Found)> {
    let mut stack = HashSet::new();
    lookup_in(module, from, name, load, &mut stack)
}

fn lookup_in(
    module: &Module,
    from: &Path,
    name: &str,
    load: &mut Load<'_>,
    stack: &mut HashSet<String>,
) -> Option<(PathBuf, Found)> {
    let key = from.to_string_lossy().to_string();
    if !stack.insert(key.clone()) {
        return None;
    }
    if let Some(found) = own_found(module, name) {
        stack.remove(&key);
        return Some((from.to_path_buf(), found));
    }
    if !may_reexport(module, name) {
        stack.remove(&key);
        return None;
    }
    for import in &module.imports {
        let Some((file, provider)) = load(from, &import.path) else {
            continue;
        };
        let Some(remote) = import.remote_of(name) else {
            continue;
        };
        if let Some(found) = lookup_in(&provider, &file, remote, load, stack) {
            stack.remove(&key);
            return Some(found);
        }
    }
    stack.remove(&key);
    None
}

fn may_reexport(module: &Module, name: &str) -> bool {
    match &module.exports {
        Exports::List(list) => list.has(name),
        Exports::Closed => false,
    }
}

fn own_found(module: &Module, name: &str) -> Option<Found> {
    let listed = |candidate: &str| match &module.exports {
        Exports::List(list) => list.has(candidate),
        Exports::Closed => false,
    };
    for item in &module.items {
        match item {
            Item::Data(data) if data.name == name && listed(&data.name) => {
                return Some(Found::Data(data.clone()));
            }
            Item::Synonym(synonym) if synonym.name == name && listed(&synonym.name) => {
                return Some(Found::Synonym(synonym.clone()));
            }
            Item::Def(def) if def.name == name && def_published(module, def) => {
                return Some(Found::Def(def.clone()));
            }
            Item::Place(place) if place.name == name && listed(&place.name) => {
                return Some(Found::Place(place.clone()));
            }
            Item::Class(class) if class.name == name && listed(&class.name) => {
                return Some(Found::Class(class.clone()));
            }
            _ => {}
        }
    }
    for item in &module.items {
        if let Item::Data(data) = item {
            if data.ctors.iter().any(|ctor| ctor.name == name) && listed(name) {
                return Some(Found::Data(data.clone()));
            }
        }
        if let Item::Class(class) = item {
            if class.methods.iter().any(|method| method.name == name) && listed(name) {
                return Some(Found::Class(class.clone()));
            }
        }
    }
    None
}

fn def_published(module: &Module, def: &Def) -> bool {
    match &module.exports {
        Exports::List(list) => list.has(&def.name),
        Exports::Closed => false,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::Path;

    use crate::parser::parse;
    use crate::paths::ParseMode;

    use super::instances;

    fn module(source: &str) -> crate::ast::Module {
        parse(source, ParseMode::Source).unwrap_or_else(|error| panic!("{}", error.message))
    }

    #[test]
    fn a_shared_import_is_entered_once() {
        let loads = Cell::new(0u32);
        let mut load = |from: &Path, path: &str| {
            loads.set(loads.get() + 1);
            let source = match (from.to_str().unwrap(), path) {
                ("Door.ply", "Left" | "Right") => "~ \"Mid\"\n",
                ("Left.ply" | "Right.ply", "Mid") => "~ \"A\"\n~ \"B\"\n",
                ("Mid.ply", "A" | "B") => "~ \"Shared\"\n",
                ("A.ply" | "B.ply", "Shared") => "# Show Num\n  show = showNum\n",
                _ => return None,
            };
            let file = match path {
                "Left" => "Left.ply",
                "Right" => "Right.ply",
                "Mid" => "Mid.ply",
                "A" => "A.ply",
                "B" => "B.ply",
                "Shared" => "Shared.ply",
                _ => return None,
            };
            Some((std::path::PathBuf::from(file), module(source)))
        };
        let found = instances(
            &module("~ \"Left\"\n~ \"Right\"\n"),
            Path::new("Door.ply"),
            &mut load,
        );
        assert_eq!(loads.get(), 8);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].class_names, ["Show"]);
    }
}
