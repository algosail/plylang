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
    pub(super) js: String,
}

/// Replace the top-level `exports` value. Every other byte of `package.json` stays.
pub(super) fn write_package_exports(root: &Path, entries: &[ExportEntry]) -> Result<(), BuildError> {
    let path = root.join("package.json");
    let text = fs::read_to_string(&path).map_err(|error| os_error(&path, error))?;
    let updated = match find_exports(&text) {
        Ok(ExportsSpot::Found { start, end }) => {
            let indent = line_indent(&text, start);
            let existing = serde_json::from_str::<Value>(&text[start..end]).unwrap_or(Value::Null);
            let mut out = String::new();
            out.push_str(&text[..start]);
            out.push_str(&render_merged(entries, &existing, &indent));
            out.push_str(&text[end..]);
            out
        }
        Ok(ExportsSpot::Missing { close }) => insert_exports(&text, close, entries),
        Err(JsonError::NotObject) => {
            return Err(pkg_error(&path, &text, Error::PackageJsonNotObject))
        }
        Err(JsonError::Invalid) => {
            return Err(pkg_error(&path, &text, Error::PackageJsonInvalid))
        }
    };
    fs::write(&path, updated).map_err(|error| os_error(&path, error))
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

/// Tool-owned module keys are replaced. A stale `{ ply, import }` key for a
/// module that is gone is removed. `.` is replaced only when `entries` names
/// it, which is when `ply.entry` is set. Every other key stays.
fn render_merged(entries: &[ExportEntry], existing: &Value, indent: &str) -> String {
    let preserved = preserved_exports(existing, entries);
    if preserved.is_empty() {
        return render_exports(entries, indent);
    }
    if entries.is_empty() {
        return render_preserved_only(&preserved, indent);
    }
    let owned = render_exports(entries, indent);
    let key_indent = format!("{indent}{indent}");
    let close = format!("\n{indent}}}");
    let Some(head) = owned.strip_suffix(&close) else {
        return owned;
    };
    let mut out = head.to_string();
    out.push(',');
    out.push('\n');
    write_preserved_pairs(&mut out, &preserved, &key_indent);
    out.push_str(indent);
    out.push('}');
    out
}

fn preserved_exports(existing: &Value, entries: &[ExportEntry]) -> Vec<(String, Value)> {
    let Some(object) = existing.as_object() else {
        return Vec::new();
    };
    let owned: HashSet<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    let mut kept = Vec::new();
    for (key, value) in object {
        if owned.contains(key.as_str()) {
            continue;
        }
        if is_stale_module_export(key, value) {
            continue;
        }
        kept.push((key.clone(), value.clone()));
    }
    kept
}

/// `./Old` written as exactly `{ "ply", "import" }` for a module this build
/// does not emit. `.` is never stale: without `ply.entry` a hand-written root
/// stays, and with `ply.entry` the key is owned and replaced above.
fn is_stale_module_export(key: &str, value: &Value) -> bool {
    let Some(rest) = key.strip_prefix("./") else {
        return false;
    };
    if module_name_from_path(&format!("{rest}.ply")).is_err() {
        return false;
    }
    let Some(object) = value.as_object() else {
        return false;
    };
    object.len() == 2
        && object.get("ply").and_then(Value::as_str).is_some()
        && object.get("import").and_then(Value::as_str).is_some()
}

fn write_preserved_pairs(out: &mut String, preserved: &[(String, Value)], key_indent: &str) {
    for (index, (key, value)) in preserved.iter().enumerate() {
        out.push_str(key_indent);
        out.push_str(&json_string(key));
        out.push_str(": ");
        out.push_str(&serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()));
        if index + 1 != preserved.len() {
            out.push(',');
        }
        out.push('\n');
    }
}

fn render_preserved_only(preserved: &[(String, Value)], indent: &str) -> String {
    let key_indent = format!("{indent}{indent}");
    let mut out = String::from("{\n");
    write_preserved_pairs(&mut out, preserved, &key_indent);
    out.push_str(indent);
    out.push('}');
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
        out.push_str(",\n");
        out.push_str(&field_indent);
        out.push_str("\"import\": ");
        out.push_str(&json_string(&entry.js));
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

fn line_indent(text: &str, index: usize) -> String {
    let line_start = text[..index].rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    text[line_start..]
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect()
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
        assert!(matches!(super::find_exports("[]"), Err(super::JsonError::NotObject)));
        assert!(matches!(
            super::find_exports(r#"{ "name": "\ud800" }"#),
            Err(super::JsonError::Invalid)
        ));
        assert!(matches!(super::find_exports("{"), Err(super::JsonError::Invalid)));
    }

}
