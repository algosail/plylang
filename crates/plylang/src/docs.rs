//! Package documentation. The page copies declaration comments.
//! The only words this pass adds are `Types`, `Classes`, `Instances`, and `Words`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::ast::{
    method_line, scheme_string, type_string, Class, Data, Def, Exports, Instance, Item, Module,
    Synonym,
};
use crate::build::{io_error, located, module_path_of, open_package, os_error, walk_ply, BuildError};
use crate::codes::Error;
use crate::lexer::tokenize;
use crate::parser::parse;
use crate::paths::ParseMode;
use crate::token::TokenKind;

/// Write `DOCS.md` in the package root, or `out` when it is set.
pub fn write_docs(dir: &Path, out: Option<&Path>) -> Result<(), BuildError> {
    let package = open_package(dir)?;
    let markdown = render(&package)?;
    let dest = match out {
        Some(path) => path.to_path_buf(),
        None => package.root.join("DOCS.md"),
    };
    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|error| os_error(parent, error))?;
        }
    }
    fs::write(&dest, markdown).map_err(|error| os_error(&dest, error))
}

fn render(package: &crate::build::Package) -> Result<String, BuildError> {
    if !package.source.is_dir() {
        return Err(io_error(&package.source, Error::SourceDirectoryNotFound));
    }
    let mut files = Vec::new();
    walk_ply(&package.source, &package.out, &mut files)
        .map_err(|error| os_error(&package.source, error))?;
    let mut pages = Vec::new();
    for file in files {
        let source = fs::read_to_string(&file).map_err(|error| os_error(&file, error))?;
        let module = parse(&source, ParseMode::Source)
            .map_err(|error| located(&file, &source, error))?;
        let module_path = module_path_of(&package.source, &file);
        if let Err(message) = crate::paths::module_name_from_path(&format!("{module_path}.ply")) {
            return Err(located(
                &file,
                &source,
                crate::diagnostic::Diagnostic::new(module.span, message),
            ));
        }
        let description = module_description(&source)
            .map_err(|error| located(&file, &source, error))?;
        pages.push(Page {
            path: module_path,
            file,
            description,
            module,
        });
    }
    pages.sort_by(|left, right| {
        let depth = |path: &str| path.matches('/').count();
        depth(&left.path)
            .cmp(&depth(&right.path))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(render_pages(package.name.as_deref(), &pages))
}

struct Page {
    path: String,
    file: PathBuf,
    description: String,
    module: Module,
}

fn render_pages(package_name: Option<&str>, pages: &[Page]) -> String {
    let mut seen = HashMap::new();
    let headings: Vec<(String, String)> = pages
        .iter()
        .map(|page| {
            let title = import_path(package_name, &page.path);
            let anchor = anchor(&title, &mut seen);
            (title, anchor)
        })
        .collect();
    let mut out = String::new();
    if let Some(name) = package_name {
        out.push_str("# ");
        out.push_str(name);
        out.push_str("\n\n");
    }
    for (title, anchor) in &headings {
        out.push_str("- [");
        out.push_str(title);
        out.push_str("](#");
        out.push_str(anchor);
        out.push_str(")\n");
    }
    if !headings.is_empty() {
        out.push('\n');
    }
    for (page, (title, _)) in pages.iter().zip(&headings) {
        render_page(&mut out, title, page, &mut seen);
    }
    out
}

fn render_page(out: &mut String, title: &str, page: &Page, seen: &mut HashMap<String, usize>) {
    out.push_str("## ");
    out.push_str(title);
    out.push_str("\n\n");
    push_prose(out, &page.description);
    let mut types = Vec::new();
    let mut classes = Vec::new();
    let mut instances = Vec::new();
    let mut words = Vec::new();
    for item in &page.module.items {
        match item {
            Item::Data(data) if docs_data(&page.module, data) => {
                types.push((data.name.clone(), format_data(data), data.doc.clone()))
            }
            Item::Synonym(synonym) if docs_name(&page.module, &synonym.name) => types.push((
                synonym.name.clone(),
                format_synonym(synonym),
                synonym.doc.clone(),
            )),
            Item::Class(class) if docs_class(&page.module, class) => {
                classes.push((class.name.clone(), format_class(class), class.doc.clone()))
            }
            Item::Instance(instance) if !matches!(page.module.exports, Exports::Closed) => {
                instances.push(instance_head(instance))
            }
            Item::Def(def) if docs_def(&page.module, def) => {
                words.push((def.name.clone(), format_word(def), def.doc.clone()))
            }
            _ => {}
        }
    }
    if matches!(page.module.exports, Exports::List(_)) {
        append_published(&page.file, &page.module, &mut types, &mut classes, &mut instances, &mut words);
    }
    render_group(out, "Types", &types, seen);
    render_group(out, "Classes", &classes, seen);
    render_instances(out, &instances);
    render_group(out, "Words", &words, seen);
}

fn docs_name(module: &Module, name: &str) -> bool {
    match &module.exports {
        Exports::List(list) => list.has(name),
        Exports::Closed => false,
    }
}

fn docs_data(module: &Module, data: &Data) -> bool {
    docs_name(module, &data.name) || data.ctors.iter().any(|ctor| docs_name(module, &ctor.name))
}

fn docs_class(module: &Module, class: &Class) -> bool {
    docs_name(module, &class.name)
        || class.methods.iter().any(|method| docs_name(module, &method.name))
}

fn docs_def(module: &Module, def: &Def) -> bool {
    match &module.exports {
        Exports::List(list) => list.has(&def.name),
        Exports::Closed => false,
    }
}

fn append_published(
    file: &Path,
    module: &Module,
    types: &mut Vec<(String, String, String)>,
    classes: &mut Vec<(String, String, String)>,
    instances: &mut Vec<String>,
    words: &mut Vec<(String, String, String)>,
) {
    let names = crate::surface::public_names(module);
    let mut seen = HashSet::new();
    for (name, _, _) in types.iter().chain(classes.iter()).chain(words.iter()) {
        seen.insert(name.clone());
    }
    for name in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        let mut load = |from: &Path, path: &str| crate::build::load_surface(from, path);
        let Some((_, found)) = crate::surface::lookup(module, file, &name, &mut load) else {
            continue;
        };
        match found {
            crate::surface::Found::Data(data) => {
                if types.iter().any(|(have, _, _)| have == &data.name) {
                    continue;
                }
                seen.insert(data.name.clone());
                types.push((data.name.clone(), format_data(&data), data.doc.clone()));
            }
            crate::surface::Found::Synonym(synonym) => {
                types.push((synonym.name.clone(), format_synonym(&synonym), synonym.doc.clone()));
            }
            crate::surface::Found::Class(class) => {
                if classes.iter().any(|(have, _, _)| have == &class.name) {
                    continue;
                }
                seen.insert(class.name.clone());
                classes.push((class.name.clone(), format_class(&class), class.doc.clone()));
            }
            crate::surface::Found::Def(def) => {
                words.push((def.name.clone(), format_word(&def), def.doc.clone()));
            }
        }
    }
    let mut load = |from: &Path, path: &str| crate::build::load_surface(from, path);
    for instance in crate::surface::instances(module, file, &mut load) {
        let head = instance_head(&instance);
        if !instances.iter().any(|have| have == &head) {
            instances.push(head);
        }
    }
}

fn render_group(
    out: &mut String,
    title: &str,
    items: &[(String, String, String)],
    seen: &mut HashMap<String, usize>,
) {
    if items.is_empty() {
        return;
    }
    out.push_str("### ");
    out.push_str(title);
    out.push_str("\n\n");
    for (name, signature, doc) in items {
        render_item(out, name, signature, doc, seen);
    }
}

fn render_instances(out: &mut String, heads: &[String]) {
    if heads.is_empty() {
        return;
    }
    out.push_str("### Instances\n\n");
    for head in heads {
        out.push_str("- ");
        out.push_str(head);
        out.push('\n');
    }
    out.push('\n');
}

fn render_item(
    out: &mut String,
    name: &str,
    signature: &str,
    doc: &str,
    seen: &mut HashMap<String, usize>,
) {
    let _ = anchor(name, seen);
    out.push_str("#### ");
    out.push_str(name);
    out.push_str("\n\n```ply\n");
    out.push_str(signature);
    out.push_str("\n```\n\n");
    push_prose(out, doc);
}

fn push_prose(out: &mut String, doc: &str) {
    if doc.is_empty() {
        return;
    }
    out.push_str(doc);
    out.push_str("\n\n");
}

fn import_path(package_name: Option<&str>, module_path: &str) -> String {
    match package_name {
        Some(name) => format!("{name}/{module_path}"),
        None => module_path.to_string(),
    }
}

/// A leading `..` block with a blank line after it. A block glued to the
/// next declaration stays on that declaration.
fn module_description(source: &str) -> Result<String, crate::diagnostic::Diagnostic> {
    let tokens = tokenize(source)?;
    let mut index = 0;
    while matches!(tokens.get(index).map(|token| &token.kind), Some(TokenKind::Newline)) {
        index += 1;
    }
    let mut lines = Vec::new();
    while let Some(TokenKind::Doc(text)) = tokens.get(index).map(|token| &token.kind) {
        lines.push(text.clone());
        index += 1;
        let mut breaks = 0;
        while matches!(tokens.get(index).map(|token| &token.kind), Some(TokenKind::Newline)) {
            breaks += 1;
            index += 1;
        }
        let next_is_doc = matches!(tokens.get(index).map(|token| &token.kind), Some(TokenKind::Doc(_)));
        if breaks == 1 && next_is_doc {
            continue;
        }
        if breaks >= 2 || tokens.get(index).map(|token| &token.kind).is_none_or(|kind| matches!(kind, TokenKind::Eof)) {
            return Ok(join_doc(&lines));
        }
        return Ok(String::new());
    }
    Ok(String::new())
}

fn join_doc(lines: &[String]) -> String {
    if lines.iter().all(|line| line.is_empty()) {
        String::new()
    } else {
        lines.join("\n")
    }
}

fn format_word(def: &Def) -> String {
    let mut out = String::from(": ");
    if def.vis.is_foreign() {
        out.push_str("foreign ");
    }
    out.push_str(&def.name);
    out.push_str(" ( ");
    out.push_str(&scheme_string(&def.scheme));
    out.push_str(" )");
    out
}

fn format_data(data: &Data) -> String {
    let mut out = String::from("; ");
    if data.foreign {
        out.push_str("foreign ");
    }
    out.push_str(&data.name);
    for param in &data.params {
        out.push(' ');
        out.push_str(param);
    }
    if data.foreign {
        return out;
    }
    if data.newtype {
        out.push_str(" == ");
    } else {
        out.push_str(" = ");
    }
    for (index, ctor) in data.ctors.iter().enumerate() {
        if index > 0 {
            out.push_str(" | ");
        }
        out.push_str(&ctor.name);
        if ctor.fields.iter().any(|field| field.name.is_some()) {
            for (index, field) in ctor.fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push(' ');
                out.push_str(field.name.as_deref().unwrap_or("_"));
                out.push_str(" : ");
                out.push_str(&type_string(&field.ty));
            }
        } else {
            for field in &ctor.fields {
                out.push(' ');
                out.push_str(&type_string(&field.ty));
            }
        }
    }
    out
}

fn format_synonym(synonym: &Synonym) -> String {
    let mut out = format!("- {}", synonym.name);
    for param in &synonym.params {
        out.push(' ');
        out.push_str(param);
    }
    out.push(' ');
    out.push_str(&type_string(&synonym.rhs));
    out
}

fn format_class(class: &Class) -> String {
    let mut out = String::from("& ");
    if !class.supers.is_empty() {
        for (index, constraint) in class.supers.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&constraint.class_name);
            for arg in &constraint.args {
                out.push(' ');
                out.push_str(&type_string(arg));
            }
        }
        out.push_str(" => ");
    }
    out.push_str(&class.name);
    for param in &class.params {
        out.push(' ');
        out.push_str(param);
    }
    for method in &class.methods {
        out.push_str("\n  ");
        out.push_str(&method_line(class, method));
    }
    out
}

fn instance_head(instance: &Instance) -> String {
    let mut out = instance.class_names.join(", ");
    for arg in &instance.args {
        out.push(' ');
        out.push_str(&type_string(arg));
    }
    out
}

fn anchor(title: &str, seen: &mut HashMap<String, usize>) -> String {
    let mut slug = String::new();
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if ch == ' ' || ch == '-' {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() {
        "section".to_string()
    } else {
        slug
    };
    let count = seen.entry(slug.clone()).or_insert(0);
    let anchor = if *count == 0 {
        slug
    } else {
        format!("{slug}-{count}")
    };
    *count += 1;
    anchor
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::write_docs;

    struct Temp(PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "plylang-docs-{label}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, text).unwrap();
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn package_json(name: &str) -> String {
        format!(
            r#"{{
  "name": "{name}",
  "ply": {{ "source": "src", "out": "dist", "language": 1 }}
}}"#
        )
    }

    #[test]
    fn docs_lists_modules_by_depth_and_keeps_comments() {
        let temp = Temp::new("shelf");
        temp.write("package.json", &package_json("shelf"));
        temp.write(
            "src/Age.ply",
            ".. An age in years.\n\n@ Age, older, zero\n\n.. A wrapper.\n; Age == Age Num\n\n.. One year older.\n: older ( Age -- Age )\n  Age -> 1 add Age\n\n.. True when the number is zero.\n: foreign zero ( Num -- Bool )\n\n.. Hidden.\n: step ( Num -- Num )\n  1 add\n",
        );
        temp.write(
            "src/Equal.ply",
            ".. Sameness of two values.\n\n@ Equal, eq\n\n& Equal a\n  eq ( a a -- Bool )\n\n# Equal Num\n  eq = eqNum\n\n# Equal, Sortable Int\n  eq = eqInt\n  compare = compareInt\n",
        );
        temp.write(
            "src/Data/List.ply",
            ".. Finite sequences.\n\n@ List, Nil, Cons, nil, length\n\n; List a = Nil | Cons head : a, tail : (List a)\n\n: nil ( -- (List a) )\n  Nil\n\n.. How many elements.\n: foreign length ( (List a) -- Num )\n",
        );
        temp.write(
            "src/Data/Maybe.ply",
            "@ Maybe, Nothing, Just, fromMaybe\n\n.. Nothing is absence. Just carries one field.\n..\n.. The type name is not a constructor.\n; Maybe a = Nothing | Just a\n\n.. The spare value stays.\n: fromMaybe ( a (Maybe a) -- a )\n  Nothing ->\n  Just -> nip\n\n# Equal (Maybe a)\n  eq = eqMaybe\n",
        );
        temp.write(
            "src/Data/List/NonEmpty.ply",
            ".. At least one element.\n\n@ NonEmpty, head\n\n; NonEmpty a = NonEmpty head : a, tail : (List a)\n\n.. The first element.\n: head ( (NonEmpty a) -- a )\n  head>>\n",
        );
        write_docs(&temp.0, None).unwrap_or_else(|error| panic!("{}", error.render()));
        let page = fs::read_to_string(temp.0.join("DOCS.md")).unwrap();
        assert_eq!(page, EXPECTED);

        let custom = temp.0.join("api.md");
        write_docs(&temp.0, Some(&custom)).unwrap_or_else(|error| panic!("{}", error.render()));
        assert_eq!(fs::read_to_string(&custom).unwrap(), EXPECTED);
    }

    #[test]
    fn a_public_import_keeps_the_original_comment() {
        let temp = Temp::new("door");
        temp.write("package.json", &package_json("door"));
        temp.write(
            "src/Lib.ply",
            "@ answer\n\n.. A number.\n: answer ( -- Num )\n  1\n",
        );
        temp.write(
            "src/Door.ply",
            ".. The door.\n\n@ answer\n\n~ \"Lib\" (answer)\n",
        );
        write_docs(&temp.0, None).unwrap_or_else(|error| panic!("{}", error.render()));
        let page = fs::read_to_string(temp.0.join("DOCS.md")).unwrap();
        assert!(page.contains("## door/Door\n"), "{page}");
        assert!(page.contains("The door."), "{page}");
        assert!(page.contains("#### answer\n"), "{page}");
        assert!(page.contains(": answer ( -- Num )"), "{page}");
        assert!(page.contains("A number."), "{page}");
    }

    #[test]
    fn a_package_without_a_name_uses_the_module_path() {
        let temp = Temp::new("noname");
        temp.write(
            "package.json",
            r#"{ "ply": { "source": "src", "out": "dist", "language": 1 } }"#,
        );
        temp.write(
            "src/Data/Maybe.ply",
            "@ Maybe, Nothing, Just\n\n; Maybe a = Nothing | Just a\n",
        );
        let out = temp.0.join("out").join("API.md");
        write_docs(&temp.0, Some(&out)).unwrap_or_else(|error| panic!("{}", error.render()));
        let page = fs::read_to_string(&out).unwrap();
        assert!(page.starts_with("- [Data/Maybe](#datamaybe)\n"), "{page}");
        assert!(page.contains("## Data/Maybe\n"), "{page}");
        assert!(!page.contains("# shelf"), "{page}");
        assert!(!temp.0.join("DOCS.md").exists());
    }

    const EXPECTED: &str = r#"# shelf

- [shelf/Age](#shelfage)
- [shelf/Equal](#shelfequal)
- [shelf/Data/List](#shelfdatalist)
- [shelf/Data/Maybe](#shelfdatamaybe)
- [shelf/Data/List/NonEmpty](#shelfdatalistnonempty)

## shelf/Age

An age in years.

### Types

#### Age

```ply
; Age == Age Num
```

A wrapper.

### Words

#### older

```ply
: older ( Age -- Age )
```

One year older.

#### zero

```ply
: foreign zero ( Num -- Bool )
```

True when the number is zero.

## shelf/Equal

Sameness of two values.

### Classes

#### Equal

```ply
& Equal a
  eq ( Equal a => a a -- Bool )
```

### Instances

- Equal Num
- Equal, Sortable Int

## shelf/Data/List

Finite sequences.

### Types

#### List

```ply
; List a = Nil | Cons head : a, tail : (List a)
```

### Words

#### nil

```ply
: nil ( -- (List a) )
```

#### length

```ply
: foreign length ( (List a) -- Num )
```

How many elements.

## shelf/Data/Maybe

### Types

#### Maybe

```ply
; Maybe a = Nothing | Just a
```

Nothing is absence. Just carries one field.

The type name is not a constructor.

### Instances

- Equal (Maybe a)

### Words

#### fromMaybe

```ply
: fromMaybe ( a (Maybe a) -- a )
```

The spare value stays.

## shelf/Data/List/NonEmpty

At least one element.

### Types

#### NonEmpty

```ply
; NonEmpty a = NonEmpty head : a, tail : (List a)
```

### Words

#### head

```ply
: head ( (NonEmpty a) -- a )
```

The first element.

"#;
}
