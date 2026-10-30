//! Lower a checked module to direct-style ANF.
//!
//! The operand stack is the concrete prefix. A word's own prefix is the cons
//! spine above its tail node; following that tail's link would pass the
//! caller's locals into the callee. A shuffle reuses those operands. A visible
//! quotation is inlined, so a tail it does not consume stays in the caller
//! even when two `if` branches were written with different arities.
//!
//! A quotation literal stays pending until the word that uses it has ascribed
//! its input. `cons` only records the captured value. The body is lowered
//! later, so `call` inside it sees that value's arity.

use std::collections::HashMap;

use crate::anf::{
    Access, AnfCtor, AnfData, AnfDef, AnfModule, Choice, Cond, DictField, DictValue, Expr, Let,
    LocalId, PatTest, QuoteForm, Value,
};
use crate::builtin::{self, Lowering};
use crate::ast::{Item, Lit, Module, Pattern, Term, TermNode};
use crate::diagnostic::Diagnostic;
use crate::codes::Error;
use crate::span::Span;

use super::{Checker, Dict, EffectId, StackId, StackNode, TyId, TyNode};

pub(crate) fn to_anf(module: &Module) -> Result<AnfModule, Diagnostic> {
    Checker::new().lower_module(module)
}

pub fn javascript(module: &Module) -> Result<String, Diagnostic> {
    Ok(to_anf(module)?.javascript())
}

#[derive(Clone)]
enum Operand {
    Local(LocalId),
    Quote(QuoteForm),
    /// The body waits until a use ascribes `input`.
    Pending(PendingForm),
}

#[derive(Clone)]
struct PendingForm {
    span: Span,
    terms: Vec<Term>,
    env: Vec<HashMap<String, LocalId>>,
    /// Schemes in scope when the quotation was built. Realizing restores them
    /// so a name closed over by the body is still a word.
    locals: Vec<HashMap<String, super::SemScheme>>,
    input: StackId,
    output: StackId,
    effect: EffectId,
    /// Values `cons` pushed, first cons first. Each was the top input then.
    captures: Vec<LocalId>,
}

struct Frame {
    /// Bottom to top.
    stack: Vec<Operand>,
    /// Inputs discovered from a flexible quotation tail, bottom to top.
    params: Vec<LocalId>,
    flexible: bool,
    lets: Vec<Let>,
    env: Vec<HashMap<String, LocalId>>,
}

impl Frame {
    fn find_env(&self, name: &str) -> Option<LocalId> {
        for map in self.env.iter().rev() {
            if let Some(id) = map.get(name) {
                return Some(*id);
            }
        }
        None
    }

    fn prepend_params(&mut self, checker: &mut Checker, count: usize) {
        let mut extra = Vec::with_capacity(count);
        for _ in 0..count {
            extra.push(checker.fresh_local());
        }
        self.params.splice(0..0, extra.iter().copied());
        let operands = extra.into_iter().map(Operand::Local);
        self.stack.splice(0..0, operands);
    }

    fn pop_n(&mut self, checker: &mut Checker, count: usize) -> Vec<Operand> {
        if self.stack.len() < count {
            let deficit = count - self.stack.len();
            if !self.flexible {
                panic!(
                    "stack underflow while lowering ({count} needed, {} present)",
                    self.stack.len()
                );
            }
            self.prepend_params(checker, deficit);
        }
        let at = self.stack.len() - count;
        self.stack.split_off(at)
    }

    fn pop_one(&mut self, checker: &mut Checker) -> Operand {
        self.pop_n(checker, 1).pop().expect("pop one")
    }

    fn force(
        &mut self,
        checker: &mut Checker,
        operand: Operand,
    ) -> Result<LocalId, Diagnostic> {
        match operand {
            Operand::Local(id) => Ok(id),
            Operand::Pending(pending) => {
                let quote = checker.realize(pending)?;
                self.force(checker, Operand::Quote(quote))
            }
            Operand::Quote(quote) => {
                let id = checker.fresh_local();
                checker
                    .fn_arity
                    .insert(id, (quote.params.len(), quote.body.result.len()));
                self.lets.push(Let {
                    binds: vec![id],
                    value: Value::Quote(quote),
                });
                Ok(id)
            }
        }
    }
}

impl Checker {
    fn lower_module(&mut self, module: &Module) -> Result<AnfModule, Diagnostic> {
        self.prepare(module)?;
        self.check_bodies(module)?;
        self.lower_prepared(module)
    }

    pub(crate) fn lower_prepared(&mut self, module: &Module) -> Result<AnfModule, Diagnostic> {
        let mut defs = Vec::new();
        for item in &module.items {
            if let Item::Def(def) = item {
                if !def.body.is_empty() || !def.clauses.is_empty() {
                    defs.push(self.lower_def(module, def)?);
                }
            }
        }
        let datas = module
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Data(data) if !data.ctors.is_empty() => Some(AnfData {
                    ctors: data
                        .ctors
                        .iter()
                        .map(|ctor| AnfCtor {
                            name: ctor.name.clone(),
                            fields: ctor.fields.len(),
                            names: ctor.fields.iter().map(|field| field.name.clone()).collect(),
                            newtype: data.newtype,
                        })
                        .collect(),
                }),
                _ => None,
            })
            .collect();
        let mut anf = AnfModule {
            datas,
            defs,
            needs_unit: false,
        };
        anf.needs_unit = module_uses_unit(&anf);
        Ok(anf)
    }

    fn lower_def(
        &mut self,
        module: &crate::ast::Module,
        def: &crate::ast::Def,
    ) -> Result<AnfDef, Diagnostic> {
        self.use_dicts.clear();
        self.deferred.clear();
        let first = self.lower_def_pass(module, def)?;
        if self.use_dicts.is_empty() {
            return Ok(first);
        }
        let second = self.lower_def_pass(module, def)?;
        self.use_dicts.clear();
        Ok(second)
    }

    fn lower_def_pass(
        &mut self,
        module: &crate::ast::Module,
        def: &crate::ast::Def,
    ) -> Result<AnfDef, Diagnostic> {
        let scheme = self.lookup(&def.name).expect("def was registered");
        let (input, output, effect, constraints) = self.instantiate_scheme(&scheme, true);
        let saved = self.ambient.replace(effect);
        self.assumption_locals.clear();
        let mut params = Vec::new();
        for _ in &constraints {
            let id = self.fresh_local();
            self.assumption_locals.push(id);
            params.push(id);
        }
        self.assumptions = constraints;
        self.deferred.clear();
        let mut stack = Vec::new();
        for ty in self.cons_tys(input) {
            let id = self.fresh_local();
            if let Some(arity) = self.fun_arity(ty) {
                self.fn_arity.insert(id, arity);
            }
            params.push(id);
            stack.push(Operand::Local(id));
        }
        let mut frame = Frame {
            stack,
            params: Vec::new(),
            flexible: false,
            lets: Vec::new(),
            env: vec![HashMap::new()],
        };
        self.locals.push(HashMap::new());
        let lowered = if def.clauses.is_empty() {
            self.lower_terms(&def.body, input, &mut frame)
        } else {
            self.lower_clauses(def, input, &mut frame)
        };
        self.locals.pop();
        let end = match lowered {
            Ok(end) => end,
            Err(error) => {
                self.deferred.clear();
                self.ambient = saved;
                self.assumptions.clear();
                self.assumption_locals.clear();
                return Err(error);
            }
        };
        if let Err(message) = self.unify_stack(end, output) {
            self.deferred.clear();
            self.ambient = saved;
            self.assumptions.clear();
            self.assumption_locals.clear();
            return Err(self.error(def.name_span, message));
        }
        let finished = self.finish_deferred();
        self.ambient = saved;
        self.assumptions.clear();
        self.assumption_locals.clear();
        finished?;
        self.escaped_effect(def.name_span)?;
        let result = drain_results(self, &mut frame)?;
        let concrete = self.cons_len(output);
        if result.len() != concrete {
            panic!(
                "{}: anf produced {} values, signature output has {concrete}",
                def.name,
                result.len()
            );
        }
        Ok(AnfDef {
            name: def.name.clone(),
            export: crate::surface::js_exports_def(module, def),
            params,
            body: Expr {
                lets: frame.lets,
                result,
            },
        })
    }

    fn lower_terms(
        &mut self,
        terms: &[Term],
        mut stack: StackId,
        frame: &mut Frame,
    ) -> Result<StackId, Diagnostic> {
        for term in terms {
            stack = self.lower_term(term, stack, frame)?;
        }
        Ok(stack)
    }

    fn lower_term(
        &mut self,
        term: &Term,
        stack: StackId,
        frame: &mut Frame,
    ) -> Result<StackId, Diagnostic> {
        let output = match &term.node {
            TermNode::Word(name) => self.lower_word(term.span, name, stack, frame)?,
            TermNode::Lit(lit) => self.lower_lit(lit, stack, frame),
            TermNode::Quote(terms) => {
                let input = self.stack_var();
                let output = self.stack_var();
                let effect = self.effect_var();
                let fun = self.ty_fun(input, output, effect);
                frame.stack.push(Operand::Pending(PendingForm {
                    span: term.span,
                    terms: terms.clone(),
                    env: frame.env.clone(),
                    locals: self.locals.clone(),
                    input,
                    output,
                    effect,
                    captures: Vec::new(),
                }));
                self.stack_cons(stack, fun)
            }
            TermNode::Field(name) => {
                let output = self.project_field(term.span, name, stack)?;
                let operand = frame.pop_one(self);
                let base = frame.force(self, operand)?;
                let id = self.fresh_local();
                frame.lets.push(Let {
                    binds: vec![id],
                    value: Value::Prop {
                        base,
                        name: name.clone(),
                    },
                });
                // A field may be a quotation. `call` needs the arity on this local,
                // the same way a pattern bind and a parameter already record it.
                if let Some(ty) = self.cons_tys(output).last().copied() {
                    self.note_fn(id, ty);
                }
                frame.stack.push(Operand::Local(id));
                output
            }
            TermNode::FieldSet(name) => self.lower_set_field(term.span, name, stack, frame)?,
            TermNode::FieldInsert(name) => self.lower_insert_field(term.span, name, stack, frame)?,
            TermNode::Array(elements) => self.lower_array(elements, stack, frame)?,
            TermNode::Record(fields) => self.lower_record(fields, stack, frame)?,
            TermNode::Bind(names) => self.lower_bind(term.span, names, stack, frame)?,
        };
        let concrete = self.cons_len(output);
        if concrete != frame.stack.len() {
            panic!(
                "concrete stack {concrete}, operands {} after {:?}",
                frame.stack.len(),
                term.node
            );
        }
        Ok(output)
    }

    fn lower_lit(&mut self, lit: &Lit, stack: StackId, frame: &mut Frame) -> StackId {
        let ty = match lit {
            Lit::Int(_) => self.ty_ctor("Int", Vec::new()),
            Lit::Number(_) => self.ty_ctor("Num", Vec::new()),
            Lit::String(_) => self.ty_ctor("Str", Vec::new()),
            Lit::Bool(_) => self.ty_ctor("Bool", Vec::new()),
        };
        let id = self.fresh_local();
        frame.lets.push(Let {
            binds: vec![id],
            value: Value::Lit(lit.clone()),
        });
        frame.stack.push(Operand::Local(id));
        self.stack_cons(stack, ty)
    }

    fn lower_set_field(
        &mut self,
        span: Span,
        name: &str,
        stack: StackId,
        frame: &mut Frame,
    ) -> Result<StackId, Diagnostic> {
        let output = self.set_field(span, name, stack)?;
        let value = frame.pop_one(self);
        let base = frame.pop_one(self);
        let value = frame.force(self, value)?;
        let base = frame.force(self, base)?;
        let id = self.fresh_local();
        frame.lets.push(Let {
            binds: vec![id],
            value: Value::SetProp {
                base,
                name: name.to_string(),
                value,
            },
        });
        frame.stack.push(Operand::Local(id));
        Ok(output)
    }

    fn lower_insert_field(
        &mut self,
        span: Span,
        name: &str,
        stack: StackId,
        frame: &mut Frame,
    ) -> Result<StackId, Diagnostic> {
        let output = self.insert_field(span, name, stack)?;
        let value = frame.pop_one(self);
        let base = frame.pop_one(self);
        let value = frame.force(self, value)?;
        let base = frame.force(self, base)?;
        let id = self.fresh_local();
        frame.lets.push(Let {
            binds: vec![id],
            value: Value::SetProp {
                base,
                name: name.to_string(),
                value,
            },
        });
        frame.stack.push(Operand::Local(id));
        Ok(output)
    }

    fn lower_array(
        &mut self,
        elements: &[Term],
        stack: StackId,
        frame: &mut Frame,
    ) -> Result<StackId, Diagnostic> {
        let mut elem_ty = None;
        let mut items = Vec::new();
        for element in elements {
            let (ty, local) = self.lower_element(element, frame)?;
            if let Some(prev) = elem_ty {
                self.unify_ty(ty, prev)
                    .map_err(|message| self.error(element.span, message))?;
            } else {
                elem_ty = Some(ty);
            }
            items.push(local);
        }
        let elem_ty = elem_ty.unwrap_or_else(|| self.ty_var());
        let id = self.fresh_local();
        frame.lets.push(Let {
            binds: vec![id],
            value: Value::Array(items),
        });
        frame.stack.push(Operand::Local(id));
        let ty = self.ty_ctor("Array", vec![elem_ty]);
        Ok(self.stack_cons(stack, ty))
    }

    fn lower_record(
        &mut self,
        fields: &[(String, Term)],
        stack: StackId,
        frame: &mut Frame,
    ) -> Result<StackId, Diagnostic> {
        let mut pairs = Vec::new();
        let mut locals = Vec::new();
        for (name, term) in fields {
            let (ty, local) = self.lower_element(term, frame)?;
            pairs.push((name.clone(), ty));
            locals.push((name.clone(), local));
        }
        let id = self.fresh_local();
        frame.lets.push(Let {
            binds: vec![id],
            value: Value::Record { fields: locals },
        });
        frame.stack.push(Operand::Local(id));
        let row = self.row_fields(pairs, None);
        let ty = self.ty_record(row);
        Ok(self.stack_cons(stack, ty))
    }

    /// An element does not read the surrounding stack. Its lets join the caller.
    fn lower_element(
        &mut self,
        term: &Term,
        frame: &mut Frame,
    ) -> Result<(TyId, LocalId), Diagnostic> {
        let mut inner = Frame {
            stack: Vec::new(),
            params: Vec::new(),
            flexible: false,
            lets: Vec::new(),
            env: frame.env.clone(),
        };
        let empty = self.stack_empty();
        let output = self.lower_term(term, empty, &mut inner)?;
        let output = self.find_stack(output);
        let ty = match self.stacks[output.0 as usize].clone() {
            StackNode::Cons(rest, ty) if self.cons_len(rest) == 0 && inner.stack.len() == 1 => ty,
            _ => return Err(self.error(term.span, Error::ExpectedElement)),
        };
        let operand = inner.stack.pop().expect("element");
        let local = inner.force(self, operand)?;
        frame.lets.extend(inner.lets);
        Ok((ty, local))
    }

    /// Lower a quotation after its input has been ascribed.
    ///
    /// The body runs on a private copy of that spine, the same way the checker
    /// does, so two quotations that share an input do not extend one stack.
    /// Captures replace the top parameters. A body that never pops the value
    /// keeps it under the outputs.
    fn realize(&mut self, pending: PendingForm) -> Result<QuoteForm, Diagnostic> {
        let private_in = self.copy_spine(pending.input);
        let types = self.cons_tys(private_in);
        let mut params = Vec::with_capacity(types.len());
        let mut stack = Vec::with_capacity(types.len());
        for ty in types {
            let id = self.fresh_local();
            self.note_fn(id, ty);
            params.push(id);
            stack.push(Operand::Local(id));
        }
        // A captured quotation's arity lives on its local. The type's own
        // prefix stops at the link the body wrote, so it often reads as (0, 0).
        for (offset, capture) in pending.captures.iter().enumerate() {
            if offset >= params.len() {
                break;
            }
            let param = params[params.len() - 1 - offset];
            if let Some(arity) = self.fn_arity.get(capture).copied() {
                self.fn_arity.insert(param, arity);
            }
        }
        let saved_ambient = self.ambient.replace(pending.effect);
        let saved_locals = std::mem::replace(&mut self.locals, pending.locals);
        let mut env = pending.env;
        env.push(HashMap::new());
        let mut frame = Frame {
            stack,
            params,
            flexible: self.tail_is_open(private_in),
            lets: Vec::new(),
            env,
        };
        self.locals.push(HashMap::new());
        let lowered = self.lower_terms(&pending.terms, private_in, &mut frame);
        self.locals.pop();
        self.locals = saved_locals;
        self.ambient = saved_ambient;
        let output = lowered?;
        self.unify_stack(private_in, pending.input)
            .map_err(|message| self.error(pending.span, message))?;
        self.unify_stack(output, pending.output)
            .map_err(|message| self.error(pending.span, message))?;
        let result = drain_results(self, &mut frame)?;
        let body = Expr {
            lets: frame.lets,
            result,
        };
        Ok(self.substitute_captures(pending.captures, frame.params, body))
    }

    fn substitute_captures(
        &mut self,
        captures: Vec<LocalId>,
        mut params: Vec<LocalId>,
        body: Expr,
    ) -> QuoteForm {
        let mut map = HashMap::new();
        let mut spilled = Vec::new();
        for capture in captures {
            match params.pop() {
                Some(param) => {
                    map.insert(param, capture);
                }
                None => spilled.push(capture),
            }
        }
        let mut body = self.freshen_expr(&body, &mut map);
        for capture in spilled {
            body.result.insert(0, capture);
        }
        QuoteForm { params, body }
    }

    /// A variable tail can still grow. An empty or rigid tail cannot.
    fn tail_is_open(&self, stack: StackId) -> bool {
        let mut current = self.find_stack(stack);
        loop {
            match &self.stacks[current.0 as usize] {
                StackNode::Cons(rest, _) => current = self.find_stack(*rest),
                StackNode::Var | StackNode::Template => return true,
                _ => return false,
            }
        }
    }

    fn realize_operand(&mut self, operand: Operand) -> Result<Operand, Diagnostic> {
        match operand {
            Operand::Pending(pending) => Ok(Operand::Quote(self.realize(pending)?)),
            other => Ok(other),
        }
    }

    /// Quotation header `x y ->` binds the deepest value to `x` and the top to `y`.
    fn lower_bind(
        &mut self,
        span: Span,
        names: &[String],
        stack: StackId,
        frame: &mut Frame,
    ) -> Result<StackId, Diagnostic> {
        let slot = self.locals.len() - 1;
        for name in names {
            if self.locals[slot].contains_key(name) {
                return Err(self.error(span, Error::DuplicateDefinition(name.to_string())));
            }
        }
        let mut current = stack;
        let mut bound = Vec::with_capacity(names.len());
        for name in names.iter().rev() {
            let (rest, ty) = self.pop_slot(current, span)?;
            current = rest;
            let operand = frame.pop_one(self);
            let id = frame.force(self, operand)?;
            if let Some(arity) = self.bind_arity.get(&(span, name.clone())) {
                self.fn_arity.insert(id, *arity);
            }
            let scheme = self.binding_scheme(ty);
            bound.push((name.clone(), id, scheme));
        }
        bound.reverse();
        {
            let env = frame.env.last_mut().expect("locals frame");
            for (name, id, _) in &bound {
                env.insert(name.clone(), *id);
            }
        }
        for (name, _, scheme) in bound {
            self.locals[slot].insert(name, scheme);
        }
        Ok(current)
    }

    fn lower_word(
        &mut self,
        span: Span,
        name: &str,
        stack: StackId,
        frame: &mut Frame,
    ) -> Result<StackId, Diagnostic> {
        if let Some(id) = frame.find_env(name) {
            let output = self.instantiate_word(span, name, stack)?.1;
            frame.stack.push(Operand::Local(id));
            return Ok(output);
        }
        if name == "call" {
            if let Some(output) = self.lower_unsolved_call(span, stack, frame)? {
                return Ok(output);
            }
        }
        let (input, output, constraints) = self.instantiate_use(span, name, stack)?;
        let lowering = builtin::word(name).map(|word| word.lowering);
        match lowering {
            Some(Lowering::Call) => {
                // The scheme has ascribed the function. A local produced by
                // `curry` already has that function type; record its arity
                // before `call` reads the local.
                self.note_call_arity(frame, input);
                if !self.arity_ready(input) && !self.frame_call_ready(frame) {
                    return Err(self.error(span, Error::QuotationArityUnknown));
                }
                let produced = self.lower_call(frame)?;
                // `call` returns `..t`, so the result types sit in that tail.
                // A result may itself be a quotation. A later `call` needs its arity.
                self.note_produced(output, produced, frame);
                return Ok(output);
            }
            Some(Lowering::Dip { under }) => {
                self.lower_dip(frame, under as usize)?;
                return Ok(output);
            }
            Some(Lowering::Keep { keep }) => {
                self.lower_keep(frame, keep as usize)?;
                return Ok(output);
            }
            Some(Lowering::Cleave { vals, quots }) => {
                self.lower_cleave(frame, vals as usize, quots as usize)?;
                return Ok(output);
            }
            Some(Lowering::Spread { n }) => {
                self.lower_spread(frame, n as usize)?;
                return Ok(output);
            }
            Some(Lowering::Each { n }) => {
                self.lower_each(frame, n as usize)?;
                return Ok(output);
            }
            Some(Lowering::If) => {
                self.lower_if(frame)?;
                return Ok(output);
            }
            Some(Lowering::Quote) => {
                self.lower_quote_word(frame)?;
                return Ok(output);
            }
            Some(Lowering::Cons) => {
                // The scheme has ascribed the quotation. A local may only
                // have that function type, the same way `call` records it.
                self.note_call_arity(frame, input);
                self.lower_cons(span, frame)?;
                return Ok(output);
            }
            Some(Lowering::Shuffle { take, put }) => {
                self.lower_shuffle(frame, take, put);
                return Ok(output);
            }
            Some(Lowering::Unit) | None => {}
        }
        let consumed = self.own_len(input);
        let produced = self.own_len(output);
        let args = frame.pop_n(self, consumed);
        let args = force_all(self, frame, args)?;
        if let Some(width) = self.ctor_width(name) {
            if width != args.len() {
                panic!(
                    "{name}: constructor has {width} fields, lowered with {}",
                    args.len()
                );
            }
            let id = self.fresh_local();
            frame.lets.push(Let {
                binds: vec![id],
                value: Value::Ctor {
                    name: name.to_string(),
                    fields: args,
                },
            });
            frame.stack.push(Operand::Local(id));
            return Ok(output);
        }
        let dicts = self.dicts_for_use(&constraints, span)?;
        if let Some(dicts) = dicts {
            if self.is_class_method(name) {
                self.emit_method(name, &dicts, args, produced, frame);
            } else {
                self.emit_word(name, &dicts, args, produced, frame);
            }
        } else {
            // The head is still flexible. The second pass emits the solved call.
            self.emit_call(name, args, produced, frame);
        }
        self.note_returned(output, frame);
        Ok(output)
    }

    fn instantiate_use(
        &mut self,
        span: Span,
        name: &str,
        stack: StackId,
    ) -> Result<(StackId, StackId, Vec<super::SemConstraint>), Diagnostic> {
        let Some(scheme) = self.lookup(name) else {
            return Err(self.error(span, Error::UndefinedWord(name.to_string())));
        };
        let (input, output, effect, constraints) = self.instantiate_scheme(&scheme, false);
        self.unify_stack(stack, input)
            .map_err(|message| self.error(span, message))?;
        self.join_effect(effect, span)?;
        Ok((input, output, constraints))
    }

    /// A method is a field of its dictionary. A known instance calls the word.
    fn emit_method(
        &mut self,
        name: &str,
        dicts: &[Dict],
        stack_args: Vec<LocalId>,
        produced: usize,
        frame: &mut Frame,
    ) {
        let Some((dict, extras)) = dicts.split_first() else {
            self.emit_word(name, dicts, stack_args, produced, frame);
            return;
        };
        match dict {
            Dict::Assumption(index) => {
                let base = self.assumption_locals[*index];
                let mut args = self.materialize_dicts(extras, frame);
                args.extend(stack_args);
                self.emit_prop_apply(base, name, args, produced, frame);
            }
            Dict::Super { .. } => {
                let base = self.materialize_dict(dict, frame);
                let mut args = self.materialize_dicts(extras, frame);
                args.extend(stack_args);
                self.emit_prop_apply(base, name, args, produced, frame);
            }
            Dict::Instance { methods, .. } => {
                let method = methods
                    .iter()
                    .find(|method| method.name == name)
                    .expect("class dictionary contains the method");
                let mut args = self.materialize_dicts(&method.dicts, frame);
                args.extend(self.materialize_dicts(extras, frame));
                args.extend(stack_args);
                self.emit_call(&method.word, args, produced, frame);
            }
        }
    }

    fn materialize_dicts(&mut self, dicts: &[Dict], frame: &mut Frame) -> Vec<LocalId> {
        dicts
            .iter()
            .map(|dict| self.materialize_dict(dict, frame))
            .collect()
    }

    fn emit_word(
        &mut self,
        name: &str,
        dicts: &[Dict],
        stack_args: Vec<LocalId>,
        produced: usize,
        frame: &mut Frame,
    ) {
        let mut args = Vec::new();
        for dict in dicts {
            args.push(self.materialize_dict(dict, frame));
        }
        args.extend(stack_args);
        self.emit_call(name, args, produced, frame);
    }

    fn emit_call(&mut self, word: &str, args: Vec<LocalId>, produced: usize, frame: &mut Frame) {
        let binds = self.fresh_vec(produced);
        frame.lets.push(Let {
            binds: binds.clone(),
            value: Value::Call {
                word: word.to_string(),
                args,
            },
        });
        frame.stack.extend(binds.into_iter().map(Operand::Local));
    }

    fn emit_prop_apply(
        &mut self,
        base: LocalId,
        name: &str,
        args: Vec<LocalId>,
        produced: usize,
        frame: &mut Frame,
    ) {
        let fun = self.fresh_local();
        frame.lets.push(Let {
            binds: vec![fun],
            value: Value::Prop {
                base,
                name: name.to_string(),
            },
        });
        let binds = self.fresh_vec(produced);
        frame.lets.push(Let {
            binds: binds.clone(),
            value: Value::Apply { fun, args },
        });
        frame.stack.extend(binds.into_iter().map(Operand::Local));
    }

    fn materialize_dict(&mut self, dict: &Dict, frame: &mut Frame) -> LocalId {
        match dict {
            Dict::Assumption(index) => self.assumption_locals[*index],
            Dict::Super { base, field } => {
                let base = self.materialize_dict(base, frame);
                let id = self.fresh_local();
                frame.lets.push(Let {
                    binds: vec![id],
                    value: Value::Prop {
                        base,
                        name: field.clone(),
                    },
                });
                id
            }
            Dict::Instance { methods, supers } => {
                let mut fields = Vec::new();
                for method in methods {
                    let value = if method.dicts.is_empty() {
                        DictValue::Word(method.word.clone())
                    } else {
                        let mut dicts = Vec::new();
                        for nested in &method.dicts {
                            dicts.push(self.materialize_dict(nested, frame));
                        }
                        DictValue::Closure {
                            params: self.fresh_vec(method.extra + method.arity),
                            dicts,
                            word: method.word.clone(),
                        }
                    };
                    fields.push(DictField {
                        name: method.name.clone(),
                        value,
                    });
                }
                for (name, nested) in supers {
                    fields.push(DictField {
                        name: name.clone(),
                        value: DictValue::Local(self.materialize_dict(nested, frame)),
                    });
                }
                let id = self.fresh_local();
                frame.lets.push(Let {
                    binds: vec![id],
                    value: Value::Dict { fields },
                });
                id
            }
        }
    }

    fn instantiate_word(
        &mut self,
        span: Span,
        name: &str,
        stack: StackId,
    ) -> Result<(StackId, StackId), Diagnostic> {
        let Some(scheme) = self.lookup(name) else {
            return Err(self.error(span, Error::UndefinedWord(name.to_string())));
        };
        let (input, output, effect, _) = self.instantiate_scheme(&scheme, false);
        self.unify_stack(stack, input)
            .map_err(|message| self.error(span, message))?;
        self.join_effect(effect, span)?;
        Ok((input, output))
    }

    fn lower_shuffle(&mut self, frame: &mut Frame, take: u8, put: &[u8]) {
        let items = frame.pop_n(self, take as usize);
        for index in put {
            frame.stack.push(items[*index as usize].clone());
        }
    }

    fn lower_quote_word(&mut self, frame: &mut Frame) -> Result<(), Diagnostic> {
        let value = frame.pop_one(self);
        let id = frame.force(self, value)?;
        frame.stack.push(Operand::Quote(QuoteForm {
            params: Vec::new(),
            body: Expr {
                lets: Vec::new(),
                result: vec![id],
            },
        }));
        Ok(())
    }

    /// `X [P] cons` lowers to `[X P]`. Parameters are bottom to top, and a
    /// deeper underflow is spliced in at the bottom, so the last parameter is
    /// the top input. That parameter is `X`. When `P` never pops, `X` stays
    /// under what `P` pushes and is inserted at the front of the result.
    fn lower_cons(&mut self, span: Span, frame: &mut Frame) -> Result<(), Diagnostic> {
        let quote = frame.pop_one(self);
        let value = frame.pop_one(self);
        let value = frame.force(self, value)?;
        let built = match quote {
            Operand::Pending(mut pending) => {
                pending.captures.push(value);
                Operand::Pending(pending)
            }
            Operand::Quote(form) => self.cons_quote(form, value),
            Operand::Local(fun) => self.cons_fun(span, fun, value)?,
        };
        frame.stack.push(built);
        Ok(())
    }

    fn cons_quote(&mut self, mut form: QuoteForm, value: LocalId) -> Operand {
        let mut map = HashMap::new();
        if let Some(top) = form.params.pop() {
            map.insert(top, value);
            let body = self.freshen_expr(&form.body, &mut map);
            return Operand::Quote(QuoteForm {
                params: form.params,
                body,
            });
        }
        let mut body = self.freshen_expr(&form.body, &mut map);
        body.result.insert(0, value);
        Operand::Quote(QuoteForm {
            params: form.params,
            body,
        })
    }

    fn cons_fun(
        &mut self,
        span: Span,
        fun: LocalId,
        value: LocalId,
    ) -> Result<Operand, Diagnostic> {
        let Some((arity_in, arity_out)) = self.fn_arity.get(&fun).copied() else {
            return Err(self.error(span, Error::QuotationArityUnknown));
        };
        if arity_in == 0 {
            let binds = self.fresh_vec(arity_out);
            let mut result = vec![value];
            result.extend(binds.iter().copied());
            return Ok(Operand::Quote(QuoteForm {
                params: Vec::new(),
                body: Expr {
                    lets: vec![Let {
                        binds,
                        value: Value::Apply {
                            fun,
                            args: Vec::new(),
                        },
                    }],
                    result,
                },
            }));
        }
        let params = self.fresh_vec(arity_in - 1);
        let mut args = params.clone();
        args.push(value);
        let binds = self.fresh_vec(arity_out);
        Ok(Operand::Quote(QuoteForm {
            params,
            body: Expr {
                lets: vec![Let {
                    binds: binds.clone(),
                    value: Value::Apply { fun, args },
                }],
                result: binds,
            },
        }))
    }

    fn lower_call(&mut self, frame: &mut Frame) -> Result<usize, Diagnostic> {
        let quote = frame.pop_one(self);
        let quote = self.realize_operand(quote)?;
        let (arity, produced) = self.quote_arity(&quote);
        let base = take_base(self, frame, arity)?;
        let expr = self.reduce_quote(quote, base);
        frame.lets.extend(expr.lets);
        frame.stack = expr.result.into_iter().map(Operand::Local).collect();
        Ok(produced)
    }

    fn lower_dip(&mut self, frame: &mut Frame, under: usize) -> Result<(), Diagnostic> {
        let quote = frame.pop_one(self);
        let quote = self.realize_operand(quote)?;
        let hidden = frame.pop_n(self, under);
        let (arity, _) = self.quote_arity(&quote);
        let base = take_base(self, frame, arity)?;
        let expr = self.reduce_quote(quote, base);
        frame.lets.extend(expr.lets);
        frame.stack = expr.result.into_iter().map(Operand::Local).collect();
        frame.stack.extend(hidden);
        Ok(())
    }

    fn lower_keep(&mut self, frame: &mut Frame, keep: usize) -> Result<(), Diagnostic> {
        let quote = frame.pop_one(self);
        let quote = self.realize_operand(quote)?;
        let saved = frame.pop_n(self, keep);
        let saved = force_all(self, frame, saved)?;
        frame.stack.extend(saved.iter().copied().map(Operand::Local));
        let (arity, _) = self.quote_arity(&quote);
        let base = take_base(self, frame, arity)?;
        let expr = self.reduce_quote(quote, base);
        frame.lets.extend(expr.lets);
        frame.stack = expr.result.into_iter().map(Operand::Local).collect();
        frame.stack.extend(saved.into_iter().map(Operand::Local));
        Ok(())
    }

    /// Every quotation sees the original tail. Only its produced suffix is kept.
    fn lower_cleave(
        &mut self,
        frame: &mut Frame,
        vals: usize,
        quots: usize,
    ) -> Result<(), Diagnostic> {
        let quotes = frame.pop_n(self, quots);
        let values = frame.pop_n(self, vals);
        let values = force_all(self, frame, values)?;
        let below = drain_results(self, frame)?;
        let mut outputs = Vec::new();
        for quote in quotes {
            let quote = self.realize_operand(quote)?;
            let mut base = below.clone();
            base.extend(values.iter().copied());
            outputs.extend(self.produce(frame, quote, base)?);
        }
        restore(frame, below, outputs);
        Ok(())
    }

    fn lower_spread(&mut self, frame: &mut Frame, n: usize) -> Result<(), Diagnostic> {
        let quotes = frame.pop_n(self, n);
        let values = frame.pop_n(self, n);
        let values = force_all(self, frame, values)?;
        let below = drain_results(self, frame)?;
        let mut outputs = Vec::new();
        for (value, quote) in values.into_iter().zip(quotes) {
            let quote = self.realize_operand(quote)?;
            let mut base = below.clone();
            base.push(value);
            outputs.extend(self.produce(frame, quote, base)?);
        }
        restore(frame, below, outputs);
        Ok(())
    }

    fn lower_each(&mut self, frame: &mut Frame, n: usize) -> Result<(), Diagnostic> {
        let quote = frame.pop_one(self);
        let quote = self.realize_operand(quote)?;
        let values = frame.pop_n(self, n);
        let values = force_all(self, frame, values)?;
        let below = drain_results(self, frame)?;
        let mut outputs = Vec::new();
        for value in values {
            let mut base = below.clone();
            base.push(value);
            outputs.extend(self.produce(frame, quote.clone(), base)?);
        }
        restore(frame, below, outputs);
        Ok(())
    }

    /// `reduce_quote` returns the untouched tail plus the quotation's outputs.
    /// The suffix is the outputs alone.
    fn produce(
        &mut self,
        frame: &mut Frame,
        quote: Operand,
        base: Vec<LocalId>,
    ) -> Result<Vec<LocalId>, Diagnostic> {
        let (_, produced) = self.quote_arity(&quote);
        let expr = self.reduce_quote(quote, base);
        frame.lets.extend(expr.lets);
        if produced > expr.result.len() {
            panic!(
                "quotation produced {produced}, the result has {}",
                expr.result.len()
            );
        }
        let start = expr.result.len() - produced;
        Ok(expr.result[start..].to_vec())
    }

    fn lower_if(&mut self, frame: &mut Frame) -> Result<(), Diagnostic> {
        let else_quote = frame.pop_one(self);
        let then_quote = frame.pop_one(self);
        let cond = frame.pop_one(self);
        let then_quote = self.realize_operand(then_quote)?;
        let else_quote = self.realize_operand(else_quote)?;
        let cond = frame.force(self, cond)?;
        let arity = self
            .quote_arity(&then_quote)
            .0
            .max(self.quote_arity(&else_quote).0);
        let base = take_base(self, frame, arity)?;
        let yes = self.reduce_quote(then_quote, base.clone());
        let no = self.reduce_quote(else_quote, base);
        if yes.result.len() != no.result.len() {
            panic!(
                "if branches produced {} and {} values",
                yes.result.len(),
                no.result.len()
            );
        }
        let binds = self.fresh_vec(yes.result.len());
        frame.lets.push(Let {
            binds: binds.clone(),
            value: Value::If { cond, yes, no },
        });
        frame.stack = binds.into_iter().map(Operand::Local).collect();
        Ok(())
    }

    fn lower_clauses(
        &mut self,
        def: &crate::ast::Def,
        input: StackId,
        frame: &mut Frame,
    ) -> Result<StackId, Diagnostic> {
        let prepared = self.prepare_clauses(&def.clauses, input, def.name_span)?;
        let operands = frame.pop_n(self, prepared.slots.len());
        let mut roots = Vec::with_capacity(operands.len());
        for operand in operands {
            roots.push(frame.force(self, operand)?);
        }
        let below = frame.stack.clone();
        let mut outputs = Vec::new();
        let mut choices = Vec::new();
        for clause in &def.clauses {
            let mut conds = Vec::new();
            let mut binds = HashMap::new();
            let mut schemes = HashMap::new();
            let mut arm_stack = prepared.rest;
            let mut arm = Frame {
                stack: below.clone(),
                params: Vec::new(),
                flexible: false,
                lets: Vec::new(),
                env: frame.env.clone(),
            };
            for (pattern, (ty, root)) in clause
                .patterns
                .iter()
                .zip(prepared.slots.iter().copied().zip(&roots))
            {
                self.lower_pattern(
                    pattern,
                    ty,
                    *root,
                    &[],
                    &mut conds,
                    &mut binds,
                    &mut schemes,
                    &mut arm_stack,
                    &mut arm,
                    clause.span,
                )?;
            }
            arm.env.push(binds);
            self.locals.push(schemes);
            let checked = self.lower_terms(&clause.body, arm_stack, &mut arm);
            self.locals.pop();
            outputs.push(checked?);
            let result = drain_results(self, &mut arm)?;
            choices.push(Choice {
                conds,
                body: Expr {
                    lets: arm.lets,
                    result,
                },
            });
        }
        let mut acc = outputs[0];
        for later in outputs.into_iter().skip(1) {
            self.unify_stack(later, acc)
                .map_err(|message| self.error(def.name_span, message))?;
            acc = self.find_stack(acc);
        }
        let width = choices
            .first()
            .map(|choice| choice.body.result.len())
            .unwrap_or(0);
        for choice in &choices {
            if choice.body.result.len() != width {
                panic!(
                    "{}: clauses produced {} and {width} values",
                    def.name,
                    choice.body.result.len()
                );
            }
        }
        let binds = self.fresh_vec(width);
        frame.lets.push(Let {
            binds: binds.clone(),
            value: Value::Choose { arms: choices },
        });
        frame.stack = binds.into_iter().map(Operand::Local).collect();
        Ok(acc)
    }

    fn lower_pattern(
        &mut self,
        pattern: &Pattern,
        ty: TyId,
        root: LocalId,
        steps: &[Access],
        conds: &mut Vec<Cond>,
        binds: &mut HashMap<String, LocalId>,
        schemes: &mut HashMap<String, super::SemScheme>,
        arm_stack: &mut StackId,
        arm: &mut Frame,
        span: Span,
    ) -> Result<(), Diagnostic> {
        match pattern {
            Pattern::Wild => Ok(()),
            Pattern::Bind(name) => {
                if binds.contains_key(name) {
                    return Err(self.error(span, Error::DuplicateDefinition(name.to_string())));
                }
                let id = self.materialize(arm, root, steps);
                self.note_fn(id, ty);
                binds.insert(name.clone(), id);
                schemes.insert(name.clone(), self.binding_scheme(ty));
                Ok(())
            }
            Pattern::Lit(lit) => {
                conds.push(Cond {
                    root,
                    steps: steps.to_vec(),
                    test: pat_test(lit),
                });
                Ok(())
            }
            Pattern::Unpack(name) => {
                let shape = self.ctor_shape(name, ty, span)?;
                let newtype = shape.newtype;
                if !newtype {
                    conds.push(Cond {
                        root,
                        steps: steps.to_vec(),
                        test: PatTest::Tag(name.clone()),
                    });
                }
                let width = shape.fields.len();
                let names = shape.names;
                for (index, field) in shape.fields.into_iter().enumerate() {
                    let id = if newtype {
                        self.materialize(arm, root, steps)
                    } else {
                        let mut child = steps.to_vec();
                        child.push(Self::field_step(index, width, names.get(index)));
                        self.materialize(arm, root, &child)
                    };
                    self.note_fn(id, field);
                    *arm_stack = self.stack_cons(*arm_stack, field);
                    arm.stack.push(Operand::Local(id));
                }
                Ok(())
            }
            Pattern::Ctor(name, patterns) => {
                let shape = self.ctor_shape(name, ty, span)?;
                let newtype = shape.newtype;
                if patterns.len() != shape.fields.len() {
                    let count = shape.fields.len();
                    return Err(self.error(
                        span,
                        Error::PatternArity { name: name.to_string(), count },
                    ));
                }
                if !newtype {
                    conds.push(Cond {
                        root,
                        steps: steps.to_vec(),
                        test: PatTest::Tag(name.clone()),
                    });
                }
                let width = shape.fields.len();
                let names = shape.names;
                for (index, field) in shape.fields.into_iter().enumerate() {
                    let mut child = steps.to_vec();
                    if !newtype {
                        child.push(Self::field_step(index, width, names.get(index)));
                    }
                    self.lower_pattern(
                        &patterns[index],
                        field,
                        root,
                        &child,
                        conds,
                        binds,
                        schemes,
                        arm_stack,
                        arm,
                        span,
                    )?;
                }
                Ok(())
            }
            Pattern::Array(patterns) => {
                let elem = self.array_elem(ty, span)?;
                conds.push(Cond {
                    root,
                    steps: steps.to_vec(),
                    test: PatTest::Length(patterns.len()),
                });
                for (index, pattern) in patterns.iter().enumerate() {
                    let mut child = steps.to_vec();
                    child.push(Access::Index(index));
                    self.lower_pattern(
                        pattern,
                        elem,
                        root,
                        &child,
                        conds,
                        binds,
                        schemes,
                        arm_stack,
                        arm,
                        span,
                    )?;
                }
                Ok(())
            }
            Pattern::Record(fields) => {
                let types = self.open_record_pattern(fields, ty, span)?;
                for ((name, pattern), field_ty) in fields.iter().zip(types) {
                    let mut child = steps.to_vec();
                    child.push(Access::Prop(name.clone()));
                    self.lower_pattern(
                        pattern,
                        field_ty,
                        root,
                        &child,
                        conds,
                        binds,
                        schemes,
                        arm_stack,
                        arm,
                        span,
                    )?;
                }
                Ok(())
            }
        }
    }

    /// A named field is `obj.name`. A positional field is `obj.value` or `obj.valueN`.
    fn field_step(index: usize, width: usize, name: Option<&Option<String>>) -> Access {
        match name.cloned().flatten() {
            Some(name) => Access::Prop(name),
            None => Access::Field { index, width },
        }
    }

    /// Follow constructor fields and array indexes. An empty path is the root local itself.
    fn materialize(&mut self, frame: &mut Frame, root: LocalId, steps: &[Access]) -> LocalId {
        if steps.is_empty() {
            return root;
        }
        let mut base = root;
        for step in steps {
            let id = self.fresh_local();
            let value = match step {
                Access::Field { index, width } => Value::Field {
                    base,
                    index: *index,
                    width: *width,
                },
                Access::Index(index) => Value::Index {
                    base,
                    index: *index,
                },
                Access::Prop(name) => Value::Prop {
                    base,
                    name: name.clone(),
                },
            };
            frame.lets.push(Let {
                binds: vec![id],
                value,
            });
            base = id;
        }
        base
    }

    fn note_fn(&mut self, id: LocalId, ty: TyId) {
        if let Some(arity) = self.fun_arity(ty) {
            self.fn_arity.insert(id, arity);
        }
    }

    fn frame_call_ready(&self, frame: &Frame) -> bool {
        match frame.stack.last() {
            Some(Operand::Quote(_)) | Some(Operand::Pending(_)) => true,
            Some(Operand::Local(id)) => self.fn_arity.contains_key(id),
            _ => false,
        }
    }

    fn quote_arity(&self, quote: &Operand) -> (usize, usize) {
        match quote {
            Operand::Quote(quote) => (quote.params.len(), quote.body.result.len()),
            Operand::Local(id) => self
                .fn_arity
                .get(id)
                .copied()
                .unwrap_or_else(|| panic!("function local v{id} has no recorded arity")),
            Operand::Pending(_) => panic!("quotation was not realized"),
        }
    }

    fn reduce_quote(&mut self, quote: Operand, base: Vec<LocalId>) -> Expr {
        match quote {
            Operand::Quote(quote) => {
                let arity = quote.params.len();
                if base.len() < arity {
                    panic!(
                        "quotation needs {arity} values, the caller has {}",
                        base.len()
                    );
                }
                let split = base.len() - arity;
                let mut map = HashMap::new();
                for (param, arg) in quote.params.iter().zip(&base[split..]) {
                    map.insert(*param, *arg);
                }
                let body = self.freshen_expr(&quote.body, &mut map);
                Expr {
                    lets: body.lets,
                    result: base[..split].iter().copied().chain(body.result).collect(),
                }
            }
            Operand::Local(fun) => {
                let (arity, produced) = self.fn_arity[&fun];
                if base.len() < arity {
                    panic!("call needs {arity} values, the caller has {}", base.len());
                }
                let split = base.len() - arity;
                let binds = self.fresh_vec(produced);
                Expr {
                    lets: vec![Let {
                        binds: binds.clone(),
                        value: Value::Apply {
                            fun,
                            args: base[split..].to_vec(),
                        },
                    }],
                    result: base[..split].iter().copied().chain(binds).collect(),
                }
            }
            Operand::Pending(_) => panic!("quotation was not realized"),
        }
    }

    fn freshen_expr(&mut self, expr: &Expr, map: &mut HashMap<LocalId, LocalId>) -> Expr {
        let mut lets = Vec::new();
        for let_ in &expr.lets {
            let binds = self.fresh_vec(let_.binds.len());
            for (old, new) in let_.binds.iter().zip(&binds) {
                map.insert(*old, *new);
                if let Some(arity) = self.fn_arity.get(old).copied() {
                    self.fn_arity.insert(*new, arity);
                }
            }
            lets.push(Let {
                binds,
                value: self.freshen_value(&let_.value, map),
            });
        }
        Expr {
            lets,
            result: expr
                .result
                .iter()
                .map(|id| map.get(id).copied().unwrap_or(*id))
                .collect(),
        }
    }

    fn freshen_value(&mut self, value: &Value, map: &mut HashMap<LocalId, LocalId>) -> Value {
        let local = |id: &LocalId| map.get(id).copied().unwrap_or(*id);
        match value {
            Value::Lit(lit) => Value::Lit(lit.clone()),
            Value::Quote(quote) => {
                let mut inner = map.clone();
                let mut params = Vec::new();
                for old in &quote.params {
                    let new = self.fresh_local();
                    inner.insert(*old, new);
                    if let Some(arity) = self.fn_arity.get(old).copied() {
                        self.fn_arity.insert(new, arity);
                    }
                    params.push(new);
                }
                Value::Quote(QuoteForm {
                    params,
                    body: self.freshen_expr(&quote.body, &mut inner),
                })
            }
            Value::Ctor { name, fields } => Value::Ctor {
                name: name.clone(),
                fields: fields.iter().map(local).collect(),
            },
            Value::Call { word, args } => Value::Call {
                word: word.clone(),
                args: args.iter().map(local).collect(),
            },
            Value::Apply { fun, args } => Value::Apply {
                fun: local(fun),
                args: args.iter().map(local).collect(),
            },
            Value::Field { base, index, width } => Value::Field {
                base: local(base),
                index: *index,
                width: *width,
            },
            Value::Index { base, index } => Value::Index {
                base: local(base),
                index: *index,
            },
            Value::Prop { base, name } => Value::Prop {
                base: local(base),
                name: name.clone(),
            },
            Value::SetProp { base, name, value } => Value::SetProp {
                base: local(base),
                name: name.clone(),
                value: local(value),
            },
            Value::Array(items) => Value::Array(items.iter().map(local).collect()),
            Value::Record { fields } => Value::Record {
                fields: fields
                    .iter()
                    .map(|(name, id)| (name.clone(), local(id)))
                    .collect(),
            },
            Value::Dict { fields } => Value::Dict {
                fields: fields
                    .iter()
                    .map(|field| DictField {
                        name: field.name.clone(),
                        value: match &field.value {
                            DictValue::Word(word) => DictValue::Word(word.clone()),
                            DictValue::Local(id) => DictValue::Local(local(id)),
                            DictValue::Closure { params, dicts, word } => DictValue::Closure {
                                params: params.iter().map(|_| self.fresh_local()).collect(),
                                dicts: dicts.iter().map(local).collect(),
                                word: word.clone(),
                            },
                        },
                    })
                    .collect(),
            },
            Value::If { cond, yes, no } => Value::If {
                cond: local(cond),
                yes: self.freshen_expr(yes, map),
                no: self.freshen_expr(no, map),
            },
            Value::Choose { arms } => {
                let mut choices = Vec::with_capacity(arms.len());
                for arm in arms {
                    let conds = arm
                        .conds
                        .iter()
                        .map(|cond| Cond {
                            root: map.get(&cond.root).copied().unwrap_or(cond.root),
                            steps: cond.steps.clone(),
                            test: cond.test.clone(),
                        })
                        .collect();
                    choices.push(Choice {
                        conds,
                        body: self.freshen_expr(&arm.body, map),
                    });
                }
                Value::Choose { arms: choices }
            }
        }
    }

    fn ctor_width(&self, name: &str) -> Option<usize> {
        self.datas.values().find_map(|data| {
            data.ctors
                .iter()
                .find(|ctor| ctor.name == name)
                .map(|ctor| ctor.fields.len())
        })
    }

    fn fresh_local(&mut self) -> LocalId {
        let id = self.next_local;
        self.next_local += 1;
        id
    }

    fn fresh_vec(&mut self, count: usize) -> Vec<LocalId> {
        (0..count).map(|_| self.fresh_local()).collect()
    }

    /// Cons cells of a stack, bottom to top, following links.
    fn cons_tys(&self, stack: StackId) -> Vec<TyId> {
        let mut items = Vec::new();
        let mut current = self.find_stack(stack);
        loop {
            match &self.stacks[current.0 as usize] {
                StackNode::Cons(rest, ty) => {
                    items.push(*ty);
                    current = self.find_stack(*rest);
                }
                _ => {
                    items.reverse();
                    return items;
                }
            }
        }
    }

    pub(crate) fn concrete_input_len(&mut self, name: &str) -> Option<usize> {
        let scheme = self.words.get(name)?.clone();
        let (input, _, _, constraints) = self.instantiate_scheme(&scheme, true);
        Some(self.cons_len(input) + constraints.len())
    }

    fn cons_len(&self, stack: StackId) -> usize {
        self.cons_tys(stack).len()
    }

    /// Prefix length that stops at the tail node instead of following its link.
    fn own_len(&self, mut current: StackId) -> usize {
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

    /// Types of the prefix [`own_len`] counts, bottom to top.
    fn own_tys(&self, mut current: StackId) -> Vec<TyId> {
        let mut items = Vec::new();
        loop {
            match &self.stacks[current.0 as usize] {
                StackNode::Cons(rest, ty) => {
                    items.push(*ty);
                    current = *rest;
                }
                _ => {
                    items.reverse();
                    return items;
                }
            }
        }
    }

    /// The top `produced` locals are `call`'s results. `output` is the whole
    /// stack after the call, including values the quotation left in place.
    fn note_produced(&mut self, output: StackId, produced: usize, frame: &Frame) {
        if produced == 0 || frame.stack.len() < produced {
            return;
        }
        let types = self.cons_tys(output);
        if types.len() < produced {
            return;
        }
        let type_start = types.len() - produced;
        let stack_start = frame.stack.len() - produced;
        for (operand, ty) in frame.stack[stack_start..].iter().zip(&types[type_start..]) {
            let Operand::Local(id) = operand else {
                continue;
            };
            let Some(arity) = self.fun_arity(*ty) else {
                continue;
            };
            self.fn_arity.insert(*id, arity);
        }
    }

    /// A word result is a fresh local. Remember when that local is a quotation,
    /// so a later `call` can apply it. Inline quotes already record this in `force`.
    fn note_returned(&mut self, output: StackId, frame: &Frame) {
        let types = self.own_tys(output);
        if types.len() > frame.stack.len() {
            return;
        }
        let start = frame.stack.len() - types.len();
        for (operand, ty) in frame.stack[start..].iter().zip(types) {
            let Operand::Local(id) = operand else {
                continue;
            };
            let Some(arity) = self.fun_arity(ty) else {
                continue;
            };
            self.fn_arity.insert(*id, arity);
        }
    }

    /// The function's own prefix. A tail linked on later is not part of the arity:
    /// [`prefix_len`] stops at that link instead of following it.
    /// Two distinct naked tails are not an arity: the prefix has not been written.
    /// One shared tail, or an empty tail, is arity `(0, 0)`.
    pub(crate) fn fun_arity(&self, ty: TyId) -> Option<(usize, usize)> {
        let ty = self.expose(ty);
        let TyNode::Fun(input, output, _) = &self.tys[ty.0 as usize] else {
            return None;
        };
        let input_n = self.prefix_len(*input);
        let output_n = self.prefix_len(*output);
        if input_n == 0 && output_n == 0 && self.open_tail(*input) && self.open_tail(*output) {
            let inn = self.find_stack(*input);
            let out = self.find_stack(*output);
            if inn != out {
                return None;
            }
        }
        Some((input_n, output_n))
    }

    fn open_tail(&self, stack: StackId) -> bool {
        matches!(
            self.stacks[self.find_stack(stack).0 as usize],
            StackNode::Var | StackNode::Template
        )
    }

    /// `call` of a local whose function type is fixed by an enclosing quotation.
    /// The fresh lowering has not ascribed that type yet. The check pass stored
    /// the arity. Shape the stack from it instead of from `(..s -- ..t)`.
    fn lower_unsolved_call(
        &mut self,
        span: Span,
        stack: StackId,
        frame: &mut Frame,
    ) -> Result<Option<StackId>, Diagnostic> {
        let Some(Operand::Local(id)) = frame.stack.last().cloned() else {
            return Ok(None);
        };
        let Some(&(arity_in, arity_out)) = self.fn_arity.get(&id) else {
            return Ok(None);
        };
        // A function type that already has a prefix must go through `call`'s
        // scheme. Replacing it with fresh results would drop a nested
        // function, and the next `call` would have no arity.
        // Element types may still be variables. The prefix is what matters.
        if self.known_fun_on(stack) {
            return Ok(None);
        }
        let output = self.stack_for_arity(stack, arity_in, arity_out, span)?;
        let produced = self.lower_call(frame)?;
        if produced != arity_out {
            panic!("call produces {arity_out}, lowered {produced}");
        }
        self.note_produced(output, produced, frame);
        Ok(Some(output))
    }

    fn known_fun_on(&self, stack: StackId) -> bool {
        let id = self.find_stack(stack);
        let StackNode::Cons(_, ty) = self.stacks[id.0 as usize].clone() else {
            return false;
        };
        self.fun_arity(ty).is_some()
    }

    /// The operand is a local and the scheme gave it a function type.
    fn note_call_arity(&mut self, frame: &Frame, input: StackId) {
        let Some(Operand::Local(id)) = frame.stack.last().cloned() else {
            return;
        };
        if self.fn_arity.contains_key(&id) {
            return;
        }
        let id_stack = self.find_stack(input);
        let StackNode::Cons(_, ty) = self.stacks[id_stack.0 as usize].clone() else {
            return;
        };
        self.note_fn(id, ty);
    }

    fn stack_for_arity(
        &mut self,
        stack: StackId,
        arity_in: usize,
        arity_out: usize,
        span: Span,
    ) -> Result<StackId, Diagnostic> {
        let (under, _) = self.pop_slot(stack, span)?;
        let mut current = under;
        for _ in 0..arity_in {
            let (rest, _) = self.pop_slot(current, span)?;
            current = rest;
        }
        for _ in 0..arity_out {
            let ty = self.ty_var();
            current = self.stack_cons(current, ty);
        }
        Ok(current)
    }
}

fn pat_test(lit: &Lit) -> PatTest {
    match lit {
        Lit::Bool(value) => PatTest::Bool(*value),
        Lit::Int(lexeme) => PatTest::Int(lexeme.clone()),
        Lit::Number(lexeme) => PatTest::Number(lexeme.clone()),
        Lit::String(value) => PatTest::String(value.clone()),
    }
}

fn restore(frame: &mut Frame, below: Vec<LocalId>, outputs: Vec<LocalId>) {
    frame.stack = below.into_iter().map(Operand::Local).collect();
    frame.stack.extend(outputs.into_iter().map(Operand::Local));
}

fn drain_results(
    checker: &mut Checker,
    frame: &mut Frame,
) -> Result<Vec<LocalId>, Diagnostic> {
    let operands: Vec<_> = frame.stack.drain(..).collect();
    force_all(checker, frame, operands)
}

fn force_all(
    checker: &mut Checker,
    frame: &mut Frame,
    operands: Vec<Operand>,
) -> Result<Vec<LocalId>, Diagnostic> {
    let mut ids = Vec::with_capacity(operands.len());
    for operand in operands {
        ids.push(frame.force(checker, operand)?);
    }
    Ok(ids)
}

/// The whole operand stack, after padding a flexible tail so a quotation can
/// take `min` arguments from the top.
fn take_base(
    checker: &mut Checker,
    frame: &mut Frame,
    min: usize,
) -> Result<Vec<LocalId>, Diagnostic> {
    if frame.stack.len() < min {
        let deficit = min - frame.stack.len();
        if !frame.flexible {
            panic!(
                "quotation needs {min} values, the caller has {}",
                frame.stack.len()
            );
        }
        frame.prepend_params(checker, deficit);
    }
    drain_results(checker, frame)
}

fn module_uses_unit(module: &AnfModule) -> bool {
    module.defs.iter().any(|def| expr_uses_unit(&def.body))
}

fn expr_uses_unit(expr: &Expr) -> bool {
    expr.lets.iter().any(|let_| value_uses_unit(&let_.value))
}

fn value_uses_unit(value: &Value) -> bool {
    match value {
        Value::Ctor { name, .. } => name == "Unit",
        Value::Quote(quote) => expr_uses_unit(&quote.body),
        Value::If { yes, no, .. } => expr_uses_unit(yes) || expr_uses_unit(no),
        Value::Choose { arms } => arms.iter().any(|arm| expr_uses_unit(&arm.body)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use super::{javascript, to_anf};
    use crate::parser::parse;
    use crate::paths::ParseMode;

    fn module(body: &str) -> crate::ast::Module {
        let source = format!("{body}");
        parse(&source, ParseMode::Source)
            .unwrap_or_else(|error| panic!("{}\n{source}", error.message))
    }

    fn anf(body: &str) -> String {
        let module = module(body);
        to_anf(&module)
            .unwrap_or_else(|error| panic!("{}\n{body}", error.message))
            .to_string()
    }

    fn js(body: &str) -> String {
        let module = module(body);
        javascript(&module).unwrap_or_else(|error| panic!("{}\n{body}", error.message))
    }

    fn js_err(body: &str) -> String {
        let module = module(body);
        javascript(&module).expect_err(body).message
    }

    fn next_js_test() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    fn run(body: &str, footer: &str) {
        let code = js(body);
        let dir = std::env::temp_dir().join(format!("plylang-js-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("mod-{}.mjs", next_js_test()));
        fs::write(&path, format!("{code}\n{footer}")).unwrap();
        let output = Command::new("node").arg(&path).output().unwrap();
        assert!(
            output.status.success(),
            "js:\n{code}\nstderr:\n{}\nstdout:\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn double_reuses_one_value() {
        assert_eq!(
            anf(": double ( Int -- Int )\n  dup drop"),
            "def double(v0) {\n  return v0\n}"
        );
        assert_eq!(
            js(": double ( Int -- Int )\n  dup drop"),
            "export function double(v0) {\n  return v0;\n}\n"
        );
    }

    #[test]
    fn javascript_runs_the_fragment() {
        run(
            r#"
: foreign iadd ( Int Int -- Int )
: foreign isub ( Int Int -- Int )
: foreign imul ( Int Int -- Int )
: foreign ieq ( Int Int -- Bool )
: foreign not ( Bool -- Bool )
: foreign nadd ( Num Num -- Num )

; Maybe a = Nothing | Just a
; Pair a b = Pair frs : a, snd : b

: double ( Int -- Int )
  dup iadd

: fromMaybe ( a. a (Maybe a) -- a )
  Nothing ->
  Just -> nip

: mapMaybe ( a b. (Maybe a) (a -- b) -- (Maybe b) )
  Nothing q -> q drop Nothing
  (Just x) q -> x q call Just

: bumpUnder ( (Int) (Int) -- (Int) (Int) )
  [ 1n iadd ] dip

: held ( a. a -- a )
  [ ] dip

: twice ( a. a (a -- a) -- a )
  call

: runDup ( a. a -- a a )
  [ dup ] call

: chosen ( a. a a Bool -- a )
  [ drop ] [ nip ] if

: fact ( Int -- Int )
  dup 0n ieq [ drop 1n ] [ dup 1n isub fact imul ] if

: takeSnd ( (Pair a b) -- b )
  (Pair _ y) -> y

: pairSnd ( a b. a b -- b )
  Pair takeSnd

: unit ( -- Unit )
  Unit

: label ( -- Str )
  "hi"

: flag ( Bool -- Bool )
  not

: sub ( (Int) (Int) -- Int )
  isub

: wrapCall ( a. a -- a )
  quote call

: addn ( (Num) (Num) -- Num )
  nadd

: under ( a b. a b (a -- b) -- b b )
  dip

: choose ( a. a a (Bool) (a a -- a) (a a -- a) -- a )
  if

: fromRight ( a b. b (Either a b) -- b )
  Left -> drop
  Right -> nip

; Either a b = Left a | Right b
"#,
            r#"
function iadd(a, b) { return a + b; }
function isub(a, b) { return a - b; }
function imul(a, b) { return a * b; }
function ieq(a, b) { return a === b; }
function not(a) { return !a; }
function nadd(a, b) { return a + b; }
const [lo, hi] = bumpUnder(20n, 5n);
const [a, b] = runDup(7n);
const wrapped = pairSnd(1n, 2n);
if (double(21n) !== 42n) throw new Error("double");
if (fromMaybe(1n, Nothing) !== 1n) throw new Error("from nothing");
if (fromMaybe(1n, Just(2n)) !== 2n) throw new Error("from just");
if (mapMaybe(Nothing, double).tag !== "Nothing") throw new Error("map nothing");
const mapped = mapMaybe(Just(21n), double);
if (mapped.tag !== "Just" || mapped.value !== 42n) throw new Error("map just");
if (lo !== 21n || hi !== 5n) throw new Error("dip " + lo + " " + hi);
if (held(7n) !== 7n) throw new Error("held");
if (twice(21n, double) !== 42n) throw new Error("call");
if (a !== 7n || b !== 7n) throw new Error("runDup");
if (chosen(1n, 2n, true) !== 1n) throw new Error("chosen true");
if (chosen(1n, 2n, false) !== 2n) throw new Error("chosen false");
if (fact(0n) !== 1n) throw new Error("fact0");
if (fact(5n) !== 120n) throw new Error("fact5 " + fact(5n));
if (wrapped !== 2n) throw new Error("pair");
const pair = Pair(1n, 2n);
if (pair.frs !== 1n || pair.snd !== 2n || pair.value0 !== undefined) throw new Error("pair fields");
if (unit() !== undefined) throw new Error("unit");
if (label() !== "hi") throw new Error("string");
if (flag(true) !== false) throw new Error("not");
if (sub(5n, 1n) !== 4n) throw new Error("sub");
if (wrapCall(9n) !== 9n) throw new Error("quote");
if (addn(1.5, 2) !== 3.5) throw new Error("number");
const dipped = under(3n, 1n, (n) => n + n);
if (dipped[0] !== 6n || dipped[1] !== 1n) throw new Error("dynamic dip");
const keepBottom = (x, y) => x;
const keepTop = (x, y) => y;
if (choose(1n, 2n, true, keepBottom, keepTop) !== 1n) throw new Error("dynamic if");
if (choose(1n, 2n, false, keepBottom, keepTop) !== 2n) throw new Error("dynamic if false");
if (fromRight(0n, Left(1n)) !== 0n) throw new Error("left");
if (fromRight(0n, Right(2n)) !== 2n) throw new Error("right");
console.log("ok");
"#,
        );
    }

    #[test]
    fn javascript_runs_stack_words() {
        let source = r#"
: foreign iadd ( Int Int -- Int )
: foreign imul ( Int Int -- Int )
: foreign nadd ( Num Num -- Num )
: foreign nmul ( Num Num -- Num )

: rotated ( Int Int Int -- Int Int Int )
  rot

: unrotated ( Int Int Int -- Int Int Int )
  unrot

: doubled ( Int Int -- Int Int Int Int )
  dup2

: tucked ( Int Int -- Int Int Int )
  tuck

: dipped ( Int Int Int -- Int Int Int )
  [ 1n iadd ] dip2

: kept ( Int -- Int Int )
  [ 1n iadd ] keep

: both ( Num -- Num Num )
  [ 2 nmul ] [ 3 nadd ] bi

: each ( Num Num -- Num Num )
  [ 1 nadd ] bia

: spread ( Int Int -- Int Int )
  [ 1n iadd ] [ 2n imul ] bis

: three ( Int -- Int Int Int )
  [ 1n iadd ] [ 2n imul ] [ 3n iadd ] tri

: applyBi ( Num (Num -- Num) (Num -- Num) -- Num Num )
  bi

: hold ( Int (Int -- Int) -- Int Int )
  keep

: added ( Num -- Num )
  [ 1 nadd ] cons call

: pushed ( Num -- Num Num )
  [ 1 ] cons call

: summed ( Num Num -- Num )
  [ nadd ] cons call

: wrapped ( Num -- Num )
  [ ] cons call

: closed ( Num (Num -- Num) -- ( -- Num ) )
  cons

: closed2 ( Num (Num Num -- Num) -- ( Num -- Num ) )
  cons

: applied ( Num (Num -- Num) -- Num )
  [ call ] cons call

: applied2 ( Num Num (Num Num -- Num) -- Num )
  [ call ] cons call

: choose ( Num Num Bool -- Num )
  [ drop ] [ nip ] [ if ] cons cons call
"#;
        let code = js(source);
        assert!(
            !code.contains("rot("),
            "shuffle became a call:\n{code}"
        );
        assert!(!code.contains("cons("), "cons became a call:\n{code}");
        assert_eq!(
            js(": rotated ( Int Int Int -- Int Int Int )\n  rot"),
            "export function rotated(v0, v1, v2) {\n  return [v1, v2, v0];\n}\n"
        );
        run(
            source,
            r#"
function iadd(a, b) { return a + b; }
function imul(a, b) { return a * b; }
function nadd(a, b) { return a + b; }
function nmul(a, b) { return a * b; }
const [rb, rc, ra] = rotated(1n, 2n, 3n);
if (ra !== 1n || rb !== 2n || rc !== 3n) throw new Error("rot " + [ra, rb, rc]);
const [uc, ua, ub] = unrotated(1n, 2n, 3n);
if (uc !== 3n || ua !== 1n || ub !== 2n) throw new Error("unrot " + [uc, ua, ub]);
const [d0, d1, d2, d3] = doubled(4n, 5n);
if (d0 !== 4n || d1 !== 5n || d2 !== 4n || d3 !== 5n) throw new Error("dup2");
const [t0, t1, t2] = tucked(8n, 9n);
if (t0 !== 9n || t1 !== 8n || t2 !== 9n) throw new Error("tuck " + [t0, t1, t2]);
const [x, y, z] = dipped(10n, 20n, 30n);
if (x !== 11n || y !== 20n || z !== 30n) throw new Error("dip2 " + [x, y, z]);
const [k0, k1] = kept(5n);
if (k0 !== 6n || k1 !== 5n) throw new Error("keep " + [k0, k1]);
const [b0, b1] = both(5);
if (b0 !== 10 || b1 !== 8) throw new Error("bi " + [b0, b1]);
const [e0, e1] = each(4, 5);
if (e0 !== 5 || e1 !== 6) throw new Error("bia " + [e0, e1]);
const [s0, s1] = spread(3n, 4n);
if (s0 !== 4n || s1 !== 8n) throw new Error("bis " + [s0, s1]);
const [h0, h1, h2] = three(3n);
if (h0 !== 4n || h1 !== 6n || h2 !== 6n) throw new Error("tri " + [h0, h1, h2]);
const [p, q] = applyBi(5, (n) => n * 2, (n) => n + 3);
if (p !== 10 || q !== 8) throw new Error("dynamic bi " + [p, q]);
const [held, saved] = hold(5n, (n) => n + 1n);
if (held !== 6n || saved !== 5n) throw new Error("dynamic keep " + [held, saved]);
if (added(5) !== 6) throw new Error("cons add " + added(5));
const [p0, p1] = pushed(5);
if (p0 !== 5 || p1 !== 1) throw new Error("cons push " + [p0, p1]);
if (summed(3, 5) !== 8) throw new Error("cons sum " + summed(3, 5));
if (wrapped(5) !== 5) throw new Error("cons wrap " + wrapped(5));
const f = closed(5, (n) => n + 1);
if (f() !== 6) throw new Error("cons fun " + f());
const g = closed2(5, (a, b) => a * 10 + b);
if (g(3) !== 35) throw new Error("cons fun2 " + g(3));
if (applied(5, (n) => n + 1) !== 6) throw new Error("applied " + applied(5, (n) => n + 1));
if (applied2(3, 5, (a, b) => a * 10 + b) !== 35) throw new Error("applied2");
if (choose(1, 2, true) !== 1) throw new Error("choose true " + choose(1, 2, true));
if (choose(1, 2, false) !== 2) throw new Error("choose false " + choose(1, 2, false));
"#,
        );
    }

    #[test]
    fn javascript_runs_the_maybe_example() {
        let source = "\
; Maybe a = Nothing | Just a

: fromMaybe ( a (Maybe a) -- a )
  Nothing ->
  Just -> nip

: mapMaybe ( (Maybe a) (a -- b) -- (Maybe b) )
  Nothing q -> q drop Nothing
  (Just x) q -> x q call Just
";
        let module = parse(source, ParseMode::Source).unwrap();
        let code = javascript(&module).unwrap_or_else(|error| panic!("{}", error.message));
        let dir = std::env::temp_dir().join(format!("plylang-maybe-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mod.mjs");
        fs::write(
            &path,
            format!(
                "{code}\n{}",
                r#"
const double = (n) => n + n;
if (fromMaybe(1n, Nothing) !== 1n) throw new Error("from nothing");
if (fromMaybe(1n, Just(2n)) !== 2n) throw new Error("from just");
if (mapMaybe(Nothing, double).tag !== "Nothing") throw new Error("map nothing");
const mapped = mapMaybe(Just(21n), double);
if (mapped.tag !== "Just" || mapped.value !== 42n) throw new Error("map just " + mapped.value);
"#
            ),
        )
        .unwrap();
        let output = Command::new("node").arg(&path).output().unwrap();
        assert!(
            output.status.success(),
            "js:\n{code}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn javascript_runs_a_three_arm_match() {
        run(
            r#"
; Color = Red | Green | Blue

: code ( Color -- Int )
  Red -> 1n
  Green -> 2n
  Blue -> 3n
"#,
            r#"
if (code(Red) !== 1n) throw new Error("red");
if (code(Green) !== 2n) throw new Error("green");
if (code(Blue) !== 3n) throw new Error("blue");
"#,
        );
    }

    #[test]
    fn class_method_calls_the_instance_word() {
        let body = r#"
: foreign ieq ( Int Int -- Bool )
: foreign ineg ( Int -- Int )
: foreign idiv ( Int Int -- Int )

; Maybe a = Nothing | Just a

& Eq a
  eq ( a a -- Bool )

: eqInt ( Int Int -- Bool )
  ieq

# Eq Int
  eq = eqInt

: use ( Int Int -- Bool )
  eq

: same ( a. Eq a => a a -- Bool )
  eq

: useSame ( Int Int -- Bool )
  same

& Functor f
  map ( a b. (f a) (a -- b) -- (f b) )

: mapMaybe ( a b. (Maybe a) (a -- b) -- (Maybe b) )
  Nothing q -> q drop Nothing
  (Just x) q -> x q call Just

# Functor Maybe
  map = mapMaybe

: negate ( (Maybe Int) -- (Maybe Int) )
  [ ineg ] map

: divInt ( Int Int -- (Maybe Int) )
  dup 0n ieq [ drop drop Nothing ] [ idiv Just ] if
"#;
        let code = js(body);
        assert!(code.contains("eqInt("), "{code}");
        assert!(!code.contains("function eq("), "{code}");
        assert!(code.contains(".eq"), "{code}");
        assert!(code.contains("eq: eqInt"), "{code}");
        assert!(code.contains("mapMaybe("), "{code}");
        assert!(!code.contains("function map("), "{code}");
        run(
            body,
            r#"
function ieq(a, b) { return a === b; }
function ineg(a) { return -a; }
function idiv(a, b) { return a / b; }
if (use(1n, 1n) !== true) throw new Error("eq");
if (use(1n, 2n) !== false) throw new Error("neq");
if (useSame(1n, 1n) !== true) throw new Error("same");
const negated = negate(Just(3n));
if (negated.tag !== "Just" || negated.value !== -3n) throw new Error("map");
if (negate(Nothing).tag !== "Nothing") throw new Error("map nothing");
const quotient = divInt(6n, 2n);
if (quotient.tag !== "Just" || quotient.value !== 3n) throw new Error("div");
if (divInt(1n, 0n).tag !== "Nothing") throw new Error("div0");
"#,
        );
    }

    #[test]
    fn maybe_equality_closes_over_the_inner_dictionary() {
        let body = r#"
: foreign ieq ( Int Int -- Bool )

; Maybe a = Nothing | Just a

& Eq a
  eq ( a a -- Bool )

: eqInt ( Int Int -- Bool )
  ieq

# Eq Int
  eq = eqInt

: eqMaybe ( a. Eq a => (Maybe a) (Maybe a) -- Bool )
  Nothing Nothing -> true
  (Just _) Nothing -> false
  Nothing (Just _) -> false
  (Just x) (Just y) -> x y eq

# Eq (Maybe a)
  eq = eqMaybe

: same ( a. Eq a => a a -- Bool )
  eq

: useEq ( (Maybe Int) (Maybe Int) -- Bool )
  eq

: useSame ( (Maybe Int) (Maybe Int) -- Bool )
  same
"#;
        let code = js(body);
        assert!(code.contains("eqMaybe("), "{code}");
        assert!(code.contains("eq: eqInt"), "{code}");
        assert!(code.contains("=>"), "{code}");
        run(
            body,
            r#"
function ieq(a, b) { return a === b; }
const yes = Just(1n);
const no = Just(2n);
if (useEq(yes, yes) !== true) throw new Error("jj");
if (useEq(yes, no) !== false) throw new Error("jne");
if (useEq(Nothing, Nothing) !== true) throw new Error("nn");
if (useEq(Nothing, yes) !== false) throw new Error("nj");
if (useEq(yes, Nothing) !== false) throw new Error("jn");
if (useSame(yes, no) !== false) throw new Error("same");
if (useSame(Nothing, Nothing) !== true) throw new Error("same nn");
"#,
        );
    }

    #[test]
    fn a_superclass_dictionary_carries_the_parent() {
        let body = r#"
: foreign ieq ( Int Int -- Bool )

& Eq a
  eq ( a a -- Bool )

& Eq a => Ord a
  compare ( a a -- Bool )

: eqInt ( Int Int -- Bool )
  ieq

: cmpInt ( Int Int -- Bool )
  drop drop true

# Eq Int
  eq = eqInt

# Ord Int
  compare = cmpInt

: both ( Ord a => a a -- Bool )
  eq

: use ( Int Int -- Bool )
  eq

: useBoth ( Int Int -- Bool )
  both
"#;
        let code = js(body);
        assert!(code.contains("eqInt("), "{code}");
        assert!(code.contains("Eq:"), "{code}");
        assert!(code.contains(".eq"), "{code}");
        run(
            body,
            r#"
function ieq(a, b) { return a === b; }
if (use(1n, 1n) !== true) throw new Error("eq");
if (use(1n, 2n) !== false) throw new Error("neq");
if (useBoth(1n, 1n) !== true) throw new Error("both");
if (useBoth(1n, 2n) !== false) throw new Error("both neq");
"#,
        );
    }

    #[test]
    fn result_type_picks_empty_and_pure() {
        let body = r#"
: foreign scat ( Str Str -- Str )
: foreign iadd ( Int Int -- Int )

& Neutral a
  empty ( -- a )

& Appendable a
  append ( a a -- a )

& Liftable f
  pure ( a -- (f a) )

& Chainable m
  bind ( (m a) (a -- (m b)) -- (m b) )

: emptyString ( -- Str )
  ""

: appendString ( Str Str -- Str )
  scat

# Neutral Str
  empty = emptyString

# Appendable Str
  append = appendString

: blank ( -- Str )
  empty

: beside ( Str -- Str )
  empty swap append

; Maybe a = Nothing | Just a

: pureMaybe ( a -- (Maybe a) )
  Just

: bindMaybe ( (Maybe a) (a -- (Maybe b)) -- (Maybe b) )
  Nothing _ -> Nothing
  (Just x) f -> x f call

# Liftable Maybe
  pure = pureMaybe

# Chainable Maybe
  bind = bindMaybe

: wrapped ( Int -- (Maybe Int) )
  pure

: chainInc ( (Maybe Int) -- (Maybe Int) )
  [ 1n iadd pure ] bind
"#;
        let code = js(body);
        assert!(code.contains("emptyString("), "{code}");
        assert!(code.contains("pureMaybe("), "{code}");
        assert!(code.contains("bindMaybe("), "{code}");
        run(
            body,
            r#"
function scat(a, b) { return a + b; }
function iadd(a, b) { return a + b; }
if (blank() !== "") throw new Error("blank");
if (beside("a") !== "a") throw new Error("beside");
const one = wrapped(1n);
if (one.tag !== "Just" || one.value !== 1n) throw new Error("pure");
const inc = chainInc(Just(2n));
if (inc.tag !== "Just" || inc.value !== 3n) throw new Error("chain");
if (chainInc(Nothing).tag !== "Nothing") throw new Error("chain nothing");
"#,
        );
    }

    #[test]
    fn a_product_field_is_property_access() {
        assert_eq!(
            js("; Person = Person name : Str, age : Num\n\n: nameOf ( Person -- Str )\n  name>>"),
            "export const Person = (v0, v1) => ({ tag: \"Person\", name: v0, age: v1 });\n\nexport function nameOf(v0) {\n  const v1 = v0.name;\n  return v1;\n}\n"
        );
    }

    #[test]
    fn named_constructor_field_is_a_property() {
        assert_eq!(
            js("; Entry = Entry id : Num, at : Num\n\n: get ( Entry -- Num )\n  at>>\n"),
            "export const Entry = (v0, v1) => ({ tag: \"Entry\", id: v0, at: v1 });\n\nexport function get(v0) {\n  const v1 = v0.at;\n  return v1;\n}\n"
        );
        run(
            r#"
: foreign nadd ( Num Num -- Num )

; Entry = Entry id : Num, at : Num

: shift ( Entry -- Entry )
  dup at>> 1 nadd >>at
"#,
            r#"
function nadd(a, b) { return a + b; }
const first = Entry(1, 10);
const next = shift(first);
if (first.at !== 10 || first.id !== 1) throw new Error("old " + JSON.stringify(first));
if (next.at !== 11 || next.id !== 1 || next.tag !== "Entry") throw new Error("next " + JSON.stringify(next));
"#,
        );
    }

    #[test]
    fn javascript_reads_product_fields() {
        run(
            r#"
; Person = Person name : Str, age : Int

: foreign person ( -- Person )

: nameOf ( Person -- Str )
  name>>

: ageOf ( Person -- Int )
  age>>

: keepName ( Person -- Person Str )
  dup name>>

: quoted ( Person -- Str )
  [ name>> ] call

: personName ( -- Str )
  person name>>
"#,
            r#"
function person() { return { tag: "Person", name: "Ada", age: 36n }; }
const value = { tag: "Person", name: "Ada", age: 36n };
if (nameOf(value) !== "Ada") throw new Error("name");
if (ageOf(value) !== 36n) throw new Error("age");
const both = keepName(value);
if (both[0].name !== "Ada" || both[1] !== "Ada") throw new Error("dup");
if (quoted(value) !== "Ada") throw new Error("quote");
if (personName() !== "Ada") throw new Error("foreign");
"#,
        );
    }

    #[test]
    fn javascript_runs_a_local_captured_by_a_quotation() {
        run(
            r#"
: foreign nadd ( Num Num -- Num )
: foreign amap ( (Array a) (a -- b) -- (Array b) )

: addToAll ( Num (Array Num) -- (Array Num) )
  n xs ->
    xs [ n nadd ] amap

: curry ( a b c. a (a b -- c) -- (b -- c) )
  x q ->
    [ x swap q call ]

: addOne ( Num -- Num )
  1 [ nadd ] curry call
"#,
            r#"
function nadd(a, b) { return a + b; }
function amap(arr, fn) { return arr.map(fn); }
const summed = addToAll(10, [1, 2, 3]);
if (summed.join(",") !== "11,12,13") throw new Error("addToAll " + summed);

const addTwo = curry(2, (x, y) => x + y);
if (addTwo(3) !== 5) throw new Error("curry " + addTwo(3));
if (addOne(4) !== 5) throw new Error("addOne");
"#,
        );
    }

    #[test]
    fn call_runs_a_quotation_returned_by_call() {
        run(
            r#"
: applyTo ( a b. ( -- a ) ( -- (a -- b) ) -- b )
  act getF ->
    act call getF call call

: later ( a b. ( -- a ) ( -- (a -- b) ) -- ( -- b ) )
  act getF ->
    [ act call getF call call ]
"#,
            r#"
const result = applyTo(() => 1, () => (n) => n + 1);
if (result !== 2) throw new Error("applyTo " + result);
const suspended = later(() => 1, () => (n) => n + 1);
if (suspended() !== 2) throw new Error("later " + suspended());
"#,
        );
    }

    #[test]
    fn reserved_name_is_exported_as_the_ply_word() {
        assert_eq!(
            js(": new ( Num -- Num )\n  dup drop"),
            "\
function ply_new(v0) {
  return v0;
}
export { ply_new as new };
"
        );
    }

    #[test]
    fn call_runs_each_step_of_a_curried_quotation() {
        run(
            r#"
: foreign nadd ( Num Num -- Num )

: curry ( a b c d. (a b c -- d) -- (a -- (b -- (c -- d))) )
  f ->
    [ a -> [ b -> [ c -> a b c f call ] ] ]

: use ( -- Num )
  3 2 1 [ nadd nadd ] curry call call call
"#,
            r#"
function nadd(a, b) { return a + b; }
if (use() !== 6) throw new Error("curry " + use());
"#,
        );
    }

    #[test]
    fn call_runs_a_function_bound_in_the_quotation() {
        run(
            r#"
: foreign nadd ( Num Num -- Num )
: foreign reduce ( a b. (Array a) b (b a -- b) -- b )
: foreign push ( (Array a) a -- (Array a) )

: run ( Num (Num -- Num) -- Num )
  [ x f -> x f call ] call

: applyArray ( a b. (Array a) (Array (a -- b)) -- (Array b) )
  xs fs ->
    fs <> [ acc f -> xs acc [ ys x -> ys x f call push ] reduce ] reduce

: use ( -- Num )
  2 [ 3 nadd ] run

: both ( -- (Array Num) )
  < 1 2 > < [ 10 nadd ] [ 20 nadd ] > applyArray
"#,
            r#"
function nadd(a, b) { return a + b; }
function reduce(arr, acc, fn) {
  for (const x of arr) acc = fn(acc, x);
  return acc;
}
function push(arr, x) { return arr.concat([x]); }
if (use() !== 5) throw new Error("run " + use());
const got = both().join(",");
if (got !== "11,12,21,22") throw new Error("apply " + got);
"#,
        );
    }

    #[test]
    fn call_runs_a_function_read_from_a_field() {
        run(
            r#"
; Task = Task (Num -- Num)

: runTask ( Task Num -- Num )
  (Task go) n -> n go call

: later ( Task Num -- ( -- Num ) )
  (Task go) n -> n go [ call ] cons cons
"#,
            r#"
const task = Task((n) => n + 1);
if (runTask(task, 3) !== 4) throw new Error("runTask");
if (later(task, 3)() !== 4) throw new Error("later");
"#,
        );
    }

    #[test]
    fn call_runs_a_function_synonym() {
        run(
            "\
- Task ( Num -- Num )

: run ( Num Task -- Num )
  call
",
            "\
const inc = (n) => n + 1;
if (run(3, inc) !== 4) throw new Error(\"run \" + run(3, inc));
",
        );
    }

    #[test]
    fn call_runs_a_quotation_returned_by_a_word() {
        assert_eq!(
            js(
                ": foreign print ( Str -- )\n\n: printer ( -- (Str --) )\n  [ print ]\n\n: say ( Str -- )\n  printer call\n"
            ),
            "\
export function printer() {
  const v1 = (v0) => {
    print(v0);
  };
  return v1;
}

export function say(v2) {
  const v3 = printer();
  v3(v2);
}
"
        );
    }

    #[test]
    fn io_foreign_stays_a_plain_call() {
        assert_eq!(
            js(": foreign read ( -- Str )\n\n: line ( -- Str )\n  read\n"),
            "export function line() {\n  const v0 = read();\n  return v0;\n}\n"
        );
    }

    #[test]
    fn javascript_reports_checker_errors() {
        assert_eq!(
            js_err(": bad ( Str -- Int )\n  dup drop"),
            "type mismatch: expected Int, found Str"
        );
        assert_eq!(
            js_err(": bad ( ..s -- ..s )\n  [ [ dup ] call ] drop"),
            "quotation arity is not known"
        );
    }

    #[test]
    fn javascript_runs_arrays_and_products() {
        run(
            r#"
: foreign nadd ( Num Num -- Num )
: foreign iadd ( Int Int -- Int )
: foreign amap ( (Array a) (a -- b) -- (Array b) )
: foreign push ( (Array a) a -- (Array a) )

: shift ( -- (Array Num) )
  < 1 2 3 > [ 1 nadd ] amap

: exact ( -- (Array Int) )
  < 1n 2n 3n > [ 1n iadd ] amap

: nums ( -- (Array Num) )
  < 1 2 >

: grow ( (Array Num) -- (Array Num) )
  3 push

; Person = Person name : Str, age : Num

: person ( -- Person )
  "Ada" 36 Person

: older ( Person -- Person )
  37 >>age

: bump ( Num Num -- Num Num )
  [ 1 nadd ] dip
"#,
            r#"
function nadd(a, b) { return a + b; }
function iadd(a, b) { return a + b; }
function amap(arr, fn) { return arr.map(fn); }
function push(arr, elt) { return [...arr, elt]; }
const shifted = shift();
if (shifted.join(",") !== "2,3,4") throw new Error("shift " + shifted);
const ints = exact();
if (ints[0] !== 2n || ints[2] !== 4n) throw new Error("exact");
const base = nums();
const grown = grow(base);
if (base.length !== 2 || grown.length !== 3 || grown[2] !== 3) throw new Error("push");
const p = person();
const q = older(p);
if (p.age !== 36 || q.age !== 37 || q.name !== "Ada" || p.tag !== "Person") throw new Error("product");
const pair = bump(20, 5);
if (pair[0] !== 21 || pair[1] !== 5) throw new Error("dip " + pair);
"#,
        );
    }

    #[test]
    fn javascript_rejects_a_record() {
        let braces = parse(": bad ( -- Str )\n  { name: \"Ada\" }\n", ParseMode::Source).unwrap_err();
        assert_eq!(braces.message, "unexpected character");
        let plus = parse(": bad ( Int -- Int )\n  +city\n", ParseMode::Source).unwrap_err();
        assert_eq!(plus.message, "unexpected character");
    }

    #[test]
    fn at_is_an_ordinary_call() {
        let indexed = js(": foreign at ( (Array a) Num -- a )\n\n: second ( (Array Num) -- Num )\n  1 at\n");
        assert!(indexed.contains("at(v"));
        assert!(!indexed.contains("function at"));
        assert!(!indexed.contains("array index out of range"));
    }

    #[test]
    fn a_foreign_type_emits_nothing() {
        let code = js(
            "; foreign Hub a\n; Box = Box\n\n: pass ( (Hub a) -- (Hub a) )\n  hub -> hub\n",
        );
        assert_eq!(
            code,
            "export const Box = { tag: \"Box\" };\n\nexport function pass(v0) {\n  let v1;\n  v1 = v0;\n  return v1;\n}\n"
        );
        run(
            r#"
; foreign Box a

: foreign make ( a. a -- (Box a) )
: foreign open ( a. (Box a) -- a )

: round ( a. a -- a )
  make open
"#,
            r#"
function make(value) { return { value }; }
function open(box) { return box.value; }
if (round(7) !== 7) throw new Error("round");
"#,
        );
    }

    #[test]
    fn javascript_erases_a_newtype_and_expands_a_synonym() {
        let code = js(
            "; Age == Age Num\n\n: older ( Age -- Age )\n  Age -> Age\n",
        );
        assert!(code.contains("export const Age = (value) => value;"));
        assert!(!code.contains("tag: \"Age\""));
        run(
            r#"
: foreign nadd ( Num Num -- Num )

; Person = Person name : Str, age : Num
- Human Person

: olderPerson ( Human -- Human )
  37 >>age

; Age == Age Num

: olderAge ( Age -- Age )
  Age -> 1 nadd Age
"#,
            r#"
function nadd(a, b) { return a + b; }
const person = Person("Ada", 36);
const older = olderPerson(person);
if (person.age !== 36 || older.name !== "Ada" || older.age !== 37) throw new Error("synonym");
if (Age(36) !== 36) throw new Error("identity");
if (olderAge(36) !== 37) throw new Error("age");
"#,
        );
    }

    #[test]
    fn javascript_runs_clauses() {
        let code = js(
            ": foreign at ( (Array a) Num -- a )\n\n; Maybe a = Nothing | Just a\n\n: second ( (Maybe (Array Num)) -- Num )\n  Nothing -> 0\n  (Just xs) -> xs 0 at\n",
        );
        assert!(code.contains("at("), "{code}");
        assert!(!code.contains("function at"), "{code}");
        run(
            r#"
: foreign isub ( Int Int -- Int )
: foreign imul ( Int Int -- Int )
: foreign idiv ( Int Int -- Int )
: foreign iadd ( Int Int -- Int )
: foreign at ( (Array a) Num -- a )

; Maybe a = Nothing | Just a

: toString ( Bool -- Str )
  true -> "true"
  false -> "false"

: fact ( Int -- Int )
  0n -> 1n
  n -> n dup 1n isub fact imul

: mapMaybe ( a b. (Maybe a) (a -- b) -- (Maybe b) )
  Nothing q -> q drop Nothing
  (Just x) q -> x q call Just

: divInt ( Int Int -- (Maybe Int) )
  x 0n -> Nothing
  x y -> x y idiv Just

: fromMaybe ( a. a (Maybe a) -- a )
  x Nothing -> x
  x (Just y) -> y

: flatten ( a. (Maybe (Maybe a)) -- (Maybe a) )
  Nothing -> Nothing
  (Just Nothing) -> Nothing
  (Just (Just x)) -> x Just

: fromJust ( (Maybe Int) -- Int )
  Nothing -> 0n
  Just ->

: isJust ( a. (Maybe a) -- Bool )
  Nothing -> false
  (Just _) -> true

: swapped ( a b. a b -- b a )
  x y -> y x

: bump ( Int -- Int )
  0n -> 1n
  n -> [ n 1n iadd ] call

; Age == Age Num

: years ( Age -- Num )
  (Age n) -> n

: second ( (Maybe (Array Num)) -- Num )
  Nothing -> 0
  (Just xs) -> xs 0 at

: isEmpty ( (Array a) -- Bool )
  <> -> true
  _ -> false

: takeFive ( (Array Int) -- Int )
  < 0n 1n a b _ > -> a b imul
  _ -> 0n

: onlyJust ( (Array (Maybe Int)) -- Int )
  < (Just x) > -> x
  _ -> 0n

: nested ( (Array (Array Num)) -- Num )
  < < a > > -> a
  _ -> 0
"#,
            r#"
function isub(a, b) { return a - b; }
function imul(a, b) { return a * b; }
function idiv(a, b) { return a / b; }
function iadd(a, b) { return a + b; }
function at(arr, i) { return arr[i]; }
const double = (n) => n + n;
if (toString(true) !== "true" || toString(false) !== "false") throw new Error("bool");
if (fact(0n) !== 1n || fact(5n) !== 120n) throw new Error("fact " + fact(5n));
if (mapMaybe(Nothing, double).tag !== "Nothing") throw new Error("map nothing");
const mapped = mapMaybe(Just(21n), double);
if (mapped.tag !== "Just" || mapped.value !== 42n) throw new Error("map just");
if (divInt(4n, 0n).tag !== "Nothing") throw new Error("div zero");
const quarter = divInt(4n, 2n);
if (quarter.tag !== "Just" || quarter.value !== 2n) throw new Error("div");
if (fromMaybe(1n, Nothing) !== 1n) throw new Error("from nothing");
if (fromMaybe(1n, Just(2n)) !== 2n) throw new Error("from just");
if (flatten(Nothing).tag !== "Nothing") throw new Error("flat nothing");
if (flatten(Just(Nothing)).tag !== "Nothing") throw new Error("flat just nothing");
const flat = flatten(Just(Just(3n)));
if (flat.tag !== "Just" || flat.value !== 3n) throw new Error("flat just just");
if (fromJust(Nothing) !== 0n) throw new Error("fromJust nothing");
if (fromJust(Just(4n)) !== 4n) throw new Error("fromJust");
if (isJust(Nothing) !== false || isJust(Just(1n)) !== true) throw new Error("isJust");
const pair = swapped(1n, 2n);
if (pair[0] !== 2n || pair[1] !== 1n) throw new Error("swap");
if (bump(0n) !== 1n || bump(4n) !== 5n) throw new Error("bump");
if (years(41) !== 41) throw new Error("age");
if (second(Nothing) !== 0) throw new Error("second nothing");
if (second(Just([9, 8])) !== 9) throw new Error("second");
if (isEmpty([]) !== true || isEmpty([1]) !== false) throw new Error("empty");
if (takeFive([0n, 1n, 2n, 3n, 4n]) !== 6n) throw new Error("take five " + takeFive([0n, 1n, 2n, 3n, 4n]));
if (takeFive([1n, 2n, 3n, 4n, 5n]) !== 0n) throw new Error("take other");
if (takeFive([]) !== 0n) throw new Error("take empty");
if (takeFive([0n, 1n, 2n, 3n, 4n, 5n]) !== 0n) throw new Error("take long");
if (onlyJust([Just(4n)]) !== 4n) throw new Error("just");
if (onlyJust([Nothing]) !== 0n || onlyJust([]) !== 0n) throw new Error("just miss");
if (nested([[7]]) !== 7) throw new Error("nested");
if (nested([[]]) !== 0 || nested([]) !== 0 || nested([[1, 2]]) !== 0) throw new Error("nested miss");
"#,
        );
        assert_eq!(
            js_err(": bad ( Int -- Int )\n  0n -> 1n\n"),
            "non-exhaustive match: Int"
        );
    }

    #[test]
    fn javascript_runs_named_field_patterns() {
        let code = js(
            "; Person = Person name : Str, year : Int\n\n: nameOf ( Person -- Str )\n  (Person n _) -> n\n",
        );
        assert!(code.contains(".name"), "{code}");
        assert!(!code.contains(".value"), "{code}");
        run(
            r#"
; Person = Person name : Str, year : Int

: nameOf ( Person -- Str )
  (Person n _) -> n

: born ( Person -- Str )
  (Person n 1815n) -> n
  (Person n _) -> n
"#,
            r#"
const ada = Person("Ada", 36n);
if (nameOf(ada) !== "Ada") throw new Error("name");
if (born(Person("Ada", 1815n)) !== "Ada") throw new Error("ada");
if (born(Person("Grace", 1906n)) !== "Grace") throw new Error("grace");
"#,
        );
    }

    #[test]
    fn a_method_constraint_is_passed_at_the_call() {
        run(
            r#"
& Comparable a
  eq ( a a -- Bool )

: eqInt ( Int Int -- Bool )
  drop drop true

# Comparable Int
  eq = eqInt

& Wrappable f
  wrap ( a -- (f a) )

; Box a = Mk a

: wrapBox ( a -- (Box a) )
  Mk

# Wrappable Box
  wrap = wrapBox

& Walk t
  step ( f. Wrappable f => (t Int) -- (f (t Int)) )

: stepBox ( f. Comparable Int, Wrappable f => (Box Int) -- (f (Box Int)) )
  wrap

# Walk Box
  step = stepBox

: use ( (Box Int) -- (Box (Box Int)) )
  step

: usePoly ( t f. Walk t, Wrappable f => (t Int) -- (f (t Int)) )
  step

: useVia ( (Box Int) -- (Box (Box Int)) )
  usePoly
"#,
            r#"
const once = use(Mk(1n));
if (once.tag !== "Mk" || once.value.tag !== "Mk" || once.value.value !== 1n) {
  throw new Error("use " + JSON.stringify(once));
}
const twice = useVia(Mk(2n));
if (twice.tag !== "Mk" || twice.value.tag !== "Mk" || twice.value.value !== 2n) {
  throw new Error("via " + JSON.stringify(twice));
}
"#,
        );
    }
}
