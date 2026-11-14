//! Rewrite of the top-level `exports` object in `package.json`.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::codes::Error;
use crate::paths::module_name_from_path;

use super::{os_error, pkg_error, BuildError};

pub(super) struct ExportEntry {
    pub(super) key: String,
    pub(super) ply: String,
    /// Absent when the module has no `.js`. The `import` condition is omitted.
    pub(super) js: Option<String>,
}

/// What `plylang build --exports` would do to the map.
pub(super) struct ExportChange {
    pub drift: bool,
    pub public: usize,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub path: Vec<String>,
    pub not_exported: Vec<String>,
}

/// Refresh `exports`. Listed modules keep their keys and gain current `ply` and
/// `import` paths. `runtime` is the modules that have a `.js`. The others keep
/// `ply` and lose `import`. A listed module whose file is gone is removed.
/// `publish` names the modules to add. A key that does not name a module of
/// this package stays.
pub(super) fn sync_package_exports(
    root: &Path,
    source: &[String],
    out_rel: &str,
    entry: Option<&str>,
    publish: &[String],
    runtime: &HashSet<String>,
    check_only: bool,
) -> Result<ExportChange, BuildError> {
    let path = root.join("package.json");
    let text = fs::read_to_string(&path).map_err(|error| os_error(&path, error))?;
    let source_set: HashSet<&str> = source.iter().map(String::as_str).collect();
    for name in publish {
        if !source_set.contains(name.as_str()) {
            return Err(BuildError::plain(format!(
                "module {name} is not a source file"
            )));
        }
    }
    let publish_set: HashSet<&str> = publish.iter().map(String::as_str).collect();
    match find_exports(&text) {
        Ok(ExportsSpot::Found { start, end }) => {
            let value = &text[start..end];
            let (next, change) =
                edit_exports_value(value, source, out_rel, entry, &publish_set, runtime)
                    .map_err(|_| pkg_error(&path, &text, Error::PackageJsonInvalid))?;
            if !check_only && change.drift && next != value {
                let mut updated = String::new();
                updated.push_str(&text[..start]);
                updated.push_str(&next);
                updated.push_str(&text[end..]);
                fs::write(&path, updated).map_err(|error| os_error(&path, error))?;
            }
            Ok(change)
        }
        Ok(ExportsSpot::Missing { close }) => {
            let mut modules: Vec<&str> = publish.iter().map(String::as_str).collect();
            modules.sort_unstable();
            let mut entries = Vec::new();
            if let Some(name) = entry {
                if modules.binary_search(&name).is_ok() {
                    let (ply, js) = module_target(out_rel, name, runtime);
                    entries.push(ExportEntry {
                        key: ".".to_string(),
                        ply,
                        js,
                    });
                }
            }
            for module in &modules {
                let (ply, js) = module_target(out_rel, module, runtime);
                entries.push(ExportEntry {
                    key: format!("./{module}"),
                    ply,
                    js,
                });
            }
            let added: Vec<String> = entries.iter().map(|entry| entry.key.clone()).collect();
            let not_exported = source
                .iter()
                .filter(|module| !publish_set.contains(module.as_str()))
                .cloned()
                .collect();
            let change = ExportChange {
                drift: !entries.is_empty(),
                public: entries.len(),
                added: added.clone(),
                removed: Vec::new(),
                path: Vec::new(),
                not_exported,
            };
            if !check_only && change.drift {
                let updated = insert_exports(&text, close, &entries);
                fs::write(&path, updated).map_err(|error| os_error(&path, error))?;
            }
            Ok(change)
        }
        Err(JsonError::NotObject) => Err(pkg_error(&path, &text, Error::PackageJsonNotObject)),
        Err(JsonError::Invalid) => Err(pkg_error(&path, &text, Error::PackageJsonInvalid)),
    }
}

fn module_target(
    out_rel: &str,
    module: &str,
    runtime: &HashSet<String>,
) -> (String, Option<String>) {
    let ply = format!("./{out_rel}/{module}.d.ply");
    let js = runtime
        .contains(module)
        .then(|| format!("./{out_rel}/{module}.js"));
    (ply, js)
}

fn module_of_key(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("./")?;
    if rest.is_empty() {
        return None;
    }
    module_name_from_path(&format!("{rest}.ply")).ok()?;
    Some(rest)
}

struct Prop {
    key: String,
    key_at: usize,
    value_at: usize,
    value_end: usize,
}

enum Action {
    Keep,
    Retarget(String),
    Remove,
}

fn edit_exports_value(
    value: &str,
    source: &[String],
    out_rel: &str,
    entry: Option<&str>,
    publish: &HashSet<&str>,
    runtime: &HashSet<String>,
) -> Result<(String, ExportChange), ()> {
    let source_set: HashSet<&str> = source.iter().map(String::as_str).collect();
    let props = object_properties(value)?;
    let mut listed: HashSet<&str> = HashSet::new();
    for prop in &props {
        if let Some(module) = module_of_key(&prop.key) {
            if source_set.contains(module) {
                listed.insert(module);
            }
        }
    }
    for name in publish {
        listed.insert(name);
    }
    let dot = entry
        .filter(|name| listed.contains(name))
        .map(|name| module_target(out_rel, name, runtime));
    let mut actions = Vec::with_capacity(props.len());
    let mut removed = Vec::new();
    let mut path = Vec::new();
    let mut seen_dot = false;
    for prop in &props {
        if prop.key == "." {
            seen_dot = true;
            if let Some((ply, js)) = &dot {
                let raw = &value[prop.value_at..prop.value_end];
                if paths_match(raw, ply, js.as_deref()) {
                    actions.push(Action::Keep);
                } else {
                    path.push(".".to_string());
                    actions.push(Action::Retarget(retarget_value(raw, ply, js.as_deref())));
                }
            } else {
                actions.push(Action::Keep);
            }
            continue;
        }
        if let Some(module) = module_of_key(&prop.key) {
            let raw = &value[prop.value_at..prop.value_end];
            if source_set.contains(module) {
                let (ply, js) = module_target(out_rel, module, runtime);
                if paths_match(raw, &ply, js.as_deref()) {
                    actions.push(Action::Keep);
                } else {
                    path.push(prop.key.clone());
                    actions.push(Action::Retarget(retarget_value(raw, &ply, js.as_deref())));
                }
            } else if has_ply_field(raw) {
                removed.push(prop.key.clone());
                actions.push(Action::Remove);
            } else {
                actions.push(Action::Keep);
            }
            continue;
        }
        actions.push(Action::Keep);
    }
    let mut added_entries = Vec::new();
    if dot.is_some() && !seen_dot {
        let (ply, js) = dot.clone().unwrap();
        added_entries.push(ExportEntry {
            key: ".".to_string(),
            ply,
            js,
        });
    }
    let mut fresh: Vec<&str> = publish
        .iter()
        .copied()
        .filter(|module| {
            !props
                .iter()
                .any(|prop| module_of_key(&prop.key) == Some(*module))
        })
        .collect();
    fresh.sort_unstable();
    for module in fresh {
        let (ply, js) = module_target(out_rel, module, runtime);
        added_entries.push(ExportEntry {
            key: format!("./{module}"),
            ply,
            js,
        });
    }
    let added: Vec<String> = added_entries
        .iter()
        .map(|entry| entry.key.clone())
        .collect();
    let remaining = actions
        .iter()
        .filter(|action| !matches!(action, Action::Remove))
        .count();
    let next = if remaining == 0 {
        if added_entries.is_empty() {
            "{}".to_string()
        } else {
            render_exports(&added_entries, &close_indent(value))
        }
    } else {
        let mut edited = value.to_string();
        let mut edits = Vec::new();
        for (prop, action) in props.iter().zip(actions.iter()) {
            match action {
                Action::Keep => {}
                Action::Retarget(next_value) => {
                    edits.push((prop.value_at, prop.value_end, next_value.clone()));
                }
                Action::Remove => {
                    let (start, end) = removal_range(value, prop.key_at, prop.value_end);
                    edits.push((start, end, String::new()));
                }
            }
        }
        edits.sort_by(|left, right| right.0.cmp(&left.0));
        for (start, end, replacement) in edits {
            edited.replace_range(start..end, &replacement);
        }
        if added_entries.is_empty() {
            edited
        } else {
            let indent = props
                .iter()
                .zip(actions.iter())
                .find(|(_, action)| !matches!(action, Action::Remove))
                .map(|(prop, _)| key_indent(value, prop.key_at))
                .unwrap_or_else(|| "    ".to_string());
            splice_added(&edited, &added_entries, &indent)
        }
    };
    let public = remaining + added_entries.len();
    let not_exported = source
        .iter()
        .filter(|module| !listed.contains(module.as_str()))
        .cloned()
        .collect();
    let change = ExportChange {
        drift: !added.is_empty() || !removed.is_empty() || !path.is_empty(),
        public,
        added,
        removed,
        path,
        not_exported,
    };
    Ok((next, change))
}

fn paths_match(raw: &str, ply: &str, js: Option<&str>) -> bool {
    let Ok(fields) = top_level_string_fields(raw) else {
        return false;
    };
    let ply_ok = fields
        .iter()
        .find(|field| field.name == "ply")
        .is_some_and(|field| field.decoded == ply);
    let js_ok = match js {
        Some(js) => fields
            .iter()
            .find(|field| field.name == "import")
            .is_some_and(|field| field.decoded == js),
        None => fields.iter().all(|field| field.name != "import"),
    };
    ply_ok && js_ok
}

fn has_ply_field(raw: &str) -> bool {
    serde_json::from_str::<Value>(raw)
        .ok()
        .is_some_and(|value| value.get("ply").is_some())
}

fn retarget_value(raw: &str, ply: &str, js: Option<&str>) -> String {
    align_paths(raw, ply, js).unwrap_or_else(|| fresh_target(ply, js))
}

fn fresh_target(ply: &str, js: Option<&str>) -> String {
    match js {
        Some(js) => format!(
            "{{\n      \"ply\": {},\n      \"import\": {}\n    }}",
            json_string(ply),
            json_string(js)
        ),
        None => format!("{{\n      \"ply\": {}\n    }}", json_string(ply)),
    }
}

struct Field {
    name: String,
    lit_start: usize,
    lit_end: usize,
    decoded: String,
}

fn top_level_string_fields(raw: &str) -> Result<Vec<Field>, ()> {
    let mut scan = JsonScan::new(raw);
    scan.skip_ws();
    if scan.bump() != Some('{') {
        return Err(());
    }
    let mut fields = Vec::new();
    loop {
        scan.skip_ws();
        match scan.peek() {
            Some('}') => return Ok(fields),
            Some('"') => {
                let name = scan.parse_string()?;
                scan.skip_ws();
                if scan.bump() != Some(':') {
                    return Err(());
                }
                scan.skip_ws();
                if scan.peek() == Some('"') {
                    let lit_start = scan.index;
                    let decoded = scan.parse_string()?;
                    let lit_end = scan.index;
                    if name == "ply" || name == "import" {
                        fields.push(Field {
                            name,
                            lit_start,
                            lit_end,
                            decoded,
                        });
                    }
                } else {
                    scan.skip_value()?;
                }
                scan.skip_ws();
                if scan.peek() == Some(',') {
                    scan.bump();
                }
            }
            _ => return Err(()),
        }
    }
}

fn align_paths(raw: &str, ply: &str, js: Option<&str>) -> Option<String> {
    let mut text = if js.is_none() {
        strip_named_field(raw, "import")?
    } else {
        raw.to_string()
    };
    let fields = top_level_string_fields(&text).ok()?;
    let ply_field = fields.iter().find(|field| field.name == "ply");
    let import_field = fields.iter().find(|field| field.name == "import");
    let mut reps = Vec::new();
    if let Some(field) = ply_field {
        if field.decoded != ply {
            reps.push((field.lit_start, field.lit_end, json_string(ply)));
        }
    }
    if let Some(js) = js {
        if let Some(field) = import_field {
            if field.decoded != js {
                reps.push((field.lit_start, field.lit_end, json_string(js)));
            }
        }
    }
    reps.sort_by(|left, right| right.0.cmp(&left.0));
    for (start, end, replacement) in reps {
        text.replace_range(start..end, &replacement);
    }
    let need_ply = ply_field.is_none();
    let need_js = js.is_some() && import_field.is_none();
    if need_ply || need_js {
        text = insert_missing_paths(&text, need_ply.then_some(ply), need_js.then(|| js.unwrap()))?;
    }
    Some(text)
}

fn strip_named_field(raw: &str, name: &str) -> Option<String> {
    let props = object_properties(raw).ok()?;
    let Some(prop) = props.iter().find(|prop| prop.key == name) else {
        return Some(raw.to_string());
    };
    let (start, end) = removal_range(raw, prop.key_at, prop.value_end);
    let mut text = raw.to_string();
    text.replace_range(start..end, "");
    Some(text)
}

fn insert_missing_paths(raw: &str, ply: Option<&str>, js: Option<&str>) -> Option<String> {
    let close = raw.rfind('}')?;
    let mut head_end = close;
    while head_end > 0 && raw.as_bytes()[head_end - 1].is_ascii_whitespace() {
        head_end -= 1;
    }
    let key_indent = field_indent_of(raw);
    let mut added = String::new();
    let mut first = true;
    for (name, value) in [("ply", ply), ("import", js)] {
        let Some(value) = value else {
            continue;
        };
        if !first {
            added.push(',');
        }
        first = false;
        added.push('\n');
        added.push_str(&key_indent);
        added.push('"');
        added.push_str(name);
        added.push_str("\": ");
        added.push_str(&json_string(value));
    }
    let head = &raw[..head_end];
    let close_indent = close_indent(raw);
    let mut out = String::new();
    out.push_str(head);
    if !head.trim_end().ends_with('{') {
        out.push(',');
    }
    out.push_str(&added);
    out.push('\n');
    out.push_str(&close_indent);
    out.push('}');
    Some(out)
}

fn field_indent_of(raw: &str) -> String {
    let mut scan = JsonScan::new(raw);
    scan.skip_ws();
    if scan.bump() != Some('{') {
        return "      ".to_string();
    }
    scan.skip_ws();
    if scan.peek() == Some('"') {
        let line = raw[..scan.index]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let indent: String = raw[line..scan.index]
            .chars()
            .take_while(|ch| *ch == ' ' || *ch == '\t')
            .collect();
        if !indent.is_empty() {
            return format!("{indent}  ");
        }
    }
    "      ".to_string()
}

fn object_properties(value: &str) -> Result<Vec<Prop>, ()> {
    let mut scan = JsonScan::new(value);
    scan.skip_ws();
    if scan.bump() != Some('{') {
        return Err(());
    }
    let mut props = Vec::new();
    loop {
        scan.skip_ws();
        match scan.peek() {
            Some('}') => return Ok(props),
            Some('"') => {
                let key_at = scan.index;
                let key = scan.parse_string()?;
                scan.skip_ws();
                if scan.bump() != Some(':') {
                    return Err(());
                }
                scan.skip_ws();
                let value_at = scan.index;
                scan.skip_value()?;
                let value_end = scan.index;
                props.push(Prop {
                    key,
                    key_at,
                    value_at,
                    value_end,
                });
                scan.skip_ws();
                if scan.peek() == Some(',') {
                    scan.bump();
                }
            }
            _ => return Err(()),
        }
    }
}

fn removal_range(text: &str, key_at: usize, value_end: usize) -> (usize, usize) {
    let after = skip_ascii_ws(text, value_end);
    if text[after..].starts_with(',') {
        let mut end = after + 1;
        let mut seen = end;
        while seen < text.len() && matches!(text.as_bytes()[seen], b' ' | b'\t') {
            seen += 1;
        }
        if text[seen..].starts_with("\r\n") {
            end = seen + 2;
        } else if text[seen..].starts_with('\n') {
            end = seen + 1;
        }
        return (property_start(text, key_at), end);
    }
    let mut index = key_at;
    while index > 0 {
        let ch = text[..index].chars().next_back().unwrap();
        let prev = index - ch.len_utf8();
        if ch.is_whitespace() {
            index = prev;
            continue;
        }
        if ch == ',' {
            return (prev, value_end);
        }
        break;
    }
    (property_start(text, key_at), value_end)
}

fn property_start(text: &str, key_at: usize) -> usize {
    let line = text[..key_at]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let brace = text.find('{').unwrap_or(0) + '{'.len_utf8();
    line.max(brace)
}

fn skip_ascii_ws(text: &str, mut index: usize) -> usize {
    while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

fn key_indent(text: &str, key_at: usize) -> String {
    let line = text[..key_at]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let indent: String = text[line..key_at]
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect();
    if indent.is_empty() {
        "    ".to_string()
    } else {
        indent
    }
}

fn close_indent(value: &str) -> String {
    let Some(close) = value.rfind('}') else {
        return "  ".to_string();
    };
    let line = value[..close]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let indent: String = value[line..close]
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect();
    if indent.is_empty() {
        "  ".to_string()
    } else {
        indent
    }
}

fn splice_added(value: &str, entries: &[ExportEntry], key_indent: &str) -> String {
    let Some(close) = value.rfind('}') else {
        return value.to_string();
    };
    let mut head_end = close;
    while head_end > 0 && value.as_bytes()[head_end - 1].is_ascii_whitespace() {
        head_end -= 1;
    }
    let added = render_added(entries, key_indent);
    let head = &value[..head_end];
    let mut out = String::new();
    out.push_str(head);
    if !head.trim_end().ends_with('{') {
        out.push(',');
    }
    out.push('\n');
    out.push_str(&added);
    out.push('\n');
    out.push_str(&close_indent(value));
    out.push('}');
    out
}

fn render_added(entries: &[ExportEntry], key_indent: &str) -> String {
    let field_indent = format!("{key_indent}  ");
    let mut out = String::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str(key_indent);
        out.push_str(&json_string(&entry.key));
        out.push_str(": {\n");
        out.push_str(&field_indent);
        out.push_str("\"ply\": ");
        out.push_str(&json_string(&entry.ply));
        if let Some(js) = &entry.js {
            out.push_str(",\n");
            out.push_str(&field_indent);
            out.push_str("\"import\": ");
            out.push_str(&json_string(js));
        }
        out.push('\n');
        out.push_str(key_indent);
        out.push('}');
    }
    out
}

enum ExportsSpot {
    Found { start: usize, end: usize },
    Missing { close: usize },
}

#[derive(Debug)]
enum JsonError {
    NotObject,
    Invalid,
}

/// The last top-level `"exports"` wins, matching `JSON.parse`.
fn find_exports(text: &str) -> Result<ExportsSpot, JsonError> {
    let mut scan = JsonScan::new(text);
    scan.skip_ws();
    if scan.bump() != Some('{') {
        return Err(JsonError::NotObject);
    }
    let mut found = None;
    loop {
        scan.skip_ws();
        match scan.peek() {
            Some('}') => {
                return Ok(match found {
                    Some((start, end)) => ExportsSpot::Found { start, end },
                    None => ExportsSpot::Missing { close: scan.index },
                });
            }
            Some('"') => {
                let name = scan.parse_string().map_err(|_| JsonError::Invalid)?;
                scan.skip_ws();
                if scan.bump() != Some(':') {
                    return Err(JsonError::Invalid);
                }
                scan.skip_ws();
                let start = scan.index;
                scan.skip_value().map_err(|_| JsonError::Invalid)?;
                if name == "exports" {
                    found = Some((start, scan.index));
                }
                scan.skip_ws();
                if scan.peek() == Some(',') {
                    scan.bump();
                }
            }
            _ => return Err(JsonError::Invalid),
        }
    }
}

fn insert_exports(text: &str, close: usize, entries: &[ExportEntry]) -> String {
    let indent = object_indent(text);
    let value = render_exports(entries, &indent);
    let head = text[..close].trim_end();
    let mut out = String::new();
    out.push_str(head);
    if !head.ends_with('{') && !head.ends_with(',') {
        out.push(',');
    }
    out.push('\n');
    out.push_str(&indent);
    out.push_str("\"exports\": ");
    out.push_str(&value);
    out.push('\n');
    out.push('}');
    out.push_str(&text[close + '}'.len_utf8()..]);
    out
}

fn render_exports(entries: &[ExportEntry], indent: &str) -> String {
    if entries.is_empty() {
        return "{}".to_string();
    }
    let key_indent = format!("{indent}{indent}");
    let field_indent = format!("{key_indent}{indent}");
    let mut out = String::from("{\n");
    for (index, entry) in entries.iter().enumerate() {
        out.push_str(&key_indent);
        out.push_str(&json_string(&entry.key));
        out.push_str(": {\n");
        out.push_str(&field_indent);
        out.push_str("\"ply\": ");
        out.push_str(&json_string(&entry.ply));
        if let Some(js) = &entry.js {
            out.push_str(",\n");
            out.push_str(&field_indent);
            out.push_str("\"import\": ");
            out.push_str(&json_string(js));
        }
        out.push('\n');
        out.push_str(&key_indent);
        out.push('}');
        if index + 1 != entries.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str(indent);
    out.push('}');
    out
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn object_indent(text: &str) -> String {
    for line in text.lines().skip(1) {
        let indent: String = line
            .chars()
            .take_while(|ch| *ch == ' ' || *ch == '\t')
            .collect();
        if !indent.is_empty() {
            return indent;
        }
    }
    "  ".to_string()
}

struct JsonScan<'a> {
    text: &'a str,
    index: usize,
}

impl<'a> JsonScan<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, index: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.text[self.index..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.index += ch.len_utf8();
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn parse_string(&mut self) -> Result<String, ()> {
        if self.bump() != Some('"') {
            return Err(());
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('"') => return Ok(out),
                Some('\\') => {
                    let escaped = match self.bump() {
                        Some('"') => '"',
                        Some('\\') => '\\',
                        Some('/') => '/',
                        Some('b') => '\u{0008}',
                        Some('f') => '\u{000c}',
                        Some('n') => '\n',
                        Some('r') => '\r',
                        Some('t') => '\t',
                        Some('u') => self.unicode_escape()?,
                        _ => return Err(()),
                    };
                    out.push(escaped);
                }
                Some(ch) => out.push(ch),
                None => return Err(()),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ()> {
        let mut hex = String::new();
        for _ in 0..4 {
            let ch = self.bump().ok_or(())?;
            if !ch.is_ascii_hexdigit() {
                return Err(());
            }
            hex.push(ch);
        }
        u32::from_str_radix(&hex, 16).map_err(|_| ())
    }

    /// JSON encodes a scalar outside the BMP as a surrogate pair.
    fn unicode_escape(&mut self) -> Result<char, ()> {
        let code = self.hex4()?;
        if (0xD800..=0xDBFF).contains(&code) {
            if self.bump() != Some('\\') || self.bump() != Some('u') {
                return Err(());
            }
            let low = self.hex4()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(());
            }
            let scalar = 0x10000 + (((code - 0xD800) << 10) | (low - 0xDC00));
            return char::from_u32(scalar).ok_or(());
        }
        if (0xDC00..=0xDFFF).contains(&code) {
            return Err(());
        }
        char::from_u32(code).ok_or(())
    }

    fn skip_value(&mut self) -> Result<(), ()> {
        match self.peek() {
            Some('"') => {
                self.parse_string()?;
                Ok(())
            }
            Some('{') => self.skip_container('{', '}'),
            Some('[') => self.skip_container('[', ']'),
            Some('t') => self.skip_literal("true"),
            Some('f') => self.skip_literal("false"),
            Some('n') => self.skip_literal("null"),
            Some('-' | '0'..='9') => self.skip_number(),
            _ => Err(()),
        }
    }

    fn skip_container(&mut self, open: char, close: char) -> Result<(), ()> {
        if self.bump() != Some(open) {
            return Err(());
        }
        let mut depth = 1;
        while depth > 0 {
            match self.peek() {
                Some('"') => {
                    self.parse_string()?;
                }
                Some(ch) if ch == open => {
                    self.bump();
                    depth += 1;
                }
                Some(ch) if ch == close => {
                    self.bump();
                    depth -= 1;
                }
                Some(_) => {
                    self.bump();
                }
                None => return Err(()),
            }
        }
        Ok(())
    }

    fn skip_literal(&mut self, literal: &str) -> Result<(), ()> {
        if self.text[self.index..].starts_with(literal) {
            self.index += literal.len();
            Ok(())
        } else {
            Err(())
        }
    }

    fn skip_number(&mut self) -> Result<(), ()> {
        if self.peek() == Some('-') {
            self.bump();
        }
        if !self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            return Err(());
        }
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if !self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                return Err(());
            }
            while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                self.bump();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_scan_keeps_surrogate_pairs_and_the_last_exports() {
        let pair = r#"{ "name": "\ud83d\ude00", "exports": { "a": 1 }, "exports": { "b": 2 } }"#;
        let super::ExportsSpot::Found { start, end } = super::find_exports(pair).unwrap() else {
            panic!("expected the exports value");
        };
        assert_eq!(&pair[start..end], r#"{ "b": 2 }"#);
        assert!(matches!(
            super::find_exports("[]"),
            Err(super::JsonError::NotObject)
        ));
        assert!(matches!(
            super::find_exports(r#"{ "name": "\ud800" }"#),
            Err(super::JsonError::Invalid)
        ));
        assert!(matches!(
            super::find_exports("{"),
            Err(super::JsonError::Invalid)
        ));
    }
}
