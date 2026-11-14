//! Direct-style ANF. One stack slot is one local. `dup` is two uses of that local.
//!
//! A word receives only its own prefix, bottom to top. The tail stays in the caller.
//! A visible quotation is inlined at `dip`, `keep`, `bi`, `call`, and `if`; a quotation value is a function.

use std::fmt;

use crate::ast::Lit;

pub type LocalId = u32;

#[derive(Clone, Debug, PartialEq)]
pub struct AnfModule {
    pub datas: Vec<AnfData>,
    pub defs: Vec<AnfDef>,
    /// Source order. An empty list leaves the JavaScript unchanged.
    pub places: Vec<AnfPlace>,
    /// Checks. `javascript` leaves them out. The test runner appends them.
    pub checks: Vec<AnfCheck>,
    pub needs_unit: bool,
    /// Instance words this module calls, and the module that wrote the `#`.
    pub method_homes: Vec<MethodHome>,
}

/// One place. `init` fills the first frame and is not exported.
/// The reader, the writer, and the binder are emitted beside the slot.
#[derive(Clone, Debug, PartialEq)]
pub struct AnfPlace {
    pub name: String,
    pub export: bool,
    pub init: AnfDef,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnfData {
    pub ctors: Vec<AnfCtor>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnfCtor {
    pub name: String,
    pub fields: usize,
    /// Parallel to the field count. `None` is a positional field (`value` / `valueN`).
    pub names: Vec<Option<String>>,
    /// The constructor is the identity function. An equation reads the value itself.
    pub newtype: bool,
}

/// One `?` check, lowered as two functions the runner calls.
#[derive(Clone, Debug, PartialEq)]
pub struct AnfCheck {
    pub want: AnfDef,
    pub body: AnfDef,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnfDef {
    pub name: String,
    pub export: bool,
    pub params: Vec<LocalId>,
    pub body: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub lets: Vec<Let>,
    pub result: Vec<LocalId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Let {
    pub binds: Vec<LocalId>,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuoteForm {
    pub params: Vec<LocalId>,
    pub body: Expr,
}

/// One equation. `conds` are joined with `&&` and short-circuit left to right.
/// The last equation is emitted as `else` and its conditions are not tested again.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub conds: Vec<Cond>,
    pub body: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cond {
    pub root: LocalId,
    pub steps: Vec<Access>,
    pub test: PatTest,
}

/// One step from `root` to the tested value.
#[derive(Clone, Debug, PartialEq)]
pub enum Access {
    /// Constructor field. `width == 1` is `.value`; otherwise `.value{index}`.
    Field { index: usize, width: usize },
    /// Array element. Index 0 is the left.
    Index(usize),
    /// Record field. The string is the label, read as `obj.name`.
    Prop(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PatTest {
    Bool(bool),
    Int(String),
    Number(String),
    String(String),
    Tag(String),
    /// `path.length === n`. The path is the array, not an element.
    Length(usize),
    /// The value has this runtime shape. A ground type emits no test.
    Shape(ShapeTest),
}

/// A shape JavaScript can see. `Array(None)` is any array.
#[derive(Clone, Debug, PartialEq)]
pub enum ShapeTest {
    Num,
    Int,
    Str,
    Bool,
    Array(Option<Box<ShapeTest>>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Lit(Lit),
    Quote(QuoteForm),
    Ctor {
        name: String,
        fields: Vec<LocalId>,
    },
    Call {
        word: String,
        args: Vec<LocalId>,
    },
    Apply {
        fun: LocalId,
        args: Vec<LocalId>,
    },
    /// Equation chain. One arm writes its body directly.
    Choose {
        arms: Vec<Choice>,
    },
    If {
        cond: LocalId,
        yes: Expr,
        no: Expr,
    },
    /// Field `index` of a constructor, left to right. `width` is the field count.
    Field {
        base: LocalId,
        index: usize,
        width: usize,
    },
    /// Element `index` of an array. Index 0 is the left.
    Index {
        base: LocalId,
        index: usize,
    },
    /// Property of a record object. The name is the Ply field label.
    Prop {
        base: LocalId,
        name: String,
    },
    /// A new record. The old object is not mutated. `name` is a static label.
    SetProp {
        base: LocalId,
        name: String,
        value: LocalId,
    },
    /// JavaScript array, index 0 at the left.
    Array(Vec<LocalId>),
    /// JavaScript object. Fields are in source order.
    Record {
        fields: Vec<(String, LocalId)>,
    },
    /// Class dictionary. A method is the implementing word, or a closure that
    /// closes over dictionaries fixed by the instance head. The closure's
    /// parameters are the method's own dictionaries and then the stack values.
    Dict {
        fields: Vec<DictField>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct DictField {
    pub name: String,
    pub value: DictValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DictValue {
    Word(String),
    Closure {
        params: Vec<LocalId>,
        dicts: Vec<LocalId>,
        word: String,
    },
    /// An already-built dictionary, used for a superclass field.
    Local(LocalId),
}

pub fn local_name(id: LocalId) -> String {
    format!("v{id}")
}

/// The discriminant owns the key `tag`. A field of that name is stored beside it.
pub(crate) fn field_key(name: &str) -> &str {
    if name == "tag" {
        "$tag"
    } else {
        name
    }
}

/// The module that wrote the `#` whose word `local` calls.
/// `remote` is the word's name in that module. `origin` is its module key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodHome {
    pub local: String,
    pub remote: String,
    pub origin: String,
}

impl AnfModule {
    pub fn javascript(&self) -> String {
        let mut out = String::new();
        if self.needs_unit {
            out.push_str("const Unit = undefined;\n");
        }
        for data in &self.datas {
            for ctor in &data.ctors {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&ctor_export(ctor));
                out.push('\n');
            }
        }
        if !self.datas.is_empty() && !self.defs.is_empty() {
            out.push('\n');
        }
        for (index, def) in self.defs.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(&def_export(def));
            out.push('\n');
        }
        for place in &self.places {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&place_runtime(place));
        }
        out
    }

    /// The module plus one exported function for each picture and each program.
    /// Relative imports are still the caller's concern.
    pub fn with_checks(&self) -> AnfModule {
        let mut copy = self.clone();
        let checks = std::mem::take(&mut copy.checks);
        for check in checks {
            copy.defs.push(check.want);
            copy.defs.push(check.body);
        }
        copy
    }
}

/// The slot is a stack of frames. The binder restores the previous frame
/// when the quotation throws. Init runs at load, after the functions exist.
fn place_runtime(place: &AnfPlace) -> String {
    let js_name = js_ident(&place.name);
    let slot = format!("ply$place${js_name}");
    let set = js_ident(&format!("ply_set_{}", place.name));
    let bind = js_ident(&format!("ply_bind_{}", place.name));
    let init = js_ident(&place.init.name);
    let mut out = String::new();
    out.push_str(&format!("const {slot} = [];\n"));
    out.push_str(&function_text(
        place.export,
        &js_name,
        &place.name,
        "",
        &format!("  return {slot}[{slot}.length - 1];\n"),
    ));
    out.push('\n');
    out.push_str(&function_text(
        place.export,
        &set,
        &set,
        "value",
        &format!("  {slot}[{slot}.length - 1] = value;\n"),
    ));
    out.push('\n');
    out.push_str(&function_text(
        place.export,
        &bind,
        &bind,
        "value, quot",
        &format!(
            "  {slot}.push(value);\n  try {{\n    return quot();\n  }} finally {{\n    {slot}.pop();\n  }}\n"
        ),
    ));
    out.push('\n');
    let mut init_def = place.init.clone();
    init_def.export = false;
    out.push_str(&def_export(&init_def));
    out.push('\n');
    out.push_str(&format!("{slot}.push({init}());\n"));
    out
}

fn function_text(
    export: bool,
    js_name: &str,
    export_name: &str,
    params: &str,
    body: &str,
) -> String {
    let keyword = if export && js_name == export_name {
        "export function"
    } else {
        "function"
    };
    let mut out = format!("{keyword} {js_name}({params}) {{\n{body}}}");
    if export && js_name != export_name {
        out.push_str(&format!("\nexport {{ {js_name} as {export_name} }};"));
    }
    out
}

fn ctor_export(ctor: &AnfCtor) -> String {
    let name = js_ident(&ctor.name);
    if ctor.newtype {
        return format!("export const {name} = (value) => value;");
    }
    if ctor.fields == 0 {
        return format!("export const {name} = {{ tag: \"{}\" }};", ctor.name);
    }
    if ctor.fields == 1 {
        return format!(
            "export const {name} = (value) => ({{ tag: \"{}\", value }});",
            ctor.name
        );
    }
    let params: Vec<String> = (0..ctor.fields).map(|i| format!("v{i}")).collect();
    let list = params.join(", ");
    let named = ctor.names.len() == ctor.fields && ctor.names.iter().all(|name| name.is_some());
    let fields = if named {
        ctor.names
            .iter()
            .enumerate()
            .map(|(index, name)| format!("{}: v{index}", field_key(name.as_deref().unwrap())))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        (0..ctor.fields)
            .map(|index| format!("value{index}: v{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "export const {name} = ({list}) => ({{ tag: \"{}\", {fields} }});",
        ctor.name
    )
}

fn def_export(def: &AnfDef) -> String {
    let name = js_ident(&def.name);
    let params = def
        .params
        .iter()
        .map(|id| local_name(*id))
        .collect::<Vec<_>>()
        .join(", ");
    // `export function new` is illegal. Define `ply_new` and export it as `new`.
    let keyword = if def.export && name == def.name {
        "export function"
    } else {
        "function"
    };
    let mut body = String::new();
    write_expr(&mut body, &def.body, 1, true);
    let mut out = format!("{keyword} {name}({params}) {{\n{body}}}");
    if def.export && name != def.name {
        out.push_str(&format!("\nexport {{ {name} as {} }};", def.name));
    }
    out
}

fn write_expr(out: &mut String, expr: &Expr, indent: usize, ret: bool) {
    for let_ in &expr.lets {
        write_let(out, let_, indent);
    }
    if !ret {
        return;
    }
    let pad = "  ".repeat(indent);
    match expr.result.as_slice() {
        [] => {}
        [one] => {
            out.push_str(&pad);
            out.push_str("return ");
            out.push_str(&local_name(*one));
            out.push_str(";\n");
        }
        many => {
            let names = many
                .iter()
                .map(|id| local_name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&pad);
            out.push_str("return [");
            out.push_str(&names);
            out.push_str("];\n");
        }
    }
}

fn write_let(out: &mut String, let_: &Let, indent: usize) {
    let pad = "  ".repeat(indent);
    match &let_.value {
        Value::If { cond, yes, no } => {
            for id in &let_.binds {
                out.push_str(&pad);
                out.push_str("let ");
                out.push_str(&local_name(*id));
                out.push_str(";\n");
            }
            out.push_str(&pad);
            out.push_str("if (");
            out.push_str(&local_name(*cond));
            out.push_str(") {\n");
            write_branch(out, yes, &let_.binds, indent + 1);
            out.push_str(&pad);
            out.push_str("} else {\n");
            write_branch(out, no, &let_.binds, indent + 1);
            out.push_str(&pad);
            out.push_str("}\n");
        }
        Value::Choose { arms } => {
            for id in &let_.binds {
                out.push_str(&pad);
                out.push_str("let ");
                out.push_str(&local_name(*id));
                out.push_str(";\n");
            }
            write_choice(out, arms, &let_.binds, indent);
        }
        Value::Quote(quote) => {
            out.push_str(&pad);
            out.push_str("const ");
            out.push_str(&local_name(let_.binds[0]));
            out.push_str(" = ");
            out.push_str(&quote_expr(quote, indent));
            out.push_str(";\n");
        }
        other => {
            let rendered = render_value(other);
            out.push_str(&pad);
            match let_.binds.as_slice() {
                [] => {
                    out.push_str(&rendered);
                    out.push_str(";\n");
                }
                [one] => {
                    out.push_str("const ");
                    out.push_str(&local_name(*one));
                    out.push_str(" = ");
                    out.push_str(&rendered);
                    out.push_str(";\n");
                }
                many => {
                    let names = many
                        .iter()
                        .map(|id| local_name(*id))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str("const [");
                    out.push_str(&names);
                    out.push_str("] = ");
                    out.push_str(&rendered);
                    out.push_str(";\n");
                }
            }
        }
    }
}

fn write_branch(out: &mut String, expr: &Expr, binds: &[LocalId], indent: usize) {
    for let_ in &expr.lets {
        write_let(out, let_, indent);
    }
    let pad = "  ".repeat(indent);
    for (bind, result) in binds.iter().zip(&expr.result) {
        out.push_str(&pad);
        out.push_str(&local_name(*bind));
        out.push_str(" = ");
        out.push_str(&local_name(*result));
        out.push_str(";\n");
    }
}

fn write_choice(out: &mut String, arms: &[Choice], binds: &[LocalId], indent: usize) {
    let pad = "  ".repeat(indent);
    if arms.len() == 1 {
        write_branch(out, &arms[0].body, binds, indent);
        return;
    }
    for (index, arm) in arms.iter().enumerate() {
        let last = index + 1 == arms.len();
        out.push_str(&pad);
        if last {
            out.push_str("} else {\n");
            write_branch(out, &arm.body, binds, indent + 1);
            out.push_str(&pad);
            out.push_str("}\n");
            break;
        }
        if index == 0 {
            out.push_str("if (");
        } else {
            out.push_str("} else if (");
        }
        out.push_str(&render_conds(&arm.conds));
        out.push_str(") {\n");
        write_branch(out, &arm.body, binds, indent + 1);
    }
}

fn render_conds(conds: &[Cond]) -> String {
    if conds.is_empty() {
        return "true".to_string();
    }
    conds
        .iter()
        .map(render_cond)
        .collect::<Vec<_>>()
        .join(" && ")
}

fn render_cond(cond: &Cond) -> String {
    let mut path = local_name(cond.root);
    for step in &cond.steps {
        match step {
            Access::Field { width: 1, .. } => path.push_str(".value"),
            Access::Field { index, .. } => path.push_str(&format!(".value{index}")),
            Access::Index(index) => path.push_str(&format!("[{index}]")),
            Access::Prop(name) => path.push_str(&format!(".{}", field_key(name))),
        }
    }
    match &cond.test {
        PatTest::Bool(value) => format!("{path} === {value}"),
        PatTest::Int(lexeme) => format!("{path} === {lexeme}n"),
        PatTest::Number(lexeme) => format!("{path} === {lexeme}"),
        PatTest::String(value) => format!("{path} === {}", js_string(value)),
        PatTest::Tag(tag) => format!("{path}.tag === \"{tag}\""),
        PatTest::Length(len) => format!("{path}.length === {len}"),
        PatTest::Shape(shape) => render_shape(&path, shape, 0),
    }
}

/// `e0`, `e1`, … are the element names. Each array gets the next one.
fn render_shape(path: &str, shape: &ShapeTest, depth: usize) -> String {
    match shape {
        ShapeTest::Num => format!("typeof {path} === \"number\""),
        ShapeTest::Int => format!("typeof {path} === \"bigint\""),
        ShapeTest::Str => format!("typeof {path} === \"string\""),
        ShapeTest::Bool => format!("typeof {path} === \"boolean\""),
        ShapeTest::Array(None) => format!("Array.isArray({path})"),
        ShapeTest::Array(Some(elem)) => {
            let name = format!("e{depth}");
            let inner = render_shape(&name, elem, depth + 1);
            format!("(Array.isArray({path}) && {path}.every(({name}) => {inner}))")
        }
    }
}

fn quote_expr(quote: &QuoteForm, indent: usize) -> String {
    let params = quote
        .params
        .iter()
        .map(|id| local_name(*id))
        .collect::<Vec<_>>()
        .join(", ");
    if quote.body.lets.is_empty() {
        return match quote.body.result.as_slice() {
            [] => format!("({params}) => {{}}"),
            [one] => format!("({params}) => {}", local_name(*one)),
            many => {
                let names = many
                    .iter()
                    .map(|id| local_name(*id))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({params}) => [{names}]")
            }
        };
    }
    let mut body = String::new();
    write_expr(&mut body, &quote.body, indent + 1, true);
    let pad = "  ".repeat(indent);
    format!("({params}) => {{\n{body}{pad}}}")
}

fn render_value(value: &Value) -> String {
    match value {
        Value::Lit(lit) => js_lit(lit),
        Value::Ctor { name, fields } if fields.is_empty() => js_ident(name),
        Value::Ctor { name, fields } => {
            let args = fields
                .iter()
                .map(|id| local_name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({args})", js_ident(name))
        }
        Value::Call { word, args } if word == "#show_cat" => {
            format!("({} + {})", local_name(args[0]), local_name(args[1]))
        }
        Value::Call { word, args } if word == "#hash_mix" => {
            format!("({} * 31n + {})", local_name(args[0]), local_name(args[1]))
        }
        Value::Call { word, args } => {
            let list = args
                .iter()
                .map(|id| local_name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({list})", js_ident(word))
        }
        Value::Apply { fun, args } => {
            let list = args
                .iter()
                .map(|id| local_name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({list})", local_name(*fun))
        }
        Value::Field { base, index, width } => {
            let base = local_name(*base);
            if *width == 1 {
                format!("{base}.value")
            } else {
                format!("{base}.value{index}")
            }
        }
        Value::Index { base, index } => format!("{}[{index}]", local_name(*base)),
        Value::Prop { base, name } => format!("{}.{}", local_name(*base), field_key(name)),
        Value::SetProp { base, name, value } => format!(
            "({{ ...{}, {}: {} }})",
            local_name(*base),
            field_key(name),
            local_name(*value)
        ),
        Value::Array(items) => {
            let list = items
                .iter()
                .map(|id| local_name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            format!("[{list}]")
        }
        Value::Record { fields } => {
            let list = fields
                .iter()
                .map(|(name, id)| format!("{name}: {}", local_name(*id)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({{ {list} }})")
        }
        Value::Dict { fields } => render_dict(fields),
        Value::Quote(_) | Value::If { .. } | Value::Choose { .. } => {
            unreachable!("compound value")
        }
    }
}

/// Words a generated call or dictionary mentions. Method names on a dictionary
/// are properties, not imports.
pub(crate) fn referenced_words(module: &AnfModule) -> Vec<String> {
    let mut names = Vec::new();
    for def in &module.defs {
        collect_words(&def.body, &mut names);
    }
    for place in &module.places {
        collect_words(&place.init.body, &mut names);
    }
    names
}

fn collect_words(expr: &Expr, names: &mut Vec<String>) {
    for let_ in &expr.lets {
        collect_value(&let_.value, names);
    }
}

fn collect_value(value: &Value, names: &mut Vec<String>) {
    match value {
        Value::Call { word, .. } if word.starts_with('#') => {}
        Value::Call { word, .. } => names.push(word.clone()),
        // A constructor is a JavaScript binding (`Leaf` or `Leaf(...)`).
        // A facade that re-exports it still needs that binding when a private
        // word builds the value.
        Value::Ctor { name, .. } => names.push(name.clone()),
        Value::Dict { fields } => {
            for field in fields {
                match &field.value {
                    DictValue::Word(word) => names.push(word.clone()),
                    DictValue::Closure { word, .. } => names.push(word.clone()),
                    DictValue::Local(_) => {}
                }
            }
        }
        Value::Quote(quote) => collect_words(&quote.body, names),
        Value::If { yes, no, .. } => {
            collect_words(yes, names);
            collect_words(no, names);
        }
        Value::Choose { arms } => {
            for arm in arms {
                collect_words(&arm.body, names);
            }
        }
        Value::Lit(_)
        | Value::Apply { .. }
        | Value::Field { .. }
        | Value::Index { .. }
        | Value::Prop { .. }
        | Value::SetProp { .. }
        | Value::Array(_)
        | Value::Record { .. } => {}
    }
}

fn render_dict(fields: &[DictField]) -> String {
    let parts = fields
        .iter()
        .map(|field| match &field.value {
            DictValue::Word(word) => format!("{}: {}", field.name, js_ident(word)),
            DictValue::Local(id) => format!("{}: {}", field.name, local_name(*id)),
            DictValue::Closure {
                params,
                dicts,
                word,
            } => {
                let names = params.iter().map(|id| local_name(*id)).collect::<Vec<_>>();
                let mut call = dicts.iter().map(|id| local_name(*id)).collect::<Vec<_>>();
                call.extend(names.iter().cloned());
                format!(
                    "{}: ({}) => {}({})",
                    field.name,
                    names.join(", "),
                    js_ident(word),
                    call.join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("({{ {parts} }})")
}

fn js_lit(lit: &Lit) -> String {
    match lit {
        Lit::Int(lexeme) => format!("{lexeme}n"),
        Lit::Number(lexeme) => lexeme.clone(),
        Lit::String(value) => js_string(value),
        Lit::Bool(true) => "true".to_string(),
        Lit::Bool(false) => "false".to_string(),
    }
}

fn js_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other.is_control() => out.push_str(&format!("\\u{{{:x}}}", other as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

pub(crate) fn js_ident(name: &str) -> String {
    let mangled = name.replace(':', "$");
    if JS_RESERVED.contains(&mangled.as_str()) {
        format!("ply_{mangled}")
    } else {
        mangled
    }
}

const JS_RESERVED: &[&str] = &[
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
    "NaN",
    "Infinity",
    "arguments",
    "eval",
];

impl fmt::Display for AnfModule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for data in &self.datas {
            for ctor in &data.ctors {
                if !first {
                    writeln!(formatter)?;
                }
                first = false;
                write!(formatter, "ctor {}", ctor.name)?;
                if ctor.fields == 1 {
                    write!(formatter, " value")?;
                } else if ctor.fields > 1 {
                    for index in 0..ctor.fields {
                        write!(formatter, " v{index}")?;
                    }
                }
            }
        }
        for def in &self.defs {
            if !first {
                writeln!(formatter)?;
            }
            first = false;
            let params = def
                .params
                .iter()
                .map(|id| local_name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(formatter, "def {}({params}) {{", def.name)?;
            write_anf_expr(formatter, &def.body, 1)?;
            write!(formatter, "}}")?;
        }
        Ok(())
    }
}

fn write_anf_expr(formatter: &mut fmt::Formatter<'_>, expr: &Expr, indent: usize) -> fmt::Result {
    let pad = "  ".repeat(indent);
    for let_ in &expr.lets {
        write!(formatter, "{pad}")?;
        match let_.binds.as_slice() {
            [] => {}
            [one] => write!(formatter, "{} = ", local_name(*one))?,
            many => {
                let names = many
                    .iter()
                    .map(|id| local_name(*id))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, "{names} = ")?;
            }
        }
        write_anf_value(formatter, &let_.value, indent)?;
        writeln!(formatter)?;
    }
    write!(formatter, "{pad}return")?;
    for id in &expr.result {
        write!(formatter, " {}", local_name(*id))?;
    }
    writeln!(formatter)
}

fn write_anf_value(
    formatter: &mut fmt::Formatter<'_>,
    value: &Value,
    indent: usize,
) -> fmt::Result {
    match value {
        Value::Lit(lit) => match lit {
            Lit::Int(lexeme) => write!(formatter, "{lexeme}n"),
            Lit::Number(lexeme) => write!(formatter, "{lexeme}"),
            Lit::String(text) => write!(formatter, "{:?}", text),
            Lit::Bool(flag) => write!(formatter, "{flag}"),
        },
        Value::Quote(quote) => {
            let params = quote
                .params
                .iter()
                .map(|id| local_name(*id))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(formatter, "[{params}] {{")?;
            write_anf_expr(formatter, &quote.body, indent + 1)?;
            write!(formatter, "{}}}", "  ".repeat(indent))
        }
        Value::Ctor { name, fields } => {
            write!(formatter, "{name}")?;
            for id in fields {
                write!(formatter, " {}", local_name(*id))?;
            }
            Ok(())
        }
        Value::Call { word, args } => {
            write!(formatter, "{word}")?;
            for id in args {
                write!(formatter, " {}", local_name(*id))?;
            }
            Ok(())
        }
        Value::Apply { fun, args } => {
            write!(formatter, "{}(", local_name(*fun))?;
            for (index, id) in args.iter().enumerate() {
                if index > 0 {
                    write!(formatter, ", ")?;
                }
                write!(formatter, "{}", local_name(*id))?;
            }
            write!(formatter, ")")
        }
        Value::Field { base, index, width } => {
            if *width == 1 {
                write!(formatter, "{}.value", local_name(*base))
            } else {
                write!(formatter, "{}.value{index}", local_name(*base))
            }
        }
        Value::Index { base, index } => {
            write!(formatter, "{}[{index}]", local_name(*base))
        }
        Value::Prop { base, name } => write!(formatter, "{}.{}", local_name(*base), name),
        Value::SetProp { base, name, value } => write!(
            formatter,
            "({{ ...{}, {name}: {} }})",
            local_name(*base),
            local_name(*value)
        ),
        Value::Array(items) => {
            write!(formatter, "[")?;
            for (index, id) in items.iter().enumerate() {
                if index > 0 {
                    write!(formatter, ", ")?;
                }
                write!(formatter, "{}", local_name(*id))?;
            }
            write!(formatter, "]")
        }
        Value::Record { fields } => {
            write!(formatter, "({{ ")?;
            for (index, (name, id)) in fields.iter().enumerate() {
                if index > 0 {
                    write!(formatter, ", ")?;
                }
                write!(formatter, "{name}: {}", local_name(*id))?;
            }
            write!(formatter, " }})")
        }
        Value::Dict { fields } => {
            write!(formatter, "{{")?;
            for (index, field) in fields.iter().enumerate() {
                if index > 0 {
                    write!(formatter, ", ")?;
                }
                match &field.value {
                    DictValue::Word(word) => write!(formatter, "{}: {word}", field.name)?,
                    DictValue::Local(id) => {
                        write!(formatter, "{}: {}", field.name, local_name(*id))?
                    }
                    DictValue::Closure {
                        params,
                        dicts,
                        word,
                    } => {
                        let names = params
                            .iter()
                            .map(|id| local_name(*id))
                            .collect::<Vec<_>>()
                            .join(", ");
                        write!(formatter, "{}: [{names}] => {word}", field.name)?;
                        for id in dicts {
                            write!(formatter, " {}", local_name(*id))?;
                        }
                        for id in params {
                            write!(formatter, " {}", local_name(*id))?;
                        }
                    }
                }
            }
            write!(formatter, "}}")
        }
        Value::If { cond, yes, no } => {
            writeln!(formatter, "if {} {{", local_name(*cond))?;
            write_anf_expr(formatter, yes, indent + 1)?;
            writeln!(formatter, "{}}} else {{", "  ".repeat(indent))?;
            write_anf_expr(formatter, no, indent + 1)?;
            write!(formatter, "{}}}", "  ".repeat(indent))
        }
        Value::Choose { arms } => {
            writeln!(formatter, "choose {{")?;
            for arm in arms {
                writeln!(
                    formatter,
                    "{}when {} =>",
                    "  ".repeat(indent + 1),
                    render_conds(&arm.conds)
                )?;
                write_anf_expr(formatter, &arm.body, indent + 2)?;
            }
            write!(formatter, "{}}}", "  ".repeat(indent))
        }
    }
}
