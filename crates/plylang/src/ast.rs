use std::fmt;

use crate::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub span: Span,
    pub language: Option<u32>,
    pub imports: Vec<Import>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    pub span: Span,
    pub path: String,
    pub names: Vec<String>,
    pub all: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Data(Data),
    /// Transparent synonym. Saturated uses expand to the right-hand side.
    Synonym(Synonym),
    Def(Def),
    Class(Class),
    Instance(Instance),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Data {
    pub span: Span,
    pub name_span: Span,
    pub name: String,
    pub params: Vec<String>,
    pub ctors: Vec<Ctor>,
    /// No constructors and no JavaScript. Values come from foreign words.
    /// A parameter indexes that value and is not a field.
    pub foreign: bool,
    /// One constructor, one field, nominal, and free in JavaScript.
    pub newtype: bool,
    /// Text of the `..` lines directly above this declaration. Empty if there are none.
    pub doc: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Synonym {
    pub span: Span,
    pub name_span: Span,
    pub name: String,
    pub params: Vec<String>,
    pub rhs: Type,
    /// Text of the `..` lines directly above this declaration. Empty if there are none.
    pub doc: String,
}

/// One constructor field. `name` is set when the constructor has two or more fields.
#[derive(Debug, Clone, PartialEq)]
pub struct CtorField {
    pub name: Option<String>,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ctor {
    pub span: Span,
    pub name: String,
    pub fields: Vec<CtorField>,
}

impl Ctor {
    pub(crate) fn field_tys(&self) -> Vec<Type> {
        self.fields.iter().map(|field| field.ty.clone()).collect()
    }
}

/// `foreign` and `private` are independent. Each may be absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Vis {
    pub private: bool,
    pub foreign: bool,
}

impl Vis {
    pub const PUBLIC: Self = Self {
        private: false,
        foreign: false,
    };

    pub fn is_private(self) -> bool {
        self.private
    }

    pub fn is_foreign(self) -> bool {
        self.foreign
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Def {
    pub span: Span,
    pub name_span: Span,
    pub vis: Vis,
    pub name: String,
    pub scheme: Scheme,
    pub body: Vec<Term>,
    /// Equation body. Non-empty only when `body` is empty.
    /// Patterns are bottom to top: the rightmost pattern is the top of the stack.
    pub clauses: Vec<Clause>,
    /// Text of the `..` lines directly above this declaration. Empty if there are none.
    pub doc: String,
}

/// One equation of a word. The first one wins.
#[derive(Debug, Clone, PartialEq)]
pub struct Clause {
    pub span: Span,
    pub patterns: Vec<Pattern>,
    pub body: Vec<Term>,
}

/// One stack slot. A bare constructor unpacks its fields onto the stack.
/// Parentheses keep the constructor in that one slot.
/// An array matches one fixed length. Index 0 is the left element.
/// A record reads the fields it names. The row tail stays in the type.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Wild,
    Bind(String),
    Lit(Lit),
    Unpack(String),
    Ctor(String, Vec<Pattern>),
    Array(Vec<Pattern>),
    /// `{ name : n, age : a }`. Empty `{}` binds nothing.
    Record(Vec<(String, Pattern)>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub span: Span,
    pub name_span: Span,
    pub name: String,
    /// Written before `=>`. Empty when the class has no superclass.
    pub supers: Vec<Constraint>,
    pub params: Vec<String>,
    pub methods: Vec<MethodSig>,
    /// Text of the `..` lines directly above this declaration. Empty if there are none.
    pub doc: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MethodSig {
    pub name: String,
    pub name_span: Span,
    pub scheme: Scheme,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Instance {
    pub span: Span,
    /// One head, one or more classes: `Comparable, Sortable`.
    pub class_names: Vec<String>,
    pub args: Vec<Type>,
    pub methods: Vec<MethodBind>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MethodBind {
    pub name: String,
    pub name_span: Span,
    pub word: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Scheme {
    pub vars: Vec<String>,
    pub constraints: Vec<Constraint>,
    pub effect: StackEffect,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    pub class_name: String,
    pub args: Vec<Type>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StackEffect {
    pub input: Stack,
    pub output: Stack,
    pub effects: Option<EffectRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stack {
    pub tail: StackTail,
    pub items: Vec<Type>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StackTail {
    Implicit,
    Empty,
    Var(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Var(String),
    Ctor(String),
    /// Saturated application. The head is a variable, a constructor, or another type.
    App(Box<Type>, Vec<Type>),
    Fun(Box<StackEffect>),
    Record(RecordRow),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordRow {
    pub fields: Vec<(String, Type)>,
    pub tail: Option<RecordTail>,
}

/// What follows `|` in a record type. A bare lowercase name is a row variable.
/// Any other type is spliced, and it must elaborate to a closed record.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordTail {
    Var(String),
    Type(Box<Type>),
}

impl RecordRow {
    pub(crate) fn tail_ty(&self) -> Option<&Type> {
        match &self.tail {
            Some(RecordTail::Type(ty)) => Some(ty),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectRow {
    pub labels: Vec<String>,
    pub tail: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub span: Span,
    pub node: TermNode,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TermNode {
    Word(String),
    Lit(Lit),
    Quote(Vec<Term>),
    /// `name>>` reads one field.
    Field(String),
    /// `>>name`. The new value is above the base. The result is a new value.
    FieldSet(String),
    /// `+name`. The record is below the new value. The label must be absent.
    FieldInsert(String),
    /// `< 1 2 3 >`. Elements are constants, not a stack program.
    Array(Vec<Term>),
    /// `{ name: "Ada", age: 36 }`. Each value is one constant. `{}` is empty.
    Record(Vec<(String, Term)>),
    /// Quotation header `x y ->`. The leftmost name is the deepest value.
    /// The rightmost name is the top.
    Bind(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Int(String),
    Number(String),
    String(String),
    Bool(bool),
}

impl fmt::Display for Module {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(version) = self.language {
            writeln!(formatter, "language {version}")?;
        }
        let mut need_break = false;
        for import in &self.imports {
            if need_break {
                writeln!(formatter)?;
            }
            write!(formatter, "{import}")?;
            need_break = true;
        }
        for item in &self.items {
            if need_break {
                writeln!(formatter)?;
            }
            write!(formatter, "{item}")?;
            need_break = true;
        }
        Ok(())
    }
}

impl fmt::Display for Import {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "~ \"{}\" (", self.path)?;
        let mut parts = self.names.clone();
        if self.all {
            parts.push("..".to_string());
        }
        write!(formatter, "{})", parts.join(", "))
    }
}

impl fmt::Display for Item {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data(data) => write!(formatter, "{data}"),
            Self::Synonym(synonym) => write!(formatter, "{synonym}"),
            Self::Def(def) => write!(formatter, "{def}"),
            Self::Class(class) => write!(formatter, "{class}"),
            Self::Instance(instance) => write!(formatter, "{instance}"),
        }
    }
}

impl fmt::Display for Data {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_doc(formatter, &self.doc)?;
        write!(formatter, "; ")?;
        if self.foreign {
            write!(formatter, "foreign ")?;
        }
        write!(formatter, "{}", self.name)?;
        for param in &self.params {
            write!(formatter, " {param}")?;
        }
        if self.foreign {
            return Ok(());
        }
        if self.newtype {
            write!(formatter, " == ")?;
        } else {
            write!(formatter, " = ")?;
        }
        for (index, ctor) in self.ctors.iter().enumerate() {
            if index > 0 {
                write!(formatter, " | ")?;
            }
            write!(formatter, "{}", ctor.name)?;
            write_ctor_fields(formatter, &ctor.fields)?;
        }
        Ok(())
    }
}

impl fmt::Display for Synonym {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_doc(formatter, &self.doc)?;
        write!(formatter, "- {}", self.name)?;
        for param in &self.params {
            write!(formatter, " {param}")?;
        }
        write!(formatter, " {}", type_string(&self.rhs))
    }
}

impl fmt::Display for Def {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_doc(formatter, &self.doc)?;
        write!(formatter, ": ")?;
        if self.vis.foreign {
            write!(formatter, "foreign ")?;
        }
        if self.vis.private {
            write!(formatter, "private ")?;
        }
        write!(
            formatter,
            "{} ( {} )",
            self.name,
            scheme_string(&self.scheme)
        )?;
        if !self.clauses.is_empty() {
            for clause in &self.clauses {
                write!(formatter, "{}", clause_string(clause))?;
            }
        } else if !self.body.is_empty() {
            write!(formatter, "{}", body_string(&self.body, 2))?;
        }
        Ok(())
    }
}

impl fmt::Display for Class {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_doc(formatter, &self.doc)?;
        write!(formatter, "& ")?;
        if !self.supers.is_empty() {
            for (index, constraint) in self.supers.iter().enumerate() {
                if index > 0 {
                    write!(formatter, ", ")?;
                }
                write!(formatter, "{}", constraint.class_name)?;
                write_slots(formatter, &constraint.args)?;
            }
            write!(formatter, " => ")?;
        }
        write!(formatter, "{}", self.name)?;
        for param in &self.params {
            write!(formatter, " {param}")?;
        }
        for method in &self.methods {
            write!(
                formatter,
                "\n  {} ( {} )",
                method.name,
                scheme_string(&method.scheme)
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for Instance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "# {}", self.class_names.join(", "))?;
        write_slots(formatter, &self.args)?;
        for method in &self.methods {
            write!(formatter, "\n  {} = {}", method.name, method.word)?;
        }
        Ok(())
    }
}

/// `..` lines, one per source line. An empty line in `doc` is a bare `..`.
pub fn push_doc(out: &mut String, doc: &str) {
    if doc.is_empty() {
        return;
    }
    for line in doc.split('\n') {
        out.push_str("..");
        if !line.is_empty() {
            out.push(' ');
            out.push_str(line);
        }
        out.push('\n');
    }
}

fn write_doc(formatter: &mut fmt::Formatter<'_>, doc: &str) -> fmt::Result {
    let mut text = String::new();
    push_doc(&mut text, doc);
    write!(formatter, "{text}")
}

/// Method scheme with the class constraint in front. The AST stores the
/// constraint separately, on the class.
pub fn method_line(class: &Class, method: &MethodSig) -> String {
    let mut scheme = method.scheme.clone();
    scheme.constraints.insert(
        0,
        Constraint {
            class_name: class.name.clone(),
            args: class
                .params
                .iter()
                .map(|param| Type::Var(param.clone()))
                .collect(),
        },
    );
    format!("{} ( {} )", method.name, scheme_string(&scheme))
}

pub fn scheme_string(scheme: &Scheme) -> String {
    let mut out = String::new();
    if !scheme.vars.is_empty() {
        out.push_str(&scheme.vars.join(" "));
        out.push_str(". ");
    }
    for constraint in &scheme.constraints {
        out.push_str(&constraint.class_name);
        out.push_str(&slots_string(&constraint.args));
        out.push_str(" => ");
    }
    out.push_str(&effect_string(&scheme.effect));
    out
}

pub fn effect_string(effect: &StackEffect) -> String {
    let input = stack_string(&effect.input);
    let output = stack_string(&effect.output);
    let mut out = if input.is_empty() {
        format!("-- {output}")
    } else if output.is_empty() {
        format!("{input} --")
    } else {
        format!("{input} -- {output}")
    };
    if output.is_empty() && input.is_empty() {
        out = "--".to_string();
    }
    if let Some(effects) = &effect.effects {
        out.push_str(" ! ");
        out.push_str(&effect_row_string(effects));
    }
    out
}

fn stack_string(stack: &Stack) -> String {
    let mut parts = Vec::new();
    match &stack.tail {
        StackTail::Implicit => {}
        StackTail::Empty => parts.push(".".to_string()),
        StackTail::Var(name) => parts.push(format!("..{name}")),
    }
    for item in &stack.items {
        parts.push(type_string(item));
    }
    parts.join(" ")
}

fn slots_string(items: &[Type]) -> String {
    let mut out = String::new();
    for item in items {
        out.push(' ');
        out.push_str(&type_string(item));
    }
    out
}

fn write_slots(formatter: &mut fmt::Formatter<'_>, items: &[Type]) -> fmt::Result {
    write!(formatter, "{}", slots_string(items))
}

fn write_ctor_fields(formatter: &mut fmt::Formatter<'_>, fields: &[CtorField]) -> fmt::Result {
    if fields.iter().any(|field| field.name.is_some()) {
        for (index, field) in fields.iter().enumerate() {
            if index > 0 {
                write!(formatter, ",")?;
            }
            write!(
                formatter,
                " {} : {}",
                field.name.as_deref().unwrap_or("_"),
                type_string(&field.ty)
            )?;
        }
        return Ok(());
    }
    write_slots(formatter, &fields.iter().map(|field| field.ty.clone()).collect::<Vec<_>>())
}

pub(crate) fn type_string(ty: &Type) -> String {
    match ty {
        Type::Var(name) | Type::Ctor(name) => name.clone(),
        Type::App(head, args) => {
            let mut out = format!("({}", type_string(head));
            for arg in args {
                out.push(' ');
                out.push_str(&type_string(arg));
            }
            out.push(')');
            out
        }
        Type::Fun(effect) => format!("({})", effect_string(effect)),
        Type::Record(row) => record_string(row),
    }
}

fn record_string(row: &RecordRow) -> String {
    let tail = row.tail.as_ref().map(|tail| match tail {
        RecordTail::Var(name) => name.clone(),
        RecordTail::Type(ty) => type_string(ty),
    });
    brace_string(
        &row.fields
            .iter()
            .map(|(name, ty)| format!("{name} : {}", type_string(ty)))
            .collect::<Vec<_>>(),
        &tail,
    )
}

fn effect_row_string(row: &EffectRow) -> String {
    brace_string(&row.labels, &row.tail)
}

fn brace_string(items: &[String], tail: &Option<String>) -> String {
    if items.is_empty() && tail.is_none() {
        return "{}".to_string();
    }
    let mut out = String::from("{ ");
    out.push_str(&items.join(", "));
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

fn clause_string(clause: &Clause) -> String {
    let mut out = String::from("\n  ");
    for (index, pattern) in clause.patterns.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&pattern_string(pattern));
    }
    out.push_str(" ->");
    if !clause.body.is_empty() {
        out.push(' ');
        for (index, term) in clause.body.iter().enumerate() {
            if index > 0 {
                out.push(' ');
            }
            out.push_str(&term_string(term, 2));
        }
    }
    out
}

fn pattern_string(pattern: &Pattern) -> String {
    match pattern {
        Pattern::Wild => "_".to_string(),
        Pattern::Bind(name) | Pattern::Unpack(name) => name.clone(),
        Pattern::Lit(lit) => lit_string(lit),
        Pattern::Ctor(name, fields) => {
            let mut out = format!("({name}");
            for field in fields {
                out.push(' ');
                out.push_str(&pattern_string(field));
            }
            out.push(')');
            out
        }
        Pattern::Array(elements) => {
            if elements.is_empty() {
                "<>".to_string()
            } else {
                let inner = elements
                    .iter()
                    .map(pattern_string)
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("< {inner} >")
            }
        }
        Pattern::Record(fields) => {
            if fields.is_empty() {
                "{}".to_string()
            } else {
                let inner = fields
                    .iter()
                    .map(|(name, pattern)| format!("{name} : {}", pattern_string(pattern)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{ {inner} }}")
            }
        }
    }
}

fn lit_string(lit: &Lit) -> String {
    match lit {
        Lit::Int(lexeme) => format!("{lexeme}n"),
        Lit::Number(lexeme) => lexeme.clone(),
        Lit::Bool(value) => value.to_string(),
        Lit::String(value) => format!("\"{}\"", escape_string(value)),
    }
}

fn body_string(body: &[Term], indent: usize) -> String {
    if body.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n");
    out.push_str(&" ".repeat(indent));
    for (index, term) in body.iter().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&term_string(term, indent));
    }
    out
}

fn term_string(term: &Term, indent: usize) -> String {
    match &term.node {
        TermNode::Word(name) => name.clone(),
        TermNode::Bind(names) => format!("{} ->", names.join(" ")),
        TermNode::Field(name) => format!("{name}>>"),
        TermNode::FieldSet(name) => format!(">>{name}"),
        TermNode::FieldInsert(name) => format!("+{name}"),
        TermNode::Lit(lit) => lit_string(lit),
        TermNode::Array(elements) => {
            if elements.is_empty() {
                "<>".to_string()
            } else {
                let inner = elements
                    .iter()
                    .map(|term| term_string(term, indent))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("< {inner} >")
            }
        }
        TermNode::Record(fields) => {
            if fields.is_empty() {
                "{}".to_string()
            } else {
                let inner = fields
                    .iter()
                    .map(|(name, term)| format!("{name}: {}", term_string(term, indent)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{ {inner} }}")
            }
        }
        TermNode::Quote(terms) => {
            if terms.is_empty() {
                "[]".to_string()
            } else {
                let inner = terms
                    .iter()
                    .map(|term| term_string(term, indent))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("[ {inner} ]")
            }
        }
    }
}

fn escape_string(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}
