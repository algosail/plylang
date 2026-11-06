//! Text of a generated `.d.ply` interface.

use crate::ast::{
    push_doc, scheme_string, type_string, Data, Def, Exports, Instance, Item, Module, Type,
};

pub(super) fn interface_text(module: &Module) -> String {
    let Exports::List(list) = &module.exports else {
        return closed_interface(module);
    };
    let mut out = String::from("language 1\n\n");
    out.push_str(&list.to_string());
    out.push('\n');
    for import in &module.imports {
        out.push('\n');
        out.push_str(&import.to_string());
        out.push('\n');
    }
    for item in &module.items {
        if !interface_keeps(module, item) {
            continue;
        }
        out.push('\n');
        match item {
            Item::Def(def) => push_interface_def(&mut out, def),
            Item::Place(place) => push_interface_place(&mut out, place),
            Item::Data(data) => {
                out.push_str(&interface_data(module, data));
                out.push('\n');
            }
            other => {
                out.push_str(&other.to_string());
                out.push('\n');
            }
        }
    }
    out
}

fn interface_keeps(module: &Module, item: &Item) -> bool {
    let Some(list) = module.exports.list() else {
        return true;
    };
    match item {
        Item::Data(data) => {
            list.has(&data.name) || data.ctors.iter().any(|ctor| list.has(&ctor.name))
        }
        Item::Synonym(synonym) => list.has(&synonym.name),
        Item::Def(def) => crate::surface::js_exports_def(module, def),
        Item::Place(place) => list.has(&place.name),
        Item::Class(class) => {
            list.has(&class.name) || class.methods.iter().any(|method| list.has(&method.name))
        }
        Item::Instance(instance) => !instance_mentions_unlisted(module, instance),
    }
}

/// Constructors absent from `@` are not part of the interface.
/// A listed type with none of its constructors is still mentionable.
fn interface_data(module: &Module, data: &Data) -> String {
    let Some(list) = module.exports.list() else {
        return data.to_string();
    };
    if data.foreign {
        return data.to_string();
    }
    let mut kept = data.clone();
    kept.ctors.retain(|ctor| list.has(&ctor.name));
    if kept.ctors.is_empty() {
        kept.foreign = true;
        kept.newtype = false;
    }
    kept.to_string()
}

fn instance_mentions_unlisted(module: &Module, instance: &Instance) -> bool {
    instance
        .args
        .iter()
        .any(|ty| type_is_unlisted(module, ty))
}

fn type_is_unlisted(module: &Module, ty: &Type) -> bool {
    match ty {
        Type::Var(_) => false,
        Type::Ctor(name) => local_unlisted(module, name),
        Type::App(head, args) => {
            type_is_unlisted(module, head) || args.iter().any(|ty| type_is_unlisted(module, ty))
        }
        Type::Fun(effect) => effect
            .input
            .items
            .iter()
            .chain(&effect.output.items)
            .any(|ty| type_is_unlisted(module, ty)),
        Type::Record(row) => {
            row.fields.iter().any(|(_, ty)| type_is_unlisted(module, ty))
                || row.tail_ty().is_some_and(|ty| type_is_unlisted(module, ty))
        }
    }
}

fn local_unlisted(module: &Module, name: &str) -> bool {
    let local = module.items.iter().any(|item| match item {
        Item::Data(data) => data.name == name,
        Item::Synonym(synonym) => synonym.name == name,
        _ => false,
    });
    local
        && match module.exports.list() {
            Some(list) => !list.has(name),
            None => true,
        }
}

fn push_interface_def(out: &mut String, def: &Def) {
    push_doc(out, &def.doc);
    out.push_str(": ");
    if def.vis.is_foreign() {
        out.push_str("foreign ");
    }
    out.push_str(&def.name);
    out.push_str(" ( ");
    out.push_str(&scheme_string(&def.scheme));
    out.push_str(" )\n");
}

fn push_interface_place(out: &mut String, place: &crate::ast::Place) {
    push_doc(out, &place.doc);
    out.push_str("= ");
    out.push_str(&place.name);
    out.push(' ');
    out.push_str(&type_string(&place.ty));
    out.push('\n');
}

fn closed_interface(module: &Module) -> String {
    let mut out = String::from("language 1\n");
    for import in &module.imports {
        out.push_str(&import.to_string());
        out.push('\n');
    }
    for item in &module.items {
        match item {
            Item::Data(data) => {
                out.push('\n');
                out.push_str(&data.to_string());
                out.push('\n');
            }
            Item::Synonym(synonym) => {
                out.push('\n');
                out.push_str(&synonym.to_string());
                out.push('\n');
            }
            Item::Def(def) => {
                out.push('\n');
                push_interface_def(&mut out, def);
            }
            Item::Place(place) => {
                out.push('\n');
                push_interface_place(&mut out, place);
            }
            Item::Class(class) => {
                out.push('\n');
                out.push_str(&class.to_string());
                out.push('\n');
            }
            Item::Instance(instance) => {
                out.push('\n');
                out.push_str(&instance.to_string());
                out.push('\n');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::parser::parse;
    use crate::paths::ParseMode;

    use super::interface_text;

    #[test]
    fn an_unexported_constructor_is_omitted_from_the_interface() {
        let hidden = parse("@ Box\n\n; Box a = Mk a\n", ParseMode::Source).unwrap();
        let text = interface_text(&hidden);
        assert!(text.contains("; foreign Box a"), "{text}");
        assert!(!text.contains("Mk"), "{text}");
        let shared = parse("@ Box\n\n; Box a = Box a\n", ParseMode::Source).unwrap();
        let text = interface_text(&shared);
        assert!(text.contains("= Box a"), "{text}");
        let one = parse(
            "@ Maybe, Nothing\n\n; Maybe a = Nothing | Just a\n",
            ParseMode::Source,
        )
        .unwrap();
        let text = interface_text(&one);
        assert!(text.contains("Nothing"), "{text}");
        assert!(!text.contains("Just"), "{text}");
    }

}
