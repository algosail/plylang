//! Equation clauses on a word. Patterns line up with the signature:
//! the leftmost pattern is the deepest of the matched slots, the rightmost is the top.
//! A name is a local. It is not on the stack until the body mentions it.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::anf::ShapeTest;
use crate::ast::{Clause, Def, Lit, Pattern};
use crate::codes::Error;
use crate::diagnostic::Diagnostic;
use crate::span::Span;

use super::{Checker, RowId, StackId, StackNode, TyId, TyNode};

pub(super) struct ClauseInfo {
    pub binds: Vec<(String, TyId)>,
    /// Values the pattern puts back, and the constructor a kept value is narrowed to.
    pub pushes: Vec<(TyId, Option<String>)>,
}

/// How `(Name …)` lines up with the constructor's fields.
pub(super) enum FieldMatch {
    /// Zip patterns with fields in declaration order. A nullary `(None)` is this.
    Positional,
    /// Each pattern is a bind. The index is that pattern's field.
    Named(Vec<usize>),
}

pub(super) struct PreparedClauses {
    pub rest: StackId,
    pub slots: Vec<TyId>,
    pub infos: Vec<ClauseInfo>,
}

pub(super) struct CtorShape {
    pub fields: Vec<TyId>,
    pub names: Vec<Option<String>>,
    pub newtype: bool,
    /// Tag stored on the value. A renamed pattern still matches the defining name.
    pub tag: String,
}

/// One field of a grouped constructor pattern.
pub(super) enum SlotPat<'a> {
    Plain(&'a Pattern),
    /// A parenthesized type, then a name, `_`, or `..`.
    Test {
        ty: &'a Pattern,
        bind: &'a Pattern,
    },
}

/// The type the binder has, and the witness to emit. `test` is absent when the
/// field is already that type.
pub(super) struct TypeWitness {
    pub ty: TyId,
    pub test: Option<ShapeTest>,
}

/// What a slot can still be. `Open` is an infinite scalar with some literals removed.
/// `Array` is every length when `open`, except lengths listed in `apart`.
/// An apart length with no rows is covered. `open: false` means only those lengths remain.
/// `Record` lists the fields the type has. A pattern may read fewer of them.
/// `Deep` is a data type the walk did not enter. `Any` is a type the walk did see
/// and does not know, such as a variable; a constructor does not cover `Any`.
/// A child is an [`Rc`], so cloning a node copies one layer. The same type at the
/// same depth is built once. Past [`NEED_DEPTH`], or after [`NEED_NODES`] shapes,
/// the walk stops at `Deep` instead of copying a recursive type down every field.
/// A wildcard covers that slot. A constructor does not, because the other
/// constructors were not built.
#[derive(Clone, Debug)]
enum Need {
    Any,
    /// The walk stopped. A wildcard or a binding covers the slot.
    Deep,
    Bool {
        yes: bool,
        no: bool,
    },
    Open {
        name: String,
        excluded: Vec<Lit>,
    },
    Exact(Lit),
    Data {
        ctors: Vec<CtorNeed>,
    },
    Array {
        elem: Rc<Need>,
        open: bool,
        apart: Vec<Apart>,
    },
    Record {
        fields: Vec<FieldNeed>,
    },
}

/// One label of a record that is still in play. Order is alphabetical.
#[derive(Clone, Debug)]
struct FieldNeed {
    name: String,
    need: Rc<Need>,
}

/// One fixed length that is no longer the plain open element.
#[derive(Clone, Debug)]
struct Apart {
    len: usize,
    rows: Vec<Vec<Need>>,
}

#[derive(Clone, Debug)]
struct CtorNeed {
    name: String,
    /// `None` means this constructor is already covered.
    fields: Option<Vec<Rc<Need>>>,
}

const NEED_DEPTH: usize = 16;
/// Distinct shapes in one coverage walk. A recursive type reuses a shape.
const NEED_NODES: usize = 65_536;
const EXAMPLE_CAP: usize = 8;

/// Identity of a type term. Field instantiations copy a term into a fresh id,
/// so coverage shares nodes by this shape rather than by that id.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum TypeKey {
    Other,
    Ctor(String, Vec<TypeKey>),
    Record(Vec<(String, TypeKey)>),
}

struct NeedMemo {
    nodes: HashMap<(TypeKey, usize), Rc<Need>>,
    keys: HashMap<TyId, TypeKey>,
    built: usize,
}

impl Checker {
    pub(super) fn check_clauses(
        &mut self,
        def: &Def,
        input: StackId,
    ) -> Result<StackId, Diagnostic> {
        let prepared = self.prepare_clauses(&def.clauses, input, def.name_span)?;
        let mut outputs = Vec::new();
        for (clause, info) in def.clauses.iter().zip(&prepared.infos) {
            let mut arm = prepared.rest;
            for (ty, ctor) in &info.pushes {
                arm = self.stack_cons(arm, *ty);
                if let Some(ctor) = ctor {
                    self.note_refine(arm, ctor);
                }
            }
            let mut frame = HashMap::new();
            for (name, ty) in &info.binds {
                frame.insert(name.clone(), self.binding_scheme(*ty));
            }
            self.locals.push(frame);
            let checked = self.check_terms(&clause.body, arm);
            self.locals.pop();
            outputs.push(checked?);
        }
        let mut acc = outputs[0];
        for later in outputs.into_iter().skip(1) {
            self.unify_stack(later, acc)
                .map_err(|message| self.error(def.name_span, message))?;
            acc = self.find_stack(acc);
        }
        Ok(acc)
    }

    /// Type-check every pattern and decide coverage. Both `check` and `javascript`
    /// call this: lowering never goes through `check_bodies`.
    pub(super) fn prepare_clauses(
        &mut self,
        clauses: &[Clause],
        input: StackId,
        report: Span,
    ) -> Result<PreparedClauses, Diagnostic> {
        let count = clauses[0].patterns.len();
        if count == 0 {
            return Err(self.error(clauses[0].span, Error::ExpectedPattern));
        }
        for clause in clauses.iter().skip(1) {
            if clause.patterns.len() != count {
                return Err(self.error(clause.span, Error::ExpectedPatternCount(count)));
            }
        }
        let (rest, slots) = self.take_top(input, count, clauses[0].span)?;
        let mut infos = Vec::new();
        for clause in clauses {
            let mut binds = Vec::new();
            let mut pushes = Vec::new();
            for (pattern, ty) in clause.patterns.iter().zip(&slots) {
                self.pattern_effect(pattern, *ty, clause.span, &mut binds, &mut pushes)?;
            }
            infos.push(ClauseInfo { binds, pushes });
        }
        self.exhaust(clauses, &slots, report)?;
        Ok(PreparedClauses { rest, slots, infos })
    }

    fn take_top(
        &mut self,
        stack: StackId,
        count: usize,
        span: Span,
    ) -> Result<(StackId, Vec<TyId>), Diagnostic> {
        let mut current = stack;
        let mut top_first = Vec::with_capacity(count);
        for _ in 0..count {
            let (rest, ty) = self.pop_slot(current, span)?;
            top_first.push(ty);
            current = rest;
        }
        top_first.reverse();
        Ok((current, top_first))
    }

    /// The name is not a word, and the slot is already off the stack. The note is
    /// the type alone: a hidden tail under it prints as nothing.
    fn note_pattern_bind(&mut self, span: Span, ty: TyId) {
        let tail = self.stack_rigid("");
        let stack = self.stack_cons(tail, ty);
        self.sites.push(super::RawSite {
            span,
            before: stack,
            after: None,
            here: None,
        });
    }

    fn pattern_effect(
        &mut self,
        pattern: &Pattern,
        ty: TyId,
        span: Span,
        binds: &mut Vec<(String, TyId)>,
        pushes: &mut Vec<(TyId, Option<String>)>,
    ) -> Result<(), Diagnostic> {
        match pattern {
            Pattern::Wild => Ok(()),
            Pattern::Spill => Err(self.error(span, Error::ExpectedPattern)),
            Pattern::Bind(name_span, name) => {
                if binds.iter().any(|(bound, _)| bound == name) {
                    return Err(self.error(span, Error::DuplicateDefinition(name.to_string())));
                }
                binds.push((name.clone(), ty));
                self.note_pattern_bind(*name_span, ty);
                Ok(())
            }
            Pattern::Lit(lit) => {
                let need = self.lit_ty(lit);
                self.unify_ty(ty, need)
                    .map_err(|message| self.error(span, message))?;
                Ok(())
            }
            Pattern::Unpack(name) => {
                let shape = self.ctor_shape(name, ty, span)?;
                pushes.push((ty, Some(shape.tag)));
                Ok(())
            }
            Pattern::Open(name) => {
                let shape = self.ctor_shape(name, ty, span)?;
                for field in shape.fields {
                    pushes.push((field, None));
                }
                Ok(())
            }
            Pattern::Ctor(name, patterns) => {
                let shape = self.ctor_shape(name, ty, span)?;
                if let Some(groups) = group_type_tests(patterns, shape.fields.len()) {
                    if self.type_test_heads(&groups) {
                        return self.effect_type_tests(&shape, &groups, span, binds, pushes);
                    }
                }
                match self.ctor_match(name, &shape, patterns, span)? {
                    FieldMatch::Positional => {
                        for (pattern, field) in patterns.iter().zip(&shape.fields) {
                            self.pattern_effect(pattern, *field, span, binds, pushes)?;
                        }
                    }
                    FieldMatch::Named(indexes) => {
                        for (pattern, index) in patterns.iter().zip(&indexes) {
                            self.pattern_effect(
                                pattern,
                                shape.fields[*index],
                                span,
                                binds,
                                pushes,
                            )?;
                        }
                    }
                }
                Ok(())
            }
            Pattern::Array(patterns) => {
                let elem = self.array_elem(ty, span)?;
                for pattern in patterns {
                    self.pattern_effect(pattern, elem, span, binds, pushes)?;
                }
                Ok(())
            }
            Pattern::Record(fields) => {
                let types = self.open_record_pattern(fields, ty, span)?;
                for (pattern, field_ty) in fields.iter().zip(types) {
                    self.pattern_effect(&pattern.1, field_ty, span, binds, pushes)?;
                }
                Ok(())
            }
        }
    }

    /// Field types of a record pattern. The pattern row is open, so a closed
    /// slot may keep labels the pattern does not name. A missing label is
    /// `record has no field NAME`.
    pub(super) fn open_record_pattern(
        &mut self,
        fields: &[(String, Pattern)],
        ty: TyId,
        span: Span,
    ) -> Result<Vec<TyId>, Diagnostic> {
        let mut pairs = Vec::with_capacity(fields.len());
        let mut ids = Vec::with_capacity(fields.len());
        for (name, _) in fields {
            let field_ty = self.ty_var();
            pairs.push((name.clone(), field_ty));
            ids.push(field_ty);
        }
        let tail = self.row_var();
        let row = self.row_fields(pairs, Some(tail));
        let record = self.ty_record(row);
        self.unify_ty(ty, record)
            .map_err(|message| self.error(span, message))?;
        Ok(ids.into_iter().map(|id| self.find_ty(id)).collect())
    }

    /// Element type of an array pattern. A flexible slot becomes `(Array _)`.
    pub(super) fn array_elem(&mut self, ty: TyId, span: Span) -> Result<TyId, Diagnostic> {
        let found = self.expose(ty);
        if let TyNode::Ctor(name, args) = self.tys[found.0 as usize].clone() {
            if name == "Array" && args.len() == 1 {
                return Ok(args[0]);
            }
        }
        let elem = self.ty_var();
        let arr = self.ty_ctor("Array", vec![elem]);
        self.unify_ty(ty, arr)
            .map_err(|message| self.error(span, message))?;
        Ok(self.find_ty(elem))
    }

    /// Field types of `name` applied to `ty`, the field names, and whether the data is a newtype.
    pub(super) fn ctor_shape(
        &mut self,
        name: &str,
        ty: TyId,
        span: Span,
    ) -> Result<CtorShape, Diagnostic> {
        let ty = self.expose(ty);
        let (data_name, args) = match self.tys[ty.0 as usize].clone() {
            TyNode::Ctor(data_name, args) => (data_name, args),
            _ => {
                return Err(self.error(span, Error::ExpectedDataType(self.show_ty(ty))));
            }
        };
        let Some(data) = self.datas.get(&data_name).cloned() else {
            return Err(self.error(span, Error::ExpectedDataType(self.show_ty(ty))));
        };
        if args.len() != data.params.len() {
            return Err(self.error(span, Error::ExpectedDataType(self.show_ty(ty))));
        }
        let written = name.to_string();
        let name = self.canonical(name).to_string();
        let Some(ctor) = data.ctors.iter().find(|ctor| ctor.name == name) else {
            return Err(self.error(
                span,
                Error::ConstructorNotOf {
                    name: written,
                    type_name: data_name.to_string(),
                },
            ));
        };
        if let Err(error) = self.constructor_open(&data_name, &written) {
            return Err(self.error(span, error));
        }
        let fields = self.instantiate_fields(&data.params, &args, &ctor.fields);
        Ok(CtorShape {
            fields,
            names: ctor.names.clone(),
            newtype: data.newtype,
            tag: name,
        })
    }

    /// `(Name)` with fields is an error. A full list of patterns is positional,
    /// including derive's `(Person l0 l1)` and `(Sent _ headers _ _ _)`.
    /// A full list of distinct field names, and any shorter list of names, binds
    /// those fields. Order does not matter. An unknown name is `no field`.
    pub(super) fn ctor_match(
        &self,
        name: &str,
        shape: &CtorShape,
        patterns: &[Pattern],
        span: Span,
    ) -> Result<FieldMatch, Diagnostic> {
        if patterns.is_empty() {
            if shape.fields.is_empty() {
                return Ok(FieldMatch::Positional);
            }
            return Err(self.error(span, Error::PatternForm(name.to_string())));
        }
        // `..` here was not the binder of a type test. `(Pair (Num) ..)` is this.
        if patterns
            .iter()
            .any(|pattern| matches!(pattern, Pattern::Spill))
        {
            return Err(self.error(span, Error::PatternForm(name.to_string())));
        }
        let all_binds = patterns
            .iter()
            .all(|pattern| matches!(pattern, Pattern::Bind(..)));
        let all_named = shape.names.iter().all(|field| field.is_some());
        let by_name = all_binds
            && all_named
            && (patterns.len() != shape.fields.len() || binds_name_fields(patterns, &shape.names));
        if by_name {
            let mut indexes = Vec::with_capacity(patterns.len());
            for pattern in patterns {
                let Pattern::Bind(_, field) = pattern else {
                    return Err(self.error(span, Error::PatternForm(name.to_string())));
                };
                let Some(index) = shape
                    .names
                    .iter()
                    .position(|have| have.as_deref() == Some(field.as_str()))
                else {
                    return Err(self.error(
                        span,
                        Error::NoSuchField {
                            type_name: name.to_string(),
                            name: field.clone(),
                        },
                    ));
                };
                indexes.push(index);
            }
            return Ok(FieldMatch::Named(indexes));
        }
        if patterns.len() == shape.fields.len() {
            return Ok(FieldMatch::Positional);
        }
        if all_binds {
            let count = shape.fields.len();
            return Err(self.error(
                span,
                Error::PatternArity {
                    name: name.to_string(),
                    count,
                },
            ));
        }
        Err(self.error(span, Error::PatternForm(name.to_string())))
    }

    /// `(Some (Array Num) items)` binds `items` at `(Array Num)`. The field type
    /// stays as it was: a variable is not unified with the pattern.
    fn effect_type_tests(
        &mut self,
        shape: &CtorShape,
        groups: &[SlotPat],
        span: Span,
        binds: &mut Vec<(String, TyId)>,
        pushes: &mut Vec<(TyId, Option<String>)>,
    ) -> Result<(), Diagnostic> {
        for (index, group) in groups.iter().enumerate() {
            match group {
                SlotPat::Plain(pattern) => {
                    self.pattern_effect(pattern, shape.fields[index], span, binds, pushes)?;
                }
                SlotPat::Test { ty, bind } => {
                    let witness = self.check_type_test(ty, shape.fields[index], span)?;
                    self.bind_type_test(bind, witness.ty, span, binds, pushes)?;
                }
            }
        }
        Ok(())
    }

    /// A parenthesized type, then the name of that value. `None` when the field
    /// is already that type and no runtime test is needed.
    pub(super) fn check_type_test(
        &mut self,
        pattern: &Pattern,
        field: TyId,
        span: Span,
    ) -> Result<TypeWitness, Diagnostic> {
        let ty = self.pattern_type(pattern, span)?;
        if !self.types_overlap(field, ty) {
            return Err(self.error(span, self.ty_mismatch(ty, field)));
        }
        if self.same_ground(field, ty) {
            return Ok(TypeWitness { ty, test: None });
        }
        match self.shape_of_ty(ty) {
            Some(test) => Ok(TypeWitness {
                ty,
                test: Some(test),
            }),
            None => Err(self.error(span, Error::PatternCannotTest(self.show_ty(ty)))),
        }
    }

    fn bind_type_test(
        &mut self,
        bind: &Pattern,
        ty: TyId,
        span: Span,
        binds: &mut Vec<(String, TyId)>,
        pushes: &mut Vec<(TyId, Option<String>)>,
    ) -> Result<(), Diagnostic> {
        match bind {
            Pattern::Wild => Ok(()),
            Pattern::Spill => {
                pushes.push((ty, None));
                Ok(())
            }
            Pattern::Bind(name_span, name) => {
                if binds.iter().any(|(bound, _)| bound == name) {
                    return Err(self.error(span, Error::DuplicateDefinition(name.to_string())));
                }
                binds.push((name.clone(), ty));
                self.note_pattern_bind(*name_span, ty);
                Ok(())
            }
            _ => unreachable!("a type test binder is a name, _, or .."),
        }
    }

    /// The head of every type test is a type. A value constructor falls through
    /// to the ordinary pattern, so `(Some (Right text) extra)` stays an error.
    pub(super) fn type_test_heads(&self, groups: &[SlotPat]) -> bool {
        groups.iter().all(|group| match group {
            SlotPat::Plain(_) => true,
            SlotPat::Test { ty, .. } => match ty {
                Pattern::Ctor(name, _) | Pattern::Unpack(name) => self.type_known(name),
                _ => false,
            },
        })
    }

    fn type_known(&self, name: &str) -> bool {
        self.type_arity.contains_key(name)
            || self.extra_arity.contains_key(name)
            || (self.hidden_type_visible()
                && (self.datas.contains_key(name) || self.synonyms.contains_key(name)))
    }

    fn pattern_type(&mut self, pattern: &Pattern, span: Span) -> Result<TyId, Diagnostic> {
        match pattern {
            Pattern::Bind(..) | Pattern::Wild => Ok(self.ty_var()),
            Pattern::Unpack(name) => self.pattern_ctor(name, Vec::new(), span),
            Pattern::Ctor(name, args) => {
                let mut ids = Vec::with_capacity(args.len());
                for arg in args {
                    ids.push(self.pattern_type(arg, span)?);
                }
                self.pattern_ctor(name, ids, span)
            }
            _ => Err(self.error(span, Error::PatternCannotTest("that pattern".into()))),
        }
    }

    fn pattern_ctor(
        &mut self,
        name: &str,
        args: Vec<TyId>,
        span: Span,
    ) -> Result<TyId, Diagnostic> {
        let arity = self.arity_of(name, span)?;
        if args.len() != arity {
            return Err(self.arity_error(span, name, arity));
        }
        if self.syn_in_scope(name) {
            let (id, kind) = self.expand_synonym(name, &args, span)?;
            self.demand_type(id, kind, span)?;
            return Ok(id);
        }
        let canon = self.canonical(name).to_string();
        Ok(self.ty_ctor(&canon, args))
    }

    /// A variable on either side still overlaps. This does not bind it.
    fn types_overlap(&self, left: TyId, right: TyId) -> bool {
        self.overlap_ty(left, right, &mut HashSet::new())
    }

    fn overlap_ty(&self, left: TyId, right: TyId, seen: &mut HashSet<(u32, u32)>) -> bool {
        let left = self.expose(left);
        let right = self.expose(right);
        if left == right {
            return true;
        }
        let key = if left.0 < right.0 {
            (left.0, right.0)
        } else {
            (right.0, left.0)
        };
        if !seen.insert(key) {
            return true;
        }
        match (&self.tys[left.0 as usize], &self.tys[right.0 as usize]) {
            (TyNode::Var | TyNode::Rigid(_) | TyNode::Template, _)
            | (_, TyNode::Var | TyNode::Rigid(_) | TyNode::Template) => true,
            (TyNode::Ctor(left_name, left_args), TyNode::Ctor(right_name, right_args)) => {
                left_name == right_name
                    && left_args.len() == right_args.len()
                    && left_args
                        .iter()
                        .zip(right_args)
                        .all(|(left, right)| self.overlap_ty(*left, *right, seen))
            }
            _ => false,
        }
    }

    fn same_ground(&self, left: TyId, right: TyId) -> bool {
        self.type_is_ground(left) && self.type_is_ground(right) && self.congruent_ty(left, right)
    }

    fn type_is_ground(&self, ty: TyId) -> bool {
        self.ground_ty(ty, &mut HashSet::new())
    }

    fn ground_ty(&self, ty: TyId, seen: &mut HashSet<u32>) -> bool {
        let ty = self.expose(ty);
        if !seen.insert(ty.0) {
            return true;
        }
        match &self.tys[ty.0 as usize] {
            TyNode::Var | TyNode::Rigid(_) | TyNode::Template => false,
            TyNode::Ctor(_, args) => args.iter().all(|arg| self.ground_ty(*arg, seen)),
            TyNode::App(head, args) => {
                self.ground_ty(*head, seen) && args.iter().all(|arg| self.ground_ty(*arg, seen))
            }
            TyNode::Fun(input, output, _) => {
                self.ground_stack(*input, seen) && self.ground_stack(*output, seen)
            }
            TyNode::Record(row) => self.ground_row(*row, seen),
            TyNode::Link(_) | TyNode::Alias { .. } => true,
        }
    }

    fn ground_stack(&self, stack: StackId, seen: &mut HashSet<u32>) -> bool {
        match &self.stacks[self.find_stack(stack).0 as usize] {
            StackNode::Cons(rest, ty) => {
                self.ground_ty(*ty, seen) && self.ground_stack(*rest, seen)
            }
            StackNode::Empty => true,
            StackNode::Var | StackNode::Rigid { .. } | StackNode::Template | StackNode::Link(_) => {
                false
            }
        }
    }

    fn ground_row(&self, row: RowId, seen: &mut HashSet<u32>) -> bool {
        match self.peel_row(row) {
            Ok((fields, None)) => fields.iter().all(|(_, field)| self.ground_ty(*field, seen)),
            _ => false,
        }
    }

    /// `Num`, `Int`, `Str`, `Bool`, and arrays of those. A variable element is any array.
    fn shape_of_ty(&self, ty: TyId) -> Option<ShapeTest> {
        let ty = self.expose(ty);
        match &self.tys[ty.0 as usize] {
            TyNode::Ctor(name, args) if args.is_empty() => scalar_shape(name),
            TyNode::Ctor(name, args) if name == "Array" && args.len() == 1 => {
                let elem = self.expose(args[0]);
                match &self.tys[elem.0 as usize] {
                    TyNode::Var | TyNode::Rigid(_) => Some(ShapeTest::Array(None)),
                    _ => Some(ShapeTest::Array(Some(Box::new(self.shape_of_ty(elem)?)))),
                }
            }
            _ => None,
        }
    }

    fn exhaust(
        &mut self,
        clauses: &[Clause],
        slots: &[TyId],
        report: Span,
    ) -> Result<(), Diagnostic> {
        let clauses = self.canonical_clauses(clauses);
        let mut memo = NeedMemo {
            nodes: HashMap::new(),
            keys: HashMap::new(),
            built: 0,
        };
        let mut rows = vec![slots
            .iter()
            .map(|ty| self.need_of(*ty, 0, &mut memo))
            .collect::<Vec<_>>()];
        for clause in clauses {
            if !rows.iter().any(|row| row_hit(row, &clause.patterns)) {
                return Err(self.error(clause.span, Error::RedundantPattern));
            }
            let mut next = Vec::new();
            for row in rows {
                if row_hit(&row, &clause.patterns) {
                    next.extend(subtract_row(&row, &clause.patterns));
                } else {
                    next.push(row);
                }
            }
            rows = next;
        }
        if rows.is_empty() {
            return Ok(());
        }
        let examples = explain(&rows);
        let error = if examples.is_empty() {
            Error::NonExhaustive
        } else {
            Error::NonExhaustiveExamples(examples)
        };
        Err(self.error(report, error))
    }

    /// Exhaustiveness compares pattern names with the constructors of the data type.
    fn canonical_clauses(&self, clauses: &[Clause]) -> Vec<Clause> {
        clauses
            .iter()
            .map(|clause| {
                let mut clause = clause.clone();
                for pattern in &mut clause.patterns {
                    self.canonical_pattern(pattern);
                }
                clause
            })
            .collect()
    }

    fn canonical_pattern(&self, pattern: &mut Pattern) {
        match pattern {
            Pattern::Unpack(name) | Pattern::Open(name) => {
                *name = self.canonical(name).to_string();
            }
            Pattern::Ctor(name, fields) => {
                *name = self.canonical(name).to_string();
                for field in fields {
                    self.canonical_pattern(field);
                }
            }
            Pattern::Array(items) => {
                for item in items {
                    self.canonical_pattern(item);
                }
            }
            Pattern::Record(fields) => {
                for (_, field) in fields {
                    self.canonical_pattern(field);
                }
            }
            Pattern::Wild | Pattern::Spill | Pattern::Bind(_, _) | Pattern::Lit(_) => {}
        }
    }

    /// One shape per type structure and depth. A later field of the same type shares it.
    fn need_of(&mut self, ty: TyId, depth: usize, memo: &mut NeedMemo) -> Need {
        let ty = self.expose(ty);
        let key = self.type_key(ty, &mut memo.keys);
        if let Some(found) = memo.nodes.get(&(key.clone(), depth)) {
            return (**found).clone();
        }
        if depth > NEED_DEPTH || memo.built >= NEED_NODES {
            let deep = Need::Deep;
            memo.nodes.insert((key, depth), Rc::new(Need::Deep));
            return deep;
        }
        memo.built += 1;
        let built = self.need_shape(ty, depth, memo);
        memo.nodes.insert((key, depth), Rc::new(built.clone()));
        built
    }

    fn type_key(&self, ty: TyId, keys: &mut HashMap<TyId, TypeKey>) -> TypeKey {
        let ty = self.expose(ty);
        if let Some(found) = keys.get(&ty) {
            return found.clone();
        }
        keys.insert(ty, TypeKey::Other);
        let built = match self.tys[ty.0 as usize].clone() {
            TyNode::Ctor(name, args) => {
                let args = args
                    .into_iter()
                    .map(|arg| self.type_key(arg, keys))
                    .collect();
                TypeKey::Ctor(name, args)
            }
            TyNode::Record(row) => match self.peel_row(row) {
                Ok((fields, _)) => {
                    let fields = fields
                        .into_iter()
                        .map(|(name, field)| (name, self.type_key(field, keys)))
                        .collect();
                    TypeKey::Record(fields)
                }
                Err(_) => TypeKey::Other,
            },
            _ => TypeKey::Other,
        };
        keys.insert(ty, built.clone());
        built
    }

    fn need_shape(&mut self, ty: TyId, depth: usize, memo: &mut NeedMemo) -> Need {
        if let TyNode::Record(row) = self.tys[ty.0 as usize].clone() {
            return self.record_need(row, depth, memo);
        }
        let TyNode::Ctor(name, args) = self.tys[ty.0 as usize].clone() else {
            return Need::Any;
        };
        if args.is_empty() {
            match name.as_str() {
                "Bool" => {
                    return Need::Bool {
                        yes: true,
                        no: true,
                    }
                }
                "Int" => {
                    return Need::Open {
                        name: "Int".to_string(),
                        excluded: Vec::new(),
                    };
                }
                "Num" => {
                    return Need::Open {
                        name: "Num".to_string(),
                        excluded: Vec::new(),
                    };
                }
                "Str" => {
                    return Need::Open {
                        name: "Str".to_string(),
                        excluded: Vec::new(),
                    };
                }
                _ => {}
            }
        }
        if name == "Array" && args.len() == 1 {
            return Need::Array {
                elem: Rc::new(self.need_of(args[0], depth + 1, memo)),
                open: true,
                apart: Vec::new(),
            };
        }
        let Some(data) = self.datas.get(&name).cloned() else {
            return Need::Any;
        };
        if args.len() != data.params.len() {
            return Need::Any;
        }
        let mut ctors = Vec::new();
        for ctor in &data.ctors {
            let fields = self.instantiate_fields(&data.params, &args, &ctor.fields);
            let mut needs = Vec::new();
            for field in fields {
                needs.push(Rc::new(self.need_of(field, depth + 1, memo)));
            }
            ctors.push(CtorNeed {
                name: ctor.name.clone(),
                fields: Some(needs),
            });
        }
        Need::Data { ctors }
    }

    fn record_need(&mut self, row: super::RowId, depth: usize, memo: &mut NeedMemo) -> Need {
        let Ok((fields, _)) = self.peel_row(row) else {
            return Need::Any;
        };
        let mut out = Vec::with_capacity(fields.len());
        for (name, field_ty) in fields {
            out.push(FieldNeed {
                name,
                need: Rc::new(self.need_of(field_ty, depth + 1, memo)),
            });
        }
        Need::Record { fields: out }
    }
}

/// `(Some (Array Num) items)` is two patterns for one field: a parenthesized
/// type, then a name, `_`, or `..`. Grouping runs only when the counts differ, so
/// `(Person (Some name) age)` stays two positional fields.
pub(super) fn group_type_tests<'a>(
    patterns: &'a [Pattern],
    slots: usize,
) -> Option<Vec<SlotPat<'a>>> {
    if patterns.len() == slots {
        return None;
    }
    let grouped = partition_slots(patterns, slots)?;
    if grouped
        .iter()
        .any(|slot| matches!(slot, SlotPat::Test { .. }))
    {
        Some(grouped)
    } else {
        None
    }
}

fn partition_slots<'a>(patterns: &'a [Pattern], slots: usize) -> Option<Vec<SlotPat<'a>>> {
    if slots == 0 {
        return if patterns.is_empty() {
            Some(Vec::new())
        } else {
            None
        };
    }
    if patterns.len() < slots {
        return None;
    }
    if patterns.len() > slots {
        if let Some(head) = type_test_at(patterns) {
            if let Some(mut rest) = partition_slots(&patterns[2..], slots - 1) {
                rest.insert(0, head);
                return Some(rest);
            }
        }
    }
    let mut rest = partition_slots(&patterns[1..], slots - 1)?;
    rest.insert(0, SlotPat::Plain(&patterns[0]));
    Some(rest)
}

fn type_test_at<'a>(patterns: &'a [Pattern]) -> Option<SlotPat<'a>> {
    let (head, tail) = patterns.split_first()?;
    let Pattern::Ctor(_, _) = head else {
        return None;
    };
    let (bind, _) = tail.split_first()?;
    if matches!(bind, Pattern::Bind(..) | Pattern::Wild | Pattern::Spill) {
        Some(SlotPat::Test { ty: head, bind })
    } else {
        None
    }
}

/// Every bind is a different field of this constructor.
fn binds_name_fields(patterns: &[Pattern], names: &[Option<String>]) -> bool {
    let mut seen = std::collections::HashSet::new();
    for pattern in patterns {
        let Pattern::Bind(_, name) = pattern else {
            return false;
        };
        if !names
            .iter()
            .any(|field| field.as_deref() == Some(name.as_str()))
        {
            return false;
        }
        if !seen.insert(name.as_str()) {
            return false;
        }
    }
    true
}

fn row_hit(row: &[Need], patterns: &[Pattern]) -> bool {
    patterns
        .iter()
        .zip(row)
        .all(|(pattern, need)| overlaps(pattern, need))
}

fn overlaps(pattern: &Pattern, need: &Need) -> bool {
    if matches!(need, Need::Any) {
        return true;
    }
    match pattern {
        Pattern::Wild | Pattern::Bind(..) => true,
        Pattern::Spill => false,
        Pattern::Lit(Lit::Bool(true)) => match need {
            Need::Bool { yes, .. } => *yes,
            Need::Exact(Lit::Bool(true)) => true,
            _ => false,
        },
        Pattern::Lit(Lit::Bool(false)) => match need {
            Need::Bool { no, .. } => *no,
            Need::Exact(Lit::Bool(false)) => true,
            _ => false,
        },
        Pattern::Lit(lit) => match need {
            Need::Open { name, excluded } => {
                lit_name(lit) == Some(name.as_str()) && !excluded.iter().any(|item| item == lit)
            }
            Need::Exact(other) => other == lit,
            _ => false,
        },
        Pattern::Unpack(name) | Pattern::Open(name) => live_ctor(need, name).is_some(),
        Pattern::Ctor(name, patterns) => {
            let Some(fields) = live_ctor(need, name) else {
                return false;
            };
            if let Some(groups) = group_type_tests(patterns, fields.len()) {
                return groups
                    .iter()
                    .zip(fields)
                    .all(|(group, field)| group_overlaps(group, field));
            }
            // A named subset covers the constructor. Its binds are irrefutable.
            if patterns.len() != fields.len() {
                return true;
            }
            patterns
                .iter()
                .zip(fields)
                .all(|(pattern, field)| overlaps(pattern, field))
        }
        Pattern::Array(patterns) => match need {
            Need::Array { elem, open, apart } => match part_of(apart, patterns.len()) {
                Some(part) => part.rows.iter().any(|row| row_hit(row, patterns)),
                None => *open && patterns.iter().all(|pattern| overlaps(pattern, elem)),
            },
            _ => false,
        },
        Pattern::Record(fields) => match need {
            Need::Record { fields: have } => fields.iter().all(|(name, pattern)| {
                have.iter()
                    .find(|field| field.name == *name)
                    .is_some_and(|field| overlaps(pattern, &field.need))
            }),
            _ => false,
        },
    }
}

fn live_ctor<'a>(need: &'a Need, name: &str) -> Option<&'a [Rc<Need>]> {
    let Need::Data { ctors } = need else {
        return None;
    };
    ctors.iter().find_map(|ctor| {
        if ctor.name == name {
            ctor.fields.as_deref()
        } else {
            None
        }
    })
}

/// Rows that remain after this pattern. An empty result means the row is covered.
/// The first slot splits into "missed by this pattern" and "hit, then the tail".
fn subtract_row(row: &[Need], patterns: &[Pattern]) -> Vec<Vec<Need>> {
    if patterns.is_empty() {
        return Vec::new();
    }
    if !overlaps(&patterns[0], &row[0]) {
        return vec![row.to_vec()];
    }
    let mut out = Vec::new();
    for left in subtract_slot(&row[0], &patterns[0]) {
        let mut next = vec![left];
        next.extend(row[1..].iter().cloned());
        out.push(next);
    }
    if let Some(covered) = covered_slot(&patterns[0], &row[0]) {
        for tail in subtract_row(&row[1..], &patterns[1..]) {
            let mut next = vec![covered.clone()];
            next.extend(tail);
            out.push(next);
        }
    }
    out
}

fn subtract_slot(need: &Need, pattern: &Pattern) -> Vec<Need> {
    if !overlaps(pattern, need) {
        return vec![need.clone()];
    }
    match pattern {
        Pattern::Wild | Pattern::Bind(..) => Vec::new(),
        Pattern::Spill => vec![need.clone()],
        _ if matches!(need, Need::Any) => vec![Need::Any],
        Pattern::Lit(Lit::Bool(true)) => match need {
            Need::Bool { no: true, .. } => vec![Need::Bool {
                yes: false,
                no: true,
            }],
            _ => Vec::new(),
        },
        Pattern::Lit(Lit::Bool(false)) => match need {
            Need::Bool { yes: true, .. } => vec![Need::Bool {
                yes: true,
                no: false,
            }],
            _ => Vec::new(),
        },
        Pattern::Lit(lit) => match need {
            Need::Open { name, excluded } => {
                let mut excluded = excluded.clone();
                excluded.push(lit.clone());
                vec![Need::Open {
                    name: name.clone(),
                    excluded,
                }]
            }
            Need::Exact(_) => Vec::new(),
            _ => vec![need.clone()],
        },
        Pattern::Unpack(name) | Pattern::Open(name) => without_ctor(need, name),
        Pattern::Ctor(name, patterns) => match need {
            Need::Data { ctors } => {
                let mut others = Vec::new();
                let mut target = None;
                for ctor in ctors {
                    if ctor.name == *name && ctor.fields.is_some() && target.is_none() {
                        target = Some(ctor.clone());
                    } else if ctor.fields.is_some() {
                        others.push(ctor.clone());
                    }
                }
                let Some(target) = target else {
                    return vec![need.clone()];
                };
                let Some(fields) = target.fields else {
                    return vec![need.clone()];
                };
                if let Some(groups) = group_type_tests(patterns, fields.len()) {
                    return subtract_grouped(need, name, &fields, &groups, others);
                }
                if patterns.len() != fields.len() {
                    return without_ctor(need, name);
                }
                assemble(name, others, subtract_fields(&fields, patterns))
            }
            _ => vec![need.clone()],
        },
        Pattern::Array(patterns) => subtract_array(need, patterns),
        Pattern::Record(fields) => subtract_record(need, fields),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rel {
    Miss,
    Partial,
    Hit,
}

fn scalar_shape(name: &str) -> Option<ShapeTest> {
    match name {
        "Num" => Some(ShapeTest::Num),
        "Int" => Some(ShapeTest::Int),
        "Str" => Some(ShapeTest::Str),
        "Bool" => Some(ShapeTest::Bool),
        _ => None,
    }
}

fn shape_of_pattern(pattern: &Pattern) -> Option<ShapeTest> {
    match pattern {
        Pattern::Ctor(name, args) if name == "Array" && args.len() == 1 => {
            if matches!(args[0], Pattern::Bind(..) | Pattern::Wild) {
                Some(ShapeTest::Array(None))
            } else {
                Some(ShapeTest::Array(Some(Box::new(shape_of_pattern(
                    &args[0],
                )?))))
            }
        }
        Pattern::Ctor(name, args) if args.is_empty() => scalar_shape(name),
        Pattern::Unpack(name) => scalar_shape(name),
        _ => None,
    }
}

fn type_rel(pattern: &Pattern, need: &Need) -> Rel {
    match shape_of_pattern(pattern) {
        Some(shape) => shape_rel(&shape, need),
        // A data type is accepted only when the field is already that type.
        None => match need {
            Need::Any | Need::Deep => Rel::Partial,
            _ => Rel::Hit,
        },
    }
}

fn shape_rel(shape: &ShapeTest, need: &Need) -> Rel {
    match (shape, need) {
        (_, Need::Any | Need::Deep) => Rel::Partial,
        (ShapeTest::Num, Need::Open { name, .. }) if name == "Num" => Rel::Hit,
        (ShapeTest::Num, Need::Exact(Lit::Number(_))) => Rel::Hit,
        (ShapeTest::Int, Need::Open { name, .. }) if name == "Int" => Rel::Hit,
        (ShapeTest::Int, Need::Exact(Lit::Int(_))) => Rel::Hit,
        (ShapeTest::Str, Need::Open { name, .. }) if name == "Str" => Rel::Hit,
        (ShapeTest::Str, Need::Exact(Lit::String(_))) => Rel::Hit,
        (ShapeTest::Bool, Need::Bool { .. } | Need::Exact(Lit::Bool(_))) => Rel::Hit,
        (ShapeTest::Array(None), Need::Array { .. }) => Rel::Hit,
        (
            ShapeTest::Array(Some(elem)),
            Need::Array {
                elem: need_elem, ..
            },
        ) => shape_rel(elem, need_elem),
        _ => Rel::Miss,
    }
}

fn group_overlaps(group: &SlotPat, field: &Need) -> bool {
    match group {
        SlotPat::Plain(pattern) => overlaps(pattern, field),
        SlotPat::Test { ty, .. } => type_rel(ty, field) != Rel::Miss,
    }
}

/// The test can fail, so the constructor stays in the uncovered set.
fn group_partial_test(group: &SlotPat, field: &Need) -> bool {
    match group {
        SlotPat::Test { ty, .. } => type_rel(ty, field) == Rel::Partial,
        SlotPat::Plain(_) => false,
    }
}

fn slot_as_cover(group: &SlotPat) -> Pattern {
    match group {
        SlotPat::Plain(pattern) => (*pattern).clone(),
        SlotPat::Test { .. } => Pattern::Wild,
    }
}

fn subtract_grouped(
    need: &Need,
    name: &str,
    fields: &[Rc<Need>],
    groups: &[SlotPat],
    others: Vec<CtorNeed>,
) -> Vec<Need> {
    if groups
        .iter()
        .zip(fields)
        .any(|(group, field)| !group_overlaps(group, field))
    {
        return vec![need.clone()];
    }
    if groups
        .iter()
        .zip(fields)
        .any(|(group, field)| group_partial_test(group, field))
    {
        return vec![need.clone()];
    }
    let flat: Vec<Pattern> = groups.iter().map(slot_as_cover).collect();
    assemble(name, others, subtract_fields(fields, &flat))
}

fn subtract_fields(fields: &[Rc<Need>], patterns: &[Pattern]) -> Vec<Vec<Need>> {
    let owned: Vec<Need> = fields.iter().map(|field| (**field).clone()).collect();
    subtract_row(&owned, patterns)
}

fn assemble(name: &str, others: Vec<CtorNeed>, remainders: Vec<Vec<Need>>) -> Vec<Need> {
    let mut out = Vec::new();
    if !others.is_empty() {
        out.push(Need::Data { ctors: others });
    }
    for remainder in remainders {
        out.push(Need::Data {
            ctors: vec![CtorNeed {
                name: name.to_string(),
                fields: Some(remainder.into_iter().map(Rc::new).collect()),
            }],
        });
    }
    out
}

fn covered_fields(name: &str, fields: &[Rc<Need>], patterns: &[Pattern]) -> Option<Need> {
    let mut covered = Vec::new();
    for (pattern, field) in patterns.iter().zip(fields) {
        covered.push(Rc::new(covered_slot(pattern, field)?));
    }
    Some(Need::Data {
        ctors: vec![CtorNeed {
            name: name.to_string(),
            fields: Some(covered),
        }],
    })
}

fn without_ctor(need: &Need, name: &str) -> Vec<Need> {
    let Need::Data { ctors } = need else {
        return vec![need.clone()];
    };
    let left: Vec<_> = ctors
        .iter()
        .filter(|ctor| ctor.name != name && ctor.fields.is_some())
        .cloned()
        .collect();
    if left.is_empty() {
        Vec::new()
    } else {
        vec![Need::Data { ctors: left }]
    }
}

fn covered_ctor(need: &Need, name: &str) -> Option<Need> {
    let Need::Data { ctors } = need else {
        return None;
    };
    let ctor = ctors
        .iter()
        .find(|ctor| ctor.name == name && ctor.fields.is_some())?;
    Some(Need::Data {
        ctors: vec![ctor.clone()],
    })
}

fn part_of(apart: &[Apart], len: usize) -> Option<&Apart> {
    apart.iter().find(|part| part.len == len)
}

/// Element rows of this length. `None` when a closed set of lengths does not include it.
fn rows_for(elem: &Need, open: bool, apart: &[Apart], len: usize) -> Option<Vec<Vec<Need>>> {
    if let Some(part) = part_of(apart, len) {
        return Some(part.rows.clone());
    }
    if open {
        Some(vec![vec![elem.clone(); len]])
    } else {
        None
    }
}

fn subtract_array(need: &Need, patterns: &[Pattern]) -> Vec<Need> {
    let Need::Array { elem, open, apart } = need else {
        return vec![need.clone()];
    };
    let len = patterns.len();
    let Some(base) = rows_for(elem, *open, apart, len) else {
        return vec![need.clone()];
    };
    let remainders: Vec<Vec<Need>> = base
        .iter()
        .flat_map(|row| subtract_row(row, patterns))
        .collect();
    let mut next: Vec<Apart> = apart
        .iter()
        .filter(|part| part.len != len)
        .cloned()
        .collect();
    next.push(Apart {
        len,
        rows: remainders,
    });
    if !*open && next.iter().all(|part| part.rows.is_empty()) {
        return Vec::new();
    }
    vec![Need::Array {
        elem: elem.clone(),
        open: *open,
        apart: next,
    }]
}

fn covered_slot(pattern: &Pattern, need: &Need) -> Option<Need> {
    match pattern {
        Pattern::Wild | Pattern::Bind(..) => Some(need.clone()),
        Pattern::Spill => None,
        _ if matches!(need, Need::Any) => None,
        Pattern::Lit(Lit::Bool(true)) => match need {
            Need::Bool { yes: true, .. } => Some(Need::Bool {
                yes: true,
                no: false,
            }),
            Need::Exact(Lit::Bool(true)) => Some(need.clone()),
            _ => None,
        },
        Pattern::Lit(Lit::Bool(false)) => match need {
            Need::Bool { no: true, .. } => Some(Need::Bool {
                yes: false,
                no: true,
            }),
            Need::Exact(Lit::Bool(false)) => Some(need.clone()),
            _ => None,
        },
        Pattern::Lit(lit) => match need {
            Need::Open { name, excluded }
                if lit_name(lit) == Some(name.as_str())
                    && !excluded.iter().any(|item| item == lit) =>
            {
                Some(Need::Exact(lit.clone()))
            }
            Need::Exact(other) if other == lit => Some(Need::Exact(lit.clone())),
            _ => None,
        },
        Pattern::Unpack(name) | Pattern::Open(name) => covered_ctor(need, name),
        Pattern::Ctor(name, patterns) => {
            let Need::Data { ctors } = need else {
                return None;
            };
            let ctor = ctors
                .iter()
                .find(|ctor| ctor.name == *name && ctor.fields.is_some())?;
            let fields = ctor.fields.as_ref()?;
            if let Some(groups) = group_type_tests(patterns, fields.len()) {
                if groups
                    .iter()
                    .zip(fields.iter())
                    .any(|(group, field)| group_partial_test(group, field))
                {
                    return None;
                }
                if !groups
                    .iter()
                    .zip(fields.iter())
                    .all(|(group, field)| group_overlaps(group, field))
                {
                    return None;
                }
                let flat: Vec<Pattern> = groups.iter().map(slot_as_cover).collect();
                return covered_fields(name, fields, &flat);
            }
            if patterns.len() != fields.len() {
                return covered_ctor(need, name);
            }
            covered_fields(name, fields, patterns)
        }
        Pattern::Array(patterns) => {
            let Need::Array { elem, open, apart } = need else {
                return None;
            };
            let len = patterns.len();
            let Some(base) = rows_for(elem, *open, apart, len) else {
                return None;
            };
            let mut hit = Vec::new();
            for row in &base {
                if !row_hit(row, patterns) {
                    continue;
                }
                let mut covered = Vec::new();
                let mut ok = true;
                for (pattern, slot) in patterns.iter().zip(row) {
                    match covered_slot(pattern, slot) {
                        Some(need) => covered.push(need),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    hit.push(covered);
                }
            }
            if hit.is_empty() {
                return None;
            }
            Some(Need::Array {
                elem: elem.clone(),
                open: false,
                apart: vec![Apart { len, rows: hit }],
            })
        }
        Pattern::Record(fields) => covered_record(need, fields),
    }
}

/// Residuals of a record pattern. An empty pattern covers the whole record.
/// A field the pattern does not name stays as it is.
fn subtract_record(need: &Need, fields: &[(String, Pattern)]) -> Vec<Need> {
    let Need::Record { fields: have } = need else {
        return vec![need.clone()];
    };
    if fields.is_empty() {
        return Vec::new();
    }
    let Some((indexes, row)) = project_fields(have, fields) else {
        return vec![need.clone()];
    };
    let patterns: Vec<Pattern> = fields.iter().map(|(_, pattern)| pattern.clone()).collect();
    let remainders = subtract_row(&row, &patterns);
    remainders
        .into_iter()
        .map(|remainder| {
            let mut next = have.clone();
            for (offset, index) in indexes.iter().enumerate() {
                next[*index].need = Rc::new(remainder[offset].clone());
            }
            Need::Record { fields: next }
        })
        .collect()
}

fn covered_record(need: &Need, fields: &[(String, Pattern)]) -> Option<Need> {
    let Need::Record { fields: have } = need else {
        return None;
    };
    let mut next = have.clone();
    for (name, pattern) in fields {
        let index = next.iter().position(|field| field.name == *name)?;
        next[index].need = Rc::new(covered_slot(pattern, &next[index].need)?);
    }
    Some(Need::Record { fields: next })
}

/// Mentioned fields in pattern order, paired with their index in `have`.
fn project_fields(
    have: &[FieldNeed],
    fields: &[(String, Pattern)],
) -> Option<(Vec<usize>, Vec<Need>)> {
    let mut indexes = Vec::with_capacity(fields.len());
    let mut row = Vec::with_capacity(fields.len());
    for (name, _) in fields {
        let index = have.iter().position(|field| field.name == *name)?;
        indexes.push(index);
        row.push((*have[index].need).clone());
    }
    Some((indexes, row))
}

fn lit_name(lit: &Lit) -> Option<&str> {
    match lit {
        Lit::Int(_) => Some("Int"),
        Lit::Number(_) => Some("Num"),
        Lit::String(_) => Some("Str"),
        Lit::Bool(_) => None,
    }
}

fn explain(rows: &[Vec<Need>]) -> String {
    let mut examples = Vec::new();
    for row in rows {
        for example in render_row(row) {
            examples.push(example);
            if examples.len() == EXAMPLE_CAP {
                return examples.join(", ");
            }
        }
    }
    examples.join(", ")
}

fn render_row(row: &[Need]) -> Vec<String> {
    let mut acc = vec![String::new()];
    for slot in row {
        let parts = render_need(slot);
        if parts.is_empty() {
            return Vec::new();
        }
        let mut next = Vec::new();
        for prefix in &acc {
            for part in &parts {
                if next.len() == EXAMPLE_CAP {
                    break;
                }
                let mut text = prefix.clone();
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(part);
                next.push(text);
            }
        }
        acc = next;
    }
    acc
}

fn render_need(need: &Need) -> Vec<String> {
    match need {
        Need::Any | Need::Deep => vec!["_".to_string()],
        Need::Bool { yes, no } => {
            let mut parts = Vec::new();
            if *yes {
                parts.push("true".to_string());
            }
            if *no {
                parts.push("false".to_string());
            }
            parts
        }
        Need::Open { name, .. } => vec![name.clone()],
        Need::Exact(lit) => vec![show_lit(lit)],
        Need::Data { ctors } => {
            let mut parts = Vec::new();
            for ctor in ctors {
                let Some(fields) = &ctor.fields else {
                    continue;
                };
                parts.extend(render_ctor(&ctor.name, fields));
                if parts.len() >= EXAMPLE_CAP {
                    parts.truncate(EXAMPLE_CAP);
                    break;
                }
            }
            parts
        }
        Need::Array { .. } => vec!["Array".to_string()],
        Need::Record { fields } => render_record(fields),
    }
}

fn render_record(fields: &[FieldNeed]) -> Vec<String> {
    if fields.is_empty() {
        return vec!["{}".to_string()];
    }
    let mut acc = vec![String::new()];
    for (index, field) in fields.iter().enumerate() {
        let parts = render_need(&field.need);
        if parts.is_empty() {
            return Vec::new();
        }
        let mut next = Vec::new();
        for prefix in &acc {
            for part in &parts {
                if next.len() == EXAMPLE_CAP {
                    break;
                }
                let shown =
                    if part.contains(' ') && !part.starts_with('{') && !part.starts_with('(') {
                        format!("({part})")
                    } else {
                        part.clone()
                    };
                let text = if index == 0 {
                    format!("{} : {shown}", field.name)
                } else {
                    format!("{prefix}, {} : {shown}", field.name)
                };
                next.push(text);
            }
        }
        acc = next;
    }
    acc.into_iter()
        .map(|text| format!("{{ {text} }}"))
        .collect()
}

fn render_ctor(name: &str, fields: &[Rc<Need>]) -> Vec<String> {
    if fields.is_empty() {
        return vec![name.to_string()];
    }
    let field_examples: Vec<Vec<String>> = fields.iter().map(|field| render_need(field)).collect();
    if field_examples
        .iter()
        .all(|examples| examples.len() == 1 && examples[0] == "_")
    {
        return vec![name.to_string()];
    }
    let mut acc = vec![name.to_string()];
    for examples in &field_examples {
        let mut next = Vec::new();
        for prefix in &acc {
            for example in examples {
                if next.len() == EXAMPLE_CAP {
                    break;
                }
                next.push(format!("{prefix} {example}"));
            }
        }
        acc = next;
    }
    acc.into_iter()
        .map(|text| text.strip_suffix(" _").unwrap_or(&text).to_string())
        .collect()
}

fn show_lit(lit: &Lit) -> String {
    match lit {
        Lit::Int(lexeme) => format!("{lexeme}n"),
        Lit::Number(lexeme) => lexeme.clone(),
        Lit::Bool(value) => value.to_string(),
        Lit::String(value) => {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
    }
}
