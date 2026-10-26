//! `plylang build` compiles a package: resolve imports, write `.js` and `.d.ply`.
//! `plylang build --exports` then replaces the `exports` field with those modules.
//! `plylang check` runs that compile and writes nothing.
//! A word named `main` with scheme `( -- Unit )` is called at the bottom of its
//! module when Node starts that file.
//!
//! A local import is a module path such as `Data/Either`. A package import is
//! `@scope/name/...` or an unscoped `name/...`. `plylang check --js` prints the
//! JavaScript `build` would write for one source file. A file outside a package
//! is printed alone, and its imports are not resolved.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::anf::{js_ident, referenced_words, AnfModule};
use crate::ast::{scheme_string, Def, Import, Item, Module, Type};
use crate::check::{
    check, check_with_imports, collect_sites, collect_sites_with_imports, KnownModule,
    ResolvedImport, StackAt,
};
use crate::codes::{error_url, Error};
use crate::diagnostic::Diagnostic;
use crate::parser::parse;
use crate::paths::{mode_from_path, module_name_from_path, ParseMode};
use crate::span::{line_col, Span};
use crate::LANGUAGE_VERSION;

#[derive(Debug)]
pub struct BuildError {
    pub message: String,
    /// `PL0001`. A read or write that is not `source directory not found` is `PL0112`.
    pub code: &'static str,
    rendered: String,
}

impl BuildError {
    pub fn render(&self) -> String {
        self.rendered.clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Gray,
    Black,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Local,
    External,
}

struct Unit {
    path: PathBuf,
    display: String,
    source: String,
    module: Module,
    package_root: PathBuf,
    /// `Data/Num` for a file of the package being built.
    module_path: Option<String>,
    kind: Kind,
    color: Color,
    deps: Vec<usize>,
}

/// Word that starts its module when Node runs the compiled file. Scheme `( -- Unit )`.
const ENTRY_WORD: &str = "main";

pub(crate) struct Package {
    pub(crate) root: PathBuf,
    pub(crate) source: PathBuf,
    /// npm `name`, when the field is a non-empty string.
    pub(crate) name: Option<String>,
    /// Hand-written JavaScript for `foreign` words. `None` when the package has none.
    foreign: Option<PathBuf>,
    pub(crate) out: PathBuf,
    out_rel: String,
    exports: HashMap<String, String>,
}

struct Output {
    js_path: PathBuf,
    js: String,
    iface_path: PathBuf,
    iface: String,
}

struct Session {
    package: Package,
    units: Vec<Unit>,
    by_path: HashMap<PathBuf, usize>,
    by_module: HashMap<String, usize>,
    exports_cache: HashMap<PathBuf, HashMap<String, String>>,
    topo: Vec<usize>,
    outputs: Vec<Output>,
}

enum Spec {
    Local(String),
    Package { name: String, subpath: String },
}

enum JsExport {
    Missing,
    Unknown,
    Arity(usize),
}

pub fn build(dir: &Path) -> Result<(), BuildError> {
    run_build(dir, false)
}

/// Build the package, then replace `package.json` `exports` with one entry per built module.
pub fn build_exports(dir: &Path) -> Result<(), BuildError> {
    run_build(dir, true)
}

fn run_build(dir: &Path, write_exports: bool) -> Result<(), BuildError> {
    let mut session = open_session(dir)?;
    let entries = export_entries(&session.units, &session.package.out_rel);
    if write_exports {
        session.package.exports = entries
            .iter()
            .map(|entry| (entry.key.clone(), entry.ply.clone()))
            .collect();
    }
    session.check_all()?;
    session.write_outputs()?;
    if write_exports {
        write_package_exports(&session.package.root, &entries)?;
    }
    Ok(())
}

/// Package check: the same work as [`build`], without writing `.js` or `.d.ply`.
pub fn check_package(dir: &Path) -> Result<(), BuildError> {
    let mut session = open_session(dir)?;
    session.check_all()
}

/// JavaScript for one file. A package source is the text [`build`] would write.
/// Anything else is lowered alone, and its imports are not resolved.
pub fn javascript_file(path: &Path) -> Result<String, BuildError> {
    if let Some(js) = package_javascript(path)? {
        return Ok(js);
    }
    snippet_javascript(path)
}

fn open_session(dir: &Path) -> Result<Session, BuildError> {
    let root = fs::canonicalize(dir).map_err(|error| os_error(dir, error))?;
    let package = read_package(&root)?;
    let mut session = Session {
        package,
        units: Vec::new(),
        by_path: HashMap::new(),
        by_module: HashMap::new(),
        exports_cache: HashMap::new(),
        topo: Vec::new(),
        outputs: Vec::new(),
    };
    session.collect_sources()?;
    Ok(session)
}

fn package_javascript(path: &Path) -> Result<Option<String>, BuildError> {
    let Some(root) = find_ply_root(path) else {
        return Ok(None);
    };
    let Ok(file) = fs::canonicalize(path) else {
        return Ok(None);
    };
    let mut session = open_session(&root)?;
    let Some(id) = session.by_path.get(&file).copied() else {
        return Ok(None);
    };
    session.emit_one(id).map(Some)
}

fn snippet_javascript(path: &Path) -> Result<String, BuildError> {
    let source = fs::read_to_string(path).map_err(|error| os_error(path, error))?;
    let module = parse(&source, mode_from_path(&path.to_string_lossy()))
        .map_err(|error| located(path, &source, error))?;
    let js = crate::check::javascript(&module).map_err(|error| located(path, &source, error))?;
    apply_entry(path, &source, &module, js)
}

/// Check one editor buffer. Inside a package, imports resolve as in `plylang build`.
/// Outside a package this is the single-file checker. `plylang check` checks the
/// whole package and does not call this.
pub fn check_buffer(path: &Path, source: &str) -> Result<(), Diagnostic> {
    let mode = if path.to_string_lossy().ends_with(".d.ply") {
        ParseMode::Interface
    } else {
        ParseMode::Source
    };
    let module = parse(source, mode)?;
    if mode == ParseMode::Source {
        reject_bad_entry(&module)?;
    }
    let Some(root) = find_ply_root(path) else {
        return check(&module);
    };
    let package = read_package(&root).map_err(|error| {
        Diagnostic::with_code(Span::new(0, 0), error.code, error.message)
    })?;
    let mut doc = OpenDoc {
        package,
        units: Vec::new(),
        by_file: HashMap::new(),
        by_module: HashMap::new(),
        exports_cache: HashMap::new(),
    };
    doc.push_buffer(path, module);
    doc.visit(0, &mut Vec::new(), None)?;
    doc.check_open()
}

/// Stack notes for the editor. Same import rules as [`check_buffer`].
/// A failed check still returns the terms reached before the error.
/// A package that cannot be resolved falls back to a single-file check.
pub fn stack_sites(path: &Path, source: &str) -> Vec<StackAt> {
    let mode = if path.to_string_lossy().ends_with(".d.ply") {
        ParseMode::Interface
    } else {
        ParseMode::Source
    };
    let Ok(module) = parse(source, mode) else {
        return Vec::new();
    };
    let Some(root) = find_ply_root(path) else {
        return collect_sites(&module);
    };
    let Ok(package) = read_package(&root) else {
        return collect_sites(&module);
    };
    let mut doc = OpenDoc {
        package,
        units: Vec::new(),
        by_file: HashMap::new(),
        by_module: HashMap::new(),
        exports_cache: HashMap::new(),
    };
    doc.push_buffer(path, module.clone());
    if doc.visit(0, &mut Vec::new(), None).is_err() {
        return collect_sites(&module);
    }
    doc.sites()
}

/// Import paths the package of `from_file` can name: local exports, then
/// `ply` exports of installed dependencies. The current file is left out.
pub(crate) fn available_import_paths(from_file: &Path) -> Vec<String> {
    let Some(root) = find_ply_root(from_file) else {
        return Vec::new();
    };
    let Ok(package) = read_package(&root) else {
        return Vec::new();
    };
    let current = module_path_under(&package, from_file);
    let mut paths = Vec::new();
    for key in package.exports.keys() {
        let Some(path) = local_import_path(key) else {
            continue;
        };
        if current.as_deref() == Some(path.as_str()) {
            continue;
        }
        paths.push(path);
    }
    let text = fs::read_to_string(root.join("package.json")).unwrap_or_default();
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        for field in ["dependencies", "devDependencies", "optionalDependencies"] {
            let Some(deps) = value.get(field).and_then(Value::as_object) else {
                continue;
            };
            for name in deps.keys() {
                let Some(installed) = find_installed(from_file, name) else {
                    continue;
                };
                let Ok(pkg_text) = fs::read_to_string(installed.join("package.json")) else {
                    continue;
                };
                let Ok(pkg) = serde_json::from_str::<Value>(&pkg_text) else {
                    continue;
                };
                for key in export_map(&pkg).keys() {
                    if let Some(path) = package_import_path(name, key) {
                        paths.push(path);
                    }
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn local_import_path(key: &str) -> Option<String> {
    let rest = key.strip_prefix("./")?;
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

fn package_import_path(name: &str, key: &str) -> Option<String> {
    if key == "." {
        return Some(name.to_string());
    }
    let rest = key.strip_prefix("./")?;
    if rest.is_empty() {
        Some(name.to_string())
    } else {
        Some(format!("{name}/{rest}"))
    }
}

fn module_path_under(package: &Package, file: &Path) -> Option<String> {
    let file = canonical(file);
    let source = canonical(&package.source);
    if !file.starts_with(&source) {
        return None;
    }
    Some(module_path_of(&source, &file))
}

/// Read the module an import names, for hover and completion. Missing packages yield `None`.
pub(crate) fn load_provider(from_file: &Path, import_path: &str) -> Option<Module> {
    match classify(import_path)? {
        Spec::Local(module_path) => {
            let root = find_ply_root(from_file)?;
            let package = read_package(&root).ok()?;
            let file = package.source.join(&module_path).with_extension("ply");
            read_module(&file)
        }
        Spec::Package { name, subpath } => {
            let pkg_root = find_installed(from_file, &name)?;
            let text = fs::read_to_string(pkg_root.join("package.json")).ok()?;
            let value: Value = serde_json::from_str(&text).ok()?;
            let exports = export_map(&value);
            let target = exports.get(&export_key(&subpath))?;
            read_module(&target_file(&pkg_root, target))
        }
    }
}

impl Session {
    fn check_all(&mut self) -> Result<(), BuildError> {
        let ids: Vec<usize> = (0..self.units.len()).collect();
        for id in ids {
            self.visit(id, &mut Vec::new())?;
        }
        let order = self.topo.clone();
        for id in order {
            self.check_unit(id)?;
        }
        Ok(())
    }

    fn write_outputs(&self) -> Result<(), BuildError> {
        for output in &self.outputs {
            if let Some(parent) = output.js_path.parent() {
                fs::create_dir_all(parent).map_err(|error| os_error(parent, error))?;
            }
            fs::write(&output.js_path, &output.js)
                .map_err(|error| os_error(&output.js_path, error))?;
            if let Some(parent) = output.iface_path.parent() {
                fs::create_dir_all(parent).map_err(|error| os_error(parent, error))?;
            }
            fs::write(&output.iface_path, &output.iface)
                .map_err(|error| os_error(&output.iface_path, error))?;
        }
        Ok(())
    }

    /// Check one local module and the imports it reaches, then return its JavaScript.
    fn emit_one(&mut self, id: usize) -> Result<String, BuildError> {
        self.visit(id, &mut Vec::new())?;
        let order = self.topo.clone();
        for unit in order {
            self.check_unit(unit)?;
        }
        let module_path = self.units[id].module_path.clone().expect("local module");
        let js_path = self.package.out.join(module_path).with_extension("js");
        Ok(self
            .outputs
            .iter()
            .find(|output| output.js_path == js_path)
            .map(|output| output.js.clone())
            .expect("checked local module has javascript"))
    }

    fn collect_sources(&mut self) -> Result<(), BuildError> {
        if !self.package.source.is_dir() {
            return Err(io_error(
                &self.package.source,
                Error::SourceDirectoryNotFound,
            ));
        }
        let mut files = Vec::new();
        walk_ply(&self.package.source, &self.package.out, &mut files)
            .map_err(|error| os_error(&self.package.source, error))?;
        files.sort();
        for file in files {
            let source = fs::read_to_string(&file).map_err(|error| os_error(&file, error))?;
            let module = parse(&source, ParseMode::Source)
                .map_err(|error| located(&file, &source, error))?;
            let module_path = module_path_of(&self.package.source, &file);
            if let Err(message) = module_name_from_path(&format!("{module_path}.ply")) {
                return Err(located(
                    &file,
                    &source,
                    Diagnostic::new(module.span, message),
                ));
            }
            let canonical = fs::canonicalize(&file).unwrap_or(file.clone());
            let id = self.units.len();
            self.by_path.insert(canonical.clone(), id);
            self.by_module.insert(module_path.clone(), id);
            self.units.push(Unit {
                path: canonical,
                display: file.display().to_string(),
                source,
                module,
                package_root: self.package.root.clone(),
                module_path: Some(module_path),
                kind: Kind::Local,
                color: Color::White,
                deps: Vec::new(),
            });
        }
        Ok(())
    }

    fn visit(&mut self, id: usize, stack: &mut Vec<usize>) -> Result<(), BuildError> {
        if self.units[id].color != Color::White {
            return Ok(());
        }
        self.units[id].color = Color::Gray;
        stack.push(id);
        let count = self.units[id].module.imports.len();
        for index in 0..count {
            let path = self.units[id].module.imports[index].path.clone();
            let span = self.units[id].module.imports[index].span;
            let target = self.resolve(id, &path, span)?;
            if self.units[target].color == Color::Gray {
                return Err(self.fail(id, span, cycle_message(&self.units, stack, target)));
            }
            self.units[id].deps.push(target);
            self.visit(target, stack)?;
        }
        stack.pop();
        self.units[id].color = Color::Black;
        self.topo.push(id);
        Ok(())
    }

    fn resolve(&mut self, from: usize, import_path: &str, span: Span) -> Result<usize, BuildError> {
        let spec = classify(import_path).ok_or_else(|| {
            self.fail(from, span, Error::CannotResolveImport(import_path.to_string()))
        })?;
        match spec {
            Spec::Local(module_path) => self.resolve_local(from, &module_path, import_path, span),
            Spec::Package { name, subpath } => {
                self.resolve_package(from, &name, &subpath, import_path, span)
            }
        }
    }

    fn resolve_local(
        &mut self,
        from: usize,
        module_path: &str,
        import_path: &str,
        span: Span,
    ) -> Result<usize, BuildError> {
        let package_root = self.units[from].package_root.clone();
        if package_root == self.package.root {
            self.require_export(from, module_path, span)?;
            return self.by_module.get(module_path).copied().ok_or_else(|| {
                self.fail(from, span, Error::CannotResolveImport(import_path.to_string()))
            });
        }
        let exports = self.package_exports(&package_root)?;
        let Some(target) = exports.get(&export_key(module_path)).cloned() else {
            return Err(self.fail(from, span, Error::CannotResolveImport(import_path.to_string())));
        };
        let file = target_file(&package_root, &target);
        if !file.is_file() {
            return Err(self.fail(from, span, Error::CannotResolveImport(import_path.to_string())));
        }
        self.load_external(file, package_root)
    }

    fn resolve_package(
        &mut self,
        from: usize,
        name: &str,
        subpath: &str,
        import_path: &str,
        span: Span,
    ) -> Result<usize, BuildError> {
        let start = self.units[from].path.clone();
        let Some(pkg_root) = find_installed(&start, name) else {
            return Err(self.fail(from, span, Error::CannotResolveImport(import_path.to_string())));
        };
        let exports = self.package_exports(&pkg_root)?;
        let Some(target) = exports.get(&export_key(subpath)).cloned() else {
            return Err(self.fail(from, span, Error::CannotResolveImport(import_path.to_string())));
        };
        let file = target_file(&pkg_root, &target);
        if !file.is_file() {
            return Err(self.fail(from, span, Error::CannotResolveImport(import_path.to_string())));
        }
        self.load_external(file, pkg_root)
    }

    fn require_export(&self, from: usize, module_path: &str, span: Span) -> Result<(), BuildError> {
        let key = export_key(module_path);
        let expected = format!("./{}/{module_path}.d.ply", self.package.out_rel);
        let found = self
            .package
            .exports
            .get(&key)
            .map(|target| target.replace('\\', "/"))
            .as_deref()
            == Some(expected.as_str());
        if found {
            return Ok(());
        }
        Err(self.fail(
            from,
            span,
            Error::ModuleNotExported(module_path.replace('/', ".")),
        ))
    }

    fn load_external(&mut self, file: PathBuf, package_root: PathBuf) -> Result<usize, BuildError> {
        let canonical = fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
        if let Some(id) = self.by_path.get(&canonical).copied() {
            return Ok(id);
        }
        let source = fs::read_to_string(&file).map_err(|error| os_error(&file, error))?;
        let module = parse(&source, ParseMode::Interface)
            .map_err(|error| located(&file, &source, error))?;
        let id = self.units.len();
        self.by_path.insert(canonical.clone(), id);
        self.units.push(Unit {
            path: canonical,
            display: file.display().to_string(),
            source,
            module,
            package_root,
            module_path: None,
            kind: Kind::External,
            color: Color::White,
            deps: Vec::new(),
        });
        Ok(id)
    }

    fn package_exports(
        &mut self,
        root: &Path,
    ) -> Result<HashMap<String, String>, BuildError> {
        if let Some(cached) = self.exports_cache.get(root) {
            return Ok(cached.clone());
        }
        if root == self.package.root {
            return Ok(self.package.exports.clone());
        }
        let path = root.join("package.json");
        let text = fs::read_to_string(&path).map_err(|error| os_error(&path, error))?;
        let value: Value = serde_json::from_str(&text).map_err(|_| {
            pkg_error(&path, &text, Error::PackageJsonInvalid)
        })?;
        let exports = export_map(&value);
        self.exports_cache.insert(root.to_path_buf(), exports.clone());
        Ok(exports)
    }

    fn check_unit(&mut self, id: usize) -> Result<(), BuildError> {
        let import_count = self.units[id].module.imports.len();
        let mut providers = Vec::with_capacity(import_count);
        let mut knowns = Vec::with_capacity(import_count);
        let mut imports = Vec::with_capacity(import_count);
        for index in 0..import_count {
            let dep = self.units[id].deps[index];
            providers.push(self.units[dep].module.clone());
            knowns.push(self.closure(dep));
            imports.push(self.units[id].module.imports[index].clone());
        }
        let resolved: Vec<ResolvedImport<'_>> = (0..import_count)
            .map(|index| ResolvedImport {
                provider: &providers[index],
                import: &imports[index],
                known: &knowns[index],
            })
            .collect();
        let module = self.units[id].module.clone();
        let checked = check_with_imports(&module, &resolved)
            .map_err(|error| self.fail_diagnostic(id, error))?;
        if self.units[id].kind == Kind::External {
            return Ok(());
        }
        self.check_foreign(id, &checked.foreign_arity)?;
        let module_path = self.units[id].module_path.clone().expect("local module");
        let js_path = self
            .package
            .out
            .join(&module_path)
            .with_extension("js");
        let iface_path = self.package.out.join(format!("{module_path}.d.ply"));
        let header = self.js_header(id, &js_path, &checked.anf);
        let body = checked.anf.javascript();
        let js = combine(&header, &body);
        let js = match entry_def(&module) {
            Some(def) if accepts_entry(def) => with_entry_guard(js),
            Some(def) => return Err(self.fail(id, def.name_span, Error::EntryWord)),
            None => js,
        };
        let iface = interface_text(&module);
        self.outputs.push(Output {
            js_path,
            js,
            iface_path,
            iface,
        });
        Ok(())
    }

    fn closure(&self, id: usize) -> Vec<KnownModule> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut pending = vec![id];
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            out.push(KnownModule {
                origin: self.units[id].path.display().to_string(),
                module: self.units[id].module.clone(),
            });
            for dep in &self.units[id].deps {
                pending.push(*dep);
            }
        }
        out
    }

    fn check_foreign(&self, id: usize, arities: &[(String, usize)]) -> Result<(), BuildError> {
        if arities.is_empty() {
            return Ok(());
        }
        let Some(js_path) = self.foreign_js(id) else {
            let name = &arities[0].0;
            return Err(self.fail_on_def(
                id,
                name,
                Error::PackageJsonNoForeign,
            ));
        };
        let source = fs::read_to_string(&js_path).map_err(|error| os_error(&js_path, error))?;
        for (name, expected) in arities {
            match exported_arity(&source, name) {
                JsExport::Missing => {
                    return Err(self.fail_on_def(
                        id,
                        name,
                        Error::ForeignNotExported(name.to_string()),
                    ));
                }
                JsExport::Unknown => {}
                JsExport::Arity(found) if found != *expected => {
                    return Err(self.fail_on_def(
                        id,
                        name,
                        Error::ForeignArity { name: name.to_string(), expected: *expected, found },
                    ));
                }
                JsExport::Arity(_) => {}
            }
        }
        Ok(())
    }

    fn js_header(&self, id: usize, from_js: &Path, anf: &AnfModule) -> String {
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        let imports = &self.units[id].module.imports;
        let deps = &self.units[id].deps;
        for (import, dep) in imports.iter().zip(deps) {
            let names = value_names(&self.units[*dep].module, import);
            if names.is_empty() {
                continue;
            }
            let Some(spec) = self.import_spec(import, *dep, from_js) else {
                continue;
            };
            push_group(&mut groups, spec, names);
        }
        let local = local_runtime_names(&self.units[id].module);
        let mut extra: Vec<String> = referenced_words(anf)
            .into_iter()
            .filter(|name| !local.contains(name))
            .collect();
        extra.sort();
        extra.dedup();
        for name in extra {
            for (import, dep) in imports.iter().zip(deps) {
                if !public_values(&self.units[*dep].module)
                    .iter()
                    .any(|have| have == &name)
                {
                    continue;
                }
                let Some(spec) = self.import_spec(import, *dep, from_js) else {
                    continue;
                };
                push_group(&mut groups, spec, vec![name]);
                break;
            }
        }
        let mut header = String::new();
        for (spec, names) in &groups {
            header.push_str(&import_line(spec, names));
            header.push('\n');
        }
        let foreign = foreign_names(&self.units[id].module);
        if !foreign.is_empty() {
            let sibling = self.foreign_js(id).expect("foreign directory");
            let spec = relative_specifier(from_js, &sibling);
            let names: Vec<String> = foreign.iter().map(|(name, _)| name.clone()).collect();
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

    /// `src/Ply/Int.ply` is implemented by `foreign/Ply/Int.js`.
    fn foreign_js(&self, id: usize) -> Option<PathBuf> {
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

    fn fail(&self, id: usize, span: Span, error: Error) -> BuildError {
        located(
            Path::new(&self.units[id].display),
            &self.units[id].source,
            Diagnostic::new(span, error),
        )
    }

    fn fail_diagnostic(&self, id: usize, diagnostic: Diagnostic) -> BuildError {
        located(
            Path::new(&self.units[id].display),
            &self.units[id].source,
            diagnostic,
        )
    }

    fn fail_on_def(&self, id: usize, name: &str, error: Error) -> BuildError {
        let span = self.units[id]
            .module
            .items
            .iter()
            .find_map(|item| match item {
                Item::Def(def) if def.name == name => Some(def.name_span),
                _ => None,
            })
            .unwrap_or(self.units[id].module.span);
        self.fail(id, span, error)
    }
}

struct DocUnit {
    path: PathBuf,
    module: Module,
    package_root: PathBuf,
    /// `Data.Num` from the path, or the file name for a package outside this one.
    label: String,
    color: Color,
    deps: Vec<usize>,
}

struct OpenDoc {
    package: Package,
    units: Vec<DocUnit>,
    by_file: HashMap<PathBuf, usize>,
    by_module: HashMap<String, usize>,
    exports_cache: HashMap<PathBuf, HashMap<String, String>>,
}

impl OpenDoc {
    fn push_buffer(&mut self, path: &Path, module: Module) {
        let file = canonical(path);
        let source_root = canonical(&self.package.source);
        let module_path = file
            .starts_with(&source_root)
            .then(|| module_path_of(&source_root, &file));
        self.insert(file, module, self.package.root.clone(), module_path);
    }

    fn insert(
        &mut self,
        file: PathBuf,
        module: Module,
        package_root: PathBuf,
        module_path: Option<String>,
    ) -> usize {
        let label = module_path
            .as_deref()
            .map(|path| path.replace('/', "."))
            .unwrap_or_else(|| file_label(&file));
        let id = self.units.len();
        self.by_file.insert(file.clone(), id);
        if let Some(module_path) = module_path {
            self.by_module.insert(module_path, id);
        }
        self.units.push(DocUnit {
            path: file,
            module,
            package_root,
            label,
            color: Color::White,
            deps: Vec::new(),
        });
        id
    }

    fn visit(
        &mut self,
        id: usize,
        stack: &mut Vec<usize>,
        blame: Option<Span>,
    ) -> Result<(), Diagnostic> {
        if self.units[id].color != Color::White {
            return Ok(());
        }
        self.units[id].color = Color::Gray;
        stack.push(id);
        let count = self.units[id].module.imports.len();
        for index in 0..count {
            let path = self.units[id].module.imports[index].path.clone();
            let span = self.units[id].module.imports[index].span;
            let here = if id == 0 { span } else { blame.unwrap_or(span) };
            let target = self.resolve(id, &path, here)?;
            if self.units[target].color == Color::Gray {
                return Err(Diagnostic::new(here, doc_cycle(&self.units, stack, target)));
            }
            self.units[id].deps.push(target);
            self.visit(target, stack, Some(here))?;
        }
        stack.pop();
        self.units[id].color = Color::Black;
        Ok(())
    }

    fn resolve(&mut self, from: usize, import_path: &str, span: Span) -> Result<usize, Diagnostic> {
        let spec = classify(import_path).ok_or_else(|| missing(span, import_path))?;
        match spec {
            Spec::Local(module_path) => self.resolve_local(from, &module_path, import_path, span),
            Spec::Package { name, subpath } => {
                self.resolve_package(from, &name, &subpath, import_path, span)
            }
        }
    }

    fn resolve_local(
        &mut self,
        from: usize,
        module_path: &str,
        import_path: &str,
        span: Span,
    ) -> Result<usize, Diagnostic> {
        let package_root = self.units[from].package_root.clone();
        if package_root == self.package.root {
            self.require_export(module_path, span)?;
            if let Some(id) = self.by_module.get(module_path).copied() {
                return Ok(id);
            }
            let file = self.package.source.join(module_path).with_extension("ply");
            return self.load(file, package_root, Some(module_path.to_string()), span, import_path);
        }
        let exports = self.package_exports(&package_root, span, import_path)?;
        let Some(target) = exports.get(&export_key(module_path)).cloned() else {
            return Err(missing(span, import_path));
        };
        let file = target_file(&package_root, &target);
        self.load(file, package_root, None, span, import_path)
    }

    fn resolve_package(
        &mut self,
        from: usize,
        name: &str,
        subpath: &str,
        import_path: &str,
        span: Span,
    ) -> Result<usize, Diagnostic> {
        let start = self
            .by_file
            .iter()
            .find(|(_, id)| **id == from)
            .map(|(path, _)| path.clone())
            .unwrap_or_else(|| self.package.root.clone());
        let Some(pkg_root) = find_installed(&start, name) else {
            return Err(missing(span, import_path));
        };
        let exports = self.package_exports(&pkg_root, span, import_path)?;
        let Some(target) = exports.get(&export_key(subpath)).cloned() else {
            return Err(missing(span, import_path));
        };
        let file = target_file(&pkg_root, &target);
        self.load(file, pkg_root, None, span, import_path)
    }

    fn load(
        &mut self,
        file: PathBuf,
        package_root: PathBuf,
        module_path: Option<String>,
        span: Span,
        import_path: &str,
    ) -> Result<usize, Diagnostic> {
        if !file.is_file() {
            return Err(missing(span, import_path));
        }
        let canonical = canonical(&file);
        if let Some(id) = self.by_file.get(&canonical).copied() {
            return Ok(id);
        }
        let source = fs::read_to_string(&file).map_err(|error| {
            let os = Error::Os(error.to_string());
            Diagnostic::with_code(span, os.code(), format!("{}: {}", file.display(), os.message()))
        })?;
        let mode = if file.to_string_lossy().ends_with(".d.ply") {
            ParseMode::Interface
        } else {
            ParseMode::Source
        };
        let module = parse(&source, mode).map_err(|error| {
            let (line, col) = line_col(&source, error.span.start);
            Diagnostic::with_code(
                span,
                error.code,
                format!("{}:{line}:{col}: {}", file.display(), error.message),
            )
        })?;
        Ok(self.insert(canonical, module, package_root, module_path))
    }

    fn require_export(&self, module_path: &str, span: Span) -> Result<(), Diagnostic> {
        let key = export_key(module_path);
        let expected = format!("./{}/{module_path}.d.ply", self.package.out_rel);
        let found = self
            .package
            .exports
            .get(&key)
            .map(|target| target.replace('\\', "/"))
            .as_deref()
            == Some(expected.as_str());
        if found {
            return Ok(());
        }
        Err(Diagnostic::new(
            span,
            Error::ModuleNotExported(module_path.replace('/', ".")),
        ))
    }

    fn package_exports(
        &mut self,
        root: &Path,
        span: Span,
        import_path: &str,
    ) -> Result<HashMap<String, String>, Diagnostic> {
        if root == self.package.root {
            return Ok(self.package.exports.clone());
        }
        if let Some(cached) = self.exports_cache.get(root) {
            return Ok(cached.clone());
        }
        let path = root.join("package.json");
        let text = fs::read_to_string(&path).map_err(|_| missing(span, import_path))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| Diagnostic::new(span, Error::PackageJsonInvalid))?;
        let exports = export_map(&value);
        self.exports_cache.insert(root.to_path_buf(), exports.clone());
        Ok(exports)
    }

    fn check_open(&self) -> Result<(), Diagnostic> {
        let import_count = self.units[0].module.imports.len();
        let mut providers = Vec::with_capacity(import_count);
        let mut knowns = Vec::with_capacity(import_count);
        let mut imports = Vec::with_capacity(import_count);
        for index in 0..import_count {
            let dep = self.units[0].deps[index];
            providers.push(self.units[dep].module.clone());
            knowns.push(self.closure(dep));
            imports.push(self.units[0].module.imports[index].clone());
        }
        let resolved: Vec<ResolvedImport<'_>> = (0..import_count)
            .map(|index| ResolvedImport {
                provider: &providers[index],
                import: &imports[index],
                known: &knowns[index],
            })
            .collect();
        check_with_imports(&self.units[0].module, &resolved).map(|_| ())
    }

    fn sites(&self) -> Vec<StackAt> {
        let import_count = self.units[0].module.imports.len();
        let mut providers = Vec::with_capacity(import_count);
        let mut knowns = Vec::with_capacity(import_count);
        let mut imports = Vec::with_capacity(import_count);
        for index in 0..import_count {
            let dep = self.units[0].deps[index];
            providers.push(self.units[dep].module.clone());
            knowns.push(self.closure(dep));
            imports.push(self.units[0].module.imports[index].clone());
        }
        let resolved: Vec<ResolvedImport<'_>> = (0..import_count)
            .map(|index| ResolvedImport {
                provider: &providers[index],
                import: &imports[index],
                known: &knowns[index],
            })
            .collect();
        let sites = collect_sites_with_imports(&self.units[0].module, &resolved);
        if sites.is_empty() {
            collect_sites(&self.units[0].module)
        } else {
            sites
        }
    }

    fn closure(&self, id: usize) -> Vec<KnownModule> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let mut pending = vec![id];
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            out.push(KnownModule {
                origin: self.units[id].path.display().to_string(),
                module: self.units[id].module.clone(),
            });
            for dep in &self.units[id].deps {
                pending.push(*dep);
            }
        }
        out
    }
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn missing(span: Span, import_path: &str) -> Diagnostic {
    Diagnostic::new(span, Error::CannotResolveImport(import_path.to_string()))
}

fn doc_cycle(units: &[DocUnit], stack: &[usize], again: usize) -> Error {
    let start = stack.iter().position(|id| *id == again).unwrap_or(0);
    let mut names: Vec<String> = stack[start..]
        .iter()
        .map(|id| units[*id].label.clone())
        .collect();
    names.push(units[again].label.clone());
    Error::ImportCycle(names.join(", "))
}

fn read_package(root: &Path) -> Result<Package, BuildError> {
    let path = root.join("package.json");
    let text = fs::read_to_string(&path).map_err(|error| os_error(&path, error))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| pkg_error(&path, &text, Error::PackageJsonInvalid))?;
    let Some(ply) = value.get("ply") else {
        return Err(pkg_error(&path, &text, Error::PackageJsonNoPly));
    };
    let Some(source_rel) = ply.get("source").and_then(Value::as_str) else {
        return Err(pkg_error(
            &path,
            &text,
            Error::PackageJsonSource,
        ));
    };
    let Some(out_rel) = ply.get("out").and_then(Value::as_str) else {
        return Err(pkg_error(
            &path,
            &text,
            Error::PackageJsonOut,
        ));
    };
    let Some(language) = ply.get("language").and_then(Value::as_u64) else {
        return Err(pkg_error(
            &path,
            &text,
            Error::PackageJsonLanguage,
        ));
    };
    if language == 0 || language > u64::from(u32::MAX) {
        return Err(pkg_error(
            &path,
            &text,
            Error::PackageJsonLanguage,
        ));
    }
    let language = language as u32;
    if language > LANGUAGE_VERSION {
        return Err(pkg_error(
            &path,
            &text,
            Error::LanguageTooNew { version: language, compiler: LANGUAGE_VERSION },
        ));
    }
    let source_rel = clean_rel(source_rel);
    let out_rel = clean_rel(out_rel);
    if source_rel.is_empty() || out_rel.is_empty() || source_rel == out_rel {
        return Err(pkg_error(
            &path,
            &text,
            Error::OutSameAsSource,
        ));
    }
    let foreign = match ply.get("foreign") {
        None => None,
        Some(value) => {
            let Some(rel) = value.as_str() else {
                return Err(pkg_error(
                    &path,
                    &text,
                    Error::PackageJsonForeign,
                ));
            };
            let rel = clean_rel(rel);
            if rel.is_empty()
                || rel == source_rel
                || rel == out_rel
                || nests(&rel, &source_rel)
                || nests(&rel, &out_rel)
            {
                return Err(pkg_error(
                    &path,
                    &text,
                    Error::ForeignSameDirectory,
                ));
            }
            Some(root.join(rel))
        }
    };
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    Ok(Package {
        source: root.join(&source_rel),
        name,
        foreign,
        out: root.join(&out_rel),
        out_rel,
        exports: export_map(&value),
        root: root.to_path_buf(),
    })
}

pub(crate) fn open_package(dir: &Path) -> Result<Package, BuildError> {
    let root = fs::canonicalize(dir).map_err(|error| os_error(dir, error))?;
    read_package(&root)
}

/// One directory lies inside the other. `src` and `src/js` overlap; `src` and `foreign` do not.
fn nests(path: &str, other: &str) -> bool {
    path.starts_with(&format!("{other}/")) || other.starts_with(&format!("{path}/"))
}

fn export_map(value: &Value) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(exports) = value.get("exports").and_then(Value::as_object) else {
        return map;
    };
    for (key, child) in exports {
        if let Some(target) = ply_target(child) {
            map.insert(key.clone(), target);
        }
    }
    map
}

fn ply_target(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    if let Some(text) = value.get("ply").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    let object = value.as_object()?;
    for child in object.values() {
        if let Some(text) = child.get("ply").and_then(Value::as_str) {
            return Some(text.to_string());
        }
    }
    None
}

fn classify(path: &str) -> Option<Spec> {
    if path.is_empty() || path.starts_with('.') || path.starts_with('/') {
        return None;
    }
    if let Some(rest) = path.strip_prefix('@') {
        let mut parts = rest.splitn(3, '/');
        let scope = parts.next()?;
        let name = parts.next()?;
        if scope.is_empty() || name.is_empty() {
            return None;
        }
        let subpath = parts.next().unwrap_or("").to_string();
        return Some(Spec::Package {
            name: format!("@{scope}/{name}"),
            subpath,
        });
    }
    let first = path.split('/').next()?;
    if first.chars().next()?.is_ascii_uppercase() {
        return Some(Spec::Local(path.to_string()));
    }
    let mut parts = path.splitn(2, '/');
    let name = parts.next()?.to_string();
    if name.is_empty() {
        return None;
    }
    Some(Spec::Package {
        name,
        subpath: parts.next().unwrap_or("").to_string(),
    })
}

fn export_key(subpath: &str) -> String {
    if subpath.is_empty() {
        ".".to_string()
    } else {
        format!("./{subpath}")
    }
}

struct ExportEntry {
    key: String,
    ply: String,
    js: String,
}

fn export_entries(units: &[Unit], out_rel: &str) -> Vec<ExportEntry> {
    let mut entries = Vec::new();
    for unit in units {
        if unit.kind != Kind::Local {
            continue;
        }
        let Some(module_path) = &unit.module_path else {
            continue;
        };
        entries.push(ExportEntry {
            key: export_key(module_path),
            ply: format!("./{out_rel}/{module_path}.d.ply"),
            js: format!("./{out_rel}/{module_path}.js"),
        });
    }
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

/// Replace the top-level `exports` value. Every other byte of `package.json` stays.
fn write_package_exports(root: &Path, entries: &[ExportEntry]) -> Result<(), BuildError> {
    let path = root.join("package.json");
    let text = fs::read_to_string(&path).map_err(|error| os_error(&path, error))?;
    let updated = match find_exports(&text) {
        Ok(ExportsSpot::Found { start, end }) => {
            let indent = line_indent(&text, start);
            let mut out = String::new();
            out.push_str(&text[..start]);
            out.push_str(&render_exports(entries, &indent));
            out.push_str(&text[end..]);
            out
        }
        Ok(ExportsSpot::Missing { close }) => insert_exports(&text, close, entries),
        Err(JsonError::NotObject) => {
            return Err(pkg_error(&path, &text, Error::PackageJsonNotObject))
        }
        Err(JsonError::Invalid) => {
            return Err(pkg_error(&path, &text, Error::PackageJsonInvalid))
        }
    };
    fs::write(&path, updated).map_err(|error| os_error(&path, error))
}

enum ExportsSpot {
    Found { start: usize, end: usize },
    Missing { close: usize },
}

#[derive(Debug)]
enum JsonError {
    NotObject,
    Invalid,
}

/// The last top-level `"exports"` wins, matching `JSON.parse`.
fn find_exports(text: &str) -> Result<ExportsSpot, JsonError> {
    let mut scan = JsonScan::new(text);
    scan.skip_ws();
    if scan.bump() != Some('{') {
        return Err(JsonError::NotObject);
    }
    let mut found = None;
    loop {
        scan.skip_ws();
        match scan.peek() {
            Some('}') => {
                return Ok(match found {
                    Some((start, end)) => ExportsSpot::Found { start, end },
                    None => ExportsSpot::Missing { close: scan.index },
                });
            }
            Some('"') => {
                let name = scan.parse_string().map_err(|_| JsonError::Invalid)?;
                scan.skip_ws();
                if scan.bump() != Some(':') {
                    return Err(JsonError::Invalid);
                }
                scan.skip_ws();
                let start = scan.index;
                scan.skip_value().map_err(|_| JsonError::Invalid)?;
                if name == "exports" {
                    found = Some((start, scan.index));
                }
                scan.skip_ws();
                if scan.peek() == Some(',') {
                    scan.bump();
                }
            }
            _ => return Err(JsonError::Invalid),
        }
    }
}

fn insert_exports(text: &str, close: usize, entries: &[ExportEntry]) -> String {
    let indent = object_indent(text);
    let value = render_exports(entries, &indent);
    let head = text[..close].trim_end();
    let mut out = String::new();
    out.push_str(head);
    if !head.ends_with('{') && !head.ends_with(',') {
        out.push(',');
    }
    out.push('\n');
    out.push_str(&indent);
    out.push_str("\"exports\": ");
    out.push_str(&value);
    out.push('\n');
    out.push('}');
    out.push_str(&text[close + '}'.len_utf8()..]);
    out
}

fn render_exports(entries: &[ExportEntry], indent: &str) -> String {
    if entries.is_empty() {
        return "{}".to_string();
    }
    let key_indent = format!("{indent}{indent}");
    let field_indent = format!("{key_indent}{indent}");
    let mut out = String::from("{\n");
    for (index, entry) in entries.iter().enumerate() {
        out.push_str(&key_indent);
        out.push_str(&json_string(&entry.key));
        out.push_str(": {\n");
        out.push_str(&field_indent);
        out.push_str("\"ply\": ");
        out.push_str(&json_string(&entry.ply));
        out.push_str(",\n");
        out.push_str(&field_indent);
        out.push_str("\"import\": ");
        out.push_str(&json_string(&entry.js));
        out.push('\n');
        out.push_str(&key_indent);
        out.push('}');
        if index + 1 != entries.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(indent);
    out.push('}');
    out
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn line_indent(text: &str, index: usize) -> String {
    let line_start = text[..index].rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    text[line_start..]
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect()
}

fn object_indent(text: &str) -> String {
    for line in text.lines().skip(1) {
        let indent: String = line
            .chars()
            .take_while(|ch| *ch == ' ' || *ch == '\t')
            .collect();
        if !indent.is_empty() {
            return indent;
        }
    }
    "  ".to_string()
}

struct JsonScan<'a> {
    text: &'a str,
    index: usize,
}

impl<'a> JsonScan<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, index: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.text[self.index..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.index += ch.len_utf8();
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn parse_string(&mut self) -> Result<String, ()> {
        if self.bump() != Some('"') {
            return Err(());
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('"') => return Ok(out),
                Some('\\') => {
                    let escaped = match self.bump() {
                        Some('"') => '"',
                        Some('\\') => '\\',
                        Some('/') => '/',
                        Some('b') => '\u{0008}',
                        Some('f') => '\u{000c}',
                        Some('n') => '\n',
                        Some('r') => '\r',
                        Some('t') => '\t',
                        Some('u') => self.unicode_escape()?,
                        _ => return Err(()),
                    };
                    out.push(escaped);
                }
                Some(ch) => out.push(ch),
                None => return Err(()),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ()> {
        let mut hex = String::new();
        for _ in 0..4 {
            let ch = self.bump().ok_or(())?;
            if !ch.is_ascii_hexdigit() {
                return Err(());
            }
            hex.push(ch);
        }
        u32::from_str_radix(&hex, 16).map_err(|_| ())
    }

    /// JSON encodes a scalar outside the BMP as a surrogate pair.
    fn unicode_escape(&mut self) -> Result<char, ()> {
        let code = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&code) {
            if self.bump() != Some('\\') || self.bump() != Some('u') {
                return Err(());
            }
            let low = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(());
            }
            let scalar = 0x10000 + (((code - 0xD800) << 10) | (low - 0xDC00));
            return char::from_u32(scalar).ok_or(());
        }
        if (0xDC00..=0xDFFF).contains(&code) {
            return Err(());
        }
        char::from_u32(code).ok_or(())
    }

    fn skip_value(&mut self) -> Result<(), ()> {
        match self.peek() {
            Some('"') => {
                self.parse_string()?;
                Ok(())
            }
            Some('{') => self.skip_container('{', '}'),
            Some('[') => self.skip_container('[', ']'),
            Some('t') => self.skip_literal("true"),
            Some('f') => self.skip_literal("false"),
            Some('n') => self.skip_literal("null"),
            Some('-' | '0'..='9') => self.skip_number(),
            _ => Err(()),
        }
    }

    fn skip_container(&mut self, open: char, close: char) -> Result<(), ()> {
        if self.bump() != Some(open) {
            return Err(());
        }
        let mut depth = 1;
        while depth > 0 {
            match self.peek() {
                Some('"') => {
                    self.parse_string()?;
                }
                Some(ch) if ch == open => {
                    self.bump();
                    depth += 1;
                }
                Some(ch) if ch == close => {
                    self.bump();
                    depth -= 1;
                }
                Some(_) => {
                    self.bump();
                }
                None => return Err(()),
            }
        }
        Ok(())
    }

    fn skip_literal(&mut self, literal: &str) -> Result<(), ()> {
        if self.text[self.index..].starts_with(literal) {
            self.index += literal.len();
            Ok(())
        } else {
            Err(())
        }
    }

    fn skip_number(&mut self) -> Result<(), ()> {
        if self.peek() == Some('-') {
            self.bump();
        }
        if !self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            return Err(());
        }
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if !self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                return Err(());
            }
            while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                self.bump();
            }
        }
        Ok(())
    }
}

fn target_file(root: &Path, target: &str) -> PathBuf {
    let relative = target.strip_prefix("./").unwrap_or(target);
    root.join(relative)
}

fn find_installed(start: &Path, name: &str) -> Option<PathBuf> {
    let mut dir = start.parent()?.to_path_buf();
    loop {
        let candidate = dir.join("node_modules").join(name);
        if candidate.join("package.json").is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn find_ply_root(start: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        start.to_path_buf()
    } else {
        start.parent()?.to_path_buf()
    };
    loop {
        let package_json = dir.join("package.json");
        if package_json.is_file() {
            if let Ok(text) = fs::read_to_string(&package_json) {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    if value.get("ply").and_then(|ply| ply.get("source")).is_some() {
                        return Some(dir);
                    }
                }
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn read_module(path: &Path) -> Option<Module> {
    let source = fs::read_to_string(path).ok()?;
    let text = path.to_string_lossy();
    let mode = if text.ends_with(".d.ply") {
        ParseMode::Interface
    } else {
        ParseMode::Source
    };
    parse(&source, mode).ok()
}

pub(crate) fn walk_ply(dir: &Path, out: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.starts_with(out) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            walk_ply(&path, out, files)?;
        } else if name.ends_with(".ply") && !name.ends_with(".d.ply") {
            files.push(path);
        }
    }
    Ok(())
}

pub(crate) fn module_path_of(source: &Path, file: &Path) -> String {
    let relative = file
        .strip_prefix(source)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/");
    relative
        .strip_suffix(".ply")
        .unwrap_or(&relative)
        .to_string()
}

fn clean_rel(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_string();
    }
    path.trim_matches('/').to_string()
}

fn cycle_message(units: &[Unit], stack: &[usize], again: usize) -> Error {
    let start = stack.iter().position(|id| *id == again).unwrap_or(0);
    let mut names: Vec<String> = stack[start..]
        .iter()
        .map(|id| unit_label(&units[*id]))
        .collect();
    names.push(unit_label(&units[again]));
    Error::ImportCycle(names.join(", "))
}

fn unit_label(unit: &Unit) -> String {
    unit.module_path
        .as_deref()
        .map(|path| path.replace('/', "."))
        .unwrap_or_else(|| file_label(&unit.path))
}

fn file_label(path: &Path) -> String {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("external");
    name.strip_suffix(".d.ply")
        .or_else(|| name.strip_suffix(".ply"))
        .unwrap_or(name)
        .to_string()
}

fn interface_text(module: &Module) -> String {
    let mut out = String::from("language 1\n");
    for import in &module.imports {
        out.push_str(&import.to_string());
        out.push('\n');
    }
    for item in &module.items {
        match item {
            Item::Data(data) => {
                out.push('\n');
                out.push_str(&data.to_string());
                out.push('\n');
            }
            Item::Synonym(synonym) => {
                out.push('\n');
                out.push_str(&synonym.to_string());
                out.push('\n');
            }
            Item::Def(def) => {
                out.push('\n');
                crate::ast::push_doc(&mut out, &def.doc);
                out.push_str(": ");
                if def.vis.is_foreign() {
                    out.push_str("foreign ");
                }
                if def.vis.is_private() {
                    out.push_str("private ");
                }
                out.push_str(&def.name);
                out.push_str(" ( ");
                out.push_str(&scheme_string(&def.scheme));
                out.push_str(" )\n");
            }
            Item::Class(class) => {
                out.push('\n');
                out.push_str(&class.to_string());
                out.push('\n');
            }
            Item::Instance(instance) => {
                out.push('\n');
                out.push_str(&instance.to_string());
                out.push('\n');
            }
        }
    }
    out
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
            Item::Class(_) | Item::Instance(_) | Item::Synonym(_) => {}
        }
    }
    names
}

fn value_names(module: &Module, import: &Import) -> Vec<String> {
    let public = public_values(module);
    if import.all {
        return public;
    }
    import
        .names
        .iter()
        .filter(|name| public.iter().any(|public| public == *name))
        .cloned()
        .collect()
}

fn public_values(module: &Module) -> Vec<String> {
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

fn foreign_names(module: &Module) -> Vec<(String, bool)> {
    module
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Def(def) if def.vis.is_foreign() => {
                Some((def.name.clone(), def.vis.is_private()))
            }
            _ => None,
        })
        .collect()
}

fn push_group(groups: &mut Vec<(String, Vec<String>)>, spec: String, names: Vec<String>) {
    if let Some((_, existing)) = groups.iter_mut().find(|(have, _)| have == &spec) {
        for name in names {
            if !existing.contains(&name) {
                existing.push(name);
            }
        }
    } else {
        groups.push((spec, names));
    }
}

fn import_line(spec: &str, names: &[String]) -> String {
    let bindings = names
        .iter()
        .map(|name| import_binding(name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("import {{ {bindings} }} from \"{spec}\";")
}

fn import_binding(name: &str) -> String {
    let local = js_ident(name);
    if local == name {
        name.to_string()
    } else {
        format!("{name} as {local}")
    }
}

fn export_binding(name: &str) -> String {
    let local = js_ident(name);
    if local == name {
        name.to_string()
    } else {
        format!("{local} as {name}")
    }
}

fn combine(header: &str, body: &str) -> String {
    if header.is_empty() {
        body.to_string()
    } else if body.is_empty() {
        header.to_string()
    } else {
        format!("{header}\n{body}")
    }
}

fn relative_specifier(from_file: &Path, to_file: &Path) -> String {
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

pub(crate) fn located(path: &Path, source: &str, diagnostic: Diagnostic) -> BuildError {
    BuildError {
        message: diagnostic.message.clone(),
        code: diagnostic.code,
        rendered: diagnostic.render(&path.display().to_string(), source),
    }
}

fn pkg_error(path: &Path, source: &str, error: Error) -> BuildError {
    located(path, source, Diagnostic::new(Span::new(0, 0), error))
}

fn reject_bad_entry(module: &Module) -> Result<(), Diagnostic> {
    match entry_def(module) {
        Some(def) if !accepts_entry(def) => Err(Diagnostic::new(def.name_span, Error::EntryWord)),
        _ => Ok(()),
    }
}

fn apply_entry(
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

fn entry_def(module: &Module) -> Option<&Def> {
    module.items.iter().find_map(|item| match item {
        Item::Def(def) if def.name == ENTRY_WORD => Some(def),
        _ => None,
    })
}

/// `( -- Unit )`, including a private word. No constraints, no effects, no inputs.
fn accepts_entry(def: &Def) -> bool {
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

fn with_entry_guard(js: String) -> String {
    let (import_line, call) = entry_pieces(&js);
    let mut js = format!("{import_line}{js}");
    if !js.ends_with('\n') {
        js.push('\n');
    }
    js.push_str(&call);
    js
}

fn entry_pieces(js: &str) -> (String, String) {
    let mut used = Vec::new();
    let realpath = take_js_name(js, &mut used, "plyRealpath");
    let path_to_file = take_js_name(js, &mut used, "plyPathToFileURL");
    let flag = take_js_name(js, &mut used, "plyIsEntry");
    let call = js_ident(ENTRY_WORD);
    let import_line = format!(
        "import {{ realpathSync as {realpath} }} from \"node:fs\";\nimport {{ pathToFileURL as {path_to_file} }} from \"node:url\";\n"
    );
    let guard = format!(
        "let {flag} = import.meta.main === true;\nif (import.meta.main === undefined && process.argv[1]) {{\n  try {{\n    {flag} = import.meta.url === {path_to_file}({realpath}(process.argv[1])).href;\n  }} catch {{\n    {flag} = false;\n  }}\n}}\nif ({flag}) {call}();\n"
    );
    (import_line, guard)
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

pub(crate) fn io_error(path: &Path, error: Error) -> BuildError {
    let detail = error.message();
    let code = error.code();
    BuildError {
        message: format!("{}: {detail}", path.display()),
        code,
        rendered: format!(
            "{}: error: {code}: {detail}\n{}",
            path.display(),
            error_url(code)
        ),
    }
}

pub(crate) fn os_error(path: &Path, error: std::io::Error) -> BuildError {
    io_error(path, Error::Os(error.to_string()))
}

fn exported_arity(source: &str, name: &str) -> JsExport {
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut bare = false;
    let mut arity = None;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index] == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        if matches!(bytes[index], b'\'' | b'"' | b'`') {
            index = skip_js_string(bytes, index);
            continue;
        }
        if is_word_at(bytes, index, b"export") {
            let mut scan = Scan {
                bytes,
                index: index + "export".len(),
            };
            match scan.one_export(name) {
                JsExport::Arity(count) => arity = Some(count),
                JsExport::Unknown => bare = true,
                JsExport::Missing => {}
            }
            index += "export".len();
            continue;
        }
        if is_ident_start(bytes[index]) {
            while index < bytes.len() && is_ident_cont(bytes[index]) {
                index += 1;
            }
            continue;
        }
        index += 1;
    }
    if let Some(count) = arity {
        JsExport::Arity(count)
    } else if bare {
        JsExport::Unknown
    } else {
        JsExport::Missing
    }
}

struct Scan<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl Scan<'_> {
    fn skip(&mut self) {
        loop {
            while self.index < self.bytes.len() && self.bytes[self.index].is_ascii_whitespace() {
                self.index += 1;
            }
            if self.index + 1 < self.bytes.len()
                && self.bytes[self.index] == b'/'
                && self.bytes[self.index + 1] == b'/'
            {
                while self.index < self.bytes.len() && self.bytes[self.index] != b'\n' {
                    self.index += 1;
                }
                continue;
            }
            if self.index + 1 < self.bytes.len()
                && self.bytes[self.index] == b'/'
                && self.bytes[self.index + 1] == b'*'
            {
                self.index += 2;
                while self.index + 1 < self.bytes.len()
                    && !(self.bytes[self.index] == b'*' && self.bytes[self.index + 1] == b'/')
                {
                    self.index += 1;
                }
                self.index = (self.index + 2).min(self.bytes.len());
                continue;
            }
            break;
        }
    }

    fn eat(&mut self, token: &[u8]) -> bool {
        self.skip();
        if self.bytes[self.index..].starts_with(token) {
            self.index += token.len();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &[u8]) -> bool {
        self.skip();
        if is_word_at(self.bytes, self.index, word) {
            self.index += word.len();
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Option<String> {
        self.skip();
        if self.index >= self.bytes.len() || !is_ident_start(self.bytes[self.index]) {
            return None;
        }
        let start = self.index;
        self.index += 1;
        while self.index < self.bytes.len() && is_ident_cont(self.bytes[self.index]) {
            self.index += 1;
        }
        Some(String::from_utf8_lossy(&self.bytes[start..self.index]).into_owned())
    }

    fn looking_at_ident(&mut self) -> bool {
        self.skip();
        self.index < self.bytes.len() && is_ident_start(self.bytes[self.index])
    }

    fn parens(&mut self) -> Option<String> {
        self.skip();
        if self.index >= self.bytes.len() || self.bytes[self.index] != b'(' {
            return None;
        }
        self.index += 1;
        let start = self.index;
        let mut depth = 1i32;
        while self.index < self.bytes.len() && depth > 0 {
            match self.bytes[self.index] {
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    self.index += 1;
                }
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let inner =
                            String::from_utf8_lossy(&self.bytes[start..self.index]).into_owned();
                        self.index += 1;
                        return Some(inner);
                    }
                    self.index += 1;
                }
                b'\'' | b'"' | b'`' => self.index = skip_js_string(self.bytes, self.index),
                _ => self.index += 1,
            }
        }
        None
    }

    fn one_export(&mut self, name: &str) -> JsExport {
        let _ = self.eat_word(b"async");
        if self.eat_word(b"function") {
            let Some(found) = self.ident() else {
                return JsExport::Missing;
            };
            if found != name {
                return JsExport::Missing;
            }
            let Some(params) = self.parens() else {
                return JsExport::Missing;
            };
            return JsExport::Arity(param_count(&params));
        }
        if self.eat_word(b"const") || self.eat_word(b"let") || self.eat_word(b"var") {
            let Some(found) = self.ident() else {
                return JsExport::Missing;
            };
            if found != name {
                return JsExport::Missing;
            }
            if !self.eat(b"=") {
                return JsExport::Missing;
            }
            if self.eat_word(b"function") {
                if self.looking_at_ident() {
                    let _ = self.ident();
                }
                let Some(params) = self.parens() else {
                    return JsExport::Missing;
                };
                return JsExport::Arity(param_count(&params));
            }
            self.skip();
            if self.index < self.bytes.len() && self.bytes[self.index] == b'(' {
                let Some(params) = self.parens() else {
                    return JsExport::Missing;
                };
                if self.eat(b"=>") {
                    return JsExport::Arity(param_count(&params));
                }
            }
            return JsExport::Missing;
        }
        if self.eat(b"{") {
            if export_list_has(self, name) {
                return JsExport::Unknown;
            }
            return JsExport::Missing;
        }
        JsExport::Unknown
    }
}

fn export_list_has(scan: &mut Scan<'_>, name: &str) -> bool {
    loop {
        scan.skip();
        if scan.eat(b"}") {
            return false;
        }
        let Some(first) = scan.ident() else {
            return false;
        };
        let mut exported = first;
        if scan.eat_word(b"as") {
            if let Some(alias) = scan.ident() {
                exported = alias;
            }
        }
        if exported == name {
            return true;
        }
        scan.skip();
        if scan.eat(b",") {
            continue;
        }
        let _ = scan.eat(b"}");
        return false;
    }
}

fn param_count(params: &str) -> usize {
    let mut depth = 0i32;
    let mut count = 0;
    let mut slot = false;
    for ch in params.chars() {
        match ch {
            '(' | '[' | '{' => {
                depth += 1;
                slot = true;
            }
            ')' | ']' | '}' if depth > 0 => depth -= 1,
            ',' if depth == 0 => {
                if slot {
                    count += 1;
                }
                slot = false;
            }
            ch if ch.is_whitespace() => {}
            _ => slot = true,
        }
    }
    if slot {
        count += 1;
    }
    count
}

fn is_word_at(bytes: &[u8], index: usize, word: &[u8]) -> bool {
    if index > 0 && is_ident_cont(bytes[index - 1]) {
        return false;
    }
    if !bytes[index..].starts_with(word) {
        return false;
    }
    let end = index + word.len();
    end >= bytes.len() || !is_ident_cont(bytes[end])
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$'
}

fn is_ident_cont(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

fn skip_js_string(bytes: &[u8], index: usize) -> usize {
    if index >= bytes.len() {
        return index;
    }
    let quote = bytes[index];
    let mut index = index + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        if bytes[index] == quote {
            return index + 1;
        }
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{build, build_exports, check_package, exported_arity, relative_specifier, JsExport};

    struct Temp(PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("plylang-{label}-{}-{nanos}", std::process::id()));
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

    #[test]
    fn check_buffer_accepts_an_imported_constructor() {
        let temp = Temp::new("buffer");
        temp.write(
            "package.json",
            &package_json(
                r#""./Data/Maybe": { "ply": "./dist/Data/Maybe.d.ply", "import": "./dist/Data/Maybe.js" }"#,
            ),
        );
        temp.write("src/Data/Maybe.ply", "; Maybe a = Nothing | Just a\n");
        let source = "~ \"Data/Maybe\" (Maybe, Just)\n\n: wrap ( a. a -- (Maybe a) )\n  Just\n";
        temp.write("src/App.ply", source);
        let path = temp.0.join("src/App.ply");
        super::check_buffer(&path, source).unwrap_or_else(|error| {
            panic!("{}", error.render("App.ply", source))
        });
        let outside = super::check_buffer(
            std::path::Path::new("/definitely/not/a/ply/package/App.ply"),
            source,
        )
        .unwrap_err();
        assert_eq!(outside.message, "undefined type Maybe");
    }

    #[test]
    fn check_buffer_reports_a_dependency_parse_error() {
        let temp = Temp::new("dep-parse");
        temp.write(
            "package.json",
            &package_json(
                r#"
    "./Data/Maybe": { "ply": "./dist/Data/Maybe.d.ply", "import": "./dist/Data/Maybe.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        temp.write("src/Data/Maybe.ply", ": f ( Int -- Int )\n");
        let source = "~ \"Data/Maybe\" (f)\n\n: use ( Int -- Int )\n  f\n";
        temp.write("src/App.ply", source);
        let error = super::check_buffer(&temp.0.join("src/App.ply"), source).unwrap_err();
        assert_eq!(error.code, "PL0030");
        let maybe = temp.0.join("src/Data/Maybe.ply");
        assert!(
            error.message.contains(&maybe.display().to_string()),
            "{}",
            error.message
        );
        assert!(error.message.contains("missing body"), "{}", error.message);
    }

    #[test]
    fn check_buffer_reports_a_dependency_read_error() {
        use std::os::unix::fs::PermissionsExt;

        let temp = Temp::new("dep-read");
        temp.write(
            "package.json",
            &package_json(
                r#""./Data/Maybe": { "ply": "./dist/Data/Maybe.d.ply", "import": "./dist/Data/Maybe.js" }"#,
            ),
        );
        let maybe = temp.0.join("src/Data/Maybe.ply");
        temp.write("src/Data/Maybe.ply", "; Maybe a = Nothing | Just a\n");
        let mut perms = fs::metadata(&maybe).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&maybe, perms).unwrap();
        let source = "~ \"Data/Maybe\" (Maybe)\n\n: use ( a. -- (Maybe a) )\n  Nothing\n";
        let error = super::check_buffer(&temp.0.join("src/App.ply"), source).unwrap_err();
        let mut perms = fs::metadata(&maybe).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&maybe, perms).unwrap();
        assert_eq!(error.code, "PL0112", "{}", error.message);
        assert!(
            error.message.contains(&maybe.display().to_string()),
            "{}",
            error.message
        );
    }

    #[test]
    fn check_writes_nothing_and_js_prints_one_module() {
        let temp = Temp::new("check-js");
        temp.write(
            "package.json",
            &package_json(
                r#""./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }"#,
            ),
        );
        temp.write("src/App.ply", ": id ( Int -- Int )\n  dup drop\n");
        temp.write("src/Broken.ply", ": bad ( Int -- Int )\n  \"no\"\n");
        let app = temp.0.join("src/App.ply");
        let js = super::javascript_file(&app).unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!temp.0.join("dist").exists());
        super::check_package(&temp.0).unwrap_err();
        assert!(!temp.0.join("dist").exists());
        fs::remove_file(temp.0.join("src/Broken.ply")).unwrap();
        super::check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!temp.0.join("dist").exists());
        super::build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let written = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert_eq!(js, written);

        let lone = Temp::new("snippet");
        lone.write("Id.ply", ": id ( Int -- Int )\n  dup drop\n");
        let snippet = super::javascript_file(&lone.0.join("Id.ply"))
            .unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(snippet.contains("id"), "{snippet}");
        assert!(!snippet.contains("import "), "{snippet}");
    }

    #[test]
    fn build_exports_rewrites_the_exports_field() {
        let temp = Temp::new("exports");
        let original = "{\n  \"name\": \"@algosail/b\",\n  \"description\": \"say \\\"hi\\\"\",\n  \"type\": \"module\",\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 },\n  \"exports\": {\n    \"./Old\": { \"ply\": \"./dist/Old.d.ply\", \"import\": \"./dist/Old.js\" }\n  }\n}\n";
        temp.write("package.json", original);
        temp.write(
            "src/App.ply",
            "~ \"Data/Num\" (add)\n\n: twice ( Int -- Int )\n  dup add\n",
        );
        temp.write("src/Data/Num.ply", ": add ( Int Int -- Int )\n  drop\n");
        temp.write("src/Hidden.ply", ": private step ( Int -- Int )\n  dup drop\n");
        let error = build(&temp.0).unwrap_err();
        assert_eq!(error.message, "module Data.Num is not exported");
        assert_eq!(fs::read_to_string(temp.0.join("package.json")).unwrap(), original);

        build_exports(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let expected = "{\n  \"name\": \"@algosail/b\",\n  \"description\": \"say \\\"hi\\\"\",\n  \"type\": \"module\",\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 },\n  \"exports\": {\n    \"./App\": {\n      \"ply\": \"./dist/App.d.ply\",\n      \"import\": \"./dist/App.js\"\n    },\n    \"./Data/Num\": {\n      \"ply\": \"./dist/Data/Num.d.ply\",\n      \"import\": \"./dist/Data/Num.js\"\n    },\n    \"./Hidden\": {\n      \"ply\": \"./dist/Hidden.d.ply\",\n      \"import\": \"./dist/Hidden.js\"\n    }\n  }\n}\n";
        let written = fs::read_to_string(temp.0.join("package.json")).unwrap();
        assert_eq!(written, expected);
        assert!(temp.0.join("dist/App.js").is_file());
        assert!(temp.0.join("dist/Data/Num.d.ply").is_file());
        assert!(temp.0.join("dist/Hidden.js").is_file());
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert_eq!(fs::read_to_string(temp.0.join("package.json")).unwrap(), expected);

        let fresh = Temp::new("exports-fresh");
        let bare = "{\n  \"name\": \"@algosail/shelf\",\n  \"ply\": { \"source\": \"src\", \"out\": \"out\", \"language\": 1 }\n}\n";
        fresh.write("package.json", bare);
        fresh.write("src/Age.ply", ": older ( Int -- Int )\n  dup drop\n");
        build_exports(&fresh.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert_eq!(
            fs::read_to_string(fresh.0.join("package.json")).unwrap(),
            "{\n  \"name\": \"@algosail/shelf\",\n  \"ply\": { \"source\": \"src\", \"out\": \"out\", \"language\": 1 },\n  \"exports\": {\n    \"./Age\": {\n      \"ply\": \"./out/Age.d.ply\",\n      \"import\": \"./out/Age.js\"\n    }\n  }\n}\n"
        );
        assert!(fresh.0.join("out/Age.js").is_file());

        let empty = Temp::new("exports-empty");
        empty.write(
            "package.json",
            "{\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 }\n}\n",
        );
        fs::create_dir(empty.0.join("src")).unwrap();
        build_exports(&empty.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert_eq!(
            fs::read_to_string(empty.0.join("package.json")).unwrap(),
            "{\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 },\n  \"exports\": {}\n}\n"
        );

        let broken = Temp::new("exports-broken");
        let untouched = "{\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 }\n}\n";
        broken.write("package.json", untouched);
        broken.write("src/Bad.ply", ": bad ( Int -- Int )\n  \"no\"\n");
        build_exports(&broken.0).unwrap_err();
        assert_eq!(fs::read_to_string(broken.0.join("package.json")).unwrap(), untouched);
        assert!(!broken.0.join("dist").exists());
    }

    fn package_json(exports: &str) -> String {
        format!(
            r#"{{
  "name": "@algosail/b",
  "type": "module",
  "ply": {{ "source": "src", "foreign": "foreign", "out": "dist", "language": 1 }},
  "exports": {{ {exports} }}
}}"#,
        )
    }

    #[test]
    fn counts_foreign_js_parameters() {
        assert!(matches!(
            exported_arity("export function add(a, b) { return a + b; }", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export const add = (a, b) => a + b;", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export const add = function (a) { return a; }", "add"),
            JsExport::Arity(1)
        ));
        assert!(matches!(
            exported_arity("export const add = function add(a, b, c) { return a; }", "add"),
            JsExport::Arity(3)
        ));
        assert!(matches!(
            exported_arity("export { add };", "add"),
            JsExport::Unknown
        ));
        assert!(matches!(
            exported_arity("export function sub(a) {}", "add"),
            JsExport::Missing
        ));
        assert!(matches!(
            exported_arity("export { sub }", "add"),
            JsExport::Missing
        ));
        assert!(matches!(
            exported_arity("export class add {}", "add"),
            JsExport::Unknown
        ));
        assert!(matches!(
            exported_arity("export async function add(a, b) { return a; }", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export async\nfunction add(a, b) { return a; }", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export /* c */ function add(a) { return a; }", "add"),
            JsExport::Arity(1)
        ));
        assert!(matches!(
            exported_arity("export // c\nfunction add(a) { return a; }", "add"),
            JsExport::Arity(1)
        ));
    }

    #[test]
    fn json_scan_keeps_surrogate_pairs_and_the_last_exports() {
        let pair = r#"{ "name": "\ud83d\ude00", "exports": { "a": 1 }, "exports": { "b": 2 } }"#;
        let super::ExportsSpot::Found { start, end } = super::find_exports(pair).unwrap() else {
            panic!("expected the exports value");
        };
        assert_eq!(&pair[start..end], r#"{ "b": 2 }"#);
        assert!(matches!(super::find_exports("[]"), Err(super::JsonError::NotObject)));
        assert!(matches!(
            super::find_exports(r#"{ "name": "\ud800" }"#),
            Err(super::JsonError::Invalid)
        ));
        assert!(matches!(super::find_exports("{"), Err(super::JsonError::Invalid)));
    }

    #[test]
    fn build_exports_keeps_a_surrogate_and_replaces_the_last_exports() {
        let emoji = Temp::new("exports-emoji");
        emoji.write(
            "package.json",
            "{\n  \"name\": \"\\ud83d\\ude00\",\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 }\n}\n",
        );
        emoji.write("src/App.ply", ": id ( Int -- Int )\n  dup drop\n");
        build_exports(&emoji.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let written = fs::read_to_string(emoji.0.join("package.json")).unwrap();
        assert!(written.contains("\\ud83d\\ude00"), "{written}");

        let last = Temp::new("exports-last");
        last.write(
            "package.json",
            "{\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 },\n  \"exports\": { \"./Old\": { \"ply\": \"./dist/Old.d.ply\", \"import\": \"./dist/Old.js\" } },\n  \"name\": \"shelf\",\n  \"exports\": { \"./Older\": { \"ply\": \"./dist/Older.d.ply\", \"import\": \"./dist/Older.js\" } }\n}\n",
        );
        last.write("src/App.ply", ": id ( Int -- Int )\n  dup drop\n");
        build_exports(&last.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let written = fs::read_to_string(last.0.join("package.json")).unwrap();
        assert!(written.contains("./Old"), "{written}");
        assert!(!written.contains("./Older"), "{written}");
        let value: serde_json::Value = serde_json::from_str(&written).unwrap();
        let keys: Vec<_> = value
            .get("exports")
            .and_then(|exports| exports.as_object())
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, vec!["./App".to_string()], "{written}");
    }

    #[test]
    fn build_rejects_one_data_name_from_two_files() {
        let temp = Temp::new("two-box");
        temp.write(
            "package.json",
            &package_json(
                r#"
    "./Data/Num": { "ply": "./dist/Data/Num.d.ply", "import": "./dist/Data/Num.js" },
    "./Data/Other": { "ply": "./dist/Data/Other.d.ply", "import": "./dist/Data/Other.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        temp.write("src/Data/Num.ply", "; Box a = Mk a a\n");
        temp.write(
            "src/Data/Other.ply",
            "; Box a = Box a\n\n: wrap ( a. a -- (Box a) )\n  Box\n",
        );
        temp.write(
            "src/App.ply",
            "~ \"Data/Num\" (Mk)\n~ \"Data/Other\" (wrap)\n\n: id ( Int -- Int )\n  dup drop\n",
        );
        let error = build(&temp.0).unwrap_err();
        assert_eq!(error.message, "duplicate definition Box");

        let again = Temp::new("same-box");
        again.write(
            "package.json",
            &package_json(
                r#"
    "./Data/Num": { "ply": "./dist/Data/Num.d.ply", "import": "./dist/Data/Num.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        again.write("src/Data/Num.ply", "; Box a = Mk a a\n");
        again.write(
            "src/App.ply",
            "~ \"Data/Num\" (Box)\n~ \"Data/Num\" (Mk)\n\n: pair ( a. a a -- (Box a) )\n  Mk\n",
        );
        build(&again.0).unwrap_or_else(|error| panic!("{}", error.render()));
    }

    #[test]
    fn a_missing_foreign_file_is_an_io_error() {
        let temp = Temp::new("foreign-missing");
        temp.write(
            "package.json",
            &package_json(
                r#""./Data/Num": { "ply": "./dist/Data/Num.d.ply", "import": "./dist/Data/Num.js" }"#,
            ),
        );
        temp.write("src/Data/Num.ply", ": foreign add ( Int Int -- Int )\n");
        let error = build(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0112", "{}", error.message);
        assert!(error.message.contains("foreign/Data/Num.js"), "{}", error.message);
    }

    #[test]
    fn relative_js_specifiers() {
        assert_eq!(
            relative_specifier(std::path::Path::new("dist/App.js"), std::path::Path::new("dist/Data/Num.js")),
            "./Data/Num.js"
        );
        assert_eq!(
            relative_specifier(
                std::path::Path::new("dist/Data/Maybe.js"),
                std::path::Path::new("dist/Data/Either.js")
            ),
            "./Either.js"
        );
        assert_eq!(
            relative_specifier(
                std::path::Path::new("dist/Data/Num.js"),
                std::path::Path::new("src/Data/Num.js")
            ),
            "../../src/Data/Num.js"
        );
    }

    #[test]
    fn package_b_calls_package_a_under_node() {
        let temp = Temp::new("ab");
        temp.write(
            "node_modules/@algosail/a/package.json",
            r#"{
  "name": "@algosail/a",
  "type": "module",
  "exports": {
    "./Data/Num": {
      "ply": "./dist/Data/Num.d.ply",
      "import": "./dist/Data/Num.js"
    }
  }
}"#,
        );
        temp.write(
            "node_modules/@algosail/a/dist/Data/Num.d.ply",
            "language 1\n: foreign add ( (Int) (Int) -- Int )\n",
        );
        temp.write(
            "node_modules/@algosail/a/dist/Data/Num.js",
            "export function add(a, b) { return a + b; }\n",
        );
        temp.write(
            "package.json",
            &package_json(
                r#""./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }"#,
            ),
        );
        temp.write(
            "src/App.ply",
            "~ \"@algosail/a/Data/Num\" (add)\n\n: twice ( Int -- Int )\n  dup add\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(
            js.contains("import { add } from \"@algosail/a/Data/Num\";"),
            "{js}"
        );
        let iface = fs::read_to_string(temp.0.join("dist/App.d.ply")).unwrap();
        assert!(iface.starts_with("language 1\n"), "{iface}");
        assert!(!iface.contains("module "), "{iface}");
        assert!(iface.contains("~ \"@algosail/a/Data/Num\" (add)"), "{iface}");
        assert!(iface.contains(": twice ( Int -- Int )"), "{iface}");
        assert!(!iface.contains("dup add"), "{iface}");
        node(
            &temp.0,
            "import { twice } from \"./dist/App.js\";\nif (twice(21n) !== 42n) throw new Error(String(twice(21n)));\n",
        );
    }

    #[test]
    fn a_dependency_interface_installs_superclasses_first() {
        let temp = Temp::new("super");
        temp.write(
            "node_modules/@algosail/cls/package.json",
            r#"{
  "name": "@algosail/cls",
  "type": "module",
  "ply": { "source": "src", "foreign": "foreign", "out": "dist", "language": 1 },
  "exports": {
    "./Mappable": { "ply": "./dist/Mappable.d.ply", "import": "./dist/Mappable.js" },
    "./Applicable": { "ply": "./dist/Applicable.d.ply", "import": "./dist/Applicable.js" },
    "./Wrappable": { "ply": "./dist/Wrappable.d.ply", "import": "./dist/Wrappable.js" }
  }
}"#,
        );
        temp.write(
            "node_modules/@algosail/cls/dist/Mappable.d.ply",
            "language 1\n\n& Mappable f\n  map ( a b. (f a) (a -- b) -- (f b) )\n",
        );
        temp.write(
            "node_modules/@algosail/cls/dist/Applicable.d.ply",
            "language 1\n~ \"Mappable\" (Mappable)\n\n& Mappable f => Applicable f\n  apply ( a b. (f a) (f (a -- b)) -- (f b) )\n",
        );
        temp.write(
            "node_modules/@algosail/cls/dist/Wrappable.d.ply",
            "language 1\n~ \"Applicable\" (Applicable)\n\n& Applicable f => Wrappable f\n  wrap ( a -- (f a) )\n",
        );
        temp.write(
            "package.json",
            &package_json(
                r#""./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }"#,
            ),
        );
        temp.write(
            "src/App.ply",
            "~ \"@algosail/cls/Wrappable\" (Wrappable, wrap)\n\n: use ( f a. Wrappable f => a -- (f a) )\n  wrap\n",
        );
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
    }

    #[test]
    fn local_import_is_checked_and_called() {
        let temp = Temp::new("local");
        temp.write(
            "package.json",
            &package_json(
                r#"
    "./Data/Num": { "ply": "./dist/Data/Num.d.ply", "import": "./dist/Data/Num.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        temp.write(
            "src/Data/Num.ply",
            ".. Add two integers.\n: add ( (Int) (Int) -- Int )\n  iadd\n\n: foreign iadd ( Int Int -- Int )\n\n.. secret\n: private hidden ( Int -- Int )\n  dup drop\n",
        );
        temp.write(
            "foreign/Data/Num.js",
            "export function iadd(a, b) { return a + b; }\n",
        );
        temp.write(
            "src/App.ply",
            "~ \"Data/Num\" (add)\n\n: twice ( Int -- Int )\n  dup add\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(js.contains("import { add } from \"./Data/Num.js\";"), "{js}");
        let num = fs::read_to_string(temp.0.join("dist/Data/Num.js")).unwrap();
        assert!(
            num.contains("from \"../../foreign/Data/Num.js\""),
            "{num}"
        );
        let iface = fs::read_to_string(temp.0.join("dist/Data/Num.d.ply")).unwrap();
        assert!(iface.contains(": private hidden ( Int -- Int )"), "{iface}");
        assert!(iface.contains(".. secret\n"), "{iface}");
        assert!(!num.contains("export function hidden"), "{num}");
        assert!(num.contains("function hidden"), "{num}");
        assert!(iface.contains(".. Add two integers.\n"), "{iface}");
        assert!(iface.contains(": add ( Int Int -- Int )"), "{iface}");
        let module = crate::parse(&iface, crate::ParseMode::Interface)
            .unwrap_or_else(|error| panic!("{}", error.render("Data/Num.d.ply", &iface)));
        let crate::ast::Item::Def(def) = &module.items[0] else {
            panic!("expected a definition, got {iface}");
        };
        assert!(def.body.is_empty());
        assert_eq!(def.doc, "Add two integers.");
        assert!(
            matches!(
                def.scheme.effect.input.items.as_slice(),
                [crate::ast::Type::Ctor(left), crate::ast::Type::Ctor(right)]
                    if left == "Int" && right == "Int"
            ),
            "{iface}"
        );
        node(
            &temp.0,
            "import { twice } from \"./dist/App.js\";\nif (twice(21n) !== 42n) throw new Error(String(twice(21n)));\n",
        );
        let source = fs::read_to_string(temp.0.join("src/App.ply")).unwrap();
        let line = source.lines().nth(3).unwrap();
        let found = crate::hover::hover(
            &source,
            &temp.0.join("src/App.ply"),
            3,
            line.find("add").unwrap() as u32,
        )
        .unwrap();
        assert_eq!(
            found.contents,
            "```ply\n: add ( Int Int -- Int )\n```\n\nAdd two integers.\n\n```ply\nInt Int -- Int\n```"
        );
    }

    #[test]
    fn build_reports_cycles_exports_names_and_arity() {
        let cycle = Temp::new("cycle");
        cycle.write(
            "package.json",
            &package_json(
                r#"
    "./Data/A": { "ply": "./dist/Data/A.d.ply", "import": "./dist/Data/A.js" },
    "./Data/B": { "ply": "./dist/Data/B.d.ply", "import": "./dist/Data/B.js" }
"#,
            ),
        );
        cycle.write(
            "src/Data/A.ply",
            "~ \"Data/B\" (b)\n\n: a ( Int -- Int )\n  b\n",
        );
        cycle.write(
            "src/Data/B.ply",
            "~ \"Data/A\" (a)\n\n: b ( Int -- Int )\n  a\n",
        );
        let error = build(&cycle.0).unwrap_err();
        assert_eq!(error.message, "import cycle: Data.A, Data.B, Data.A");

        let missing = Temp::new("export");
        missing.write(
            "package.json",
            &package_json(
                r#""./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }"#,
            ),
        );
        missing.write(
            "src/Data/Num.ply",
            ": add ( (Int) (Int) -- Int )\n  drop\n",
        );
        missing.write(
            "src/App.ply",
            "~ \"Data/Num\" (add)\n\n: twice ( Int -- Int )\n  dup add\n",
        );
        let error = build(&missing.0).unwrap_err();
        assert_eq!(error.message, "module Data.Num is not exported");

        let renamed = Temp::new("name");
        renamed.write("package.json", &package_json(""));
        renamed.write("src/data/Num.ply", ": add ( Int -- Int )\n  dup drop\n");
        let error = build(&renamed.0).unwrap_err();
        assert_eq!(
            error.message,
            "module segment 'data' must start with an uppercase letter"
        );

        let arity = Temp::new("arity");
        arity.write(
            "package.json",
            &package_json(
                r#""./Data/Num": { "ply": "./dist/Data/Num.d.ply", "import": "./dist/Data/Num.js" }"#,
            ),
        );
        arity.write(
            "src/Data/Num.ply",
            ": foreign add ( (Int) (Int) -- Int )\n",
        );
        arity.write(
            "foreign/Data/Num.js",
            "export function add(a) { return a; }\n",
        );
        let error = build(&arity.0).unwrap_err();
        assert_eq!(error.message, "foreign word add expects 2 parameters, found 1");

        let omitted = Temp::new("no-foreign-dir");
        omitted.write(
            "package.json",
            r#"{ "ply": { "source": "src", "out": "dist", "language": 1 }, "exports": { "./Data/Num": { "ply": "./dist/Data/Num.d.ply" } } }"#,
        );
        omitted.write("src/Data/Num.ply", ": foreign add ( Int Int -- Int )\n");
        let error = build(&omitted.0).unwrap_err();
        assert_eq!(error.message, "package.json has no \"ply.foreign\" field");

        let same = Temp::new("foreign-is-source");
        same.write(
            "package.json",
            r#"{ "ply": { "source": "src", "foreign": "src", "out": "dist", "language": 1 } }"#,
        );
        let error = build(&same.0).unwrap_err();
        assert_eq!(
            error.message,
            "ply.foreign must be a different directory from ply.source and ply.out"
        );

        let newer = Temp::new("lang");
        newer.write(
            "package.json",
            r#"{ "ply": { "source": "src", "out": "dist", "language": 2 } }"#,
        );
        let error = build(&newer.0).unwrap_err();
        assert_eq!(
            error.message,
            "language version 2 is newer than this compiler (1)"
        );
    }

    #[test]
    fn class_dictionary_imports_the_implementing_word() {
        let eq = ": foreign ieq ( Int Int -- Bool )\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  ieq\n\n# Eq Int\n  eq = eqInt\n\n: same ( a. Eq a => a a -- Bool )\n  eq\n";
        let eq_js = "export function ieq(a, b) { return a === b; }\n";
        let exports = r#"
    "./Data/Eq": { "ply": "./dist/Data/Eq.d.ply", "import": "./dist/Data/Eq.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#;
        let temp = Temp::new("class");
        temp.write("package.json", &package_json(exports));
        temp.write("src/Data/Eq.ply", eq);
        temp.write("foreign/Data/Eq.js", eq_js);
        temp.write(
            "src/App.ply",
            "~ \"Data/Eq\" (same)\n\n: use ( Int Int -- Bool )\n  same\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let iface = fs::read_to_string(temp.0.join("dist/Data/Eq.d.ply")).unwrap();
        assert!(
            iface.contains("& Eq a\n  eq ( a a -- Bool )\n"),
            "{iface}"
        );
        assert!(
            iface.contains("# Eq Int\n  eq = eqInt\n"),
            "{iface}"
        );
        assert!(!iface.contains("eq ( Eq a =>"), "{iface}");
        let js = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(
            js.contains("import { same, eqInt } from \"./Data/Eq.js\";"),
            "{js}"
        );
        assert!(js.contains("eq: eqInt"), "{js}");
        node(
            &temp.0,
            "import { use } from \"./dist/App.js\";\nif (use(1n, 1n) !== true) throw new Error(\"eq\");\nif (use(1n, 2n) !== false) throw new Error(\"neq\");\n",
        );

        let via = Temp::new("class-via");
        via.write(
            "package.json",
            &package_json(
                r#"
    "./Data/Eq": { "ply": "./dist/Data/Eq.d.ply", "import": "./dist/Data/Eq.js" },
    "./Data/Cmp": { "ply": "./dist/Data/Cmp.d.ply", "import": "./dist/Data/Cmp.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        via.write(
            "src/Data/Eq.ply",
            ": foreign ieq ( Int Int -- Bool )\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  ieq\n\n# Eq Int\n  eq = eqInt\n",
        );
        via.write("foreign/Data/Eq.js", eq_js);
        via.write(
            "src/Data/Cmp.ply",
            "~ \"Data/Eq\" (Eq, eq)\n\n: same ( a. Eq a => a a -- Bool )\n  eq\n",
        );
        via.write(
            "src/App.ply",
            "~ \"Data/Cmp\" (same)\n\n: use ( Int Int -- Bool )\n  same\n",
        );
        let error = build(&via.0).unwrap_err();
        assert_eq!(error.message, "no instance for Eq Int");
        via.write(
            "src/App.ply",
            "~ \"Data/Cmp\" (same)\n~ \"Data/Eq\" ()\n\n: use ( Int Int -- Bool )\n  same\n",
        );
        build(&via.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(via.0.join("dist/App.js")).unwrap();
        assert!(js.contains("import { same } from \"./Data/Cmp.js\";"), "{js}");
        assert!(js.contains("import { eqInt } from \"./Data/Eq.js\";"), "{js}");
        node(
            &via.0,
            "import { use } from \"./dist/App.js\";\nif (use(1n, 1n) !== true) throw new Error(\"via\");\n",
        );

        let bare = Temp::new("class-bare");
        bare.write("package.json", &package_json(exports));
        bare.write(
            "src/Data/Eq.ply",
            ": foreign ieq ( Int Int -- Bool )\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  ieq\n\n# Eq Int\n  eq = eqInt\n",
        );
        bare.write("foreign/Data/Eq.js", eq_js);
        bare.write(
            "src/App.ply",
            "~ \"Data/Eq\" ()\n\n: use ( Int Int -- Bool )\n  eq\n",
        );
        let error = build(&bare.0).unwrap_err();
        assert_eq!(error.message, "undefined word eq");

        let own = Temp::new("builtin-head");
        own.write("package.json", &package_json(exports));
        own.write("src/Data/Eq.ply", "& Eq a\n  eq ( a a -- Bool )\n");
        own.write(
            "src/App.ply",
            "~ \"Data/Eq\" (Eq)\n\n: eqBool ( Bool Bool -- Bool )\n  drop drop true\n\n# Eq Bool\n  eq = eqBool\n",
        );
        build(&own.0).unwrap_or_else(|error| panic!("{}", error.render()));

        let orphan = Temp::new("orphan");
        orphan.write(
            "package.json",
            &package_json(
                r#"
    "./Data/Eq": { "ply": "./dist/Data/Eq.d.ply", "import": "./dist/Data/Eq.js" },
    "./Data/Maybe": { "ply": "./dist/Data/Maybe.d.ply", "import": "./dist/Data/Maybe.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        orphan.write("src/Data/Eq.ply", "& Eq a\n  eq ( a a -- Bool )\n");
        orphan.write("src/Data/Maybe.ply", "; Maybe a = Nothing | Just a\n");
        orphan.write(
            "src/App.ply",
            "~ \"Data/Eq\" (Eq)\n~ \"Data/Maybe\" (Maybe)\n\n: eqMaybe ( a. (Maybe a) (Maybe a) -- Bool )\n  drop drop true\n\n# Eq (Maybe a)\n  eq = eqMaybe\n",
        );
        let error = build(&orphan.0).unwrap_err();
        assert_eq!(error.message, "orphan instance");

        let local = Temp::new("local-head");
        local.write("package.json", &package_json(exports));
        local.write("src/Data/Eq.ply", "& Eq a\n  eq ( a a -- Bool )\n");
        local.write(
            "src/App.ply",
            "~ \"Data/Eq\" (Eq, eq)\n\n; Box a = Mk a\n\n: eqBox ( a. (Box a) (Box a) -- Bool )\n  drop drop true\n\n# Eq (Box a)\n  eq = eqBox\n\n: use ( (Box Int) (Box Int) -- Bool )\n  eq\n",
        );
        build(&local.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(local.0.join("dist/App.js")).unwrap();
        assert!(js.contains("eqBox("), "{js}");

        let both = Temp::new("two-classes-one-method");
        both.write(
            "package.json",
            &package_json(
                r#"
    "./Data/Eq": { "ply": "./dist/Data/Eq.d.ply", "import": "./dist/Data/Eq.js" },
    "./Data/Also": { "ply": "./dist/Data/Also.d.ply", "import": "./dist/Data/Also.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        both.write("src/Data/Eq.ply", "& Eq a\n  eq ( a a -- Bool )\n");
        both.write("src/Data/Also.ply", "& Also a\n  eq ( a a -- Bool )\n");
        both.write(
            "src/App.ply",
            "~ \"Data/Eq\" (Eq)\n~ \"Data/Also\" (Also)\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq, Also Int\n  eq = eqInt\n",
        );
        let error = build(&both.0).unwrap_err();
        assert_eq!(error.message, "method eq is a method of Eq and Also");
    }

    #[test]
    fn zero_arg_foreign_keeps_its_js_arity() {
        let temp = Temp::new("io");
        temp.write(
            "package.json",
            &package_json(
                r#""./Data/Io": { "ply": "./dist/Data/Io.d.ply", "import": "./dist/Data/Io.js" }"#,
            ),
        );
        temp.write(
            "src/Data/Io.ply",
            ": foreign read ( -- Str )\n\n: line ( -- Str )\n  read\n",
        );
        temp.write(
            "foreign/Data/Io.js",
            "export function read() { return \"hi\"; }\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(temp.0.join("dist/Data/Io.js")).unwrap();
        assert!(js.contains("from \"../../foreign/Data/Io.js\""), "{js}");
        assert!(js.contains("read()"), "{js}");
        assert!(!js.contains("read(v"), "{js}");
        let iface = fs::read_to_string(temp.0.join("dist/Data/Io.d.ply")).unwrap();
        assert!(
            iface.contains(": foreign read ( -- Str )"),
            "{iface}"
        );
        assert!(iface.contains(": line ( -- Str )"), "{iface}");
        node(
            &temp.0,
            "import { line } from \"./dist/Data/Io.js\";\nif (line() !== \"hi\") throw new Error(\"io\");\n",
        );

        temp.write(
            "src/Data/Io.ply",
            ": foreign read ( -- Str )\n\n: line ( -- Int )\n  read\n",
        );
        let error = build(&temp.0).unwrap_err();
        assert_eq!(
            error.message,
            "type mismatch: expected Int, found Str"
        );
    }

    #[test]
    fn private_foreign_is_called_and_not_exported() {
        let temp = Temp::new("priv-foreign");
        temp.write(
            "package.json",
            &package_json(
                r#""./Data/Io": { "ply": "./dist/Data/Io.d.ply", "import": "./dist/Data/Io.js" }"#,
            ),
        );
        temp.write(
            "src/Data/Io.ply",
            ": foreign read ( -- Str )\n\n: private foreign secret ( -- Str )\n\n: useSecret ( -- Str )\n  secret\n\n: private hidden ( Str -- Str )\n  drop secret\n",
        );
        temp.write(
            "foreign/Data/Io.js",
            "export function read() { return \"hi\"; }\nexport function secret() { return \"sh\"; }\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(temp.0.join("dist/Data/Io.js")).unwrap();
        assert!(
            js.contains("import { read, secret } from \"../../foreign/Data/Io.js\";"),
            "{js}"
        );
        assert!(js.contains("export { read };"), "{js}");
        let export_line = js.lines().find(|line| line.starts_with("export {")).unwrap();
        assert_eq!(export_line, "export { read };");
        assert!(js.contains("function hidden"), "{js}");
        assert!(!js.contains("export function hidden"), "{js}");
        assert!(js.contains("export function useSecret"), "{js}");
        let iface = fs::read_to_string(temp.0.join("dist/Data/Io.d.ply")).unwrap();
        assert!(iface.contains(": foreign read ( -- Str )"), "{iface}");
        assert!(iface.contains(": foreign private secret ( -- Str )"), "{iface}");
        assert!(iface.contains(": private hidden ( Str -- Str )"), "{iface}");
        let module = crate::parse(&iface, crate::ParseMode::Interface)
            .unwrap_or_else(|error| panic!("{}", error.render("Data/Io.d.ply", &iface)));
        let secret = module.items.iter().find_map(|item| match item {
            crate::ast::Item::Def(def) if def.name == "secret" => Some(def),
            _ => None,
        });
        let secret = secret.expect("secret");
        assert!(secret.vis.is_foreign() && secret.vis.is_private());
        node(
            &temp.0,
            "import { useSecret, read } from \"./dist/Data/Io.js\";\nif (read() !== \"hi\" || useSecret() !== \"sh\") throw new Error(\"io\");\n",
        );
        temp.write(
            "src/App.ply",
            "~ \"Data/Io\" (secret)\n\n: use ( -- Str )\n  secret\n",
        );
        let error = build(&temp.0).unwrap_err();
        assert_eq!(error.message, "~ \"Data/Io\" does not export secret");
    }

    #[test]
    fn main_runs_from_node_and_an_import_does_not_call_it() {
        let temp = Temp::new("entry-main");
        temp.write("package.json", &package_json(""));
        temp.write(
            "src/App.ply",
            ": foreign touch ( -- Unit )\n\n: private main ( -- Unit )\n  touch\n",
        );
        temp.write(
            "src/Tool.ply",
            ": foreign touch ( -- Unit )\n\n: main ( -- Unit )\n  touch\n",
        );
        temp.write("src/Other.ply", ": idle ( -- Unit )\n  Unit\n");
        let app_mark = temp.0.join("app.txt");
        let tool_mark = temp.0.join("tool.txt");
        temp.write("foreign/App.js", &touch_js(&app_mark));
        temp.write("foreign/Tool.js", &touch_js(&tool_mark));
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!temp.0.join("dist").exists());
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));

        let app = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(app.contains("function main("), "{app}");
        assert!(!app.contains("export function main("), "{app}");
        assert!(app.contains("if (plyIsEntry) main();"), "{app}");
        assert!(!app.lines().any(|line| line.trim() == "main();"), "{app}");
        let tool = fs::read_to_string(temp.0.join("dist/Tool.js")).unwrap();
        assert!(tool.contains("export function main("), "{tool}");
        assert!(tool.contains("if (plyIsEntry) main();"), "{tool}");
        let other = fs::read_to_string(temp.0.join("dist/Other.js")).unwrap();
        assert!(!other.contains("import.meta.main"), "{other}");
        assert!(!temp.0.join("dist/run.js").exists());

        let importer = temp.0.join("use.mjs");
        fs::write(
            &importer,
            "import \"./dist/App.js\";\nimport { main } from \"./dist/Tool.js\";\nif (typeof main !== \"function\") throw new Error(\"main\");\n",
        )
        .unwrap();
        let imported = Command::new("node").arg(&importer).current_dir(&temp.0).output().unwrap();
        assert!(
            imported.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&imported.stderr)
        );
        assert!(!app_mark.exists(), "import called private main");
        assert!(!tool_mark.exists(), "import called main");

        let app_run = Command::new("node")
            .arg(temp.0.join("dist/App.js"))
            .output()
            .unwrap();
        assert!(app_run.status.success(), "{}", String::from_utf8_lossy(&app_run.stderr));
        assert_eq!(fs::read_to_string(&app_mark).unwrap(), "ran\n");
        assert!(!tool_mark.exists());
        let tool_run = Command::new("node")
            .arg(temp.0.join("dist/Tool.js"))
            .output()
            .unwrap();
        assert!(tool_run.status.success(), "{}", String::from_utf8_lossy(&tool_run.stderr));
        assert_eq!(fs::read_to_string(&tool_mark).unwrap(), "ran\n");

        temp.write(
            "src/Tool.ply",
            ": foreign boom ( -- Unit )\n\n: main ( -- Unit )\n  boom\n",
        );
        temp.write(
            "foreign/Tool.js",
            "export function boom() { throw new Error(\"boom\"); }\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let thrown = Command::new("node")
            .arg(temp.0.join("dist/Tool.js"))
            .output()
            .unwrap();
        assert_eq!(thrown.status.code(), Some(1));
    }

    #[test]
    fn main_accepts_only_the_unit_scheme() {
        let temp = Temp::new("entry-scheme");
        temp.write("package.json", &package_json(""));
        temp.write("src/App.ply", ": main ( . -- . Unit )\n  Unit\n");
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        temp.write("src/App.ply", ": main ( ..s -- ..s Unit )\n  Unit\n");
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        temp.write("src/App.ply", ": start ( -- Unit )\n  Unit\n");
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(!js.contains("import.meta.main"), "{js}");

        temp.write("src/App.ply", ": main ( -- Num )\n  1\n");
        let error = check_package(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0115");
        assert_eq!(error.message, "word main must have scheme ( -- Unit )");
        assert!(error.render().contains("App.ply"), "{}", error.render());
        temp.write("src/App.ply", ": main ( a. a -- a Unit )\n  Unit\n");
        let error = check_package(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0115");

        let lone = Temp::new("entry-snippet");
        lone.write("App.ply", ": main ( -- Unit )\n  Unit\n");
        let js = super::javascript_file(&lone.0.join("App.ply"))
            .unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(js.contains("if (plyIsEntry) main();"), "{js}");
        lone.write("App.ply", ": main ( -- Num )\n  1\n");
        let error = super::javascript_file(&lone.0.join("App.ply")).unwrap_err();
        assert_eq!(error.code, "PL0115");
    }

    fn touch_js(mark: &std::path::Path) -> String {
        let mark_lit = serde_json::to_string(&mark.to_string_lossy().into_owned()).unwrap();
        format!(
            "import {{ appendFileSync }} from \"node:fs\";\nexport function touch() {{ appendFileSync({mark_lit}, \"ran\\n\"); }}\n"
        )
    }

    fn node(dir: &std::path::Path, script: &str) {
        let path = dir.join("_check.mjs");
        fs::write(&path, script).unwrap();
        let output = Command::new("node")
            .arg(&path)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
