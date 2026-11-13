//! Arity of one name exported from a foreign JavaScript file.

pub(super) enum JsExport {
    Missing,
    Unknown,
    Arity(usize),
}

pub(super) fn exported_arity(source: &str, name: &str) -> JsExport {
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut bare = false;
    let mut arity = None;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index] == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        if matches!(bytes[index], b'\'' | b'"' | b'`') {
            index = skip_js_string(bytes, index);
            continue;
        }
        if is_word_at(bytes, index, b"export") {
            let mut scan = Scan {
                bytes,
                index: index + "export".len(),
            };
            match scan.one_export(name) {
                JsExport::Arity(count) => arity = Some(count),
                JsExport::Unknown => bare = true,
                JsExport::Missing => {}
            }
            index += "export".len();
            continue;
        }
        if is_ident_start(bytes[index]) {
            while index < bytes.len() && is_ident_cont(bytes[index]) {
                index += 1;
            }
            continue;
        }
        index += 1;
    }
    if let Some(count) = arity {
        JsExport::Arity(count)
    } else if bare {
        JsExport::Unknown
    } else {
        JsExport::Missing
    }
}

struct Scan<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl Scan<'_> {
    fn skip(&mut self) {
        loop {
            while self.index < self.bytes.len() && self.bytes[self.index].is_ascii_whitespace() {
                self.index += 1;
            }
            if let Some(next) = comment_end(self.bytes, self.index) {
                self.index = next;
                continue;
            }
            break;
        }
    }

    fn eat(&mut self, token: &[u8]) -> bool {
        self.skip();
        if self.bytes[self.index..].starts_with(token) {
            self.index += token.len();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &[u8]) -> bool {
        self.skip();
        if is_word_at(self.bytes, self.index, word) {
            self.index += word.len();
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Option<String> {
        self.skip();
        if self.index >= self.bytes.len() || !is_ident_start(self.bytes[self.index]) {
            return None;
        }
        let start = self.index;
        self.index += 1;
        while self.index < self.bytes.len() && is_ident_cont(self.bytes[self.index]) {
            self.index += 1;
        }
        Some(String::from_utf8_lossy(&self.bytes[start..self.index]).into_owned())
    }

    fn looking_at_ident(&mut self) -> bool {
        self.skip();
        self.index < self.bytes.len() && is_ident_start(self.bytes[self.index])
    }

    fn parens(&mut self) -> Option<String> {
        self.skip();
        if self.index >= self.bytes.len() || self.bytes[self.index] != b'(' {
            return None;
        }
        self.index += 1;
        let start = self.index;
        let mut depth = 1i32;
        while self.index < self.bytes.len() && depth > 0 {
            match self.bytes[self.index] {
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    self.index += 1;
                }
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let inner =
                            String::from_utf8_lossy(&self.bytes[start..self.index]).into_owned();
                        self.index += 1;
                        return Some(inner);
                    }
                    self.index += 1;
                }
                b'\'' | b'"' => self.index = skip_js_string(self.bytes, self.index),
                b'`' => self.index = skip_template(self.bytes, self.index),
                b'/' => {
                    if let Some(next) = comment_end(self.bytes, self.index) {
                        self.index = next;
                    } else {
                        self.index += 1;
                    }
                }
                _ => self.index += 1,
            }
        }
        None
    }

    fn one_export(&mut self, name: &str) -> JsExport {
        let _ = self.eat_word(b"async");
        if self.eat_word(b"function") {
            let Some(found) = self.ident() else {
                return JsExport::Missing;
            };
            if found != name {
                return JsExport::Missing;
            }
            let Some(params) = self.parens() else {
                return JsExport::Missing;
            };
            return JsExport::Arity(param_count(&params));
        }
        if self.eat_word(b"const") || self.eat_word(b"let") || self.eat_word(b"var") {
            let Some(found) = self.ident() else {
                return JsExport::Missing;
            };
            if found != name {
                return JsExport::Missing;
            }
            if !self.eat(b"=") {
                return JsExport::Missing;
            }
            if self.eat_word(b"function") {
                if self.looking_at_ident() {
                    let _ = self.ident();
                }
                let Some(params) = self.parens() else {
                    return JsExport::Missing;
                };
                return JsExport::Arity(param_count(&params));
            }
            self.skip();
            if self.index < self.bytes.len() && self.bytes[self.index] == b'(' {
                let Some(params) = self.parens() else {
                    return JsExport::Missing;
                };
                if self.eat(b"=>") {
                    return JsExport::Arity(param_count(&params));
                }
            }
            return JsExport::Missing;
        }
        if self.eat(b"{") {
            if export_list_has(self, name) {
                return JsExport::Unknown;
            }
            return JsExport::Missing;
        }
        JsExport::Unknown
    }
}

fn export_list_has(scan: &mut Scan<'_>, name: &str) -> bool {
    loop {
        scan.skip();
        if scan.eat(b"}") {
            return false;
        }
        let Some(first) = scan.ident() else {
            return false;
        };
        let mut exported = first;
        if scan.eat_word(b"as") {
            if let Some(alias) = scan.ident() {
                exported = alias;
            }
        }
        if exported == name {
            return true;
        }
        scan.skip();
        if scan.eat(b",") {
            continue;
        }
        let _ = scan.eat(b"}");
        return false;
    }
}

fn comment_end(bytes: &[u8], index: usize) -> Option<usize> {
    if index + 1 >= bytes.len() || bytes[index] != b'/' {
        return None;
    }
    if bytes[index + 1] == b'/' {
        let mut index = index + 2;
        while index < bytes.len() && bytes[index] != b'\n' {
            index += 1;
        }
        return Some(index);
    }
    if bytes[index + 1] == b'*' {
        let mut index = index + 2;
        while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
            index += 1;
        }
        return Some((index + 2).min(bytes.len()));
    }
    None
}

fn param_count(params: &str) -> usize {
    let bytes = params.as_bytes();
    let mut index = 0;
    let mut depth = 0i32;
    let mut count = 0;
    let mut slot = false;
    while index < bytes.len() {
        if let Some(next) = comment_end(bytes, index) {
            index = next;
            continue;
        }
        match bytes[index] {
            b'\'' | b'"' => {
                index = skip_js_string(bytes, index);
                slot = true;
            }
            b'`' => {
                index = skip_template(bytes, index);
                slot = true;
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                slot = true;
                index += 1;
            }
            b')' | b']' | b'}' if depth > 0 => {
                depth -= 1;
                index += 1;
            }
            b',' if depth == 0 => {
                if slot {
                    count += 1;
                }
                slot = false;
                index += 1;
            }
            byte if byte.is_ascii_whitespace() => index += 1,
            _ => {
                slot = true;
                index += 1;
            }
        }
    }
    if slot {
        count += 1;
    }
    count
}

/// A template literal, including `${ ... }` so a comma there is not a parameter.
fn skip_template(bytes: &[u8], index: usize) -> usize {
    if index >= bytes.len() || bytes[index] != b'`' {
        return index;
    }
    let mut index = index + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        if bytes[index] == b'`' {
            return index + 1;
        }
        if bytes[index] == b'$' && index + 1 < bytes.len() && bytes[index + 1] == b'{' {
            index = skip_template_expr(bytes, index + 2);
            continue;
        }
        index += 1;
    }
    index
}

fn skip_template_expr(bytes: &[u8], index: usize) -> usize {
    let mut depth = 1i32;
    let mut index = index;
    while index < bytes.len() && depth > 0 {
        if let Some(next) = comment_end(bytes, index) {
            index = next;
            continue;
        }
        match bytes[index] {
            b'\'' | b'"' => index = skip_js_string(bytes, index),
            b'`' => index = skip_template(bytes, index),
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth -= 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    index
}

fn is_word_at(bytes: &[u8], index: usize, word: &[u8]) -> bool {
    if index > 0 && is_ident_cont(bytes[index - 1]) {
        return false;
    }
    if !bytes[index..].starts_with(word) {
        return false;
    }
    let end = index + word.len();
    end >= bytes.len() || !is_ident_cont(bytes[end])
}

fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$'
}

fn is_ident_cont(byte: u8) -> bool {
    is_ident_start(byte) || byte.is_ascii_digit()
}

fn skip_js_string(bytes: &[u8], index: usize) -> usize {
    if index >= bytes.len() {
        return index;
    }
    let quote = bytes[index];
    let mut index = index + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
            continue;
        }
        if bytes[index] == quote {
            return index + 1;
        }
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::{exported_arity, JsExport};

    #[test]
    fn counts_foreign_js_parameters() {
        assert!(matches!(
            exported_arity("export function add(a, b) { return a + b; }", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export const add = (a, b) => a + b;", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export const add = function (a) { return a; }", "add"),
            JsExport::Arity(1)
        ));
        assert!(matches!(
            exported_arity(
                "export const add = function add(a, b, c) { return a; }",
                "add"
            ),
            JsExport::Arity(3)
        ));
        assert!(matches!(
            exported_arity("export { add };", "add"),
            JsExport::Unknown
        ));
        assert!(matches!(
            exported_arity("export function sub(a) {}", "add"),
            JsExport::Missing
        ));
        assert!(matches!(
            exported_arity("export { sub }", "add"),
            JsExport::Missing
        ));
        assert!(matches!(
            exported_arity("export class add {}", "add"),
            JsExport::Unknown
        ));
        assert!(matches!(
            exported_arity("export async function add(a, b) { return a; }", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export async\nfunction add(a, b) { return a; }", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export /* c */ function add(a) { return a; }", "add"),
            JsExport::Arity(1)
        ));
        assert!(matches!(
            exported_arity("export // c\nfunction add(a) { return a; }", "add"),
            JsExport::Arity(1)
        ));
        assert!(matches!(
            exported_arity("export function add(a = \",\", b) { return b; }", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export function add(a /* ) */ , b) { return b; }", "add"),
            JsExport::Arity(2)
        ));
        assert!(matches!(
            exported_arity("export function add(a /* , */ b) { return a; }", "add"),
            JsExport::Arity(1)
        ));
        assert!(matches!(
            exported_arity("export function add(a = `${x, y}`, b) { return b; }", "add"),
            JsExport::Arity(2)
        ));
    }
}
