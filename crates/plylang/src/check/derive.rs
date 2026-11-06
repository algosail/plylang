//! Derived instances for `Comparable`, `Showable`, `Sortable`, and `Hashable`.
//!
//! A module that can see the class receives an instance for each sum or product
//! it declares, unless a handwritten instance or a `{method}{Type}` word is already
//! there. Comparable's method name is the one that class declares, so a prelude
//! `equal` derives `equalPerson` and a test class whose method is `eq` still
//! derives `eqPerson`. A function field, a `Ref` field, or a `Cell` field is left alone. `Num` and `Map` keep
//! the instances written next to them, because those modules do not ask for a
//! derived one.
//!
//! `show` concatenates with `ply_show_cat` and `hash` mixes with `ply_hash_mix`.
//! Those words exist only while the derived body is checked. Lowering emits
//! `+` and `* 31n +` and does not import a function.

use crate::ast::{Instance, Item, Module, Term, TermNode, Type};
use crate::diagnostic::Diagnostic;
use crate::parser::parse;
use crate::paths::ParseMode;
use crate::span::Span;

use super::{Checker, TyNode};

impl Checker {
    pub(super) fn derive_instances(&mut self, module: &Module) -> Result<(), Diagnostic> {
        if let Some(method) = self.sole_class_method("Comparable") {
            self.derive_class(module, "Comparable", &method, &method, eq_source)?;
        }
        self.derive_showable(module)?;
        self.derive_sortable(module)?;
        self.derive_hashable(module)?;
        Ok(())
    }

    fn derive_showable(&mut self, module: &Module) -> Result<(), Diagnostic> {
        if module.language.is_some() {
            return Ok(());
        }
        let added = !self.words.contains_key("show");
        if !self.class_method_ready("Showable", "show") {
            return Ok(());
        }
        let added_cat = self.install_derive_word("ply_show_cat", "( Str Str -- Str )");
        let before = self.derived.len();
        let derived = self.derive_pending(module, "Showable", "show", show_source);
        if added_cat {
            self.words.remove("ply_show_cat");
        }
        derived?;
        if added_cat && self.derived.len() > before {
            self.inline_show_cat = true;
        }
        if added && self.derived.len() == before {
            self.words.remove("show");
        }
        self.use_dicts.clear();
        Ok(())
    }

    fn derive_sortable(&mut self, module: &Module) -> Result<(), Diagnostic> {
        if module.language.is_some() {
            return Ok(());
        }
        let added = !self.words.contains_key("compare");
        if !self.class_method_ready("Sortable", "compare") {
            return Ok(());
        }
        // The generated arms return LT, EQ, and GT. Without those constructors
        // the body would be a hard error, so this class is skipped.
        if !self.ordering_ready() {
            if added {
                self.words.remove("compare");
            }
            return Ok(());
        }
        let before = self.derived.len();
        self.derive_pending(module, "Sortable", "compare", compare_source)?;
        if added && self.derived.len() == before {
            self.words.remove("compare");
        }
        self.use_dicts.clear();
        Ok(())
    }

    fn derive_hashable(&mut self, module: &Module) -> Result<(), Diagnostic> {
        if module.language.is_some() {
            return Ok(());
        }
        let added = !self.words.contains_key("hash");
        if !self.class_method_ready("Hashable", "hash") {
            return Ok(());
        }
        let added_mix = self.install_derive_word("ply_hash_mix", "( Int Int -- Int )");
        let before = self.derived.len();
        let derived = self.derive_pending(module, "Hashable", "hash", hash_source);
        if added_mix {
            self.words.remove("ply_hash_mix");
        }
        derived?;
        if added_mix && self.derived.len() > before {
            self.inline_hash_mix = true;
        }
        if added && self.derived.len() == before {
            self.words.remove("hash");
        }
        self.use_dicts.clear();
        Ok(())
    }

    fn derive_class(
        &mut self,
        module: &Module,
        class_name: &str,
        method: &str,
        prefix: &str,
        source_for: fn(&crate::ast::Data, &str) -> String,
    ) -> Result<(), Diagnostic> {
        if module.language.is_some() {
            return Ok(());
        }
        let added = !self.words.contains_key(method);
        if !self.class_method_ready(class_name, method) {
            return Ok(());
        }
        let before = self.derived.len();
        self.derive_pending(module, class_name, prefix, source_for)?;
        if added && self.derived.len() == before {
            self.words.remove(method);
        }
        self.use_dicts.clear();
        Ok(())
    }

    /// Comparable declares one method. Its spelling is that method's name.
    fn sole_class_method(&self, class_name: &str) -> Option<String> {
        let class = self.classes.get(class_name)?;
        let [method] = class.methods.as_slice() else {
            return None;
        };
        Some(method.name.clone())
    }

    /// The class is in scope, and `method` is that class's method.
    fn class_method_ready(&mut self, class_name: &str, method: &str) -> bool {
        let Some(class) = self.classes.get(class_name).cloned() else {
            return false;
        };
        let Some(info) = class.methods.iter().find(|item| item.name == method) else {
            return false;
        };
        if let Some(existing) = self.words.get(method) {
            return existing
                .constraints
                .iter()
                .any(|constraint| constraint.class_name == class_name);
        }
        if !self.visible_classes.contains(class_name) {
            return false;
        }
        self.words.insert(method.to_string(), info.scheme.clone());
        true
    }

    fn ordering_ready(&self) -> bool {
        ["LT", "EQ", "GT"].into_iter().all(|name| {
            self.ctor_words
                .get(name)
                .is_some_and(|ctor| ctor.type_name == "Ordering")
        })
    }

    /// A scheme used only while derived bodies are checked. `true` when this
    /// call inserted it. A word the module already defined is left in place.
    pub(super) fn install_derive_word(&mut self, name: &str, scheme: &str) -> bool {
        if self.words.contains_key(name) {
            return false;
        }
        let source = format!(": {name} {scheme}\n  drop\n");
        let parsed = parse(&source, ParseMode::Source).unwrap_or_else(|error| {
            panic!("derive helper {name}: {}\n{source}", error.message)
        });
        let Some(Item::Def(def)) = parsed.items.into_iter().next() else {
            panic!("derive helper {name} has no word\n{source}");
        };
        let saved = self.relax_visibility;
        self.relax_visibility = true;
        let registered = self.register_def(&def);
        self.relax_visibility = saved;
        registered.unwrap_or_else(|error| panic!("derive helper {name}: {}\n{source}", error.message));
        true
    }

    fn derive_pending(
        &mut self,
        module: &Module,
        class_name: &str,
        prefix: &str,
        source_for: fn(&crate::ast::Data, &str) -> String,
    ) -> Result<(), Diagnostic> {
        let mut pending: Vec<usize> = module
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item {
                Item::Data(data) if self.derivable_for(data, class_name, prefix) => Some(index),
                _ => None,
            })
            .collect();
        loop {
            let mut later = Vec::new();
            let mut progress = false;
            for index in pending {
                let Item::Data(data) = &module.items[index] else {
                    continue;
                };
                let data = data.clone();
                let source = source_for(&data, prefix);
                if let Some(items) = self.derive_generated(module, &data, &source)? {
                    progress = true;
                    self.derived.extend(items);
                } else {
                    later.push(index);
                }
            }
            if !progress {
                break;
            }
            pending = later;
        }
        Ok(())
    }

    fn derivable_for(&self, data: &crate::ast::Data, class_name: &str, prefix: &str) -> bool {
        if data.foreign || data.ctors.is_empty() || self.has_instance(class_name, &data.name) {
            return false;
        }
        if self.words.contains_key(&format!("{prefix}{}", data.name)) {
            return false;
        }
        if prefix == "compare"
            && data.ctors.iter().any(|ctor| ctor.fields.len() > 1)
            && self.words.contains_key(&format!("plyOrdering{}", data.name))
        {
            return false;
        }
        data.ctors.iter().all(|ctor| {
            ctor.fields
                .iter()
                .all(|field| !type_blocks_derive(&field.ty))
        })
    }

    fn has_instance(&self, class_name: &str, data_name: &str) -> bool {
        self.instances.iter().any(|inst| {
            if inst.class_name != class_name || inst.args.len() != 1 {
                return false;
            }
            let ty = self.expose(inst.args[0]);
            matches!(&self.tys[ty.0 as usize], TyNode::Ctor(name, _) if name == data_name)
        })
    }

    /// `Ok(None)` when a field has no instance. The attempt is removed.
    fn derive_generated(
        &mut self,
        module: &Module,
        data: &crate::ast::Data,
        source: &str,
    ) -> Result<Option<Vec<Item>>, Diagnostic> {
        let parsed = parse(source, ParseMode::Source).unwrap_or_else(|error| {
            panic!("derived {} for {}: {}\n{source}", data.name, data.name, error.message)
        });
        let mut defs = Vec::new();
        let mut instance = None;
        for item in parsed.items {
            match item {
                Item::Def(item) => defs.push(item),
                Item::Instance(item) => instance = Some(item),
                _ => {}
            }
        }
        if defs.is_empty() {
            panic!("derived {} has no word\n{source}", data.name);
        }
        let Some(mut instance) = instance else {
            panic!("derived {} has no instance\n{source}", data.name);
        };
        let base = 1_000_000_000u32 + (self.derived.len() as u32) * 10_000;
        for def in &mut defs {
            shift_def(def, base);
        }
        shift_instance(&mut instance, base);
        let mark = self.instances.len();
        let sites = self.sites.len();
        let words: Vec<String> = defs.iter().map(|def| def.name.clone()).collect();
        let saved = self.relax_visibility;
        self.relax_visibility = true;
        for def in &defs {
            if let Err(error) = self.register_def(def) {
                self.relax_visibility = saved;
                for name in &words {
                    self.words.remove(name);
                }
                return Err(error);
            }
        }
        let origin = self.module_key.clone();
        let declared = self.declare_instance(&instance, module, true, &origin);
        self.relax_visibility = saved;
        if let Err(error) = declared {
            for name in &words {
                self.words.remove(name);
            }
            self.instances.truncate(mark);
            self.sites.truncate(sites);
            if error.code == "PL0087" || error.code == "PL0088" {
                return Ok(None);
            }
            return Err(error);
        }
        for def in &defs {
            self.relax_visibility = true;
            let checked = self.check_body(def);
            self.relax_visibility = saved;
            if let Err(error) = checked {
                for name in &words {
                    self.words.remove(name);
                }
                self.instances.truncate(mark);
                self.sites.truncate(sites);
                self.use_dicts.clear();
                if error.code == "PL0088" {
                    return Ok(None);
                }
                return Err(Diagnostic::with_code(data.name_span, error.code, error.message));
            }
        }
        self.sites.truncate(sites);
        let mut items: Vec<Item> = defs.into_iter().map(Item::Def).collect();
        if derived_is_public(module, data) {
            items.push(Item::Instance(instance));
        }
        Ok(Some(items))
    }
}

fn type_blocks_derive(ty: &Type) -> bool {
    match ty {
        Type::Var(_) => false,
        Type::Ctor(name) => name == "Ref" || name == "Cell",
        Type::App(head, args) => type_blocks_derive(head) || args.iter().any(type_blocks_derive),
        Type::Fun(_) => true,
        Type::Record(row) => {
            row.fields.iter().any(|(_, ty)| type_blocks_derive(ty))
                || row.tail_ty().is_some_and(type_blocks_derive)
        }
    }
}

fn derived_is_public(module: &Module, data: &crate::ast::Data) -> bool {
    let Some(list) = module.exports.list() else {
        return false;
    };
    list.has(&data.name) || data.ctors.iter().any(|ctor| list.has(&ctor.name))
}

fn field_vars(data: &crate::ast::Data) -> Vec<String> {
    let mut found = Vec::new();
    for ctor in &data.ctors {
        for field in &ctor.fields {
            collect_vars(&field.ty, &mut found);
        }
    }
    data.params
        .iter()
        .filter(|param| found.iter().any(|have| have == *param))
        .cloned()
        .collect()
}

fn collect_vars(ty: &Type, found: &mut Vec<String>) {
    match ty {
        Type::Var(name) => {
            if !found.iter().any(|have| have == name) {
                found.push(name.clone());
            }
        }
        Type::Ctor(_) => {}
        Type::App(head, args) => {
            collect_vars(head, found);
            for arg in args {
                collect_vars(arg, found);
            }
        }
        Type::Fun(effect) => {
            for ty in effect.input.items.iter().chain(&effect.output.items) {
                collect_vars(ty, found);
            }
        }
        Type::Record(row) => {
            for (_, ty) in &row.fields {
                collect_vars(ty, found);
            }
            if let Some(ty) = row.tail_ty() {
                collect_vars(ty, found);
            }
        }
    }
}

fn head_of(data: &crate::ast::Data) -> String {
    if data.params.is_empty() {
        data.name.clone()
    } else {
        format!("({} {})", data.name, data.params.join(" "))
    }
}

fn derived_scheme(data: &crate::ast::Data, class_name: &str, effect: &str) -> String {
    let constrained = field_vars(data);
    if constrained.is_empty() {
        format!("( {effect} )")
    } else {
        let vars = constrained.join(" ");
        let classes = constrained
            .iter()
            .map(|var| format!("{class_name} {var}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("( {vars}. {classes} => {effect} )")
    }
}

fn eq_source(data: &crate::ast::Data, method: &str) -> String {
    let word = format!("{method}{}", data.name);
    let head = head_of(data);
    let scheme = derived_scheme(data, "Comparable", &format!("{head} {head} -- Bool"));
    let mut body = String::new();
    for ctor in &data.ctors {
        let left = ctor_pattern(&ctor.name, ctor.fields.len(), "l");
        let right = ctor_pattern(&ctor.name, ctor.fields.len(), "r");
        body.push_str(&format!("  {left} {right} ->\n"));
        if ctor.fields.is_empty() {
            body.push_str("    true\n");
            continue;
        }
        body.push_str(&format!("    l0 r0 {method}\n"));
        for index in 1..ctor.fields.len() {
            body.push_str(&format!("    l{index} r{index} [ {method} ] cons cons\n"));
            body.push_str("    [ false ]\n");
            body.push_str("    if\n");
        }
    }
    if data.ctors.len() > 1 {
        body.push_str("  _ _ ->\n    false\n");
    }
    format!(": {word} {scheme}\n{body}\n# Comparable {head}\n  {method} = {word}\n")
}

fn show_source(data: &crate::ast::Data, _method: &str) -> String {
    let word = format!("show{}", data.name);
    let head = head_of(data);
    let scheme = derived_scheme(data, "Showable", &format!("{head} -- Str"));
    let mut body = String::new();
    for ctor in &data.ctors {
        let pattern = ctor_pattern(&ctor.name, ctor.fields.len(), "l");
        body.push_str(&format!("  {pattern} ->\n"));
        if ctor.fields.is_empty() {
            body.push_str(&format!("    \"{}\"\n", ctor.name));
            continue;
        }
        body.push_str(&format!("    \"({} \"\n", ctor.name));
        body.push_str("    l0 show ply_show_cat\n");
        for index in 1..ctor.fields.len() {
            body.push_str("    \" \" ply_show_cat\n");
            body.push_str(&format!("    l{index} show ply_show_cat\n"));
        }
        body.push_str("    \")\" ply_show_cat\n");
    }
    format!(": {word} {scheme}\n{body}\n# Showable {head}\n  show = {word}\n")
}

fn compare_source(data: &crate::ast::Data, _method: &str) -> String {
    let word = format!("compare{}", data.name);
    let head = head_of(data);
    let scheme = derived_scheme(data, "Sortable", &format!("{head} {head} -- Ordering"));
    let helper = format!("plyOrdering{}", data.name);
    let needs_helper = data.ctors.iter().any(|ctor| ctor.fields.len() > 1);
    let mut body = String::new();
    for ctor in &data.ctors {
        let left = ctor_pattern(&ctor.name, ctor.fields.len(), "l");
        let right = ctor_pattern(&ctor.name, ctor.fields.len(), "r");
        body.push_str(&format!("  {left} {right} ->\n"));
        body.push_str(&compare_fields(&helper, ctor.fields.len()));
    }
    if data.ctors.len() > 1 {
        for ctor in data.ctors.iter().take(data.ctors.len() - 1) {
            let pattern = ctor_wild(&ctor.name, ctor.fields.len());
            body.push_str(&format!("  {pattern} _ ->\n    LT\n"));
            body.push_str(&format!("  _ {pattern} ->\n    GT\n"));
        }
    }
    let mut source = String::new();
    if needs_helper {
        source.push_str(&format!(
            ": {helper} ( Ordering ( -- Ordering ) -- Ordering )\n  (LT) _ -> LT\n  (GT) _ -> GT\n  (EQ) next -> next call\n\n"
        ));
    }
    source.push_str(&format!(
        ": {word} {scheme}\n{body}\n# Sortable {head}\n  compare = {word}\n"
    ));
    source
}

/// The first difference wins. Later fields stay in a quotation until the
/// earlier comparison is EQ.
fn compare_fields(helper: &str, count: usize) -> String {
    if count == 0 {
        return "    EQ\n".to_string();
    }
    field_step(helper, 0, count, 2)
}

fn field_step(helper: &str, index: usize, count: usize, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    if index + 1 == count {
        return format!("{pad}l{index} r{index} compare\n");
    }
    let mut out = format!("{pad}l{index} r{index} compare\n");
    out.push_str(&format!("{pad}[\n"));
    out.push_str(&field_step(helper, index + 1, count, indent + 1));
    out.push_str(&format!("{pad}] {helper}\n"));
    out
}

fn hash_source(data: &crate::ast::Data, _method: &str) -> String {
    let word = format!("hash{}", data.name);
    let head = head_of(data);
    let scheme = derived_scheme(data, "Hashable", &format!("{head} -- Int"));
    let mut body = String::new();
    for (index, ctor) in data.ctors.iter().enumerate() {
        let pattern = ctor_pattern(&ctor.name, ctor.fields.len(), "l");
        body.push_str(&format!("  {pattern} ->\n"));
        body.push_str(&format!("    {index}n\n"));
        for field in 0..ctor.fields.len() {
            body.push_str(&format!("    l{field} hash ply_hash_mix\n"));
        }
    }
    format!(": {word} {scheme}\n{body}\n# Hashable {head}\n  hash = {word}\n")
}

fn ctor_pattern(name: &str, fields: usize, side: &str) -> String {
    if fields == 0 {
        return format!("({name})");
    }
    let names = (0..fields)
        .map(|index| format!("{side}{index}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("({name} {names})")
}

fn ctor_wild(name: &str, fields: usize) -> String {
    if fields == 0 {
        return format!("({name})");
    }
    let names = std::iter::repeat("_").take(fields).collect::<Vec<_>>().join(" ");
    format!("({name} {names})")
}

fn shift_span(span: Span, base: u32) -> Span {
    Span {
        start: span.start.saturating_add(base),
        end: span.end.saturating_add(base),
    }
}

fn shift_def(def: &mut crate::ast::Def, base: u32) {
    def.span = shift_span(def.span, base);
    def.name_span = shift_span(def.name_span, base);
    for clause in &mut def.clauses {
        clause.span = shift_span(clause.span, base);
        for pattern in &mut clause.patterns {
            shift_pattern(pattern, base);
        }
        for term in &mut clause.body {
            shift_term(term, base);
        }
    }
    for term in &mut def.body {
        shift_term(term, base);
    }
}

fn shift_instance(instance: &mut Instance, base: u32) {
    instance.span = shift_span(instance.span, base);
    for binding in &mut instance.methods {
        binding.name_span = shift_span(binding.name_span, base);
    }
}

fn shift_pattern(pattern: &mut crate::ast::Pattern, base: u32) {
    match pattern {
        crate::ast::Pattern::Bind(span, _) => *span = shift_span(*span, base),
        crate::ast::Pattern::Ctor(_, fields) | crate::ast::Pattern::Array(fields) => {
            for field in fields {
                shift_pattern(field, base);
            }
        }
        crate::ast::Pattern::Record(fields) => {
            for (_, field) in fields {
                shift_pattern(field, base);
            }
        }
        crate::ast::Pattern::Wild | crate::ast::Pattern::Lit(_) | crate::ast::Pattern::Unpack(_) => {
        }
    }
}

fn shift_term(term: &mut Term, base: u32) {
    term.span = shift_span(term.span, base);
    match &mut term.node {
        TermNode::Quote(terms) | TermNode::Array(terms) => {
            for term in terms {
                shift_term(term, base);
            }
        }
        TermNode::Record(fields) => {
            for (_, term) in fields {
                shift_term(term, base);
            }
        }
        TermNode::Word(_)
        | TermNode::Lit(_)
        | TermNode::Field(_)
        | TermNode::FieldSet(_)
        | TermNode::FieldInsert(_)
        | TermNode::PlaceSet(_)
        | TermNode::PlaceBind(_)
        | TermNode::Bind(_) => {}
    }
}
