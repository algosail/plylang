//! Public surface of a module.
//!
//! A module publishes its own public items and every `~ public` import.
//! A public import publishes the names it lists (or every public name, when
//! the list is `..`) and the instances of that module. A method word of those
//! instances is published with them. An import without `public` publishes nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::ast::{Class, Data, Def, Import, Instance, Item, Module, Synonym};

pub(crate) enum Found {
    Data(Data),
    Synonym(Synonym),
    Def(Def),
    Class(Class),
}

pub(crate) fn own_public_names(module: &Module) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for item in &module.items {
        if let Item::Data(data) = item {
            push(&mut names, &mut seen, data.name.clone());
            for ctor in &data.ctors {
                push(&mut names, &mut seen, ctor.name.clone());
            }
        }
        if let Item::Synonym(synonym) = item {
            push(&mut names, &mut seen, synonym.name.clone());
        }
    }
    for item in &module.items {
        if let Item::Def(def) = item {
            if !def.vis.is_private() {
                push(&mut names, &mut seen, def.name.clone());
            }
        }
    }
    for item in &module.items {
        if let Item::Class(class) = item {
            push(&mut names, &mut seen, class.name.clone());
            for method in &class.methods {
                push(&mut names, &mut seen, method.name.clone());
            }
        }
    }
    names
}

pub(crate) fn own_public_values(module: &Module) -> Vec<String> {
    let mut names = Vec::new();
    for item in &module.items {
        if let Item::Data(data) = item {
            for ctor in &data.ctors {
                names.push(ctor.name.clone());
            }
        }
    }
    for item in &module.items {
        if let Item::Def(def) = item {
            if !def.vis.is_private() {
                names.push(def.name.clone());
            }
        }
    }
    names
}

/// Words named by `#` lines written in this module.
pub(crate) fn own_method_words(module: &Module) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    for item in &module.items {
        if let Item::Instance(instance) = item {
            for binding in &instance.methods {
                push(&mut names, &mut seen, binding.word.clone());
            }
        }
    }
    names
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

/// Names importers receive from `(..)`, in source order.
pub(crate) fn public_names(
    module: &Module,
    from: &Path,
    load: &mut Load<'_>,
) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = HashSet::new();
    collect_names(module, from, load, &mut names, &mut seen, &mut stack);
    names
}

fn collect_names(
    module: &Module,
    from: &Path,
    load: &mut Load<'_>,
    names: &mut Vec<String>,
    seen: &mut HashSet<String>,
    stack: &mut HashSet<String>,
) {
    let key = from.to_string_lossy().to_string();
    if !stack.insert(key.clone()) {
        return;
    }
    for import in &module.imports {
        if !import.publish {
            continue;
        }
        let Some((file, provider)) = load(from, &import.path) else {
            continue;
        };
        if import.all {
            collect_names(&provider, &file, load, names, seen, stack);
        } else {
            for name in &import.names {
                push(names, seen, name.clone());
            }
        }
        for word in method_words(&provider, &file, load) {
            push(names, seen, word);
        }
    }
    for name in own_public_names(module) {
        push(names, seen, name);
    }
    stack.remove(&key);
}

/// Method words of the instances this module publishes, including its own `#` lines.
pub(crate) fn method_words(module: &Module, from: &Path, load: &mut Load<'_>) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = HashSet::new();
    collect_methods(module, from, load, &mut names, &mut seen, &mut stack);
    names
}

fn collect_methods(
    module: &Module,
    from: &Path,
    load: &mut Load<'_>,
    names: &mut Vec<String>,
    seen: &mut HashSet<String>,
    stack: &mut HashSet<String>,
) {
    let key = from.to_string_lossy().to_string();
    if !stack.insert(key.clone()) {
        return;
    }
    let public = own_public_names(module);
    for word in own_method_words(module) {
        if public.iter().any(|have| have == &word) {
            push(names, seen, word);
        }
    }
    for import in &module.imports {
        if !import.publish {
            continue;
        }
        let Some((file, provider)) = load(from, &import.path) else {
            continue;
        };
        collect_methods(&provider, &file, load, names, seen, stack);
    }
    stack.remove(&key);
}

pub(crate) fn instances(module: &Module, from: &Path, load: &mut Load<'_>) -> Vec<Instance> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = HashSet::new();
    collect_instances(module, from, load, &mut found, &mut seen, &mut stack);
    found
}

fn collect_instances(
    module: &Module,
    from: &Path,
    load: &mut Load<'_>,
    found: &mut Vec<Instance>,
    seen: &mut HashSet<String>,
    stack: &mut HashSet<String>,
) {
    let key = from.to_string_lossy().to_string();
    if !stack.insert(key.clone()) {
        return;
    }
    for instance in own_instances(module) {
        let label = instance.to_string();
        if seen.insert(label) {
            found.push(instance);
        }
    }
    for import in &module.imports {
        if !import.publish {
            continue;
        }
        let Some((file, provider)) = load(from, &import.path) else {
            continue;
        };
        collect_instances(&provider, &file, load, found, seen, stack);
    }
    stack.remove(&key);
}

/// The file that declares `name`, following `~ public` imports.
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
    for import in &module.imports {
        if !import.publish {
            continue;
        }
        let Some((file, provider)) = load(from, &import.path) else {
            continue;
        };
        if !import_offers(import, &provider, &file, name, load) {
            continue;
        }
        if let Some(found) = lookup_in(&provider, &file, name, load, stack) {
            stack.remove(&key);
            return Some(found);
        }
    }
    stack.remove(&key);
    None
}

fn import_offers(
    import: &Import,
    provider: &Module,
    file: &Path,
    name: &str,
    load: &mut Load<'_>,
) -> bool {
    import.all
        || import.names.iter().any(|have| have == name)
        || method_words(provider, file, load).iter().any(|have| have == name)
}

fn own_found(module: &Module, name: &str) -> Option<Found> {
    for item in &module.items {
        match item {
            Item::Data(data) if data.name == name => return Some(Found::Data(data.clone())),
            Item::Synonym(synonym) if synonym.name == name => {
                return Some(Found::Synonym(synonym.clone()))
            }
            Item::Def(def) if def.name == name && !def.vis.is_private() => {
                return Some(Found::Def(def.clone()))
            }
            Item::Class(class) if class.name == name => return Some(Found::Class(class.clone())),
            _ => {}
        }
    }
    for item in &module.items {
        if let Item::Data(data) = item {
            if data.ctors.iter().any(|ctor| ctor.name == name) {
                return Some(Found::Data(data.clone()));
            }
        }
        if let Item::Class(class) = item {
            if class.methods.iter().any(|method| method.name == name) {
                return Some(Found::Class(class.clone()));
            }
        }
    }
    None
}
