//! Classes, instances, and the dictionaries they select.

use std::collections::HashSet;

use crate::ast::{Class, Instance, Item, Module, Type};
use crate::builtin;
use crate::codes::Error;
use crate::diagnostic::Diagnostic;
use crate::span::Span;

use super::{
    collect_ty_vars, find_def, instance_key, known_at, subst_type, Checker, ClassInfo, ClassMethod,
    DeferredUse, Dict, EffectId, InstanceInfo, InstanceMethod, KnownModule, Memo, ResolvedMethod,
    RowId, RowNode, Scope, Seen, SemConstraint, SemScheme, StackId, StackNode, Subst, TyId, TyNode,
};

impl Checker {
    pub(super) fn register_class(&mut self, class: &Class) -> Result<(), Diagnostic> {
        if self.classes.contains_key(&class.name) {
            return Err(self.error(
                class.name_span,
                Error::DuplicateDefinition(class.name.to_string()),
            ));
        }
        self.install_class(class, true)
    }

    pub(super) fn ensure_class(&mut self, class: &Class) -> Result<(), Diagnostic> {
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
            if methods
                .iter()
                .any(|have: &ClassMethod| have.name == method.name)
            {
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
            let mut scheme =
                self.elaborate_with(&method.scheme, method.name_span, scope, preset)?;
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

    pub(super) fn elab_constraint(
        &mut self,
        constraint: &crate::ast::Constraint,
        scope: &mut Scope,
        span: Span,
    ) -> Result<SemConstraint, Diagnostic> {
        let written = constraint.class_name.clone();
        if !self.relax_visibility && !self.visible_classes.contains(&written) {
            return Err(self.error(span, Error::UndefinedClass(written)));
        }
        let canon = self.canonical(&written).to_string();
        let class = self
            .classes
            .get(&canon)
            .cloned()
            .ok_or_else(|| self.error(span, Error::UndefinedClass(written.clone())))?;
        if constraint.args.len() != class.params.len() {
            let count = class.params.len();
            return Err(self.error(
                span,
                Error::ClassArity {
                    name: written,
                    count,
                },
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
            class_name: canon,
            args,
        })
    }

    pub(super) fn export_imported_method(
        &mut self,
        provider: &Module,
        remote: &str,
        local: &str,
        span: Span,
    ) -> Result<bool, Diagnostic> {
        let Some(class) = provider.items.iter().find_map(|item| match item {
            Item::Class(class) if class.methods.iter().any(|method| method.name == remote) => {
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
                    .find(|method| method.name == remote)
                    .map(|method| method.scheme.clone())
            })
            .expect("class method was installed");
        self.export_word(local, scheme, span)?;
        Ok(true)
    }

    /// Instances written in `origin`, plus instances reached through its imports.
    /// Imports are walked first, so a superclass instance is present before the
    /// subclass instance that needs it. A module is entered once. Dropping it
    /// from the path afterwards walked every path and copied each instance again.
    pub(super) fn import_instance_surface(
        &mut self,
        origin: &str,
        known: &[KnownModule],
    ) -> Result<(), Diagnostic> {
        if self.seen_instances.contains(origin) {
            return Ok(());
        }
        let Some(known_module) = known_at(known, origin) else {
            return Ok(());
        };
        self.seen_instances.insert(origin.to_string());
        let key = instance_key(known_module);
        // A superclass instance often lives in an imported module. Declare those
        // first. The module is already marked seen, so a cycle cannot re-enter.
        for (index, _) in known_module.module.imports.iter().enumerate() {
            let Some(dep) = known_module.deps.get(index) else {
                continue;
            };
            self.import_instance_surface(dep, known)?;
        }
        for item in &known_module.module.items {
            if let Item::Instance(instance) = item {
                // A head this file kept off `@` is not a type the importer can
                // name. The interface omits that instance. Importing it would
                // look the hidden type up in the importer.
                if crate::surface::instance_head_is_hidden(&known_module.module, instance) {
                    continue;
                }
                self.declare_instance(instance, &known_module.module, false, &key)?;
            }
        }
        Ok(())
    }

    pub(super) fn declare_instance(
        &mut self,
        instance: &Instance,
        module: &Module,
        local: bool,
        origin: &str,
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
                .filter(|(_, class)| {
                    class
                        .methods
                        .iter()
                        .any(|method| method.name == binding.name)
                })
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
            self.declare_class_instance(instance, module, local, name, class, origin)?;
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
        origin: &str,
    ) -> Result<(), Diagnostic> {
        if local && self.orphan(class_name, &instance.args) {
            return Err(self.error(instance.span, Error::OrphanInstance));
        }
        if instance.args.len() != class.params.len() {
            let count = class.params.len();
            return Err(self.error(
                instance.span,
                Error::ClassArity {
                    name: class_name.to_string(),
                    count,
                },
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
                return Err(
                    self.error(instance.span, Error::MissingMethod(method.name.to_string()))
                );
            }
        }
        let mut methods = Vec::new();
        for method in &class.methods {
            let binding = instance
                .methods
                .iter()
                .find(|binding| binding.name == method.name)
                .expect("method was checked");
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
            origin: origin.to_string(),
        };
        for existing in self.instances.clone() {
            if self.overlaps(&existing, &info) {
                // The same instance from the same module is idempotent. A second
                // `#` in this file is still incoherent.
                if !local && existing.origin == info.origin {
                    return Ok(());
                }
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
            Type::Fun(effect) => effect
                .input
                .items
                .iter()
                .chain(&effect.output.items)
                .any(|ty| self.defines_type_seen(ty, seen)),
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
            TyNode::Alias { args, body, .. } => {
                args.into_iter()
                    .any(|arg| self.ty_has_skolem(arg, skolems, seen))
                    || self.ty_has_skolem(body, skolems, seen)
            }
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
                self.stack_has_skolem(rest, skolems, seen) || self.ty_has_skolem(ty, skolems, seen)
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
    pub(super) fn solve_or_defer(
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
        constraints.iter().any(|constraint| {
            constraint
                .args
                .iter()
                .any(|ty| self.flexible(*ty, &mut Seen::default()))
        })
    }

    pub(super) fn dicts_for_use(
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

    pub(super) fn finish_deferred(&mut self) -> Result<(), Diagnostic> {
        let pending = std::mem::take(&mut self.deferred);
        for item in pending {
            if let Some(constraint) = item.constraints.iter().find(|constraint| {
                constraint
                    .args
                    .iter()
                    .any(|ty| self.flexible(*ty, &mut Seen::default()))
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
                        origin: inst.origin.clone(),
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
        found.ok_or_else(|| self.error(span, Error::NoInstance(self.show_constraint(constraint))))
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
                return Err(self.error(span, Error::NoInstance(self.show_constraint(constraint))));
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

    /// An assumption's superclasses cover `wanted`. The projection is one chain
    /// of fields. Several paths from one assumption are the same dictionary, so
    /// the shortest chain is enough. A second assumption that also covers
    /// `wanted` is incoherent.
    fn entail(&mut self, wanted: &SemConstraint, span: Span) -> Result<Option<Dict>, Diagnostic> {
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
            if found.is_some() {
                return Err(self.error(span, Error::IncoherentInstance));
            }
            let path = paths
                .iter()
                .min_by_key(|path| path.len())
                .expect("a non-empty path list has a shortest path");
            let mut dict = Dict::Assumption(index);
            for field in path {
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

    pub(super) fn is_class_method(&self, name: &str) -> bool {
        let name = self.canonical(name);
        self.classes
            .values()
            .any(|class| class.methods.iter().any(|method| method.name == name))
    }
}
