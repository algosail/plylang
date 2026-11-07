//! `plylang build` compiles a package: resolve imports, write `.js` and `.d.ply`,
//! then align `package.json` `exports` with those modules.
//! [`build`] is that compile and writes no `exports`. `plylang test` uses it.
//! [`build_exports`] aligns `exports`: one key per source module, `.` when
//! `ply.entry` names one, and a key this tool does not own stays.
//! `--exports` calls the same alignment. `plylang check` writes nothing.
//! A word named `main` with scheme `( -- Unit )` is called at the bottom of its
//! module when Node starts that file. `plylang compile` writes one page script
//! from the same graph. `main` is the root when the entry module defines it.
//! A module with no `main` and at least one place roots the script at those
//! places. `--entry`, `--out`, and `--foreign` override that one run and are
//! not written back.
//!
//! A local import is a module path such as `Data/Either`. A package import is
//! `@scope/name/...` or an unscoped `name/...`. `plylang check --js` prints the
//! JavaScript `build` would write for one source file. A file outside a package
//! is printed alone, and its imports are not resolved.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::ast::{Def, Exports, Item, Module, StackTail, Type};
use crate::check::{
    check_and_note, check_with_imports_key, collect_sites, collect_sites_with_imports_key,
    module_notes, KnownModule, ResolvedImport, StackAt,
};
use crate::codes::{error_url, Error};
use crate::diagnostic::Diagnostic;
use crate::parser::parse;
use crate::paths::{mode_from_path, module_name_from_path, ParseMode};
use crate::span::{line_col, Span};
use crate::LANGUAGE_VERSION;

mod exports;
mod header;
mod interface;
mod js_arity;

use header::{accepts_entry, apply_entry, combine, entry_def, reject_bad_entry, with_entry_guard};

#[cfg(test)]
use header::relative_specifier;

use exports::{write_package_exports, ExportEntry};
use interface::interface_text;
use js_arity::{exported_arity, JsExport};


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

    pub(crate) fn plain(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            message: message.clone(),
            code: "",
            rendered: message,
        }
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
    /// Module opened by a pathless package import and the `exports` key `.`.
    entry: Option<String>,
    dependencies: HashSet<String>,
    dev_dependencies: HashSet<String>,
    optional_dependencies: HashSet<String>,
    peer_dependencies: HashSet<String>,
}

impl Package {
    /// Listed in `devDependencies` and in none of the other dependency objects.
    fn dev_only(&self, name: &str) -> bool {
        self.dev_dependencies.contains(name)
            && !self.dependencies.contains(name)
            && !self.optional_dependencies.contains(name)
            && !self.peer_dependencies.contains(name)
    }
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
    /// Utilities under `test/`, keyed by the path a test imports (`Support/Session`).
    by_test: HashMap<String, usize>,
    exports_cache: HashMap<PathBuf, HashMap<String, String>>,
    topo: Vec<usize>,
    /// Published JavaScript values of each unit. Empty until `fill_surfaces`.
    surfaces: Vec<Vec<String>>,
    outputs: Vec<Output>,
}

enum Spec {
    Local(String),
    Package { name: String, subpath: String },
}


/// One `plylang build`. Absent fields keep the package values. Nothing is written back
/// except `exports` when [`BuildFlags::exports`] is set.
#[derive(Debug, Clone, Default)]
pub struct BuildFlags {
    /// Module path, the same shape as `ply.entry`. The `exports` key `.` uses it.
    pub entry: Option<String>,
    /// Module directory, relative to the package root. Overrides `ply.out`.
    pub out: Option<String>,
    /// Hand-written JavaScript directory, relative to the package root.
    pub foreign: Option<String>,
    /// Replace `package.json` `exports` with one entry per built module.
    pub exports: bool,
}

/// One `plylang compile`. Absent fields keep the package values. Nothing is written
/// back into `package.json`.
#[derive(Debug, Clone, Default)]
pub struct CompileFlags {
    /// Module path, the same shape as `ply.entry`.
    pub entry: Option<String>,
    /// The page script file, relative to the package root.
    /// Absent writes `<ply.out>/script.js`.
    pub out: Option<String>,
    /// Hand-written JavaScript directory, relative to the package root.
    pub foreign: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SessionOver {
    entry: Option<String>,
    out_rel: Option<String>,
    foreign: Option<String>,
}

pub fn build(dir: &Path) -> Result<(), BuildError> {
    build_with(dir, &BuildFlags::default())
}

/// Build the package, then replace `package.json` `exports` with one entry per built module.
pub fn build_exports(dir: &Path) -> Result<(), BuildError> {
    let mut flags = BuildFlags::default();
    flags.exports = true;
    build_with(dir, &flags)
}

/// Build the package. [`BuildFlags`] override `ply.entry`, `ply.out`, and `ply.foreign`
/// for this run and are not written back. `exports` is rewritten only when
/// [`BuildFlags::exports`] is set.
pub fn build_with(dir: &Path, flags: &BuildFlags) -> Result<(), BuildError> {
    let mut session = open_session_over(
        dir,
        &SessionOver {
            entry: flags.entry.clone(),
            out_rel: flags.out.clone(),
            foreign: flags.foreign.clone(),
        },
    )?;
    let entries = export_entries(
        &session.units,
        &session.package.out_rel,
        session.package.entry.as_deref(),
    );
    if flags.exports {
        session.package.exports = entries
            .iter()
            .map(|entry| (entry.key.clone(), entry.ply.clone()))
            .collect();
    }
    session.check_all()?;
    session.write_outputs()?;
    if flags.exports {
        write_package_exports(&session.package.root, &entries)?;
    }
    Ok(())
}

/// Write the Node modules, then one classic page script.
/// Does not rewrite `package.json`.
pub fn compile(dir: &Path) -> Result<(), BuildError> {
    compile_with(dir, &CompileFlags::default())
}

/// [`compile`] with [`CompileFlags`]. `--out` is the script file, not the module directory.
pub fn compile_with(dir: &Path, flags: &CompileFlags) -> Result<(), BuildError> {
    let mut session = open_session_over(
        dir,
        &SessionOver {
            entry: flags.entry.clone(),
            out_rel: None,
            foreign: flags.foreign.clone(),
        },
    )?;
    let script = match &flags.out {
        Some(raw) => session.package.root.join(kept_rel(&session.package, raw)?),
        None => session.package.out.join("script.js"),
    };
    let call_main = session.prepare_page()?;
    session.check_all()?;
    session.write_outputs()?;
    let entry = session
        .package
        .entry
        .clone()
        .expect("compile package has ply.entry");
    crate::bundle::write_page(&session.package.out, &entry, &script, call_main)
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
    open_session_over(dir, &SessionOver::default())
}

fn open_session_over(dir: &Path, over: &SessionOver) -> Result<Session, BuildError> {
    let root = fs::canonicalize(dir).map_err(|error| os_error(dir, error))?;
    let mut package = read_package(&root)?;
    apply_over(&mut package, over)?;
    let mut session = Session {
        package,
        units: Vec::new(),
        by_path: HashMap::new(),
        by_module: HashMap::new(),
        by_test: HashMap::new(),
        exports_cache: HashMap::new(),
        topo: Vec::new(),
        surfaces: Vec::new(),
        outputs: Vec::new(),
    };
    session.collect_sources()?;
    if let Some(name) = session.package.entry.clone() {
        if !session.by_module.contains_key(&name) {
            let path = session.package.root.join("package.json");
            let text = fs::read_to_string(&path).unwrap_or_default();
            return Err(pkg_error(&path, &text, Error::EntryModule(name)));
        }
    }
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

pub(crate) struct CompiledTest {
    pub js: String,
    /// Where the JavaScript must be written so its import paths resolve.
    pub js_path: PathBuf,
    /// A source module already written by [`build`]. The runner must not delete it.
    pub keep: bool,
    /// Public words `( -- Verdict )`, in source order. The runner calls these.
    pub words: Vec<TestWord>,
}

/// One discovered test. `line` is the 1-based line of the word.
#[derive(Clone)]
pub(crate) struct TestWord {
    pub name: String,
    pub line: u32,
}

/// The package that contains `start`, and the directory `ply.out` points at.
pub(crate) fn package_dirs(start: &Path) -> Result<(PathBuf, PathBuf), BuildError> {
    let found = find_ply_root(start).ok_or_else(|| {
        BuildError::plain(format!("no ply package at {}", start.display()))
    })?;
    let root = fs::canonicalize(&found).map_err(|error| os_error(&found, error))?;
    let package = read_package(&root)?;
    Ok((root, package.out))
}

/// Compile one file in a package. Imports resolve the same way as [`build`].
/// A file outside `ply.source` is still compiled against that package.
pub(crate) fn compile_test_file(root: &Path, file: &Path) -> Result<CompiledTest, BuildError> {
    let mut session = open_session(root)?;
    let (id, keep) = session.add_test_file(file)?;
    let js_path = session.test_js_path(id);
    let words = {
        let unit = &session.units[id];
        test_words(&unit.module, &unit.source)
    };
    let js = session.emit_one(id)?;
    // Dependencies under ply.source are already in ply.out. A harness outside
    // that tree is only compiled here, and the test's JavaScript imports it.
    session.write_outputs()?;
    Ok(CompiledTest {
        js,
        js_path,
        keep,
        words,
    })
}

/// A test is a word that takes nothing and returns `Verdict`.
fn test_words(module: &Module, source: &str) -> Vec<TestWord> {
    module
        .items
        .iter()
        .filter_map(|item| {
            let Item::Def(def) = item else {
                return None;
            };
            if def.vis.is_foreign() || !bare_verdict(def) {
                return None;
            }
            let (line, _) = line_col(source, def.name_span.start);
            Some(TestWord {
                name: def.name.clone(),
                line,
            })
        })
        .collect()
}

fn bare_verdict(def: &Def) -> bool {
    let scheme = &def.scheme;
    if !scheme.vars.is_empty() || !scheme.constraints.is_empty() || scheme.effect.effects.is_some()
    {
        return false;
    }
    let input = &scheme.effect.input;
    let output = &scheme.effect.output;
    let open = |tail: &StackTail| matches!(tail, StackTail::Implicit | StackTail::Empty);
    open(&input.tail)
        && open(&output.tail)
        && input.items.is_empty()
        && matches!(output.items.as_slice(), [Type::Ctor(name)] if name == "Verdict")
}

fn snippet_javascript(path: &Path) -> Result<String, BuildError> {
    let source = fs::read_to_string(path).map_err(|error| os_error(path, error))?;
    let module = parse(&source, mode_from_path(&path.to_string_lossy()))
        .map_err(|error| located(path, &source, error))?;
    let js = crate::check::javascript(&module).map_err(|error| located(path, &source, error))?;
    apply_entry(path, &source, &module, js)
}

/// One editor check: the diagnostic, and the stack notes for the same text.
/// Hover reads `sites` instead of checking the buffer again.
pub struct BufferNotes {
    pub error: Option<Diagnostic>,
    pub sites: Vec<StackAt>,
    /// Canonical paths this check loaded, including a module reached through a
    /// closed file. An open importer is stale when one of these files changes.
    pub deps: Vec<PathBuf>,
}

/// Check one editor buffer. Inside a package, imports resolve as in `plylang build`.
/// Outside a package this is the single-file checker. `plylang check` checks the
/// whole package and does not call this.
pub fn check_buffer(path: &Path, source: &str) -> Result<(), Diagnostic> {
    match buffer_notes(path, source).error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Stack notes for the editor. Same import rules as [`check_buffer`].
/// A failed check still returns the terms reached before the error.
/// A package that cannot be resolved falls back to a single-file check.
pub fn stack_sites(path: &Path, source: &str) -> Vec<StackAt> {
    buffer_notes(path, source).sites
}

/// The diagnostic and the stack notes of one buffer, from one check.
/// Dependencies are read from disk.
pub fn buffer_notes(path: &Path, source: &str) -> BufferNotes {
    buffer_notes_open(path, source, &[])
}

/// Check one buffer. `open` is unsaved editor text, as `(path, text)`.
/// A loaded file that is open is read from that text. A closed file is read
/// from disk. The buffer being checked is `source`, even when `open` also
/// names its path.
pub fn buffer_notes_open(path: &Path, source: &str, open: &[(&Path, &str)]) -> BufferNotes {
    let mode = if path.to_string_lossy().ends_with(".d.ply") {
        ParseMode::Interface
    } else {
        ParseMode::Source
    };
    let module = match parse(source, mode) {
        Ok(module) => module,
        Err(error) => {
            return BufferNotes {
                error: Some(error),
                sites: Vec::new(),
                deps: Vec::new(),
            };
        }
    };
    let entry_error = if mode == ParseMode::Source {
        reject_bad_entry(&module).err()
    } else {
        None
    };
    let Some(root) = find_ply_root(path) else {
        let (error, sites) = module_notes(&module);
        return BufferNotes {
            error: entry_error.or(error),
            sites,
            deps: Vec::new(),
        };
    };
    let package = match read_package(&root) {
        Ok(package) => package,
        Err(error) => {
            return BufferNotes {
                error: entry_error.or(Some(Diagnostic::with_code(
                    Span::new(0, 0),
                    error.code,
                    error.message,
                ))),
                sites: collect_sites(&module),
                deps: Vec::new(),
            };
        }
    };
    let mut overlays = HashMap::new();
    for (file, text) in open {
        overlays.insert(canonical(file), *text);
    }
    let mut doc = OpenDoc {
        package,
        units: Vec::new(),
        by_file: HashMap::new(),
        by_module: HashMap::new(),
        exports_cache: HashMap::new(),
        open: overlays,
        touched: Vec::new(),
    };
    doc.push_buffer(path, module.clone());
    if let Err(error) = doc.visit(0, &mut Vec::new(), None) {
        return BufferNotes {
            error: entry_error.or(Some(error)),
            sites: collect_sites(&module),
            deps: doc.dependency_paths(),
        };
    }
    let (error, sites) = doc.noted();
    BufferNotes {
        error: entry_error.or(error),
        sites,
        deps: doc.dependency_paths(),
    }
}

/// Import paths the package of `from_file` can name: files under the source
/// directory, utilities under `test/` when `from_file` is a test, then `ply`
/// exports of installed dependencies. The current file is left out. A source
/// file does not see a package that is only a devDependency.
pub(crate) fn available_import_paths(from_file: &Path) -> Vec<String> {
    let Some(root) = find_ply_root(from_file) else {
        return Vec::new();
    };
    let Ok(package) = read_package(&root) else {
        return Vec::new();
    };
    let current = module_path_under(&package, from_file);
    let mut paths = Vec::new();
    let mut files = Vec::new();
    if package.source.is_dir() {
        let _ = walk_ply(&package.source, &package.out, &mut files);
    }
    for file in files {
        let module_path = module_path_of(&package.source, &file);
        if current.as_deref() == Some(module_path.as_str()) {
            continue;
        }
        if module_name_from_path(&format!("{module_path}.ply")).is_err() {
            continue;
        }
        paths.push(module_path);
    }
    if importer_in_test(&package, from_file) {
        let test_root = package.root.join("test");
        let mut helpers = Vec::new();
        if test_root.is_dir() {
            let _ = walk_ply(&test_root, &package.out, &mut helpers);
        }
        for file in helpers {
            if canonical(&file) == canonical(from_file) {
                continue;
            }
            let module_path = module_path_of(&test_root, &file);
            if module_name_from_path(&format!("{module_path}.ply")).is_err() {
                continue;
            }
            if matches!(test_role(&file), Ok(TestRole::Utility)) {
                paths.push(module_path);
            }
        }
    }
    let library = path_under(&package.source, from_file);
    let text = fs::read_to_string(root.join("package.json")).unwrap_or_default();
    if let Ok(value) = serde_json::from_str::<Value>(&text) {
        for field in ["dependencies", "devDependencies", "optionalDependencies"] {
            if library && field == "devDependencies" {
                continue;
            }
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

enum Located {
    Local(PathBuf),
    Package {
        root: PathBuf,
        manifest: Value,
        file: PathBuf,
    },
}

fn locate_import(from_file: &Path, import_path: &str) -> Option<Located> {
    match classify(import_path)? {
        Spec::Local(module_path) => {
            let root = find_ply_root(from_file)?;
            let package = read_package(&root).ok()?;
            let source_file = package.source.join(&module_path).with_extension("ply");
            if importer_in_test(&package, from_file) {
                if let Some((file, role)) = classify_named_test(&package, &module_path) {
                    let source_exists = source_file.is_file();
                    match role {
                        TestRole::Utility if source_exists => return None,
                        TestRole::Utility => return Some(Located::Local(file)),
                        TestRole::Broken if !source_exists => return Some(Located::Local(file)),
                        TestRole::Entry | TestRole::Broken => {}
                    }
                }
            }
            if source_file.is_file() {
                return Some(Located::Local(source_file));
            }
            if let Some(file) = interface_file(&package.root, &module_path) {
                return Some(Located::Local(file));
            }
            Some(Located::Local(source_file))
        }
        Spec::Package { name, subpath } => {
            let root = find_installed(from_file, &name)?;
            let text = fs::read_to_string(root.join("package.json")).ok()?;
            let manifest: Value = serde_json::from_str(&text).ok()?;
            let target = export_map(&manifest)
                .get(&export_key(&subpath))?
                .clone();
            let file = target_file(&root, &target);
            Some(Located::Package { root, manifest, file })
        }
    }
}

/// Read the module an import names, for hover and completion. Missing packages yield `None`.
/// This is the exported file. Go to definition uses [`provider_file`], which prefers source.
pub(crate) fn load_provider(from_file: &Path, import_path: &str) -> Option<Module> {
    let file = match locate_import(from_file, import_path)? {
        Located::Local(file) | Located::Package { file, .. } => file,
    };
    read_module(&file)
}

/// The file an import names. A package export under `ply.out` that ends in `.d.ply`
/// yields the matching file under `ply.source` when that source file exists.
pub(crate) fn provider_file(from_file: &Path, import_path: &str) -> Option<PathBuf> {
    let file = match locate_import(from_file, import_path)? {
        Located::Local(file) => file,
        Located::Package { root, manifest, file } => {
            source_beside_interface(&root, &manifest, &file).unwrap_or(file)
        }
    };
    file.is_file().then_some(file)
}

/// `dist/Ply/Ref.d.ply` beside `src/Ply/Ref.ply`. The interface stays when the source is absent.
fn source_beside_interface(root: &Path, manifest: &Value, file: &Path) -> Option<PathBuf> {
    let file_name = file.file_name()?.to_str()?;
    if !file_name.ends_with(".d.ply") {
        return None;
    }
    let ply = manifest.get("ply")?;
    let source_rel = clean_rel(ply.get("source").and_then(Value::as_str).unwrap_or("src"));
    let out_rel = clean_rel(ply.get("out").and_then(Value::as_str).unwrap_or("dist"));
    if source_rel.is_empty() || out_rel.is_empty() {
        return None;
    }
    let rel = file.strip_prefix(root.join(&out_rel)).ok()?;
    let rel = rel.to_str()?.replace('\\', "/");
    let stem = rel.strip_suffix(".d.ply")?;
    let source_file = root.join(source_rel).join(format!("{stem}.ply"));
    source_file.is_file().then_some(source_file)
}

impl Session {
    fn check_all(&mut self) -> Result<(), BuildError> {
        let ids: Vec<usize> = (0..self.units.len()).collect();
        for id in ids {
            self.visit(id, &mut Vec::new())?;
        }
        self.fill_surfaces();
        let order = self.topo.clone();
        for id in order {
            self.check_unit(id)?;
        }
        Ok(())
    }

    /// `plylang compile` writes one page. `ply.entry` names its module.
    /// A `main` of scheme `( -- Unit )` is the root and the script calls it.
    /// A module with no `main` and at least one place roots the script at
    /// those places and does not call `main`. Anything else is PL0129.
    /// A `main` of another scheme stays PL0115 from the module check.
    /// The bool is whether the script calls `main`.
    fn prepare_page(&self) -> Result<bool, BuildError> {
        let Some(name) = &self.package.entry else {
            let path = self.package.root.join("package.json");
            let text = fs::read_to_string(&path).unwrap_or_default();
            return Err(pkg_error(&path, &text, Error::BrowserEntry));
        };
        let id = *self.by_module.get(name).expect("entry module");
        let module = &self.units[id].module;
        if entry_def(module).is_some() {
            return Ok(true);
        }
        if has_place(module) {
            return Ok(false);
        }
        Err(self.fail(id, Span::new(0, 0), Error::BrowserEntry))
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
        self.fill_surfaces();
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

    fn test_js_path(&self, id: usize) -> PathBuf {
        let module_path = self.units[id].module_path.clone().expect("test module");
        self.package.out.join(module_path).with_extension("js")
    }

    /// A source file is compiled in place. A test file outside `ply.source`
    /// becomes a module whose path is its path relative to the package root.
    fn add_test_file(&mut self, file: &Path) -> Result<(usize, bool), BuildError> {
        let canonical = fs::canonicalize(file).map_err(|error| os_error(file, error))?;
        if let Some(id) = self.by_path.get(&canonical).copied() {
            return Ok((id, true));
        }
        let relative = canonical.strip_prefix(&self.package.root).map_err(|_| {
            BuildError::plain(format!(
                "test file is outside the package: {}",
                canonical.display()
            ))
        })?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        let Some(module_path) = relative.strip_suffix(".ply") else {
            return Err(BuildError::plain(format!(
                "test file is not a .ply file: {}",
                canonical.display()
            )));
        };
        if module_path.is_empty() || module_path.ends_with(".d") {
            return Err(BuildError::plain(format!(
                "test file is not a source file: {}",
                canonical.display()
            )));
        }
        if let Some(id) = self.by_module.get(module_path).copied() {
            if self.units[id].path != canonical {
                return Err(BuildError::plain(format!(
                    "test file clashes with module {module_path}"
                )));
            }
        }
        let source = fs::read_to_string(&canonical).map_err(|error| os_error(&canonical, error))?;
        let module = parse(&source, ParseMode::Source)
            .map_err(|error| located(&canonical, &source, error))?;
        let id = self.units.len();
        self.by_path.insert(canonical.clone(), id);
        self.units.push(Unit {
            path: canonical.clone(),
            display: canonical.display().to_string(),
            source,
            module,
            package_root: self.package.root.clone(),
            module_path: Some(module_path.to_string()),
            kind: Kind::Local,
            color: Color::White,
            deps: Vec::new(),
        });
        Ok((id, false))
    }

    /// A utility under `test/`. Its module path is the import name, so the
    /// library build does not publish it and a source module of that name is
    /// a different file.
    fn load_test_module(&mut self, file: &Path, module_path: &str) -> Result<usize, BuildError> {
        if let Some(id) = self.by_test.get(module_path).copied() {
            return Ok(id);
        }
        let canonical = fs::canonicalize(file).map_err(|error| os_error(file, error))?;
        if let Some(id) = self.by_path.get(&canonical).copied() {
            self.by_test.insert(module_path.to_string(), id);
            return Ok(id);
        }
        let source = fs::read_to_string(&canonical).map_err(|error| os_error(&canonical, error))?;
        let module = parse(&source, ParseMode::Source)
            .map_err(|error| located(&canonical, &source, error))?;
        let id = self.units.len();
        self.by_path.insert(canonical.clone(), id);
        self.by_test.insert(module_path.to_string(), id);
        self.units.push(Unit {
            path: canonical.clone(),
            display: canonical.display().to_string(),
            source,
            module,
            package_root: self.package.root.clone(),
            module_path: Some(module_path.to_string()),
            kind: Kind::Local,
            color: Color::White,
            deps: Vec::new(),
        });
        Ok(id)
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
            if importer_in_test(&self.package, &self.units[from].path) {
                if let Some((file, role)) = classify_named_test(&self.package, module_path) {
                    let source_exists = self.by_module.contains_key(module_path);
                    match role {
                        TestRole::Utility if source_exists => {
                            return Err(self.fail(
                                from,
                                span,
                                Error::TestModuleShadow(module_path.to_string()),
                            ));
                        }
                        TestRole::Utility => return self.load_test_module(&file, module_path),
                        TestRole::Broken if !source_exists => {
                            return self.load_test_module(&file, module_path);
                        }
                        TestRole::Broken | TestRole::Entry => {}
                    }
                }
            }
            if let Some(id) = self.by_module.get(module_path).copied() {
                return Ok(id);
            }
            return Err(self.fail(from, span, Error::CannotResolveImport(import_path.to_string())));
        }
        if let Some(file) = self.exported_file(&package_root, module_path) {
            return self.load_external(file, package_root, Some(module_path.to_string()));
        }
        if let Some(file) = interface_file(&package_root, module_path) {
            return self.load_external(file, package_root, Some(module_path.to_string()));
        }
        Err(self.fail(from, span, Error::CannotResolveImport(import_path.to_string())))
    }

    fn resolve_package(
        &mut self,
        from: usize,
        name: &str,
        subpath: &str,
        import_path: &str,
        span: Span,
    ) -> Result<usize, BuildError> {
        if self.units[from].package_root == self.package.root
            && path_under(&self.package.source, &self.units[from].path)
            && self.package.dev_only(name)
        {
            return Err(self.fail(from, span, Error::DevDependency(import_path.to_string())));
        }
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
        let module_path = if subpath.is_empty() {
            None
        } else {
            Some(subpath.to_string())
        };
        self.load_external(file, pkg_root, module_path)
    }

    /// An exports entry, when the key exists and the file is on disk.
    fn exported_file(&mut self, package_root: &Path, module_path: &str) -> Option<PathBuf> {
        let exports = self.package_exports(package_root).ok()?;
        let target = exports.get(&export_key(module_path))?;
        let file = target_file(package_root, target);
        file.is_file().then_some(file)
    }

    fn load_external(
        &mut self,
        file: PathBuf,
        package_root: PathBuf,
        module_path: Option<String>,
    ) -> Result<usize, BuildError> {
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
            module_path,
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
        if self.unit_needs_public(id) && matches!(module.exports, Exports::Closed) {
            return Err(self.fail(id, module.span, Error::MissingPublicList));
        }
        let key = module_key_of(
            &self.units[id].package_root,
            self.units[id].module_path.as_deref(),
        );
        let checked = check_with_imports_key(&module, &resolved, &key)
            .map_err(|error| self.fail_diagnostic(id, error))?;
        if self.units[id].kind == Kind::External {
            return Ok(());
        }
        self.units[id].module.items.extend(checked.derived.iter().cloned());
        let module = self.units[id].module.clone();
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
            out.push(known_at(&self.units, id));
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

    fn unit_needs_public(&self, id: usize) -> bool {
        if self.units[id].kind == Kind::External {
            return false;
        }
        let path = &self.units[id].path;
        if path_under(&self.package.source, path) {
            return true;
        }
        importer_in_test(&self.package, path) && !is_test_entry(&self.units[id].module)
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
    module_path: Option<String>,
    /// `Data.Num` from the path, or the file name for a package outside this one.
    label: String,
    color: Color,
    deps: Vec<usize>,
}

struct OpenDoc<'a> {
    package: Package,
    units: Vec<DocUnit>,
    by_file: HashMap<PathBuf, usize>,
    by_module: HashMap<String, usize>,
    exports_cache: HashMap<PathBuf, HashMap<String, String>>,
    /// Unsaved text keyed by the canonical path. Disk is the fallback.
    open: HashMap<PathBuf, &'a str>,
    /// Every file `load` considered, in visit order. A parse failure still
    /// records the path, so the importer is known before the module parses.
    touched: Vec<PathBuf>,
}

impl<'a> OpenDoc<'a> {
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
        if let Some(module_path) = module_path.clone() {
            self.by_module.insert(module_path, id);
        }
        self.units.push(DocUnit {
            path: file,
            module,
            package_root,
            module_path,
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
            if importer_in_test(&self.package, &self.units[from].path) {
                if let Some((file, role)) = classify_named_test(&self.package, module_path) {
                    let source_exists = self
                        .package
                        .source
                        .join(module_path)
                        .with_extension("ply")
                        .is_file();
                    match role {
                        TestRole::Utility if source_exists => {
                            return Err(Diagnostic::new(
                                span,
                                Error::TestModuleShadow(module_path.to_string()),
                            ));
                        }
                        TestRole::Utility => {
                            return self.load(
                                file,
                                package_root,
                                Some(module_path.to_string()),
                                span,
                                import_path,
                            );
                        }
                        TestRole::Broken if !source_exists => {
                            return self.load(
                                file,
                                package_root,
                                Some(module_path.to_string()),
                                span,
                                import_path,
                            );
                        }
                        TestRole::Broken | TestRole::Entry => {}
                    }
                }
            }
            if let Some(id) = self.by_module.get(module_path).copied() {
                return Ok(id);
            }
            let file = self.package.source.join(module_path).with_extension("ply");
            return self.load(file, package_root, Some(module_path.to_string()), span, import_path);
        }
        if let Some(file) = self.exported_file(&package_root, module_path, span, import_path)? {
            return self.load(
                file,
                package_root,
                Some(module_path.to_string()),
                span,
                import_path,
            );
        }
        if let Some(file) = interface_file(&package_root, module_path) {
            return self.load(
                file,
                package_root,
                Some(module_path.to_string()),
                span,
                import_path,
            );
        }
        Err(missing(span, import_path))
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
        if self.units[from].package_root == self.package.root
            && path_under(&self.package.source, &self.units[from].path)
            && self.package.dev_only(name)
        {
            return Err(Diagnostic::new(span, Error::DevDependency(import_path.to_string())));
        }
        let Some(pkg_root) = find_installed(&start, name) else {
            return Err(missing(span, import_path));
        };
        let exports = self.package_exports(&pkg_root, span, import_path)?;
        let Some(target) = exports.get(&export_key(subpath)).cloned() else {
            return Err(missing(span, import_path));
        };
        let file = target_file(&pkg_root, &target);
        let module_path = if subpath.is_empty() {
            None
        } else {
            Some(subpath.to_string())
        };
        self.load(file, pkg_root, module_path, span, import_path)
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
        self.touched.push(canonical.clone());
        if let Some(id) = self.by_file.get(&canonical).copied() {
            return Ok(id);
        }
        let mode = if file.to_string_lossy().ends_with(".d.ply") {
            ParseMode::Interface
        } else {
            ParseMode::Source
        };
        // The editor copy wins over disk, including when the parse fails.
        let module = {
            let from_editor = self.open.get(&canonical).copied();
            let disk = if from_editor.is_none() {
                Some(fs::read_to_string(&file).map_err(|error| {
                    let os = Error::Os(error.to_string());
                    Diagnostic::with_code(
                        span,
                        os.code(),
                        format!("{}: {}", file.display(), os.message()),
                    )
                })?)
            } else {
                None
            };
            let source = match from_editor {
                Some(text) => text,
                None => disk.as_deref().unwrap(),
            };
            parse(source, mode).map_err(|error| {
                let (line, col) = line_col(source, error.span.start);
                Diagnostic::with_code(
                    span,
                    error.code,
                    format!("{}:{line}:{col}: {}", file.display(), error.message),
                )
            })?
        };
        Ok(self.insert(canonical, module, package_root, module_path))
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

    fn noted(&self) -> (Option<Diagnostic>, Vec<StackAt>) {
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
        let key = module_key_of(
            &self.units[0].package_root,
            self.units[0].module_path.as_deref(),
        );
        if self.buffer_needs_public() && matches!(self.units[0].module.exports, Exports::Closed) {
            return (
                Some(Diagnostic::new(
                    self.units[0].module.span,
                    Error::MissingPublicList,
                )),
                self.sites(),
            );
        }
        let noted = check_and_note(&self.units[0].module, &resolved, &key);
        if !noted.sites.is_empty() {
            return (noted.error, noted.sites);
        }
        (noted.error, collect_sites(&self.units[0].module))
    }

    fn dependency_paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for path in &self.touched {
            if path == &self.units[0].path || out.contains(path) {
                continue;
            }
            out.push(path.clone());
        }
        out
    }

    fn buffer_needs_public(&self) -> bool {
        let path = &self.units[0].path;
        if path_under(&self.package.source, path) {
            return true;
        }
        importer_in_test(&self.package, path) && !is_test_entry(&self.units[0].module)
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
        let key = module_key_of(
            &self.units[0].package_root,
            self.units[0].module_path.as_deref(),
        );
        let sites = collect_sites_with_imports_key(&self.units[0].module, &resolved, &key);
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
            out.push(known_at(&self.units, id));
            for dep in &self.units[id].deps {
                pending.push(*dep);
            }
        }
        out
    }

    fn exported_file(
        &mut self,
        package_root: &Path,
        module_path: &str,
        span: Span,
        import_path: &str,
    ) -> Result<Option<PathBuf>, Diagnostic> {
        let exports = self.package_exports(package_root, span, import_path)?;
        let Some(target) = exports.get(&export_key(module_path)) else {
            return Ok(None);
        };
        let file = target_file(package_root, target);
        Ok(file.is_file().then_some(file))
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
    import_cycle(names)
}

fn read_package(root: &Path) -> Result<Package, BuildError> {
    let path = root.join("package.json");
    let text = fs::read_to_string(&path).map_err(|error| os_error(&path, error))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| pkg_error(&path, &text, Error::PackageJsonInvalid))?;
    let Some(ply) = value.get("ply") else {
        return Err(pkg_error(&path, &text, Error::PackageJsonNoPly));
    };
    let source_rel = match ply.get("source") {
        None => "src".to_string(),
        Some(value) => {
            let Some(text_rel) = value.as_str() else {
                return Err(pkg_error(&path, &text, Error::PackageJsonSource));
            };
            clean_rel(text_rel)
        }
    };
    let out_rel = match ply.get("out") {
        None => "dist".to_string(),
        Some(value) => {
            let Some(text_rel) = value.as_str() else {
                return Err(pkg_error(&path, &text, Error::PackageJsonOut));
            };
            clean_rel(text_rel)
        }
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
    if source_rel.is_empty() || out_rel.is_empty() || source_rel == out_rel {
        return Err(pkg_error(
            &path,
            &text,
            Error::OutSameAsSource,
        ));
    }
    let foreign = match ply.get("foreign") {
        None => {
            let rel = "foreign";
            let dir = root.join(rel);
            if dir.is_dir() {
                if rel == source_rel
                    || rel == out_rel
                    || nests(rel, &source_rel)
                    || nests(rel, &out_rel)
                {
                    return Err(pkg_error(&path, &text, Error::ForeignSameDirectory));
                }
                Some(dir)
            } else {
                None
            }
        }
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
    let entry = match ply.get("entry") {
        None => None,
        Some(value) => {
            let Some(name) = value.as_str() else {
                return Err(pkg_error(&path, &text, Error::PackageJsonEntry));
            };
            let name = clean_rel(name);
            module_name_from_path(&format!("{name}.ply"))
                .map_err(|error| pkg_error(&path, &text, error))?;
            Some(name)
        }
    };
    if ply.get("target").is_some() {
        return Err(pkg_error(&path, &text, Error::PackageJsonTarget));
    }
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
        entry,
        dependencies: name_set(&value, "dependencies"),
        dev_dependencies: name_set(&value, "devDependencies"),
        optional_dependencies: name_set(&value, "optionalDependencies"),
        peer_dependencies: name_set(&value, "peerDependencies"),
        root: root.to_path_buf(),
    })
}

fn name_set(value: &Value, field: &str) -> HashSet<String> {
    value
        .get(field)
        .and_then(Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
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

fn export_entries(units: &[Unit], out_rel: &str, entry: Option<&str>) -> Vec<ExportEntry> {
    let mut entries = Vec::new();
    for unit in units {
        if unit.kind != Kind::Local {
            continue;
        }
        let Some(module_path) = &unit.module_path else {
            continue;
        };
        // A test file's path starts at the package root (`test/Data/State`).
        // Source modules are the ones `exports` names.
        if module_path.starts_with("test/") {
            continue;
        }
        entries.push(ExportEntry {
            key: export_key(module_path),
            ply: format!("./{out_rel}/{module_path}.d.ply"),
            js: format!("./{out_rel}/{module_path}.js"),
        });
    }
    if let Some(name) = entry {
        let key = export_key(name);
        if let Some(module) = entries.iter().find(|item| item.key == key) {
            entries.push(ExportEntry {
                key: ".".to_string(),
                ply: module.ply.clone(),
                js: module.js.clone(),
            });
        }
    }
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries
}

fn target_file(root: &Path, target: &str) -> PathBuf {
    let relative = target.strip_prefix("./").unwrap_or(target);
    root.join(relative)
}

pub(crate) fn find_installed(start: &Path, name: &str) -> Option<PathBuf> {
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

/// A file under `test/` that is not itself under the source directory.
fn importer_in_test(package: &Package, file: &Path) -> bool {
    path_under(&package.root.join("test"), file) && !path_under(&package.source, file)
}

fn path_under(dir: &Path, file: &Path) -> bool {
    if !dir.exists() {
        return false;
    }
    canonical(file).starts_with(&canonical(dir))
}

enum TestRole {
    /// `( -- Verdict )` or `suite`. Not a module.
    Entry,
    /// A module only a test can import.
    Utility,
    /// The file did not parse. The runner still reports it as a test.
    Broken,
}

fn test_role(file: &Path) -> std::io::Result<TestRole> {
    let source = fs::read_to_string(file)?;
    Ok(role_of_source(&source))
}

fn role_of_source(source: &str) -> TestRole {
    match parse(source, ParseMode::Source) {
        Err(_) => TestRole::Broken,
        Ok(module) if is_test_entry(&module) => TestRole::Entry,
        Ok(_) => TestRole::Utility,
    }
}

/// A test entry is a word `( -- Verdict )` or a word named `suite`.
fn is_test_entry(module: &Module) -> bool {
    module.items.iter().any(|item| {
        let Item::Def(def) = item else {
            return false;
        };
        def.name == "suite" || (!def.vis.is_foreign() && bare_verdict(def))
    })
}

/// `test/{module}.ply` when that file can be imported, or when it fails to parse.
/// A test entry is not a module.
fn classify_named_test(package: &Package, module_path: &str) -> Option<(PathBuf, TestRole)> {
    if module_name_from_path(&format!("{module_path}.ply")).is_err() {
        return None;
    }
    let file = package
        .root
        .join("test")
        .join(module_path)
        .with_extension("ply");
    if !file.is_file() {
        return None;
    }
    let role = test_role(&file).ok()?;
    Some((file, role))
}

/// Parse failure counts as an entry so the runner shows the diagnostic.
pub(crate) fn file_is_test_entry(path: &Path) -> Result<bool, BuildError> {
    let source = fs::read_to_string(path).map_err(|error| os_error(path, error))?;
    Ok(!matches!(role_of_source(&source), TestRole::Utility))
}

/// A utility whose path is already a source module fails the test run.
pub(crate) fn reject_shadowed_test_modules(root: &Path) -> Result<(), BuildError> {
    let package = read_package(root)?;
    let test_root = package.root.join("test");
    if !test_root.is_dir() {
        return Ok(());
    }
    let mut files = Vec::new();
    walk_ply(&test_root, &package.out, &mut files).map_err(|error| os_error(&test_root, error))?;
    files.sort();
    for file in files {
        let module_path = module_path_of(&test_root, &file);
        if module_name_from_path(&format!("{module_path}.ply")).is_err() {
            continue;
        }
        let source = fs::read_to_string(&file).map_err(|error| os_error(&file, error))?;
        if !matches!(role_of_source(&source), TestRole::Utility) {
            continue;
        }
        let src = package.source.join(&module_path).with_extension("ply");
        if src.is_file() {
            return Err(located(
                &file,
                &source,
                Diagnostic::new(Span::new(0, 0), Error::TestModuleShadow(module_path)),
            ));
        }
    }
    Ok(())
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
                    if value.get("ply").and_then(Value::as_object).is_some() {
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

pub(crate) fn read_module(path: &Path) -> Option<Module> {
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

trait LoadedUnit {
    fn file(&self) -> &Path;
    fn loaded_module(&self) -> &Module;
    fn loaded_root(&self) -> &Path;
    fn loaded_module_path(&self) -> Option<&str>;
    fn dep_ids(&self) -> &[usize];
}

impl LoadedUnit for Unit {
    fn file(&self) -> &Path {
        &self.path
    }
    fn loaded_module(&self) -> &Module {
        &self.module
    }
    fn loaded_root(&self) -> &Path {
        &self.package_root
    }
    fn loaded_module_path(&self) -> Option<&str> {
        self.module_path.as_deref()
    }
    fn dep_ids(&self) -> &[usize] {
        &self.deps
    }
}

impl LoadedUnit for DocUnit {
    fn file(&self) -> &Path {
        &self.path
    }
    fn loaded_module(&self) -> &Module {
        &self.module
    }
    fn loaded_root(&self) -> &Path {
        &self.package_root
    }
    fn loaded_module_path(&self) -> Option<&str> {
        self.module_path.as_deref()
    }
    fn dep_ids(&self) -> &[usize] {
        &self.deps
    }
}

fn known_at(units: &[impl LoadedUnit], id: usize) -> KnownModule {
    let unit = &units[id];
    let origin = unit.file().display().to_string();
    let deps = unit
        .dep_ids()
        .iter()
        .map(|dep| units[*dep].file().display().to_string())
        .collect();
    known_module(
        &origin,
        unit.loaded_module().clone(),
        unit.loaded_root(),
        unit.loaded_module_path(),
        deps,
    )
}

fn known_module(
    origin: &str,
    module: Module,
    package_root: &Path,
    module_path: Option<&str>,
    deps: Vec<String>,
) -> KnownModule {
    KnownModule {
        origin: origin.to_string(),
        module,
        deps,
        module_key: module_key_of(package_root, module_path),
    }
}

/// Package root and module path. The same module is the same instance origin
/// whether the file on disk is source or a generated interface.
fn module_key_of(package_root: &Path, module_path: Option<&str>) -> String {
    match module_path {
        Some(path) if !path.is_empty() => format!("{}\n{path}", package_root.display()),
        _ => String::new(),
    }
}

/// `{ply.out}/{module}.d.ply` when that file exists. A package specifier still
/// resolves only through `exports`. This is the sibling of a local import.
fn interface_file(package_root: &Path, module_path: &str) -> Option<PathBuf> {
    if module_path.is_empty() {
        return None;
    }
    let text = fs::read_to_string(package_root.join("package.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let out = value.get("ply")?.get("out")?.as_str()?;
    let out = clean_rel(out);
    if out.is_empty() {
        return None;
    }
    let file = package_root.join(out).join(format!("{module_path}.d.ply"));
    file.is_file().then_some(file)
}

pub(crate) fn load_surface(from: &Path, import_path: &str) -> Option<(PathBuf, Module)> {
    let file = provider_file(from, import_path)?;
    let module = read_module(&file)?;
    Some((file, module))
}

fn clean_rel(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    while let Some(rest) = path.strip_prefix("./") {
        path = rest.to_string();
    }
    path.trim_matches('/').to_string()
}

fn has_place(module: &Module) -> bool {
    module
        .items
        .iter()
        .any(|item| matches!(item, Item::Place(_)))
}

/// Apply one run's `--entry`, `--out`, and `--foreign`. The package file stays as it was.
fn apply_over(package: &mut Package, over: &SessionOver) -> Result<(), BuildError> {
    if over.entry.is_none() && over.out_rel.is_none() && over.foreign.is_none() {
        return Ok(());
    }
    let manifest = package.root.join("package.json");
    let text = fs::read_to_string(&manifest).unwrap_or_default();
    if let Some(entry) = &over.entry {
        let name = clean_rel(entry);
        module_name_from_path(&format!("{name}.ply"))
            .map_err(|error| pkg_error(&manifest, &text, error))?;
        package.entry = Some(name);
    }
    if let Some(out) = &over.out_rel {
        let rel = kept_text(&manifest, &text, out)?;
        package.out_rel = rel.clone();
        package.out = package.root.join(rel);
    }
    if let Some(foreign) = &over.foreign {
        let rel = kept_text(&manifest, &text, foreign)?;
        package.foreign = Some(package.root.join(rel));
    }
    if over.out_rel.is_some() || over.foreign.is_some() {
        check_directories(package, &manifest, &text)?;
    }
    Ok(())
}

fn kept_rel(package: &Package, raw: &str) -> Result<String, BuildError> {
    let manifest = package.root.join("package.json");
    let text = fs::read_to_string(&manifest).unwrap_or_default();
    kept_text(&manifest, &text, raw)
}

fn kept_text(manifest: &Path, text: &str, raw: &str) -> Result<String, BuildError> {
    if path_leaves(raw) {
        return Err(pkg_error(
            manifest,
            text,
            Error::PathLeavesPackage(raw.to_string()),
        ));
    }
    Ok(clean_rel(raw))
}

fn path_leaves(raw: &str) -> bool {
    if raw.is_empty() || Path::new(raw).is_absolute() {
        return true;
    }
    let cleaned = clean_rel(raw);
    cleaned.is_empty() || cleaned.split('/').any(|part| part.is_empty() || part == "..")
}

fn check_directories(package: &Package, manifest: &Path, text: &str) -> Result<(), BuildError> {
    let source_rel = package_rel(&package.root, &package.source);
    let out_rel = &package.out_rel;
    if source_rel.is_empty() || out_rel.is_empty() || source_rel == *out_rel || nests(&source_rel, out_rel)
    {
        return Err(pkg_error(manifest, text, Error::OutSameAsSource));
    }
    if let Some(foreign) = &package.foreign {
        let rel = package_rel(&package.root, foreign);
        if rel.is_empty()
            || rel == source_rel
            || rel == *out_rel
            || nests(&rel, &source_rel)
            || nests(&rel, out_rel)
        {
            return Err(pkg_error(manifest, text, Error::ForeignSameDirectory));
        }
    }
    Ok(())
}

fn package_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

fn import_cycle(names: Vec<String>) -> Error {
    Error::ImportCycle(names.join(", "))
}

fn cycle_message(units: &[Unit], stack: &[usize], again: usize) -> Error {
    let start = stack.iter().position(|id| *id == again).unwrap_or(0);
    let mut names: Vec<String> = stack[start..]
        .iter()
        .map(|id| unit_label(&units[*id]))
        .collect();
    names.push(unit_label(&units[again]));
    import_cycle(names)
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        build, build_exports, build_with, check_buffer, check_package, compile, compile_with,
        relative_specifier, BuildFlags, CompileFlags,
    };

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
        temp.write(
            "src/Data/Maybe.ply",
            "@ Maybe, Nothing, Just\n\n; Maybe a = Nothing | Just a\n",
        );
        let source = "@ wrap\n~ \"Data/Maybe\" Maybe, Just\n\n: wrap ( a. a -- (Maybe a) )\n  Just\n";
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
        let source = "~ \"Data/Maybe\" f\n\n: use ( Int -- Int )\n  f\n";
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
    fn open_text_hides_an_export_from_an_importer() {
        let temp = Temp::new("open-export");
        temp.write(
            "package.json",
            &package_json(
                r#"
    "./Data/Lib": { "ply": "./dist/Data/Lib.d.ply", "import": "./dist/Data/Lib.js" },
    "./Data/Mid": { "ply": "./dist/Data/Mid.d.ply", "import": "./dist/Data/Mid.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        temp.write("src/Data/Lib.ply", "@ kept\n\n: kept ( Int -- Int )\n  dup drop\n");
        temp.write(
            "src/Data/Mid.ply",
            "@ kept\n~ \"Data/Lib\" kept\n\n: pass ( Int -- Int )\n  kept\n",
        );
        let app = "@ go\n~ \"Data/Lib\" kept\n\n: go ( Int -- Int )\n  kept\n";
        temp.write("src/App.ply", app);
        let app_path = temp.0.join("src/App.ply");
        let lib_path = temp.0.join("src/Data/Lib.ply");
        let mid_path = temp.0.join("src/Data/Mid.ply");
        super::check_buffer(&app_path, app).unwrap_or_else(|error| {
            panic!("{}", error.render("App.ply", app))
        });
        let hidden = "@\n\n: kept ( Int -- Int )\n  dup drop\n";
        let notes = super::buffer_notes_open(&app_path, app, &[(&lib_path, hidden)]);
        let error = notes.error.expect("missing export");
        assert_eq!(error.code, "PL0091", "{}", error.message);
        assert!(error.message.contains("kept"), "{}", error.message);
        let lib = fs::canonicalize(&lib_path).unwrap();
        assert!(notes.deps.contains(&lib), "{:?}", notes.deps);

        let through = "@ go\n~ \"Data/Mid\" kept\n\n: go ( Int -- Int )\n  kept\n";
        temp.write("src/App.ply", through);
        let notes = super::buffer_notes_open(&app_path, through, &[(&lib_path, hidden)]);
        let error = notes.error.expect("re-export lost kept");
        assert_eq!(error.code, "PL0091", "{}", error.message);
        let mid = fs::canonicalize(&mid_path).unwrap();
        assert!(notes.deps.contains(&lib), "{:?}", notes.deps);
        assert!(notes.deps.contains(&mid), "{:?}", notes.deps);
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
        let source = "~ \"Data/Maybe\" Maybe\n\n: use ( a. -- (Maybe a) )\n  Nothing\n";
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
        temp.write("src/App.ply", "@ id\n\n: id ( Int -- Int )\n  dup drop\n");
        temp.write("src/Broken.ply", "@ bad\n\n: bad ( Int -- Int )\n  \"no\"\n");
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
            "@ twice\n~ \"Data/Num\" add\n\n: twice ( Int -- Int )\n  dup add\n",
        );
        temp.write("src/Data/Num.ply", "@ add\n\n: add ( Int Int -- Int )\n  drop\n");
        temp.write("src/Hidden.ply", "@\n\n: step ( Int -- Int )\n  dup drop\n");
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert_eq!(fs::read_to_string(temp.0.join("package.json")).unwrap(), original);
        assert!(temp.0.join("dist/Data/Num.js").is_file());

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
        fresh.write("src/Age.ply", "@ older\n\n: older ( Int -- Int )\n  dup drop\n");
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
        broken.write("src/Bad.ply", "@ bad\n\n: bad ( Int -- Int )\n  \"no\"\n");
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
    fn an_instance_word_comes_from_the_selected_module() {
        let temp = Temp::new("instance-home");
        temp.write(
            "package.json",
            &package_json(
                r#"
    "./A": { "ply": "./dist/A.d.ply", "import": "./dist/A.js" },
    "./B": { "ply": "./dist/B.d.ply", "import": "./dist/B.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        temp.write(
            "src/A.ply",
            "@ Eq\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqInt\n",
        );
        temp.write(
            "src/B.ply",
            "@ Eq, eq\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  drop drop false\n\n# Eq Int\n  eq = eqInt\n",
        );
        temp.write(
            "src/App.ply",
            "@ use\n~ \"B\" Eq, eq\n\n: use ( Int Int -- Bool )\n  eq\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(js.contains("from \"./B.js\""), "{js}");
        assert!(!js.contains("from \"./A.js\""), "{js}");
        node(
            &temp.0,
            "import { use } from \"./dist/App.js\";\nif (use(1n, 2n) !== false) throw new Error(\"home\");\n",
        );
    }

    #[test]
    fn build_exports_keeps_a_surrogate_and_replaces_the_last_exports() {
        let emoji = Temp::new("exports-emoji");
        emoji.write(
            "package.json",
            "{\n  \"name\": \"\\ud83d\\ude00\",\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 }\n}\n",
        );
        emoji.write("src/App.ply", "@ id\n\n: id ( Int -- Int )\n  dup drop\n");
        build_exports(&emoji.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let written = fs::read_to_string(emoji.0.join("package.json")).unwrap();
        assert!(written.contains("\\ud83d\\ude00"), "{written}");

        let last = Temp::new("exports-last");
        last.write(
            "package.json",
            "{\n  \"ply\": { \"source\": \"src\", \"out\": \"dist\", \"language\": 1 },\n  \"exports\": { \"./Old\": { \"ply\": \"./dist/Old.d.ply\", \"import\": \"./dist/Old.js\" } },\n  \"name\": \"shelf\",\n  \"exports\": { \"./Older\": { \"ply\": \"./dist/Older.d.ply\", \"import\": \"./dist/Older.js\" } }\n}\n",
        );
        last.write("src/App.ply", "@ id\n\n: id ( Int -- Int )\n  dup drop\n");
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
        temp.write(
            "src/Data/Num.ply",
            "@ Box, Mk\n\n; Box a = Mk one : a, two : a\n",
        );
        temp.write(
            "src/Data/Other.ply",
            "@ Box, wrap\n\n; Box a = Box a\n\n: wrap ( a. a -- (Box a) )\n  Box\n",
        );
        temp.write(
            "src/App.ply",
            "@ id\n~ \"Data/Num\" Mk\n~ \"Data/Other\" wrap\n\n: id ( Int -- Int )\n  dup drop\n",
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
        again.write(
            "src/Data/Num.ply",
            "@ Box, Mk\n\n; Box a = Mk one : a, two : a\n",
        );
        again.write(
            "src/App.ply",
            "@ pair\n~ \"Data/Num\" Box\n~ \"Data/Num\" Mk\n\n: pair ( a. a a -- (Box a) )\n  Mk\n",
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
        temp.write(
            "src/Data/Num.ply",
            "@ add\n\n: foreign add ( Int Int -- Int )\n",
        );
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
            "language 1\n\n@ add\n\n: foreign add ( (Int) (Int) -- Int )\n",
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
            "@ twice\n~ \"@algosail/a/Data/Num\" add\n\n: twice ( Int -- Int )\n  dup add\n",
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
        assert!(iface.contains("~ \"@algosail/a/Data/Num\" add"), "{iface}");
        assert!(iface.contains(": twice ( Int -- Int )"), "{iface}");
        assert!(!iface.contains("dup add"), "{iface}");
        node(
            &temp.0,
            "import { twice } from \"./dist/App.js\";\nif (twice(21n) !== 42n) throw new Error(String(twice(21n)));\n",
        );
    }

    #[test]
    fn an_alias_and_a_rename_import_under_node() {
        let temp = Temp::new("alias");
        temp.write(
            "package.json",
            &package_json(
                r#"
    "./Data/Bus": { "ply": "./dist/Data/Bus.d.ply", "import": "./dist/Data/Bus.js" },
    "./Alias": { "ply": "./dist/Alias.d.ply", "import": "./dist/Alias.js" },
    "./Rename": { "ply": "./dist/Rename.d.ply", "import": "./dist/Rename.js" },
    "./Again": { "ply": "./dist/Again.d.ply", "import": "./dist/Again.js" }
"#,
            ),
        );
        temp.write(
            "src/Data/Bus.ply",
            "@ send\n\n: send ( Int -- Int )\n  dup drop\n",
        );
        temp.write(
            "src/Alias.ply",
            "@ use\n~ \"Data/Bus\" : Bus\n\n: use ( Int -- Int )\n  Bus:send\n",
        );
        temp.write(
            "src/Rename.ply",
            "@ use\n~ \"Data/Bus\" send : busSend\n\n: use ( Int -- Int )\n  busSend\n",
        );
        temp.write(
            "src/Again.ply",
            "@ busSend\n~ \"Data/Bus\" send : busSend\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let alias = fs::read_to_string(temp.0.join("dist/Alias.js")).unwrap();
        assert!(
            alias.contains("import { send as Bus$send } from \"./Data/Bus.js\";"),
            "{alias}"
        );
        let alias_iface = fs::read_to_string(temp.0.join("dist/Alias.d.ply")).unwrap();
        assert!(alias_iface.contains("~ \"Data/Bus\" : Bus"), "{alias_iface}");
        let rename = fs::read_to_string(temp.0.join("dist/Rename.js")).unwrap();
        assert!(
            rename.contains("import { send as busSend } from \"./Data/Bus.js\";"),
            "{rename}"
        );
        let rename_iface = fs::read_to_string(temp.0.join("dist/Rename.d.ply")).unwrap();
        assert!(
            rename_iface.contains("~ \"Data/Bus\" send : busSend"),
            "{rename_iface}"
        );
        let again = fs::read_to_string(temp.0.join("dist/Again.js")).unwrap();
        assert!(
            again.contains("export { send as busSend } from \"./Data/Bus.js\";"),
            "{again}"
        );
        node(
            &temp.0,
            "import { use as aliased } from \"./dist/Alias.js\";\nimport { use as renamed } from \"./dist/Rename.js\";\nimport { busSend } from \"./dist/Again.js\";\nif (aliased(1n) !== 1n) throw new Error(String(aliased(1n)));\nif (renamed(2n) !== 2n) throw new Error(String(renamed(2n)));\nif (busSend(3n) !== 3n) throw new Error(String(busSend(3n)));\n",
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
            "language 1\n\n@ Mappable, map\n\n& Mappable f\n  map ( a b. (f a) (a -- b) -- (f b) )\n",
        );
        temp.write(
            "node_modules/@algosail/cls/dist/Applicable.d.ply",
            "language 1\n\n@ Applicable, apply\n\n~ \"Mappable\" Mappable\n\n& Mappable f => Applicable f\n  apply ( a b. (f a) (f (a -- b)) -- (f b) )\n",
        );
        temp.write(
            "node_modules/@algosail/cls/dist/Wrappable.d.ply",
            "language 1\n\n@ Wrappable, wrap\n\n~ \"Applicable\" Applicable\n\n& Applicable f => Wrappable f\n  wrap ( a -- (f a) )\n",
        );
        temp.write(
            "package.json",
            &package_json(
                r#""./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }"#,
            ),
        );
        temp.write(
            "src/App.ply",
            "@ use\n~ \"@algosail/cls/Wrappable\" Wrappable, wrap\n\n: use ( f a. Wrappable f => a -- (f a) )\n  wrap\n",
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
            "@ add\n\n.. Add two integers.\n: add ( (Int) (Int) -- Int )\n  iadd\n\n: foreign iadd ( Int Int -- Int )\n\n.. secret\n: hidden ( Int -- Int )\n  dup drop\n",
        );
        temp.write(
            "foreign/Data/Num.js",
            "export function iadd(a, b) { return a + b; }\n",
        );
        temp.write(
            "src/App.ply",
            "@ twice\n~ \"Data/Num\" add\n\n: twice ( Int -- Int )\n  dup add\n",
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
        assert!(!iface.contains("hidden"), "{iface}");
        assert!(!iface.contains(".. secret\n"), "{iface}");
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
        let line = source.lines().nth(4).unwrap();
        let found = crate::hover::hover(
            &source,
            &temp.0.join("src/App.ply"),
            4,
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
            "@ a\n~ \"Data/B\" b\n\n: a ( Int -- Int )\n  b\n",
        );
        cycle.write(
            "src/Data/B.ply",
            "@ b\n~ \"Data/A\" a\n\n: b ( Int -- Int )\n  a\n",
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
            "@ add\n\n: add ( (Int) (Int) -- Int )\n  drop\n",
        );
        missing.write(
            "src/App.ply",
            "@ twice\n~ \"Data/Num\" add\n\n: twice ( Int -- Int )\n  dup add\n",
        );
        let before = fs::read_to_string(missing.0.join("package.json")).unwrap();
        build(&missing.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert_eq!(fs::read_to_string(missing.0.join("package.json")).unwrap(), before);
        assert!(missing.0.join("dist/Data/Num.js").is_file());
        assert!(missing.0.join("dist/App.js").is_file());

        let renamed = Temp::new("name");
        renamed.write("package.json", &package_json(""));
        renamed.write("src/data/Num.ply", "@ add\n\n: add ( Int -- Int )\n  dup drop\n");
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
            "@ add\n\n: foreign add ( (Int) (Int) -- Int )\n",
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
        omitted.write(
            "src/Data/Num.ply",
            "@ add\n\n: foreign add ( Int Int -- Int )\n",
        );
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
        let eq = "@ Eq, eq, same\n\n: foreign ieq ( Int Int -- Bool )\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  ieq\n\n# Eq Int\n  eq = eqInt\n\n: same ( a. Eq a => a a -- Bool )\n  eq\n";
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
            "@ use\n~ \"Data/Eq\" same\n\n: use ( Int Int -- Bool )\n  same\n",
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
            "@ Eq, eq\n\n: foreign ieq ( Int Int -- Bool )\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  ieq\n\n# Eq Int\n  eq = eqInt\n",
        );
        via.write("foreign/Data/Eq.js", eq_js);
        via.write(
            "src/Data/Cmp.ply",
            "@ same\n~ \"Data/Eq\" Eq, eq\n\n: same ( a. Eq a => a a -- Bool )\n  eq\n",
        );
        via.write(
            "src/App.ply",
            "@ use\n~ \"Data/Cmp\" same\n\n: use ( Int Int -- Bool )\n  same\n",
        );
        build(&via.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(via.0.join("dist/App.js")).unwrap();
        assert!(js.contains("import { same } from \"./Data/Cmp.js\";"), "{js}");
        assert!(js.contains("import { eqInt } from \"./Data/Eq.js\";"), "{js}");
        node(
            &via.0,
            "import { use } from \"./dist/App.js\";\nif (use(1n, 1n) !== true) throw new Error(\"via\");\n",
        );
        via.write(
            "src/App.ply",
            "@ use\n~ \"Data/Cmp\" same\n~ \"Data/Eq\"\n\n: use ( Int Int -- Bool )\n  same\n",
        );
        build(&via.0).unwrap_or_else(|error| panic!("{}", error.render()));

        let bare = Temp::new("class-bare");
        bare.write("package.json", &package_json(exports));
        bare.write(
            "src/Data/Eq.ply",
            "@\n\n: foreign ieq ( Int Int -- Bool )\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  ieq\n\n# Eq Int\n  eq = eqInt\n",
        );
        bare.write("foreign/Data/Eq.js", eq_js);
        bare.write(
            "src/App.ply",
            "@ use\n~ \"Data/Eq\"\n\n: use ( Int Int -- Bool )\n  eq\n",
        );
        let error = build(&bare.0).unwrap_err();
        assert_eq!(error.message, "undefined word eq");

        let own = Temp::new("builtin-head");
        own.write("package.json", &package_json(exports));
        own.write("src/Data/Eq.ply", "@ Eq\n\n& Eq a\n  eq ( a a -- Bool )\n");
        own.write(
            "src/App.ply",
            "@\n~ \"Data/Eq\" Eq\n\n: eqBool ( Bool Bool -- Bool )\n  drop drop true\n\n# Eq Bool\n  eq = eqBool\n",
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
        orphan.write("src/Data/Eq.ply", "@ Eq\n\n& Eq a\n  eq ( a a -- Bool )\n");
        orphan.write("src/Data/Maybe.ply", "@ Maybe\n\n; Maybe a = Nothing | Just a\n");
        orphan.write(
            "src/App.ply",
            "@\n~ \"Data/Eq\" Eq\n~ \"Data/Maybe\" Maybe\n\n: eqMaybe ( a. (Maybe a) (Maybe a) -- Bool )\n  drop drop true\n\n# Eq (Maybe a)\n  eq = eqMaybe\n",
        );
        let error = build(&orphan.0).unwrap_err();
        assert_eq!(error.message, "orphan instance");

        let local = Temp::new("local-head");
        local.write("package.json", &package_json(exports));
        local.write("src/Data/Eq.ply", "@ Eq, eq\n\n& Eq a\n  eq ( a a -- Bool )\n");
        local.write(
            "src/App.ply",
            "@\n~ \"Data/Eq\" Eq, eq\n\n; Box a = Mk a\n\n: eqBox ( a. (Box a) (Box a) -- Bool )\n  drop drop true\n\n# Eq (Box a)\n  eq = eqBox\n\n: use ( (Box Int) (Box Int) -- Bool )\n  eq\n",
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
        both.write("src/Data/Eq.ply", "@ Eq\n\n& Eq a\n  eq ( a a -- Bool )\n");
        both.write("src/Data/Also.ply", "@ Also\n\n& Also a\n  eq ( a a -- Bool )\n");
        both.write(
            "src/App.ply",
            "@\n~ \"Data/Eq\" Eq\n~ \"Data/Also\" Also\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq, Also Int\n  eq = eqInt\n",
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
            "@ read, line\n\n: foreign read ( -- Str )\n\n: line ( -- Str )\n  read\n",
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
            "@ read, line\n\n: foreign read ( -- Str )\n\n: line ( -- Int )\n  read\n",
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
            "@ read, useSecret\n\n: foreign read ( -- Str )\n\n: foreign secret ( -- Str )\n\n: useSecret ( -- Str )\n  secret\n\n: hidden ( Str -- Str )\n  drop secret\n",
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
        assert!(iface.contains(": useSecret ( -- Str )"), "{iface}");
        assert!(!iface.contains("secret"), "{iface}");
        assert!(!iface.contains("hidden"), "{iface}");
        crate::parse(&iface, crate::ParseMode::Interface)
            .unwrap_or_else(|error| panic!("{}", error.render("Data/Io.d.ply", &iface)));
        node(
            &temp.0,
            "import { useSecret, read } from \"./dist/Data/Io.js\";\nif (read() !== \"hi\" || useSecret() !== \"sh\") throw new Error(\"io\");\n",
        );
        temp.write(
            "src/App.ply",
            "@ use\n~ \"Data/Io\" secret\n\n: use ( -- Str )\n  secret\n",
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
            "@\n\n: foreign touch ( -- Unit )\n\n: main ( -- Unit )\n  touch\n",
        );
        temp.write(
            "src/Tool.ply",
            "@ main\n\n: foreign touch ( -- Unit )\n\n: main ( -- Unit )\n  touch\n",
        );
        temp.write("src/Other.ply", "@\n\n: idle ( -- Unit )\n  Unit\n");
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
        assert!(!temp.0.join("dist/script.js").exists());

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
            "@ main\n\n: foreign boom ( -- Unit )\n\n: main ( -- Unit )\n  boom\n",
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
        temp.write("src/App.ply", "@\n\n: main ( . -- . Unit )\n  Unit\n");
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        temp.write("src/App.ply", "@\n\n: main ( ..s -- ..s Unit )\n  Unit\n");
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        temp.write("src/App.ply", "@\n\n: start ( -- Unit )\n  Unit\n");
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(!js.contains("import.meta.main"), "{js}");

        temp.write("src/App.ply", "@\n\n: main ( -- Num )\n  1\n");
        let error = check_package(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0115");
        assert_eq!(error.message, "word main must have scheme ( -- Unit )");
        assert!(error.render().contains("App.ply"), "{}", error.render());
        temp.write("src/App.ply", "@\n\n: main ( a. a -- a Unit )\n  Unit\n");
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

    #[test]
    fn public_import_publishes_the_word_and_the_instance() {
        let lib = "\
@ Eq, eq, Box, answer

.. Two numbers.
& Eq a
  eq ( a a -- Bool )

; Box a = Box a

.. Equal numbers.
: eqNum ( Num Num -- Bool )
  drop drop true

: eqBox ( a. (Box a) (Box a) -- Bool )
  drop drop true

# Eq Num
  eq = eqNum

# Eq (Box a)
  eq = eqBox

.. A number.
: answer ( -- Num )
  1

: hidden ( -- Num )
  0
";
        let exports = r#"
    "./Lib": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" },
    "./Door": { "ply": "./dist/Door.d.ply", "import": "./dist/Door.js" },
    "./Wide": { "ply": "./dist/Wide.d.ply", "import": "./dist/Wide.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" },
    "./UseAll": { "ply": "./dist/UseAll.d.ply", "import": "./dist/UseAll.js" }
"#;
        let temp = Temp::new("public");
        temp.write("package.json", &package_json(exports));
        temp.write("src/Lib.ply", lib);
        temp.write("src/Door.ply", "@ answer\n~ \"Lib\" answer\n");
        temp.write("src/Wide.ply", "@ answer\n~ \"Lib\" answer\n");
        temp.write(
            "src/App.ply",
            "@ same, use\n~ \"Door\" answer\n~ \"Lib\" Eq, eq\n\n: same ( a. Eq a => a a -- Bool )\n  eq\n\n: use ( Num Num -- Bool )\n  same\n",
        );
        temp.write(
            "src/UseAll.ply",
            "@ use\n~ \"Door\" answer\n\n: use ( -- Num )\n  answer\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let door = fs::read_to_string(temp.0.join("dist/Door.js")).unwrap();
        assert!(
            door.contains("export { answer } from \"./Lib.js\";"),
            "{door}"
        );
        assert!(!door.contains("eqNum"), "{door}");
        assert!(!door.contains("eqBox"), "{door}");
        assert!(!door.contains("function "), "{door}");
        let door_iface = fs::read_to_string(temp.0.join("dist/Door.d.ply")).unwrap();
        assert!(door_iface.contains("@ answer\n"), "{door_iface}");
        assert!(door_iface.contains("~ \"Lib\" answer\n"), "{door_iface}");
        assert!(!door_iface.contains("~ public"), "{door_iface}");
        assert!(!door_iface.contains("# Eq"), "{door_iface}");
        let app = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(app.contains("import { answer } from \"./Door.js\";"), "{app}");
        assert!(app.contains("import { eqNum } from \"./Lib.js\";"), "{app}");
        assert!(app.contains("eq: eqNum"), "{app}");
        let wide = fs::read_to_string(temp.0.join("dist/UseAll.js")).unwrap();
        assert!(wide.contains("answer") && wide.contains("from \"./Door.js\""), "{wide}");
        assert!(!wide.contains("eqNum"), "{wide}");
        node(
            &temp.0,
            "import { use } from \"./dist/App.js\";\nif (use(1, 2) !== true) throw new Error(\"eq\");\n",
        );

        let again = Temp::new("public-again");
        again.write(
            "package.json",
            &package_json(
                r#"
    "./Lib": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" },
    "./Door": { "ply": "./dist/Door.d.ply", "import": "./dist/Door.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        again.write("src/Lib.ply", lib);
        again.write("src/Door.ply", "@ answer\n~ \"Lib\" answer\n");
        again.write(
            "src/App.ply",
            "@ use\n~ \"Door\"\n~ \"Lib\"\n~ \"Lib\" Eq, eq\n\n: use ( Num Num -- Bool )\n  eq\n",
        );
        build(&again.0).unwrap_or_else(|error| panic!("{}", error.render()));

        let hidden = Temp::new("public-hidden");
        hidden.write(
            "package.json",
            &package_json(r#""./Lib": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" }, "./Door": { "ply": "./dist/Door.d.ply", "import": "./dist/Door.js" }"#),
        );
        hidden.write("src/Lib.ply", lib);
        hidden.write("src/Door.ply", "@ hidden\n~ \"Lib\" hidden\n");
        let error = build(&hidden.0).unwrap_err();
        assert_eq!(error.message, "~ \"Lib\" does not export hidden");

        let dup = Temp::new("public-dup");
        dup.write(
            "package.json",
            &package_json(r#""./Lib": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" }, "./Door": { "ply": "./dist/Door.d.ply", "import": "./dist/Door.js" }"#),
        );
        dup.write("src/Lib.ply", lib);
        dup.write(
            "src/Door.ply",
            "@ answer\n~ \"Lib\" answer\n\n: answer ( -- Num )\n  2\n",
        );
        let error = build(&dup.0).unwrap_err();
        assert_eq!(error.message, "duplicate definition answer");

        let clash = Temp::new("public-clash");
        clash.write(
            "package.json",
            &package_json(
                r#"
    "./Lib": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" },
    "./Door": { "ply": "./dist/Door.d.ply", "import": "./dist/Door.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        clash.write("src/Lib.ply", lib);
        clash.write("src/Door.ply", "@ Eq\n~ \"Lib\" Eq\n");
        clash.write(
            "src/App.ply",
            "@\n~ \"Door\" Eq\n\n: eqNum ( Num Num -- Bool )\n  drop drop false\n\n# Eq Num\n  eq = eqNum\n",
        );
        let error = build(&clash.0).unwrap_err();
        assert_eq!(error.message, "incoherent instance");

        let stopped = Temp::new("public-stopped");
        stopped.write(
            "package.json",
            &package_json(
                r#"
    "./Lib": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" },
    "./Door": { "ply": "./dist/Door.d.ply", "import": "./dist/Door.js" },
    "./Mid": { "ply": "./dist/Mid.d.ply", "import": "./dist/Mid.js" },
    "./Next": { "ply": "./dist/Next.d.ply", "import": "./dist/Next.js" }
"#,
            ),
        );
        stopped.write("src/Lib.ply", lib);
        stopped.write("src/Door.ply", "@ answer\n~ \"Lib\" answer\n");
        stopped.write(
            "src/Mid.ply",
            "@ pass, same\n~ \"Door\" answer\n~ \"Lib\" Eq, eq\n\n: pass ( Num Num -- Bool )\n  eq\n\n: same ( a. Eq a => a a -- Bool )\n  eq\n",
        );
        stopped.write(
            "src/Next.ply",
            "@ use\n~ \"Mid\" same\n\n: use ( Num Num -- Bool )\n  same\n",
        );
        build(&stopped.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let next = fs::read_to_string(stopped.0.join("dist/Next.js")).unwrap();
        assert!(next.contains("eqNum") && next.contains("from \"./Lib.js\""), "{next}");

        let both = Temp::new("public-both");
        both.write(
            "package.json",
            &package_json(
                r#"
    "./Lib": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" },
    "./Door": { "ply": "./dist/Door.d.ply", "import": "./dist/Door.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        both.write("src/Lib.ply", lib);
        both.write("src/Door.ply", "@ Box\n~ \"Lib\" Box\n");
        both.write(
            "src/App.ply",
            "@ id\n~ \"Door\" Box\n~ \"Lib\" Box\n\n: id ( a. (Box a) -- (Box a) )\n  dup drop\n",
        );
        let error = build(&both.0).unwrap_err();
        assert_eq!(error.message, "duplicate definition Box");
    }

    #[test]
    fn a_published_local_import_resolves_beside_exports() {
        let temp = Temp::new("sibling");
        temp.write(
            "node_modules/@algosail/door/package.json",
            r#"{
  "name": "@algosail/door",
  "type": "module",
  "ply": { "source": "src", "out": "dist", "language": 1 },
  "exports": {
    ".": { "ply": "./dist/Door.d.ply", "import": "./dist/Door.js" }
  }
}"#,
        );
        temp.write(
            "node_modules/@algosail/door/dist/Door.d.ply",
            "language 1\n\n@ answer\n\n~ \"Lib\" answer\n",
        );
        temp.write(
            "node_modules/@algosail/door/dist/Lib.d.ply",
            "language 1\n\n@ answer\n\n: answer ( -- Num )\n",
        );
        temp.write(
            "node_modules/@algosail/door/dist/Lib.js",
            "export function answer() { return 1; }\n",
        );
        temp.write(
            "node_modules/@algosail/door/dist/Door.js",
            "export { answer } from \"./Lib.js\";\n",
        );
        temp.write(
            "package.json",
            &package_json(r#""./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }"#),
        );
        temp.write(
            "src/App.ply",
            "@ use\n~ \"@algosail/door\" answer\n\n: use ( -- Num )\n  answer\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(js.contains("import { answer } from \"@algosail/door\";"), "{js}");
        node(
            &temp.0,
            "import { use } from \"./dist/App.js\";\nif (use() !== 1) throw new Error(String(use()));\n",
        );
        temp.write(
            "src/App.ply",
            "@ use\n~ \"@algosail/door/Lib\" answer\n\n: use ( -- Num )\n  answer\n",
        );
        let error = build(&temp.0).unwrap_err();
        assert_eq!(error.message, "cannot resolve ~ \"@algosail/door/Lib\"");
    }

    #[test]
    fn defaults_entry_and_a_kept_export_key() {
        let fresh = Temp::new("defaults");
        fresh.write(
            "package.json",
            "{\n  \"name\": \"fresh\",\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1 }\n}\n",
        );
        fresh.write("src/App.ply", "@ add\n\n: foreign add ( Num Num -- Num )\n");
        fresh.write(
            "foreign/App.js",
            "export function add(a, b) { return a + b; }\n",
        );
        build(&fresh.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let js = fs::read_to_string(fresh.0.join("dist/App.js")).unwrap();
        assert!(js.contains("foreign/App.js"), "{js}");
        assert_eq!(
            fs::read_to_string(fresh.0.join("package.json")).unwrap(),
            "{\n  \"name\": \"fresh\",\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1 }\n}\n"
        );

        let bare = Temp::new("no-foreign");
        bare.write(
            "package.json",
            "{\n  \"ply\": { \"language\": 1 }\n}\n",
        );
        bare.write("src/App.ply", "@ add\n\n: foreign add ( Num Num -- Num )\n");
        let error = build(&bare.0).unwrap_err();
        assert_eq!(error.code, "PL0107", "{}", error.message);

        let typed = Temp::new("entry-type");
        typed.write(
            "package.json",
            "{\n  \"ply\": { \"language\": 1, \"entry\": 1 }\n}\n",
        );
        fs::create_dir(typed.0.join("src")).unwrap();
        let error = build(&typed.0).unwrap_err();
        assert_eq!(error.code, "PL0123", "{}", error.message);

        let missing = Temp::new("entry-missing");
        missing.write(
            "package.json",
            "{\n  \"ply\": { \"language\": 1, \"entry\": \"Missing\" }\n}\n",
        );
        missing.write("src/App.ply", "@ id\n\n: id ( Num -- Num )\n  dup drop\n");
        let error = build(&missing.0).unwrap_err();
        assert_eq!(error.code, "PL0124", "{}", error.message);

        let aligned = Temp::new("entry-align");
        let original = "{\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1, \"entry\": \"App\" },\n  \"exports\": {\n    \"./keep\": \"./keep.js\",\n    \".\": { \"ply\": \"./dist/Old.d.ply\", \"import\": \"./dist/Old.js\" },\n    \"./Old\": { \"ply\": \"./dist/Old.d.ply\", \"import\": \"./dist/Old.js\" }\n  }\n}\n";
        aligned.write("package.json", original);
        aligned.write("src/App.ply", "@ id\n\n: id ( Num -- Num )\n  dup drop\n");
        build(&aligned.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert_eq!(fs::read_to_string(aligned.0.join("package.json")).unwrap(), original);
        build_exports(&aligned.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let written = fs::read_to_string(aligned.0.join("package.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&written).unwrap();
        let exports = value.get("exports").unwrap();
        assert_eq!(exports.get("./keep").and_then(|item| item.as_str()), Some("./keep.js"));
        assert_eq!(
            exports.get(".").and_then(|item| item.get("ply")).and_then(|item| item.as_str()),
            Some("./dist/App.d.ply"),
            "{written}"
        );
        assert_eq!(
            exports.get("./App").and_then(|item| item.get("ply")).and_then(|item| item.as_str()),
            Some("./dist/App.d.ply"),
            "{written}"
        );
        assert!(exports.get("./Old").is_none(), "{written}");

        let root = Temp::new("keep-dot");
        root.write(
            "package.json",
            "{\n  \"ply\": { \"language\": 1 },\n  \"exports\": {\n    \".\": { \"ply\": \"./dist/Stream.d.ply\", \"import\": \"./dist/Stream.js\" },\n    \"./Old\": { \"ply\": \"./dist/Old.d.ply\", \"import\": \"./dist/Old.js\" }\n  }\n}\n",
        );
        root.write("src/App.ply", "@ id\n\n: id ( Num -- Num )\n  dup drop\n");
        build_exports(&root.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let written = fs::read_to_string(root.0.join("package.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&written).unwrap();
        let exports = value.get("exports").unwrap();
        assert_eq!(
            exports.get(".").and_then(|item| item.get("ply")).and_then(|item| item.as_str()),
            Some("./dist/Stream.d.ply"),
            "{written}"
        );
        assert!(exports.get("./App").is_some(), "{written}");
        assert!(exports.get("./Old").is_none(), "{written}");
    }

    #[test]
    fn tests_import_helpers_and_source_cannot_use_a_dev_dependency() {
        let temp = Temp::new("helpers");
        temp.write(
            "package.json",
            "{\n  \"name\": \"app\",\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1 },\n  \"devDependencies\": { \"@demo/onlydev\": \"1.0.0\" }\n}\n",
        );
        temp.write(
            "node_modules/@demo/onlydev/package.json",
            "{\n  \"name\": \"@demo/onlydev\",\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1 },\n  \"exports\": {\n    \".\": { \"ply\": \"./dist/Door.d.ply\", \"import\": \"./dist/Door.js\" }\n  }\n}\n",
        );
        temp.write(
            "node_modules/@demo/onlydev/dist/Door.d.ply",
            "language 1\n\n@ answer\n\n: answer ( -- Num )\n",
        );
        temp.write(
            "node_modules/@demo/onlydev/dist/Door.js",
            "export function answer() { return 1; }\n",
        );
        temp.write("src/Lib.ply", "@ answer\n\n: answer ( -- Num )\n  1\n");
        temp.write(
            "test/Support/Session.ply",
            "@ twice\n~ \"Lib\" answer\n\n: twice ( -- Num )\n  answer\n",
        );
        let pass = "\
~ \"Support/Session\" twice
~ \"@demo/onlydev\" answer

; Verdict = Verdict ok : Bool, name : Str, text : Str

: yes ( -- Verdict )
  twice drop
  answer drop
  true \"\" \"\" Verdict
";
        temp.write("test/Pass.ply", pass);
        temp.write(
            "src/Bad.ply",
            "@ use\n~ \"@demo/onlydev\" answer\n\n: use ( -- Num )\n  answer\n",
        );
        let error = build(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0121", "{}", error.message);
        assert_eq!(error.message, "~ \"@demo/onlydev\" is a devDependency");
        fs::remove_file(temp.0.join("src/Bad.ply")).unwrap();
        temp.write(
            "src/Bad.ply",
            "@ use\n~ \"Support/Session\" twice\n\n: use ( -- Num )\n  twice\n",
        );
        let error = build(&temp.0).unwrap_err();
        assert_eq!(error.message, "cannot resolve ~ \"Support/Session\"");
        fs::remove_file(temp.0.join("src/Bad.ply")).unwrap();

        check_buffer(&temp.0.join("test/Pass.ply"), pass)
            .unwrap_or_else(|error| panic!("{}", error.render("test/Pass.ply", pass)));
        let blocked = "~ \"Support/Session\" twice\n\n: use ( -- Num )\n  twice\n";
        let error = check_buffer(&temp.0.join("src/Lib.ply"), blocked).unwrap_err();
        assert_eq!(error.message, "cannot resolve ~ \"Support/Session\"");

        let report = crate::test_run::test_package(&temp.0, "test/**/*.ply")
            .unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!report.failed(), "{report:?}");
        assert_eq!(report.files.len(), 1);

        let shadow = Temp::new("shadow");
        shadow.write(
            "package.json",
            "{\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1 }\n}\n",
        );
        shadow.write("src/Data/State.ply", "@ current\n\n: current ( -- Num )\n  1\n");
        shadow.write("test/Data/State.ply", "@ helper\n\n: helper ( -- Num )\n  2\n");
        shadow.write(
            "test/Pass.ply",
            "\
; Verdict = Verdict ok : Bool, name : Str, text : Str

: yes ( -- Verdict )
  true \"\" \"\" Verdict
",
        );
        let error = crate::test_run::test_package(&shadow.0, "test/**/*.ply").unwrap_err();
        assert_eq!(error.code, "PL0122", "{}", error.render());
        assert_eq!(error.message, "test module Data/State is already a source module");

        let entry = Temp::new("test-entry");
        entry.write(
            "package.json",
            "{\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1 }\n}\n",
        );
        entry.write("src/Data/State.ply", "@ current\n\n: current ( -- Num )\n  1\n");
        entry.write(
            "test/Data/State.ply",
            "\
~ \"Data/State\" current

; Verdict = Verdict ok : Bool, name : Str, text : Str

: reads ( -- Verdict )
  current drop
  true \"\" \"\" Verdict
",
        );
        let report = crate::test_run::test_package(&entry.0, "test/**/*.ply")
            .unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!report.failed(), "{report:?}");
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].checks[0].name, "reads");
        assert!(report.files[0].checks[0].ok);
    }

    #[test]
    fn an_unlisted_instance_word_is_still_called() {
        let temp = Temp::new("map-hidden");
        temp.write(
            "package.json",
            &package_json(
                r#"
    "./Classes/Mappable": { "ply": "./dist/Classes/Mappable.d.ply", "import": "./dist/Classes/Mappable.js" },
    "./Data/Stream": { "ply": "./dist/Data/Stream.d.ply", "import": "./dist/Data/Stream.js" },
    "./Stream": { "ply": "./dist/Stream.d.ply", "import": "./dist/Stream.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" }
"#,
            ),
        );
        temp.write(
            "src/Classes/Mappable.ply",
            "@ Mappable, map\n\n& Mappable f\n  map ( a b. (f a) (a -- b) -- (f b) )\n",
        );
        temp.write(
            "src/Data/Stream.ply",
            "@ Stream\n~ \"Classes/Mappable\" Mappable, map\n\n; Stream a = Stream a\n\n: mapStream ( a b. (Stream a) (a -- b) -- (Stream b) )\n  (Stream x) q ->\n    x q call\n    Stream\n\n# Mappable Stream\n  map = mapStream\n",
        );
        temp.write("src/Stream.ply", "@ Stream\n~ \"Data/Stream\" Stream\n");
        temp.write(
            "src/App.ply",
            "@ twice\n~ \"Stream\" Stream\n~ \"Classes/Mappable\" map\n\n: twice ( (Stream Num) -- (Stream Num) )\n  [ dup drop ] map\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let facade = fs::read_to_string(temp.0.join("dist/Stream.js")).unwrap();
        assert!(!facade.contains("mapStream"), "{facade}");
        let app = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(
            app.contains("import { mapStream } from \"./Data/Stream.js\";"),
            "{app}"
        );
        node(
            &temp.0,
            "import { twice } from \"./dist/App.js\";\nimport { Stream } from \"./dist/Data/Stream.js\";\nconst out = twice(Stream(1));\nif (out.value !== 1) throw new Error(JSON.stringify(out));\n",
        );
    }

    #[test]
    fn compile_writes_a_shaken_page_and_keeps_the_node_guard() {
        let lib = Temp::new("page-lib");
        lib.write(
            "package.json",
            r#"{
  "name": "@algosail/lib",
  "type": "module",
  "ply": { "source": "src", "foreign": "foreign", "out": "dist", "language": 1 },
  "exports": {
    ".": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" },
    "./Lib": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" }
  }
}
"#,
        );
        lib.write(
            "src/Lib.ply",
            "@ ping\n\n: foreign ping ( -- Unit )\n\n: foreign boom ( -- Unit )\n\n: main ( -- Unit )\n  boom\n",
        );
        let boom_mark = lib.0.join("boom.txt");
        let boom_lit = serde_json::to_string(&boom_mark.to_string_lossy().into_owned()).unwrap();
        lib.write(
            "foreign/Lib.js",
            &format!(
                "import {{\n  inner\n}} from \"./Inner.js\";\n\nexport class Marker {{}}\nlet n = 0;\nexport function ping() {{\n  inner();\n  n += 1;\n  if (!(new Marker() instanceof Marker)) throw new Error(\"class\");\n  if (n !== 1) throw new Error(String(n));\n}}\nexport function boom() {{\n  globalThis.process.getBuiltinModule(\"fs\").appendFileSync({boom_lit}, \"boom\\n\");\n}}\n"
            ),
        );
        lib.write("foreign/Inner.js", "export function inner() {}\n");
        build(&lib.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!lib.0.join("dist/script.js").exists());

        let app = Temp::new("page-app");
        let app_mark = app.0.join("app.txt");
        let app_lit = serde_json::to_string(&app_mark.to_string_lossy().into_owned()).unwrap();
        app.write(
            "package.json",
            r#"{
  "name": "@algosail/app",
  "type": "module",
  "dependencies": { "@algosail/lib": "0.0.0" },
  "ply": { "source": "src", "foreign": "foreign", "out": "dist", "language": 1, "entry": "App" },
  "exports": {}
}
"#,
        );
        app.write(
            "src/Face.ply",
            "@ ping\n~ \"@algosail/lib\" ping\n",
        );
        app.write(
            "src/App.ply",
            "@\n~ \"Face\" ping\n\n: foreign touch ( -- Unit )\n\n: spare ( -- Unit )\n  touch\n\n: main ( -- Unit )\n  ping drop\n  touch\n",
        );
        app.write(
            "foreign/App.js",
            &format!(
                "export function touch() {{\n  globalThis.process.getBuiltinModule(\"fs\").appendFileSync({app_lit}, \"ran\\n\");\n}}\n"
            ),
        );
        let link = app.0.join("node_modules/@algosail/lib");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&lib.0, &link).unwrap();

        check_package(&app.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!app.0.join("dist").exists());
        build(&app.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!app.0.join("dist/script.js").exists());
        compile(&app.0).unwrap_or_else(|error| panic!("{}", error.render()));

        let module = fs::read_to_string(app.0.join("dist/App.js")).unwrap();
        assert!(module.contains("if (plyIsEntry) main();"), "{module}");
        assert!(module.contains("function spare"), "{module}");
        let script = fs::read_to_string(app.0.join("dist/script.js")).unwrap();
        assert!(script.contains("class Marker"), "{script}");
        assert!(script.contains("function inner"), "{script}");
        assert!(script.contains("function ping"), "{script}");
        assert!(script.contains("function touch"), "{script}");
        assert!(!script.contains("function spare"), "{script}");
        assert!(!script.contains("function boom"), "{script}");
        assert!(!script.contains("boom.txt"), "{script}");
        assert!(script.trim_end().ends_with(".main();"), "{script}");
        assert!(!script.contains("from \"node:"), "{script}");
        assert!(!script.contains("import.meta"), "{script}");
        assert!(!script.contains("plyIsEntry"), "{script}");
        assert!(!script.contains("\nexport ") && !script.contains("\nimport "), "{script}");
        assert!(!app.0.join("dist/run.js").exists());

        let plain = Temp::new("page-plain");
        fs::write(plain.0.join("script.js"), &script).unwrap();
        let checked = Command::new("node")
            .arg("--check")
            .arg(plain.0.join("script.js"))
            .output()
            .unwrap();
        assert!(
            checked.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&checked.stderr)
        );

        let importer = app.0.join("use.mjs");
        fs::write(&importer, "import \"./dist/App.js\";\nimport { ping } from \"./dist/Face.js\";\nif (typeof ping !== \"function\") throw new Error(\"ping\");\n").unwrap();
        let imported = Command::new("node").arg(&importer).current_dir(&app.0).output().unwrap();
        assert!(
            imported.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&imported.stderr)
        );
        assert!(!app_mark.exists(), "import called main");
        assert!(!boom_mark.exists(), "import called the dependency main");

        let ran = Command::new("node")
            .arg(app.0.join("dist/script.js"))
            .current_dir(&app.0)
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(fs::read_to_string(&app_mark).unwrap(), "ran\n");
        assert!(!boom_mark.exists(), "page called the dependency main");

        fs::remove_file(&app_mark).unwrap();
        let module_run = Command::new("node")
            .arg(app.0.join("dist/App.js"))
            .output()
            .unwrap();
        assert!(
            module_run.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&module_run.stderr)
        );
        assert_eq!(fs::read_to_string(&app_mark).unwrap(), "ran\n");
        assert!(!boom_mark.exists());
    }

    #[test]
    fn compile_flags_choose_the_entry_and_the_script() {
        let temp = Temp::new("page-flags");
        let original = "{\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1, \"entry\": \"App\", \"foreign\": \"foreign\" }\n}\n";
        temp.write("package.json", original);
        let boom = temp.0.join("boom.txt");
        let ping = temp.0.join("ping.txt");
        let boom_lit = serde_json::to_string(&boom.to_string_lossy().into_owned()).unwrap();
        let ping_lit = serde_json::to_string(&ping.to_string_lossy().into_owned()).unwrap();
        temp.write(
            "src/App.ply",
            "@\n\n: foreign boom ( -- Unit )\n\n: spare ( -- Unit )\n  Unit\n\n: main ( -- Unit )\n  boom\n",
        );
        temp.write(
            "src/Data/Ping.ply",
            "@\n\n: foreign ping ( -- Unit )\n\n: spare ( -- Unit )\n  Unit\n\n: main ( -- Unit )\n  ping\n",
        );
        temp.write(
            "foreign/App.js",
            &format!(
                "export function boom() {{\n  globalThis.process.getBuiltinModule(\"fs\").appendFileSync({boom_lit}, \"boom\\n\");\n}}\n"
            ),
        );
        temp.write(
            "foreign/Data/Ping.js",
            &format!(
                "export function ping() {{\n  globalThis.process.getBuiltinModule(\"fs\").appendFileSync({ping_lit}, \"ping\\n\");\n}}\n"
            ),
        );
        compile_with(
            &temp.0,
            &CompileFlags {
                entry: Some("Data/Ping".to_string()),
                out: Some("page/Ping.js".to_string()),
                foreign: None,
            },
        )
        .unwrap_or_else(|error| panic!("{}", error.render()));
        assert_eq!(fs::read_to_string(temp.0.join("package.json")).unwrap(), original);
        assert!(!temp.0.join("dist/script.js").exists());
        assert!(temp.0.join("dist/App.js").is_file());
        assert!(temp.0.join("dist/Data/Ping.js").is_file());
        let script = fs::read_to_string(temp.0.join("page/Ping.js")).unwrap();
        assert!(script.contains("function ping"), "{script}");
        assert!(!script.contains("function boom"), "{script}");
        assert!(!script.contains("function spare"), "{script}");
        assert!(script.trim_end().ends_with(".main();"), "{script}");
        assert!(!script.contains("\nimport ") && !script.contains("\nexport "), "{script}");
        let ran = Command::new("node")
            .arg(temp.0.join("page/Ping.js"))
            .current_dir(&temp.0)
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(fs::read_to_string(&ping).unwrap(), "ping\n");
        assert!(!boom.exists());

        compile(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let page = fs::read_to_string(temp.0.join("dist/script.js")).unwrap();
        assert!(page.contains("function boom"), "{page}");
        assert!(!page.contains("function ping"), "{page}");
        assert_eq!(fs::read_to_string(temp.0.join("package.json")).unwrap(), original);

        let missing = compile_with(
            &temp.0,
            &CompileFlags {
                entry: Some("Missing".to_string()),
                ..CompileFlags::default()
            },
        )
        .unwrap_err();
        assert_eq!(missing.code, "PL0124", "{}", missing.render());
        let escaped = compile_with(
            &temp.0,
            &CompileFlags {
                out: Some("../page.js".to_string()),
                ..CompileFlags::default()
            },
        )
        .unwrap_err();
        assert_eq!(escaped.code, "PL0138", "{}", escaped.message);
        let absolute = compile_with(
            &temp.0,
            &CompileFlags {
                out: Some("/tmp/page.js".to_string()),
                ..CompileFlags::default()
            },
        )
        .unwrap_err();
        assert_eq!(absolute.code, "PL0138");
        let overlap = compile_with(
            &temp.0,
            &CompileFlags {
                foreign: Some("src".to_string()),
                ..CompileFlags::default()
            },
        )
        .unwrap_err();
        assert_eq!(overlap.code, "PL0106", "{}", overlap.message);
        assert_eq!(fs::read_to_string(temp.0.join("package.json")).unwrap(), original);
    }

    #[test]
    fn compile_roots_a_place_when_the_module_has_no_main() {
        let temp = Temp::new("page-place");
        let mark = temp.0.join("ran.txt");
        let lit = serde_json::to_string(&mark.to_string_lossy().into_owned()).unwrap();
        temp.write(
            "package.json",
            "{\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1, \"entry\": \"Isle\", \"foreign\": \"foreign\" }\n}\n",
        );
        temp.write(
            "src/Face.ply",
            "@ ping\n\n: foreign ping ( -- Unit )\n\n: hidden ( -- Unit )\n  Unit\n",
        );
        temp.write(
            "src/Isle.ply",
            "@\n~ \"Face\" ping\n\n: foreign boom ( -- Unit )\n\n: spare ( -- Unit )\n  boom\n\n= offered Unit\n  ping\n",
        );
        temp.write(
            "foreign/Face.js",
            &format!(
                "import {{ inner }} from \"./Inner.js\"\nexport function ping() {{\n  inner();\n  globalThis.process.getBuiltinModule(\"fs\").appendFileSync({lit}, \"ran\\n\");\n}}\n"
            ),
        );
        temp.write("foreign/Inner.js", "export function inner() {}\n");
        temp.write(
            "foreign/Isle.js",
            "export function boom() {}\n",
        );
        compile(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let script = fs::read_to_string(temp.0.join("dist/script.js")).unwrap();
        assert!(script.contains("function ping"), "{script}");
        assert!(script.contains("function inner"), "{script}");
        assert!(script.contains("ply$init$offered"), "{script}");
        assert!(!script.contains("function hidden"), "{script}");
        assert!(!script.contains("function spare"), "{script}");
        assert!(!script.contains("function boom"), "{script}");
        assert!(!script.contains(".main("), "{script}");
        assert!(!script.contains("from \"node:"), "{script}");
        assert!(!script.contains("\nimport ") && !script.contains("\nexport "), "{script}");
        let ran = Command::new("node")
            .arg(temp.0.join("dist/script.js"))
            .current_dir(&temp.0)
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(fs::read_to_string(&mark).unwrap(), "ran\n");

        temp.write(
            "src/Isle.ply",
            "@\n~ \"Face\" ping\n\n: main ( -- Unit )\n  Unit\n\n= offered Unit\n  ping\n",
        );
        fs::remove_file(&mark).unwrap();
        compile(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let both = fs::read_to_string(temp.0.join("dist/script.js")).unwrap();
        assert!(both.trim_end().ends_with(".main();"), "{both}");
        assert!(both.contains("ply$init$offered"), "{both}");
        assert!(both.contains("function ping"), "{both}");
        let again = Command::new("node")
            .arg(temp.0.join("dist/script.js"))
            .current_dir(&temp.0)
            .output()
            .unwrap();
        assert!(
            again.status.success(),
            "stderr:\n{}",
            String::from_utf8_lossy(&again.stderr)
        );
        assert_eq!(fs::read_to_string(&mark).unwrap(), "ran\n");
    }

    #[test]
    fn build_flags_override_one_run() {
        let temp = Temp::new("build-flags");
        let original = "{\n  \"type\": \"module\",\n  \"ply\": { \"language\": 1, \"entry\": \"App\", \"foreign\": \"foreign\" },\n  \"exports\": {\n    \"./keep\": \"./keep.js\"\n  }\n}\n";
        temp.write("package.json", original);
        temp.write(
            "src/App.ply",
            "@ add\n\n: foreign add ( Num Num -- Num )\n",
        );
        temp.write("src/Data/Num.ply", "@ id\n\n: id ( Num -- Num )\n  dup drop\n");
        temp.write("foreign/App.js", "export function other() { return 0; }\n");
        temp.write("host/App.js", "export function add(a, b) { return a + b; }\n");
        let missing = build(&temp.0).unwrap_err();
        assert_eq!(missing.code, "PL0108", "{}", missing.message);
        build_with(
            &temp.0,
            &BuildFlags {
                foreign: Some("host".to_string()),
                out: Some("built".to_string()),
                ..BuildFlags::default()
            },
        )
        .unwrap_or_else(|error| panic!("{}", error.render()));
        assert_eq!(fs::read_to_string(temp.0.join("package.json")).unwrap(), original);
        assert!(!temp.0.join("dist").exists());
        let js = fs::read_to_string(temp.0.join("built/App.js")).unwrap();
        assert!(js.contains("host/App.js"), "{js}");
        assert!(!js.contains("foreign/App.js"), "{js}");
        node(
            &temp.0,
            "import { add } from \"./built/App.js\";\nif (add(1, 2) !== 3) throw new Error(String(add(1, 2)));\n",
        );

        let same = build_with(
            &temp.0,
            &BuildFlags {
                out: Some("src".to_string()),
                ..BuildFlags::default()
            },
        )
        .unwrap_err();
        assert_eq!(same.code, "PL0104", "{}", same.message);
        let nested = build_with(
            &temp.0,
            &BuildFlags {
                out: Some("foreign".to_string()),
                ..BuildFlags::default()
            },
        )
        .unwrap_err();
        assert_eq!(nested.code, "PL0106", "{}", nested.message);
        let left = build_with(
            &temp.0,
            &BuildFlags {
                foreign: Some("../host".to_string()),
                ..BuildFlags::default()
            },
        )
        .unwrap_err();
        assert_eq!(left.code, "PL0138", "{}", left.message);
        assert_eq!(fs::read_to_string(temp.0.join("package.json")).unwrap(), original);

        build_with(
            &temp.0,
            &BuildFlags {
                entry: Some("Data/Num".to_string()),
                out: Some("built".to_string()),
                foreign: Some("host".to_string()),
                exports: true,
            },
        )
        .unwrap_or_else(|error| panic!("{}", error.render()));
        let written = fs::read_to_string(temp.0.join("package.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(
            value.get("ply").and_then(|ply| ply.get("entry")).and_then(|item| item.as_str()),
            Some("App"),
            "{written}"
        );
        assert_eq!(
            value.get("ply").and_then(|ply| ply.get("out")).and_then(|item| item.as_str()),
            None,
            "{written}"
        );
        assert_eq!(
            value.get("ply").and_then(|ply| ply.get("foreign")).and_then(|item| item.as_str()),
            Some("foreign"),
            "{written}"
        );
        let exports = value.get("exports").unwrap();
        assert_eq!(exports.get("./keep").and_then(|item| item.as_str()), Some("./keep.js"));
        assert_eq!(
            exports.get(".").and_then(|item| item.get("ply")).and_then(|item| item.as_str()),
            Some("./built/Data/Num.d.ply"),
            "{written}"
        );
        assert!(temp.0.join("built/Data/Num.js").is_file());
    }

    #[test]
    fn compile_rejects_a_bad_package() {
        let temp = Temp::new("page-bad");
        temp.write(
            "package.json",
            r#"{ "ply": { "language": 1, "target": "web" } }"#,
        );
        let error = check_package(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0127", "{}", error.message);
        assert_eq!(error.message, "package.json ply.target is not a field");
        temp.write(
            "package.json",
            r#"{ "ply": { "language": 1, "target": "node" } }"#,
        );
        let error = check_package(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0127");
        temp.write(
            "package.json",
            r#"{ "ply": { "language": 1, "target": 1 } }"#,
        );
        let error = check_package(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0127");

        temp.write("package.json", r#"{ "ply": { "language": 1 } }"#);
        temp.write("src/App.ply", "@\n\n: main ( -- Unit )\n  Unit\n");
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let error = compile(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0129", "{}", error.render());
        assert!(!temp.0.join("dist").exists());

        temp.write(
            "package.json",
            r#"{ "ply": { "language": 1, "entry": "App" } }"#,
        );
        temp.write("src/App.ply", "@\n\n: idle ( -- Unit )\n  Unit\n");
        check_package(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let error = compile(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0129", "{}", error.render());

        temp.write("src/App.ply", "@\n\n: main ( -- Num )\n  1\n");
        let error = check_package(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0115");
        let error = compile(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0115");

        temp.write(
            "src/App.ply",
            "@\n\n: foreign touch ( -- Unit )\n\n: main ( -- Unit )\n  touch\n",
        );
        temp.write(
            "foreign/App.js",
            "import { readFileSync } from \"node:fs\";\nexport function touch() { readFileSync(\"x\"); }\n",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        assert!(!temp.0.join("dist/script.js").exists());
        let error = compile(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0130", "{}", error.render());
        assert!(error.message.contains("node:fs"), "{}", error.message);
        assert!(!temp.0.join("dist/script.js").exists());

        temp.write("foreign/App.js", "export default function touch() {}\n");
        let error = compile(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0130", "{}", error.message);
        assert!(error.message.contains("export default"), "{}", error.message);

        temp.write(
            "foreign/App.js",
            "import { x } from \"left-pad\";\nexport function touch() { return x; }\n",
        );
        let error = compile(&temp.0).unwrap_err();
        assert_eq!(error.code, "PL0130", "{}", error.message);
        assert!(error.message.contains("left-pad"), "{}", error.message);
        assert!(!temp.0.join("dist/script.js").exists());
    }

    #[test]
    fn a_place_keeps_one_frame_stack_in_its_defining_module() {
        let exports = r#"
    "./Lib": { "ply": "./dist/Lib.d.ply", "import": "./dist/Lib.js" },
    "./Door": { "ply": "./dist/Door.d.ply", "import": "./dist/Door.js" },
    "./App": { "ply": "./dist/App.d.ply", "import": "./dist/App.js" },
    "./Floor": { "ply": "./dist/Floor.d.ply", "import": "./dist/Floor.js" },
    "./Use": { "ply": "./dist/Use.d.ply", "import": "./dist/Use.js" }
"#;
        let temp = Temp::new("place");
        temp.write("package.json", &package_json(exports));
        temp.write(
            "src/Lib.ply",
            "\
@ base

.. The current base.
= base Num
  10
",
        );
        temp.write("src/Door.ply", "@ base\n~ \"Lib\" base\n");
        temp.write(
            "src/App.ply",
            "\
@ read, store, inside

~ \"Lib\" base

: read ( -- Num )
  base

: store ( Num -- )
  =base

: inside ( Num -- Num )
  [ base ] =>base
",
        );
        temp.write(
            "src/Floor.ply",
            "\
@ read, set, around

~ \"Lib\" base : floor

: read ( -- Num )
  floor

: set ( Num -- )
  =floor

: around ( Num -- Num )
  [ floor ] =>floor
",
        );
        temp.write(
            "src/Use.ply",
            "\
@ around

~ \"Door\" base

: around ( Num -- Num )
  [ base ] =>base
",
        );
        build(&temp.0).unwrap_or_else(|error| panic!("{}", error.render()));
        let lib = fs::read_to_string(temp.0.join("dist/Lib.js")).unwrap();
        assert!(lib.contains("const ply$place$base = [];"), "{lib}");
        assert!(lib.contains("export function base()"), "{lib}");
        assert!(lib.contains("export function ply_set_base(value)"), "{lib}");
        assert!(lib.contains("export function ply_bind_base(value, quot)"), "{lib}");
        assert!(lib.contains("ply$place$base.push(value);"), "{lib}");
        assert!(lib.contains("ply$place$base.pop();"), "{lib}");
        assert!(lib.contains("function ply$init$base("), "{lib}");
        assert!(lib.contains("ply$place$base.push(ply$init$base());"), "{lib}");
        assert!(!lib.contains("export function ply$init$"), "{lib}");
        let iface = fs::read_to_string(temp.0.join("dist/Lib.d.ply")).unwrap();
        assert!(iface.contains("= base Num\n"), "{iface}");
        assert!(iface.contains(".. The current base."), "{iface}");
        assert!(!iface.contains("10"), "{iface}");
        let door = fs::read_to_string(temp.0.join("dist/Door.js")).unwrap();
        assert!(door.contains("export { base } from \"./Lib.js\";"), "{door}");
        assert!(!door.contains("ply_bind_base"), "{door}");
        let app = fs::read_to_string(temp.0.join("dist/App.js")).unwrap();
        assert!(app.contains("ply_set_base"), "{app}");
        assert!(app.contains("ply_bind_base"), "{app}");
        assert!(app.contains("from \"./Lib.js\""), "{app}");
        let floor = fs::read_to_string(temp.0.join("dist/Floor.js")).unwrap();
        assert!(floor.contains("base as floor"), "{floor}");
        assert!(floor.contains("ply_set_base as ply_set_floor"), "{floor}");
        assert!(floor.contains("ply_bind_base as ply_bind_floor"), "{floor}");
        assert!(floor.contains("from \"./Lib.js\""), "{floor}");
        let use_js = fs::read_to_string(temp.0.join("dist/Use.js")).unwrap();
        assert!(use_js.contains("from \"./Door.js\""), "{use_js}");
        assert!(use_js.contains("ply_bind_base"), "{use_js}");
        assert!(use_js.contains("from \"./Lib.js\""), "{use_js}");
        assert!(!use_js.contains("ply_bind_base } from \"./Door.js\""), "{use_js}");
        node(
            &temp.0,
            "\
import { base } from \"./dist/Lib.js\";
import { read, store, inside } from \"./dist/App.js\";
if (base() !== 10 || read() !== 10) throw new Error(\"init\");
if (inside(4) !== 4) throw new Error(\"inside\");
if (base() !== 10) throw new Error(\"inside leaked\");
store(7);
if (base() !== 7 || read() !== 7) throw new Error(\"store\");
",
        );
        node(
            &temp.0,
            "\
import { base } from \"./dist/Lib.js\";
import { read, set, around } from \"./dist/Floor.js\";
if (read() !== 10) throw new Error(\"floor\");
if (around(4) !== 4) throw new Error(\"around\");
if (base() !== 10) throw new Error(\"around leaked\");
set(6);
if (base() !== 6 || read() !== 6) throw new Error(\"set\");
",
        );
        node(
            &temp.0,
            "\
import { base } from \"./dist/Lib.js\";
import { around } from \"./dist/Use.js\";
if (around(5) !== 5) throw new Error(\"facade\");
if (base() !== 10) throw new Error(\"facade leaked\");
",
        );
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
