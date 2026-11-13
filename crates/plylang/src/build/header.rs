//! JavaScript import header, and the Node guard around `main`.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::anf::{js_ident, referenced_words, AnfModule, MethodHome};
use crate::ast::{Def, Exports, Import, Item, Module, Type};
use crate::codes::Error;
use crate::diagnostic::Diagnostic;

use super::{classify, located, module_key_of, BuildError, Session, Spec, ENTRY_WORD};

impl Session {
    pub(super) fn js_header(&self, id: usize, from_js: &Path, anf: &AnfModule) -> String {
        if matches!(self.units[id].module.exports, Exports::List(_)) {
            return self.list_js_header(id, from_js, anf);
        }
        self.closed_js_header(id, from_js, anf)
    }

    /// A `@` list. A name from an import is re-exported when the body does
    /// not mention it. A mentioned name is imported and exported as a binding.
    /// An instance word is imported from the module that defines it.
    fn list_js_header(&self, id: usize, from_js: &Path, anf: &AnfModule) -> String {
        let listed: HashSet<String> = self.units[id]
            .module
            .exports
            .list()
            .expect("name list")
            .names
            .iter()
            .map(|item| item.name.clone())
            .collect();
        let imports = self.units[id].module.imports.clone();
        let deps = self.units[id].deps.clone();
        let referenced: HashSet<String> = referenced_words(anf).into_iter().collect();
        let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
        let mut reexports: Vec<(String, Vec<(String, String)>)> = Vec::new();
        let mut local_exports: Vec<String> = Vec::new();
        for (import, dep) in imports.iter().zip(&deps) {
            let brought = self.imported_values(*dep, import);
            if brought.is_empty() {
                continue;
            }
            let Some(spec) = self.import_spec(import, *dep, from_js) else {
                continue;
            };
            let mut local_names = Vec::new();
            let mut reexport_names = Vec::new();
            for (remote, local_name) in brought {
                if listed.contains(&local_name) && !referenced.contains(&local_name) {
                    reexport_names.push((remote, local_name));
                } else {
                    if listed.contains(&local_name) && !local_exports.contains(&local_name) {
                        local_exports.push(local_name.clone());
                    }
                    local_names.push((remote, local_name));
                }
            }
            if !local_names.is_empty() {
                push_group(&mut groups, spec.clone(), local_names);
            }
            if !reexport_names.is_empty() {
                push_group(&mut reexports, spec, reexport_names);
            }
        }
        self.import_referenced(
            id,
            from_js,
            anf,
            &imports,
            &deps,
            &mut groups,
            Some((&listed, &mut local_exports)),
        );
        let mut header = String::new();
        for (spec, names) in &groups {
            header.push_str(&import_line(spec, names));
            header.push('\n');
        }
        let sibling = self.foreign_js(id);
        header.push_str(&foreign_header(&self.units[id].module, sibling, from_js));
        if !local_exports.is_empty() {
            let exported = local_exports
                .iter()
                .map(|name| export_binding(name))
                .collect::<Vec<_>>()
                .join(", ");
            header.push_str(&format!("export {{ {exported} }};\n"));
        }
        for (spec, names) in &reexports {
            header.push_str(&reexport_line(spec, names));
            header.push('\n');
        }
        header
    }

    fn spec_for_extra(
        &self,
        id: usize,
        name: &str,
        from_js: &Path,
        imports: &[Import],
        deps: &[usize],
        homes: &[MethodHome],
    ) -> Option<String> {
        if let Some((home, _)) = self.place_helper(id, name) {
            if home == id {
                return None;
            }
            return self.unit_js_spec(home, from_js);
        }
        for (import, dep) in imports.iter().zip(deps) {
            let surface = self.value_surface(*dep);
            if import
                .bindings(surface)
                .iter()
                .any(|(_, local_name)| local_name == name)
            {
                return self.import_spec(import, *dep, from_js);
            }
        }
        let home = self.method_unit(name, homes)?;
        if home == id {
            return None;
        }
        self.unit_js_spec(home, from_js)
    }

    /// The module whose `#` selected this word.
    fn method_unit(&self, name: &str, homes: &[MethodHome]) -> Option<usize> {
        let home = homes.iter().find(|home| home.local == name)?;
        self.units.iter().position(|unit| {
            module_key_of(&unit.package_root, unit.module_path.as_deref()) == home.origin
                || unit.path.display().to_string() == home.origin
        })
    }

    fn unit_js_spec(&self, id: usize, from_js: &Path) -> Option<String> {
        let unit = &self.units[id];
        let module_path = unit.module_path.as_deref().unwrap_or("");
        if unit.package_root == self.package.root {
            if module_path.is_empty() {
                return None;
            }
            let to_js = self.package.out.join(module_path).with_extension("js");
            return Some(relative_specifier(from_js, &to_js));
        }
        let name = npm_name(&unit.package_root)?;
        if module_path.is_empty() {
            Some(name)
        } else {
            Some(format!("{name}/{module_path}"))
        }
    }

    /// A file with no name list. Test entries are this. Imports are used here
    /// and are not re-exported. Every local word is a JavaScript export, so the
    /// runner can call it.
    fn closed_js_header(&self, id: usize, from_js: &Path, anf: &AnfModule) -> String {
        let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
        let imports = self.units[id].module.imports.clone();
        let deps = self.units[id].deps.clone();
        for (import, dep) in imports.iter().zip(&deps) {
            let names = self.imported_values(*dep, import);
            if names.is_empty() {
                continue;
            }
            let Some(spec) = self.import_spec(import, *dep, from_js) else {
                continue;
            };
            push_group(&mut groups, spec, names);
        }
        self.import_referenced(id, from_js, anf, &imports, &deps, &mut groups, None);
        let mut header = String::new();
        for (spec, names) in &groups {
            header.push_str(&import_line(spec, names));
            header.push('\n');
        }
        let foreign = foreign_names(&self.units[id].module);
        if !foreign.is_empty() {
            let sibling = self.foreign_js(id).expect("foreign directory");
            let spec = relative_specifier(from_js, &sibling);
            let names: Vec<(String, String)> = foreign
                .iter()
                .map(|(name, _)| (name.clone(), name.clone()))
                .collect();
            header.push_str(&import_line(&spec, &names));
            header.push('\n');
            let exported = foreign
                .iter()
                .filter(|(_, private)| !private)
                .map(|(name, _)| export_binding(name))
                .collect::<Vec<_>>()
                .join(", ");
            if !exported.is_empty() {
                header.push_str(&format!("export {{ {exported} }};\n"));
            }
        }
        header
    }

    /// Names the body mentions that this module does not define.
    /// A list header also exports a name the `@` list already published.
    fn import_referenced(
        &self,
        id: usize,
        from_js: &Path,
        anf: &AnfModule,
        imports: &[Import],
        deps: &[usize],
        groups: &mut Vec<(String, Vec<(String, String)>)>,
        mut export_listed: Option<(&HashSet<String>, &mut Vec<String>)>,
    ) {
        let local = local_runtime_names(&self.units[id].module);
        let mut extra: Vec<String> = referenced_words(anf)
            .into_iter()
            .filter(|name| !local.contains(name))
            .collect();
        extra.sort();
        extra.dedup();
        for name in extra {
            if groups
                .iter()
                .any(|(_, names)| names.iter().any(|(_, local_name)| local_name == &name))
            {
                continue;
            }
            let Some(spec) =
                self.spec_for_extra(id, &name, from_js, imports, deps, &anf.method_homes)
            else {
                continue;
            };
            let pair = self.extra_pair(id, &name, imports, deps, &anf.method_homes);
            push_group(groups, spec, vec![pair]);
            if let Some((listed, local_exports)) = export_listed.as_mut() {
                if listed.contains(&name) && !local_exports.contains(&name) {
                    local_exports.push(name);
                }
            }
        }
    }

    /// Published JavaScript values of every unit.
    ///
    /// `visit` has already rejected an import cycle, so a module's list does not
    /// depend on the caller. One pass, dependencies first, keeps a facade from
    /// walking the same module on every import path.
    pub(super) fn fill_surfaces(&mut self) {
        self.surfaces = vec![Vec::new(); self.units.len()];
        for index in 0..self.topo.len() {
            let id = self.topo[index];
            let surface = self.surface_of(id);
            self.surfaces[id] = surface;
        }
    }

    /// Values this module publishes: its own, and imported values named again
    /// in the `@` list. [`Session::fill_surfaces`] runs before any header.
    fn value_surface(&self, id: usize) -> &[String] {
        &self.surfaces[id]
    }

    pub(super) fn surface_of(&self, id: usize) -> Vec<String> {
        let Exports::List(list) = &self.units[id].module.exports else {
            return Vec::new();
        };
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        let listed: HashSet<&str> = list.names.iter().map(|item| item.name.as_str()).collect();
        for name in crate::surface::own_public_values(&self.units[id].module) {
            push_unique(&mut names, &mut seen, name);
        }
        for (import, dep) in self.units[id]
            .module
            .imports
            .iter()
            .zip(&self.units[id].deps)
        {
            for (_, local_name) in import.bindings(&self.surfaces[*dep]) {
                if listed.contains(local_name.as_str()) {
                    push_unique(&mut names, &mut seen, local_name);
                }
            }
        }
        names
    }

    fn imported_values(&self, dep: usize, import: &Import) -> Vec<(String, String)> {
        import.bindings(self.value_surface(dep))
    }

    /// `ply_set_` and `ply_bind_` come from the module that declares the place.
    /// A facade that only reexports the reader is not that module.
    fn place_helper(&self, id: usize, name: &str) -> Option<(usize, String)> {
        let local = name
            .strip_prefix("ply_set_")
            .or_else(|| name.strip_prefix("ply_bind_"))?;
        let mut seen = HashSet::new();
        self.place_defining(id, local, &mut seen)
    }

    fn place_defining(
        &self,
        id: usize,
        local: &str,
        seen: &mut HashSet<usize>,
    ) -> Option<(usize, String)> {
        if !seen.insert(id) {
            return None;
        }
        if self.units[id]
            .module
            .items
            .iter()
            .any(|item| matches!(item, Item::Place(place) if place.name == local))
        {
            return Some((id, local.to_string()));
        }
        let imports = self.units[id].module.imports.clone();
        let deps = self.units[id].deps.clone();
        for (import, dep) in imports.iter().zip(deps) {
            let Some(remote) = import.remote_of(local) else {
                continue;
            };
            if let Some(found) = self.place_defining(dep, remote, seen) {
                return Some(found);
            }
        }
        None
    }

    /// A referenced name that was not already grouped. An import binds
    /// `(remote, local)`. An instance word keeps its own name.
    /// A place helper's remote name is the defining module's `ply_set_` or `ply_bind_`.
    fn extra_pair(
        &self,
        _id: usize,
        name: &str,
        imports: &[Import],
        deps: &[usize],
        homes: &[MethodHome],
    ) -> (String, String) {
        if let Some(home) = homes.iter().find(|home| home.local == name) {
            return (home.remote.clone(), home.local.clone());
        }
        if let Some((_, canon)) = self.place_helper(_id, name) {
            let prefix = if name.starts_with("ply_bind_") {
                "ply_bind_"
            } else {
                "ply_set_"
            };
            return (format!("{prefix}{canon}"), name.to_string());
        }
        for (import, dep) in imports.iter().zip(deps) {
            if let Some((remote, _)) = import
                .bindings(self.value_surface(*dep))
                .into_iter()
                .find(|(_, local_name)| local_name == name)
            {
                return (remote, name.to_string());
            }
        }
        (name.to_string(), name.to_string())
    }

    /// `src/Ply/Int.ply` is implemented by `foreign/Ply/Int.js`.
    pub(super) fn foreign_js(&self, id: usize) -> Option<PathBuf> {
        let module_path = self.units[id].module_path.as_deref()?;
        Some(
            self.package
                .foreign
                .as_ref()?
                .join(module_path)
                .with_extension("js"),
        )
    }

    fn import_spec(&self, import: &Import, dep: usize, from_js: &Path) -> Option<String> {
        match classify(&import.path) {
            Some(Spec::Package { .. }) | None => Some(import.path.clone()),
            Some(Spec::Local(_)) => {
                let module_path = self.units[dep].module_path.as_ref()?;
                let to_js = self.package.out.join(module_path).with_extension("js");
                Some(relative_specifier(from_js, &to_js))
            }
        }
    }
}

fn foreign_header(module: &Module, sibling: Option<PathBuf>, from_js: &Path) -> String {
    let foreign = foreign_names(module);
    if foreign.is_empty() {
        return String::new();
    }
    let Some(sibling) = sibling else {
        return String::new();
    };
    let spec = relative_specifier(from_js, &sibling);
    let names: Vec<(String, String)> = foreign
        .iter()
        .map(|(name, _)| (name.clone(), name.clone()))
        .collect();
    let mut header = import_line(&spec, &names);
    header.push('\n');
    let exported = foreign
        .iter()
        .filter(|(_, hidden)| !hidden)
        .map(|(name, _)| export_binding(name))
        .collect::<Vec<_>>()
        .join(", ");
    if !exported.is_empty() {
        header.push_str(&format!("export {{ {exported} }};\n"));
    }
    header
}

fn npm_name(root: &Path) -> Option<String> {
    let text = fs::read_to_string(root.join("package.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let name = value.get("name")?.as_str()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn local_runtime_names(module: &Module) -> HashSet<String> {
    let mut names = HashSet::new();
    for item in &module.items {
        match item {
            Item::Data(data) => {
                for ctor in &data.ctors {
                    names.insert(ctor.name.clone());
                }
            }
            Item::Def(def) => {
                names.insert(def.name.clone());
            }
            Item::Place(place) => {
                names.insert(place.name.clone());
                names.insert(format!("ply_set_{}", place.name));
                names.insert(format!("ply_bind_{}", place.name));
            }
            Item::Class(_) | Item::Instance(_) | Item::Synonym(_) => {}
        }
    }
    names
}

fn foreign_names(module: &Module) -> Vec<(String, bool)> {
    module
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Def(def) if def.vis.is_foreign() => Some((
                def.name.clone(),
                !crate::surface::js_exports_def(module, def),
            )),
            _ => None,
        })
        .collect()
}

fn push_unique(names: &mut Vec<String>, seen: &mut HashSet<String>, name: String) {
    if seen.insert(name.clone()) {
        names.push(name);
    }
}

fn push_group(
    groups: &mut Vec<(String, Vec<(String, String)>)>,
    spec: String,
    names: Vec<(String, String)>,
) {
    if let Some((_, existing)) = groups.iter_mut().find(|(have, _)| have == &spec) {
        for pair in names {
            if !existing.iter().any(|have| have.1 == pair.1) {
                existing.push(pair);
            }
        }
    } else {
        groups.push((spec, names));
    }
}

fn import_line(spec: &str, names: &[(String, String)]) -> String {
    let bindings = names
        .iter()
        .map(|(remote, local)| import_binding(remote, local))
        .collect::<Vec<_>>()
        .join(", ");
    format!("import {{ {bindings} }} from \"{spec}\";")
}

fn import_binding(remote: &str, local: &str) -> String {
    let js = js_ident(local);
    if js == remote {
        remote.to_string()
    } else {
        format!("{remote} as {js}")
    }
}

fn reexport_line(spec: &str, names: &[(String, String)]) -> String {
    let bindings = names
        .iter()
        .map(|(remote, local)| {
            if remote == local {
                remote.clone()
            } else {
                format!("{remote} as {local}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("export {{ {bindings} }} from \"{spec}\";")
}

fn export_binding(name: &str) -> String {
    let local = js_ident(name);
    if local == name {
        name.to_string()
    } else {
        format!("{local} as {name}")
    }
}

pub(super) fn combine(header: &str, body: &str) -> String {
    if header.is_empty() {
        body.to_string()
    } else if body.is_empty() {
        header.to_string()
    } else {
        format!("{header}\n{body}")
    }
}

pub(super) fn relative_specifier(from_file: &Path, to_file: &Path) -> String {
    let from: Vec<Component<'_>> = from_file
        .parent()
        .unwrap_or(Path::new(""))
        .components()
        .collect();
    let to: Vec<Component<'_>> = to_file.components().collect();
    let mut shared = 0;
    while shared < from.len() && shared < to.len() && from[shared] == to[shared] {
        shared += 1;
    }
    let mut parts = Vec::new();
    for _ in shared..from.len() {
        parts.push("..".to_string());
    }
    for component in &to[shared..] {
        parts.push(component.as_os_str().to_string_lossy().into_owned());
    }
    let spec = parts.join("/");
    if spec.starts_with('.') {
        spec
    } else {
        format!("./{spec}")
    }
}

pub(super) fn reject_bad_entry(module: &Module) -> Result<(), Diagnostic> {
    match entry_def(module) {
        Some(def) if !accepts_entry(def) => Err(Diagnostic::new(def.name_span, Error::EntryWord)),
        _ => Ok(()),
    }
}

pub(super) fn apply_entry(
    path: &Path,
    source: &str,
    module: &Module,
    js: String,
) -> Result<String, BuildError> {
    match entry_def(module) {
        Some(def) if accepts_entry(def) => Ok(with_entry_guard(js)),
        Some(def) => Err(located(
            path,
            source,
            Diagnostic::new(def.name_span, Error::EntryWord),
        )),
        None => Ok(js),
    }
}

pub(super) fn entry_def(module: &Module) -> Option<&Def> {
    module.items.iter().find_map(|item| match item {
        Item::Def(def) if def.name == ENTRY_WORD => Some(def),
        _ => None,
    })
}

/// `( -- Unit )`, including a word left off the `@` list. No constraints, no effects, no inputs.
pub(super) fn accepts_entry(def: &Def) -> bool {
    let scheme = &def.scheme;
    if !scheme.constraints.is_empty() || scheme.effect.effects.is_some() {
        return false;
    }
    scheme.effect.input.items.is_empty()
        && matches!(
            scheme.effect.output.items.as_slice(),
            [Type::Ctor(name)] if name == "Unit"
        )
}

/// `main` runs when Node started this file. `import.meta.main` is that check.
/// The guard imports nothing: a module with `main` is still imported by other
/// files, and those importers must not gain `node:fs`.
pub(super) fn with_entry_guard(js: String) -> String {
    let call = entry_guard(&js);
    let mut js = js;
    if !js.ends_with('\n') {
        js.push('\n');
    }
    js.push_str(&call);
    js
}

fn entry_guard(js: &str) -> String {
    let mut used = Vec::new();
    let flag = take_js_name(js, &mut used, "plyIsEntry");
    let call = js_ident(ENTRY_WORD);
    format!("let {flag} = import.meta.main === true;\nif ({flag}) {call}();\n")
}

fn take_js_name(js: &str, used: &mut Vec<String>, base: &str) -> String {
    let mut name = base.to_string();
    let mut n = 2u32;
    while js_word(js, &name) || used.iter().any(|have| have == &name) {
        name = format!("{base}{n}");
        n += 1;
    }
    used.push(name.clone());
    name
}

fn js_word(js: &str, name: &str) -> bool {
    js.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .any(|word| word == name)
}
