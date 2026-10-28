//! Equation clauses on a word. Patterns line up with the signature:
//! the leftmost pattern is the deepest of the matched slots, the rightmost is the top.
//! A name is a local. It is not on the stack until the body mentions it.

use std::collections::HashMap;

use crate::ast::{Clause, Def, Lit, Pattern};
use crate::diagnostic::Diagnostic;
use crate::codes::Error;
use crate::span::Span;

use super::{Checker, StackId, TyId, TyNode};

pub(super) struct ClauseInfo {
    pub binds: Vec<(String, TyId)>,
    pub pushes: Vec<TyId>,
}

pub(super) struct PreparedClauses {
    pub rest: StackId,
    pub slots: Vec<TyId>,
    pub infos: Vec<ClauseInfo>,
}

/// What a slot can still be. `Open` is an infinite scalar with some literals removed.
/// `Array` is every length when `open`, except lengths listed in `apart`.
/// An apart length with no rows is covered. `open: false` means only those lengths remain.
/// `Record` lists the fields the type has. A pattern may read fewer of them.
/// `Deep` is a data type the walk did not enter. `Any` is a type the walk did see
/// and does not know, such as a variable; a constructor does not cover `Any`.
#[derive(Clone, Debug)]
enum Need {
    Any,
    /// Fields past [`NEED_DEPTH`]. Any pattern covers the slot.
    Deep,
    Bool { yes: bool, no: bool },
    Open { name: String, excluded: Vec<Lit> },
    Exact(Lit),
    Data { ctors: Vec<CtorNeed> },
    Array {
        elem: Box<Need>,
        open: bool,
        apart: Vec<Apart>,
    },
    Record { fields: Vec<FieldNeed> },
}

/// One label of a record that is still in play. Order is alphabetical.
#[derive(Clone, Debug)]
struct FieldNeed {
    name: String,
    need: Need,
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
    fields: Option<Vec<Need>>,
}

const NEED_DEPTH: usize = 16;
const EXAMPLE_CAP: usize = 8;

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
            for ty in &info.pushes {
                arm = self.stack_cons(arm, *ty);
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

    fn pattern_effect(
        &mut self,
        pattern: &Pattern,
        ty: TyId,
        span: Span,
        binds: &mut Vec<(String, TyId)>,
        pushes: &mut Vec<TyId>,
    ) -> Result<(), Diagnostic> {
        match pattern {
            Pattern::Wild => Ok(()),
            Pattern::Bind(name) => {
                if binds.iter().any(|(bound, _)| bound == name) {
                    return Err(self.error(span, Error::DuplicateDefinition(name.to_string())));
                }
                binds.push((name.clone(), ty));
                Ok(())
            }
            Pattern::Lit(lit) => {
                let need = self.lit_ty(lit);
                self.unify_ty(ty, need)
                    .map_err(|message| self.error(span, message))?;
                Ok(())
            }
            Pattern::Unpack(name) => {
                let (fields, _) = self.ctor_shape(name, ty, span)?;
                pushes.extend(fields);
                Ok(())
            }
            Pattern::Ctor(name, patterns) => {
                let (fields, _) = self.ctor_shape(name, ty, span)?;
                if patterns.len() != fields.len() {
                    let count = fields.len();
                    return Err(self.error(span, Error::PatternArity { name: name.to_string(), count }));
                }
                for (pattern, field) in patterns.iter().zip(&fields) {
                    self.pattern_effect(pattern, *field, span, binds, pushes)?;
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

    /// Field types of `name` applied to `ty`, and whether the data is a newtype.
    pub(super) fn ctor_shape(
        &mut self,
        name: &str,
        ty: TyId,
        span: Span,
    ) -> Result<(Vec<TyId>, bool), Diagnostic> {
        let ty = self.expose(ty);
        let (data_name, args) = match self.tys[ty.0 as usize].clone() {
            TyNode::Ctor(data_name, args) => (data_name, args),
            _ => {
                return Err(self.error(
                    span,
                    Error::ExpectedDataType(self.show_ty(ty)),
                ));
            }
        };
        let Some(data) = self.datas.get(&data_name).cloned() else {
            return Err(self.error(
                span,
                Error::ExpectedDataType(self.show_ty(ty)),
            ));
        };
        if args.len() != data.params.len() {
            return Err(self.error(
                span,
                Error::ExpectedDataType(self.show_ty(ty)),
            ));
        }
        let Some(ctor) = data.ctors.iter().find(|ctor| ctor.name == name) else {
            return Err(self.error(
                span,
                Error::ConstructorNotOf { name: name.to_string(), type_name: data_name.to_string() },
            ));
        };
        let fields = self.instantiate_fields(&data.params, &args, &ctor.fields);
        Ok((fields, data.newtype))
    }

    fn exhaust(
        &mut self,
        clauses: &[Clause],
        slots: &[TyId],
        report: Span,
    ) -> Result<(), Diagnostic> {
        let mut rows = vec![slots
            .iter()
            .map(|ty| self.need_of(*ty, 0))
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

    fn need_of(&mut self, ty: TyId, depth: usize) -> Need {
        if depth > NEED_DEPTH {
            return Need::Deep;
        }
        let ty = self.expose(ty);
        if let TyNode::Record(row) = self.tys[ty.0 as usize].clone() {
            return self.record_need(row, depth);
        }
        let TyNode::Ctor(name, args) = self.tys[ty.0 as usize].clone() else {
            return Need::Any;
        };
        if args.is_empty() {
            match name.as_str() {
                "Bool" => return Need::Bool { yes: true, no: true },
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
                elem: Box::new(self.need_of(args[0], depth + 1)),
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
                needs.push(self.need_of(field, depth + 1));
            }
            ctors.push(CtorNeed {
                name: ctor.name.clone(),
                fields: Some(needs),
            });
        }
        Need::Data { ctors }
    }

    fn record_need(&mut self, row: super::RowId, depth: usize) -> Need {
        let Ok((fields, _)) = self.peel_row(row) else {
            return Need::Any;
        };
        let mut out = Vec::with_capacity(fields.len());
        for (name, field_ty) in fields {
            out.push(FieldNeed {
                name,
                need: self.need_of(field_ty, depth + 1),
            });
        }
        Need::Record { fields: out }
    }
}

fn row_hit(row: &[Need], patterns: &[Pattern]) -> bool {
    patterns
        .iter()
        .zip(row)
        .all(|(pattern, need)| overlaps(pattern, need))
}

fn overlaps(pattern: &Pattern, need: &Need) -> bool {
    if matches!(need, Need::Any | Need::Deep) {
        return true;
    }
    match pattern {
        Pattern::Wild | Pattern::Bind(_) => true,
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
        Pattern::Unpack(name) => live_ctor(need, name).is_some(),
        Pattern::Ctor(name, patterns) => {
            let Some(fields) = live_ctor(need, name) else {
                return false;
            };
            patterns.len() == fields.len()
                && patterns
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

fn live_ctor<'a>(need: &'a Need, name: &str) -> Option<&'a [Need]> {
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
    if matches!(need, Need::Deep) {
        return Vec::new();
    }
    match pattern {
        Pattern::Wild | Pattern::Bind(_) => Vec::new(),
        _ if matches!(need, Need::Any) => vec![Need::Any],
        Pattern::Lit(Lit::Bool(true)) => match need {
            Need::Bool { no: true, .. } => vec![Need::Bool { yes: false, no: true }],
            _ => Vec::new(),
        },
        Pattern::Lit(Lit::Bool(false)) => match need {
            Need::Bool { yes: true, .. } => vec![Need::Bool { yes: true, no: false }],
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
        Pattern::Unpack(name) => match need {
            Need::Data { ctors } => {
                let left: Vec<_> = ctors
                    .iter()
                    .filter(|ctor| ctor.name != *name && ctor.fields.is_some())
                    .cloned()
                    .collect();
                if left.is_empty() {
                    Vec::new()
                } else {
                    vec![Need::Data { ctors: left }]
                }
            }
            _ => vec![need.clone()],
        },
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
                if patterns.len() != fields.len() {
                    return vec![need.clone()];
                }
                let remainders = subtract_row(&fields, patterns);
                let mut out = Vec::new();
                if !others.is_empty() {
                    out.push(Need::Data { ctors: others });
                }
                for remainder in remainders {
                    out.push(Need::Data {
                        ctors: vec![CtorNeed {
                            name: name.clone(),
                            fields: Some(remainder),
                        }],
                    });
                }
                out
            }
            _ => vec![need.clone()],
        },
        Pattern::Array(patterns) => subtract_array(need, patterns),
        Pattern::Record(fields) => subtract_record(need, fields),
    }
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
    if matches!(need, Need::Deep) {
        return Some(Need::Deep);
    }
    match pattern {
        Pattern::Wild | Pattern::Bind(_) => Some(need.clone()),
        _ if matches!(need, Need::Any) => None,
        Pattern::Lit(Lit::Bool(true)) => match need {
            Need::Bool { yes: true, .. } => Some(Need::Bool { yes: true, no: false }),
            Need::Exact(Lit::Bool(true)) => Some(need.clone()),
            _ => None,
        },
        Pattern::Lit(Lit::Bool(false)) => match need {
            Need::Bool { no: true, .. } => Some(Need::Bool { yes: false, no: true }),
            Need::Exact(Lit::Bool(false)) => Some(need.clone()),
            _ => None,
        },
        Pattern::Lit(lit) => match need {
            Need::Open { name, excluded }
                if lit_name(lit) == Some(name.as_str()) && !excluded.iter().any(|item| item == lit) =>
            {
                Some(Need::Exact(lit.clone()))
            }
            Need::Exact(other) if other == lit => Some(Need::Exact(lit.clone())),
            _ => None,
        },
        Pattern::Unpack(name) => {
            let Need::Data { ctors } = need else {
                return None;
            };
            let ctor = ctors.iter().find(|ctor| ctor.name == *name && ctor.fields.is_some())?;
            Some(Need::Data {
                ctors: vec![ctor.clone()],
            })
        }
        Pattern::Ctor(name, patterns) => {
            let Need::Data { ctors } = need else {
                return None;
            };
            let ctor = ctors.iter().find(|ctor| ctor.name == *name && ctor.fields.is_some())?;
            let fields = ctor.fields.as_ref()?;
            if patterns.len() != fields.len() {
                return None;
            }
            let mut covered = Vec::new();
            for (pattern, field) in patterns.iter().zip(fields) {
                covered.push(covered_slot(pattern, field)?);
            }
            Some(Need::Data {
                ctors: vec![CtorNeed {
                    name: name.clone(),
                    fields: Some(covered),
                }],
            })
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
                next[*index].need = remainder[offset].clone();
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
        next[index].need = covered_slot(pattern, &next[index].need)?;
    }
    Some(Need::Record { fields: next })
}

/// Mentioned fields in pattern order, paired with their index in `have`.
fn project_fields(have: &[FieldNeed], fields: &[(String, Pattern)]) -> Option<(Vec<usize>, Vec<Need>)> {
    let mut indexes = Vec::with_capacity(fields.len());
    let mut row = Vec::with_capacity(fields.len());
    for (name, _) in fields {
        let index = have.iter().position(|field| field.name == *name)?;
        indexes.push(index);
        row.push(have[index].need.clone());
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
                let shown = if part.contains(' ') && !part.starts_with('{') && !part.starts_with('(') {
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
    acc.into_iter().map(|text| format!("{{ {text} }}")).collect()
}

fn render_ctor(name: &str, fields: &[Need]) -> Vec<String> {
    if fields.is_empty() {
        return vec![name.to_string()];
    }
    let field_examples: Vec<Vec<String>> = fields.iter().map(render_need).collect();
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
        .map(|text| {
            text.strip_suffix(" _")
                .unwrap_or(&text)
                .to_string()
        })
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
