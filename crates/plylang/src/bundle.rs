//! Page script for `plylang compile`.
//! `compile` still writes the Node modules. This writes `script.js` beside
//! them: each reached binding is a function, dependencies first, then `main()`.
//! A binding stays when `main` can call it. The rest of a file is left out.
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

pub(crate) fn write_page(out: &Path, entry: &str) -> Result<(), BuildError> {
    let entry_path = out.join(entry).with_extension("js");
    if !entry_path.is_file() {
        return Err(fail(&entry_path, "", "missing script"));
    }
    let text = link(&entry_path)?;
    let script = out.join("script.js");
    fs::write(&script, text).map_err(|error| os_error(&script, error))
}

fn link(entry: &Path) -> Result<String, BuildError> {
    let mut graph = Graph {
        mods: Vec::new(),
        index: HashMap::new(),
    };
    let mut stack = Vec::new();
    let entry_id = graph.load(entry.to_path_buf(), &mut stack)?;
    if !publishes_main(&graph.mods[entry_id]) {
        graph.mods[entry_id]
            .exports
            .push(("main".to_string(), "main".to_string()));
    }
    prepare_items(&mut graph);
    let reach = reachable(&graph, entry_id)?;
    Ok(emit(&graph, &reach, entry_id))
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

fn reachable(graph: &Graph, entry: usize) -> Result<Reach, BuildError> {
    let mut reach = Reach {
        bindings: vec![HashSet::new(); graph.mods.len()],
        imports: vec![HashSet::new(); graph.mods.len()],
    };
    let mut loose = vec![false; graph.mods.len()];
    let slot = find_slot(graph, entry, "main").ok_or_else(|| {
        fail(
            &graph.mods[entry].path,
            &graph.mods[entry].source,
            "main",
        )
    })?;
    let mut work = vec![slot];
    while let Some(slot) = work.pop() {
        if !reach.bindings[slot.module].insert(slot.name.clone()) {
            continue;
        }
        let uses = binding_uses(graph, slot.module, &slot.name);
        follow_uses(graph, slot.module, &uses, &mut reach, &mut work)?;
        if !loose[slot.module] {
            loose[slot.module] = true;
            let mut extra = Vec::new();
            for item in &graph.mods[slot.module].items {
                if let Item::Loose { uses, .. } = item {
                    extra.extend(uses.iter().cloned());
                }
            }
            follow_uses(graph, slot.module, &extra, &mut reach, &mut work)?;
        }
    }
    Ok(reach)
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

fn live_order(graph: &Graph, reach: &Reach, entry: usize) -> Vec<usize> {
    let mut seen = vec![false; graph.mods.len()];
    let mut order = Vec::new();
    fn walk(graph: &Graph, reach: &Reach, id: usize, seen: &mut [bool], order: &mut Vec<usize>) {
        if seen[id] || reach.bindings[id].is_empty() {
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
                walk(graph, reach, slot.module, seen, order);
            }
        }
        order.push(id);
    }
    walk(graph, reach, entry, &mut seen, &mut order);
    order
}

fn emit(graph: &Graph, reach: &Reach, entry: usize) -> String {
    let order = live_order(graph, reach, entry);
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
    let entry_var = &vars[&entry];
    out.push_str(&format!("{entry_var}.main();\n"));
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

/// Drop the Node entry guard. A real `node:` import is left in place, and a
/// guard import without its suffix is an error: the page would still see `node:`.
fn strip_entry_guard(js: &str) -> Result<String, ()> {
    let (stripped, had_guard_import) = remove_guard_imports(js);
    if let Some(body) = cut_guard_suffix(&stripped) {
        return Ok(body);
    }
    if had_guard_import {
        Err(())
    } else {
        Ok(stripped)
    }
}

fn remove_guard_imports(js: &str) -> (String, bool) {
    let mut out = String::new();
    let mut had = false;
    for line in js.split_inclusive('\n') {
        if is_guard_import(line) {
            had = true;
            continue;
        }
        out.push_str(line);
    }
    (out, had)
}

fn is_guard_import(line: &str) -> bool {
    let line = line.strip_suffix('\n').unwrap_or(line);
    if let Some(rest) = line.strip_prefix("import { realpathSync as ") {
        let Some((ident, rest)) = take_ident(rest) else {
            return false;
        };
        return !ident.is_empty() && rest == " } from \"node:fs\";";
    }
    if let Some(rest) = line.strip_prefix("import { pathToFileURL as ") {
        let Some((ident, rest)) = take_ident(rest) else {
            return false;
        };
        return !ident.is_empty() && rest == " } from \"node:url\";";
    }
    false
}

fn cut_guard_suffix(js: &str) -> Option<String> {
    let mut from = 0;
    while from < js.len() {
        let rel = js[from..].find("let ")?;
        let at = from + rel;
        let line_start = at == 0 || js.as_bytes()[at - 1] == b'\n';
        if line_start && guard_matches(&js[at..]) {
            return Some(js[..at].to_string());
        }
        from = at + 4;
    }
    None
}

fn guard_matches(text: &str) -> bool {
    let Some(text) = text.strip_prefix("let ") else {
        return false;
    };
    let Some((flag, text)) = take_ident(text) else {
        return false;
    };
    let Some(text) = text.strip_prefix(" = import.meta.main === true;\n") else {
        return false;
    };
    let Some(text) = text.strip_prefix("if (import.meta.main === undefined && process.argv[1]) {\n  try {\n    ") else {
        return false;
    };
    let Some(text) = text.strip_prefix(&flag) else {
        return false;
    };
    let Some(text) = text.strip_prefix(" = import.meta.url === ") else {
        return false;
    };
    let Some((_, text)) = take_ident(text) else {
        return false;
    };
    let Some(text) = text.strip_prefix("(") else {
        return false;
    };
    let Some((_, text)) = take_ident(text) else {
        return false;
    };
    let Some(text) = text.strip_prefix("(process.argv[1])).href;\n  } catch {\n    ") else {
        return false;
    };
    let Some(text) = text.strip_prefix(&flag) else {
        return false;
    };
    let Some(text) = text.strip_prefix(" = false;\n  }\n}\nif (") else {
        return false;
    };
    let Some(text) = text.strip_prefix(&flag) else {
        return false;
    };
    let Some(text) = text.strip_prefix(") ") else {
        return false;
    };
    let Some((_, text)) = take_ident(text) else {
        return false;
    };
    let Some(text) = text.strip_prefix("();") else {
        return false;
    };
    text.trim().is_empty()
}

fn take_ident(text: &str) -> Option<(&str, &str)> {
    let mut chars = text.chars();
    let first = chars.next()?;
    if !is_ident_start(first) {
        return None;
    }
    let mut len = first.len_utf8();
    for ch in chars {
        if !is_ident_cont(ch) {
            break;
        }
        len += ch.len_utf8();
    }
    Some((&text[..len], &text[len..]))
}

struct Parsed {
    imports: Vec<SpecNames>,
    reexports: Vec<SpecNames>,
    /// `(public, local)`.
    exports: Vec<(String, String)>,
    body: String,
}

struct SpecNames {
    spec: String,
    /// Import: `(remote, local)`. Reexport: `(remote, public)`.
    names: Vec<(String, String)>,
}

enum ExportKind {
    List(Vec<(String, String)>),
    Reexport(SpecNames),
    Keep(String),
}

fn parse_module(src: &str) -> Result<Parsed, String> {
    let mut scan = Scan { src, i: 0 };
    let mut body = String::new();
    let mut imports = Vec::new();
    let mut reexports = Vec::new();
    let mut exports = Vec::new();
    let mut can_start = true;
    let mut depth = 0i32;
    let mut value = false;
    while scan.i < src.len() {
        if scan.at_space_or_comment() {
            let start = scan.i;
            scan.skip_space();
            body.push_str(&src[start..scan.i]);
            continue;
        }
        if depth == 0 && can_start && scan.starts_word("import") {
            imports.push(parse_import(&mut scan)?);
            can_start = true;
            value = false;
            continue;
        }
        if depth == 0 && can_start && scan.starts_word("export") {
            match parse_export(&mut scan)? {
                ExportKind::List(names) => {
                    for (local, public) in names {
                        exports.push((public, local));
                    }
                    can_start = true;
                }
                ExportKind::Reexport(item) => {
                    reexports.push(item);
                    can_start = true;
                }
                ExportKind::Keep(name) => {
                    exports.push((name.clone(), name));
                    can_start = false;
                }
            }
            value = false;
            continue;
        }
        let start = scan.i;
        if scan.peek().is_some_and(is_ident_start) {
            let _ = scan.ident();
            body.push_str(&src[start..scan.i]);
            can_start = false;
            value = true;
            continue;
        }
        match scan.peek() {
            Some('"' | '\'') => {
                let end = end_of_string(src, scan.i).ok_or_else(|| "string".to_string())?;
                body.push_str(&src[scan.i..end]);
                scan.i = end;
                can_start = false;
                value = true;
            }
            Some('`') => {
                let end = end_of_template(src, scan.i).ok_or_else(|| "template".to_string())?;
                body.push_str(&src[scan.i..end]);
                scan.i = end;
                can_start = false;
                value = true;
            }
            Some(ch) if ch.is_ascii_digit() => {
                let end = end_of_number(src, scan.i);
                body.push_str(&src[scan.i..end]);
                scan.i = end;
                can_start = false;
                value = true;
            }
            Some('/') => {
                if value {
                    body.push('/');
                    scan.bump();
                    value = false;
                    can_start = false;
                } else if let Some(end) = end_of_regex(src, scan.i) {
                    body.push_str(&src[scan.i..end]);
                    scan.i = end;
                    value = true;
                    can_start = false;
                } else {
                    body.push('/');
                    scan.bump();
                    value = false;
                    can_start = false;
                }
            }
            Some(ch @ ('(' | '[' | '{')) => {
                depth += 1;
                body.push(ch);
                scan.bump();
                can_start = false;
                value = false;
            }
            Some(ch @ (')' | ']' | '}')) => {
                depth = (depth - 1).max(0);
                body.push(ch);
                scan.bump();
                can_start = ch == '}' && depth == 0;
                value = true;
            }
            Some(';') => {
                body.push(';');
                scan.bump();
                can_start = depth == 0;
                value = false;
            }
            Some(ch) => {
                body.push(ch);
                scan.bump();
                can_start = false;
                value = false;
            }
            None => break,
        }
    }
    Ok(Parsed { imports, reexports, exports, body })
}

fn parse_import(scan: &mut Scan<'_>) -> Result<SpecNames, String> {
    scan.eat_word("import");
    scan.skip_space();
    if scan.rest().starts_with('*') {
        return Err("import *".to_string());
    }
    if matches!(scan.peek(), Some('"' | '\'')) {
        return Err(scan.string()?);
    }
    if scan.peek() != Some('{') {
        return Err("import name".to_string());
    }
    let names = parse_names(scan)?;
    scan.skip_space();
    if !scan.eat_word("from") {
        return Err("import".to_string());
    }
    scan.skip_space();
    let spec = scan.string()?;
    scan.skip_space();
    if !scan.eat(';') {
        return Err(spec);
    }
    Ok(SpecNames { spec, names })
}

fn parse_export(scan: &mut Scan<'_>) -> Result<ExportKind, String> {
    scan.eat_word("export");
    scan.skip_space();
    if scan.starts_word("default") {
        return Err("export default".to_string());
    }
    if scan.peek() == Some('{') {
        let names = parse_names(scan)?;
        scan.skip_space();
        if scan.eat_word("from") {
            scan.skip_space();
            let spec = scan.string()?;
            scan.skip_space();
            if !scan.eat(';') {
                return Err(spec);
            }
            return Ok(ExportKind::Reexport(SpecNames { spec, names }));
        }
        scan.skip_space();
        if !scan.eat(';') {
            return Err("export".to_string());
        }
        return Ok(ExportKind::List(names));
    }
    if scan.starts_word("let") {
        return Err("export let".to_string());
    }
    if scan.starts_word("var") {
        return Err("export var".to_string());
    }
    let keyword_at = scan.i;
    if scan.eat_word("async") {
        scan.skip_space();
        if !scan.eat_word("function") {
            return Err("export".to_string());
        }
        scan.skip_space();
        let name = scan.ident().ok_or_else(|| "export".to_string())?;
        scan.i = keyword_at;
        return Ok(ExportKind::Keep(name));
    }
    if scan.eat_word("function") || scan.eat_word("class") || scan.eat_word("const") {
        scan.skip_space();
        if matches!(scan.peek(), Some('{' | '[')) {
            return Err("export const".to_string());
        }
        let name = scan.ident().ok_or_else(|| "export".to_string())?;
        scan.i = keyword_at;
        return Ok(ExportKind::Keep(name));
    }
    Err("export".to_string())
}

/// `(left, right)` with `as`, or the same name twice.
fn parse_names(scan: &mut Scan<'_>) -> Result<Vec<(String, String)>, String> {
    if !scan.eat('{') {
        return Err("export".to_string());
    }
    let mut names = Vec::new();
    loop {
        scan.skip_space();
        if scan.eat('}') {
            break;
        }
        let first = scan.ident().ok_or_else(|| "export".to_string())?;
        scan.skip_space();
        let second = if scan.eat_word("as") {
            scan.skip_space();
            scan.ident().ok_or_else(|| "export".to_string())?
        } else {
            first.clone()
        };
        names.push((first, second));
        scan.skip_space();
        if scan.eat(',') {
            continue;
        }
        if scan.eat('}') {
            break;
        }
        return Err("export".to_string());
    }
    Ok(names)
}

struct Scan<'a> {
    src: &'a str,
    i: usize,
}

impl Scan<'_> {
    fn rest(&self) -> &str {
        &self.src[self.i..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn bump(&mut self) {
        if let Some(ch) = self.peek() {
            self.i += ch.len_utf8();
        }
    }

    fn at_space_or_comment(&self) -> bool {
        match self.peek() {
            Some(ch) if ch.is_whitespace() => true,
            _ => {
                let rest = self.rest();
                rest.starts_with("//") || rest.starts_with("/*")
            }
        }
    }

    fn skip_space(&mut self) {
        loop {
            while self.peek().is_some_and(|ch| ch.is_whitespace()) {
                self.bump();
            }
            let rest = self.rest();
            if rest.starts_with("//") {
                self.i += 2;
                while self.peek().is_some_and(|ch| ch != '\n') {
                    self.bump();
                }
                continue;
            }
            if rest.starts_with("/*") {
                self.i += 2;
                while !self.rest().is_empty() && !self.rest().starts_with("*/") {
                    self.bump();
                }
                if self.rest().starts_with("*/") {
                    self.i += 2;
                }
                continue;
            }
            break;
        }
    }

    fn eat(&mut self, ch: char) -> bool {
        if self.peek() == Some(ch) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn starts_word(&self, word: &str) -> bool {
        let rest = self.rest();
        if !rest.starts_with(word) {
            return false;
        }
        match rest[word.len()..].chars().next() {
            Some(ch) if is_ident_cont(ch) => false,
            _ => self.i == 0 || !prev_ident(&self.src[..self.i]),
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if self.starts_word(word) {
            self.i += word.len();
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Option<String> {
        let ch = self.peek()?;
        if !is_ident_start(ch) {
            return None;
        }
        let start = self.i;
        self.bump();
        while self.peek().is_some_and(is_ident_cont) {
            self.bump();
        }
        Some(self.src[start..self.i].to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        let quote = self.peek().ok_or_else(|| "import".to_string())?;
        if quote != '"' && quote != '\'' {
            return Err("import".to_string());
        }
        self.bump();
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if ch == '\\' {
                self.bump();
                let escaped = self.peek().ok_or_else(|| "import".to_string())?;
                self.bump();
                out.push(escaped);
                continue;
            }
            if ch == quote {
                self.bump();
                return Ok(out);
            }
            out.push(ch);
            self.bump();
        }
        Err("import".to_string())
    }
}

fn prev_ident(text: &str) -> bool {
    text.chars().next_back().is_some_and(is_ident_cont)
}

fn is_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if is_ident_start(ch) => chars.all(is_ident_cont),
        _ => false,
    }
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || ch == '$'
}

fn is_ident_cont(ch: char) -> bool {
    is_ident_start(ch) || ch.is_ascii_digit()
}

fn end_of_string(src: &str, start: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let quote = *bytes.get(start)?;
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        if bytes[index] == quote {
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

fn end_of_template(src: &str, start: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    if bytes.get(start) != Some(&b'`') {
        return None;
    }
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'`' => return Some(index + 1),
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                index += 2;
                let mut depth = 1i32;
                while index < bytes.len() && depth > 0 {
                    match bytes[index] {
                        b'\'' | b'"' => index = end_of_string(src, index)?,
                        b'`' => index = end_of_template(src, index)?,
                        b'/' if bytes.get(index + 1) == Some(&b'/') => {
                            index += 2;
                            while index < bytes.len() && bytes[index] != b'\n' {
                                index += 1;
                            }
                        }
                        b'/' if bytes.get(index + 1) == Some(&b'*') => {
                            index += 2;
                            while index + 1 < bytes.len()
                                && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                            {
                                index += 1;
                            }
                            index = (index + 2).min(bytes.len());
                        }
                        b'{' => {
                            depth += 1;
                            index += 1;
                        }
                        b'}' => {
                            depth -= 1;
                            index += 1;
                        }
                        _ => index += 1,
                    }
                }
            }
            _ => index += 1,
        }
    }
    None
}

fn end_of_number(src: &str, start: usize) -> usize {
    let bytes = src.as_bytes();
    let mut index = start;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if bytes.get(index) == Some(&b'.') && bytes.get(index + 1).is_some_and(|byte| byte.is_ascii_digit()) {
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        let mut next = index + 1;
        if matches!(bytes.get(next), Some(b'+' | b'-')) {
            next += 1;
        }
        if bytes.get(next).is_some_and(|byte| byte.is_ascii_digit()) {
            index = next + 1;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
        }
    }
    index
}

fn end_of_regex(src: &str, start: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    if bytes.get(start) != Some(&b'/') {
        return None;
    }
    let mut index = start + 1;
    let mut class = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'\n' => return None,
            b'[' if !class => {
                class = true;
                index += 1;
            }
            b']' if class => {
                class = false;
                index += 1;
            }
            b'/' if !class => {
                index += 1;
                while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                    index += 1;
                }
                return Some(index);
            }
            _ => index += 1,
        }
    }
    None
}

fn split_items(src: &str) -> Vec<Item> {
    let mut index = 0;
    let mut items = Vec::new();
    while index < src.len() {
        index = skip_gap(src, index);
        if index >= src.len() {
            break;
        }
        let start = index;
        if let Some((end, name)) = take_declared(src, index) {
            push_item(&mut items, src, start, end, name);
            index = end;
            continue;
        }
        if at_word(src, index, "const") || at_word(src, index, "let") || at_word(src, index, "var")
        {
            let (end, name) = take_lexical(src, index);
            push_item(&mut items, src, start, end, name);
            index = end;
            continue;
        }
        let end = statement_end(src, index).0.max(next_index(src, index));
        push_item(&mut items, src, start, end, None);
        index = end;
    }
    items
}

fn push_item(items: &mut Vec<Item>, src: &str, start: usize, end: usize, name: Option<String>) {
    let end = end.max(start);
    let text = src[start..end].to_string();
    let uses = references(&text);
    match name {
        Some(name) => items.push(Item::Binding { name, text, uses }),
        None => items.push(Item::Loose { text, uses }),
    }
}

fn take_declared(src: &str, start: usize) -> Option<(usize, Option<String>)> {
    let mut index = start;
    if at_word(src, index, "async") {
        let next = skip_gap(src, index + "async".len());
        if !at_word(src, next, "function") {
            return None;
        }
        index = next;
    }
    let class = at_word(src, index, "class");
    if !class && !at_word(src, index, "function") {
        return None;
    }
    index += if class { "class".len() } else { "function".len() };
    index = skip_gap(src, index);
    if !class && src[index..].starts_with('*') {
        index = skip_gap(src, index + 1);
    }
    let name = word_at(src, index);
    body_end(src, start).map(|end| (end, name))
}

fn take_lexical(src: &str, start: usize) -> (usize, Option<String>) {
    let mut index = start;
    if at_word(src, index, "const") {
        index += "const".len();
    } else if at_word(src, index, "let") {
        index += "let".len();
    } else {
        index += "var".len();
    }
    index = skip_gap(src, index);
    if src[index..].starts_with('{') || src[index..].starts_with('[') {
        return (statement_end(src, start).0, None);
    }
    let name = word_at(src, index);
    let (end, comma) = statement_end(src, start);
    if comma { (end, None) } else { (end, name) }
}

fn body_end(src: &str, start: usize) -> Option<usize> {
    let mut index = start;
    let mut value = false;
    let mut paren = 0i32;
    let mut brace = 0i32;
    let mut seen = false;
    while index < src.len() {
        index = skip_gap(src, index);
        let Some((next, tok, next_value)) = token_at(src, index, value) else {
            return None;
        };
        value = next_value;
        index = next;
        match tok {
            Tok::Punct('(') => paren += 1,
            Tok::Punct(')') => paren -= 1,
            Tok::Punct('{') => {
                if paren == 0 && brace == 0 {
                    seen = true;
                }
                brace += 1;
            }
            Tok::Punct('}') => {
                brace -= 1;
                if seen && brace == 0 && paren == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// End index, and whether a comma appeared outside `()` `[]` `{}`.
fn statement_end(src: &str, start: usize) -> (usize, bool) {
    let mut index = start;
    let mut value = false;
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    let mut started = false;
    let mut root_comma = false;
    loop {
        let before_gap = index;
        index = skip_gap(src, index);
        if index >= src.len() {
            return (src.len(), root_comma);
        }
        let depth_zero = paren == 0 && bracket == 0 && brace == 0;
        if depth_zero && started && src[before_gap..index].contains('\n') && is_stmt_word(src, index)
        {
            return (before_gap, root_comma);
        }
        let Some((next, tok, next_value)) = token_at(src, index, value) else {
            return (src.len(), root_comma);
        };
        if next <= index {
            return (src.len(), root_comma);
        }
        value = next_value;
        index = next;
        started = true;
        if !depth_zero {
            match tok {
                Tok::Punct('(') => paren += 1,
                Tok::Punct(')') => paren -= 1,
                Tok::Punct('[') => bracket += 1,
                Tok::Punct(']') => bracket -= 1,
                Tok::Punct('{') => brace += 1,
                Tok::Punct('}') => brace -= 1,
                _ => {}
            }
            continue;
        }
        match tok {
            Tok::Punct(';') => return (index, root_comma),
            Tok::Punct(',') => root_comma = true,
            Tok::Punct('(') => paren += 1,
            Tok::Punct(')') => paren -= 1,
            Tok::Punct('[') => bracket += 1,
            Tok::Punct(']') => bracket -= 1,
            Tok::Punct('{') => brace += 1,
            Tok::Punct('}') => brace -= 1,
            _ => {}
        }
    }
}

fn references(src: &str) -> Vec<String> {
    let mut names = Vec::new();
    collect_refs(src, 0, src.len(), &mut names);
    names
}

fn collect_refs(src: &str, start: usize, end: usize, names: &mut Vec<String>) {
    let mut index = start;
    let mut value = false;
    let mut prev = Prev::Other;
    while index < end {
        index = skip_gap(src, index);
        if index >= end {
            break;
        }
        if src[index..].starts_with('`') {
            index = collect_template(src, index, names);
            prev = Prev::Other;
            value = true;
            continue;
        }
        let Some((next, tok, next_value)) = token_at(src, index, value) else {
            break;
        };
        if next <= index || next > end && !matches!(tok, Tok::Word(_)) {
            break;
        }
        let next = next.min(end);
        value = next_value;
        match tok {
            Tok::Word(name) => {
                let look = skip_gap(src, next);
                let next_ch = src.get(look..).and_then(|rest| rest.chars().next());
                let key = matches!(prev, Prev::Brace | Prev::Comma) && next_ch == Some(':');
                if !key && prev != Prev::Dot && !is_js_keyword(&name) {
                    names.push(name);
                }
                prev = Prev::Other;
            }
            Tok::Punct('?') if src[next..].starts_with('.') => {
                index = next + 1;
                prev = Prev::Dot;
                value = false;
                continue;
            }
            Tok::Punct('.') => prev = Prev::Dot,
            Tok::Punct('{') => prev = Prev::Brace,
            Tok::Punct(',') => prev = Prev::Comma,
            _ => prev = Prev::Other,
        }
        index = next;
    }
}

fn collect_template(src: &str, start: usize, names: &mut Vec<String>) -> usize {
    let bytes = src.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'`' => return index + 1,
            b'$' if bytes.get(index + 1) == Some(&b'{') => {
                let inner = index + 2;
                let close = interp_end(src, inner);
                collect_refs(src, inner, close, names);
                index = (close + 1).min(bytes.len());
            }
            _ => {
                index += src[index..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
            }
        }
    }
    index
}

fn interp_end(src: &str, start: usize) -> usize {
    let mut index = start;
    let mut depth = 1i32;
    let mut value = false;
    while index < src.len() && depth > 0 {
        index = skip_gap(src, index);
        if index >= src.len() {
            return src.len();
        }
        if src[index..].starts_with('`') {
            let end = end_of_template(src, index).unwrap_or(src.len());
            index = end;
            value = true;
            continue;
        }
        let Some((next, tok, next_value)) = token_at(src, index, value) else {
            return src.len();
        };
        value = next_value;
        match tok {
            Tok::Punct('{') => depth += 1,
            Tok::Punct('}') => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
        index = next;
    }
    index
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    Other,
    Dot,
    Brace,
    Comma,
}

enum Tok {
    Word(String),
    Punct(char),
    Lit,
}

fn token_at(src: &str, index: usize, value: bool) -> Option<(usize, Tok, bool)> {
    if index >= src.len() {
        return None;
    }
    let ch = src[index..].chars().next()?;
    if ch == '"' || ch == '\'' {
        return Some((end_of_string(src, index).unwrap_or(src.len()), Tok::Lit, true));
    }
    if ch == '`' {
        return Some((end_of_template(src, index).unwrap_or(src.len()), Tok::Lit, true));
    }
    if ch.is_ascii_digit() {
        let mut end = end_of_number(src, index);
        if src[end..].starts_with('n')
            && !src[end + 1..].chars().next().is_some_and(is_ident_cont)
        {
            end += 1;
        }
        return Some((end, Tok::Lit, true));
    }
    if ch == '/' && !value {
        if let Some(end) = end_of_regex(src, index) {
            return Some((end, Tok::Lit, true));
        }
    }
    if is_ident_start(ch) {
        let word = word_at(src, index)?;
        return Some((index + word.len(), Tok::Word(word), true));
    }
    let end = index + ch.len_utf8();
    Some((end, Tok::Punct(ch), matches!(ch, ')' | ']' | '}')))
}

fn is_stmt_word(src: &str, index: usize) -> bool {
    at_word(src, index, "function")
        || at_word(src, index, "class")
        || at_word(src, index, "const")
        || at_word(src, index, "let")
        || at_word(src, index, "var")
        || at_word(src, index, "async")
}

fn at_word(src: &str, index: usize, word: &str) -> bool {
    let Some(rest) = src.get(index..) else {
        return false;
    };
    if !rest.starts_with(word) {
        return false;
    }
    if rest[word.len()..].chars().next().is_some_and(is_ident_cont) {
        return false;
    }
    index == 0 || !src[..index].chars().next_back().is_some_and(is_ident_cont)
}

fn word_at(src: &str, index: usize) -> Option<String> {
    let ch = src.get(index..)?.chars().next()?;
    if !is_ident_start(ch) {
        return None;
    }
    let mut end = index + ch.len_utf8();
    while let Some(next) = src.get(end..).and_then(|rest| rest.chars().next()) {
        if !is_ident_cont(next) {
            break;
        }
        end += next.len_utf8();
    }
    Some(src[index..end].to_string())
}

fn skip_gap(src: &str, mut index: usize) -> usize {
    loop {
        while let Some(ch) = src.get(index..).and_then(|rest| rest.chars().next()) {
            if !ch.is_whitespace() {
                break;
            }
            index += ch.len_utf8();
        }
        if src[index..].starts_with("//") {
            index += 2;
            while index < src.len() && !src[index..].starts_with('\n') {
                index += src[index..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
            }
            continue;
        }
        if src[index..].starts_with("/*") {
            index += 2;
            while index + 1 < src.len() && !src[index..].starts_with("*/") {
                index += src[index..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
            }
            if src[index..].starts_with("*/") {
                index += 2;
            }
            continue;
        }
        return index;
    }
}

fn next_index(src: &str, index: usize) -> usize {
    index + src[index..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(0)
}

fn is_js_keyword(name: &str) -> bool {
    matches!(
        name,
        "async"
            | "await"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "let"
            | "new"
            | "null"
            | "return"
            | "static"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "undefined"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
            | "of"
    )
}

#[cfg(test)]
mod tests {
    use super::{references, split_items, strip_entry_guard, Item};

    #[test]
    fn strips_the_node_entry_guard() {
        let js = "\
import { realpathSync as plyRealpath } from \"node:fs\";
import { pathToFileURL as plyPathToFileURL } from \"node:url\";
export function ping() { return 1; }
let plyIsEntry = import.meta.main === true;
if (import.meta.main === undefined && process.argv[1]) {
  try {
    plyIsEntry = import.meta.url === plyPathToFileURL(plyRealpath(process.argv[1])).href;
  } catch {
    plyIsEntry = false;
  }
}
if (plyIsEntry) main();
";
        let body = strip_entry_guard(js).unwrap();
        assert_eq!(body, "export function ping() { return 1; }\n");
        let bare = "export function ping() { return 1; }\n";
        assert_eq!(strip_entry_guard(bare).unwrap(), bare);
        let leftover = "import { realpathSync as plyRealpath } from \"node:fs\";\nexport function ping() {}\n";
        assert!(strip_entry_guard(leftover).is_err());
    }

    #[test]
    fn splits_bindings_and_ignores_object_keys() {
        let src = "\
function ping() { inner(); n += 1; new Marker(); return `x ${g()}`; }
function boom() { extra(); }
let n = 0;
class Marker {}
const made = { tag: \"Effect\", value: n };
";
        let items = split_items(src);
        let names: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                Item::Binding { name, .. } => Some(name.as_str()),
                Item::Loose { .. } => None,
            })
            .collect();
        assert_eq!(names, ["ping", "boom", "n", "Marker", "made"]);
        let uses = match &items[0] {
            Item::Binding { uses, .. } => uses.clone(),
            Item::Loose { .. } => panic!("ping"),
        };
        assert!(uses.contains(&"inner".to_string()), "{uses:?}");
        assert!(uses.contains(&"n".to_string()), "{uses:?}");
        assert!(uses.contains(&"Marker".to_string()), "{uses:?}");
        assert!(uses.contains(&"g".to_string()), "{uses:?}");
        let made = references("const made = { tag: \"Effect\", value: n };");
        assert!(made.contains(&"n".to_string()), "{made:?}");
        assert!(!made.contains(&"tag".to_string()), "{made:?}");
        assert!(!made.contains(&"value".to_string()), "{made:?}");
    }
}
