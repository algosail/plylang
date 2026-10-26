//! Lower a checked module to direct-style ANF.
//!
//! The operand stack is the concrete prefix. A word's own prefix is the cons
//! spine above its tail node; following that tail's link would pass the
//! caller's locals into the callee. A shuffle reuses those operands. A visible
//! quotation is inlined, so a tail it does not consume stays in the caller
//! even when two `if` branches were written with different arities.

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

use super::{Checker, Dict, StackId, StackNode, TyId, TyNode};

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

    fn force(&mut self, checker: &mut Checker, operand: Operand) -> LocalId {
        match operand {
            Operand::Local(id) => id,
            Operand::Quote(quote) => {
                let id = checker.fresh_local();
                checker
                    .fn_arity
                    .insert(id, (quote.params.len(), quote.body.result.len()));
                self.lets.push(Let {
                    binds: vec![id],
                    value: Value::Quote(quote),
                });
                id
            }
        }
    }
}

impl Checker {
    fn lower_module(&mut self, module: &Module) -> Result<AnfModule, Diagnostic> {
        self.prepare(module)?;
        self.lower_prepared(module)
    }

    pub(crate) fn lower_prepared(&mut self, module: &Module) -> Result<AnfModule, Diagnostic> {
        let mut defs = Vec::new();
        for item in &module.items {
            if let Item::Def(def) = item {
                if !def.body.is_empty() || !def.clauses.is_empty() {
                    defs.push(self.lower_def(def)?);
                }
            }
        }
        let datas = module
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Data(data) => Some(AnfData {
                    ctors: data
                        .ctors
                        .iter()
                        .map(|ctor| AnfCtor {
                            name: ctor.name.clone(),
                            fields: ctor.fields.len(),
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

    fn lower_def(&mut self, def: &crate::ast::Def) -> Result<AnfDef, Diagnostic> {
        self.use_dicts.clear();
        self.deferred.clear();
        let first = self.lower_def_pass(def)?;
        if self.use_dicts.is_empty() {
            return Ok(first);
        }
        let second = self.lower_def_pass(def)?;
        self.use_dicts.clear();
        Ok(second)
    }

    fn lower_def_pass(&mut self, def: &crate::ast::Def) -> Result<AnfDef, Diagnostic> {
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
        let result = drain_results(self, &mut frame);
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
            export: !def.vis.is_private(),
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
                let (fun, operand) = self.lower_quotation(terms, &frame.env)?;
                frame.stack.push(operand);
                self.stack_cons(stack, fun)
            }
            TermNode::Field(name) => {
                let output = self.project_field(term.span, name, stack)?;
                let operand = frame.pop_one(self);
                let base = frame.force(self, operand);
                let id = self.fresh_local();
                frame.lets.push(Let {
                    binds: vec![id],
                    value: Value::Prop {
                        base,
                        name: name.clone(),
                    },
                });
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
        let value = frame.force(self, value);
        let base = frame.force(self, base);
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
        let value = frame.force(self, value);
        let base = frame.force(self, base);
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
        let local = inner.force(self, operand);
        frame.lets.extend(inner.lets);
        Ok((ty, local))
    }

    fn lower_quotation(
        &mut self,
        terms: &[Term],
        env: &[HashMap<String, LocalId>],
    ) -> Result<(TyId, Operand), Diagnostic> {
        let local = self.effect_var();
        let saved = self.ambient.replace(local);
        let input = self.stack_var();
        let mut env = env.to_vec();
        env.push(HashMap::new());
        let mut frame = Frame {
            stack: Vec::new(),
            params: Vec::new(),
            flexible: true,
            lets: Vec::new(),
            env,
        };
        self.locals.push(HashMap::new());
        let lowered = self.lower_terms(terms, input, &mut frame);
        self.locals.pop();
        self.ambient = saved;
        let output = lowered?;
        let result = drain_results(self, &mut frame);
        let quote = QuoteForm {
            params: frame.params,
            body: Expr {
                lets: frame.lets,
                result,
            },
        };
        Ok((self.ty_fun(input, output, local), Operand::Quote(quote)))
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
            let id = frame.force(self, operand);
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
        let (input, output, constraints) = self.instantiate_use(span, name, stack)?;
        let lowering = builtin::word(name).map(|word| word.lowering);
        match lowering {
            Some(Lowering::Call) => {
                self.known_arity(input, span)?;
                self.lower_call(frame);
                return Ok(output);
            }
            Some(Lowering::Dip { under }) => {
                self.lower_dip(frame, under as usize);
                return Ok(output);
            }
            Some(Lowering::Keep { keep }) => {
                self.lower_keep(frame, keep as usize);
                return Ok(output);
            }
            Some(Lowering::Cleave { vals, quots }) => {
                self.lower_cleave(frame, vals as usize, quots as usize);
                return Ok(output);
            }
            Some(Lowering::Spread { n }) => {
                self.lower_spread(frame, n as usize);
                return Ok(output);
            }
            Some(Lowering::Each { n }) => {
                self.lower_each(frame, n as usize);
                return Ok(output);
            }
            Some(Lowering::If) => {
                self.lower_if(frame);
                return Ok(output);
            }
            Some(Lowering::Quote) => {
                self.lower_quote_word(frame);
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
        let args = force_all(self, frame, args);
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
        let Some(dict) = dicts.first() else {
            self.emit_word(name, dicts, stack_args, produced, frame);
            return;
        };
        match dict {
            Dict::Assumption(index) => {
                let base = self.assumption_locals[*index];
                self.emit_prop_apply(base, name, stack_args, produced, frame);
            }
            Dict::Super { .. } => {
                let base = self.materialize_dict(dict, frame);
                self.emit_prop_apply(base, name, stack_args, produced, frame);
            }
            Dict::Instance { methods, .. } => {
                let method = methods
                    .iter()
                    .find(|method| method.name == name)
                    .expect("class dictionary contains the method");
                let mut args = Vec::new();
                for nested in &method.dicts {
                    args.push(self.materialize_dict(nested, frame));
                }
                args.extend(stack_args);
                self.emit_call(&method.word, args, produced, frame);
            }
        }
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
                            params: self.fresh_vec(method.arity),
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

    fn lower_quote_word(&mut self, frame: &mut Frame) {
        let value = frame.pop_one(self);
        let id = frame.force(self, value);
        frame.stack.push(Operand::Quote(QuoteForm {
            params: Vec::new(),
            body: Expr {
                lets: Vec::new(),
                result: vec![id],
            },
        }));
    }

    fn lower_call(&mut self, frame: &mut Frame) {
        let quote = frame.pop_one(self);
        let (arity, _) = self.quote_arity(&quote);
        let base = take_base(self, frame, arity);
        let expr = self.reduce_quote(quote, base);
        frame.lets.extend(expr.lets);
        frame.stack = expr.result.into_iter().map(Operand::Local).collect();
    }

    fn lower_dip(&mut self, frame: &mut Frame, under: usize) {
        let quote = frame.pop_one(self);
        let hidden = frame.pop_n(self, under);
        let (arity, _) = self.quote_arity(&quote);
        let base = take_base(self, frame, arity);
        let expr = self.reduce_quote(quote, base);
        frame.lets.extend(expr.lets);
        frame.stack = expr.result.into_iter().map(Operand::Local).collect();
        frame.stack.extend(hidden);
    }

    fn lower_keep(&mut self, frame: &mut Frame, keep: usize) {
        let quote = frame.pop_one(self);
        let saved = frame.pop_n(self, keep);
        let saved = force_all(self, frame, saved);
        frame.stack.extend(saved.iter().copied().map(Operand::Local));
        let (arity, _) = self.quote_arity(&quote);
        let base = take_base(self, frame, arity);
        let expr = self.reduce_quote(quote, base);
        frame.lets.extend(expr.lets);
        frame.stack = expr.result.into_iter().map(Operand::Local).collect();
        frame.stack.extend(saved.into_iter().map(Operand::Local));
    }

    /// Every quotation sees the original tail. Only its produced suffix is kept.
    fn lower_cleave(&mut self, frame: &mut Frame, vals: usize, quots: usize) {
        let quotes = frame.pop_n(self, quots);
        let values = frame.pop_n(self, vals);
        let values = force_all(self, frame, values);
        let below = drain_results(self, frame);
        let mut outputs = Vec::new();
        for quote in quotes {
            let mut base = below.clone();
            base.extend(values.iter().copied());
            outputs.extend(self.produce(frame, quote, base));
        }
        restore(frame, below, outputs);
    }

    fn lower_spread(&mut self, frame: &mut Frame, n: usize) {
        let quotes = frame.pop_n(self, n);
        let values = frame.pop_n(self, n);
        let values = force_all(self, frame, values);
        let below = drain_results(self, frame);
        let mut outputs = Vec::new();
        for (value, quote) in values.into_iter().zip(quotes) {
            let mut base = below.clone();
            base.push(value);
            outputs.extend(self.produce(frame, quote, base));
        }
        restore(frame, below, outputs);
    }

    fn lower_each(&mut self, frame: &mut Frame, n: usize) {
        let quote = frame.pop_one(self);
        let values = frame.pop_n(self, n);
        let values = force_all(self, frame, values);
        let below = drain_results(self, frame);
        let mut outputs = Vec::new();
        for value in values {
            let mut base = below.clone();
            base.push(value);
            outputs.extend(self.produce(frame, quote.clone(), base));
        }
        restore(frame, below, outputs);
    }

    /// `reduce_quote` returns the untouched tail plus the quotation's outputs.
    /// The suffix is the outputs alone.
    fn produce(&mut self, frame: &mut Frame, quote: Operand, base: Vec<LocalId>) -> Vec<LocalId> {
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
        expr.result[start..].to_vec()
    }

    fn lower_if(&mut self, frame: &mut Frame) {
        let else_quote = frame.pop_one(self);
        let then_quote = frame.pop_one(self);
        let cond = frame.pop_one(self);
        let cond = frame.force(self, cond);
        let arity = self
            .quote_arity(&then_quote)
            .0
            .max(self.quote_arity(&else_quote).0);
        let base = take_base(self, frame, arity);
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
            roots.push(frame.force(self, operand));
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
            let result = drain_results(self, &mut arm);
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
                let (fields, newtype) = self.ctor_shape(name, ty, span)?;
                if !newtype {
                    conds.push(Cond {
                        root,
                        steps: steps.to_vec(),
                        test: PatTest::Tag(name.clone()),
                    });
                }
                let width = fields.len();
                for (index, field) in fields.into_iter().enumerate() {
                    let id = if newtype {
                        self.materialize(arm, root, steps)
                    } else {
                        let mut child = steps.to_vec();
                        child.push(Access::Field { index, width });
                        self.materialize(arm, root, &child)
                    };
                    self.note_fn(id, field);
                    *arm_stack = self.stack_cons(*arm_stack, field);
                    arm.stack.push(Operand::Local(id));
                }
                Ok(())
            }
            Pattern::Ctor(name, patterns) => {
                let (fields, newtype) = self.ctor_shape(name, ty, span)?;
                if patterns.len() != fields.len() {
                    let count = fields.len();
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
                let width = fields.len();
                for (index, field) in fields.into_iter().enumerate() {
                    let mut child = steps.to_vec();
                    if !newtype {
                        child.push(Access::Field { index, width });
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

    fn quote_arity(&self, quote: &Operand) -> (usize, usize) {
        match quote {
            Operand::Quote(quote) => (quote.params.len(), quote.body.result.len()),
            Operand::Local(id) => self
                .fn_arity
                .get(id)
                .copied()
                .unwrap_or_else(|| panic!("function local v{id} has no recorded arity")),
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
        }
    }

    fn freshen_expr(&mut self, expr: &Expr, map: &mut HashMap<LocalId, LocalId>) -> Expr {
        let mut lets = Vec::new();
        for let_ in &expr.lets {
            let binds = self.fresh_vec(let_.binds.len());
            for (old, new) in let_.binds.iter().zip(&binds) {
                map.insert(*old, *new);
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

    fn fun_arity(&self, ty: TyId) -> Option<(usize, usize)> {
        let ty = self.find_ty(ty);
        match &self.tys[ty.0 as usize] {
            TyNode::Fun(input, output, _) => Some((self.cons_len(*input), self.cons_len(*output))),
            _ => None,
        }
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

fn drain_results(checker: &mut Checker, frame: &mut Frame) -> Vec<LocalId> {
    let operands: Vec<_> = frame.stack.drain(..).collect();
    force_all(checker, frame, operands)
}

fn force_all(checker: &mut Checker, frame: &mut Frame, operands: Vec<Operand>) -> Vec<LocalId> {
    operands
        .into_iter()
        .map(|operand| frame.force(checker, operand))
        .collect()
}

/// The whole operand stack, after padding a flexible tail so a quotation can
/// take `min` arguments from the top.
fn take_base(checker: &mut Checker, frame: &mut Frame, min: usize) -> Vec<LocalId> {
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
        Value::Ctor { name, .. } => name == "Void",
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
; Pair a b = Pair a b

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

: unit ( -- Void )
  Void

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
if (pair.value0 !== 1n || pair.value1 !== 2n || pair.values !== undefined) throw new Error("pair fields");
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
"#;
        let code = js(source);
        assert!(
            !code.contains("rot("),
            "shuffle became a call:\n{code}"
        );
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
    fn record_field_is_property_access() {
        assert_eq!(
            js(": nameOf ( { age : Int, name : Str } -- Str )\n  /name"),
            "export function nameOf(v0) {\n  const v1 = v0.name;\n  return v1;\n}\n"
        );
    }

    #[test]
    fn javascript_reads_record_fields() {
        run(
            r#"
; Person = Mk { name : Str, age : Int }

: foreign person ( -- { name : Str, age : Int } )

: nameOf ( { age : Int, name : Str } -- Str )
  /name

: ageOf ( { name : Str, age : Int } -- Int )
  /age

: keepName ( { name : Str } -- { name : Str } Str )
  dup /name

: quoted ( { name : Str } -- Str )
  [ /name ] call

: fromMk ( Person -- Str )
  Mk -> /name

: personName ( -- Str )
  person /name
"#,
            r#"
function person() { return { name: "Ada", age: 36n }; }
const value = { name: "Ada", age: 36n };
if (nameOf(value) !== "Ada") throw new Error("name");
if (ageOf(value) !== 36n) throw new Error("age");
const both = keepName({ name: "Ada" });
if (both[0].name !== "Ada" || both[1] !== "Ada") throw new Error("dup");
if (quoted(value) !== "Ada") throw new Error("quote");
if (fromMk(Mk(value)) !== "Ada") throw new Error("mk");
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
    fn javascript_runs_arrays_and_records() {
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

: person ( -- { name : Str, age : Num } )
  { name: "Ada", age: 36 }

: older ( { name : Str, age : Num } -- { name : Str, age : Num } )
  37 =age

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
if (p.age !== 36 || q.age !== 37 || q.name !== "Ada") throw new Error("record");
const pair = bump(20, 5);
if (pair[0] !== 21 || pair[1] !== 5) throw new Error("dip " + pair);
"#,
        );
    }

    #[test]
    fn javascript_runs_field_insert() {
        let code = js(
            r#"
- Person { name : Str, age : Num }
- PersonWithCity { city : Str | Person }

: inParis ( Person -- PersonWithCity )
  "Paris" +city

: withCity ( Person Str -- PersonWithCity )
  +city

: moved ( Person -- { name : Str, age : Num, city : Str, zip : Str } )
  "Paris" +city "75001" +zip
"#,
        );
        assert!(code.contains("({ ..."), "{code}");
        assert!(code.contains("city:"), "{code}");
        run(
            r#"
- Person { name : Str, age : Num }
- PersonWithCity { city : Str | Person }

: inParis ( Person -- PersonWithCity )
  "Paris" +city

: withCity ( Person Str -- PersonWithCity )
  +city

: moved ( Person -- { name : Str, age : Num, city : Str, zip : Str } )
  "Paris" +city "75001" +zip
"#,
            r#"
const person = { name: "Ada", age: 36 };
const paris = inParis(person);
if (person.city !== undefined || paris.city !== "Paris" || paris.name !== "Ada" || paris.age !== 36) {
  throw new Error("inParis " + JSON.stringify(paris));
}
const london = withCity({ name: "Grace", age: 30 }, "London");
if (london.city !== "London" || london.name !== "Grace" || london.age !== 30) {
  throw new Error("withCity " + JSON.stringify(london));
}
const movedTo = moved({ name: "Ada", age: 36 });
if (movedTo.city !== "Paris" || movedTo.zip !== "75001" || movedTo.name !== "Ada") {
  throw new Error("moved " + JSON.stringify(movedTo));
}
"#,
        );
    }

    #[test]
    fn at_is_an_ordinary_call() {
        let indexed = js(": foreign at ( (Array a) Num -- a )\n\n: second ( (Array Num) -- Num )\n  1 at\n");
        assert!(indexed.contains("at(v"));
        assert!(!indexed.contains("function at"));
        assert!(!indexed.contains("array index out of range"));
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

- Person { name : Str, age : Num }

: olderPerson ( Person -- Person )
  37 =age

; Age == Age Num

: olderAge ( Age -- Age )
  Age -> 1 nadd Age

; Rec == Rec { name : Str, age : Num }

: olderRec ( Rec -- Rec )
  Rec -> 37 =age Rec
"#,
            r#"
function nadd(a, b) { return a + b; }
const person = olderPerson({ name: "Ada", age: 36 });
if (person.name !== "Ada" || person.age !== 37) throw new Error("synonym");
if (Age(36) !== 36) throw new Error("identity");
if (olderAge(36) !== 37) throw new Error("age");
const rec = olderRec({ name: "Ada", age: 36 });
if (rec.name !== "Ada" || rec.age !== 37) throw new Error("newtype record");
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
    fn javascript_runs_record_patterns() {
        let code = js(
            ": nameOf ( { name : Str | r } -- Str )\n  { name : n } -> n\n",
        );
        assert!(code.contains(".name"), "{code}");
        assert!(!code.contains(".value"), "{code}");
        run(
            r#"
; Maybe a = Nothing | Just a

: nameOf ( { name : Str | r } -- Str )
  { name : n } -> n

: born ( { name : Str, year : Int } -- Str )
  { year : 1815n, name : n } -> n
  { name : n } -> n

: parisName ( { who : (Maybe { name : Str, city : Str }) } -- Str )
  { who : (Just { city : "Paris", name : n }) } -> n
  { who : (Just { name : n }) } -> n
  _ -> ""

; Person = Mk { name : Str, age : Int }

: personName ( Person -- Str )
  (Mk { name : n }) -> n

: headName ( { people : (Array { name : Str }) } -- Str )
  { people : < { name : n } > } -> n
  _ -> ""

: touch ( { name : Str | r } -- Bool )
  {} -> true
"#,
            r#"
if (nameOf({ name: "Ada", age: 36 }) !== "Ada") throw new Error("name");
if (born({ name: "Ada", year: 1815n }) !== "Ada") throw new Error("ada");
if (born({ name: "Grace", year: 1906n }) !== "Grace") throw new Error("grace");
const ada = { tag: "Just", value: { name: "Ada", city: "Paris" } };
const grace = { tag: "Just", value: { name: "Grace", city: "London" } };
if (parisName({ who: ada }) !== "Ada") throw new Error("paris");
if (parisName({ who: grace }) !== "Grace") throw new Error("london");
if (parisName({ who: { tag: "Nothing" } }) !== "") throw new Error("nobody");
if (personName({ tag: "Mk", value: { name: "Ada", age: 36 } }) !== "Ada") throw new Error("mk");
if (headName({ people: [{ name: "Ada" }] }) !== "Ada") throw new Error("head");
if (headName({ people: [] }) !== "") throw new Error("no head");
if (headName({ people: [{ name: "Ada" }, { name: "Grace" }] }) !== "") throw new Error("long");
if (touch({ name: "Ada", age: 36 }) !== true) throw new Error("touch");
"#,
        );
    }
}
