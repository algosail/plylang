//! Page script for `plylang compile`.
//! `compile` still writes the Node modules. This writes one classic script:
//! each reached binding is a function, dependencies first. A `main` of scheme
//! `( -- Unit )` is the root and the script calls it. A module with no `main`
//! and at least one place roots the script at those places and does not call
//! `main`. A binding stays when that root can call it. The rest of a file is left out.
//! The graph is the JavaScript this compiler wrote and the foreign files it
//! imports. A `node:` import, a package without `ply`, or a file outside that
//! package stops the compile.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::build::{find_installed, located, os_error};
use crate::codes::Error;
use crate::diagnostic::Diagnostic;
use crate::span::Span;
use crate::BuildError;

mod parse;

use parse::{is_ident, parse_module, split_items, strip_entry_guard};

pub(crate) fn write_page(
    modules_dir: &Path,
    entry: &str,
    script: &Path,
    call_main: bool,
) -> Result<(), BuildError> {
    let entry_path = modules_dir.join(entry).with_extension("js");
    if !entry_path.is_file() {
        return Err(fail(&entry_path, "", "missing script"));
    }
    let text = link(&entry_path, call_main)?;
    if let Some(parent) = script.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| os_error(parent, error))?;
        }
    }
    fs::write(script, text).map_err(|error| os_error(script, error))
}

fn link(entry: &Path, call_main: bool) -> Result<String, BuildError> {
    let mut graph = Graph {
        mods: Vec::new(),
        index: HashMap::new(),
    };
    let mut stack = Vec::new();
    let entry_id = graph.load(entry.to_path_buf(), &mut stack)?;
    if call_main && !publishes_main(&graph.mods[entry_id]) {
        graph.mods[entry_id]
            .exports
            .push(("main".to_string(), "main".to_string()));
    }
    prepare_items(&mut graph);
    let reach = reachable(&graph, entry_id, call_main)?;
    Ok(emit(&graph, &reach, entry_id, call_main))
}

struct Graph {
    mods: Vec<Mod>,
    index: HashMap<PathBuf, usize>,
}

struct Mod {
    path: PathBuf,
    source: String,
    imports: Vec<Resolved>,
    reexports: Vec<Resolved>,
    /// `(public, local)`.
    exports: Vec<(String, String)>,
    body: String,
    items: Vec<Item>,
}

enum Item {
    /// A top-level function, class, or single `const` / `let` / `var`.
    Binding { name: String, text: String, uses: Vec<String> },
    /// A statement that is not one binding. Kept when the module is reached.
    Loose { text: String, uses: Vec<String> },
}

struct Slot {
    module: usize,
    name: String,
}

struct Resolved {
    dep: usize,
    /// `(name in the dependency, name here)`.
    names: Vec<(String, String)>,
}

impl Graph {
    fn load(&mut self, path: PathBuf, stack: &mut Vec<PathBuf>) -> Result<usize, BuildError> {
        let path = fs::canonicalize(&path).map_err(|error| os_error(&path, error))?;
        if let Some(&id) = self.index.get(&path) {
            if stack.iter().any(|have| have == &path) {
                return Err(fail(&path, "", &cycle_spec(stack, &path)));
            }
            return Ok(id);
        }
        let source = fs::read_to_string(&path).map_err(|error| os_error(&path, error))?;
        let stripped = strip_entry_guard(&source).map_err(|_| fail(&path, &source, "node:fs"))?;
        let parsed = parse_module(&stripped).map_err(|form| fail(&path, &source, &form))?;
        let id = self.mods.len();
        self.index.insert(path.clone(), id);
        self.mods.push(Mod {
            path: path.clone(),
            source,
            imports: Vec::new(),
            reexports: Vec::new(),
            exports: Vec::new(),
            body: String::new(),
            items: Vec::new(),
        });
        stack.push(path.clone());
        let mut imports = Vec::new();
        for item in parsed.imports {
            let dep = self.depend(id, &item.spec, stack)?;
            imports.push(Resolved { dep, names: item.names });
        }
        let mut reexports = Vec::new();
        for item in parsed.reexports {
            let dep = self.depend(id, &item.spec, stack)?;
            reexports.push(Resolved { dep, names: item.names });
        }
        stack.pop();
        let module = &mut self.mods[id];
        module.imports = imports;
        module.reexports = reexports;
        module.exports = parsed.exports;
        module.body = parsed.body;
        Ok(id)
    }

    fn depend(&mut self, id: usize, spec: &str, stack: &mut Vec<PathBuf>) -> Result<usize, BuildError> {
        let from = self.mods[id].path.clone();
        let dep_path = match resolve(&from, spec) {
            Ok(path) => path,
            Err(spec) => {
                let source = self.mods[id].source.clone();
                let path = self.mods[id].path.clone();
                return Err(fail(&path, &source, &spec));
            }
        };
        self.load(dep_path, stack)
    }
}

fn publishes_main(module: &Mod) -> bool {
    module.exports.iter().any(|(public, _)| public == "main")
        || module
            .reexports
            .iter()
            .any(|item| item.names.iter().any(|(_, public)| public == "main"))
}

fn prepare_items(graph: &mut Graph) {
    for module in &mut graph.mods {
        let mut known = HashSet::new();
        let split = split_items(&module.body);
        for item in &split {
            if let Item::Binding { name, .. } = item {
                known.insert(name.clone());
            }
        }
        for import in &module.imports {
            for (_, local) in &import.names {
                known.insert(local.clone());
            }
        }
        module.items = split
            .into_iter()
            .map(|item| match item {
                Item::Binding { name, text, uses } => Item::Binding {
                    uses: keep_uses(uses, &known, Some(&name)),
                    name,
                    text,
                },
                Item::Loose { text, uses } => Item::Loose {
                    uses: keep_uses(uses, &known, None),
                    text,
                },
            })
            .collect();
    }
}

fn keep_uses(uses: Vec<String>, known: &HashSet<String>, self_name: Option<&str>) -> Vec<String> {
    let mut uses: Vec<String> = uses
        .into_iter()
        .filter(|name| known.contains(name) && self_name != Some(name.as_str()))
        .collect();
    uses.sort();
    uses.dedup();
    uses
}

struct Reach {
    bindings: Vec<HashSet<String>>,
    imports: Vec<HashSet<String>>,
}

fn reachable(graph: &Graph, entry: usize, call_main: bool) -> Result<Reach, BuildError> {
    let mut reach = Reach {
        bindings: vec![HashSet::new(); graph.mods.len()],
        imports: vec![HashSet::new(); graph.mods.len()],
    };
    let mut loose = vec![false; graph.mods.len()];
    let mut work = Vec::new();
    if call_main {
        let slot = find_slot(graph, entry, "main").ok_or_else(|| {
            fail(
                &graph.mods[entry].path,
                &graph.mods[entry].source,
                "main",
            )
        })?;
        work.push(slot);
    } else {
        follow_loose(graph, entry, &mut reach, &mut loose, &mut work)?;
    }
    while let Some(slot) = work.pop() {
        if !reach.bindings[slot.module].insert(slot.name.clone()) {
            continue;
        }
        let uses = binding_uses(graph, slot.module, &slot.name);
        follow_uses(graph, slot.module, &uses, &mut reach, &mut work)?;
        follow_loose(graph, slot.module, &mut reach, &mut loose, &mut work)?;
    }
    Ok(reach)
}

/// Loose statements of a reached module run once. A place-rooted page starts here.
fn follow_loose(
    graph: &Graph,
    module: usize,
    reach: &mut Reach,
    loose: &mut [bool],
    work: &mut Vec<Slot>,
) -> Result<(), BuildError> {
    if loose[module] {
        return Ok(());
    }
    loose[module] = true;
    let mut extra = Vec::new();
    for item in &graph.mods[module].items {
        if let Item::Loose { uses, .. } = item {
            extra.extend(uses.iter().cloned());
        }
    }
    follow_uses(graph, module, &extra, reach, work)
}

fn binding_uses(graph: &Graph, module: usize, name: &str) -> Vec<String> {
    graph.mods[module]
        .items
        .iter()
        .find_map(|item| match item {
            Item::Binding { name: have, uses, .. } if have == name => Some(uses.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn follow_uses(
    graph: &Graph,
    module: usize,
    uses: &[String],
    reach: &mut Reach,
    work: &mut Vec<Slot>,
) -> Result<(), BuildError> {
    for name in uses {
        if has_binding(&graph.mods[module], name) {
            work.push(Slot {
                module,
                name: name.clone(),
            });
            continue;
        }
        let Some((dep, remote)) = import_local(&graph.mods[module], name) else {
            continue;
        };
        reach.imports[module].insert(name.clone());
        let slot = find_slot(graph, dep, &remote).ok_or_else(|| {
            fail(
                &graph.mods[module].path,
                &graph.mods[module].source,
                &remote,
            )
        })?;
        work.push(slot);
    }
    Ok(())
}

fn has_binding(module: &Mod, name: &str) -> bool {
    module.items.iter().any(|item| {
        matches!(item, Item::Binding { name: have, .. } if have == name)
    })
}

fn import_local(module: &Mod, local: &str) -> Option<(usize, String)> {
    for import in &module.imports {
        for (remote, name) in &import.names {
            if name == local {
                return Some((import.dep, remote.clone()));
            }
        }
    }
    None
}

/// The binding a public name refers to, following `export { name }` and
/// `export { name } from`.
fn find_slot(graph: &Graph, module: usize, public: &str) -> Option<Slot> {
    fn walk(graph: &Graph, module: usize, public: &str, stack: &mut Vec<(usize, String)>) -> Option<Slot> {
        if stack.iter().any(|(have, name)| *have == module && name == public) {
            return None;
        }
        stack.push((module, public.to_string()));
        let module_ref = &graph.mods[module];
        let found = if let Some((_, local)) = module_ref.exports.iter().find(|(name, _)| name == public) {
            if has_binding(module_ref, local) {
                Some(Slot {
                    module,
                    name: local.clone(),
                })
            } else if let Some((dep, remote)) = import_local(module_ref, local) {
                walk(graph, dep, &remote, stack)
            } else {
                None
            }
        } else if let Some((dep, remote)) = reexport_public(module_ref, public) {
            walk(graph, dep, &remote, stack)
        } else if has_binding(module_ref, public) {
            Some(Slot {
                module,
                name: public.to_string(),
            })
        } else {
            None
        };
        stack.pop();
        found
    }
    walk(graph, module, public, &mut Vec::new())
}

fn reexport_public(module: &Mod, public: &str) -> Option<(usize, String)> {
    for item in &module.reexports {
        for (remote, name) in &item.names {
            if name == public {
                return Some((item.dep, remote.clone()));
            }
        }
    }
    None
}

fn live_order(graph: &Graph, reach: &Reach, entry: usize, force_entry: bool) -> Vec<usize> {
    let mut seen = vec![false; graph.mods.len()];
    let mut order = Vec::new();
    fn walk(
        graph: &Graph,
        reach: &Reach,
        id: usize,
        force: bool,
        seen: &mut [bool],
        order: &mut Vec<usize>,
    ) {
        if seen[id] || (reach.bindings[id].is_empty() && !force) {
            return;
        }
        seen[id] = true;
        let specs: Vec<(usize, String, String)> = graph.mods[id]
            .imports
            .iter()
            .flat_map(|import| {
                import
                    .names
                    .iter()
                    .map(|(remote, local)| (import.dep, remote.clone(), local.clone()))
            })
            .collect();
        for (dep, remote, local) in specs {
            if !reach.imports[id].contains(&local) {
                continue;
            }
            if let Some(slot) = find_slot(graph, dep, &remote) {
                walk(graph, reach, slot.module, false, seen, order);
            }
        }
        order.push(id);
    }
    walk(graph, reach, entry, force_entry, &mut seen, &mut order);
    order
}

fn emit(graph: &Graph, reach: &Reach, entry: usize, call_main: bool) -> String {
    let order = live_order(graph, reach, entry, !call_main);
    let prefix = fresh_prefix(graph);
    let mut vars = HashMap::new();
    for (index, id) in order.iter().enumerate() {
        vars.insert(*id, format!("{prefix}{index}"));
    }
    let mut out = String::new();
    for id in &order {
        let specs: Vec<(usize, String, String)> = graph.mods[*id]
            .imports
            .iter()
            .flat_map(|import| {
                import
                    .names
                    .iter()
                    .map(|(remote, local)| (import.dep, remote.clone(), local.clone()))
            })
            .collect();
        let mut groups: Vec<(usize, Vec<(String, String)>)> = Vec::new();
        for (dep, remote, local) in specs {
            if !reach.imports[*id].contains(&local) {
                continue;
            }
            let Some(slot) = find_slot(graph, dep, &remote) else {
                continue;
            };
            let key = export_key(graph, &slot);
            if let Some(group) = groups.iter_mut().find(|(have, _)| *have == slot.module) {
                group.1.push((key, local));
            } else {
                groups.push((slot.module, vec![(key, local)]));
            }
        }
        let module = &graph.mods[*id];
        let var = &vars[id];
        out.push_str(&format!("const {var} = (() => {{\n"));
        for (provider, binds) in &groups {
            let Some(dep) = vars.get(provider) else {
                continue;
            };
            let list = binds
                .iter()
                .map(|(remote, local)| bind(remote, local))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("  const {{ {list} }} = {dep};\n"));
        }
        for item in &module.items {
            match item {
                Item::Binding { name, text, .. } if reach.bindings[*id].contains(name) => {
                    out.push_str(text);
                    if !text.ends_with('\n') {
                        out.push('\n');
                    }
                }
                Item::Loose { text, .. } => {
                    out.push_str(text);
                    if !text.ends_with('\n') {
                        out.push('\n');
                    }
                }
                Item::Binding { .. } => {}
            }
        }
        out.push_str("  return {\n");
        for (public, local) in &module.exports {
            if has_binding(module, local) && reach.bindings[*id].contains(local) {
                out.push_str(&format!("    {}: {local},\n", prop(public)));
            }
        }
        out.push_str("  };\n})();\n");
    }
    if call_main {
        let entry_var = &vars[&entry];
        out.push_str(&format!("{entry_var}.main();\n"));
    }
    hide_script_close(&out)
}

fn export_key(graph: &Graph, slot: &Slot) -> String {
    let module = &graph.mods[slot.module];
    module
        .exports
        .iter()
        .find(|(public, local)| local == &slot.name && has_binding(module, local) && is_ident(public))
        .map(|(public, _)| public.clone())
        .unwrap_or_else(|| slot.name.clone())
}

fn fresh_prefix(graph: &Graph) -> String {
    let mut prefix = "ply$".to_string();
    for _ in 0..8 {
        let used = graph.mods.iter().any(|module| module.source.contains(&prefix));
        if !used {
            break;
        }
        prefix.push('_');
    }
    prefix
}

fn bind(remote: &str, local: &str) -> String {
    if remote == local && is_ident(remote) {
        remote.to_string()
    } else {
        format!("{}: {local}", prop(remote))
    }
}

fn prop(name: &str) -> String {
    if is_ident(name) {
        name.to_string()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| "\"\"".to_string())
    }
}

fn hide_script_close(js: &str) -> String {
    let bytes = js.as_bytes();
    let mut out = String::with_capacity(js.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + 8 <= bytes.len()
            && bytes[index] == b'<'
            && bytes[index + 1] == b'/'
            && bytes[index + 2..index + 8].eq_ignore_ascii_case(b"script")
        {
            out.push_str("<\\/");
            index += 2;
            continue;
        }
        let ch = js[index..].chars().next().unwrap();
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

fn cycle_spec(stack: &[PathBuf], again: &Path) -> String {
    let mut names = Vec::new();
    let mut seen = false;
    for path in stack {
        if path == again {
            seen = true;
        }
        if seen {
            names.push(short_name(path));
        }
    }
    names.push(short_name(again));
    format!("cycle {}", names.join(", "))
}

fn short_name(path: &Path) -> String {
    let file = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    match path.parent().and_then(|parent| parent.file_name()) {
        Some(parent) => format!("{}/{}", parent.to_string_lossy(), file),
        None => file,
    }
}

fn fail(path: &Path, source: &str, spec: &str) -> BuildError {
    located(
        path,
        source,
        Diagnostic::new(Span::new(0, 0), Error::BrowserLoad(spec.to_string())),
    )
}

fn resolve(from: &Path, spec: &str) -> Result<PathBuf, String> {
    if spec.starts_with('.') {
        return resolve_relative(from, spec);
    }
    resolve_package(from, spec)
}

fn resolve_relative(from: &Path, spec: &str) -> Result<PathBuf, String> {
    let root = ply_root(from).ok_or_else(|| spec.to_string())?;
    let root = fs::canonicalize(&root).map_err(|_| spec.to_string())?;
    let mut path = from.parent().unwrap_or(from).to_path_buf();
    for component in Path::new(spec).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !path.pop() {
                    return Err(spec.to_string());
                }
            }
            Component::Normal(part) => path.push(part),
            _ => return Err(spec.to_string()),
        }
    }
    if !path.is_file() {
        return Err(spec.to_string());
    }
    let path = fs::canonicalize(&path).map_err(|_| spec.to_string())?;
    if !inside_package(&root, &path) {
        return Err(spec.to_string());
    }
    Ok(path)
}

fn resolve_package(from: &Path, spec: &str) -> Result<PathBuf, String> {
    if spec.starts_with("node:") || spec.is_empty() {
        return Err(spec.to_string());
    }
    let (name, subpath) = split_package(spec).ok_or_else(|| spec.to_string())?;
    let root = if let Some(found) = find_installed(from, &name) {
        found
    } else if let Some(found) = self_package(from, &name) {
        found
    } else {
        return Err(spec.to_string());
    };
    let root = fs::canonicalize(&root).map_err(|_| spec.to_string())?;
    if !is_ply(&root) {
        return Err(spec.to_string());
    }
    let target = import_path(&root, &subpath).ok_or_else(|| spec.to_string())?;
    let relative = target.strip_prefix("./").unwrap_or(&target);
    let file = root.join(relative);
    if !file.is_file() {
        return Err(spec.to_string());
    }
    let file = fs::canonicalize(&file).map_err(|_| spec.to_string())?;
    if !inside_package(&root, &file) {
        return Err(spec.to_string());
    }
    Ok(file)
}

/// The nearest package of this name. Node resolves a package's own name
/// without a copy under `node_modules`.
fn self_package(start: &Path, name: &str) -> Option<PathBuf> {
    let mut dir = start.parent()?.to_path_buf();
    loop {
        let manifest = dir.join("package.json");
        if manifest.is_file() {
            let value = read_json(&manifest).ok()?;
            let found = value.get("name")?.as_str()?;
            if found == name && value.get("ply").is_some() {
                return Some(dir);
            }
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn ply_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.parent()?.to_path_buf();
    loop {
        let manifest = dir.join("package.json");
        if manifest.is_file() {
            let value = read_json(&manifest).ok()?;
            if value.get("ply").is_some() {
                return Some(dir);
            }
            return None;
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn is_ply(root: &Path) -> bool {
    read_json(&root.join("package.json"))
        .ok()
        .is_some_and(|value| value.get("ply").is_some())
}

fn inside_package(root: &Path, file: &Path) -> bool {
    let Ok(relative) = file.strip_prefix(root) else {
        return false;
    };
    !relative.components().any(|component| component.as_os_str() == "node_modules")
}

fn import_path(root: &Path, subpath: &str) -> Option<String> {
    let value = read_json(&root.join("package.json")).ok()?;
    let exports = value.get("exports")?.as_object()?;
    let key = if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{subpath}")
    };
    condition_string(exports.get(&key)?, "import")
}

fn condition_string(value: &Value, key: &str) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(text) = value.get(key).and_then(Value::as_str) {
        return Some(text.to_string());
    }
    if let Some(text) = value
        .get(key)
        .and_then(|child| child.get("default"))
        .and_then(Value::as_str)
    {
        return Some(text.to_string());
    }
    let object = value.as_object()?;
    for child in object.values() {
        if let Some(text) = child.get(key).and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    None
}

fn split_package(spec: &str) -> Option<(String, String)> {
    if spec.starts_with('@') {
        let mut parts = spec[1..].splitn(3, '/');
        let scope = parts.next()?;
        let name = parts.next()?;
        if scope.is_empty() || name.is_empty() {
            return None;
        }
        let subpath = parts.next().unwrap_or("").to_string();
        return Some((format!("@{scope}/{name}"), subpath));
    }
    let mut parts = spec.splitn(2, '/');
    let name = parts.next()?.to_string();
    if name.is_empty() || name.chars().next()?.is_ascii_uppercase() {
        return None;
    }
    Some((name, parts.next().unwrap_or("").to_string()))
}

fn read_json(path: &Path) -> Result<Value, ()> {
    let text = fs::read_to_string(path).map_err(|_| ())?;
    serde_json::from_str(&text).map_err(|_| ())
}
