//! Stack checker. One snoc-list, Hindley–Milner unification, no subtyping.
//!
//! A signature's outer tail and its named tails are rigid, so a body cannot
//! consume below what the signature wrote. An implicit tail inside a nested
//! quotation stays flexible: `call` has to unify it with the stack under the
//! quotation. Word schemes instantiate to fresh variables.
//!
//! Effect labels are not part of the language. A quotation still carries an
//! empty effect row so function types stay uniform. Nothing joins that row,
//! and a program cannot name a label.

mod clauses;
mod lower;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::ast::{
    Class, EffectRow, Import, Instance, Item, Lit, Module, RecordTail, Scheme, Stack,
    StackEffect, StackTail, Synonym, Term, TermNode, Type,
};

use crate::builtin;
use crate::diagnostic::Diagnostic;
use crate::codes::Error;
use crate::parser::parse;
use crate::paths::ParseMode;
use crate::span::Span;
pub use lower::javascript;

pub fn check(module: &Module) -> Result<(), Diagnostic> {
    Checker::new().check_module(module)
}

/// One module in an import closure, with the file that declared it.
/// The same `origin` is the same nominal types.
pub(crate) struct KnownModule {
    pub origin: String,
    pub module: Module,
}

pub(crate) struct ResolvedImport<'a> {
    pub provider: &'a Module,
    pub import: &'a Import,
    /// `provider` plus every module its signatures can mention.
    pub known: &'a [KnownModule],
}

#[derive(Debug)]
pub(crate) struct CheckedModule {
    pub anf: crate::anf::AnfModule,
    pub foreign_arity: Vec<(String, usize)>,
}

/// Stack at one term, rendered after the whole definition has been checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackAt {
    pub span: Span,
    /// Concrete stack before the term. A hidden signature tail is `.` when nothing sits above it.
    pub before: String,
    /// Stack after the term. Absent when checking stopped on this term.
    pub after: Option<String>,
    /// Instantiated `input -- output` of a word at this use.
    pub here: Option<String>,
}

struct RawSite {
    span: Span,
    before: StackId,
    after: Option<StackId>,
    here: Option<(StackId, StackId, EffectId)>,
}

/// Check `module` after installing imports. Single-file `check` does not call this.
pub(crate) fn check_with_imports(
    module: &Module,
    imports: &[ResolvedImport<'_>],
) -> Result<CheckedModule, Diagnostic> {
    let mut checker = Checker::new();
    for imported in imports {
        checker.import_resolved(imported.provider, imported.import, imported.known)?;
    }
    checker.prepare(module)?;
    checker.check_bodies(module)?;
    let mut foreign_arity = Vec::new();
    for item in &module.items {
        if let Item::Def(def) = item {
            if def.vis.is_foreign() {
                if let Some(arity) = checker.concrete_input_len(&def.name) {
                    foreign_arity.push((def.name.clone(), arity));
                }
            }
        }
    }
    let anf = checker.lower_prepared(module)?;
    Ok(CheckedModule { anf, foreign_arity })
}

/// Sites from a single-file check. A body that fails still yields the terms reached before it.
pub(crate) fn collect_sites(module: &Module) -> Vec<StackAt> {
    let mut checker = Checker::new();
    let _ = checker.check_module(module);
    checker.render_sites()
}

pub(crate) fn collect_sites_with_imports(
    module: &Module,
    imports: &[ResolvedImport<'_>],
) -> Vec<StackAt> {
    let mut checker = Checker::new();
    for imported in imports {
        if checker
            .import_resolved(imported.provider, imported.import, imported.known)
            .is_err()
        {
            return Vec::new();
        }
    }
    if checker.prepare(module).is_err() {
        return Vec::new();
    }
    let _ = checker.check_bodies(module);
    checker.render_sites()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct TyId(u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct StackId(u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct RowId(u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct EffectId(u32);

#[derive(Clone, Debug)]
enum TyNode {
    /// Quantified placeholder in a scheme template. Instantiation replaces it.
    Template,
    Var,
    Rigid(String),
    Link(TyId),
    Ctor(String, Vec<TyId>),
    /// Application whose head is a variable, not a type constructor.
    App(TyId, Vec<TyId>),
    Fun(StackId, StackId, EffectId),
    /// Record row. Fields live on the row node; this id is that row.
    Record(RowId),
}

#[derive(Clone, Debug)]
enum StackNode {
    Template,
    Var,
    /// `hidden` tails come from an implicit signature tail and print as nothing
    /// when values sit above them.
    Rigid {
        name: String,
        hidden: bool,
    },
    Link(StackId),
    Empty,
    Cons(StackId, TyId),
}

/// A record tail. `Fields` is a flat extension; a variable is only a tail.
#[derive(Clone, Debug)]
enum RowNode {
    Template,
    Var,
    Rigid(String),
    Link(RowId),
    Fields {
        fields: Vec<(String, TyId)>,
        tail: Option<RowId>,
    },
}

/// An effect row. Labels have no types. A variable is only a tail.
#[derive(Clone, Debug)]
enum EffectNode {
    Template,
    Var,
    Rigid(String),
    Link(EffectId),
    Labels {
        labels: Vec<String>,
        tail: Option<EffectId>,
    },
}

#[derive(Clone, Debug)]
struct QuantTail {
    name: String,
    id: StackId,
    /// Rigid when this scheme is the signature being checked.
    skolem: bool,
}

#[derive(Clone, Debug)]
struct QuantRow {
    name: String,
    id: RowId,
    /// Rigid when this scheme is the signature being checked.
    skolem: bool,
}

#[derive(Clone, Debug)]
struct QuantEffect {
    name: String,
    id: EffectId,
    /// Rigid when this scheme is the signature being checked.
    skolem: bool,
}

#[derive(Clone, Debug)]
struct SemScheme {
    tys: Vec<(String, TyId)>,
    tails: Vec<QuantTail>,
    rows: Vec<QuantRow>,
    constraints: Vec<SemConstraint>,
    input: StackId,
    output: StackId,
    effect_vars: Vec<QuantEffect>,
    effect: EffectId,
}

#[derive(Clone, Debug)]
struct SemConstraint {
    class_name: String,
    args: Vec<TyId>,
}

/// A use whose instance head was still flexible. Solved after the result type is known.
struct DeferredUse {
    span: Span,
    constraints: Vec<SemConstraint>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct KindId(u32);

#[derive(Clone, Debug)]
enum KindNode {
    Var,
    Link(KindId),
    Type,
    Arrow(KindId, KindId),
}

/// Nodes already walked. A repeat is sharing or a cycle, not a fresh occurrence.
#[derive(Default)]
struct Seen {
    ty: HashSet<TyId>,
    stack: HashSet<StackId>,
    row: HashSet<RowId>,
    effect: HashSet<EffectId>,
    kind: HashSet<KindId>,
}

#[derive(Clone, Debug)]
struct ClassInfo {
    params: Vec<(String, TyId)>,
    methods: Vec<ClassMethod>,
    /// Immediate superclasses. Arguments use this class's parameter types.
    supers: Vec<SemConstraint>,
}

#[derive(Clone, Debug)]
struct ClassMethod {
    name: String,
    scheme: SemScheme,
    /// Concrete stack prefix of the method. Dictionary arguments are not included.
    arity: usize,
}

#[derive(Clone, Debug)]
struct InstanceInfo {
    class_name: String,
    vars: Vec<(String, TyId)>,
    tails: Vec<QuantTail>,
    rows: Vec<QuantRow>,
    args: Vec<TyId>,
    methods: Vec<InstanceMethod>,
    /// Superclass constraints at this head. Type variables are this instance's.
    supers: Vec<SemConstraint>,
}

#[derive(Clone, Debug)]
struct InstanceMethod {
    name: String,
    word: String,
    /// Dictionaries fixed by the instance head. The call site does not pass them.
    context: Vec<SemConstraint>,
    /// Method constraints after the class. The call site passes these dictionaries,
    /// because they mention the method's own variables rather than the head.
    extra: usize,
    arity: usize,
}

/// A solved constraint. `Assumption` is a dictionary parameter of the current def.
/// `Super` projects a superclass dictionary out of another dictionary.
#[derive(Clone, Debug)]
enum Dict {
    Assumption(usize),
    Super { base: Box<Dict>, field: String },
    Instance {
        methods: Vec<ResolvedMethod>,
        /// Immediate superclass dictionaries. The field name is the class.
        supers: Vec<(String, Dict)>,
    },
}

#[derive(Clone, Debug)]
struct ResolvedMethod {
    name: String,
    word: String,
    dicts: Vec<Dict>,
    extra: usize,
    arity: usize,
}

enum Edit {
    Ty(u32, TyNode),
    Stack(u32, StackNode),
    Row(u32, RowNode),
    Effect(u32, EffectNode),
}

#[derive(Clone, Debug)]
struct CtorInfo {
    name: String,
    fields: Vec<TyId>,
    /// Constructor word. Filled for every data type except builtin Unit.
    word: Option<SemScheme>,
}

#[derive(Clone, Debug)]
struct DataType {
    params: Vec<TyId>,
    ctors: Vec<CtorInfo>,
    newtype: bool,
    /// File that declared the type. Empty for a type in the module being checked.
    origin: String,
}

#[derive(Clone)]
struct SynBody {
    params: Vec<String>,
    rhs: Type,
}

struct Subst {
    ty: HashMap<TyId, TyId>,
    stack: HashMap<StackId, StackId>,
    row: HashMap<RowId, RowId>,
    effect: HashMap<EffectId, EffectId>,
}

struct Memo {
    ty: HashMap<TyId, TyId>,
    stack: HashMap<StackId, StackId>,
    row: HashMap<RowId, RowId>,
    effect: HashMap<EffectId, EffectId>,
}

#[derive(Clone)]
struct Scope {
    tys: HashMap<String, TyId>,
    tails: HashMap<String, StackId>,
    rows: HashMap<String, RowId>,
    effects: HashMap<String, EffectId>,
    /// One omitted `!` for the whole scheme, shared with nested quotation types.
    implicit_effect: Option<EffectId>,
    quantified_tys: Vec<(String, TyId)>,
    quantified_tails: Vec<QuantTail>,
    quantified_rows: Vec<QuantRow>,
    quantified_effects: Vec<QuantEffect>,
    /// Nested quotation types. Their implicit tails and effects are not skolems.
    nested: bool,
}

struct Checker {
    tys: Vec<TyNode>,
    stacks: Vec<StackNode>,
    rows: Vec<RowNode>,
    effects: Vec<EffectNode>,
    /// Effect row of the definition or quotation currently being checked.
    ambient: Option<EffectId>,
    empty: Option<StackId>,
    words: HashMap<String, SemScheme>,
    type_arity: HashMap<String, usize>,
    datas: HashMap<String, DataType>,
    /// Transparent synonyms. A body is visible only while its name is in
    /// `type_arity` or `extra_arity`.
    synonyms: HashMap<String, SynBody>,
    /// Synonym names currently being expanded. A repeat is a cycle.
    expanding: Vec<String>,
    builtin_words: HashSet<String>,
    builtin_types: HashSet<String>,
    installed: HashSet<String>,
    /// Type arities visible only while an imported signature is elaborated.
    extra_arity: HashMap<String, usize>,
    locals: Vec<HashMap<String, SemScheme>>,
    /// JS arity of a local that holds a function, recorded before its tail is extended.
    fn_arity: HashMap<u32, (usize, usize)>,
    next_local: u32,
    sites: Vec<RawSite>,
    pending_here: Option<(StackId, StackId, EffectId)>,
    kinds: Vec<KindNode>,
    type_kind: Option<KindId>,
    ty_kind: HashMap<TyId, KindId>,
    classes: HashMap<String, ClassInfo>,
    /// Class names that constraints in this module may mention.
    visible_classes: HashSet<String>,
    defined_classes: HashSet<String>,
    defined_data: HashSet<String>,
    instances: Vec<InstanceInfo>,
    assumptions: Vec<SemConstraint>,
    /// Uses whose head is fixed by the result, such as `empty` and `pure`.
    deferred: Vec<DeferredUse>,
    /// Solved form of [`deferred`], keyed by the use span. The second lower pass reads it.
    use_dicts: HashMap<Span, Vec<Dict>>,
    /// Parallel to [`assumptions`] while lowering. Empty during checking.
    assumption_locals: Vec<u32>,
    solving: Vec<String>,
    trail: Vec<Edit>,
    recording: bool,
    frozen_ty: HashSet<TyId>,
    frozen_stack: HashSet<StackId>,
    frozen_row: HashSet<RowId>,
    frozen_effect: HashSet<EffectId>,
    /// Nested effect variables of the signature currently being checked.
    /// A body that instantiates one has outgrown the annotation.
    guarded: Vec<EffectId>,
    /// Imported signatures may mention a class before it is visible here.
    relax_visibility: bool,
}

impl Checker {
    fn new() -> Self {
        let mut checker = Self {
            tys: Vec::new(),
            stacks: Vec::new(),
            rows: Vec::new(),
            effects: Vec::new(),
            ambient: None,
            empty: None,
            words: HashMap::new(),
            type_arity: HashMap::new(),
            datas: HashMap::new(),
            synonyms: HashMap::new(),
            expanding: Vec::new(),
            builtin_words: HashSet::new(),
            builtin_types: HashSet::new(),
            installed: HashSet::new(),
            extra_arity: HashMap::new(),
            locals: Vec::new(),
            fn_arity: HashMap::new(),
            next_local: 0,
            sites: Vec::new(),
            pending_here: None,
            kinds: Vec::new(),
            type_kind: None,
            ty_kind: HashMap::new(),
            classes: HashMap::new(),
            visible_classes: HashSet::new(),
            defined_classes: HashSet::new(),
            defined_data: HashSet::new(),
            instances: Vec::new(),
            assumptions: Vec::new(),
            deferred: Vec::new(),
            use_dicts: HashMap::new(),
            assumption_locals: Vec::new(),
            solving: Vec::new(),
            trail: Vec::new(),
            recording: false,
            frozen_ty: HashSet::new(),
            frozen_stack: HashSet::new(),
            frozen_row: HashSet::new(),
            frozen_effect: HashSet::new(),
            guarded: Vec::new(),
            relax_visibility: false,
        };
        checker.preload();
        checker
    }

    fn check_module(&mut self, module: &Module) -> Result<(), Diagnostic> {
        self.prepare(module)?;
        self.check_bodies(module)
    }

    fn prepare(&mut self, module: &Module) -> Result<(), Diagnostic> {
        for item in &module.items {
            match item {
                Item::Data(data) => {
                    if !self.builtin_types.contains(&data.name) {
                        self.type_arity.insert(data.name.clone(), data.params.len());
                    }
                }
                Item::Synonym(synonym) => self.register_synonym(synonym)?,
                Item::Def(_) | Item::Class(_) | Item::Instance(_) => {}
            }
        }
        self.check_synonym_bodies(module)?;
        for item in &module.items {
            match item {
                Item::Data(data) => {
                    self.defined_data.insert(data.name.clone());
                    self.install_data(data)?;
                }
                Item::Class(class) => self.register_class(class)?,
                Item::Def(def) => self.register_def(def)?,
                Item::Synonym(_) | Item::Instance(_) => {}
            }
        }
        for item in &module.items {
            if let Item::Instance(instance) = item {
                self.declare_instance(instance, module, true)?;
            }
        }
        Ok(())
    }

    fn check_bodies(&mut self, module: &Module) -> Result<(), Diagnostic> {
        for item in &module.items {
            if let Item::Def(def) = item {
                if !def.body.is_empty() || !def.clauses.is_empty() {
                    self.check_body(def)?;
                }
            }
        }
        Ok(())
    }

    fn preload(&mut self) {
        for ty in builtin::TYPES {
            self.type_arity.insert(ty.name.to_string(), ty.arity);
            self.builtin_types.insert(ty.name.to_string());
        }
        for word in builtin::WORDS {
            self.load_aliases(&[word.name], word.scheme);
        }
        self.install_unit();
    }

    fn install_unit(&mut self) {
        self.datas.insert(
            "Unit".to_string(),
            DataType {
                params: Vec::new(),
                ctors: vec![CtorInfo {
                    name: "Unit".to_string(),
                    fields: Vec::new(),
                    word: None,
                }],
                newtype: false,
                origin: "*builtin*".to_string(),
            },
        );
        self.installed.insert("Unit".to_string());
    }

    fn load_aliases(&mut self, names: &[&str], signature: &str) {
        let scheme = self
            .elaborate_scheme(&parsed_scheme(signature), Span::new(0, 0))
            .unwrap_or_else(|error| panic!("builtin `{signature}`: {}", error.message));
        for name in names {
            self.builtin_words.insert((*name).to_string());
            self.words.insert((*name).to_string(), scheme.clone());
        }
    }

    fn install_data(&mut self, data: &crate::ast::Data) -> Result<(), Diagnostic> {
        self.add_data(data, None, "", data.name_span)
    }

    /// `only` selects constructor words. `None` exports every constructor and
    /// rejects a data type that is already staged. An empty set stages the
    /// type for patterns without putting its constructors in scope.
    /// `origin` is the declaring file. A second file with the same name is a
    /// different type, so it is not the one already in `datas`.
    fn add_data(
        &mut self,
        data: &crate::ast::Data,
        only: Option<&HashSet<String>>,
        origin: &str,
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self.builtin_types.contains(&data.name) {
            return Err(self.error(span, Error::CannotRedefineBuiltin(data.name.to_string())));
        }
        if let Some(stored) = self.datas.get(&data.name) {
            if stored.origin != origin || only.is_none() {
                return Err(self.error(span, Error::DuplicateDefinition(data.name.to_string())));
            }
            return self.export_ctors(&data.name, only, span);
        }
        if !self.installed.insert(data.name.clone()) {
            return Err(self.error(
                data.name_span,
                Error::DuplicateDefinition(data.name.to_string()),
            ));
        }
        let mut param_scope = Scope::new();
        for param in &data.params {
            if param_scope.tys.contains_key(param) {
                return Err(self.error(data.name_span, Error::DuplicateTypeVariable(param.to_string())));
            }
            let kind = self.kind_type();
            let id = self.ty_of_kind(kind);
            param_scope.tys.insert(param.clone(), id);
            param_scope.quantified_tys.push((param.clone(), id));
        }
        let params: Vec<TyId> = param_scope
            .quantified_tys
            .iter()
            .map(|(_, id)| *id)
            .collect();
        let mut ctors = Vec::new();
        for ctor in &data.ctors {
            let mut scope = Scope {
                tys: param_scope.tys.clone(),
                tails: HashMap::new(),
                rows: HashMap::new(),
                effects: HashMap::new(),
                implicit_effect: None,
                quantified_tys: param_scope.quantified_tys.clone(),
                quantified_tails: Vec::new(),
                quantified_rows: Vec::new(),
                quantified_effects: Vec::new(),
                nested: false,
            };
            let mut fields = Vec::new();
            for field in &ctor.fields {
                fields.push(self.elab_type(field, &mut scope, ctor.span)?);
            }
            let tail = self.fresh_tail(&mut scope, "", false);
            let effect = self.fresh_effect(&mut scope, "", true);
            let mut input = tail;
            for field in &fields {
                input = self.stack_cons(input, *field);
            }
            let result = self.ty_ctor(&data.name, params.clone());
            let output = self.stack_cons(tail, result);
            let scheme = SemScheme {
                tys: scope.quantified_tys,
                tails: scope.quantified_tails,
                rows: scope.quantified_rows,
                constraints: Vec::new(),
                input,
                output,
                effect_vars: scope.quantified_effects,
                effect,
            };
            let export = match only {
                None => true,
                Some(names) => names.contains(&ctor.name),
            };
            if export {
                let word_span = if only.is_none() { ctor.span } else { span };
                self.export_word(&ctor.name, scheme.clone(), word_span)?;
            }
            ctors.push(CtorInfo {
                name: ctor.name.clone(),
                fields,
                word: Some(scheme),
            });
        }
        self.datas.insert(
            data.name.clone(),
            DataType {
                params,
                ctors,
                newtype: data.newtype,
                origin: origin.to_string(),
            },
        );
        Ok(())
    }

    fn register_synonym(&mut self, synonym: &Synonym) -> Result<(), Diagnostic> {
        if self.builtin_types.contains(&synonym.name) {
            return Err(self.error(
                synonym.name_span,
                Error::CannotRedefineBuiltin(synonym.name.to_string()),
            ));
        }
        if self.type_arity.contains_key(&synonym.name) {
            return Err(self.error(
                synonym.name_span,
                Error::DuplicateDefinition(synonym.name.to_string()),
            ));
        }
        self.type_arity
            .insert(synonym.name.clone(), synonym.params.len());
        self.synonyms
            .insert(synonym.name.clone(), SynBody::from(synonym));
        Ok(())
    }

    /// Every synonym is registered first, so a right-hand side may name another.
    fn check_synonym_bodies(&mut self, module: &Module) -> Result<(), Diagnostic> {
        let names: Vec<(String, usize, Span)> = module
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Synonym(synonym) => {
                    Some((synonym.name.clone(), synonym.params.len(), synonym.name_span))
                }
                _ => None,
            })
            .collect();
        for (name, count, span) in names {
            let mut args = Vec::with_capacity(count);
            for _ in 0..count {
                let kind = self.kind_type();
                args.push(self.ty_of_kind(kind));
            }
            let (ty, kind) = self.expand_synonym(&name, &args, span)?;
            self.demand_type(ty, kind, span)?;
        }
        Ok(())
    }

    fn export_ctors(
        &mut self,
        data_name: &str,
        only: Option<&HashSet<String>>,
        span: Span,
    ) -> Result<(), Diagnostic> {
        let stored = self
            .datas
            .get(data_name)
            .cloned()
            .expect("data was staged");
        for ctor in &stored.ctors {
            let export = match only {
                None => true,
                Some(names) => names.contains(&ctor.name),
            };
            if !export {
                continue;
            }
            let Some(scheme) = ctor.word.clone() else {
                continue;
            };
            self.export_word(&ctor.name, scheme, span)?;
        }
        Ok(())
    }

    fn export_ctors_named(
        &mut self,
        provider: &Module,
        name: &str,
        names: &HashSet<String>,
        span: Span,
        origin: &str,
    ) -> Result<(), Diagnostic> {
        let data = data_of_ctor(provider, name).expect("constructor was found");
        if let Some(stored) = self.datas.get(&data.name) {
            if stored.origin != origin {
                return Err(self.error(span, Error::DuplicateDefinition(data.name.to_string())));
            }
            self.export_ctors(&data.name, Some(names), span)
        } else {
            self.add_data(data, Some(names), origin, span)
        }
    }

    fn export_word(&mut self, name: &str, scheme: SemScheme, span: Span) -> Result<(), Diagnostic> {
        if self.builtin_words.contains(name) {
            return Err(self.error(span, Error::CannotRedefineBuiltin(name.to_string())));
        }
        if self.words.contains_key(name) {
            return Err(self.error(span, Error::DuplicateDefinition(name.to_string())));
        }
        self.words.insert(name.to_string(), scheme);
        Ok(())
    }

    /// Bring the selected public names of `provider` into scope.
    /// `known` includes `provider` and the modules its signatures mention.
    pub(crate) fn import_resolved(
        &mut self,
        provider: &Module,
        import: &Import,
        known: &[KnownModule],
    ) -> Result<(), Diagnostic> {
        let saved = std::mem::take(&mut self.extra_arity);
        for known_module in known {
            for item in &known_module.module.items {
                match item {
                    Item::Data(data) => {
                        self.extra_arity
                            .entry(data.name.clone())
                            .or_insert(data.params.len());
                    }
                    Item::Synonym(synonym) => {
                        self.extra_arity
                            .entry(synonym.name.clone())
                            .or_insert(synonym.params.len());
                    }
                    Item::Def(_) | Item::Class(_) | Item::Instance(_) => {}
                }
            }
        }
        let result = self.import_with_extra(provider, import, known);
        self.extra_arity = saved;
        result
    }

    fn import_with_extra(
        &mut self,
        provider: &Module,
        import: &Import,
        known: &[KnownModule],
    ) -> Result<(), Diagnostic> {
        self.stash_known_synonyms(known);
        let none = HashSet::new();
        for known_module in known {
            for item in &known_module.module.items {
                if let Item::Data(data) = item {
                    self.add_data(data, Some(&none), &known_module.origin, import.span)?;
                }
            }
        }
        // A superclass module is installed before the class that extends it.
        // Within one module, source order still applies.
        for module in dependency_order(known) {
            for item in &module.items {
                if let Item::Class(class) = item {
                    self.ensure_class(class)?;
                }
            }
        }
        let names = if import.all {
            public_names(provider)
        } else {
            import.names.clone()
        };
        let origin = known
            .first()
            .map(|known_module| known_module.origin.as_str())
            .unwrap_or("");
        for name in &names {
            self.export_imported(provider, name, import, origin)?;
        }
        self.import_instances(provider)?;
        Ok(())
    }

    fn export_imported(
        &mut self,
        provider: &Module,
        name: &str,
        import: &Import,
        origin: &str,
    ) -> Result<(), Diagnostic> {
        let mut found = false;
        if let Some(data) = find_data(provider, name) {
            self.publish_type(data, import.span)?;
            found = true;
        }
        if let Some(synonym) = find_synonym(provider, name) {
            self.publish_synonym(synonym, import.span)?;
            found = true;
        }
        if data_of_ctor(provider, name).is_some() {
            let set = HashSet::from([name.to_string()]);
            self.export_ctors_named(provider, name, &set, import.span, origin)?;
            found = true;
        }
        if let Some(def) = find_def(provider, name) {
            if !def.vis.is_private() {
                let mut def = def.clone();
                def.name_span = import.span;
                let saved = self.relax_visibility;
                self.relax_visibility = true;
                let registered = self.register_def(&def);
                self.relax_visibility = saved;
                registered?;
                found = true;
            }
        }
        if find_class_item(provider, name).is_some() {
            self.visible_classes.insert(name.to_string());
            found = true;
        }
        if self.export_imported_method(provider, name, import.span)? {
            found = true;
        }
        if !found {
            return Err(self.error(
                import.span,
                Error::ImportNotExported { path: import.path.to_string(), name: name.to_string() },
            ));
        }
        Ok(())
    }

    fn publish_type(&mut self, data: &crate::ast::Data, span: Span) -> Result<(), Diagnostic> {
        if self.builtin_types.contains(&data.name) {
            return Err(self.error(span, Error::CannotRedefineBuiltin(data.name.to_string())));
        }
        if self.type_arity.contains_key(&data.name) {
            return Err(self.error(span, Error::DuplicateDefinition(data.name.to_string())));
        }
        self.type_arity.insert(data.name.clone(), data.params.len());
        Ok(())
    }

    fn publish_synonym(&mut self, synonym: &Synonym, span: Span) -> Result<(), Diagnostic> {
        if self.builtin_types.contains(&synonym.name) {
            return Err(self.error(span, Error::CannotRedefineBuiltin(synonym.name.to_string())));
        }
        if self.type_arity.contains_key(&synonym.name) {
            return Err(self.error(span, Error::DuplicateDefinition(synonym.name.to_string())));
        }
        self.type_arity
            .insert(synonym.name.clone(), synonym.params.len());
        self.synonyms
            .entry(synonym.name.clone())
            .or_insert_with(|| SynBody::from(synonym));
        Ok(())
    }

    fn stash_known_synonyms(&mut self, known: &[KnownModule]) {
        for known_module in known {
            for item in &known_module.module.items {
                if let Item::Synonym(synonym) = item {
                    self.synonyms
                        .entry(synonym.name.clone())
                        .or_insert_with(|| SynBody::from(synonym));
                }
            }
        }
    }

    fn syn_in_scope(&self, name: &str) -> bool {
        self.synonyms.contains_key(name)
            && (self.type_arity.contains_key(name) || self.extra_arity.contains_key(name))
    }

    /// Elaborate a saturated synonym. The caller has already checked the arity.
    fn expand_synonym(
        &mut self,
        name: &str,
        args: &[TyId],
        span: Span,
    ) -> Result<(TyId, KindId), Diagnostic> {
        if self.expanding.iter().any(|have| have == name) {
            return Err(self.error(span, Error::CyclicType(name.to_string())));
        }
        let body = self
            .synonyms
            .get(name)
            .cloned()
            .expect("synonym is in scope");
        self.expanding.push(name.to_string());
        let mut scope = Scope::new();
        for (param, arg) in body.params.iter().zip(args) {
            scope.tys.insert(param.clone(), *arg);
            if !self.ty_kind.contains_key(arg) {
                let kind = self.kind_type();
                self.ty_kind.insert(*arg, kind);
            }
        }
        let elaborated = self.elab_ty(&body.rhs, &mut scope, span);
        self.expanding.pop();
        elaborated
    }

    fn register_def(&mut self, def: &crate::ast::Def) -> Result<(), Diagnostic> {
        if self.builtin_words.contains(&def.name) {
            return Err(self.error(
                def.name_span,
                Error::CannotRedefineBuiltin(def.name.to_string()),
            ));
        }
        if self.words.contains_key(&def.name) {
            return Err(self.error(def.name_span, Error::DuplicateDefinition(def.name.to_string())));
        }
        let scheme = self.elaborate_scheme(&def.scheme, def.name_span)?;
        self.words.insert(def.name.clone(), scheme);
        Ok(())
    }

    fn check_body(&mut self, def: &crate::ast::Def) -> Result<(), Diagnostic> {
        let scheme = self.lookup(&def.name).expect("def was registered");
        let (input, output, effect, constraints) = self.instantiate_scheme(&scheme, true);
        let saved = self.ambient.replace(effect);
        self.assumptions = constraints;
        self.deferred.clear();
        self.locals.push(HashMap::new());
        let checked = if def.clauses.is_empty() {
            self.check_terms(&def.body, input)
        } else {
            self.check_clauses(def, input)
        };
        self.locals.pop();
        let stack = match checked {
            Ok(stack) => stack,
            Err(error) => {
                self.deferred.clear();
                self.ambient = saved;
                self.assumptions.clear();
                return Err(error);
            }
        };
        if let Err(message) = self.unify_stack(stack, output) {
            self.deferred.clear();
            self.ambient = saved;
            self.assumptions.clear();
            return Err(self.error(def.name_span, message));
        }
        let finished = self.finish_deferred();
        self.ambient = saved;
        self.assumptions.clear();
        finished?;
        self.escaped_effect(def.name_span)
    }

    fn check_terms(&mut self, terms: &[Term], mut stack: StackId) -> Result<StackId, Diagnostic> {
        for term in terms {
            stack = self.check_term(term, stack)?;
        }
        Ok(stack)
    }

    fn check_term(&mut self, term: &Term, stack: StackId) -> Result<StackId, Diagnostic> {
        self.pending_here = None;
        let result = match &term.node {
            TermNode::Word(name) => self.apply_word(term.span, name, stack),
            TermNode::Lit(lit) => {
                let ty = self.lit_ty(lit);
                Ok(self.stack_cons(stack, ty))
            }
            TermNode::Quote(terms) => {
                let fun = self.check_quote(terms)?;
                Ok(self.stack_cons(stack, fun))
            }
            TermNode::Field(name) => self.project_field(term.span, name, stack),
            TermNode::FieldSet(name) => self.set_field(term.span, name, stack),
            TermNode::FieldInsert(name) => self.insert_field(term.span, name, stack),
            TermNode::Array(elements) => {
                let ty = self.check_array(term.span, elements)?;
                Ok(self.stack_cons(stack, ty))
            }
            TermNode::Record(fields) => {
                let ty = self.check_record(term.span, fields)?;
                Ok(self.stack_cons(stack, ty))
            }
            TermNode::Bind(names) => self.check_bind(term.span, names, stack),
        };
        let (after, here) = match &result {
            Ok(after) => (Some(*after), self.pending_here.take()),
            Err(_) => (None, None),
        };
        self.sites.push(RawSite {
            span: term.span,
            before: stack,
            after,
            here,
        });
        result
    }

    fn check_quote(&mut self, terms: &[Term]) -> Result<TyId, Diagnostic> {
        let local = self.effect_var();
        let saved = self.ambient.replace(local);
        let input = self.stack_var();
        self.locals.push(HashMap::new());
        let checked = self.check_terms(terms, input);
        self.locals.pop();
        self.ambient = saved;
        let output = checked?;
        Ok(self.ty_fun(input, output, local))
    }

    /// Pop one stack slot. An open tail becomes one fresh value over a fresh tail.
    fn pop_slot(&mut self, stack: StackId, span: Span) -> Result<(StackId, TyId), Diagnostic> {
        let stack = self.find_stack(stack);
        match self.stacks[stack.0 as usize].clone() {
            StackNode::Cons(rest, ty) => Ok((rest, ty)),
            StackNode::Var => {
                let ty = self.ty_var();
                let tail = self.stack_var();
                let cons = self.stack_cons(tail, ty);
                self.stacks[stack.0 as usize] = StackNode::Link(cons);
                Ok((tail, ty))
            }
            StackNode::Empty | StackNode::Rigid { .. } => Err(self.error(span, Error::StackUnderflow)),
            other => panic!("bind on {other:?}"),
        }
    }

    /// Quotation header `x y ->` names the deepest value `x` and the top `y`.
    fn check_bind(
        &mut self,
        span: Span,
        names: &[String],
        stack: StackId,
    ) -> Result<StackId, Diagnostic> {
        let frame = self.locals.len() - 1;
        for name in names {
            if self.locals[frame].contains_key(name) {
                return Err(self.error(span, Error::DuplicateDefinition(name.to_string())));
            }
        }
        let mut current = stack;
        let mut schemes = Vec::with_capacity(names.len());
        for _ in names.iter().rev() {
            let (rest, ty) = self.pop_slot(current, span)?;
            schemes.push(self.binding_scheme(ty));
            current = rest;
        }
        schemes.reverse();
        for (name, scheme) in names.iter().zip(schemes) {
            self.locals[frame].insert(name.clone(), scheme);
        }
        Ok(current)
    }

    fn apply_word(
        &mut self,
        span: Span,
        name: &str,
        stack: StackId,
    ) -> Result<StackId, Diagnostic> {
        let Some(scheme) = self.lookup(name) else {
            return Err(self.error(span, Error::UndefinedWord(name.to_string())));
        };
        let (input, output, effect, constraints) = self.instantiate_scheme(&scheme, false);
        self.unify_stack(stack, input)
            .map_err(|message| self.error(span, message))?;
        self.join_effect(effect, span)?;
        self.solve_or_defer(&constraints, span)?;
        if name == "call" {
            self.known_arity(input, span)?;
        }
        self.pending_here = Some((input, output, effect));
        Ok(output)
    }

    /// `call` needs a finite prefix whose elements are solved types.
    /// A naked tail has length 0.
    fn known_arity(&self, call_input: StackId, span: Span) -> Result<(), Diagnostic> {
        let id = self.find_stack(call_input);
        let StackNode::Cons(_, ty) = self.stacks[id.0 as usize].clone() else {
            return Ok(());
        };
        let ty = self.find_ty(ty);
        let TyNode::Fun(input, _, _) = self.tys[ty.0 as usize].clone() else {
            return Ok(());
        };
        let mut current = self.find_stack(input);
        loop {
            match self.stacks[current.0 as usize].clone() {
                StackNode::Cons(rest, item) => {
                    if !self.solved(item) {
                        return Err(self.error(span, Error::QuotationArityUnknown));
                    }
                    current = self.find_stack(rest);
                }
                _ => return Ok(()),
            }
        }
    }

    fn solved(&self, ty: TyId) -> bool {
        !matches!(
            &self.tys[self.find_ty(ty).0 as usize],
            TyNode::Var | TyNode::Template
        )
    }

    fn instantiate_fields(&mut self, params: &[TyId], args: &[TyId], fields: &[TyId]) -> Vec<TyId> {
        let mut subst = Subst::default();
        for (param, arg) in params.iter().zip(args) {
            subst.ty.insert(*param, *arg);
        }
        let mut memo = Memo::default();
        fields
            .iter()
            .map(|field| self.copy_ty(*field, &subst, &mut memo))
            .collect()
    }

    /// A pattern name pushes its field. Only the tail is quantified, so the
    /// field type stays the scrutinee's type.
    fn binding_scheme(&mut self, field: TyId) -> SemScheme {
        let tail = self.push_stack(StackNode::Template);
        let output = self.stack_cons(tail, field);
        let effect = self.effect_var();
        SemScheme {
            tys: Vec::new(),
            tails: vec![QuantTail {
                name: String::new(),
                id: tail,
                skolem: false,
            }],
            rows: Vec::new(),
            constraints: Vec::new(),
            input: tail,
            output,
            effect_vars: vec![QuantEffect {
                name: String::new(),
                id: effect,
                skolem: false,
            }],
            effect,
        }
    }

    /// A nested effect variable is polymorphic in the annotation. The body may
    /// not pin it to a concrete row; that row would be forgotten at the next use.
    fn escaped_effect(&self, span: Span) -> Result<(), Diagnostic> {
        for id in &self.guarded {
            if !self.effect_is_bare(*id) {
                return Err(self.error(
                    span,
                    Error::EffectNotOpen { found: self.show_effect_row(*id) },
                ));
            }
        }
        Ok(())
    }

    fn join_effect(&mut self, effect: EffectId, span: Span) -> Result<(), Diagnostic> {
        let Some(ambient) = self.ambient else {
            return Ok(());
        };
        self.unify_effect(effect, ambient)
            .map_err(|message| self.error(span, message))
    }

    /// `/name` reads one field. The row tail keeps every other label.
    fn project_field(
        &mut self,
        span: Span,
        name: &str,
        stack: StackId,
    ) -> Result<StackId, Diagnostic> {
        let field_ty = self.ty_var();
        let row_tail = self.row_var();
        let row = self.row_fields(vec![(name.to_string(), field_ty)], Some(row_tail));
        let record = self.ty_record(row);
        let hidden = self.stack_var();
        let input = self.stack_cons(hidden, record);
        let output = self.stack_cons(hidden, field_ty);
        self.unify_stack(stack, input)
            .map_err(|message| self.error(span, message))?;
        let effect = match self.ambient {
            Some(effect) => effect,
            None => self.effect_var(),
        };
        self.pending_here = Some((input, output, effect));
        Ok(output)
    }

    /// `=name` replaces one field. The record is below the new value.
    /// The label must already be there: this does not extend the row.
    fn set_field(
        &mut self,
        span: Span,
        name: &str,
        stack: StackId,
    ) -> Result<StackId, Diagnostic> {
        let field_ty = self.ty_var();
        let row_tail = self.row_var();
        let row = self.row_fields(vec![(name.to_string(), field_ty)], Some(row_tail));
        let record = self.ty_record(row);
        let hidden = self.stack_var();
        let under = self.stack_cons(hidden, record);
        let input = self.stack_cons(under, field_ty);
        let output = self.stack_cons(hidden, record);
        self.unify_stack(stack, input)
            .map_err(|message| self.error(span, message))?;
        let effect = match self.ambient {
            Some(effect) => effect,
            None => self.effect_var(),
        };
        self.pending_here = Some((input, output, effect));
        Ok(output)
    }

    /// `+name` adds one label. The record below must already be a closed row
    /// that does not have the label. The value on top supplies the field type.
    /// An open tail is not extended, and a missing label is not a constraint.
    fn insert_field(
        &mut self,
        span: Span,
        name: &str,
        stack: StackId,
    ) -> Result<StackId, Diagnostic> {
        let (rest, value_ty) = self.pop_slot(stack, span)?;
        let (rest, record_ty) = self.pop_slot(rest, span)?;
        let record_ty = self.find_ty(record_ty);
        let TyNode::Record(row) = self.tys[record_ty.0 as usize].clone() else {
            return Err(self.error(
                span,
                Error::ExpectedRecord(self.show_ty(record_ty)),
            ));
        };
        let (mut fields, tail) = self.peel_row(row).map_err(|message| self.error(span, message))?;
        if tail.is_some() {
            return Err(self.error(span, Error::RecordTailOpen));
        }
        if fields.contains_key(name) {
            return Err(self.error(span, Error::DuplicateField(name.to_string())));
        }
        fields.insert(name.to_string(), self.find_ty(value_ty));
        let pairs = fields.into_iter().collect();
        let row = self.row_fields(pairs, None);
        let record = self.ty_record(row);
        let output = self.stack_cons(rest, record);
        let effect = match self.ambient {
            Some(effect) => effect,
            None => self.effect_var(),
        };
        self.pending_here = Some((stack, output, effect));
        Ok(output)
    }

    fn lit_ty(&mut self, lit: &Lit) -> TyId {
        match lit {
            Lit::Int(_) => self.ty_ctor("Int", Vec::new()),
            Lit::Number(_) => self.ty_ctor("Num", Vec::new()),
            Lit::String(_) => self.ty_ctor("Str", Vec::new()),
            Lit::Bool(_) => self.ty_ctor("Bool", Vec::new()),
        }
    }

    /// One constant: a literal, a quotation, or a nested array or record.
    fn check_element(&mut self, term: &Term) -> Result<TyId, Diagnostic> {
        match &term.node {
            TermNode::Lit(lit) => Ok(self.lit_ty(lit)),
            TermNode::Quote(terms) => self.check_quote(terms),
            TermNode::Array(elements) => self.check_array(term.span, elements),
            TermNode::Record(fields) => self.check_record(term.span, fields),
            _ => Err(self.error(term.span, Error::ExpectedElement)),
        }
    }

    fn check_array(&mut self, _span: Span, elements: &[Term]) -> Result<TyId, Diagnostic> {
        let mut elem_ty = None;
        for element in elements {
            let ty = self.check_element(element)?;
            if let Some(prev) = elem_ty {
                self.unify_ty(ty, prev)
                    .map_err(|message| self.error(element.span, message))?;
            } else {
                elem_ty = Some(ty);
            }
        }
        let elem_ty = elem_ty.unwrap_or_else(|| self.ty_var());
        Ok(self.ty_ctor("Array", vec![elem_ty]))
    }

    fn check_record(
        &mut self,
        span: Span,
        fields: &[(String, Term)],
    ) -> Result<TyId, Diagnostic> {
        let mut seen = HashSet::new();
        let mut pairs = Vec::new();
        for (name, term) in fields {
            if !seen.insert(name.clone()) {
                return Err(self.error(span, Error::DuplicateField(name.to_string())));
            }
            pairs.push((name.clone(), self.check_element(term)?));
        }
        let row = self.row_fields(pairs, None);
        Ok(self.ty_record(row))
    }

    fn lookup(&self, name: &str) -> Option<SemScheme> {
        for frame in self.locals.iter().rev() {
            if let Some(scheme) = frame.get(name) {
                return Some(scheme.clone());
            }
        }
        self.words.get(name).cloned()
    }

    fn instantiate_scheme(
        &mut self,
        scheme: &SemScheme,
        skolemize: bool,
    ) -> (StackId, StackId, EffectId, Vec<SemConstraint>) {
        let mut subst = Subst::default();
        for (name, id) in &scheme.tys {
            let fresh = if skolemize {
                self.ty_rigid(name)
            } else {
                self.ty_var()
            };
            if let Some(kind) = self.ty_kind.get(id).copied() {
                self.ty_kind.insert(fresh, kind);
            }
            subst.ty.insert(*id, fresh);
        }
        for tail in &scheme.tails {
            let fresh = if skolemize && tail.skolem {
                self.stack_rigid(&tail.name)
            } else {
                self.stack_var()
            };
            subst.stack.insert(tail.id, fresh);
        }
        for row in &scheme.rows {
            let fresh = if skolemize && row.skolem {
                self.row_rigid(&row.name)
            } else {
                self.row_var()
            };
            subst.row.insert(row.id, fresh);
        }
        let mut guarded = Vec::new();
        for effect in &scheme.effect_vars {
            let fresh = if skolemize && effect.skolem {
                self.effect_rigid(&effect.name)
            } else {
                self.effect_var()
            };
            if skolemize && !effect.skolem {
                guarded.push(fresh);
            }
            subst.effect.insert(effect.id, fresh);
        }
        if skolemize {
            self.guarded = guarded;
        }
        let mut memo = Memo::default();
        let input = self.copy_stack(scheme.input, &subst, &mut memo);
        let output = self.copy_stack(scheme.output, &subst, &mut memo);
        let effect = self.copy_effect(scheme.effect, &subst, &mut memo);
        let constraints = scheme
            .constraints
            .iter()
            .map(|constraint| SemConstraint {
                class_name: constraint.class_name.clone(),
                args: constraint
                    .args
                    .iter()
                    .map(|arg| self.copy_ty(*arg, &subst, &mut memo))
                    .collect(),
            })
            .collect();
        (input, output, effect, constraints)
    }

    fn copy_ty(&mut self, id: TyId, subst: &Subst, memo: &mut Memo) -> TyId {
        let id = self.find_ty(id);
        if let Some(replaced) = subst.ty.get(&id) {
            return *replaced;
        }
        if let Some(copied) = memo.ty.get(&id) {
            return *copied;
        }
        match self.tys[id.0 as usize].clone() {
            TyNode::Template => {
                let fresh = self.ty_var();
                memo.ty.insert(id, fresh);
                fresh
            }
            TyNode::Var | TyNode::Rigid(_) => id,
            TyNode::Link(next) => self.copy_ty(next, subst, memo),
            TyNode::Ctor(name, args) => {
                let fresh = self.ty_var();
                memo.ty.insert(id, fresh);
                let args = args
                    .into_iter()
                    .map(|arg| self.copy_ty(arg, subst, memo))
                    .collect();
                self.tys[fresh.0 as usize] = TyNode::Ctor(name, args);
                fresh
            }
            TyNode::App(head, args) => {
                let fresh = self.ty_var();
                memo.ty.insert(id, fresh);
                let mut copied = self.copy_ty(head, subst, memo);
                for arg in args {
                    let arg = self.copy_ty(arg, subst, memo);
                    copied = self.type_app(copied, arg);
                }
                self.tys[fresh.0 as usize] = TyNode::Link(copied);
                fresh
            }
            TyNode::Fun(input, output, effect) => {
                let fresh = self.ty_var();
                memo.ty.insert(id, fresh);
                let input = self.copy_stack(input, subst, memo);
                let output = self.copy_stack(output, subst, memo);
                let effect = self.copy_effect(effect, subst, memo);
                self.tys[fresh.0 as usize] = TyNode::Fun(input, output, effect);
                fresh
            }
            TyNode::Record(row) => {
                let fresh = self.ty_var();
                memo.ty.insert(id, fresh);
                let row = self.copy_row(row, subst, memo);
                self.tys[fresh.0 as usize] = TyNode::Record(row);
                fresh
            }
        }
    }

    fn copy_row(&mut self, id: RowId, subst: &Subst, memo: &mut Memo) -> RowId {
        let id = self.find_row(id);
        if let Some(replaced) = subst.row.get(&id) {
            return *replaced;
        }
        if let Some(copied) = memo.row.get(&id) {
            return *copied;
        }
        match self.rows[id.0 as usize].clone() {
            RowNode::Template => {
                let fresh = self.row_var();
                memo.row.insert(id, fresh);
                fresh
            }
            RowNode::Var | RowNode::Rigid(_) => id,
            RowNode::Link(next) => self.copy_row(next, subst, memo),
            RowNode::Fields { fields, tail } => {
                let fresh = self.row_var();
                memo.row.insert(id, fresh);
                let fields = fields
                    .into_iter()
                    .map(|(name, ty)| (name, self.copy_ty(ty, subst, memo)))
                    .collect();
                let tail = tail.map(|tail| self.copy_row(tail, subst, memo));
                self.rows[fresh.0 as usize] = RowNode::Fields { fields, tail };
                fresh
            }
        }
    }

    fn copy_effect(&mut self, id: EffectId, subst: &Subst, memo: &mut Memo) -> EffectId {
        let id = self.find_effect(id);
        if let Some(replaced) = subst.effect.get(&id) {
            return *replaced;
        }
        if let Some(copied) = memo.effect.get(&id) {
            return *copied;
        }
        match self.effects[id.0 as usize].clone() {
            EffectNode::Template => {
                let fresh = self.effect_var();
                memo.effect.insert(id, fresh);
                fresh
            }
            EffectNode::Var | EffectNode::Rigid(_) => id,
            EffectNode::Link(next) => self.copy_effect(next, subst, memo),
            EffectNode::Labels { labels, tail } => {
                let fresh = self.effect_var();
                memo.effect.insert(id, fresh);
                let tail = tail.map(|tail| self.copy_effect(tail, subst, memo));
                self.effects[fresh.0 as usize] = EffectNode::Labels { labels, tail };
                fresh
            }
        }
    }

    fn copy_stack(&mut self, id: StackId, subst: &Subst, memo: &mut Memo) -> StackId {
        let id = self.find_stack(id);
        if let Some(replaced) = subst.stack.get(&id) {
            return *replaced;
        }
        if let Some(copied) = memo.stack.get(&id) {
            return *copied;
        }
        match self.stacks[id.0 as usize].clone() {
            StackNode::Template => {
                let fresh = self.stack_var();
                memo.stack.insert(id, fresh);
                fresh
            }
            StackNode::Var | StackNode::Rigid { .. } | StackNode::Empty => id,
            StackNode::Link(next) => self.copy_stack(next, subst, memo),
            StackNode::Cons(rest, ty) => {
                let fresh = self.stack_var();
                memo.stack.insert(id, fresh);
                let rest = self.copy_stack(rest, subst, memo);
                let ty = self.copy_ty(ty, subst, memo);
                self.stacks[fresh.0 as usize] = StackNode::Cons(rest, ty);
                fresh
            }
        }
    }

    fn quantify_var(&mut self, scope: &mut Scope, name: String) {
        let kind = self.kind_var();
        let id = self.ty_of_kind(kind);
        scope.tys.insert(name.clone(), id);
        scope.quantified_tys.push((name, id));
    }

    fn elaborate_scheme(&mut self, scheme: &Scheme, span: Span) -> Result<SemScheme, Diagnostic> {
        self.elaborate_with(scheme, span, Scope::new(), 0)
    }

    /// `preset` quantified types keep the kind they already have. Later binders
    /// default an unsolved kind to `Type`.
    fn elaborate_with(
        &mut self,
        scheme: &Scheme,
        span: Span,
        mut scope: Scope,
        preset: usize,
    ) -> Result<SemScheme, Diagnostic> {
        for name in &scheme.vars {
            if scope.tys.contains_key(name) {
                return Err(self.error(span, Error::DuplicateTypeVariable(name.to_string())));
            }
            self.quantify_var(&mut scope, name.clone());
        }
        // A lowercase name written in the scheme is quantified for the whole
        // scheme. Class parameters and an explicit `a b.` list are already bound.
        // Row tails, effect tails, and stack tails are not type variables.
        let mut inferred = Vec::new();
        collect_scheme_vars(scheme, &mut inferred);
        for name in inferred {
            if scope.tys.contains_key(&name) {
                continue;
            }
            self.quantify_var(&mut scope, name);
        }
        let (input, output, effect) = self.elab_effect(&scheme.effect, &mut scope, span)?;
        let mut constraints = Vec::new();
        for constraint in &scheme.constraints {
            constraints.push(self.elab_constraint(constraint, &mut scope, span)?);
        }
        for (_, id) in scope.quantified_tys.iter().skip(preset) {
            if let Some(kind) = self.ty_kind.get(id).copied() {
                self.default_kind(kind);
            }
        }
        Ok(SemScheme {
            tys: scope.quantified_tys,
            tails: scope.quantified_tails,
            rows: scope.quantified_rows,
            constraints,
            input,
            output,
            effect_vars: scope.quantified_effects,
            effect,
        })
    }

    fn elab_effect(
        &mut self,
        effect: &StackEffect,
        scope: &mut Scope,
        span: Span,
    ) -> Result<(StackId, StackId, EffectId), Diagnostic> {
        let row = self.elab_effect_row(effect.effects.as_ref(), scope, span)?;
        let implicit = if matches!(effect.input.tail, StackTail::Implicit)
            || matches!(effect.output.tail, StackTail::Implicit)
        {
            Some(self.fresh_tail(scope, "", !scope.nested))
        } else {
            None
        };
        let input = self.elab_stack(&effect.input, implicit, scope, span)?;
        let output = self.elab_stack(&effect.output, implicit, scope, span)?;
        Ok((input, output, row))
    }

    /// Programs cannot write an effect row. A missing row is one open variable
    /// for the whole scheme, distinct from a record `| name`.
    fn elab_effect_row(
        &mut self,
        row: Option<&EffectRow>,
        scope: &mut Scope,
        span: Span,
    ) -> Result<EffectId, Diagnostic> {
        let Some(row) = row else {
            return Ok(self.implicit_effect(scope));
        };
        let mut seen = HashSet::new();
        let mut labels = Vec::new();
        for label in &row.labels {
            if !seen.insert(label.clone()) {
                return Err(self.error(span, Error::DuplicateEffect(label.to_string())));
            }
            labels.push(label.clone());
        }
        let tail = match &row.tail {
            Some(name) => Some(self.named_effect(scope, name, span)?),
            None => None,
        };
        Ok(self.effect_labels(labels, tail))
    }

    fn named_effect(
        &mut self,
        scope: &mut Scope,
        name: &str,
        span: Span,
    ) -> Result<EffectId, Diagnostic> {
        if scope.tys.contains_key(name) {
            return Err(self.error(span, Error::DuplicateTypeVariable(name.to_string())));
        }
        if let Some(id) = scope.effects.get(name) {
            return Ok(*id);
        }
        Ok(self.fresh_effect(scope, name, true))
    }

    fn implicit_effect(&mut self, scope: &mut Scope) -> EffectId {
        if let Some(id) = scope.implicit_effect {
            return id;
        }
        let id = self.fresh_effect(scope, "", !scope.nested);
        scope.implicit_effect = Some(id);
        id
    }

    fn fresh_effect(&mut self, scope: &mut Scope, name: &str, skolem: bool) -> EffectId {
        let id = self.push_effect(EffectNode::Template);
        if !name.is_empty() {
            scope.effects.insert(name.to_string(), id);
        }
        scope.quantified_effects.push(QuantEffect {
            name: name.to_string(),
            id,
            skolem,
        });
        id
    }

    fn elab_stack(
        &mut self,
        stack: &Stack,
        implicit: Option<StackId>,
        scope: &mut Scope,
        span: Span,
    ) -> Result<StackId, Diagnostic> {
        let mut acc = match &stack.tail {
            StackTail::Empty => self.stack_empty(),
            StackTail::Var(name) => self.named_tail(scope, name),
            StackTail::Implicit => implicit.expect("implicit stack tail"),
        };
        for item in &stack.items {
            let ty = self.elab_type(item, scope, span)?;
            acc = self.stack_cons(acc, ty);
        }
        Ok(acc)
    }

    /// A stack slot or a record field. Kind must be `Type`.
    fn elab_type(&mut self, ty: &Type, scope: &mut Scope, span: Span) -> Result<TyId, Diagnostic> {
        let (id, kind) = self.elab_ty(ty, scope, span)?;
        self.demand_type(id, kind, span)?;
        Ok(id)
    }

    fn elab_ty(
        &mut self,
        ty: &Type,
        scope: &mut Scope,
        span: Span,
    ) -> Result<(TyId, KindId), Diagnostic> {
        match ty {
            Type::Var(name) => {
                let id = scope.tys.get(name).copied().ok_or_else(|| {
                    self.error(span, Error::TypeVariableNotQuantified(name.to_string()))
                })?;
                let kind = match self.ty_kind.get(&id).copied() {
                    Some(kind) => kind,
                    None => self.kind_type(),
                };
                Ok((id, kind))
            }
            Type::Ctor(name) => {
                let arity = self.arity_of(name, span)?;
                if arity == 0 && self.syn_in_scope(name) {
                    return self.expand_synonym(name, &[], span);
                }
                Ok((self.ty_ctor(name, Vec::new()), self.ctor_kind(arity)))
            }
            Type::App(head, args) => {
                if let Type::Ctor(name) = head.as_ref() {
                    let arity = self.arity_of(name, span)?;
                    if args.len() > arity {
                        return Err(self.arity_error(span, name, arity));
                    }
                    if args.len() == arity && self.syn_in_scope(name) {
                        let mut ids = Vec::with_capacity(args.len());
                        for arg in args {
                            let (arg_id, arg_kind) = self.elab_ty(arg, scope, span)?;
                            self.demand_type(arg_id, arg_kind, span)?;
                            ids.push(arg_id);
                        }
                        return self.expand_synonym(name, &ids, span);
                    }
                }
                let (mut id, mut kind) = self.elab_ty(head, scope, span)?;
                for arg in args {
                    let (arg_id, arg_kind) = self.elab_ty(arg, scope, span)?;
                    let result = self.kind_var();
                    let expect = self.kind_arrow(arg_kind, result);
                    self.unify_kind(kind, expect).map_err(|message| self.error(span, message))?;
                    id = self.type_app(id, arg_id);
                    kind = result;
                }
                Ok((id, kind))
            }
            Type::Fun(effect) => {
                let saved = scope.nested;
                scope.nested = true;
                let (input, output, row) = self.elab_effect(effect, scope, span)?;
                scope.nested = saved;
                Ok((self.ty_fun(input, output, row), self.kind_type()))
            }
            Type::Record(row) => Ok((self.elab_record(row, scope, span)?, self.kind_type())),
        }
    }

    fn type_app(&mut self, head: TyId, arg: TyId) -> TyId {
        match self.tys[self.find_ty(head).0 as usize].clone() {
            TyNode::Ctor(name, mut args) => {
                args.push(arg);
                self.ty_ctor(&name, args)
            }
            TyNode::App(spine, mut args) => {
                args.push(arg);
                self.push_ty(TyNode::App(spine, args))
            }
            _ => self.push_ty(TyNode::App(head, vec![arg])),
        }
    }

    fn demand_type(&mut self, ty: TyId, kind: KindId, span: Span) -> Result<(), Diagnostic> {
        let kind = self.find_kind(kind);
        match self.kinds[kind.0 as usize].clone() {
            KindNode::Type => Ok(()),
            KindNode::Var => {
                self.kinds[kind.0 as usize] = KindNode::Type;
                Ok(())
            }
            KindNode::Arrow(_, _) => {
                if let Some(message) = self.underapplied(ty) {
                    Err(self.error(span, message))
                } else {
                    Err(self.error(
                        span,
                        Error::KindMismatch { expected: "Type".to_string(), found: self.show_kind(kind) },
                    ))
                }
            }
            KindNode::Link(next) => self.demand_type(ty, next, span),
        }
    }

    fn underapplied(&self, ty: TyId) -> Option<Error> {
        let TyNode::Ctor(name, args) = &self.tys[self.find_ty(ty).0 as usize] else {
            return None;
        };
        let arity = self
            .type_arity
            .get(name)
            .or_else(|| self.extra_arity.get(name))
            .copied()?;
        if args.len() < arity {
            Some(Error::TypeConstructorArity {
                name: name.to_string(),
                count: arity,
            })
        } else {
            None
        }
    }

    fn elab_record(
        &mut self,
        row: &crate::ast::RecordRow,
        scope: &mut Scope,
        span: Span,
    ) -> Result<TyId, Diagnostic> {
        let mut seen = HashSet::new();
        let mut fields = Vec::new();
        for (name, ty) in &row.fields {
            if !seen.insert(name.clone()) {
                return Err(self.error(span, Error::DuplicateField(name.to_string())));
            }
            fields.push((name.clone(), self.elab_type(ty, scope, span)?));
        }
        let tail = match &row.tail {
            Some(RecordTail::Var(name)) => Some(self.named_row(scope, name, span)?),
            Some(RecordTail::Type(ty)) => {
                let spliced = self.elab_type(ty, scope, span)?;
                self.splice_closed_record(spliced, span, &mut seen, &mut fields)?;
                None
            }
            None => None,
        };
        let row = self.row_fields(fields, tail);
        Ok(self.ty_record(row))
    }

    /// Copy fields from a tail type. The tail must be a record with no `| r`.
    fn splice_closed_record(
        &mut self,
        spliced: TyId,
        span: Span,
        seen: &mut HashSet<String>,
        fields: &mut Vec<(String, TyId)>,
    ) -> Result<(), Diagnostic> {
        let spliced = self.find_ty(spliced);
        let TyNode::Record(row_id) = self.tys[spliced.0 as usize].clone() else {
            return Err(self.error(
                span,
                Error::ExpectedRecord(self.show_ty(spliced)),
            ));
        };
        let (spliced_fields, spliced_tail) = self
            .peel_row(row_id)
            .map_err(|message| self.error(span, message))?;
        if spliced_tail.is_some() {
            return Err(self.error(span, Error::RecordTailOpen));
        }
        for (name, ty) in spliced_fields {
            if !seen.insert(name.clone()) {
                return Err(self.error(span, Error::DuplicateField(name.to_string())));
            }
            fields.push((name, ty));
        }
        Ok(())
    }

    fn arity_of(&self, name: &str, span: Span) -> Result<usize, Diagnostic> {
        self.type_arity
            .get(name)
            .or_else(|| self.extra_arity.get(name))
            .copied()
            .ok_or_else(|| self.error(span, Error::UndefinedType(name.to_string())))
    }

    fn arity_error(&self, span: Span, name: &str, arity: usize) -> Diagnostic {
        self.error(
            span,
            Error::TypeConstructorArity {
                name: name.to_string(),
                count: arity,
            },
        )
    }

    fn named_tail(&mut self, scope: &mut Scope, name: &str) -> StackId {
        if let Some(id) = scope.tails.get(name) {
            return *id;
        }
        self.fresh_tail(scope, name, true)
    }

    fn fresh_tail(&mut self, scope: &mut Scope, name: &str, skolem: bool) -> StackId {
        let id = self.push_stack(StackNode::Template);
        if !name.is_empty() {
            scope.tails.insert(name.to_string(), id);
        }
        scope.quantified_tails.push(QuantTail {
            name: name.to_string(),
            id,
            skolem,
        });
        id
    }

    /// `| name` is one quantified row for the whole scheme. A type variable
    /// already bound before the dot cannot be reused as that tail.
    fn named_row(&mut self, scope: &mut Scope, name: &str, span: Span) -> Result<RowId, Diagnostic> {
        if scope.tys.contains_key(name) {
            return Err(self.error(span, Error::DuplicateTypeVariable(name.to_string())));
        }
        if let Some(id) = scope.rows.get(name) {
            return Ok(*id);
        }
        Ok(self.fresh_row(scope, name))
    }

    fn fresh_row(&mut self, scope: &mut Scope, name: &str) -> RowId {
        let id = self.push_row(RowNode::Template);
        if !name.is_empty() {
            scope.rows.insert(name.to_string(), id);
        }
        scope.quantified_rows.push(QuantRow {
            name: name.to_string(),
            id,
            skolem: true,
        });
        id
    }

    fn unify_ty(&mut self, have: TyId, need: TyId) -> Result<(), Error> {
        let have = self.find_ty(have);
        let need = self.find_ty(need);
        if have == need {
            return Ok(());
        }
        match (
            self.tys[have.0 as usize].clone(),
            self.tys[need.0 as usize].clone(),
        ) {
            (TyNode::Var, _) => self.bind_ty(have, need),
            (_, TyNode::Var) => self.bind_ty(need, have),
            (TyNode::Ctor(name, args), TyNode::Ctor(need_name, need_args))
                if name == need_name && args.len() == need_args.len() =>
            {
                for (have_arg, need_arg) in args.into_iter().zip(need_args) {
                    self.unify_ty(have_arg, need_arg)?;
                }
                Ok(())
            }
            (
                TyNode::Fun(have_in, have_out, have_effect),
                TyNode::Fun(need_in, need_out, need_effect),
            ) => {
                self.unify_stack(have_in, need_in)?;
                self.unify_stack(have_out, need_out)?;
                self.unify_effect(have_effect, need_effect)
            }
            (TyNode::Record(have_row), TyNode::Record(need_row)) => {
                self.unify_record(have_row, need_row)
            }
            (TyNode::App(have_head, have_args), TyNode::App(need_head, need_args))
                if have_args.len() == need_args.len() =>
            {
                self.unify_ty(have_head, need_head)?;
                for (have_arg, need_arg) in have_args.into_iter().zip(need_args) {
                    self.unify_ty(have_arg, need_arg)?;
                }
                Ok(())
            }
            (TyNode::App(head, app_args), TyNode::Ctor(name, ctor_args)) => {
                self.unify_spine(have, need, head, app_args, &name, ctor_args, true)
            }
            (TyNode::Ctor(name, ctor_args), TyNode::App(head, app_args)) => {
                self.unify_spine(have, need, head, app_args, &name, ctor_args, false)
            }
            _ => Err(self.ty_mismatch(have, need)),
        }
    }

    /// `(f a)` and `(Maybe Int)` are the same shape: `f` is the unsaturated
    /// constructor and `a` is its last argument. A shorter constructor cannot
    /// absorb a longer application.
    fn unify_spine(
        &mut self,
        have: TyId,
        need: TyId,
        app_head: TyId,
        app_args: Vec<TyId>,
        ctor_name: &str,
        mut ctor_args: Vec<TyId>,
        app_is_have: bool,
    ) -> Result<(), Error> {
        if ctor_args.len() < app_args.len() {
            return Err(self.ty_mismatch(have, need));
        }
        let prefix = ctor_args.len() - app_args.len();
        let rest = ctor_args.split_off(prefix);
        let head = self.ty_ctor(ctor_name, ctor_args);
        if app_is_have {
            self.unify_ty(app_head, head)?;
            for (app_arg, ctor_arg) in app_args.into_iter().zip(rest) {
                self.unify_ty(app_arg, ctor_arg)?;
            }
        } else {
            self.unify_ty(head, app_head)?;
            for (ctor_arg, app_arg) in rest.into_iter().zip(app_args) {
                self.unify_ty(ctor_arg, app_arg)?;
            }
        }
        Ok(())
    }

    fn ty_mismatch(&self, have: TyId, need: TyId) -> Error {
        Error::TypeMismatch {
            expected: self.show_ty(need),
            found: self.show_ty(have),
        }
    }

    fn unify_stack(&mut self, have: StackId, need: StackId) -> Result<(), Error> {
        let have = self.find_stack(have);
        let need = self.find_stack(need);
        if have == need {
            return Ok(());
        }
        match (
            self.stacks[have.0 as usize].clone(),
            self.stacks[need.0 as usize].clone(),
        ) {
            (StackNode::Cons(have_rest, have_ty), StackNode::Cons(need_rest, need_ty)) => {
                self.unify_ty(have_ty, need_ty)?;
                self.unify_stack(have_rest, need_rest)
            }
            (StackNode::Var, _) => self.bind_stack(have, need),
            (_, StackNode::Var) => self.bind_stack(need, have),
            (StackNode::Empty, StackNode::Empty) => Ok(()),
            (StackNode::Empty, StackNode::Cons(_, _))
            | (StackNode::Rigid { .. }, StackNode::Cons(_, _)) => Err(Error::StackUnderflow),
            _ => Err(Error::TypeMismatch { expected: self.show_stack(need), found: self.show_stack(have) }),
        }
    }

    fn bind_ty(&mut self, var_id: TyId, target: TyId) -> Result<(), Error> {
        let var_id = self.find_ty(var_id);
        let target = self.find_ty(target);
        if var_id == target {
            return Ok(());
        }
        if self.frozen_ty.contains(&var_id) {
            return Err(Error::Frozen);
        }
        if self.occurs_ty(var_id, target, &mut Seen::default()) {
            return Err(Error::OccursCheck);
        }
        if self.recording {
            self.trail
                .push(Edit::Ty(var_id.0, self.tys[var_id.0 as usize].clone()));
        }
        self.tys[var_id.0 as usize] = TyNode::Link(target);
        Ok(())
    }

    fn bind_stack(&mut self, var_id: StackId, target: StackId) -> Result<(), Error> {
        let var_id = self.find_stack(var_id);
        let target = self.find_stack(target);
        if var_id == target {
            return Ok(());
        }
        if self.frozen_stack.contains(&var_id) {
            return Err(Error::Frozen);
        }
        if self.occurs_stack(var_id, target, &mut Seen::default()) {
            return Err(Error::OccursCheck);
        }
        if self.recording {
            self.trail
                .push(Edit::Stack(var_id.0, self.stacks[var_id.0 as usize].clone()));
        }
        self.stacks[var_id.0 as usize] = StackNode::Link(target);
        Ok(())
    }

    /// Rows are commutative. An open tail absorbs labels the other side lacks.
    /// A second peel runs after field types unify, because that can instantiate a tail.
    fn unify_record(&mut self, have: RowId, need: RowId) -> Result<(), Error> {
        // Unifying a field can instantiate a tail and reveal more labels.
        // The name set only grows. A repeated set that did not settle is a cycle.
        let mut seen = HashSet::new();
        loop {
            let (have_fields, _) = self.peel_row(have)?;
            let (need_fields, _) = self.peel_row(need)?;
            let mut before = BTreeSet::new();
            before.extend(have_fields.keys().cloned());
            before.extend(need_fields.keys().cloned());
            if !seen.insert(before.clone()) {
                return Err(Error::OccursCheck);
            }
            for (name, have_ty) in &have_fields {
                if let Some(need_ty) = need_fields.get(name) {
                    self.unify_ty(*have_ty, *need_ty)?;
                }
            }
            let (have_fields, have_tail) = self.peel_row(have)?;
            let (need_fields, need_tail) = self.peel_row(need)?;
            let mut after = BTreeSet::new();
            after.extend(have_fields.keys().cloned());
            after.extend(need_fields.keys().cloned());
            if before != after {
                continue;
            }
            let mut only_have = Vec::new();
            let mut only_need = Vec::new();
            for (name, ty) in &have_fields {
                if !need_fields.contains_key(name) {
                    only_have.push((name.clone(), *ty));
                }
            }
            for (name, ty) in &need_fields {
                if !have_fields.contains_key(name) {
                    only_need.push((name.clone(), *ty));
                }
            }
            return self.unify_row_tails(have_tail, need_tail, only_have, only_need, have, need);
        }
    }

    fn unify_row_tails(
        &mut self,
        have_tail: Option<RowId>,
        need_tail: Option<RowId>,
        only_have: Vec<(String, TyId)>,
        only_need: Vec<(String, TyId)>,
        have: RowId,
        need: RowId,
    ) -> Result<(), Error> {
        let have_end = have_tail.map(|id| self.row_end(id));
        let need_end = need_tail.map(|id| self.row_end(id));
        match (have_end, need_end) {
            (None, None) => self.missing_or_ok(&only_need, &only_have),
            (Some(RowEnd::Flex(var)), None) => {
                if let Some((name, _)) = only_have.first() {
                    Err(Error::RecordHasNoField(name.to_string()))
                } else {
                    self.bind_extension(var, only_need, None)
                }
            }
            (None, Some(RowEnd::Flex(var))) => {
                if let Some((name, _)) = only_need.first() {
                    Err(Error::RecordHasNoField(name.to_string()))
                } else {
                    self.bind_extension(var, only_have, None)
                }
            }
            (Some(RowEnd::Flex(left)), Some(RowEnd::Flex(right))) => {
                if left == right {
                    if only_have.is_empty() && only_need.is_empty() {
                        Ok(())
                    } else {
                        Err(Error::OccursCheck)
                    }
                } else {
                    let fresh = self.row_var();
                    self.bind_extension(left, only_need, Some(fresh))?;
                    self.bind_extension(right, only_have, Some(fresh))
                }
            }
            (Some(RowEnd::Skolem(_)), None) | (None, Some(RowEnd::Skolem(_))) => {
                if let Some(name) = first_missing(&only_need, &only_have) {
                    Err(Error::RecordHasNoField(name.to_string()))
                } else {
                    self.row_mismatch(need, have)
                }
            }
            (Some(RowEnd::Skolem(_)), Some(RowEnd::Flex(var))) => {
                if let Some((name, _)) = only_need.first() {
                    Err(Error::RecordHasNoField(name.to_string()))
                } else {
                    let tail = have_tail.expect("skolem row tail");
                    self.bind_extension(var, only_have, Some(tail))
                }
            }
            (Some(RowEnd::Flex(var)), Some(RowEnd::Skolem(_))) => {
                if let Some((name, _)) = only_have.first() {
                    Err(Error::RecordHasNoField(name.to_string()))
                } else {
                    let tail = need_tail.expect("skolem row tail");
                    self.bind_extension(var, only_need, Some(tail))
                }
            }
            (Some(RowEnd::Skolem(left)), Some(RowEnd::Skolem(right))) => {
                if left == right && only_have.is_empty() && only_need.is_empty() {
                    Ok(())
                } else if left == right {
                    Err(Error::OccursCheck)
                } else if let Some(name) = first_missing(&only_need, &only_have) {
                    Err(Error::RecordHasNoField(name.to_string()))
                } else {
                    self.row_mismatch(need, have)
                }
            }
        }
    }

    fn missing_or_ok(
        &self,
        only_need: &[(String, TyId)],
        only_have: &[(String, TyId)],
    ) -> Result<(), Error> {
        if let Some(name) = first_missing(only_need, only_have) {
            Err(Error::RecordHasNoField(name.to_string()))
        } else {
            Ok(())
        }
    }

    fn row_mismatch(&self, need: RowId, have: RowId) -> Result<(), Error> {
        Err(Error::TypeMismatch { expected: self.show_record(need, 0), found: self.show_record(have, 0) })
    }

    fn bind_extension(
        &mut self,
        var: RowId,
        fields: Vec<(String, TyId)>,
        tail: Option<RowId>,
    ) -> Result<(), Error> {
        let target = if fields.is_empty() {
            match tail {
                Some(tail) => tail,
                None => self.row_fields(Vec::new(), None),
            }
        } else {
            self.row_fields(fields, tail)
        };
        self.bind_row(var, target)
    }

    fn bind_row(&mut self, var_id: RowId, target: RowId) -> Result<(), Error> {
        let var_id = self.find_row(var_id);
        let target = self.find_row(target);
        if self.frozen_row.contains(&var_id) {
            return Err(Error::Frozen);
        }
        if !matches!(self.rows[var_id.0 as usize], RowNode::Var) {
            panic!("binding a rigid row");
        }
        if var_id == target {
            return Ok(());
        }
        if self.occurs_row(var_id, target, &mut Seen::default()) {
            return Err(Error::OccursCheck);
        }
        if self.recording {
            self.trail
                .push(Edit::Row(var_id.0, self.rows[var_id.0 as usize].clone()));
        }
        self.rows[var_id.0 as usize] = RowNode::Link(target);
        Ok(())
    }

    fn peel_row(&self, id: RowId) -> Result<(BTreeMap<String, TyId>, Option<RowId>), Error> {
        let mut fields = BTreeMap::new();
        let mut current = Some(id);
        let mut seen = HashSet::new();
        while let Some(row) = current {
            let row = self.find_row(row);
            if !seen.insert(row) {
                return Err(Error::OccursCheck);
            }
            match &self.rows[row.0 as usize] {
                RowNode::Fields { fields: found, tail } => {
                    for (name, ty) in found {
                        if fields.insert(name.clone(), *ty).is_some() {
                            return Err(Error::DuplicateField(name.to_string()));
                        }
                    }
                    current = *tail;
                }
                RowNode::Link(next) => current = Some(*next),
                RowNode::Var | RowNode::Rigid(_) | RowNode::Template => {
                    return Ok((fields, Some(row)));
                }
            }
        }
        Ok((fields, None))
    }

    fn row_end(&self, id: RowId) -> RowEnd {
        let id = self.find_row(id);
        match &self.rows[id.0 as usize] {
            RowNode::Var => RowEnd::Flex(id),
            RowNode::Rigid(_) | RowNode::Template => RowEnd::Skolem(id),
            other => panic!("row tail is {other:?}"),
        }
    }

    /// Effect rows are commutative and carry no types. An open tail absorbs
    /// labels the other side lacks. A skolem tail does not.
    fn unify_effect(&mut self, have: EffectId, need: EffectId) -> Result<(), Error> {
        let (have_labels, have_tail) = self.peel_effect(have)?;
        let (need_labels, need_tail) = self.peel_effect(need)?;
        let only_have: Vec<String> = have_labels.difference(&need_labels).cloned().collect();
        let only_need: Vec<String> = need_labels.difference(&have_labels).cloned().collect();
        let have_end = have_tail.map(|id| self.effect_end(id));
        let need_end = need_tail.map(|id| self.effect_end(id));
        match (have_end, need_end) {
            (None, None) => self.effect_same(&only_have, &only_need, need, have),
            (Some(EffectEnd::Flex(var)), None) => {
                if only_have.is_empty() {
                    self.bind_effect_labels(var, &only_need, None)
                } else {
                    self.effect_mismatch(need, have)
                }
            }
            (None, Some(EffectEnd::Flex(var))) => {
                if only_need.is_empty() {
                    self.bind_effect_labels(var, &only_have, None)
                } else {
                    self.effect_mismatch(need, have)
                }
            }
            (Some(EffectEnd::Flex(left)), Some(EffectEnd::Flex(right))) => {
                if left == right {
                    if only_have.is_empty() && only_need.is_empty() {
                        Ok(())
                    } else {
                        Err(Error::OccursCheck)
                    }
                } else {
                    let fresh = self.effect_var();
                    self.bind_effect_labels(left, &only_need, Some(fresh))?;
                    self.bind_effect_labels(right, &only_have, Some(fresh))
                }
            }
            (Some(EffectEnd::Skolem(_)), None) | (None, Some(EffectEnd::Skolem(_))) => {
                self.effect_mismatch(need, have)
            }
            (Some(EffectEnd::Skolem(_)), Some(EffectEnd::Flex(var))) => {
                if only_need.is_empty() {
                    let tail = have_tail.expect("skolem effect tail");
                    self.bind_effect_labels(var, &only_have, Some(tail))
                } else {
                    self.effect_mismatch(need, have)
                }
            }
            (Some(EffectEnd::Flex(var)), Some(EffectEnd::Skolem(_))) => {
                if only_have.is_empty() {
                    let tail = need_tail.expect("skolem effect tail");
                    self.bind_effect_labels(var, &only_need, Some(tail))
                } else {
                    self.effect_mismatch(need, have)
                }
            }
            (Some(EffectEnd::Skolem(left)), Some(EffectEnd::Skolem(right))) => {
                if left == right && only_have.is_empty() && only_need.is_empty() {
                    Ok(())
                } else if left == right {
                    Err(Error::OccursCheck)
                } else {
                    self.effect_mismatch(need, have)
                }
            }
        }
    }

    fn effect_same(
        &self,
        only_have: &[String],
        only_need: &[String],
        need: EffectId,
        have: EffectId,
    ) -> Result<(), Error> {
        if only_have.is_empty() && only_need.is_empty() {
            Ok(())
        } else {
            self.effect_mismatch(need, have)
        }
    }

    fn effect_mismatch(&self, need: EffectId, have: EffectId) -> Result<(), Error> {
        Err(Error::EffectMismatch { expected: self.show_effect_row(need), found: self.show_effect_row(have) })
    }

    fn bind_effect_labels(
        &mut self,
        var: EffectId,
        labels: &[String],
        tail: Option<EffectId>,
    ) -> Result<(), Error> {
        let target = if labels.is_empty() {
            match tail {
                Some(tail) => tail,
                None => self.effect_labels(Vec::new(), None),
            }
        } else {
            self.effect_labels(labels.to_vec(), tail)
        };
        self.bind_effect(var, target)
    }

    fn bind_effect(&mut self, var_id: EffectId, target: EffectId) -> Result<(), Error> {
        let var_id = self.find_effect(var_id);
        let target = self.find_effect(target);
        if self.frozen_effect.contains(&var_id) {
            return Err(Error::Frozen);
        }
        if !matches!(self.effects[var_id.0 as usize], EffectNode::Var) {
            panic!("binding a rigid effect");
        }
        if var_id == target {
            return Ok(());
        }
        if self.occurs_effect(var_id, target, &mut Seen::default()) {
            return Err(Error::OccursCheck);
        }
        if self.recording {
            self.trail.push(Edit::Effect(
                var_id.0,
                self.effects[var_id.0 as usize].clone(),
            ));
        }
        self.effects[var_id.0 as usize] = EffectNode::Link(target);
        Ok(())
    }

    fn peel_effect(&self, id: EffectId) -> Result<(BTreeSet<String>, Option<EffectId>), Error> {
        let mut labels = BTreeSet::new();
        let mut current = Some(id);
        let mut seen = HashSet::new();
        while let Some(effect) = current {
            let effect = self.find_effect(effect);
            if !seen.insert(effect) {
                return Err(Error::OccursCheck);
            }
            match &self.effects[effect.0 as usize] {
                EffectNode::Labels { labels: found, tail } => {
                    for name in found {
                        if !labels.insert(name.clone()) {
                            return Err(Error::DuplicateEffect(name.to_string()));
                        }
                    }
                    current = *tail;
                }
                EffectNode::Link(next) => current = Some(*next),
                EffectNode::Var | EffectNode::Rigid(_) | EffectNode::Template => {
                    return Ok((labels, Some(effect)));
                }
            }
        }
        Ok((labels, None))
    }

    fn effect_end(&self, id: EffectId) -> EffectEnd {
        let id = self.find_effect(id);
        match &self.effects[id.0 as usize] {
            EffectNode::Var => EffectEnd::Flex(id),
            EffectNode::Rigid(_) | EffectNode::Template => EffectEnd::Skolem(id),
            other => panic!("effect tail is {other:?}"),
        }
    }

    fn occurs_effect(&self, var_id: EffectId, in_id: EffectId, seen: &mut Seen) -> bool {
        let in_id = self.find_effect(in_id);
        if in_id == var_id {
            return true;
        }
        if !seen.effect.insert(in_id) {
            return false;
        }
        match &self.effects[in_id.0 as usize] {
            EffectNode::Labels { tail, .. } => {
                tail.is_some_and(|tail| self.occurs_effect(var_id, tail, seen))
            }
            EffectNode::Link(next) => self.occurs_effect(var_id, *next, seen),
            _ => false,
        }
    }

    fn occurs_ty(&self, var_id: TyId, in_id: TyId, seen: &mut Seen) -> bool {
        let in_id = self.find_ty(in_id);
        if in_id == var_id {
            return true;
        }
        if !seen.ty.insert(in_id) {
            return false;
        }
        match self.tys[in_id.0 as usize].clone() {
            TyNode::Ctor(_, args) => args.into_iter().any(|arg| self.occurs_ty(var_id, arg, seen)),
            TyNode::App(head, args) => {
                self.occurs_ty(var_id, head, seen)
                    || args.into_iter().any(|arg| self.occurs_ty(var_id, arg, seen))
            }
            TyNode::Fun(input, output, _) => {
                self.ty_in_stack(var_id, input, seen) || self.ty_in_stack(var_id, output, seen)
            }
            TyNode::Record(row) => self.ty_in_row(var_id, row, seen),
            TyNode::Link(next) => self.occurs_ty(var_id, next, seen),
            _ => false,
        }
    }

    fn ty_in_row(&self, var_id: TyId, row: RowId, seen: &mut Seen) -> bool {
        let row = self.find_row(row);
        if !seen.row.insert(row) {
            return false;
        }
        match self.rows[row.0 as usize].clone() {
            RowNode::Fields { fields, tail } => {
                fields.iter().any(|(_, ty)| self.occurs_ty(var_id, *ty, seen))
                    || tail.is_some_and(|tail| self.ty_in_row(var_id, tail, seen))
            }
            RowNode::Link(next) => self.ty_in_row(var_id, next, seen),
            _ => false,
        }
    }

    fn ty_in_stack(&self, var_id: TyId, stack: StackId, seen: &mut Seen) -> bool {
        let stack = self.find_stack(stack);
        if !seen.stack.insert(stack) {
            return false;
        }
        match self.stacks[stack.0 as usize].clone() {
            StackNode::Cons(rest, ty) => {
                self.occurs_ty(var_id, ty, seen) || self.ty_in_stack(var_id, rest, seen)
            }
            StackNode::Link(next) => self.ty_in_stack(var_id, next, seen),
            _ => false,
        }
    }

    fn occurs_stack(&self, var_id: StackId, in_id: StackId, seen: &mut Seen) -> bool {
        let in_id = self.find_stack(in_id);
        if in_id == var_id {
            return true;
        }
        if !seen.stack.insert(in_id) {
            return false;
        }
        match self.stacks[in_id.0 as usize].clone() {
            StackNode::Cons(rest, ty) => {
                self.occurs_stack(var_id, rest, seen) || self.stack_in_ty(var_id, ty, seen)
            }
            StackNode::Link(next) => self.occurs_stack(var_id, next, seen),
            _ => false,
        }
    }

    fn stack_in_ty(&self, var_id: StackId, ty: TyId, seen: &mut Seen) -> bool {
        let ty = self.find_ty(ty);
        if !seen.ty.insert(ty) {
            return false;
        }
        match self.tys[ty.0 as usize].clone() {
            TyNode::Fun(input, output, _) => {
                self.occurs_stack(var_id, input, seen) || self.occurs_stack(var_id, output, seen)
            }
            TyNode::Ctor(_, args) => args.into_iter().any(|arg| self.stack_in_ty(var_id, arg, seen)),
            TyNode::App(head, args) => {
                self.stack_in_ty(var_id, head, seen)
                    || args.into_iter().any(|arg| self.stack_in_ty(var_id, arg, seen))
            }
            TyNode::Record(row) => self.stack_in_row(var_id, row, seen),
            TyNode::Link(next) => self.stack_in_ty(var_id, next, seen),
            _ => false,
        }
    }

    fn stack_in_row(&self, var_id: StackId, row: RowId, seen: &mut Seen) -> bool {
        let row = self.find_row(row);
        if !seen.row.insert(row) {
            return false;
        }
        match self.rows[row.0 as usize].clone() {
            RowNode::Fields { fields, tail } => {
                fields.iter().any(|(_, ty)| self.stack_in_ty(var_id, *ty, seen))
                    || tail.is_some_and(|tail| self.stack_in_row(var_id, tail, seen))
            }
            RowNode::Link(next) => self.stack_in_row(var_id, next, seen),
            _ => false,
        }
    }

    fn occurs_row(&self, var_id: RowId, in_row: RowId, seen: &mut Seen) -> bool {
        let in_row = self.find_row(in_row);
        if in_row == var_id {
            return true;
        }
        if !seen.row.insert(in_row) {
            return false;
        }
        match self.rows[in_row.0 as usize].clone() {
            RowNode::Fields { fields, tail } => {
                fields.iter().any(|(_, ty)| self.row_in_ty(var_id, *ty, seen))
                    || tail.is_some_and(|tail| self.occurs_row(var_id, tail, seen))
            }
            RowNode::Link(next) => self.occurs_row(var_id, next, seen),
            _ => false,
        }
    }

    fn row_in_ty(&self, var_id: RowId, ty: TyId, seen: &mut Seen) -> bool {
        let ty = self.find_ty(ty);
        if !seen.ty.insert(ty) {
            return false;
        }
        match self.tys[ty.0 as usize].clone() {
            TyNode::Record(row) => self.occurs_row(var_id, row, seen),
            TyNode::Ctor(_, args) => args.into_iter().any(|arg| self.row_in_ty(var_id, arg, seen)),
            TyNode::App(head, args) => {
                self.row_in_ty(var_id, head, seen)
                    || args.into_iter().any(|arg| self.row_in_ty(var_id, arg, seen))
            }
            TyNode::Fun(input, output, _) => {
                self.row_in_stack(var_id, input, seen) || self.row_in_stack(var_id, output, seen)
            }
            TyNode::Link(next) => self.row_in_ty(var_id, next, seen),
            _ => false,
        }
    }

    fn row_in_stack(&self, var_id: RowId, stack: StackId, seen: &mut Seen) -> bool {
        let stack = self.find_stack(stack);
        if !seen.stack.insert(stack) {
            return false;
        }
        match self.stacks[stack.0 as usize].clone() {
            StackNode::Cons(rest, ty) => {
                self.row_in_ty(var_id, ty, seen) || self.row_in_stack(var_id, rest, seen)
            }
            StackNode::Link(next) => self.row_in_stack(var_id, next, seen),
            _ => false,
        }
    }

    fn show_ty(&self, id: TyId) -> String {
        self.show_ty_at(id, 0)
    }

    fn show_ty_at(&self, id: TyId, depth: usize) -> String {
        if depth > 32 {
            return "_".to_string();
        }
        match self.tys[self.find_ty(id).0 as usize].clone() {
            TyNode::Var | TyNode::Template => "_".to_string(),
            TyNode::Rigid(name) => name,
            TyNode::Link(next) => self.show_ty_at(next, depth + 1),
            TyNode::Ctor(name, args) if args.is_empty() => name,
            TyNode::Ctor(name, args) => {
                let mut out = format!("({name}");
                for arg in args {
                    out.push(' ');
                    out.push_str(&self.show_ty_at(arg, depth + 1));
                }
                out.push(')');
                out
            }
            TyNode::App(head, args) => {
                let mut out = format!("({}", self.show_ty_at(head, depth + 1));
                for arg in args {
                    out.push(' ');
                    out.push_str(&self.show_ty_at(arg, depth + 1));
                }
                out.push(')');
                out
            }
            TyNode::Fun(input, output, effect) => {
                format!(
                    "({}{})",
                    self.show_effect(input, output, depth + 1),
                    self.effect_suffix(effect)
                )
            }
            TyNode::Record(row) => self.show_record(row, depth + 1),
        }
    }

    /// Labels are alphabetical. An unsolved tail is `| _`.
    fn show_record(&self, id: RowId, depth: usize) -> String {
        if depth > 32 {
            return "_".to_string();
        }
        let mut fields = Vec::new();
        let mut seen = HashSet::new();
        let mut current = Some(id);
        let mut guard = 0;
        let tail = loop {
            guard += 1;
            if guard > 64 {
                break Some("_".to_string());
            }
            let Some(row) = current else {
                break None;
            };
            let row = self.find_row(row);
            match &self.rows[row.0 as usize] {
                RowNode::Fields { fields: found, tail } => {
                    for (name, ty) in found {
                        if seen.insert(name.clone()) {
                            fields.push((name.clone(), self.show_ty_at(*ty, depth + 1)));
                        }
                    }
                    current = *tail;
                }
                RowNode::Var | RowNode::Template => break Some("_".to_string()),
                RowNode::Rigid(name) => {
                    break Some(if name.is_empty() {
                        "_".to_string()
                    } else {
                        name.clone()
                    });
                }
                RowNode::Link(next) => current = Some(*next),
            }
        };
        fields.sort_by(|left, right| left.0.cmp(&right.0));
        format_row(&fields, tail.as_deref())
    }

    fn show_effect(&self, input: StackId, output: StackId, depth: usize) -> String {
        let input = self.show_stack_at(input, depth, false);
        let output = self.show_stack_at(output, depth, false);
        match (input.is_empty(), output.is_empty()) {
            (true, true) => "--".to_string(),
            (true, false) => format!("-- {output}"),
            (false, true) => format!("{input} --"),
            (false, false) => format!("{input} -- {output}"),
        }
    }

    fn show_stack(&self, id: StackId) -> String {
        self.show_stack_at(id, 0, false)
    }

    /// Like [`show_stack`], but a lone hidden signature tail is `.`.
    fn show_note(&self, id: StackId) -> String {
        let text = self.show_stack_at(id, 0, true);
        if text.is_empty() {
            ".".to_string()
        } else {
            text
        }
    }

    fn show_note_effect(&self, input: StackId, output: StackId, effect: EffectId) -> String {
        let input = self.show_note(input);
        let output = self.show_note(output);
        let line = match (input.as_str(), output.as_str()) {
            (".", ".") => "--".to_string(),
            (".", output) => format!("-- {output}"),
            (input, ".") => format!("{input} --"),
            (input, output) => format!("{input} -- {output}"),
        };
        format!("{line}{}", self.effect_suffix(effect))
    }

    /// A bare open row prints as nothing, so a pure word keeps `Int -- Int`.
    fn effect_suffix(&self, effect: EffectId) -> String {
        if self.effect_is_bare(effect) {
            String::new()
        } else {
            format!(" ! {}", self.show_effect_row(effect))
        }
    }

    fn effect_is_bare(&self, effect: EffectId) -> bool {
        match self.peel_effect(effect) {
            Ok((labels, Some(_))) => labels.is_empty(),
            _ => false,
        }
    }

    /// Labels are alphabetical. An unsolved tail is `| _`.
    fn show_effect_row(&self, id: EffectId) -> String {
        let Ok((labels, tail)) = self.peel_effect(id) else {
            return "_".to_string();
        };
        let tail = tail.map(|tail| self.show_effect_tail(tail));
        let labels: Vec<String> = labels.into_iter().collect();
        format_effect(&labels, tail.as_deref())
    }

    fn show_effect_tail(&self, id: EffectId) -> String {
        match &self.effects[self.find_effect(id).0 as usize] {
            EffectNode::Rigid(name) if !name.is_empty() => name.clone(),
            _ => "_".to_string(),
        }
    }

    fn render_sites(&self) -> Vec<StackAt> {
        self.sites
            .iter()
            .map(|site| StackAt {
                span: site.span,
                before: self.show_note(site.before),
                after: site.after.map(|id| self.show_note(id)),
                here: site
                    .here
                    .map(|(input, output, effect)| self.show_note_effect(input, output, effect)),
            })
            .collect()
    }

    fn show_stack_at(&self, id: StackId, depth: usize, hide_lone_hidden: bool) -> String {
        let mut items = Vec::new();
        let mut current = self.find_stack(id);
        let mut guard = 0;
        let bottom = loop {
            guard += 1;
            if guard > 64 || depth > 32 {
                items.push(".._".to_string());
                break Bottom::Open;
            }
            match self.stacks[current.0 as usize].clone() {
                StackNode::Cons(rest, ty) => {
                    items.push(self.show_ty_at(ty, depth + 1));
                    current = self.find_stack(rest);
                }
                StackNode::Empty => break Bottom::Empty,
                StackNode::Var | StackNode::Template | StackNode::Link(_) => break Bottom::Open,
                StackNode::Rigid { name, hidden } => {
                    break if hidden {
                        Bottom::Hidden
                    } else {
                        Bottom::Named(name)
                    };
                }
            }
        };
        items.reverse();
        let mut parts = Vec::new();
        match bottom {
            Bottom::Hidden if !items.is_empty() => {}
            Bottom::Hidden if hide_lone_hidden => {}
            Bottom::Hidden | Bottom::Open => parts.push(".._".to_string()),
            Bottom::Empty => parts.push(".".to_string()),
            Bottom::Named(name) => parts.push(format!("..{name}")),
        }
        parts.extend(items);
        parts.join(" ")
    }

    /// Follow links to a node that is not a link. A repeated id is a link cycle.
    fn find_ty(&self, mut id: TyId) -> TyId {
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(id) {
                return id;
            }
            match &self.tys[id.0 as usize] {
                TyNode::Link(next) => id = *next,
                _ => return id,
            }
        }
    }

    /// Follow links to a node that is not a link. A repeated id is a link cycle.
    fn find_row(&self, mut id: RowId) -> RowId {
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(id) {
                return id;
            }
            match &self.rows[id.0 as usize] {
                RowNode::Link(next) => id = *next,
                _ => return id,
            }
        }
    }

    /// Follow links to a node that is not a link. A repeated id is a link cycle.
    fn find_stack(&self, mut id: StackId) -> StackId {
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(id) {
                return id;
            }
            match &self.stacks[id.0 as usize] {
                StackNode::Link(next) => id = *next,
                _ => return id,
            }
        }
    }

    fn push_ty(&mut self, node: TyNode) -> TyId {
        let id = TyId(self.tys.len() as u32);
        self.tys.push(node);
        id
    }

    fn push_stack(&mut self, node: StackNode) -> StackId {
        let id = StackId(self.stacks.len() as u32);
        self.stacks.push(node);
        id
    }

    fn push_row(&mut self, node: RowNode) -> RowId {
        let id = RowId(self.rows.len() as u32);
        self.rows.push(node);
        id
    }

    fn ty_var(&mut self) -> TyId {
        self.push_ty(TyNode::Var)
    }

    fn ty_rigid(&mut self, name: &str) -> TyId {
        self.push_ty(TyNode::Rigid(name.to_string()))
    }

    fn ty_ctor(&mut self, name: &str, args: Vec<TyId>) -> TyId {
        self.push_ty(TyNode::Ctor(name.to_string(), args))
    }

    fn ty_fun(&mut self, input: StackId, output: StackId, effect: EffectId) -> TyId {
        self.push_ty(TyNode::Fun(input, output, effect))
    }

    fn ty_record(&mut self, row: RowId) -> TyId {
        self.push_ty(TyNode::Record(row))
    }

    fn row_var(&mut self) -> RowId {
        self.push_row(RowNode::Var)
    }

    fn row_rigid(&mut self, name: &str) -> RowId {
        self.push_row(RowNode::Rigid(name.to_string()))
    }

    fn row_fields(&mut self, fields: Vec<(String, TyId)>, tail: Option<RowId>) -> RowId {
        self.push_row(RowNode::Fields { fields, tail })
    }

    fn push_effect(&mut self, node: EffectNode) -> EffectId {
        let id = EffectId(self.effects.len() as u32);
        self.effects.push(node);
        id
    }

    fn effect_var(&mut self) -> EffectId {
        self.push_effect(EffectNode::Var)
    }

    fn effect_rigid(&mut self, name: &str) -> EffectId {
        self.push_effect(EffectNode::Rigid(name.to_string()))
    }

    fn effect_labels(&mut self, labels: Vec<String>, tail: Option<EffectId>) -> EffectId {
        self.push_effect(EffectNode::Labels { labels, tail })
    }

    /// Follow links to a node that is not a link. A repeated id is a link cycle.
    fn find_effect(&self, mut id: EffectId) -> EffectId {
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(id) {
                return id;
            }
            match &self.effects[id.0 as usize] {
                EffectNode::Link(next) => id = *next,
                _ => return id,
            }
        }
    }

    fn stack_var(&mut self) -> StackId {
        self.push_stack(StackNode::Var)
    }

    fn stack_rigid(&mut self, name: &str) -> StackId {
        self.push_stack(StackNode::Rigid {
            name: name.to_string(),
            hidden: name.is_empty(),
        })
    }

    fn stack_empty(&mut self) -> StackId {
        if let Some(id) = self.empty {
            return id;
        }
        let id = self.push_stack(StackNode::Empty);
        self.empty = Some(id);
        id
    }

    fn stack_cons(&mut self, rest: StackId, ty: TyId) -> StackId {
        self.push_stack(StackNode::Cons(rest, ty))
    }

    fn kind_type(&mut self) -> KindId {
        if let Some(id) = self.type_kind {
            return id;
        }
        let id = self.push_kind(KindNode::Type);
        self.type_kind = Some(id);
        id
    }

    fn kind_var(&mut self) -> KindId {
        self.push_kind(KindNode::Var)
    }

    fn kind_arrow(&mut self, arg: KindId, result: KindId) -> KindId {
        self.push_kind(KindNode::Arrow(arg, result))
    }

    fn push_kind(&mut self, node: KindNode) -> KindId {
        let id = KindId(self.kinds.len() as u32);
        self.kinds.push(node);
        id
    }

    fn ty_of_kind(&mut self, kind: KindId) -> TyId {
        let id = self.push_ty(TyNode::Template);
        self.ty_kind.insert(id, kind);
        id
    }

    fn ctor_kind(&mut self, arity: usize) -> KindId {
        let mut kind = self.kind_type();
        for _ in 0..arity {
            let arg = self.kind_type();
            kind = self.kind_arrow(arg, kind);
        }
        kind
    }

    /// Follow links to a node that is not a link. A repeated id is a link cycle.
    fn find_kind(&self, mut id: KindId) -> KindId {
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(id) {
                return id;
            }
            match &self.kinds[id.0 as usize] {
                KindNode::Link(next) => id = *next,
                _ => return id,
            }
        }
    }

    fn unify_kind(&mut self, have: KindId, need: KindId) -> Result<(), Error> {
        let have = self.find_kind(have);
        let need = self.find_kind(need);
        if have == need {
            return Ok(());
        }
        match (
            self.kinds[have.0 as usize].clone(),
            self.kinds[need.0 as usize].clone(),
        ) {
            (KindNode::Var, _) => self.bind_kind(have, need),
            (_, KindNode::Var) => self.bind_kind(need, have),
            (KindNode::Type, KindNode::Type) => Ok(()),
            (KindNode::Arrow(have_arg, have_ret), KindNode::Arrow(need_arg, need_ret)) => {
                self.unify_kind(have_arg, need_arg)?;
                self.unify_kind(have_ret, need_ret)
            }
            _ => Err(Error::KindMismatch { expected: self.show_kind(need), found: self.show_kind(have) }),
        }
    }

    fn bind_kind(&mut self, var_id: KindId, target: KindId) -> Result<(), Error> {
        if self.kind_occurs(var_id, target, &mut Seen::default()) {
            return Err(Error::OccursCheck);
        }
        self.kinds[var_id.0 as usize] = KindNode::Link(target);
        Ok(())
    }

    fn kind_occurs(&self, var_id: KindId, kind: KindId, seen: &mut Seen) -> bool {
        let kind = self.find_kind(kind);
        if kind == var_id {
            return true;
        }
        if !seen.kind.insert(kind) {
            return false;
        }
        match &self.kinds[kind.0 as usize] {
            KindNode::Arrow(arg, result) => {
                self.kind_occurs(var_id, *arg, seen) || self.kind_occurs(var_id, *result, seen)
            }
            KindNode::Link(next) => self.kind_occurs(var_id, *next, seen),
            _ => false,
        }
    }

    fn default_kind(&mut self, kind: KindId) {
        let kind = self.find_kind(kind);
        match self.kinds[kind.0 as usize].clone() {
            KindNode::Var => self.kinds[kind.0 as usize] = KindNode::Type,
            KindNode::Arrow(arg, result) => {
                self.default_kind(arg);
                self.default_kind(result);
            }
            _ => {}
        }
    }

    fn show_kind(&self, kind: KindId) -> String {
        self.show_kind_prec(kind, false)
    }

    fn show_kind_prec(&self, kind: KindId, atom: bool) -> String {
        match self.kinds[self.find_kind(kind).0 as usize].clone() {
            KindNode::Type => "Type".to_string(),
            KindNode::Var => "_".to_string(),
            KindNode::Link(next) => self.show_kind_prec(next, atom),
            KindNode::Arrow(arg, result) => {
                let text = format!(
                    "{} -> {}",
                    self.show_kind_prec(arg, true),
                    self.show_kind_prec(result, false)
                );
                if atom {
                    format!("({text})")
                } else {
                    text
                }
            }
        }
    }

    fn register_class(&mut self, class: &Class) -> Result<(), Diagnostic> {
        if self.classes.contains_key(&class.name) {
            return Err(self.error(
                class.name_span,
                Error::DuplicateDefinition(class.name.to_string()),
            ));
        }
        self.install_class(class, true)
    }

    fn ensure_class(&mut self, class: &Class) -> Result<(), Diagnostic> {
        if self.classes.contains_key(&class.name) {
            return Ok(());
        }
        self.install_class(class, false)
    }

    fn install_class(&mut self, class: &Class, export_methods: bool) -> Result<(), Diagnostic> {
        if class.params.is_empty() {
            return Err(self.error(
                class.name_span,
                Error::ClassNeedsParameter(class.name.to_string()),
            ));
        }
        let mut params = Vec::new();
        for param in &class.params {
            if params.iter().any(|(name, _)| name == param) {
                return Err(self.error(
                    class.name_span,
                    Error::DuplicateTypeVariable(param.to_string()),
                ));
            }
            let kind = self.kind_var();
            params.push((param.clone(), self.ty_of_kind(kind)));
        }
        // Reinstalling a class from an import closure elaborates constraints
        // on classes that are installed but not named in this import list.
        let saved_relax = self.relax_visibility;
        if !export_methods {
            self.relax_visibility = true;
        }
        let methods = self.elaborate_methods(class, &params, export_methods);
        self.relax_visibility = saved_relax;
        let methods = methods?;
        // A methodless class learns its parameter kind from the superclasses.
        // Defaulting first would turn that variable into `Type`.
        let supers = self.elaborate_supers(class, &params, export_methods)?;
        for (_, id) in &params {
            if let Some(kind) = self.ty_kind.get(id).copied() {
                self.default_kind(kind);
            }
        }
        if export_methods {
            self.visible_classes.insert(class.name.clone());
            self.defined_classes.insert(class.name.clone());
        }
        self.classes.insert(
            class.name.clone(),
            ClassInfo {
                params,
                methods,
                supers,
            },
        );
        Ok(())
    }

    fn elaborate_methods(
        &mut self,
        class: &Class,
        params: &[(String, TyId)],
        export_methods: bool,
    ) -> Result<Vec<ClassMethod>, Diagnostic> {
        let mut methods = Vec::new();
        for method in &class.methods {
            if methods.iter().any(|have: &ClassMethod| have.name == method.name) {
                return Err(self.error(
                    method.name_span,
                    Error::DuplicateDefinition(method.name.to_string()),
                ));
            }
            let mut scope = Scope::new();
            for (name, id) in params {
                scope.tys.insert(name.clone(), *id);
                scope.quantified_tys.push((name.clone(), *id));
            }
            let preset = scope.quantified_tys.len();
            let mut scheme = self.elaborate_with(&method.scheme, method.name_span, scope, preset)?;
            scheme.constraints.insert(
                0,
                SemConstraint {
                    class_name: class.name.clone(),
                    args: params.iter().map(|(_, id)| *id).collect(),
                },
            );
            let arity = self.prefix_len(scheme.input);
            if export_methods {
                self.export_word(&method.name, scheme.clone(), method.name_span)?;
            }
            methods.push(ClassMethod {
                name: method.name.clone(),
                scheme,
                arity,
            });
        }
        Ok(methods)
    }

    /// Superclass arguments are the class parameters. A free name is not quantified.
    /// An unsolved parameter kind is still a variable here. It is defaulted to
    /// `Type` after the superclasses have been elaborated.
    fn elaborate_supers(
        &mut self,
        class: &Class,
        params: &[(String, TyId)],
        local: bool,
    ) -> Result<Vec<SemConstraint>, Diagnostic> {
        let mut scope = Scope::new();
        for (name, id) in params {
            scope.tys.insert(name.clone(), *id);
        }
        let saved = self.relax_visibility;
        // Re-installing an imported class sees superclasses that are installed
        // but not named in this import list.
        if !local {
            self.relax_visibility = true;
        }
        let mut seen = HashSet::new();
        let mut supers = Vec::new();
        for constraint in &class.supers {
            if constraint.class_name == class.name
                || self.superclass_reaches(&constraint.class_name, &class.name)
            {
                self.relax_visibility = saved;
                return Err(self.error(
                    class.name_span,
                    Error::CyclicSuperclass(class.name.to_string()),
                ));
            }
            if !seen.insert(constraint.class_name.clone()) {
                self.relax_visibility = saved;
                return Err(self.error(
                    class.name_span,
                    Error::DuplicateDefinition(constraint.class_name.to_string()),
                ));
            }
            match self.elab_constraint(constraint, &mut scope, class.name_span) {
                Ok(sem) => supers.push(sem),
                Err(error) => {
                    self.relax_visibility = saved;
                    return Err(error);
                }
            }
        }
        self.relax_visibility = saved;
        Ok(supers)
    }

    fn superclass_reaches(&self, start: &str, target: &str) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(class) = self.classes.get(&name) else {
                continue;
            };
            for sup in &class.supers {
                if sup.class_name == target {
                    return true;
                }
                stack.push(sup.class_name.clone());
            }
        }
        false
    }

    fn elab_constraint(
        &mut self,
        constraint: &crate::ast::Constraint,
        scope: &mut Scope,
        span: Span,
    ) -> Result<SemConstraint, Diagnostic> {
        if !self.relax_visibility && !self.visible_classes.contains(&constraint.class_name) {
            return Err(self.error(
                span,
                Error::UndefinedClass(constraint.class_name.to_string()),
            ));
        }
        let class = self
            .classes
            .get(&constraint.class_name)
            .cloned()
            .ok_or_else(|| {
                self.error(span, Error::UndefinedClass(constraint.class_name.to_string()))
            })?;
        if constraint.args.len() != class.params.len() {
            let count = class.params.len();
            return Err(self.error(
                span,
                Error::ClassArity { name: constraint.class_name.to_string(), count },
            ));
        }
        let mut args = Vec::new();
        for (arg, (_, param)) in constraint.args.iter().zip(&class.params) {
            let (ty, kind) = self.elab_ty(arg, scope, span)?;
            let expect = match self.ty_kind.get(param).copied() {
                Some(kind) => kind,
                None => self.kind_type(),
            };
            self.unify_kind(kind, expect)
                .map_err(|message| self.error(span, message))?;
            args.push(ty);
        }
        Ok(SemConstraint {
            class_name: constraint.class_name.clone(),
            args,
        })
    }

    fn export_imported_method(
        &mut self,
        provider: &Module,
        name: &str,
        span: Span,
    ) -> Result<bool, Diagnostic> {
        let Some(class) = provider.items.iter().find_map(|item| match item {
            Item::Class(class) if class.methods.iter().any(|method| method.name == name) => {
                Some(class.clone())
            }
            _ => None,
        }) else {
            return Ok(false);
        };
        self.ensure_class(&class)?;
        let scheme = self
            .classes
            .get(&class.name)
            .and_then(|info| {
                info.methods
                    .iter()
                    .find(|method| method.name == name)
                    .map(|method| method.scheme.clone())
            })
            .expect("class method was installed");
        self.export_word(name, scheme, span)?;
        Ok(true)
    }

    fn import_instances(&mut self, module: &Module) -> Result<(), Diagnostic> {
        for item in &module.items {
            if let Item::Instance(instance) = item {
                self.declare_instance(instance, module, false)?;
            }
        }
        Ok(())
    }

    fn declare_instance(
        &mut self,
        instance: &Instance,
        module: &Module,
        local: bool,
    ) -> Result<(), Diagnostic> {
        let mut seen_classes = HashSet::new();
        for name in &instance.class_names {
            if !seen_classes.insert(name.clone()) {
                return Err(self.error(instance.span, Error::DuplicateDefinition(name.to_string())));
            }
        }
        let mut classes = Vec::new();
        for name in &instance.class_names {
            let class = self.classes.get(name).cloned().ok_or_else(|| {
                self.error(instance.span, Error::UndefinedClass(name.to_string()))
            })?;
            classes.push(class);
        }
        let mut seen = HashSet::new();
        for binding in &instance.methods {
            if !seen.insert(binding.name.clone()) {
                return Err(self.error(
                    binding.name_span,
                    Error::DuplicateDefinition(binding.name.to_string()),
                ));
            }
            let owners: Vec<&str> = instance
                .class_names
                .iter()
                .zip(&classes)
                .filter(|(_, class)| class.methods.iter().any(|method| method.name == binding.name))
                .map(|(name, _)| name.as_str())
                .collect();
            if owners.len() != 1 {
                let error = if owners.is_empty() {
                    Error::MethodNotOf {
                        name: binding.name.to_string(),
                        classes: instance.class_names.join(", "),
                    }
                } else {
                    Error::MethodOfSeveral {
                        name: binding.name.to_string(),
                        owners: owners.join(" and "),
                    }
                };
                return Err(self.error(binding.name_span, error));
            }
        }
        for (name, class) in instance.class_names.iter().zip(&classes) {
            self.declare_class_instance(instance, module, local, name, class)?;
        }
        Ok(())
    }

    /// One class from an instance declaration. The head is elaborated again so
    /// each class sees the argument kinds afresh.
    fn declare_class_instance(
        &mut self,
        instance: &Instance,
        module: &Module,
        local: bool,
        class_name: &str,
        class: &ClassInfo,
    ) -> Result<(), Diagnostic> {
        if local && self.orphan(class_name, &instance.args) {
            return Err(self.error(instance.span, Error::OrphanInstance));
        }
        if instance.args.len() != class.params.len() {
            let count = class.params.len();
            return Err(self.error(
                instance.span,
                Error::ClassArity { name: class_name.to_string(), count },
            ));
        }
        let mut names = Vec::new();
        for arg in &instance.args {
            collect_ty_vars(arg, &mut names);
        }
        let mut scope = Scope::new();
        for name in &names {
            let kind = self.kind_var();
            let id = self.ty_of_kind(kind);
            scope.tys.insert(name.clone(), id);
            scope.quantified_tys.push((name.clone(), id));
        }
        let mut head = Vec::new();
        for (arg, (_, param)) in instance.args.iter().zip(&class.params) {
            let (ty, kind) = self.elab_ty(arg, &mut scope, instance.span)?;
            let expect = match self.ty_kind.get(param).copied() {
                Some(kind) => kind,
                None => self.kind_type(),
            };
            self.unify_kind(kind, expect)
                .map_err(|message| self.error(instance.span, message))?;
            head.push(ty);
        }
        for (_, id) in &scope.quantified_tys {
            if let Some(kind) = self.ty_kind.get(id).copied() {
                self.default_kind(kind);
            }
        }
        let bound: HashSet<&str> = instance
            .methods
            .iter()
            .map(|binding| binding.name.as_str())
            .collect();
        for method in &class.methods {
            if !bound.contains(method.name.as_str()) {
                return Err(self.error(instance.span, Error::MissingMethod(method.name.to_string())));
            }
        }
        let mut methods = Vec::new();
        for method in &class.methods {
            let binding = instance
                .methods
                .iter()
                .find(|binding| binding.name == method.name)
                .expect("method was checked");
            if local {
                if let Some(def) = find_def(module, &binding.word) {
                    if def.vis.is_private() {
                        return Err(self.error(
                            binding.name_span,
                            Error::InstanceWordPrivate(binding.word.to_string()),
                        ));
                    }
                }
            }
            let word = self.lookup_word_scheme(module, &binding.word, binding.name_span)?;
            let (context, extra) =
                self.check_impl(class, method, &word, &head, binding.name_span)?;
            methods.push(InstanceMethod {
                name: method.name.clone(),
                word: binding.word.clone(),
                context,
                extra,
                arity: method.arity,
            });
        }
        let supers = self.substitute_constraints(&class.params, &head, &class.supers);
        self.require_superclass_instances(&supers, instance.span)?;
        let info = InstanceInfo {
            class_name: class_name.to_string(),
            vars: scope.quantified_tys,
            tails: scope.quantified_tails,
            rows: scope.quantified_rows,
            args: head,
            methods,
            supers,
        };
        for existing in self.instances.clone() {
            if self.overlaps(&existing, &info) {
                return Err(self.error(instance.span, Error::IncoherentInstance));
            }
        }
        self.instances.push(info);
        Ok(())
    }

    fn orphan(&self, class_name: &str, args: &[Type]) -> bool {
        if self.defined_classes.contains(class_name) {
            return false;
        }
        !args
            .iter()
            .any(|ty| self.defines_type(ty) || self.builtin_in(ty))
    }

    /// Any builtin constructor in the head counts, including under another type.
    fn builtin_in(&self, ty: &Type) -> bool {
        self.builtin_in_seen(ty, &mut Vec::new())
    }

    fn builtin_in_seen(&self, ty: &Type, seen: &mut Vec<String>) -> bool {
        if let Some((name, rhs)) = self.expand_ast(ty, seen) {
            seen.push(name);
            let found = self.builtin_in_seen(&rhs, seen);
            seen.pop();
            return found;
        }
        match ty {
            Type::Ctor(name) => builtin::builtin_type(name),
            Type::App(head, args) => {
                self.builtin_in_seen(head, seen)
                    || args.iter().any(|arg| self.builtin_in_seen(arg, seen))
            }
            Type::Fun(effect) => effect
                .input
                .items
                .iter()
                .chain(&effect.output.items)
                .any(|ty| self.builtin_in_seen(ty, seen)),
            Type::Record(row) => {
                row.fields
                    .iter()
                    .any(|(_, ty)| self.builtin_in_seen(ty, seen))
                    || row
                        .tail_ty()
                        .is_some_and(|ty| self.builtin_in_seen(ty, seen))
            }
            Type::Var(_) => false,
        }
    }

    fn defines_type(&self, ty: &Type) -> bool {
        self.defines_type_seen(ty, &mut Vec::new())
    }

    /// A synonym expands before the orphan rule, so it is not its own head.
    fn defines_type_seen(&self, ty: &Type, seen: &mut Vec<String>) -> bool {
        if let Some((name, rhs)) = self.expand_ast(ty, seen) {
            seen.push(name);
            let owns = self.defines_type_seen(&rhs, seen);
            seen.pop();
            return owns;
        }
        match ty {
            Type::Ctor(name) => self.defined_data.contains(name),
            Type::App(head, args) => {
                self.defines_type_seen(head, seen)
                    || args.iter().any(|arg| self.defines_type_seen(arg, seen))
            }
            Type::Fun(effect) => {
                effect
                    .input
                    .items
                    .iter()
                    .chain(&effect.output.items)
                    .any(|ty| self.defines_type_seen(ty, seen))
            }
            Type::Record(row) => {
                row.fields
                    .iter()
                    .any(|(_, ty)| self.defines_type_seen(ty, seen))
                    || row
                        .tail_ty()
                        .is_some_and(|ty| self.defines_type_seen(ty, seen))
            }
            Type::Var(_) => false,
        }
    }

    fn expand_ast(&self, ty: &Type, seen: &[String]) -> Option<(String, Type)> {
        let (name, args) = match ty {
            Type::Ctor(name) => (name, Vec::new()),
            Type::App(head, args) => {
                let Type::Ctor(name) = head.as_ref() else {
                    return None;
                };
                (name, args.clone())
            }
            _ => return None,
        };
        if seen.iter().any(|have| have == name) || !self.syn_in_scope(name) {
            return None;
        }
        let body = self.synonyms.get(name)?;
        if args.len() != body.params.len() {
            return None;
        }
        Some((name.clone(), subst_type(&body.rhs, &body.params, &args)))
    }

    fn lookup_word_scheme(
        &mut self,
        module: &Module,
        name: &str,
        span: Span,
    ) -> Result<SemScheme, Diagnostic> {
        // The provider's own definition is the instance. An importer often
        // already has the class method under that name, and that scheme is
        // not the implementation.
        if let Some(def) = find_def(module, name) {
            let saved = self.relax_visibility;
            self.relax_visibility = true;
            let result = self.elaborate_scheme(&def.scheme, def.name_span);
            self.relax_visibility = saved;
            return result;
        }
        if let Some(scheme) = self.lookup(name) {
            return Ok(scheme);
        }
        Err(self.error(span, Error::UndefinedWord(name.to_string())))
    }

    /// The implementation word lists head dictionaries first and the method's own
    /// constraints last. A head dictionary is solved when the instance dictionary
    /// is built. A method constraint mentions a variable of the method, so the
    /// call site passes that dictionary.
    fn check_impl(
        &mut self,
        class: &ClassInfo,
        method: &ClassMethod,
        word: &SemScheme,
        head: &[TyId],
        span: Span,
    ) -> Result<(Vec<SemConstraint>, usize), Diagnostic> {
        let (need_in, need_out, need_effect, skolems, extras) =
            self.method_at_head(class, method, head);
        let (have_in, have_out, have_effect, context) = self.instantiate_scheme(word, false);
        self.unify_stack(have_in, need_in)
            .map_err(|message| self.error(span, message))?;
        self.unify_stack(have_out, need_out)
            .map_err(|message| self.error(span, message))?;
        self.unify_effect(have_effect, need_effect)
            .map_err(|message| self.error(span, message))?;
        if context.len() < extras.len() {
            return Err(self.error(
                span,
                Error::TypeMismatch {
                    expected: self.show_constraints(&extras),
                    found: self.show_constraints(&context),
                },
            ));
        }
        let split = context.len() - extras.len();
        for (have, need) in context[split..].iter().zip(&extras) {
            if have.class_name != need.class_name || have.args.len() != need.args.len() {
                return Err(self.error(
                    span,
                    Error::TypeMismatch {
                        expected: self.show_constraint(need),
                        found: self.show_constraint(have),
                    },
                ));
            }
            for (have_arg, need_arg) in have.args.iter().zip(&need.args) {
                self.unify_ty(*have_arg, *need_arg)
                    .map_err(|message| self.error(span, message))?;
            }
        }
        let mut kept = Vec::new();
        for constraint in &context[..split] {
            let shown = self.show_constraint(constraint);
            if constraint
                .args
                .iter()
                .any(|arg| self.ty_has_skolem(*arg, &skolems, &mut Seen::default()))
            {
                return Err(self.error(span, Error::AmbiguousConstraint(shown)));
            }
            for arg in &constraint.args {
                if self.flexible(*arg, &mut Seen::default()) {
                    return Err(self.error(span, Error::AmbiguousConstraint(shown)));
                }
            }
            kept.push(constraint.clone());
        }
        Ok((kept, extras.len()))
    }

    /// A method variable that is not the class parameter. The implementation
    /// must work for every such variable, so a constraint still mentioning it
    /// is not determined by the instance head.
    fn ty_has_skolem(&self, ty: TyId, skolems: &[TyId], seen: &mut Seen) -> bool {
        let ty = self.find_ty(ty);
        if skolems.contains(&ty) {
            return true;
        }
        if !seen.ty.insert(ty) {
            return false;
        }
        match self.tys[ty.0 as usize].clone() {
            TyNode::Ctor(_, args) => args
                .into_iter()
                .any(|arg| self.ty_has_skolem(arg, skolems, seen)),
            TyNode::App(head, args) => {
                self.ty_has_skolem(head, skolems, seen)
                    || args
                        .into_iter()
                        .any(|arg| self.ty_has_skolem(arg, skolems, seen))
            }
            TyNode::Fun(input, output, _) => {
                self.stack_has_skolem(input, skolems, seen)
                    || self.stack_has_skolem(output, skolems, seen)
            }
            TyNode::Record(row) => self.row_has_skolem(row, skolems, seen),
            TyNode::Link(next) => self.ty_has_skolem(next, skolems, seen),
            _ => false,
        }
    }

    fn stack_has_skolem(&self, stack: StackId, skolems: &[TyId], seen: &mut Seen) -> bool {
        let stack = self.find_stack(stack);
        if !seen.stack.insert(stack) {
            return false;
        }
        match self.stacks[stack.0 as usize].clone() {
            StackNode::Cons(rest, ty) => {
                self.stack_has_skolem(rest, skolems, seen)
                    || self.ty_has_skolem(ty, skolems, seen)
            }
            StackNode::Link(next) => self.stack_has_skolem(next, skolems, seen),
            _ => false,
        }
    }

    fn row_has_skolem(&self, row: RowId, skolems: &[TyId], seen: &mut Seen) -> bool {
        let row = self.find_row(row);
        if !seen.row.insert(row) {
            return false;
        }
        match self.rows[row.0 as usize].clone() {
            RowNode::Fields { fields, tail } => {
                fields
                    .into_iter()
                    .any(|(_, ty)| self.ty_has_skolem(ty, skolems, seen))
                    || tail.is_some_and(|tail| self.row_has_skolem(tail, skolems, seen))
            }
            RowNode::Link(next) => self.row_has_skolem(next, skolems, seen),
            _ => false,
        }
    }

    fn method_at_head(
        &mut self,
        class: &ClassInfo,
        method: &ClassMethod,
        head: &[TyId],
    ) -> (StackId, StackId, EffectId, Vec<TyId>, Vec<SemConstraint>) {
        let mut subst = Subst::default();
        let mut skolems = Vec::new();
        for ((_, param), arg) in class.params.iter().zip(head) {
            subst.ty.insert(*param, *arg);
        }
        for (name, id) in &method.scheme.tys {
            if subst.ty.contains_key(id) {
                continue;
            }
            let fresh = self.ty_rigid(name);
            skolems.push(fresh);
            if let Some(kind) = self.ty_kind.get(id).copied() {
                self.ty_kind.insert(fresh, kind);
            }
            subst.ty.insert(*id, fresh);
        }
        for tail in &method.scheme.tails {
            subst.stack.insert(tail.id, self.stack_var());
        }
        for row in &method.scheme.rows {
            subst.row.insert(row.id, self.row_var());
        }
        for effect in &method.scheme.effect_vars {
            let fresh = if effect.skolem {
                self.effect_rigid(&effect.name)
            } else {
                self.effect_var()
            };
            subst.effect.insert(effect.id, fresh);
        }
        let mut memo = Memo::default();
        let input = self.copy_stack(method.scheme.input, &subst, &mut memo);
        let output = self.copy_stack(method.scheme.output, &subst, &mut memo);
        let effect = self.copy_effect(method.scheme.effect, &subst, &mut memo);
        // The first constraint is the class itself, inserted when the class was installed.
        let extras = if method.scheme.constraints.is_empty() {
            Vec::new()
        } else {
            self.copy_constraints(&method.scheme.constraints[1..], &subst, &mut memo)
        };
        (input, output, effect, skolems, extras)
    }

    fn overlaps(&mut self, left: &InstanceInfo, right: &InstanceInfo) -> bool {
        if left.class_name != right.class_name || left.args.len() != right.args.len() {
            return false;
        }
        let (left_args, _, _) = self.copy_instance(left);
        let (right_args, _, _) = self.copy_instance(right);
        self.trial(|checker| {
            for (left_arg, right_arg) in left_args.iter().zip(&right_args) {
                checker.unify_ty(*left_arg, *right_arg)?;
            }
            Ok(())
        })
        .is_ok()
    }

    fn copy_instance(
        &mut self,
        inst: &InstanceInfo,
    ) -> (Vec<TyId>, Vec<InstanceMethod>, Vec<SemConstraint>) {
        let mut subst = Subst::default();
        for (_, id) in &inst.vars {
            subst.ty.insert(*id, self.ty_var());
        }
        for tail in &inst.tails {
            subst.stack.insert(tail.id, self.stack_var());
        }
        for row in &inst.rows {
            subst.row.insert(row.id, self.row_var());
        }
        let mut memo = Memo::default();
        let args = inst
            .args
            .iter()
            .map(|arg| self.copy_ty(*arg, &subst, &mut memo))
            .collect();
        let methods = inst
            .methods
            .iter()
            .map(|method| InstanceMethod {
                name: method.name.clone(),
                word: method.word.clone(),
                arity: method.arity,
                extra: method.extra,
                context: self.copy_constraints(&method.context, &subst, &mut memo),
            })
            .collect();
        let supers = self.copy_constraints(&inst.supers, &subst, &mut memo);
        (args, methods, supers)
    }

    fn copy_constraints(
        &mut self,
        constraints: &[SemConstraint],
        subst: &Subst,
        memo: &mut Memo,
    ) -> Vec<SemConstraint> {
        constraints
            .iter()
            .map(|constraint| SemConstraint {
                class_name: constraint.class_name.clone(),
                args: constraint
                    .args
                    .iter()
                    .map(|arg| self.copy_ty(*arg, subst, memo))
                    .collect(),
            })
            .collect()
    }

    /// Class parameters are replaced by `args`. The arguments stay the same ids.
    fn substitute_constraints(
        &mut self,
        params: &[(String, TyId)],
        args: &[TyId],
        constraints: &[SemConstraint],
    ) -> Vec<SemConstraint> {
        let mut subst = Subst::default();
        for ((_, param), arg) in params.iter().zip(args) {
            subst.ty.insert(*param, *arg);
        }
        let mut memo = Memo::default();
        self.copy_constraints(constraints, &subst, &mut memo)
    }

    /// Solve now when the head is concrete. `empty` and `pure` pin it from the
    /// result, which is not known until the body has been unified.
    fn solve_or_defer(
        &mut self,
        constraints: &[SemConstraint],
        span: Span,
    ) -> Result<(), Diagnostic> {
        if self.constraints_flexible(constraints) {
            self.deferred.push(DeferredUse {
                span,
                constraints: constraints.to_vec(),
            });
            return Ok(());
        }
        self.solve_all(constraints, span)?;
        Ok(())
    }

    fn constraints_flexible(&self, constraints: &[SemConstraint]) -> bool {
        constraints
            .iter()
            .any(|constraint| constraint.args.iter().any(|ty| self.flexible(*ty, &mut Seen::default())))
    }

    fn dicts_for_use(
        &mut self,
        constraints: &[SemConstraint],
        span: Span,
    ) -> Result<Option<Vec<Dict>>, Diagnostic> {
        if let Some(dicts) = self.use_dicts.get(&span) {
            return Ok(Some(dicts.clone()));
        }
        if self.constraints_flexible(constraints) {
            self.deferred.push(DeferredUse {
                span,
                constraints: constraints.to_vec(),
            });
            return Ok(None);
        }
        Ok(Some(self.solve_all(constraints, span)?))
    }

    fn finish_deferred(&mut self) -> Result<(), Diagnostic> {
        let pending = std::mem::take(&mut self.deferred);
        for item in pending {
            if let Some(constraint) = item.constraints.iter().find(|constraint| {
                constraint.args.iter().any(|ty| self.flexible(*ty, &mut Seen::default()))
            }) {
                let shown = self.show_constraint(constraint);
                return Err(self.error(item.span, Error::AmbiguousConstraint(shown)));
            }
            let dicts = self.solve_all(&item.constraints, item.span)?;
            self.use_dicts.insert(item.span, dicts);
        }
        Ok(())
    }

    fn solve_all(
        &mut self,
        constraints: &[SemConstraint],
        span: Span,
    ) -> Result<Vec<Dict>, Diagnostic> {
        let mut dicts = Vec::new();
        for constraint in constraints {
            dicts.push(self.solve_one(constraint, span)?);
        }
        Ok(dicts)
    }

    fn solve_one(&mut self, constraint: &SemConstraint, span: Span) -> Result<Dict, Diagnostic> {
        let key = self.show_constraint(constraint);
        if self.solving.iter().any(|seen| seen == &key) {
            return Err(self.error(span, Error::CyclicInstance(key)));
        }
        if let Some(index) = self.match_assumption(constraint) {
            return Ok(Dict::Assumption(index));
        }
        if let Some(dict) = self.entail(constraint, span)? {
            return Ok(dict);
        }
        self.solving.push(key);
        let saved = (
            self.frozen_ty.clone(),
            self.frozen_stack.clone(),
            self.frozen_row.clone(),
            self.frozen_effect.clone(),
        );
        for arg in &constraint.args {
            self.freeze_ty(*arg);
        }
        let instances = self.instances.clone();
        let mut found = None;
        let mut failure = None;
        for inst in &instances {
            if inst.class_name != constraint.class_name {
                continue;
            }
            let Ok((methods, supers)) = self.match_instance(inst, &constraint.args) else {
                continue;
            };
            let mut resolved = Vec::new();
            let mut skip = false;
            for method in &methods {
                match self.solve_all(&method.context, span) {
                    Ok(dicts) => resolved.push(ResolvedMethod {
                        name: method.name.clone(),
                        word: method.word.clone(),
                        dicts,
                        extra: method.extra,
                        arity: method.arity,
                    }),
                    Err(error) if error.code == "PL0088" => {
                        skip = true;
                        break;
                    }
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            if failure.is_some() {
                break;
            }
            if skip {
                continue;
            }
            let mut super_dicts = Vec::new();
            for sup in &supers {
                match self.solve_one(sup, span) {
                    Ok(dict) => super_dicts.push((sup.class_name.clone(), dict)),
                    Err(error) if error.code == "PL0088" => {
                        skip = true;
                        break;
                    }
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            if failure.is_some() {
                break;
            }
            if skip {
                continue;
            }
            if found.is_some() {
                failure = Some(self.error(span, Error::IncoherentInstance));
                break;
            }
            found = Some(Dict::Instance {
                methods: resolved,
                supers: super_dicts,
            });
        }
        self.frozen_ty = saved.0;
        self.frozen_stack = saved.1;
        self.frozen_row = saved.2;
        self.frozen_effect = saved.3;
        self.solving.pop();
        if let Some(error) = failure {
            return Err(error);
        }
        found.ok_or_else(|| {
            self.error(
                span,
                Error::NoInstance(self.show_constraint(constraint)),
            )
        })
    }

    fn match_assumption(&mut self, wanted: &SemConstraint) -> Option<usize> {
        let assumptions = self.assumptions.clone();
        for (index, assumption) in assumptions.iter().enumerate() {
            if assumption.class_name != wanted.class_name
                || assumption.args.len() != wanted.args.len()
            {
                continue;
            }
            let saved = (
                self.frozen_ty.clone(),
                self.frozen_stack.clone(),
                self.frozen_row.clone(),
            self.frozen_effect.clone(),
            );
            for arg in assumption.args.iter().chain(&wanted.args) {
                self.freeze_ty(*arg);
            }
            let ok = self
                .trial(|checker| {
                    for (have, need) in assumption.args.iter().zip(&wanted.args) {
                        checker.unify_ty(*have, *need)?;
                    }
                    Ok(())
                })
                .is_ok();
            self.frozen_ty = saved.0;
            self.frozen_stack = saved.1;
            self.frozen_row = saved.2;
        self.frozen_effect = saved.3;
            if ok {
                return Some(index);
            }
        }
        None
    }

    fn match_instance(
        &mut self,
        inst: &InstanceInfo,
        wanted: &[TyId],
    ) -> Result<(Vec<InstanceMethod>, Vec<SemConstraint>), Error> {
        if inst.args.len() != wanted.len() {
            return Err(Error::Arity);
        }
        let (args, methods, supers) = self.copy_instance(inst);
        let saved = (
            self.frozen_ty.clone(),
            self.frozen_stack.clone(),
            self.frozen_row.clone(),
            self.frozen_effect.clone(),
        );
        for arg in wanted {
            self.freeze_ty(*arg);
        }
        let result = self.trial(|checker| {
            for (got, need) in args.iter().zip(wanted) {
                checker.unify_ty(*got, *need)?;
            }
            Ok((methods, supers))
        });
        self.frozen_ty = saved.0;
        self.frozen_stack = saved.1;
        self.frozen_row = saved.2;
        self.frozen_effect = saved.3;
        result
    }

    /// The subclass instance needs each superclass instance for the same head.
    /// A matching head is enough: its method context is solved when the
    /// dictionary is built, which is how `Comparable (Box a)` keeps `Comparable a`.
    fn require_superclass_instances(
        &mut self,
        constraints: &[SemConstraint],
        span: Span,
    ) -> Result<(), Diagnostic> {
        let instances = self.instances.clone();
        for constraint in constraints {
            let matched = instances.iter().any(|inst| {
                inst.class_name == constraint.class_name
                    && self.instance_head_matches(inst, &constraint.args)
            });
            if !matched {
                return Err(self.error(
                    span,
                    Error::NoInstance(self.show_constraint(constraint)),
                ));
            }
        }
        Ok(())
    }

    fn instance_head_matches(&mut self, inst: &InstanceInfo, wanted: &[TyId]) -> bool {
        if inst.args.len() != wanted.len() {
            return false;
        }
        let (args, _, _) = self.copy_instance(inst);
        let saved = (
            self.frozen_ty.clone(),
            self.frozen_stack.clone(),
            self.frozen_row.clone(),
            self.frozen_effect.clone(),
        );
        for arg in wanted {
            self.freeze_ty(*arg);
        }
        let mark = self.trail.len();
        let recording = self.recording;
        self.recording = true;
        let mut ok = true;
        for (got, need) in args.iter().zip(wanted) {
            if self.unify_ty(*got, *need).is_err() {
                ok = false;
                break;
            }
        }
        self.undo(mark);
        self.recording = recording;
        self.frozen_ty = saved.0;
        self.frozen_stack = saved.1;
        self.frozen_row = saved.2;
        self.frozen_effect = saved.3;
        ok
    }

    /// An assumption's superclasses cover `wanted`. One path becomes a chain of
    /// field projections. Two paths, or two assumptions, are incoherent.
    fn entail(
        &mut self,
        wanted: &SemConstraint,
        span: Span,
    ) -> Result<Option<Dict>, Diagnostic> {
        let assumptions = self.assumptions.clone();
        let mut found = None;
        for (index, assumption) in assumptions.iter().enumerate() {
            let paths = self.entail_paths(
                &assumption.class_name,
                &assumption.args,
                wanted,
                &HashSet::new(),
            )?;
            if paths.is_empty() {
                continue;
            }
            if paths.len() > 1 || found.is_some() {
                return Err(self.error(span, Error::IncoherentInstance));
            }
            let mut dict = Dict::Assumption(index);
            for field in &paths[0] {
                dict = Dict::Super {
                    base: Box::new(dict),
                    field: field.clone(),
                };
            }
            found = Some(dict);
        }
        Ok(found)
    }

    fn entail_paths(
        &mut self,
        class_name: &str,
        args: &[TyId],
        wanted: &SemConstraint,
        seen: &HashSet<String>,
    ) -> Result<Vec<Vec<String>>, Diagnostic> {
        if seen.contains(class_name) {
            return Ok(Vec::new());
        }
        let Some(class) = self.classes.get(class_name).cloned() else {
            return Ok(Vec::new());
        };
        let supers = self.substitute_constraints(&class.params, args, &class.supers);
        let mut next_seen = seen.clone();
        next_seen.insert(class_name.to_string());
        let mut paths = Vec::new();
        for sup in &supers {
            if self.constraints_match(sup, wanted) {
                paths.push(vec![sup.class_name.clone()]);
            }
            for mut path in self.entail_paths(&sup.class_name, &sup.args, wanted, &next_seen)? {
                path.insert(0, sup.class_name.clone());
                paths.push(path);
            }
        }
        Ok(paths)
    }

    /// True when both constraints are the same class and the arguments already agree.
    /// Links made by the comparison are dropped: entailment does not instantiate.
    fn constraints_match(&mut self, have: &SemConstraint, need: &SemConstraint) -> bool {
        if have.class_name != need.class_name || have.args.len() != need.args.len() {
            return false;
        }
        let saved = self.frozen_ty.clone();
        for arg in have.args.iter().chain(&need.args) {
            self.freeze_ty(*arg);
        }
        let mark = self.trail.len();
        let recording = self.recording;
        self.recording = true;
        let mut ok = true;
        for (have_arg, need_arg) in have.args.iter().zip(&need.args) {
            if self.unify_ty(*have_arg, *need_arg).is_err() {
                ok = false;
                break;
            }
        }
        self.undo(mark);
        self.recording = recording;
        self.frozen_ty = saved;
        ok
    }

    fn trial<T>(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mark = self.trail.len();
        let saved = self.recording;
        self.recording = true;
        let result = body(self);
        if result.is_err() {
            self.undo(mark);
        } else {
            self.trail.truncate(mark);
        }
        self.recording = saved;
        result
    }

    fn undo(&mut self, mark: usize) {
        while self.trail.len() > mark {
            match self.trail.pop().unwrap() {
                Edit::Ty(id, node) => self.tys[id as usize] = node,
                Edit::Stack(id, node) => self.stacks[id as usize] = node,
                Edit::Row(id, node) => self.rows[id as usize] = node,
                Edit::Effect(id, node) => self.effects[id as usize] = node,
            }
        }
    }

    fn freeze_ty(&mut self, ty: TyId) {
        let ty = self.find_ty(ty);
        match self.tys[ty.0 as usize].clone() {
            TyNode::Var => {
                self.frozen_ty.insert(ty);
            }
            TyNode::Ctor(_, args) => {
                for arg in args {
                    self.freeze_ty(arg);
                }
            }
            TyNode::App(head, args) => {
                self.freeze_ty(head);
                for arg in args {
                    self.freeze_ty(arg);
                }
            }
            TyNode::Fun(input, output, effect) => {
                self.freeze_stack(input);
                self.freeze_stack(output);
                self.freeze_effect(effect);
            }
            TyNode::Record(row) => self.freeze_row(row),
            _ => {}
        }
    }

    fn freeze_stack(&mut self, stack: StackId) {
        let stack = self.find_stack(stack);
        match self.stacks[stack.0 as usize].clone() {
            StackNode::Var => {
                self.frozen_stack.insert(stack);
            }
            StackNode::Cons(rest, ty) => {
                self.freeze_stack(rest);
                self.freeze_ty(ty);
            }
            _ => {}
        }
    }

    fn freeze_row(&mut self, row: RowId) {
        let row = self.find_row(row);
        match self.rows[row.0 as usize].clone() {
            RowNode::Var => {
                self.frozen_row.insert(row);
            }
            RowNode::Fields { fields, tail } => {
                for (_, ty) in fields {
                    self.freeze_ty(ty);
                }
                if let Some(tail) = tail {
                    self.freeze_row(tail);
                }
            }
            _ => {}
        }
    }

    fn freeze_effect(&mut self, effect: EffectId) {
        let effect = self.find_effect(effect);
        match self.effects[effect.0 as usize].clone() {
            EffectNode::Var => {
                self.frozen_effect.insert(effect);
            }
            EffectNode::Labels { tail, .. } => {
                if let Some(tail) = tail {
                    self.freeze_effect(tail);
                }
            }
            _ => {}
        }
    }

    fn flexible(&self, ty: TyId, seen: &mut Seen) -> bool {
        let ty = self.find_ty(ty);
        if !seen.ty.insert(ty) {
            return false;
        }
        match self.tys[ty.0 as usize].clone() {
            TyNode::Var => true,
            TyNode::Ctor(_, args) => args.into_iter().any(|arg| self.flexible(arg, seen)),
            TyNode::App(head, args) => {
                self.flexible(head, seen) || args.into_iter().any(|arg| self.flexible(arg, seen))
            }
            TyNode::Fun(input, output, _) => {
                self.flexible_stack(input, seen) || self.flexible_stack(output, seen)
            }
            TyNode::Record(row) => self.flexible_row(row, seen),
            TyNode::Link(next) => self.flexible(next, seen),
            _ => false,
        }
    }

    fn flexible_stack(&self, stack: StackId, seen: &mut Seen) -> bool {
        let stack = self.find_stack(stack);
        if !seen.stack.insert(stack) {
            return false;
        }
        match self.stacks[stack.0 as usize].clone() {
            StackNode::Var => true,
            StackNode::Cons(rest, ty) => {
                self.flexible_stack(rest, seen) || self.flexible(ty, seen)
            }
            StackNode::Link(next) => self.flexible_stack(next, seen),
            _ => false,
        }
    }

    fn flexible_row(&self, row: RowId, seen: &mut Seen) -> bool {
        let row = self.find_row(row);
        if !seen.row.insert(row) {
            return false;
        }
        match self.rows[row.0 as usize].clone() {
            RowNode::Var => true,
            RowNode::Fields { fields, tail } => {
                fields
                    .into_iter()
                    .any(|(_, ty)| self.flexible(ty, seen))
                    || tail.is_some_and(|tail| self.flexible_row(tail, seen))
            }
            RowNode::Link(next) => self.flexible_row(next, seen),
            _ => false,
        }
    }

    fn show_constraint(&self, constraint: &SemConstraint) -> String {
        let mut out = constraint.class_name.clone();
        for arg in &constraint.args {
            out.push(' ');
            out.push_str(&self.show_ty(*arg));
        }
        out
    }

    fn show_constraints(&self, constraints: &[SemConstraint]) -> String {
        if constraints.is_empty() {
            return "no constraint".to_string();
        }
        constraints
            .iter()
            .map(|constraint| self.show_constraint(constraint))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn prefix_len(&self, mut current: StackId) -> usize {
        let mut count = 0;
        loop {
            match &self.stacks[current.0 as usize] {
                StackNode::Cons(rest, _) => {
                    count += 1;
                    current = *rest;
                }
                _ => return count,
            }
        }
    }

    fn is_class_method(&self, name: &str) -> bool {
        self.classes
            .values()
            .any(|class| class.methods.iter().any(|method| method.name == name))
    }

    fn error(&self, span: Span, error: Error) -> Diagnostic {
        Diagnostic::new(span, error)
    }
}

/// Modules in an import closure, dependencies first.
/// The closure walk lists the provider first and is not a topological order:
/// a later import can be installed before the superclass it names.
fn dependency_order(known: &[KnownModule]) -> Vec<&Module> {
    let mut by_key = HashMap::new();
    for (index, known_module) in known.iter().enumerate() {
        by_key.insert(file_key(&known_module.origin), index);
    }
    let mut edges = vec![Vec::new(); known.len()];
    for (index, known_module) in known.iter().enumerate() {
        for import in &known_module.module.imports {
            let Some(dep) = by_key.get(&file_key(&import.path)).copied() else {
                continue;
            };
            if dep != index {
                edges[index].push(dep);
            }
        }
    }
    let mut seen = vec![false; known.len()];
    let mut order = Vec::with_capacity(known.len());
    for index in 0..known.len() {
        visit_dependency(index, &edges, &mut seen, &mut order);
    }
    order.into_iter().map(|index| &known[index].module).collect()
}

fn visit_dependency(index: usize, edges: &[Vec<usize>], seen: &mut [bool], order: &mut Vec<usize>) {
    if seen[index] {
        return;
    }
    seen[index] = true;
    for dep in &edges[index] {
        visit_dependency(*dep, edges, seen, order);
    }
    order.push(index);
}

/// `Ply/Applicable.ply`, `dist/Ply/Applicable.d.ply`, and `Ply/Applicable`
/// all key as `Applicable`. An interface path has to lose `.d.ply` whole.
/// Stripping `.ply` first leaves `Applicable.d`, and the superclass is missed.
fn file_key(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if let Some(stem) = name.strip_suffix(".d.ply") {
        return stem.to_string();
    }
    name.strip_suffix(".ply").unwrap_or(name).to_string()
}

/// Free type variables in a scheme, in source order. Stack, row, and effect
/// tails are not included: those names live in other spaces.
fn collect_scheme_vars(scheme: &Scheme, names: &mut Vec<String>) {
    for constraint in &scheme.constraints {
        for arg in &constraint.args {
            collect_ty_vars(arg, names);
        }
    }
    for item in scheme
        .effect
        .input
        .items
        .iter()
        .chain(&scheme.effect.output.items)
    {
        collect_ty_vars(item, names);
    }
}

/// Free type variables in an instance head, in source order.
fn collect_ty_vars(ty: &Type, names: &mut Vec<String>) {
    match ty {
        Type::Var(name) => {
            if !names.iter().any(|have| have == name) {
                names.push(name.clone());
            }
        }
        Type::Ctor(_) => {}
        Type::App(head, args) => {
            collect_ty_vars(head, names);
            for arg in args {
                collect_ty_vars(arg, names);
            }
        }
        Type::Fun(effect) => {
            for item in effect.input.items.iter().chain(&effect.output.items) {
                collect_ty_vars(item, names);
            }
        }
        Type::Record(row) => {
            for (_, field) in &row.fields {
                collect_ty_vars(field, names);
            }
            if let Some(ty) = row.tail_ty() {
                collect_ty_vars(ty, names);
            }
        }
    }
}

impl Scope {
    fn new() -> Self {
        Self {
            tys: HashMap::new(),
            tails: HashMap::new(),
            rows: HashMap::new(),
            effects: HashMap::new(),
            implicit_effect: None,
            quantified_tys: Vec::new(),
            quantified_tails: Vec::new(),
            quantified_rows: Vec::new(),
            quantified_effects: Vec::new(),
            nested: false,
        }
    }
}

impl Default for Subst {
    fn default() -> Self {
        Self {
            ty: HashMap::new(),
            stack: HashMap::new(),
            row: HashMap::new(),
            effect: HashMap::new(),
        }
    }
}

impl Default for Memo {
    fn default() -> Self {
        Self {
            ty: HashMap::new(),
            stack: HashMap::new(),
            row: HashMap::new(),
            effect: HashMap::new(),
        }
    }
}

enum RowEnd {
    Flex(RowId),
    Skolem(RowId),
}

enum EffectEnd {
    Flex(EffectId),
    Skolem(EffectId),
}

fn format_effect(labels: &[String], tail: Option<&str>) -> String {
    if labels.is_empty() && tail.is_none() {
        return "{}".to_string();
    }
    let mut out = String::from("{ ");
    out.push_str(&labels.join(", "));
    if let Some(name) = tail {
        if !labels.is_empty() {
            out.push(' ');
        }
        out.push_str("| ");
        out.push_str(name);
    }
    out.push_str(" }");
    out
}

fn first_missing<'a>(
    only_need: &'a [(String, TyId)],
    only_have: &'a [(String, TyId)],
) -> Option<&'a str> {
    only_need
        .first()
        .or(only_have.first())
        .map(|(name, _)| name.as_str())
}

fn format_row(fields: &[(String, String)], tail: Option<&str>) -> String {
    if fields.is_empty() && tail.is_none() {
        return "{}".to_string();
    }
    let mut out = String::from("{ ");
    let items = fields
        .iter()
        .map(|(name, ty)| format!("{name} : {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&items);
    if let Some(name) = tail {
        if !items.is_empty() {
            out.push(' ');
        }
        out.push_str("| ");
        out.push_str(name);
    }
    out.push_str(" }");
    out
}

enum Bottom {
    Empty,
    Hidden,
    Open,
    Named(String),
}

fn public_names(module: &Module) -> Vec<String> {
    let mut names = Vec::new();
    for item in &module.items {
        if let Item::Data(data) = item {
            names.push(data.name.clone());
            for ctor in &data.ctors {
                names.push(ctor.name.clone());
            }
        }
        if let Item::Synonym(synonym) = item {
            names.push(synonym.name.clone());
        }
    }
    for item in &module.items {
        if let Item::Def(def) = item {
            if !def.vis.is_private() {
                names.push(def.name.clone());
            }
        }
    }
    for item in &module.items {
        if let Item::Class(class) = item {
            names.push(class.name.clone());
            for method in &class.methods {
                names.push(method.name.clone());
            }
        }
    }
    names
}

fn find_class_item<'a>(module: &'a Module, name: &str) -> Option<&'a Class> {
    module.items.iter().find_map(|item| match item {
        Item::Class(class) if class.name == name => Some(class),
        _ => None,
    })
}

impl SynBody {
    fn from(synonym: &Synonym) -> Self {
        Self {
            params: synonym.params.clone(),
            rhs: synonym.rhs.clone(),
        }
    }
}

fn subst_type(ty: &Type, params: &[String], args: &[Type]) -> Type {
    match ty {
        Type::Var(name) => params
            .iter()
            .position(|param| param == name)
            .map(|index| args[index].clone())
            .unwrap_or_else(|| ty.clone()),
        Type::Ctor(_) => ty.clone(),
        Type::App(head, app_args) => Type::App(
            Box::new(subst_type(head, params, args)),
            app_args
                .iter()
                .map(|arg| subst_type(arg, params, args))
                .collect(),
        ),
        Type::Fun(effect) => Type::Fun(Box::new(StackEffect {
            input: subst_stack(&effect.input, params, args),
            output: subst_stack(&effect.output, params, args),
            effects: effect.effects.clone(),
        })),
        Type::Record(row) => Type::Record(crate::ast::RecordRow {
            fields: row
                .fields
                .iter()
                .map(|(name, field)| (name.clone(), subst_type(field, params, args)))
                .collect(),
            tail: row.tail.as_ref().map(|tail| match tail {
                RecordTail::Var(name) => RecordTail::Var(name.clone()),
                RecordTail::Type(ty) => RecordTail::Type(Box::new(subst_type(ty, params, args))),
            }),
        }),
    }
}

fn subst_stack(stack: &Stack, params: &[String], args: &[Type]) -> Stack {
    Stack {
        tail: stack.tail.clone(),
        items: stack
            .items
            .iter()
            .map(|ty| subst_type(ty, params, args))
            .collect(),
    }
}

fn find_synonym<'a>(module: &'a Module, name: &str) -> Option<&'a Synonym> {
    module.items.iter().find_map(|item| match item {
        Item::Synonym(synonym) if synonym.name == name => Some(synonym),
        _ => None,
    })
}

fn find_data<'a>(module: &'a Module, name: &str) -> Option<&'a crate::ast::Data> {
    module.items.iter().find_map(|item| match item {
        Item::Data(data) if data.name == name => Some(data),
        _ => None,
    })
}

fn data_of_ctor<'a>(module: &'a Module, name: &str) -> Option<&'a crate::ast::Data> {
    module.items.iter().find_map(|item| match item {
        Item::Data(data) if data.ctors.iter().any(|ctor| ctor.name == name) => Some(data),
        _ => None,
    })
}

fn find_def<'a>(module: &'a Module, name: &str) -> Option<&'a crate::ast::Def> {
    module.items.iter().find_map(|item| match item {
        Item::Def(def) if def.name == name => Some(def),
        _ => None,
    })
}

fn parsed_scheme(signature: &str) -> Scheme {
    let source = format!(": w ( {signature} )\n  0\n");
    let module = parse(&source, ParseMode::Source)
        .unwrap_or_else(|error| panic!("builtin `{signature}`: {}", error.message));
    match &module.items[0] {
        Item::Def(def) => def.scheme.clone(),
        _ => panic!("builtin `{signature}` did not parse as a def"),
    }
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::parser::parse;
    use crate::paths::ParseMode;

    fn check_source(source: &str, mode: ParseMode) -> Result<(), String> {
        let module = parse(source, mode).map_err(|error| error.message)?;
        check(&module).map_err(|error| error.message)
    }

    fn ok(body: &str) {
        check_source(body, ParseMode::Source).unwrap_or_else(|error| panic!("{error}\n{body}"));
    }

    fn err(body: &str, message: &str) {
        let got = check_source(body, ParseMode::Source).expect_err(body);
        assert_eq!(got, message, "{body}");
    }

    #[test]
    fn checks_the_maybe_example() {
        let source = "\
; Maybe a = Nothing | Just a

: fromMaybe ( a (Maybe a) -- a )
  Nothing ->
  Just -> nip

: mapMaybe ( (Maybe a) (a -- b) -- (Maybe b) )
  Nothing q -> q drop Nothing
  (Just x) q -> x q call Just
";
        check_source(source, ParseMode::Source).unwrap_or_else(|error| panic!("{error}\n{source}"));
    }

    #[test]
    fn builtins_match_dip_and_if() {
        ok(": double ( Int -- Int )\n  dup drop");
        ok(": swapped ( a b. a b -- b a )\n  swap");
        ok(": nipped ( a b. a b -- b )\n  nip");
        ok(": overed ( a b. a b -- a b a )\n  over");
        ok(": wrap ( a. a -- (-- a) )\n  quote");
        ok(": held ( a. a -- a )\n  [ ] dip");
        ok(": held2 ( a b. a b -- a b )\n  [ ] dip");
        ok(": chosen ( a. a a Bool -- a )\n  [ nip ] [ nip ] if");
        ok(": either ( a. a a Bool -- a )\n  [ drop ] [ nip ] if");
        ok(": from_just ( a. a (Maybe a) -- a )\n  Nothing ->\n  (Just x) -> drop x\n\n; Maybe a = Nothing | Just a");
        ok(": fact ( Int -- Int )\n  fact");
        ok(": even ( Int -- Bool )\n  odd\n\n: odd ( Int -- Bool )\n  even");
        ok(": flag ( -- Bool )\n  true");
        ok(": label ( -- Str )\n  \"hi\"");
        ok(": half ( (Num) (Num) -- Num )\n  drop");
        ok(": unit_value ( -- Unit )\n  Unit");
        ok(": match_unit ( Unit -- Int )\n  Unit -> 1n");
        ok(": maybe_int ( (Maybe Int) -- Int )\n  Nothing -> 0n\n  Just ->\n\n; Maybe a = Nothing | Just a");
        ok(": foreign add ( (Int) (Int) -- Int )\n\n: use ( (Int) (Int) -- Int )\n  add");
        ok(": ignore_quote ( a. a -- a )\n  [ drop ] drop");
        ok(": run_dup ( a. a -- a a )\n  [ dup ] call");
        ok(": run_drop ( a. a -- )\n  [ drop ] call");
    }

    #[test]
    fn stack_words_follow_factor() {
        ok(": r ( a b c. a b c -- b c a )\n  rot");
        ok(": r ( a b c. a b c -- c a b )\n  unrot");
        ok(": r ( a b. a b -- a a b )\n  dupd");
        ok(": r ( a b c. a b c -- b a c )\n  swapd");
        ok(": r ( a b c. a b c -- a b c a )\n  pick");
        ok(": r ( a b. a b -- b a b )\n  tuck");
        ok(": r ( a b. a b -- a b a b )\n  dup2");
        ok(": r ( a b. a b -- )\n  drop2");
        ok(": r ( a b c. a b c -- c )\n  nip2");
        ok(": r ( a b c d. a b c d -- c d a b )\n  swap2");
        ok(": r ( a b c d. a b c d -- a b c d a b )\n  over2");
        ok(": r ( a b. a b -- a b )\n  [ ] dip2");
        ok(": r ( a b c. a b c -- a b c )\n  [ ] dip3");
        ok(": r ( a. a (a -- b) -- b a )\n  keep");
        ok(": r ( a b. a b (a b -- c) -- c a b )\n  keep2");
        ok(": r ( a. a (a -- b) (a -- c) -- b c )\n  bi");
        ok(": r ( a. a (a -- b) (a -- c) (a -- d) -- b c d )\n  tri");
        ok(": r ( a b. a b (a -- c) (b -- d) -- c d )\n  bis");
        ok(": r ( a. a a (a -- b) -- b b )\n  bia");
        ok(
            ": foreign nmul ( Num Num -- Num )\n: foreign nadd ( Num Num -- Num )\n\n: both ( Num -- Num Num )\n  [ 2 nmul ] [ 3 nadd ] bi",
        );
        err(": bad ( a. a -- a )\n  rot", "stack underflow");
        err(
            ": bad ( Num -- Num )\n  [ dup ] [ drop ] bi",
            "occurs check failed",
        );
    }

    #[test]
    fn rejects_the_spec_errors() {
        err(": bad ( ..s -- ..s )\n  drop", "stack underflow");
        err(": bad ( ..s -- ..s )\n  [ dup ] call", "stack underflow");
        err(
            ": bad ( Str -- Int )\n  dup drop",
            "type mismatch: expected Int, found Str",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n: bad ( a. a (Maybe a) -- a )\n  Nothing ->",
            "non-exhaustive match: Just",
        );
        err(
            ": bad ( ..s -- ..s )\n  [ [ dup ] call ] drop",
            "quotation arity is not known",
        );
        err(
            ": bad ( { name : Str, name : Int } -- Str )\n  drop",
            "duplicate field name",
        );
        err(
            ": bad ( Int -- Int )\n  /name",
            "type mismatch: expected { name : _ | _ }, found Int",
        );
        err(": bad ( Int -- Int )\n  nope", "undefined word nope");
        err(
            ": dup ( Int -- Int )\n  dup drop",
            "cannot redefine builtin dup",
        );
        err("; Int = Int", "cannot redefine builtin Int");
        err(
            ": bad ( Int -- Num )\n  dup drop",
            "type mismatch: expected Num, found Int",
        );
        err(
            ": bad ( Num -- Int )\n  dup drop",
            "type mismatch: expected Int, found Num",
        );
        err(
            ": bad ( Int -- Int ! {} )\n  dup drop",
            "unexpected character",
        );
        err(
            ": bad ( a b. a -- b )\n  dup drop",
            "type mismatch: expected b, found a",
        );
        err(
            ": bad ( (Int Int) -- Int )\n  drop",
            "type constructor Int expects 0 arguments",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n: bad ( Maybe -- Maybe )\n  drop",
            "type constructor Maybe expects 1 argument",
        );
        err(
            ": bad ( a b f. (a -- b) (f a) -- (f b) )\n  drop",
            "type mismatch: expected (f b), found (.._ a -- .._ b)",
        );
        err(": bad ( Foo -- Foo )\n  drop", "undefined type Foo");
        ok(": id ( a -- a )\n  dup drop");
        err("; Box = Mk a", "type variable a is not quantified");
        err(
            ": bad ( a. a -- a a )\n  [ dup ] dip",
            "stack underflow",
        );
        err(
            ": bad ( Int -- Int )\n  Nothing ->",
            "type mismatch: expected a data type, found Int",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n; Either a b = Left a | Right b\n\n: bad ( a. (Maybe a) -- a )\n  Left ->\n  Nothing ->\n  Just ->",
            "constructor Left is not a constructor of Maybe",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n: bad ( a. (Maybe a) -- a )\n  Nothing ->\n  Nothing ->\n  Just ->",
            "redundant pattern",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n: bad ( a. (Maybe a) -- a )\n  (Nothing x) ->\n  Just ->",
            "pattern Nothing expects 0 fields",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n: bad ( a. (Maybe a) -- a )\n  Nothing ->\n  (Just x y) -> drop",
            "pattern Just expects 1 field",
        );
    }

    #[test]
    fn checks_records() {
        ok(": get ( { age : Int, name : Str } -- Str )\n  /name");
        ok(": get ( a. { name : a | r } -- a )\n  /name");
        ok(": get ( a. { name : a | r } -- a )\n  /name\n\n: use ( { age : Int, name : Str } -- Str )\n  get");
        ok(": use ( { name : Str, age : Int | r } -- Str )\n  /name");
        ok(": held ( { name : Str | r } -- { name : Str | r } )\n  dup drop");
        ok(": touch ( { } -- { } )\n  dup drop");
        ok(": idrec ( { name : Str | r } -- { name : Str | r } )\n  dup drop\n\n: use ( { name : Str } -- { name : Str } )\n  idrec");
        ok(": held ( ..s { name : Str | s } -- ..s { name : Str | s } )\n  dup drop");
        ok(": read ( a. { name : a | r } ({ name : a | r } -- a) -- a )\n  call");
        ok("; Person = Mk { name : Str, age : Int }\n\n: personName ( Person -- Str )\n  Mk -> /name");
        err(
            ": bad ( { age : Int } -- Str )\n  /name",
            "record has no field name",
        );
        err(
            ": bad ( { } -- Int )\n  /name",
            "record has no field name",
        );
        err(
            ": bad ( { name : Int } -- Str )\n  /name",
            "type mismatch: expected Str, found Int",
        );
        err(
            ": bad ( { name : Str | r } -- Int )\n  /age",
            "record has no field age",
        );
        err(
            ": bad ( { name : Str | r } -- { age : Int | r } )\n  dup drop",
            "occurs check failed",
        );
        err(
            ": bad ( { name : Str | r } -- { name : Str } )\n  dup drop",
            "type mismatch: expected { name : Str }, found { name : Str | r }",
        );
        err(
            ": bad ( { name : Str | r } -- { name : Str | s } )\n  dup drop",
            "type mismatch: expected { name : Str | s }, found { name : Str | r }",
        );
        err(
            ": bad ( r. { name : Str | r } -- Str )\n  /name",
            "duplicate type variable r",
        );
        ok(": get ( { name : a | r } -- a )\n  /name");
        err(
            ": bad ( { age : Int, name : Str } -- Int )\n  dup drop",
            "type mismatch: expected Int, found { age : Int, name : Str }",
        );
        err(
            ": bad ( a. ({ name : Str } a) -- a )\n  drop",
            "kind mismatch: expected _ -> _, found Type",
        );
        err(
            ": bad ( { name : Int } -- { name : Str } )\n  dup drop",
            "type mismatch: expected Str, found Int",
        );
        check_source(
            "language 1\n: get ( a. { name : a | r } -- a )\n",
            ParseMode::Interface,
        )
        .unwrap();
    }

    #[test]
    fn checks_classes() {
        ok("& Show a\n  show ( a -- Str )\n");
        ok("& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqInt\n\n: use ( Int Int -- Bool )\n  eq\n\n: same ( a. Eq a => a a -- Bool )\n  eq\n\n: useSame ( Int Int -- Bool )\n  same\n\n: eqBool ( Bool Bool -- Bool )\n  drop drop true\n\n# Eq Bool\n  eq = eqBool\n");
        ok("; Maybe a = Nothing | Just a\n\n& Functor f\n  map ( a b. (f a) (a -- b) -- (f b) )\n\n: mapMaybe ( a b. (Maybe a) (a -- b) -- (Maybe b) )\n  Nothing q -> q drop Nothing\n  (Just x) q -> x q call Just\n\n# Functor Maybe\n  map = mapMaybe\n\n: use ( (Maybe Int) -- (Maybe Int) )\n  [ dup drop ] map\n");
        ok(": idf ( f a. (f a) -- (f a) )\n  dup drop");
        err(
            "# Show Int\n  show = showInt",
            "undefined class Show",
        );
        err(
            ": bad ( a. Show a => a -- a )\n  dup drop",
            "undefined class Show",
        );
        err(
            "& Eq\n  eq ( Int -- Int )\n",
            "class Eq expects a parameter",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n  differ ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqInt\n",
            "missing method differ",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n: bad ( Int -- Int )\n  dup drop\n\n# Eq Int\n  eq = bad\n",
            "type mismatch: expected Bool, found Int",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n: private eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqInt\n",
            "instance word eqInt is private",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n: private foreign eqInt ( Int Int -- Bool )\n\n# Eq Int\n  eq = eqInt\n",
            "instance word eqInt is private",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n: use ( Str Str -- Bool )\n  eq\n",
            "no instance for Eq Str",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqInt\n\n# Eq Int\n  eq = eqInt\n",
            "incoherent instance",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n; Maybe a = Nothing | Just a\n\n: eqMaybe ( a. (Maybe a) (Maybe a) -- Bool )\n  drop drop true\n\n# Eq (Maybe a)\n  eq = eqMaybe\n\n# Eq (Maybe Int)\n  eq = eqMaybe\n",
            "incoherent instance",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n; Maybe a = Nothing | Just a\n\n: eqMaybe ( a. Eq (Maybe a) => (Maybe a) (Maybe a) -- Bool )\n  drop drop true\n\n# Eq (Maybe a)\n  eq = eqMaybe\n\n: use ( (Maybe Int) (Maybe Int) -- Bool )\n  eq\n",
            "cyclic instance: Eq (Maybe Int)",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n: eqWeird ( b. Eq b => Int Int -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqWeird\n",
            "ambiguous constraint Eq _",
        );
        err(
            "& Neutral a\n  empty ( -- a )\n\n: emptyString ( -- Str )\n  \"\"\n\n# Neutral Str\n  empty = emptyString\n\n: bad ( -- Int )\n  empty drop 1n\n",
            "ambiguous constraint Neutral _",
        );
    }

    #[test]
    fn bang_is_not_syntax() {
        err(
            ": bad ( Int -- Int ! {} )\n  dup drop",
            "unexpected character",
        );
        err(
            ": foreign print ( Str -- ! { console } )\n",
            "unexpected character",
        );
    }

    #[test]
    fn different_branch_effects_fail_the_occurs_check() {
        err(
            ": bad ( a. a Bool -- a )\n  [ drop ] [ dup ] if",
            "occurs check failed",
        );
    }

    #[test]
    fn interface_checks_signatures_only() {
        check_source(
            "language 1\n: id ( a. a -- a )\n",
            ParseMode::Interface,
        )
        .unwrap();
        check_source(
            "language 1\n: foreign read ( -- Str )\n",
            ParseMode::Interface,
        )
        .unwrap();
        check_source(
            "language 1\n: private hidden ( Int -- Int )\n",
            ParseMode::Interface,
        )
        .unwrap();
        check_source(
            "language 1\n: foreign private secret ( -- Str )\n",
            ParseMode::Interface,
        )
        .unwrap();
    }

    #[test]
    fn occurs_check_rejects_a_tail_inside_itself() {
        let mut checker = super::Checker::new();
        let tail = checker.stack_var();
        let ty = checker.ty_var();
        let bigger = checker.stack_cons(tail, ty);
        let message = checker.unify_stack(tail, bigger).unwrap_err();
        assert_eq!(message.message(), "occurs check failed");
    }

    #[test]
    fn a_deep_acyclic_type_is_not_an_occurs_check() {
        let mut ty = "Int".to_string();
        for _ in 0..65 {
            ty = format!("(Maybe {ty})");
        }
        ok(&format!(
            "; Maybe a = Nothing | Just a\n\n: gone ( {ty} -- )\n  drop\n"
        ));
    }

    #[test]
    fn find_ty_follows_a_long_link_chain() {
        let mut checker = super::Checker::new();
        let end = checker.ty_ctor("Int", Vec::new());
        let mut id = end;
        for _ in 0..70 {
            id = checker.push_ty(super::TyNode::Link(id));
        }
        assert_eq!(checker.find_ty(id), end);
    }

    #[test]
    fn flexible_walks_a_deep_type() {
        let mut checker = super::Checker::new();
        let mut ty = checker.ty_ctor("Int", Vec::new());
        for _ in 0..70 {
            ty = checker.ty_ctor("Maybe", vec![ty]);
        }
        assert!(!checker.flexible(ty, &mut super::Seen::default()));
        let mut wrapped = checker.ty_var();
        for _ in 0..70 {
            wrapped = checker.ty_ctor("Maybe", vec![wrapped]);
        }
        assert!(checker.flexible(wrapped, &mut super::Seen::default()));
    }

    #[test]
    fn a_superclass_module_is_installed_before_the_subclass() {
        let mappable = parse(
            "& Mappable f\n  map ( a b. (f a) (a -- b) -- (f b) )\n",
            ParseMode::Source,
        )
        .unwrap();
        let applicable = parse(
            "~ \"Ply/Mappable\" (Mappable)\n\n& Mappable f => Applicable f\n  apply ( a b. (f a) (f (a -- b)) -- (f b) )\n",
            ParseMode::Source,
        )
        .unwrap();
        let flat = parse(
            "~ \"Ply/Applicable\" (Applicable)\n\n& Applicable f => Flatmappable f\n  flatmap ( a b. (f a) (a -- (f b)) -- (f b) )\n",
            ParseMode::Source,
        )
        .unwrap();
        let consumer = parse("~ \"Ply/Flatmappable\" (Flatmappable)\n", ParseMode::Source).unwrap();
        let known = vec![
            super::KnownModule {
                origin: "Ply/Flatmappable.ply".to_string(),
                module: flat.clone(),
            },
            super::KnownModule {
                origin: "Ply/Mappable.ply".to_string(),
                module: mappable,
            },
            super::KnownModule {
                origin: "Ply/Applicable.ply".to_string(),
                module: applicable,
            },
        ];
        super::check_with_imports(
            &consumer,
            &[super::ResolvedImport {
                provider: &flat,
                import: &consumer.imports[0],
                known: &known,
            }],
        )
        .unwrap();
    }

    #[test]
    fn an_interface_file_installs_the_superclass_first() {
        let mappable = parse(
            "& Mappable f\n  map ( a b. (f a) (a -- b) -- (f b) )\n",
            ParseMode::Source,
        )
        .unwrap();
        let applicable = parse(
            "~ \"Ply/Mappable\" (Mappable)\n\n& Mappable f => Applicable f\n  apply ( a b. (f a) (f (a -- b)) -- (f b) )\n",
            ParseMode::Source,
        )
        .unwrap();
        let flat = parse(
            "~ \"Ply/Applicable\" (Applicable)\n\n& Applicable f => Flatmappable f\n  flatmap ( a b. (f a) (a -- (f b)) -- (f b) )\n",
            ParseMode::Source,
        )
        .unwrap();
        let consumer = parse("~ \"Ply/Flatmappable\" (Flatmappable)\n", ParseMode::Source).unwrap();
        let known = vec![
            super::KnownModule {
                origin: "/deps/dist/Ply/Flatmappable.d.ply".to_string(),
                module: flat.clone(),
            },
            super::KnownModule {
                origin: "/deps/dist/Ply/Mappable.d.ply".to_string(),
                module: mappable,
            },
            super::KnownModule {
                origin: "/deps/dist/Ply/Applicable.d.ply".to_string(),
                module: applicable,
            },
        ];
        super::check_with_imports(
            &consumer,
            &[super::ResolvedImport {
                provider: &flat,
                import: &consumer.imports[0],
                known: &known,
            }],
        )
        .unwrap();
    }

    #[test]
    fn an_instance_word_is_not_the_imported_method() {
        let comparable = parse(
            "& Comparable a\n  eq ( a a -- Bool )\n",
            ParseMode::Source,
        )
        .unwrap();
        let number = parse(
            "~ \"Ply/Comparable\" (Comparable)\n\n: eq ( Num Num -- Bool )\n  drop drop true\n\n# Comparable Num\n  eq = eq\n",
            ParseMode::Source,
        )
        .unwrap();
        let consumer = parse(
            "~ \"Ply/Comparable\" (Comparable, eq)\n~ \"Ply/Number\" ()\n\n: use ( Num Num -- Bool )\n  eq\n",
            ParseMode::Source,
        )
        .unwrap();
        let known_number = vec![
            super::KnownModule {
                origin: "Ply/Comparable.ply".to_string(),
                module: comparable.clone(),
            },
            super::KnownModule {
                origin: "Ply/Number.ply".to_string(),
                module: number.clone(),
            },
        ];
        let known_comparable = vec![super::KnownModule {
            origin: "Ply/Comparable.ply".to_string(),
            module: comparable.clone(),
        }];
        super::check_with_imports(
            &consumer,
            &[
                super::ResolvedImport {
                    provider: &comparable,
                    import: &consumer.imports[0],
                    known: &known_comparable,
                },
                super::ResolvedImport {
                    provider: &number,
                    import: &consumer.imports[1],
                    known: &known_number,
                },
            ],
        )
        .unwrap();
    }

    #[test]
    fn imports_are_selective() {
        let provider = parse(
            "; Maybe a = Nothing | Just a\n\n: fromMaybe ( a. a (Maybe a) -- a )\n  Nothing ->\n  Just -> nip\n\n: private hidden ( Int -- Int )\n  dup drop\n",
            ParseMode::Source,
        )
        .unwrap();
        let known = vec![super::KnownModule {
            origin: "Data/Maybe".to_string(),
            module: provider.clone(),
        }];
        let check_import = |source: &str| {
            let module = parse(source, ParseMode::Source).unwrap();
            super::check_with_imports(
                &module,
                &[super::ResolvedImport {
                    provider: &provider,
                    import: &module.imports[0],
                    known: &known,
                }],
            )
        };
        check_import(
            "~ \"Data/Maybe\" (fromMaybe, Nothing)\n\n: use ( Int -- Int )\n  Nothing fromMaybe\n",
        )
        .unwrap();
        let missing_word = check_import(
            "~ \"Data/Maybe\" (Maybe)\n\n: use ( a. -- (Maybe a) )\n  Nothing\n",
        )
        .unwrap_err();
        assert_eq!(missing_word.message, "undefined word Nothing");
        let missing_type = check_import(
            "~ \"Data/Maybe\" (Nothing)\n\n: use ( a. -- (Maybe a) )\n  Nothing\n",
        )
        .unwrap_err();
        assert_eq!(missing_type.message, "undefined type Maybe");
        let hidden = check_import(
            "~ \"Data/Maybe\" (hidden)\n\n: use ( Int -- Int )\n  hidden\n",
        )
        .unwrap_err();
        assert_eq!(hidden.message, "~ \"Data/Maybe\" does not export hidden");
    }

    #[test]
    fn a_data_type_keeps_the_file_that_declared_it() {
        let num = parse("; Box a = Mk a a\n", ParseMode::Source).unwrap();
        let other = parse(
            "; Box a = Box a\n\n: wrap ( a. a -- (Box a) )\n  Box\n",
            ParseMode::Source,
        )
        .unwrap();
        let known_num = vec![super::KnownModule {
            origin: "Num.ply".to_string(),
            module: num.clone(),
        }];
        let known_other = vec![super::KnownModule {
            origin: "Other.ply".to_string(),
            module: other.clone(),
        }];
        let module = parse(
            "~ \"Num\" (Mk)\n~ \"Other\" (wrap)\n\n: id ( Int -- Int )\n  dup drop\n",
            ParseMode::Source,
        )
        .unwrap();
        let error = super::check_with_imports(
            &module,
            &[
                super::ResolvedImport {
                    provider: &num,
                    import: &module.imports[0],
                    known: &known_num,
                },
                super::ResolvedImport {
                    provider: &other,
                    import: &module.imports[1],
                    known: &known_other,
                },
            ],
        )
        .unwrap_err();
        assert_eq!(error.message, "duplicate definition Box");

        let twin = parse("; Box a = Mk a a\n", ParseMode::Source).unwrap();
        let known_twin = vec![super::KnownModule {
            origin: "Twin.ply".to_string(),
            module: twin.clone(),
        }];
        let again = parse(
            "~ \"Num\" (Box)\n~ \"Twin\" (Box)\n\n: id ( Int -- Int )\n  dup drop\n",
            ParseMode::Source,
        )
        .unwrap();
        let error = super::check_with_imports(
            &again,
            &[
                super::ResolvedImport {
                    provider: &num,
                    import: &again.imports[0],
                    known: &known_num,
                },
                super::ResolvedImport {
                    provider: &twin,
                    import: &again.imports[1],
                    known: &known_twin,
                },
            ],
        )
        .unwrap_err();
        assert_eq!(error.message, "duplicate definition Box");

        let module = parse(
            "~ \"Num\" (Box)\n~ \"Num\" (Mk)\n\n: pair ( a. a a -- (Box a) )\n  Mk\n",
            ParseMode::Source,
        )
        .unwrap();
        super::check_with_imports(
            &module,
            &[
                super::ResolvedImport {
                    provider: &num,
                    import: &module.imports[0],
                    known: &known_num,
                },
                super::ResolvedImport {
                    provider: &num,
                    import: &module.imports[1],
                    known: &known_num,
                },
            ],
        )
        .unwrap();
    }

    #[test]
    fn checks_arrays_records_strings_and_maps() {
        ok(": none ( -- (Array Int) )\n  <>");
        ok(": nums ( -- (Array Num) )\n  < 1 2 3 >");
        ok(": quotes ( -- (Array (Num -- Num)) )\n  < [ dup drop ] [ dup drop ] >");
        ok(": person ( -- { name : Str, age : Num } )\n  { name: \"Ada\", age: 36 }");
        ok(": older ( { name : Str, age : Num } -- { name : Str, age : Num } )\n  37 =age");
        ok(": text ( -- Str )\n  \"ab\"");
        ok(": hold ( (StrMap Num) -- (StrMap Num) )\n  dup drop");
        err(
            ": bad ( -- (Array Num) )\n  < 1 \"a\" >",
            "type mismatch: expected Num, found Str",
        );
        err(
            ": bad ( { name : Str } -- { name : Str } )\n  1 =age",
            "record has no field age",
        );
        err("; Array a = Mk a", "cannot redefine builtin Array");
        err("; StrMap a = Mk a", "cannot redefine builtin StrMap");
        err(": bad ( Int Int -- Int )\n  iadd", "undefined word iadd");
        err(
            ": bad ( -- (Array Num) )\n  < 1 2 3 > [ dup drop ] amap",
            "undefined word amap",
        );
    }

    #[test]
    fn locals_are_captured_by_quotations() {
        ok(": addToAll ( Num (Array Num) -- (Array Num) )\n  n xs ->\n    xs [ n ] drop\n");
        ok(": curry ( a b c. a (a b -- c) -- (b -- c) )\n  x q ->\n    [ x swap q call ]\n");
        ok(": both ( Int Int -- Int Int )\n  y x ->\n    y x\n");
        ok(": shadow ( Int -- Int )\n  dup -> dup\n");
        ok(": inner ( Int Int -- Int )\n  x n ->\n    x\n    [ n ->\n      n ] call\n\n: quoted ( a b. a b -- b a )\n  [ x y -> y x ] call\n\n: one ( a. a -- a )\n  [ x -> x ] call\n");
        err(
            ": bad ( Int Int -- Int )\n  n n -> n\n",
            "duplicate definition n",
        );
        err(
            ": bad ( -- Int )\n  n -> n\n",
            "stack underflow",
        );
        err(": bad ( Int -- Int )\n  -> n", "expected term");
        err(": bad ( Int -- Int )\n  [ -> n ]", "expected term");
    }

    #[test]
    fn synonyms_expand_and_newtypes_stay_nominal() {
        ok("- Person { name : Str, age : Num }\n\n: older ( Person -- Person )\n  37 =age\n");
        ok("- Pair a b { left : a, right : b }\n\n: id ( (Pair Int Str) -- { left : Int, right : Str } )\n  dup drop\n");
        ok("- Alias Int\n\n: use ( Alias -- Int )\n  dup drop\n");
        ok("; Maybe a = Nothing | Just a\n- Answer (Maybe Int)\n\n: get ( Answer -- Int )\n  Nothing -> 0n\n  Just ->\n");
        ok("& Eq a\n  eq ( a a -- Bool )\n\n- Person { name : Str, age : Num }\n\n: eqPerson ( { name : Str, age : Num } { name : Str, age : Num } -- Bool )\n  drop drop true\n\n# Eq Person\n  eq = eqPerson\n\n: same ( Person Person -- Bool )\n  eq\n");
        ok("; Person = Person Str\n- Alias Person\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqPerson ( Person Person -- Bool )\n  drop drop true\n\n# Eq Alias\n  eq = eqPerson\n");
        ok("; Age == Age Num\n\n: older ( Age -- Age )\n  Age -> Age\n");
        ok("; Person == Person { name : Str, age : Num }\n\n: older ( Person -- Person )\n  Person -> 37 =age Person\n");
        ok("; Age == Age Num\n\n& Eq a\n  eq ( a a -- Bool )\n\n: eqAge ( Age Age -- Bool )\n  drop drop true\n\n# Eq Age\n  eq = eqAge\n");
        err(
            "- Person { name : Str, age : Num }\n\n: bad ( Person -- Person )\n  Person ->\n",
            "type mismatch: expected a data type, found { age : Num, name : Str }",
        );
        err("- A A", "cyclic type A");
        err(
            "- A (Maybe A)\n\n; Maybe a = Nothing | Just a",
            "cyclic type A",
        );
        err("- A B\n\n- B A", "cyclic type A");
        err(
            "; Maybe a = Nothing | Just a\n\n- F Maybe",
            "type constructor Maybe expects 1 argument",
        );
        err("- Int Num", "cannot redefine builtin Int");
        err(
            "; Person = Person\n\n- Person Int",
            "duplicate definition Person",
        );
        err("- Pair a a a", "duplicate type variable a");
        err(
            "; Age == Age Num\n\n: bad ( Age -- Num )\n  dup drop\n",
            "type mismatch: expected Num, found Age",
        );
        err("; Pair == Left Int | Right Int", "newtype expects one constructor");
        err("; Unitish == Unitish", "newtype expects one field");
        err("; Two == Two Int Int", "newtype expects one field");
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n- Person { name : Str, age : Num }\n\n: eqPerson ( Person Person -- Bool )\n  drop drop true\n\n: eqRow ( { name : Str, age : Num } { name : Str, age : Num } -- Bool )\n  drop drop true\n\n# Eq Person\n  eq = eqPerson\n\n# Eq { name : Str, age : Num }\n  eq = eqRow\n",
            "incoherent instance",
        );
    }

    #[test]
    fn a_foreign_type_is_indexed_by_its_parameter() {
        ok("\
; foreign Hub a

: foreign make ( a. -- (Hub a) )

: pass ( (Hub a) -- (Hub a) )
  hub -> hub

& Tag a
  tag ( a -- Str )

: tagHub ( a. (Hub a) -- Str )
  drop \"hub\"

# Tag (Hub a)
  tag = tagHub
");
        err(
            "\
; foreign Hub a

: bad ( (Hub a) -- (Hub a) )
  Listeners -> Listeners
",
            "constructor Listeners is not a constructor of Hub",
        );
        err(
            "\
; foreign Hub a

: foreign num ( -- (Hub Num) )
: foreign text ( -- (Hub Str) )

: mix ( -- (Hub Num) )
  text
",
            "type mismatch: expected Num, found Str",
        );
    }

    #[test]
    fn a_closed_record_tail_splices_and_plus_inserts() {
        ok("- Person { name : Str, age : Num }\n- PersonWithCity { city : Str | Person }\n\n: inParis ( Person -- PersonWithCity )\n  \"Paris\" +city\n");
        ok("- Person { name : Str, age : Num }\n- PersonWithCity { city : Str | Person }\n\n: withCity ( Person Str -- PersonWithCity )\n  +city\n");
        ok("- Person a { name : a, age : Num }\n- PersonWithCity a { city : Str | (Person a) }\n\n: withCity ( a. (Person a) Str -- (PersonWithCity a) )\n  +city\n");
        ok("- Person { name : Str, age : Num }\n- PersonWithCity { city : Str | Person }\n\n: older ( { name : Str, age : Num | r } -- { name : Str, age : Num | r } )\n  37 =age\n\n: usePerson ( Person -- Person )\n  older\n\n: useCity ( PersonWithCity -- PersonWithCity )\n  older\n");
        ok("- Person { name : Str, age : Num }\n\n: moved ( Person -- { name : Str, age : Num, city : Str, zip : Str } )\n  \"Paris\" +city \"75001\" +zip\n");
        ok("- Person { name : Str, age : Num }\n- PersonWithCity { city : Str | Person }\n\n: fix ( Person -- PersonWithCity )\n  \"Lyon\" +city \"Paris\" =city\n");
        ok(": tagged ( -- { city : Str } )\n  {} \"Paris\" +city\n");
        ok(": named ( { city : Str | { name : Str } } -- Str )\n  /name\n");
        ok("- Open { name : Str | r }\n\n: get ( Open -- Str )\n  /name\n");
        err(
            "- Open { name : Str | r }\n- Bad { city : Str | Open }\n",
            "record tail must be closed",
        );
        err(
            "- Person { name : Str, age : Num }\n\n: bad ( { name : Str, age : Num | r } -- { name : Str, age : Num, city : Str | r } )\n  \"Paris\" +city\n",
            "record tail must be closed",
        );
        err(
            "- Person { name : Str, age : Num, city : Str }\n\n: bad ( Person -- Person )\n  \"Paris\" +city\n",
            "duplicate field city",
        );
        err(
            "- Bad { city : Str | Int }\n",
            "type mismatch: expected a record, found Int",
        );
        err(
            "- Person { name : Str, age : Num }\n- Bad { name : Str | Person }\n",
            "duplicate field name",
        );
        err(
            "- A { name : Str | B }\n- B { city : Str | A }\n",
            "cyclic type A",
        );
        err(
            "- Person { name : Str, age : Num }\n\n: bad ( Person -- Person )\n  \"Paris\" +city\n",
            "record has no field city",
        );
        err(
            "- Person { name : Str, age : Num }\n\n: bad ( Person -- Person )\n  \"Paris\" =city\n",
            "record has no field city",
        );
        err(
            "- Person { name : Str, age : Num }\n- PersonWithCity { city : Str | Person }\n\n: bad ( Person -- PersonWithCity )\n  1 +city\n",
            "type mismatch: expected Str, found Num",
        );
        err(
            ": bad ( a. a -- a )\n  \"x\" +name\n",
            "type mismatch: expected a record, found a",
        );
        err(
            ": bad ( Int -- Int )\n  \"x\" +name\n",
            "type mismatch: expected a record, found Int",
        );
        err(
            "; Person == Person { name : Str, age : Num }\n\n: bad ( Person -- Person )\n  \"Paris\" +city\n",
            "type mismatch: expected a record, found Person",
        );
        err(
            "- Person a { name : a }\n- Bad { city : Str | Person }\n",
            "type constructor Person expects 1 argument",
        );
        err(
            "- Wrap a { city : Str | (a) }\n",
            "type mismatch: expected a record, found _",
        );
    }

    #[test]
    fn imports_publish_synonym_names() {
        let provider = parse(
            "- Person { name : Str, age : Num }\n\n: older ( Person -- Person )\n  37 =age\n",
            ParseMode::Source,
        )
        .unwrap();
        let known = vec![super::KnownModule {
            origin: "Data/Person".to_string(),
            module: provider.clone(),
        }];
        let check_import = |source: &str| {
            let module = parse(source, ParseMode::Source).unwrap();
            super::check_with_imports(
                &module,
                &[super::ResolvedImport {
                    provider: &provider,
                    import: &module.imports[0],
                    known: &known,
                }],
            )
        };
        check_import(
            "~ \"Data/Person\" (older)\n\n: use ( { name : Str, age : Num } -- { name : Str, age : Num } )\n  older\n",
        )
        .unwrap();
        check_import(
            "~ \"Data/Person\" (..)\n\n: use ( Person -- Person )\n  older\n",
        )
        .unwrap();
        let missing = check_import(
            "~ \"Data/Person\" (older)\n\n: use ( Person -- Person )\n  older\n",
        )
        .unwrap_err();
        assert_eq!(missing.message, "undefined type Person");
    }

    #[test]
    fn clauses_match_literals_slots_and_nesting() {
        ok(
            ": toString ( Bool -- Str )\n  true -> \"true\"\n  false -> \"false\"\n",
        );
        ok(
            ": fact ( Int -- Int )\n  0n -> 1n\n  n -> n\n",
        );
        ok(
            "; Maybe a = Nothing | Just a\n\n: mapMaybe ( a b. (Maybe a) (a -- b) -- (Maybe b) )\n  Nothing q -> q drop Nothing\n  (Just x) q -> x q call Just\n\n: divInt ( Int Int -- (Maybe Int) )\n  x 0n -> Nothing\n  x y -> Nothing\n\n: fromMaybe ( a. a (Maybe a) -- a )\n  x Nothing -> x\n  x (Just y) -> y\n\n: flatten ( a. (Maybe (Maybe a)) -- (Maybe a) )\n  Nothing -> Nothing\n  (Just Nothing) -> Nothing\n  (Just (Just x)) -> x Just\n\n: fromJust ( (Maybe Int) -- Int )\n  Nothing -> 0n\n  Just ->\n\n: isJust ( a. (Maybe a) -- Bool )\n  Nothing -> false\n  (Just _) -> true\n",
        );
        ok(
            ": addToAll ( Num (Array Num) -- (Array Num) )\n  n xs ->\n    xs [ n ] drop\n",
        );
        ok(
            ": id ( a. a -- a )\n  n -> n\n",
        );
        ok(
            ": swapped ( a b. a b -- b a )\n  x y -> y x\n",
        );
        ok(
            ": held ( a. a Bool -- a )\n  true ->\n  false ->\n",
        );
        ok(
            "; Pair a b = Pair a b\n\n: snd ( a b. (Pair a b) -- b )\n  (Pair _ y) -> y\n\n: both ( (Pair Bool Bool) -- Int )\n  (Pair true true) -> 1n\n  (Pair true false) -> 2n\n  (Pair false true) -> 3n\n  (Pair false false) -> 4n\n",
        );
        ok(
            "; Age == Age Num\n\n: years ( Age -- Num )\n  (Age n) -> n\n\n: bare ( Age -- Num )\n  Age ->\n",
        );
        ok(
            ": bump ( Int -- Int )\n  0n -> 1n\n  n -> [ n ] call\n",
        );
        err(
            ": bad ( Int -- Int )\n  0n -> 1n\n",
            "non-exhaustive match: Int",
        );
        err(
            ": bad ( Bool -- Int )\n  true -> 1n\n",
            "non-exhaustive match: false",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n: bad ( (Maybe Int) -- Int )\n  (Just x) -> x\n",
            "non-exhaustive match: Nothing",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n: bad ( a. (Maybe (Maybe a)) -- Int )\n  Nothing -> 0n\n  (Just Nothing) -> 0n\n",
            "non-exhaustive match: Just Just",
        );
        err(
            ": bad ( Bool -- Bool )\n  true -> true\n  false -> false\n  _ -> true\n",
            "redundant pattern",
        );
        err(
            ": bad ( Int -- Int )\n  0n -> 1n\n  0n -> 2n\n  n -> n\n",
            "redundant pattern",
        );
        err(
            ": bad ( Bool -- Bool )\n  _ -> true\n  true -> false\n",
            "redundant pattern",
        );
        err(
            ": bad ( Bool Bool -- Bool )\n  true -> true\n  false false -> false\n",
            "expected 1 pattern",
        );
        err(
            ": bad ( Bool Bool -- Bool )\n  true false -> true\n  x -> x\n",
            "expected 2 patterns",
        );
        err(
            ": bad ( Int -- Int )\n  x y -> x\n",
            "stack underflow",
        );
        err(
            ": bad ( Bool -- Bool )\n  0n -> true\n  b -> b\n",
            "type mismatch: expected Int, found Bool",
        );
        err(
            ": bad ( a. a -- a )\n  0n -> 0n\n  x -> x\n",
            "type mismatch: expected Int, found a",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n: bad ( (Maybe Int) -- Int )\n  (Just) -> 0n\n  Nothing -> 0n\n",
            "pattern Just expects 1 field",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n: bad ( (Maybe Int) -- Int )\n  Foo -> 0n\n",
            "constructor Foo is not a constructor of Maybe",
        );
        err(
            ": bad ( Int -- Int )\n  Foo -> 0n\n  n -> n\n",
            "type mismatch: expected a data type, found Int",
        );
        err(
            "; Pair a b = Pair a b\n\n: bad ( (Pair Int Int) -- Int )\n  (Pair x x) -> x\n",
            "duplicate definition x",
        );
        err(
            "; Age == Age Num\n\n: bad ( Age -- Num )\n  (Age 0) -> 0\n",
            "non-exhaustive match: Age Num",
        );
    }

    #[test]
    fn a_deep_constructor_is_covered() {
        let mut ty = "Int".to_string();
        let mut pat = "n".to_string();
        for _ in 0..20 {
            ty = format!("(Box {ty})");
            pat = format!("(Mk {pat})");
        }
        ok(&format!(
            "; Box a = Mk a\n\n: unwrap ( {ty} -- Int )\n  {pat} -> n\n"
        ));
    }

    #[test]
    fn array_patterns_match_a_fixed_length() {
        ok(
            ": foreign imul ( Int Int -- Int )\n\n: isEmpty ( (Array a) -- Bool )\n  <> -> true\n  _ -> false\n\n: takeFive ( (Array Int) -- Int )\n  < 0n 1n a b _ > -> a b imul\n  _ -> 0n\n\n: flags ( (Array Bool) -- Bool )\n  < true > -> true\n  < false > -> false\n  _ -> false\n\n; Maybe a = Nothing | Just a\n\n: onlyJust ( (Array (Maybe Int)) -- Int )\n  < (Just x) > -> x\n  _ -> 0n\n\n: nested ( (Array (Array Num)) -- Num )\n  < < a > > -> a\n  _ -> 0\n\n- Ages (Array Int)\n\n: oneAge ( Ages -- Int )\n  < 1n > -> 1n\n  _ -> 0n\n\n: pair ( (Array Int) Bool -- Bool )\n  < 0n > true -> true\n  < 0n > false -> false\n  _ _ -> false\n",
        );
        err(
            ": bad ( (Array Int) -- Int )\n  <> -> 0n\n",
            "non-exhaustive match: Array",
        );
        err(
            ": bad ( (Array Int) -- Int )\n  < 0n > -> 0n\n  < 0n > -> 1n\n  _ -> 0n\n",
            "redundant pattern",
        );
        err(
            ": bad ( (Array Int) -- Int )\n  < a > -> a\n  < 0n > -> 0n\n  _ -> 0n\n",
            "redundant pattern",
        );
        err(
            ": bad ( (Array Bool) -- Bool )\n  < true > -> true\n  < false > -> false\n  < b > -> b\n",
            "redundant pattern",
        );
        err(
            ": bad ( (Array Int) Bool -- Bool )\n  < a > _ -> true\n  < 0n > true -> false\n",
            "redundant pattern",
        );
        ok(
            ": wider ( (Array Int) -- Int )\n  < 0n > -> 0n\n  < a > -> a\n  _ -> 0n\n",
        );
        err(
            ": bad ( (Array Int) -- Int )\n  < 0 > -> 0n\n  _ -> 0n\n",
            "type mismatch: expected Num, found Int",
        );
        err(
            ": bad ( Int -- Int )\n  <> -> 0n\n  n -> n\n",
            "type mismatch: expected (Array _), found Int",
        );
        err(
            ": bad ( (Array Int) -- Int )\n  < a a > -> a\n  _ -> 0n\n",
            "duplicate definition a",
        );
    }

    #[test]
    fn record_patterns_read_named_fields() {
        ok(
            ": nameOf ( { name : Str | r } -- Str )\n  { name : n } -> n\n\n: born ( { name : Str, year : Int } -- Str )\n  { year : 1815n, name : n } -> n\n  { name : n } -> n\n\n: touch ( { name : Str | r } -- Bool )\n  {} -> true\n\n: title ( { who : { name : Str | s } | r } -- Str )\n  { who : { name : n } } -> n\n\n: flag ( { on : Bool } -- Bool )\n  { on : true } -> true\n  { on : false } -> false\n\n: nameOnly ( { name : Str, age : Int } -- Str )\n  { name : n } -> n\n\n- Human { name : Str, age : Num }\n\n: humanName ( Human -- Str )\n  { name : n } -> n\n\n; Person = Mk { name : Str, age : Int }\n\n: personName ( Person -- Str )\n  (Mk { name : n }) -> n\n\n; Maybe a = Nothing | Just a\n\n: parisName ( { who : (Maybe { name : Str, city : Str }) } -- Str )\n  { who : (Just { city : \"Paris\", name : n }) } -> n\n  { who : (Just { name : n }) } -> n\n  _ -> \"\"\n\n: headName ( { people : (Array { name : Str | r }) } -- Str )\n  { people : < { name : n } > } -> n\n  _ -> \"\"\n\n: firstCity ( (Array { city : Str | r }) -- Str )\n  < { city : c } > -> c\n  _ -> \"\"\n\n: ada ( { name : Str | r } Bool -- Str )\n  { name : \"Ada\" } true -> \"Ada\"\n  { name : \"Ada\" } false -> \"Ada\"\n  _ _ -> \"\"\n",
        );
        err(
            ": bad ( { name : Str, year : Int } -- Str )\n  { year : 1815n } -> \"\"\n",
            "non-exhaustive match: { name : Str, year : Int }",
        );
        err(
            ": bad ( { who : (Maybe Str) } -- Str )\n  { who : (Just n) } -> n\n\n; Maybe a = Nothing | Just a",
            "non-exhaustive match: { who : Nothing }",
        );
        err(
            ": bad ( { who : (Maybe { name : Str, city : Str }) } -- Str )\n  { who : (Just { city : \"Paris\", name : n }) } -> n\n\n; Maybe a = Nothing | Just a",
            "non-exhaustive match: { who : Nothing }, { who : (Just { city : Str, name : Str }) }",
        );
        err(
            ": bad ( { nums : (Array Int) } -- Int )\n  { nums : <> } -> 0n\n",
            "non-exhaustive match: { nums : Array }",
        );
        err(
            ": bad ( { name : Str } Bool -- Str )\n  { name : \"Ada\" } true -> \"Ada\"\n  { name : \"Grace\" } _ -> \"Grace\"\n",
            "non-exhaustive match: { name : Str } true, { name : Str } false, { name : \"Ada\" } false",
        );
        err(
            ": bad ( { name : Str | r } -- Str )\n  { name : n } -> n\n  _ -> \"\"\n",
            "redundant pattern",
        );
        err(
            ": bad ( { name : Str | r } -- Str )\n  {} -> \"\"\n  { name : n } -> n\n",
            "redundant pattern",
        );
        err(
            ": bad ( { name : Str, age : Int } -- Int )\n  { age : a, name : _ } -> a\n  { name : n, age : a } -> a\n",
            "redundant pattern",
        );
        err(
            ": bad ( { name : Str | r } -- Int )\n  { age : a } -> a\n",
            "record has no field age",
        );
        err(
            ": bad ( { name : Str } -- Str )\n  { name : n, age : a } -> n\n",
            "record has no field age",
        );
        err(
            ": bad ( { name : Int } -- Int )\n  { name : 0 } -> 0n\n",
            "type mismatch: expected Num, found Int",
        );
        err(
            ": bad ( Int -- Int )\n  {} -> 0n\n",
            "type mismatch: expected { | _ }, found Int",
        );
        err(
            ": bad ( a. a -- a )\n  { name : n } -> n\n",
            "type mismatch: expected { name : _ | _ }, found a",
        );
        err(
            ": bad ( { name : Str, city : Str } -- Str )\n  { name : n, city : n } -> n\n",
            "duplicate definition n",
        );
    }

    #[test]
    fn one_head_can_implement_several_classes() {
        ok(
            "; Box a = Mk a\n\n& Comparable a\n  eq ( a a -- Bool )\n\n& Sortable a\n  compare ( a a -- Bool )\n\n: eqBox ( (Box a) (Box a) -- Bool )\n  drop drop true\n\n: cmpBox ( (Box a) (Box a) -- Bool )\n  drop drop true\n\n# Comparable, Sortable (Box a)\n  eq = eqBox\n  compare = cmpBox\n\n: useEq ( (Box Int) (Box Int) -- Bool )\n  eq\n\n: useCmp ( (Box Int) (Box Int) -- Bool )\n  compare\n",
        );
        err(
            "; Box a = Mk a\n\n& Comparable a\n  eq ( a a -- Bool )\n\n& Mappable f\n  map ( (f a) (a -- b) -- (f b) )\n\n: eqBox ( (Box a) (Box a) -- Bool )\n  drop drop true\n\n: mapBox ( (Box a) (a -- b) -- (Box b) )\n  drop Mk\n\n# Comparable, Mappable (Box a)\n  eq = eqBox\n  map = mapBox\n",
            "kind mismatch: expected Type -> Type, found Type",
        );
    }

    #[test]
    fn an_instance_word_must_cover_the_method() {
        err(
            "; Pair a b = Pair a b\n\n& Mappable f\n  map ( (f a) (a -- b) -- (f b) )\n\n: mapLeft ( (Pair a b) (a -- c) -- (Pair c b) )\n  (Pair x y) f ->\n    x f call y Pair\n\n# Mappable (Pair a)\n  map = mapLeft\n",
            "type mismatch: expected _, found a",
        );
        ok(
            "; Pair a b = Pair a b\n\n& Mappable f\n  map ( (f a) (a -- b) -- (f b) )\n\n: mapRight ( (Pair a b) (b -- c) -- (Pair a c) )\n  (Pair x y) f ->\n    x y f call Pair\n\n# Mappable (Pair a)\n  map = mapRight\n\n: use ( (Pair Int Str) (Str -- Bool) -- (Pair Int Bool) )\n  map\n",
        );
        err(
            "; Maybe a = Nothing | Just a\n\n& Comparable a\n  eq ( a a -- Bool )\n\n& Mappable f\n  map ( (f a) (a -- b) -- (f b) )\n\n: mapCmp ( Comparable b => (Maybe a) (a -- b) -- (Maybe b) )\n  drop drop Nothing\n\n# Mappable Maybe\n  map = mapCmp\n",
            "ambiguous constraint Comparable b",
        );
    }

    #[test]
    fn a_superclass_constraint_brings_the_method() {
        ok(
            "& Eq a\n  eq ( a a -- Bool )\n\n& Eq a => Ord a\n  compare ( a a -- Bool )\n\n& Ord a => Sort a\n  sort ( a -- a )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n: cmpInt ( Int Int -- Bool )\n  drop drop true\n\n: sortInt ( Int -- Int )\n  dup drop\n\n# Eq Int\n  eq = eqInt\n\n# Ord Int\n  compare = cmpInt\n\n# Sort Int\n  sort = sortInt\n\n: both ( Sort a => a a -- Bool )\n  eq\n\n: use ( Int Int -- Bool )\n  both\n\n: exact ( Eq a, Ord a => a a -- Bool )\n  eq\n",
        );
        ok(
            "& Eq a\n  eq ( a a -- Bool )\n\n& Show a\n  show ( a -- Str )\n\n& Eq a, Show a => Both a\n  id ( a -- a )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n: showInt ( Int -- Str )\n  drop \"n\"\n\n: idInt ( Int -- Int )\n  dup drop\n\n# Eq Int\n  eq = eqInt\n\n# Show Int\n  show = showInt\n\n# Both Int\n  id = idInt\n\n: both ( Both a => a a -- Bool )\n  eq\n\n: shown ( Both a => a -- Str )\n  show\n",
        );
        ok(
            "; Box a = Mk a\n\n& Eq a\n  eq ( a a -- Bool )\n\n& Eq a => Ord a\n  compare ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n: eqBox ( Eq a => (Box a) (Box a) -- Bool )\n  drop drop true\n\n: cmpBox ( (Box a) (Box a) -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqInt\n\n# Eq (Box a)\n  eq = eqBox\n\n# Ord (Box a)\n  compare = cmpBox\n\n: both ( Ord (Box a) => (Box a) (Box a) -- Bool )\n  eq\n\n: use ( (Box Int) (Box Int) -- Bool )\n  both\n",
        );
        ok(
            "& Liftable m\n  pure ( a -- (m a) )\n\n& Chainable m\n  bind ( (m a) (a -- (m b)) -- (m b) )\n\n: step ( Liftable m, Chainable m => a (a -- (m b)) -- (m b) )\n  swap pure swap bind\n\n: step2 ( Liftable m => Chainable m => a (a -- (m b)) -- (m b) )\n  swap pure swap bind\n",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n& Eq a => Ord a\n  compare ( a a -- Bool )\n\n: cmpInt ( Int Int -- Bool )\n  drop drop true\n\n# Ord Int\n  compare = cmpInt\n",
            "no instance for Eq Int",
        );
        ok(
            "\
& Mappable f
  map ( (f a) (a -- b) -- (f b) )

& Mappable f => Sequenceable f

; Box a = Mk a

: mapBox ( (Box a) (a -- b) -- (Box b) )
  (Mk x) f ->
    x f call Mk

# Mappable Box
  map = mapBox

# Sequenceable Box

: use ( Sequenceable f => (f Int) (Int -- Bool) -- (f Bool) )
  map
",
        );
        ok(
            "\
& Eq a
  eq ( a a -- Bool )

& Eq a => Law a

: eqInt ( Int Int -- Bool )
  drop drop true

# Eq Int
  eq = eqInt

# Law Int

: use ( Law a => a a -- Bool )
  eq
",
        );
        err(
            "\
& Wrappable f
  wrap ( a -- (f a) )

; Box a = Mk a

: wrapBox ( a -- (Box a) )
  Mk

# Wrappable Box
  wrap = wrapBox

& Walk t
  step ( f a. Wrappable f => (t a) -- (f (t a)) )

: stepBox ( f a. (Box a) -- (f (Box a)) )
  drop Mk

# Walk Box
  step = stepBox
",
            "type mismatch: expected Wrappable f, found no constraint",
        );
        err(
            "\
& Eq a => Eq a
  eq ( a a -- Bool )
",
            "cyclic superclass: Eq",
        );
        err(
            "\
& Missing a => Ord a
  compare ( a a -- Bool )
",
            "undefined class Missing",
        );
        err(
            "\
& Eq a
  eq ( a a -- Bool )

& Eq b => Ord a
  compare ( a a -- Bool )
",
            "type variable b is not quantified",
        );
        err(
            "\
& Mappable f
  map ( (f a) (a -- b) -- (f b) )

& Mappable f => Bad f
  id ( f -- f )
",
            "kind mismatch: expected Type -> Type, found Type",
        );
        err(
            "\
& Eq a
  eq ( a a -- Bool )

& Eq a, Eq a => Ord a
  compare ( a a -- Bool )
",
            "duplicate definition Eq",
        );
        err(
            "& Eq a\n  eq ( a a -- Bool )\n\n& Eq a => Ord a\n  compare ( a a -- Bool )\n\n& Eq a => Show a\n  show ( a -- Str )\n\n: bad ( Ord a, Show a => a a -- Bool )\n  eq\n",
            "incoherent instance",
        );
    }
}
