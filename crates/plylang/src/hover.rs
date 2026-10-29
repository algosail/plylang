//! Hover is markdown. A named `ply` block is the declared scheme. Prose from the `..`
//! lines above that declaration sits under it. One unwrapped line with `--` and no
//! name is this use. A gap shows only the stack.

use std::path::Path;

use crate::ast::{
    method_line, scheme_string, Ctor, Data, Def, Item, Module, Scheme, Stack, StackEffect,
    StackTail, Synonym, Type,
};
use crate::build::{load_provider, stack_sites};
use crate::builtin;
use crate::check::StackAt;
use crate::lexer::tokenize;
use crate::parser::parse;
use crate::paths::mode_from_path;
use crate::span::Span;
use crate::token::{Token, TokenKind};

pub struct Hover {
    pub span: Span,
    pub contents: String,
}

/// `line` and `character` are the 0-based UTF-16 position LSP sends.
pub fn hover(source: &str, path: &Path, line: u32, character: u32) -> Option<Hover> {
    let offset = offset_at(source, line, character);
    let tokens = tokenize(source).ok()?;
    let path_text = path.to_string_lossy();
    let module = parse(source, mode_from_path(&path_text)).ok()?;
    let sites = stack_sites(path, source);
    if let Some(token) = token_at(&tokens, offset) {
        if let TokenKind::Ident(name) = &token.kind {
            if let Some(contents) = word_hover(path, &module, name, token.span, &sites) {
                return Some(Hover {
                    span: token.span,
                    contents,
                });
            }
        }
    }
    let contents = gap_hover(source, &module, offset, &sites)?;
    Some(Hover {
        span: Span::new(offset as usize, offset as usize),
        contents,
    })
}

struct Declared {
    line: String,
    doc: String,
}

fn word_hover(
    path: &Path,
    module: &Module,
    name: &str,
    span: Span,
    sites: &[StackAt],
) -> Option<String> {
    let declared = declared_line(path, module, name);
    let site = sites.iter().find(|site| site.span == span);
    match (declared, site) {
        (Some(declared), Some(site)) => Some(with_context(&declared, site)),
        (Some(declared), None) => Some(hover_text(&declared.line, &declared.doc, None)),
        (None, Some(site)) => Some(context_only(site)),
        (None, None) => None,
    }
}

fn with_context(declared: &Declared, site: &StackAt) -> String {
    hover_text(&declared.line, &declared.doc, site.here.as_deref())
}

fn hover_text(line: &str, doc: &str, here: Option<&str>) -> String {
    let mut parts = vec![fence(line)];
    if !doc.is_empty() {
        parts.push(doc.replace("```", "\\`\\`\\`"));
    }
    if let Some(here) = here {
        parts.push(fence(here));
    }
    parts.join("\n\n")
}

fn fence(text: &str) -> String {
    format!("```ply\n{text}\n```")
}

fn context_only(site: &StackAt) -> String {
    match &site.here {
        Some(here) => blocks(&[here.clone()]),
        None => blocks(&[site.before.clone()]),
    }
}

/// One highlighted block per fact. A blank line keeps the fences from merging.
fn blocks(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .map(|part| fence(part))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn declared_line(path: &Path, module: &Module, name: &str) -> Option<Declared> {
    local_scheme(module, name).or_else(|| imported_scheme(path, module, name)).or_else(|| {
        builtin::word(name).map(|word| Declared {
            line: format!(": {name} ( {} )", word.scheme),
            doc: word.doc.to_string(),
        })
    })
}

fn gap_hover(source: &str, module: &Module, offset: u32, sites: &[StackAt]) -> Option<String> {
    let def = module.items.iter().find_map(|item| match item {
        Item::Def(def)
            if (!def.body.is_empty() || !def.clauses.is_empty())
                && def.span.start <= offset && offset <= def.span.end =>
        {
            Some(def)
        }
        _ => None,
    })?;
    let (window_start, window_end) = body_window(source, def)?;
    if offset < window_start || offset > window_end {
        return None;
    }
    let def_sites: Vec<&StackAt> = sites
        .iter()
        .filter(|site| def.span.start <= site.span.start && site.span.end <= def.span.end)
        .collect();
    let text = stack_at_gap(&def_sites, offset)?;
    Some(blocks(&[text]))
}

fn body_window(source: &str, def: &Def) -> Option<(u32, u32)> {
    let (first, last) = if !def.clauses.is_empty() {
        (def.clauses.first()?.span, def.clauses.last()?.span)
    } else {
        (def.body.first()?.span, def.body.last()?.span)
    };
    Some((
        line_start(source, first.start),
        line_end(source, last.end.saturating_sub(1)),
    ))
}

fn stack_at_gap(sites: &[&StackAt], offset: u32) -> Option<String> {
    if let Some(leaf) = sites.iter().copied().find(|site| {
        site.span.start <= offset && offset < site.span.end && is_leaf(site, sites)
    }) {
        return Some(leaf.before.clone());
    }
    let container = sites
        .iter()
        .copied()
        .filter(|site| site.span.start < offset && offset < site.span.end)
        .max_by_key(|site| site.span.start);
    let scope_start = container.map(|site| site.span.start).unwrap_or(0);
    let preceding = sites
        .iter()
        .copied()
        .filter(|site| site.span.start >= scope_start && site.span.end <= offset)
        .filter(|site| site.after.is_some())
        .max_by_key(|site| (site.span.end, site.span.start));
    if let Some(site) = preceding {
        return site.after.clone();
    }
    if let Some(container) = container {
        let first = sites
            .iter()
            .copied()
            .filter(|site| {
                site.span.start > container.span.start && site.span.end <= container.span.end
            })
            .min_by_key(|site| site.span.start)?;
        return Some(first.before.clone());
    }
    let first = sites.iter().copied().min_by_key(|site| site.span.start)?;
    if offset <= first.span.start {
        return Some(first.before.clone());
    }
    None
}

fn is_leaf(site: &StackAt, sites: &[&StackAt]) -> bool {
    !sites.iter().any(|other| {
        other.span != site.span
            && other.span.start >= site.span.start
            && other.span.end <= site.span.end
    })
}

fn line_start(source: &str, offset: u32) -> u32 {
    let offset = (offset as usize).min(source.len());
    source[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0) as u32
}

fn line_end(source: &str, offset: u32) -> u32 {
    let offset = (offset as usize).min(source.len());
    source[offset..]
        .find('\n')
        .map(|index| offset + index)
        .unwrap_or(source.len()) as u32
}

fn local_scheme(module: &Module, name: &str) -> Option<Declared> {
    for item in &module.items {
        if let Item::Def(def) = item {
            if def.name == name {
                return Some(declared_def(def));
            }
        }
    }
    synonym_or_ctor(module, name)
}

fn declared_def(def: &Def) -> Declared {
    Declared {
        line: format!(": {} ( {} )", def.name, scheme_string(&def.scheme)),
        doc: def.doc.clone(),
    }
}

fn imported_scheme(path: &Path, module: &Module, name: &str) -> Option<Declared> {
    for import in &module.imports {
        let wanted = import.all || import.names.iter().any(|imported| imported == name);
        if !wanted {
            continue;
        }
        let Some(provider) = load_provider(path, &import.path) else {
            continue;
        };
        if let Some(declared) = public_scheme(&provider, name) {
            return Some(declared);
        }
    }
    None
}

fn public_scheme(module: &Module, name: &str) -> Option<Declared> {
    for item in &module.items {
        if let Item::Def(def) = item {
            if def.name == name && !def.vis.is_private() {
                return Some(declared_def(def));
            }
        }
    }
    synonym_or_ctor(module, name)
}

fn synonym_or_ctor(module: &Module, name: &str) -> Option<Declared> {
    for item in &module.items {
        if let Item::Data(data) = item {
            for ctor in &data.ctors {
                if ctor.name == name {
                    return Some(Declared {
                        line: ctor_scheme(data, ctor),
                        doc: data.doc.clone(),
                    });
                }
            }
        }
    }
    for item in &module.items {
        if let Item::Data(data) = item {
            if data.foreign && data.name == name {
                return Some(Declared {
                    line: foreign_type_line(data),
                    doc: data.doc.clone(),
                });
            }
        }
    }
    for item in &module.items {
        if let Item::Synonym(synonym) = item {
            if synonym.name == name {
                return Some(declared_synonym(synonym));
            }
        }
    }
    class_scheme(module, name)
}

fn foreign_type_line(data: &Data) -> String {
    let mut out = format!("; foreign {}", data.name);
    for param in &data.params {
        out.push(' ');
        out.push_str(param);
    }
    out
}

fn declared_synonym(synonym: &Synonym) -> Declared {
    Declared {
        line: synonym_line(synonym),
        doc: synonym.doc.clone(),
    }
}

fn synonym_line(synonym: &Synonym) -> String {
    let mut out = format!("- {}", synonym.name);
    for param in &synonym.params {
        out.push(' ');
        out.push_str(param);
    }
    out.push(' ');
    out.push_str(&crate::ast::type_string(&synonym.rhs));
    out
}

fn class_scheme(module: &Module, name: &str) -> Option<Declared> {
    for item in &module.items {
        if let Item::Class(class) = item {
            if class.name == name {
                return Some(Declared {
                    line: format!("& {} {}", class.name, class.params.join(" ")),
                    doc: class.doc.clone(),
                });
            }
            if let Some(method) = class.methods.iter().find(|method| method.name == name) {
                return Some(Declared {
                    line: method_line(class, method),
                    doc: class.doc.clone(),
                });
            }
        }
    }
    None
}

fn ctor_scheme(data: &Data, ctor: &Ctor) -> String {
    let result = if data.params.is_empty() {
        Type::Ctor(data.name.clone())
    } else {
        Type::App(
            Box::new(Type::Ctor(data.name.clone())),
            data.params
                .iter()
                .map(|param| Type::Var(param.clone()))
                .collect(),
        )
    };
    let scheme = Scheme {
        vars: Vec::new(),
        constraints: Vec::new(),
        effect: StackEffect {
            input: Stack {
                tail: StackTail::Implicit,
                items: ctor.field_tys(),
            },
            output: Stack {
                tail: StackTail::Implicit,
                items: vec![result],
            },
            effects: None,
        },
    };
    format!("{} : {}", ctor.name, scheme_string(&scheme))
}

fn token_at(tokens: &[Token], offset: u32) -> Option<&Token> {
    tokens.iter().find(|token| {
        matches!(token.kind, TokenKind::Ident(_))
            && token.span.start <= offset
            && offset < token.span.end
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::hover;

    #[test]
    fn hovers_a_class_method_with_its_constraint() {
        let source = "& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqInt\n\n: use ( Int Int -- Bool )\n  eq\n";
        let declared = hover(source, Path::new("M.ply"), 1, 2).unwrap();
        assert_eq!(
            declared.contents,
            "```ply\neq ( Eq a => a a -- Bool )\n```"
        );
        let use_at = at_offset(source, source.rfind("\n  eq").unwrap() + 3);
        let used = hover(source, Path::new("M.ply"), use_at.0, use_at.1).unwrap();
        assert_eq!(
            used.contents,
            "```ply\neq ( Eq a => a a -- Bool )\n```\n\n```ply\nInt Int -- Bool\n```"
        );
    }

    #[test]
    fn hovers_a_class_doc_on_the_name_and_the_method() {
        let source = ".. Equality.\n& Eq a\n  eq ( a a -- Bool )\n\n: eqInt ( Int Int -- Bool )\n  drop drop true\n\n# Eq Int\n  eq = eqInt\n\n: use ( Int Int -- Bool )\n  eq\n";
        let class_at = at(source, "Eq a");
        let class = hover(source, Path::new("M.ply"), class_at.0, class_at.1).unwrap();
        assert_eq!(class.contents, "```ply\n& Eq a\n```\n\nEquality.");
        let method_at = at(source, "eq (");
        let method = hover(source, Path::new("M.ply"), method_at.0, method_at.1).unwrap();
        assert_eq!(
            method.contents,
            "```ply\neq ( Eq a => a a -- Bool )\n```\n\nEquality."
        );
        let use_at = at_offset(source, source.rfind("\n  eq").unwrap() + 3);
        let used = hover(source, Path::new("M.ply"), use_at.0, use_at.1).unwrap();
        assert_eq!(
            used.contents,
            "```ply\neq ( Eq a => a a -- Bool )\n```\n\nEquality.\n\n```ply\nInt Int -- Bool\n```"
        );
    }

    #[test]
    fn hovers_a_type_synonym() {
        let source = "; Person = Person name : Str, age : Num\n.. A person.\n- Human Person\n\n: older ( Human -- Human )\n  37 >>age\n";
        let at = at(source, "Human --");
        let hovered = hover(source, Path::new("M.ply"), at.0, at.1).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n- Human Person\n```\n\nA person."
        );
    }

    #[test]
    fn hovers_a_constructor_from_its_data_declaration() {
        let source = "; Maybe a = Nothing | Just a\n";
        let line = source.lines().next().unwrap();
        let nothing = hover(source, Path::new("M.ply"), 0, line.find("Nothing").unwrap() as u32)
            .unwrap();
        assert_eq!(nothing.contents, "```ply\nNothing : -- (Maybe a)\n```");
        let just = hover(source, Path::new("M.ply"), 0, line.find("Just").unwrap() as u32).unwrap();
        assert_eq!(just.contents, "```ply\nJust : a -- (Maybe a)\n```");
    }

    #[test]
    fn hovers_the_instantiated_effect_and_the_stack_before_the_word() {
        let source = ": double ( Int -- Int )\n  dup drop\n";
        let dup = at(source, "dup");
        let hovered = hover(source, Path::new("M.ply"), dup.0, dup.1).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n: dup ( ..s a -- ..s a a )\n```\n\nCopies the top value. On `x y` the result is `x y y`.\n\n```ply\nInt -- Int Int\n```"
        );
        let drop = at(source, "drop");
        let hovered = hover(source, Path::new("M.ply"), drop.0, drop.1).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n: drop ( ..s a -- ..s )\n```\n\nDrops the top value. On `x y` the result is `x`.\n\n```ply\nInt Int -- Int\n```"
        );
        let name = at(source, "double");
        let hovered = hover(source, Path::new("M.ply"), name.0, name.1).unwrap();
        assert_eq!(hovered.contents, "```ply\n: double ( Int -- Int )\n```");
    }

    #[test]
    fn shows_the_doc_between_the_declared_scheme_and_this_use() {
        let source = ".. Copy the top value.\n.. It becomes the new top.\n: double ( Int -- Int )\n  dup drop\n\n: use ( Int -- Int )\n  double\n";
        let name = at(source, "double");
        let hovered = hover(source, Path::new("M.ply"), name.0, name.1).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n: double ( Int -- Int )\n```\n\nCopy the top value.\nIt becomes the new top."
        );
        let call = source.rfind("double").unwrap();
        let (line, character) = at_offset(source, call);
        let hovered = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n: double ( Int -- Int )\n```\n\nCopy the top value.\nIt becomes the new top.\n\n```ply\nInt -- Int\n```"
        );
        let plain = ". not the doc\n: id ( Int -- Int )\n  dup drop\n";
        let id = at(plain, "id");
        let hovered = hover(plain, Path::new("M.ply"), id.0, id.1).unwrap();
        assert_eq!(hovered.contents, "```ply\n: id ( Int -- Int )\n```");
        let detached = ".. gone\n\n: id ( Int -- Int )\n  dup drop\n";
        let id = at(detached, "id");
        let hovered = hover(detached, Path::new("M.ply"), id.0, id.1).unwrap();
        assert_eq!(hovered.contents, "```ply\n: id ( Int -- Int )\n```");
        let data = ".. A value that may be missing.\n; Maybe a = Nothing | Just a\n";
        let nothing = at(data, "Nothing");
        let hovered = hover(data, Path::new("M.ply"), nothing.0, nothing.1).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\nNothing : -- (Maybe a)\n```\n\nA value that may be missing."
        );
    }

    #[test]
    fn hovers_a_foreign_type() {
        let source = ".. A listener list.\n; foreign Hub a\n\n: keep ( (Hub a) -- (Hub a) )\n  hub -> hub\n";
        let declared = at(source, "Hub a");
        let hovered = hover(source, Path::new("M.ply"), declared.0, declared.1).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n; foreign Hub a\n```\n\nA listener list."
        );
        let used = source.rfind("Hub").unwrap();
        let (line, character) = at_offset(source, used);
        let hovered = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n; foreign Hub a\n```\n\nA listener list."
        );
    }

    #[test]
    fn hovers_a_foreign_word() {
        let source = ": foreign read ( -- Str )\n\n: line ( -- Str )\n  read\n";
        let read = source.rfind("read").unwrap();
        let (line_no, character) = at_offset(source, read);
        let hovered = hover(source, Path::new("M.ply"), line_no, character).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n: read ( -- Str )\n```\n\n```ply\n-- Str\n```"
        );
    }

    #[test]
    fn hovers_the_stack_inside_a_clause() {
        let source = ": fact ( Int -- Int )\n  0n -> 1n\n  n -> n dup drop\n";
        let between = at(source, "n dup");
        let gap = hover(
            source,
            Path::new("M.ply"),
            between.0,
            between.1 + "n".len() as u32,
        )
        .unwrap();
        assert_eq!(gap.contents, "```ply\nInt\n```");
    }

    #[test]
    fn hovers_the_stack_in_the_gap() {
        let source = ": double ( Int -- Int )\n  dup drop\n";
        let between = at(source, "dup drop");
        let gap = hover(
            source,
            Path::new("M.ply"),
            between.0,
            between.1 + "dup".len() as u32,
        )
        .unwrap();
        assert_eq!(gap.contents, "```ply\nInt Int\n```");
        let indent = at(source, "dup");
        let before = hover(source, Path::new("M.ply"), indent.0, indent.1 - 1).unwrap();
        assert_eq!(before.contents, "```ply\nInt\n```");
    }

    #[test]
    fn a_quotation_under_if_shows_the_solved_stack() {
        let source = ": fact ( Int -- Int )\n  true [ drop 1n ] [ dup drop ] if\n";
        let dup = source.rfind("dup").unwrap();
        let (line, character) = at_offset(source, dup);
        let hovered = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n: dup ( ..s a -- ..s a a )\n```\n\nCopies the top value. On `x y` the result is `x y y`.\n\n```ply\nInt -- Int Int\n```"
        );
        let drop = at(source, "drop");
        let hovered = hover(source, Path::new("M.ply"), drop.0, drop.1).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n: drop ( ..s a -- ..s )\n```\n\nDrops the top value. On `x y` the result is `x`.\n\n```ply\nInt --\n```"
        );
    }

    #[test]
    fn keeps_a_nested_quotation_arrow_on_one_line() {
        let source = ": around ( Int (Int -- Int) -- (Int -- Int) Int )\n  swap\n";
        let swap = at(source, "swap");
        let hovered = hover(source, Path::new("M.ply"), swap.0, swap.1).unwrap();
        let inferred = hovered
            .contents
            .rsplit("\n\n")
            .next()
            .unwrap()
            .trim_start_matches("```ply\n")
            .trim_end_matches("\n```");
        assert!(!inferred.contains('\n'), "{inferred}");
        assert!(inferred.contains("--"), "{inferred}");
    }

    #[test]
    fn hovers_a_synonym_by_name_on_the_stack() {
        let source = "\
; Person = Person name : Str, age : Num
- Human Person

: older ( Human -- Human )
  37 >>age

: usePerson ( Human -- Human )
  older

; Inner = Inner n : Num, tag : Num
- Wrapped Inner

; Outer = Outer inner : Wrapped, name : Str

: outer ( Outer -- Outer )
  dup drop

: useOuter ( Outer -- Outer )
  outer

; Pair a b = Pair left : a, right : b

: pair ( (Pair Int Str) -- (Pair Int Str) )
  dup drop

- Age Int

: widen ( Age -- Int )
  drop 0n

: useAge ( Age -- Int )
  widen

: plain ( Int -- Int )
  dup drop

: usePlain ( Age -- Int )
  plain

: read ( Outer -- Str )
  name>>

: useRead ( Outer -- Str )
  read
";
        let older = source.rfind("older").unwrap();
        let (line, character) = at_offset(source, older);
        let hovered = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert_eq!(
            hovered.contents,
            "```ply\n: older ( Human -- Human )\n```\n\n```ply\nHuman -- Human\n```"
        );
        let between = source.find("37 >>age").unwrap();
        let (line, character) = at_offset(source, between + "37".len());
        let gap = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert_eq!(gap.contents, "```ply\nHuman Num\n```");

        let outer = source.rfind("outer").unwrap();
        let (line, character) = at_offset(source, outer);
        let hovered = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert!(
            hovered.contents.ends_with("```ply\nOuter -- Outer\n```"),
            "{}",
            hovered.contents
        );

        let dup = source.find("(Pair Int Str) )\n  dup").unwrap();
        let dup = dup + source[dup..].find("dup").unwrap();
        let (line, character) = at_offset(source, dup);
        let hovered = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert!(
            hovered.contents.ends_with(
                "```ply\n(Pair Int Str) -- (Pair Int Str) (Pair Int Str)\n```"
            ),
            "{}",
            hovered.contents
        );

        let widen = source.rfind("widen").unwrap();
        let (line, character) = at_offset(source, widen);
        let hovered = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert!(
            hovered.contents.ends_with("```ply\nAge -- Int\n```"),
            "{}",
            hovered.contents
        );

        let plain = source.rfind("plain").unwrap();
        let (line, character) = at_offset(source, plain);
        let hovered = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert!(
            hovered.contents.ends_with("```ply\nInt -- Int\n```"),
            "{}",
            hovered.contents
        );

        let row = source.rfind("\n  read").unwrap() + 3;
        let (line, character) = at_offset(source, row);
        let hovered = hover(source, Path::new("M.ply"), line, character).unwrap();
        assert!(
            hovered.contents.ends_with("```ply\nOuter -- Str\n```"),
            "{}",
            hovered.contents
        );
    }

    fn at(source: &str, needle: &str) -> (u32, u32) {
        at_offset(source, source.find(needle).unwrap())
    }

    fn at_offset(source: &str, offset: usize) -> (u32, u32) {
        let line = source[..offset].bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = source[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0);
        let character = source[line_start..offset].encode_utf16().count() as u32;
        (line, character)
    }
}

fn offset_at(source: &str, line: u32, character: u32) -> u32 {
    crate::span::byte_offset(source, line, character)
}
