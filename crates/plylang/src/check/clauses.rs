//! Equation clauses on a word. Patterns line up with the signature:
//! the leftmost pattern is the deepest of the matched slots, the rightmost is the top.
//! A name is a local. It is not on the stack until the body mentions it.
//! `_` matches any slot and leaves the value.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::anf::{ParamTest, ShapeTest};
use crate::ast::{Clause, Def, Lit, Pattern};
use crate::codes::Error;
use crate::diagnostic::Diagnostic;
use crate::span::Span;

use super::{Checker, StackId, StackNode, TyId, TyNode};

pub(super) struct ClauseInfo {
    pub binds: Vec<(String, TyId)>,
    /// Values the pattern puts back, and the constructor a kept value is narrowed to.
    /// A shape test has no constructor. The types here are the signature's types.
    /// [`ClauseInfo::subst`] rewrites them for this arm only.
    pub pushes: Vec<(TyId, Option<String>)>,
    /// Signature variables this arm learned. Not applied to the live skolems.
    pub subst: HashMap<TyId, TyId>,
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

/// A pattern is a service-class test, or it is some other pattern.
pub(super) enum CheckDecision {
    Use(ShapeTest),
    NotCheck,
}

/// The part of a pattern `Nullary`, `Unary`, or `Binary` can read.
/// A spill or a literal is absent.
enum CheckView<'a> {
    /// `Num` or `Array`.
    Unpack(&'a str),
    /// `(Num)`. `(Array)` is an arity error, not this check.
    Nullary(&'a str),
    /// `(Array Num)`, `(Array _)`, `(Map Str Num)`.
    Applied { name: &'a str, args: &'a [Pattern] },
}

/// One service instance whose head is the unsaturated constructor.
struct OpenCheck {
    ctor: String,
    arity: usize,
    word: String,
    origin: String,
    /// Parameter words, in index order. `checkUnary_1` is index 0.
    params: Vec<(String, String)>,
}

/// Constructor of a visible service head.
struct OpenHead {
    ctor: String,
    arity: usize,
}

/// One named parameter of a service pattern.
#[derive(Clone, Debug)]
struct SeenParam {
    index: usize,
    inner: Box<SeenCheck>,
}

/// A service pattern already accepted on a variable slot.
#[derive(Clone, Debug)]
struct SeenCheck {
    ctor: String,
    params: Vec<SeenParam>,
}

/// The class a head of this arity implements. Arity above two has none.
#[derive(Clone, Copy)]
struct Service {
    class: &'static str,
    method: &'static str,
    params: &'static [&'static str],
}

fn service_for(arity: usize) -> Option<Service> {
    match arity {
        0 => Some(Service {
            class: "Nullary",
            method: "checkNullary",
            params: &[],
        }),
        1 => Some(Service {
            class: "Unary",
            method: "checkUnary",
            params: &["checkUnary_1"],
        }),
        2 => Some(Service {
            class: "Binary",
            method: "checkBinary",
            params: &["checkBinary_1", "checkBinary_2"],
        }),
        _ => None,
    }
}

impl<'a> CheckView<'a> {
    fn name(&self) -> &'a str {
        match self {
            CheckView::Unpack(name) | CheckView::Nullary(name) => name,
            CheckView::Applied { name, .. } => name,
        }
    }

    /// The pattern names a parameter, such as `(Hub Num)`. `_` and a binder do not.
    fn names_parameter(&self) -> bool {
        match self {
            CheckView::Applied { args, .. } => args
                .iter()
                .any(|arg| !matches!(arg, Pattern::Wild | Pattern::Bind(..))),
            _ => false,
        }
    }
}

/// What a slot can still be. `Open` is an infinite scalar with some literals removed.
/// `Array` is every length when `open`, except lengths listed in `apart`.
/// An apart length with no rows is covered. `open: false` means only those lengths remain.
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
    /// A variable slot after one or more service patterns. The same head again
    /// is redundant. A different head still overlaps, because the tests are not
    /// proved disjoint.
    Screened {
        seen: Vec<SeenCheck>,
    },
    /// The walk stopped. A wildcard or a binding covers the slot.
    Deep,
    Bool {
        yes: bool,
        no: bool,
    },
    /// The one value `unit`.
    Unit,
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
        output: StackId,
    ) -> Result<StackId, Diagnostic> {
        let prepared = self.prepare_clauses(&def.clauses, input, def.name_span)?;
        for (clause, info) in def.clauses.iter().zip(&prepared.infos) {
            self.check_clause(clause, info, prepared.rest, output)?;
        }
        Ok(output)
    }

    /// One arm against the signature, with this arm's variables filled in.
    /// The live skolems stay as the caller sees them.
    fn check_clause(
        &mut self,
        clause: &Clause,
        info: &ClauseInfo,
        rest: StackId,
        output: StackId,
    ) -> Result<(), Diagnostic> {
        let subst = arm_subst(&info.subst);
        let saved = self.assumptions.clone();
        let mut memo = super::Memo::default();
        self.assumptions = saved
            .iter()
            .map(|constraint| super::SemConstraint {
                class_name: constraint.class_name.clone(),
                args: constraint
                    .args
                    .iter()
                    .map(|arg| self.copy_ty(*arg, &subst, &mut memo))
                    .collect(),
            })
            .collect();
        let mut arm = self.copy_stack(rest, &subst, &mut memo);
        for (ty, ctor) in &info.pushes {
            let ty = self.copy_ty(*ty, &subst, &mut memo);
            arm = self.stack_cons(arm, ty);
            if let Some(ctor) = ctor {
                self.note_refine(arm, ctor);
            }
        }
        let mut frame = HashMap::new();
        for (name, ty) in &info.binds {
            let ty = self.copy_ty(*ty, &subst, &mut memo);
            frame.insert(name.clone(), self.binding_scheme(ty));
        }
        self.locals.push(frame);
        let checked = self.check_terms(&clause.body, arm);
        self.locals.pop();
        let checked = match checked {
            Ok(checked) => checked,
            Err(error) => {
                self.assumptions = saved;
                return Err(error);
            }
        };
        let expected = self.copy_stack(output, &subst, &mut memo);
        let unified = self
            .unify_stack(checked, expected)
            .map_err(|message| self.error(clause.span, message));
        self.assumptions = saved;
        unified
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
            let mut subst = HashMap::new();
            for (pattern, ty) in clause.patterns.iter().zip(&slots) {
                if matches!(pattern, Pattern::Wild) {
                    pushes.push((*ty, None));
                    continue;
                }
                self.pattern_effect(
                    pattern,
                    *ty,
                    clause.span,
                    &mut binds,
                    &mut pushes,
                    &mut subst,
                )?;
            }
            infos.push(ClauseInfo {
                binds,
                pushes,
                subst,
            });
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
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<(), Diagnostic> {
        match pattern {
            // A field `_` is not a stack slot. The slot form is restored in `prepare_clauses`.
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
            Pattern::Unpack(name) => match self.ctor_shape(name, ty, span) {
                Ok(shape) => {
                    pushes.push((ty, Some(shape.tag)));
                    Ok(())
                }
                Err(error) => match self.decide_check(pattern, span)? {
                    CheckDecision::Use(_) => self.effect_host(pattern, ty, span, pushes, subst),
                    CheckDecision::NotCheck => Err(error),
                },
            },
            Pattern::Open(name) => {
                let shape = self.ctor_shape(name, ty, span)?;
                for field in shape.fields {
                    pushes.push((field, None));
                }
                Ok(())
            }
            Pattern::Ctor(name, patterns) => {
                let shape = match self.ctor_shape(name, ty, span) {
                    Ok(shape) => shape,
                    Err(error) => match self.decide_check(pattern, span)? {
                        CheckDecision::Use(_) => {
                            return self.effect_host(pattern, ty, span, pushes, subst);
                        }
                        CheckDecision::NotCheck => return Err(error),
                    },
                };
                if let Some(groups) = group_type_tests(patterns, shape.fields.len()) {
                    if self.type_test_heads(&groups) {
                        return self.effect_type_tests(&shape, &groups, span, binds, pushes, subst);
                    }
                }
                match self.ctor_match(name, &shape, patterns, span)? {
                    FieldMatch::Positional => {
                        for (pattern, field) in patterns.iter().zip(&shape.fields) {
                            self.pattern_effect(pattern, *field, span, binds, pushes, subst)?;
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
                                subst,
                            )?;
                        }
                    }
                }
                Ok(())
            }
            Pattern::Array(patterns) => {
                let elem = self.array_elem(ty, span)?;
                for pattern in patterns {
                    self.pattern_effect(pattern, elem, span, binds, pushes, subst)?;
                }
                Ok(())
            }
        }
    }

    /// A shape test leaves the value. The signature variables are recorded in `subst`.
    fn effect_host(
        &mut self,
        pattern: &Pattern,
        ty: TyId,
        span: Span,
        pushes: &mut Vec<(TyId, Option<String>)>,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<(), Diagnostic> {
        let CheckDecision::Use(shape) = self.decide_check(pattern, span)? else {
            return Err(self.error(span, Error::ExpectedPattern));
        };
        self.match_host(ty, &shape, span, subst)?;
        pushes.push((ty, None));
        Ok(())
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
    /// such as `(Person l0 l1)` and `(Sent _ headers _ _ _)`.
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

    /// `(Some (Array Num) items)` binds `items` at the field. The arm's
    /// substitution turns that field into `(Array Num)`. The live skolem stays.
    fn effect_type_tests(
        &mut self,
        shape: &CtorShape,
        groups: &[SlotPat],
        span: Span,
        binds: &mut Vec<(String, TyId)>,
        pushes: &mut Vec<(TyId, Option<String>)>,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<(), Diagnostic> {
        for (index, group) in groups.iter().enumerate() {
            match group {
                SlotPat::Plain(pattern) => {
                    self.pattern_effect(pattern, shape.fields[index], span, binds, pushes, subst)?;
                }
                SlotPat::Test { ty, bind } => {
                    let witness = self.check_type_test(ty, shape.fields[index], span, subst)?;
                    self.bind_type_test(bind, witness.ty, span, binds, pushes)?;
                }
            }
        }
        Ok(())
    }

    /// A parenthesized type, then the name of that value. `test` is absent when
    /// the field is already that type. The bound type is the field, so the arm
    /// substitution and the name stay one variable.
    pub(super) fn check_type_test(
        &mut self,
        pattern: &Pattern,
        field: TyId,
        span: Span,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<TypeWitness, Diagnostic> {
        if let CheckDecision::Use(shape) = self.decide_check(pattern, span)? {
            let exposed = self.expose(field);
            if matches!(self.tys[exposed.0 as usize], TyNode::Ctor(..)) {
                let pattern_ty = self.pattern_type(pattern, span)?;
                if !self.types_overlap(field, pattern_ty) {
                    return Err(self.error(span, self.ty_mismatch(pattern_ty, field)));
                }
            }
            let test = self.match_host(field, &shape, span, subst)?;
            return Ok(TypeWitness { ty: field, test });
        }
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

    fn known_arity(&self, name: &str) -> Option<usize> {
        self.type_arity
            .get(name)
            .or_else(|| self.extra_arity.get(name))
            .copied()
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

    /// A visible `# Nullary`, `# Unary`, or `# Binary` whose head is unsaturated.
    fn open_service(&self, name: &str, service: Service) -> Option<OpenCheck> {
        let name = self.canonical(name);
        for inst in &self.instances {
            if inst.class_name != service.class || inst.args.len() != 1 {
                continue;
            }
            let arg = self.expose(inst.args[0]);
            let TyNode::Ctor(ctor, params) = &self.tys[arg.0 as usize] else {
                continue;
            };
            if ctor != name || !params.is_empty() {
                continue;
            }
            let Some(method) = inst
                .methods
                .iter()
                .find(|method| method.name == service.method)
            else {
                continue;
            };
            let mut words = Vec::with_capacity(service.params.len());
            let mut complete = true;
            for param_name in service.params {
                let Some(found) = inst
                    .methods
                    .iter()
                    .find(|method| method.name == *param_name)
                else {
                    complete = false;
                    break;
                };
                words.push((found.word.clone(), inst.origin.clone()));
            }
            if !complete {
                continue;
            }
            let Some(arity) = self.known_arity(ctor) else {
                continue;
            };
            return Some(OpenCheck {
                ctor: ctor.clone(),
                arity,
                word: method.word.clone(),
                origin: inst.origin.clone(),
                params: words,
            });
        }
        None
    }

    fn param_open(&self, id: TyId) -> bool {
        let id = self.expose(id);
        matches!(
            self.tys[id.0 as usize],
            TyNode::Var | TyNode::Template | TyNode::Rigid(_)
        )
    }

    fn open_heads(&self) -> Vec<OpenHead> {
        let mut heads = Vec::new();
        let mut seen = HashSet::new();
        for inst in &self.instances {
            let class_arity = match inst.class_name.as_str() {
                "Nullary" => 0,
                "Unary" => 1,
                "Binary" => 2,
                _ => continue,
            };
            if inst.args.len() != 1 {
                continue;
            }
            let arg = self.expose(inst.args[0]);
            let TyNode::Ctor(ctor, params) = &self.tys[arg.0 as usize] else {
                continue;
            };
            if !params.is_empty() {
                continue;
            }
            let Some(arity) = self.known_arity(ctor) else {
                continue;
            };
            if arity != class_arity || !seen.insert(ctor.clone()) {
                continue;
            }
            heads.push(OpenHead {
                ctor: ctor.clone(),
                arity,
            });
        }
        heads
    }

    fn data_with_constructors(&self, name: &str) -> bool {
        self.datas
            .get(name)
            .is_some_and(|data| !data.ctors.is_empty())
    }

    /// `Name ->` and `(Array Num)` select a visible service instance.
    /// A data type with constructors and no instance stays a constructor pattern.
    pub(super) fn decide_check(
        &mut self,
        pattern: &Pattern,
        span: Span,
    ) -> Result<CheckDecision, Diagnostic> {
        let Some(view) = check_view(pattern) else {
            return Ok(CheckDecision::NotCheck);
        };
        let name = view.name().to_string();
        if !self.type_known(&name) {
            return Ok(CheckDecision::NotCheck);
        }
        if self.syn_in_scope(&name) {
            let ty = self.pattern_type(pattern, span)?;
            return self.decide_ty(ty, span);
        }
        self.decide_view(&view, span)
    }

    /// A visible service instance for this unsaturated head.
    /// `None` means the pattern is not a check. A named parameter of an arity
    /// the service classes do not cover is `has no element test`. A head with
    /// no constructors and no instance is `no instance`.
    fn take_service(
        &self,
        name: &str,
        arity: usize,
        named: bool,
        span: Span,
    ) -> Result<Option<OpenCheck>, Diagnostic> {
        let Some(service) = service_for(arity) else {
            if named && !self.data_with_constructors(name) {
                return Err(self.error(span, Error::NoElementTest(name.to_string())));
            }
            return Ok(None);
        };
        // A visible instance is the check, including when the type has constructors.
        let Some(open) = self.open_service(name, service) else {
            if self.data_with_constructors(name) {
                return Ok(None);
            }
            return Err(self.error(
                span,
                Error::NoInstance {
                    constraint: format!("{} {name}", service.class),
                    needed_by: String::new(),
                },
            ));
        };
        Ok(Some(open))
    }

    fn decide_view(
        &mut self,
        view: &CheckView<'_>,
        span: Span,
    ) -> Result<CheckDecision, Diagnostic> {
        let name = view.name();
        // `(Num)` is the check. `(Array)` still asks for one argument.
        if matches!(view, CheckView::Nullary(_)) && self.known_arity(name) != Some(0) {
            return Ok(CheckDecision::NotCheck);
        }
        let Some(arity) = self.known_arity(name) else {
            return Ok(CheckDecision::NotCheck);
        };
        if let CheckView::Applied { args, .. } = view {
            if args.len() != arity {
                return Ok(CheckDecision::NotCheck);
            }
        }
        let Some(open) = self.take_service(name, arity, view.names_parameter(), span)? else {
            return Ok(CheckDecision::NotCheck);
        };
        let Some(params) = self.param_tests(view, &open, span)? else {
            return Ok(CheckDecision::NotCheck);
        };
        Ok(CheckDecision::Use(shape_from(&open, params)))
    }

    /// Named parameters become tests. `_` and a binder add none, and bind nothing.
    /// `None` means an argument is not itself a check, so the pattern is not one.
    fn param_tests(
        &mut self,
        view: &CheckView<'_>,
        open: &OpenCheck,
        span: Span,
    ) -> Result<Option<Vec<ParamTest>>, Diagnostic> {
        let CheckView::Applied { args, .. } = view else {
            return Ok(Some(Vec::new()));
        };
        let mut params = Vec::new();
        for (index, arg) in args.iter().enumerate() {
            if matches!(arg, Pattern::Wild | Pattern::Bind(..)) {
                continue;
            }
            let CheckDecision::Use(inner) = self.decide_check(arg, span)? else {
                return Ok(None);
            };
            let Some((word, origin)) = open.params.get(index) else {
                return Err(self.error(span, Error::NoElementTest(open.ctor.clone())));
            };
            params.push(ParamTest {
                index,
                word: word.clone(),
                origin: origin.clone(),
                inner: Box::new(inner),
            });
        }
        Ok(Some(params))
    }

    fn decide_ty(&mut self, ty: TyId, span: Span) -> Result<CheckDecision, Diagnostic> {
        let ty = self.expose(ty);
        let TyNode::Ctor(name, args) = self.tys[ty.0 as usize].clone() else {
            return Ok(CheckDecision::NotCheck);
        };
        if !self.type_known(&name) {
            return Ok(CheckDecision::NotCheck);
        }
        let Some(arity) = self.known_arity(&name) else {
            return Ok(CheckDecision::NotCheck);
        };
        if !args.is_empty() && args.len() != arity {
            return Ok(CheckDecision::NotCheck);
        }
        let named = args.iter().copied().any(|arg| !self.param_open(arg));
        let Some(open) = self.take_service(&name, arity, named, span)? else {
            return Ok(CheckDecision::NotCheck);
        };
        let mut params = Vec::new();
        for (index, arg) in args.iter().copied().enumerate() {
            if self.param_open(arg) {
                continue;
            }
            let CheckDecision::Use(inner) = self.decide_ty(arg, span)? else {
                return Ok(CheckDecision::NotCheck);
            };
            let Some((word, origin)) = open.params.get(index) else {
                return Err(self.error(span, Error::NoElementTest(name)));
            };
            params.push(ParamTest {
                index,
                word: word.clone(),
                origin: origin.clone(),
                inner: Box::new(inner),
            });
        }
        Ok(CheckDecision::Use(shape_from(&open, params)))
    }

    /// The elaborated type of a field that is already a constructor, when the
    /// pattern itself was not a `Check` form. A variable element is any array.
    fn shape_of_ty(&mut self, ty: TyId) -> Option<ShapeTest> {
        let decided = self.decide_ty(ty, Span::new(0, 0)).ok()?;
        match decided {
            CheckDecision::Use(shape) => Some(shape),
            CheckDecision::NotCheck => None,
        }
    }

    /// Learn what this shape says about `slot`. The returned test is what
    /// JavaScript must check. `None` means the slot is already that shape.
    /// A signature variable is recorded in `subst`. It is not unified in place.
    pub(super) fn match_host(
        &mut self,
        slot: TyId,
        shape: &ShapeTest,
        span: Span,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<Option<ShapeTest>, Diagnostic> {
        let slot = self.expose(slot);
        match self.tys[slot.0 as usize].clone() {
            TyNode::Rigid(_) => self.match_rigid(slot, shape, span, subst),
            TyNode::Var | TyNode::Template => self.match_flexible(slot, shape, span),
            TyNode::App(head, args) => self.match_app(head, &args, shape, span, subst),
            TyNode::Ctor(name, args) => self.match_known(&name, &args, shape, span, subst),
            _ => {
                let message = self.host_mismatch(shape, slot);
                Err(self.error(span, message))
            }
        }
    }

    /// `a := Num`, or `a := (Array b)` with a fresh element.
    fn match_rigid(
        &mut self,
        slot: TyId,
        shape: &ShapeTest,
        span: Span,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<Option<ShapeTest>, Diagnostic> {
        let slot = self.find_ty(slot);
        if let Some(prev) = subst.get(&slot).copied() {
            return self.refine_bound(prev, shape, span, subst);
        }
        subst.insert(slot, self.host_image(shape));
        Ok(Some(shape.clone()))
    }

    /// A variable this match introduced, such as the element of `Array ->`.
    /// Linking it is safe: it is not a skolem of the signature.
    fn match_flexible(
        &mut self,
        slot: TyId,
        shape: &ShapeTest,
        span: Span,
    ) -> Result<Option<ShapeTest>, Diagnostic> {
        let image = self.host_image(shape);
        self.bind_ty(slot, image)
            .map_err(|message| self.error(span, message))?;
        Ok(Some(shape.clone()))
    }

    /// `(f a)` matches a head that is no shorter than the application.
    /// The prefix of the head becomes `f`. A named prefix parameter is part of
    /// that head. The remaining parameters line up with the application.
    fn match_app(
        &mut self,
        head: TyId,
        args: &[TyId],
        shape: &ShapeTest,
        span: Span,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<Option<ShapeTest>, Diagnostic> {
        let head = self.expose(head);
        let flexible = matches!(
            self.tys[head.0 as usize],
            TyNode::Rigid(_) | TyNode::Var | TyNode::Template
        );
        if !flexible || shape.arity < args.len() {
            let slot = self.expose_app(head, args);
            let message = self.host_mismatch(shape, slot);
            return Err(self.error(span, message));
        }
        let prefix = shape.arity - args.len();
        self.bind_unsaturated(head, shape, prefix, span, subst)?;
        for (offset, arg) in args.iter().copied().enumerate() {
            let index = prefix + offset;
            if let Some(param) = shape.params.iter().find(|param| param.index == index) {
                self.match_host(arg, &param.inner, span, subst)?;
            }
        }
        Ok(Some(shape.clone()))
    }

    fn expose_app(&mut self, head: TyId, args: &[TyId]) -> TyId {
        let mut id = self.expose(head);
        for arg in args {
            id = self.type_app(id, *arg);
        }
        id
    }

    /// `f` becomes the unsaturated head. A named prefix parameter is that
    /// argument. A second pattern refines the stored image. It does not build
    /// another one: two fresh variables would not match.
    fn bind_unsaturated(
        &mut self,
        head: TyId,
        shape: &ShapeTest,
        prefix: usize,
        span: Span,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<(), Diagnostic> {
        let head = self.find_ty(head);
        if matches!(self.tys[head.0 as usize], TyNode::Rigid(_)) {
            if let Some(prev) = subst.get(&head).copied() {
                if self.refine_prefix(prev, shape, prefix, span, subst)? {
                    return Ok(());
                }
                let image = self.head_prefix(shape, prefix);
                let message = self.ty_mismatch(image, prev);
                return Err(self.error(span, message));
            }
            subst.insert(head, self.head_prefix(shape, prefix));
            return Ok(());
        }
        let image = self.head_prefix(shape, prefix);
        self.bind_ty(head, image)
            .map_err(|message| self.error(span, message))?;
        Ok(())
    }

    /// The unsaturated head. A named prefix parameter uses that test's image.
    fn head_prefix(&mut self, shape: &ShapeTest, prefix: usize) -> TyId {
        let mut params = Vec::with_capacity(prefix);
        for index in 0..prefix {
            if let Some(param) = shape.params.iter().find(|param| param.index == index) {
                params.push(self.host_image(&param.inner));
            } else {
                params.push(self.ty_var());
            }
        }
        self.ty_ctor(&shape.ctor, params)
    }

    /// Same constructor and the same prefix length. Named prefix arguments
    /// refine the variables already stored.
    fn refine_prefix(
        &mut self,
        prev: TyId,
        shape: &ShapeTest,
        prefix: usize,
        span: Span,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<bool, Diagnostic> {
        let prev = self.expose(prev);
        let TyNode::Ctor(name, args) = self.tys[prev.0 as usize].clone() else {
            return Ok(false);
        };
        if name != shape.ctor || args.len() != prefix {
            return Ok(false);
        }
        for index in 0..prefix {
            if let Some(param) = shape.params.iter().find(|param| param.index == index) {
                let Some(arg) = args.get(index).copied() else {
                    return Ok(false);
                };
                self.match_host(arg, &param.inner, span, subst)?;
            }
        }
        Ok(true)
    }

    /// The slot is already a constructor. Fill a parameter variable, or reject
    /// a different type. A parameter that was already known drops out of the
    /// test. No test when every parameter was already known.
    fn match_known(
        &mut self,
        name: &str,
        args: &[TyId],
        shape: &ShapeTest,
        span: Span,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<Option<ShapeTest>, Diagnostic> {
        if name == shape.ctor && args.len() == shape.arity {
            if shape.params.is_empty() {
                return Ok(None);
            }
            let mut kept = Vec::new();
            for param in &shape.params {
                let Some(arg) = args.get(param.index).copied() else {
                    let slot = self.ty_ctor(name, args.to_vec());
                    let message = self.host_mismatch(shape, slot);
                    return Err(self.error(span, message));
                };
                if let Some(inner) = self.match_host(arg, &param.inner, span, subst)? {
                    kept.push(ParamTest {
                        index: param.index,
                        word: param.word.clone(),
                        origin: param.origin.clone(),
                        inner: Box::new(inner),
                    });
                }
            }
            if kept.is_empty() {
                return Ok(None);
            }
            let mut next = shape.clone();
            next.params = kept;
            return Ok(Some(next));
        }
        let slot = self.ty_ctor(name, args.to_vec());
        let message = self.host_mismatch(shape, slot);
        Err(self.error(span, message))
    }

    /// `a` was already learned. The same head agrees. A tighter parameter fills
    /// the variable stored in that image. A fresh image would fail congruence.
    fn refine_bound(
        &mut self,
        prev: TyId,
        shape: &ShapeTest,
        span: Span,
        subst: &mut HashMap<TyId, TyId>,
    ) -> Result<Option<ShapeTest>, Diagnostic> {
        let prev = self.expose(prev);
        let TyNode::Ctor(name, args) = self.tys[prev.0 as usize].clone() else {
            let message = self.host_mismatch(shape, prev);
            return Err(self.error(span, message));
        };
        if name != shape.ctor || args.len() != shape.arity {
            let message = self.host_mismatch(shape, prev);
            return Err(self.error(span, message));
        }
        for param in &shape.params {
            if let Some(arg) = args.get(param.index).copied() {
                self.match_host(arg, &param.inner, span, subst)?;
            }
        }
        Ok(Some(shape.clone()))
    }

    fn host_image(&mut self, shape: &ShapeTest) -> TyId {
        let mut params = Vec::with_capacity(shape.arity);
        for index in 0..shape.arity {
            if let Some(param) = shape.params.iter().find(|param| param.index == index) {
                params.push(self.host_image(&param.inner));
            } else {
                params.push(self.ty_var());
            }
        }
        self.ty_ctor(&shape.ctor, params)
    }

    fn host_mismatch(&mut self, shape: &ShapeTest, slot: TyId) -> Error {
        let found = self.host_image(shape);
        self.ty_mismatch(found, slot)
    }

    fn exhaust(
        &mut self,
        clauses: &[Clause],
        slots: &[TyId],
        report: Span,
    ) -> Result<(), Diagnostic> {
        let clauses = self.canonical_clauses(clauses);
        let heads = self.open_heads();
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
            if !rows
                .iter()
                .any(|row| row_hit(row, &clause.patterns, &heads))
            {
                return Err(self.error(clause.span, Error::RedundantPattern));
            }
            let mut next = Vec::new();
            for row in rows {
                if row_hit(&row, &clause.patterns, &heads) {
                    next.extend(subtract_row(&row, &clause.patterns, &heads));
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
            _ => TypeKey::Other,
        };
        keys.insert(ty, built.clone());
        built
    }

    fn need_shape(&mut self, ty: TyId, depth: usize, memo: &mut NeedMemo) -> Need {
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
                "Unit" => return Need::Unit,
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
            return Need::Open {
                name,
                excluded: Vec::new(),
            };
        };
        if args.len() != data.params.len() {
            return Need::Any;
        }
        if data.ctors.is_empty() {
            return Need::Open {
                name,
                excluded: Vec::new(),
            };
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

fn row_hit(row: &[Need], patterns: &[Pattern], heads: &[OpenHead]) -> bool {
    patterns
        .iter()
        .zip(row)
        .all(|(pattern, need)| overlaps(pattern, need, heads))
}

fn overlaps(pattern: &Pattern, need: &Need, heads: &[OpenHead]) -> bool {
    if matches!(need, Need::Any) {
        return true;
    }
    if let Some(rel) = host_coverage(pattern, need, heads) {
        return rel != Rel::Miss;
    }
    if matches!(need, Need::Screened { .. }) {
        return !matches!(pattern, Pattern::Spill);
    }
    match pattern {
        Pattern::Wild | Pattern::Bind(..) => true,
        Pattern::Spill => false,
        Pattern::Lit(Lit::Unit) => matches!(need, Need::Unit),
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
                    .all(|(group, field)| group_overlaps(group, field, heads));
            }
            // A named subset covers the constructor. Its binds are irrefutable.
            if patterns.len() != fields.len() {
                return true;
            }
            patterns
                .iter()
                .zip(fields)
                .all(|(pattern, field)| overlaps(pattern, field, heads))
        }
        Pattern::Array(patterns) => match need {
            Need::Array { elem, open, apart } => match part_of(apart, patterns.len()) {
                Some(part) => part.rows.iter().any(|row| row_hit(row, patterns, heads)),
                None => {
                    *open
                        && patterns
                            .iter()
                            .all(|pattern| overlaps(pattern, elem, heads))
                }
            },
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
fn subtract_row(row: &[Need], patterns: &[Pattern], heads: &[OpenHead]) -> Vec<Vec<Need>> {
    if patterns.is_empty() {
        return Vec::new();
    }
    if !overlaps(&patterns[0], &row[0], heads) {
        return vec![row.to_vec()];
    }
    let mut out = Vec::new();
    for left in subtract_slot(&row[0], &patterns[0], heads) {
        let mut next = vec![left];
        next.extend(row[1..].iter().cloned());
        out.push(next);
    }
    if let Some(covered) = covered_slot(&patterns[0], &row[0], heads) {
        for tail in subtract_row(&row[1..], &patterns[1..], heads) {
            let mut next = vec![covered.clone()];
            next.extend(tail);
            out.push(next);
        }
    }
    out
}

fn subtract_slot(need: &Need, pattern: &Pattern, heads: &[OpenHead]) -> Vec<Need> {
    if !overlaps(pattern, need, heads) {
        return vec![need.clone()];
    }
    if let Some(rel) = host_coverage(pattern, need, heads) {
        return match rel {
            Rel::Hit => Vec::new(),
            Rel::Partial => vec![screen_check(need, pattern, heads)],
            Rel::Miss => vec![need.clone()],
        };
    }
    match pattern {
        Pattern::Wild | Pattern::Bind(..) => Vec::new(),
        Pattern::Spill => vec![need.clone()],
        _ if matches!(need, Need::Any) => vec![Need::Any],
        Pattern::Lit(Lit::Unit) => match need {
            Need::Unit => Vec::new(),
            _ => vec![need.clone()],
        },
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
                    return subtract_grouped(need, name, &fields, &groups, others, heads);
                }
                if patterns.len() != fields.len() {
                    return without_ctor(need, name);
                }
                assemble(name, others, subtract_fields(&fields, patterns, heads))
            }
            _ => vec![need.clone()],
        },
        Pattern::Array(patterns) => subtract_array(need, patterns, heads),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rel {
    Miss,
    Partial,
    Hit,
}

/// `Num`, `(Array Num)`, `(Array _)`, `(Map _ Num)`. A spill is not a check.
/// `(Array x)` does not bind `x`.
fn check_view(pattern: &Pattern) -> Option<CheckView<'_>> {
    match pattern {
        Pattern::Unpack(name) => Some(CheckView::Unpack(name)),
        Pattern::Ctor(name, args) if args.is_empty() => Some(CheckView::Nullary(name)),
        Pattern::Ctor(name, args) if args.iter().all(is_check_arg) => {
            Some(CheckView::Applied { name, args })
        }
        _ => None,
    }
}

fn is_check_arg(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Wild | Pattern::Bind(..) => true,
        other => check_view(other).is_some(),
    }
}

fn shape_from(open: &OpenCheck, params: Vec<ParamTest>) -> ShapeTest {
    ShapeTest {
        word: open.word.clone(),
        ctor: open.ctor.clone(),
        arity: open.arity,
        params,
        origin: open.origin.clone(),
    }
}

/// The check a coverage walk can see. The word and its module are not needed here.
fn check_shape(pattern: &Pattern, heads: &[OpenHead]) -> Option<SeenCheck> {
    let view = check_view(pattern)?;
    let name = view.name();
    let head = heads.iter().find(|head| head.ctor == name)?;
    let params = match &view {
        CheckView::Nullary(_) if head.arity != 0 => return None,
        CheckView::Unpack(_) | CheckView::Nullary(_) => Vec::new(),
        CheckView::Applied { args, .. } => {
            if args.len() != head.arity {
                return None;
            }
            let mut params = Vec::new();
            for (index, arg) in args.iter().enumerate() {
                if matches!(arg, Pattern::Wild | Pattern::Bind(..)) {
                    continue;
                }
                params.push(SeenParam {
                    index,
                    inner: Box::new(check_shape(arg, heads)?),
                });
            }
            params
        }
    };
    Some(SeenCheck {
        ctor: name.to_string(),
        params,
    })
}

/// A constructor of this need wins over a check of the same name.
/// `Num ->` on `Num` is a check, because `Num` has no constructors.
fn host_coverage(pattern: &Pattern, need: &Need, heads: &[OpenHead]) -> Option<Rel> {
    let shape = check_shape(pattern, heads)?;
    let name = match pattern {
        Pattern::Unpack(name) | Pattern::Open(name) | Pattern::Ctor(name, _) => name.as_str(),
        _ => return None,
    };
    if live_ctor(need, name).is_some() {
        return None;
    }
    Some(shape_rel(&shape, need))
}

/// A head with no parameter tests covers a later tighter head.
/// A tighter head does not cover a later wider one.
fn covers(prev: &SeenCheck, next: &SeenCheck) -> bool {
    if prev.ctor != next.ctor {
        return false;
    }
    prev.params.iter().all(|test| {
        next.params
            .iter()
            .any(|other| other.index == test.index && covers(&test.inner, &other.inner))
    })
}

/// A check on a variable slot remembers the head. A concrete slot stays as it is.
fn screen_check(need: &Need, pattern: &Pattern, heads: &[OpenHead]) -> Need {
    let Some(shape) = check_shape(pattern, heads) else {
        return need.clone();
    };
    match need {
        Need::Any => Need::Screened { seen: vec![shape] },
        Need::Screened { seen } => {
            let mut seen = seen.clone();
            seen.push(shape);
            Need::Screened { seen }
        }
        _ => need.clone(),
    }
}

pub(super) fn arm_subst(map: &HashMap<TyId, TyId>) -> super::Subst {
    super::Subst {
        ty: map.clone(),
        stack: HashMap::new(),
        effect: HashMap::new(),
    }
}

fn type_rel(pattern: &Pattern, need: &Need, heads: &[OpenHead]) -> Rel {
    if let Some(rel) = host_coverage(pattern, need, heads) {
        return rel;
    }
    // A data type is accepted only when the field is already that type.
    match need {
        Need::Any | Need::Deep | Need::Screened { .. } => Rel::Partial,
        _ => Rel::Hit,
    }
}

fn shape_rel(shape: &SeenCheck, need: &Need) -> Rel {
    let open = shape.params.is_empty();
    match need {
        Need::Any | Need::Deep => Rel::Partial,
        Need::Screened { seen } => {
            if seen.iter().any(|item| covers(item, shape)) {
                Rel::Miss
            } else {
                Rel::Partial
            }
        }
        Need::Open { name, .. } if name == &shape.ctor => {
            if open {
                Rel::Hit
            } else {
                Rel::Partial
            }
        }
        Need::Exact(Lit::Number(_)) if shape.ctor == "Num" && open => Rel::Hit,
        Need::Exact(Lit::Int(_)) if shape.ctor == "Int" && open => Rel::Hit,
        Need::Exact(Lit::String(_)) if shape.ctor == "Str" && open => Rel::Hit,
        Need::Bool { .. } | Need::Exact(Lit::Bool(_)) if shape.ctor == "Bool" && open => Rel::Hit,
        Need::Unit if shape.ctor == "Unit" && open => Rel::Hit,
        Need::Array { elem, .. } if shape.ctor == "Array" => {
            match shape.params.iter().find(|param| param.index == 0) {
                None => Rel::Hit,
                Some(param) => shape_rel(&param.inner, elem),
            }
        }
        _ => Rel::Miss,
    }
}

fn group_overlaps(group: &SlotPat, field: &Need, heads: &[OpenHead]) -> bool {
    match group {
        SlotPat::Plain(pattern) => overlaps(pattern, field, heads),
        SlotPat::Test { ty, .. } => type_rel(ty, field, heads) != Rel::Miss,
    }
}

/// The test can fail, so the constructor stays in the uncovered set.
fn group_partial_test(group: &SlotPat, field: &Need, heads: &[OpenHead]) -> bool {
    match group {
        SlotPat::Test { ty, .. } => type_rel(ty, field, heads) == Rel::Partial,
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
    heads: &[OpenHead],
) -> Vec<Need> {
    if groups
        .iter()
        .zip(fields)
        .any(|(group, field)| !group_overlaps(group, field, heads))
    {
        return vec![need.clone()];
    }
    if groups
        .iter()
        .zip(fields)
        .any(|(group, field)| group_partial_test(group, field, heads))
    {
        return vec![need.clone()];
    }
    let flat: Vec<Pattern> = groups.iter().map(slot_as_cover).collect();
    assemble(name, others, subtract_fields(fields, &flat, heads))
}

fn subtract_fields(
    fields: &[Rc<Need>],
    patterns: &[Pattern],
    heads: &[OpenHead],
) -> Vec<Vec<Need>> {
    let owned: Vec<Need> = fields.iter().map(|field| (**field).clone()).collect();
    subtract_row(&owned, patterns, heads)
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

fn covered_fields(
    name: &str,
    fields: &[Rc<Need>],
    patterns: &[Pattern],
    heads: &[OpenHead],
) -> Option<Need> {
    let mut covered = Vec::new();
    for (pattern, field) in patterns.iter().zip(fields) {
        covered.push(Rc::new(covered_slot(pattern, field, heads)?));
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

fn subtract_array(need: &Need, patterns: &[Pattern], heads: &[OpenHead]) -> Vec<Need> {
    let Need::Array { elem, open, apart } = need else {
        return vec![need.clone()];
    };
    let len = patterns.len();
    let Some(base) = rows_for(elem, *open, apart, len) else {
        return vec![need.clone()];
    };
    let remainders: Vec<Vec<Need>> = base
        .iter()
        .flat_map(|row| subtract_row(row, patterns, heads))
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

fn covered_slot(pattern: &Pattern, need: &Need, heads: &[OpenHead]) -> Option<Need> {
    if let Some(rel) = host_coverage(pattern, need, heads) {
        return match rel {
            Rel::Hit => Some(need.clone()),
            Rel::Partial | Rel::Miss => None,
        };
    }
    match pattern {
        Pattern::Wild | Pattern::Bind(..) => Some(need.clone()),
        Pattern::Spill => None,
        _ if matches!(need, Need::Any) => None,
        Pattern::Lit(Lit::Unit) => match need {
            Need::Unit => Some(Need::Unit),
            _ => None,
        },
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
                    .any(|(group, field)| group_partial_test(group, field, heads))
                {
                    return None;
                }
                if !groups
                    .iter()
                    .zip(fields.iter())
                    .all(|(group, field)| group_overlaps(group, field, heads))
                {
                    return None;
                }
                let flat: Vec<Pattern> = groups.iter().map(slot_as_cover).collect();
                return covered_fields(name, fields, &flat, heads);
            }
            if patterns.len() != fields.len() {
                return covered_ctor(need, name);
            }
            covered_fields(name, fields, patterns, heads)
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
                if !row_hit(row, patterns, heads) {
                    continue;
                }
                let mut covered = Vec::new();
                let mut ok = true;
                for (pattern, slot) in patterns.iter().zip(row) {
                    match covered_slot(pattern, slot, heads) {
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
    }
}

fn lit_name(lit: &Lit) -> Option<&str> {
    match lit {
        Lit::Int(_) => Some("Int"),
        Lit::Number(_) => Some("Num"),
        Lit::String(_) => Some("Str"),
        Lit::Bool(_) | Lit::Unit => None,
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
        Need::Any | Need::Deep | Need::Screened { .. } => vec!["_".to_string()],
        Need::Unit => vec!["unit".to_string()],
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
    }
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
        Lit::Unit => "unit".to_string(),
        Lit::String(value) => {
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
    }
}
