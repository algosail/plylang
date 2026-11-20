use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::{
    CompletionOptions, HoverProviderCapability, OneOf, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind,
};
use plylang::{
    buffer_notes_editor, complete, definition, hover_with_sites, utf16_position, CompletionKind,
    OpenFault, OpenText, Related, StackAt,
};
use serde::Deserialize;

fn main() {
    if let Err(error) = serve_stdio() {
        eprintln!("plylang-ls: {error}");
        std::process::exit(1);
    }
}

fn serve_stdio() -> Result<(), String> {
    let (connection, io_threads) = Connection::stdio();
    serve(connection)?;
    io_threads.join().map_err(|error| error.to_string())
}

fn serve(connection: Connection) -> Result<(), String> {
    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        definition_provider: Some(OneOf::Left(true)),
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![
                "\"".to_string(),
                "/".to_string(),
                "(".to_string(),
                ",".to_string(),
                "=".to_string(),
                ":".to_string(),
                "#".to_string(),
                "@".to_string(),
            ]),
            ..CompletionOptions::default()
        }),
        ..ServerCapabilities::default()
    };
    let capabilities = serde_json::to_value(capabilities).map_err(|error| error.to_string())?;
    connection
        .initialize(capabilities)
        .map_err(|error| error.to_string())?;
    let mut docs = HashMap::new();
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection
                    .handle_shutdown(&request)
                    .map_err(|error| error.to_string())?
                {
                    return Ok(());
                }
                if request.method == "textDocument/hover" {
                    let response = hover_response(&docs, &request);
                    connection
                        .sender
                        .send(response.into())
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                if request.method == "textDocument/definition" {
                    let response = definition_response(&docs, &request);
                    connection
                        .sender
                        .send(response.into())
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                if request.method == "textDocument/completion" {
                    let response = completion_response(&docs, &request);
                    connection
                        .sender
                        .send(response.into())
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                let response = Response::new_err(
                    request.id,
                    ErrorCode::MethodNotFound as i32,
                    "method not found".to_string(),
                );
                connection
                    .sender
                    .send(response.into())
                    .map_err(|error| error.to_string())?;
            }
            Message::Notification(notification) => {
                if let Some(uri) = closed_uri(&notification) {
                    let path = canonical_path(&uri);
                    docs.remove(&uri);
                    let stale = importers_of(&docs, &path, &uri);
                    for other in stale {
                        refresh(&connection, &mut docs, &other)?;
                    }
                    continue;
                }
                if let Some((uri, text)) = document_text(&notification) {
                    let before = docs.get(&uri).and_then(|opened| opened.surface.clone());
                    match docs.get_mut(&uri) {
                        Some(opened) => opened.text = text,
                        None => {
                            docs.insert(uri.clone(), Opened::new(text));
                        }
                    }
                    refresh(&connection, &mut docs, &uri)?;
                    let after = docs.get(&uri).and_then(|opened| opened.surface.clone());
                    if before != after {
                        let path = canonical_path(&uri);
                        let stale = importers_of(&docs, &path, &uri);
                        for other in stale {
                            refresh(&connection, &mut docs, &other)?;
                        }
                    }
                }
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn document_text(notification: &Notification) -> Option<(String, String)> {
    match notification.method.as_str() {
        "textDocument/didOpen" => {
            let params: DidOpen = serde_json::from_value(notification.params.clone()).ok()?;
            Some((params.text_document.uri, params.text_document.text))
        }
        "textDocument/didChange" => {
            let params: DidChange = serde_json::from_value(notification.params.clone()).ok()?;
            let text = params.content_changes.last()?.text.clone();
            Some((params.text_document.uri, text))
        }
        _ => None,
    }
}

fn hover_response(docs: &HashMap<String, Opened>, request: &Request) -> Response {
    let params: HoverRequest = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(_) => return Response::new_ok(request.id.clone(), serde_json::Value::Null),
    };
    let Some(opened) = docs.get(&params.text_document.uri) else {
        return Response::new_ok(request.id.clone(), serde_json::Value::Null);
    };
    let path = uri_path(&params.text_document.uri);
    let Some(found) = hover_with_sites(
        &opened.text,
        std::path::Path::new(&path),
        params.position.line,
        params.position.character,
        &opened.sites,
    ) else {
        return Response::new_ok(request.id.clone(), serde_json::Value::Null);
    };
    let (start_line, start_character) = utf16_position(&opened.text, found.span.start);
    let (end_line, end_character) = utf16_position(&opened.text, found.span.end);
    Response::new_ok(
        request.id.clone(),
        serde_json::json!({
            "contents": { "kind": "markdown", "value": found.contents },
            "range": {
                "start": { "line": start_line, "character": start_character },
                "end": { "line": end_line, "character": end_character },
            }
        }),
    )
}

fn definition_response(docs: &HashMap<String, Opened>, request: &Request) -> Response {
    let params: HoverRequest = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(_) => return Response::new_ok(request.id.clone(), serde_json::Value::Null),
    };
    let Some(opened) = docs.get(&params.text_document.uri) else {
        return Response::new_ok(request.id.clone(), serde_json::Value::Null);
    };
    let text = &opened.text;
    let path = uri_path(&params.text_document.uri);
    let Some(found) = definition(
        text,
        std::path::Path::new(&path),
        params.position.line,
        params.position.character,
    ) else {
        return Response::new_ok(request.id.clone(), serde_json::Value::Null);
    };
    let (uri, target) = if found.path == std::path::Path::new(&path) {
        (params.text_document.uri.clone(), text.clone())
    } else {
        let Ok(disk) = std::fs::read_to_string(&found.path) else {
            return Response::new_ok(request.id.clone(), serde_json::Value::Null);
        };
        let Ok(url) = url::Url::from_file_path(&found.path) else {
            return Response::new_ok(request.id.clone(), serde_json::Value::Null);
        };
        (url.to_string(), disk)
    };
    let (start_line, start_character) = utf16_position(&target, found.span.start);
    let (end_line, end_character) = utf16_position(&target, found.span.end);
    Response::new_ok(
        request.id.clone(),
        serde_json::json!({
            "uri": uri,
            "range": {
                "start": { "line": start_line, "character": start_character },
                "end": { "line": end_line, "character": end_character },
            }
        }),
    )
}

fn completion_response(docs: &HashMap<String, Opened>, request: &Request) -> Response {
    let params: HoverRequest = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(_) => {
            return Response::new_ok(
                request.id.clone(),
                serde_json::json!({ "isIncomplete": false, "items": [] }),
            );
        }
    };
    let Some(opened) = docs.get(&params.text_document.uri) else {
        return Response::new_ok(
            request.id.clone(),
            serde_json::json!({ "isIncomplete": false, "items": [] }),
        );
    };
    let text = &opened.text;
    let path = uri_path(&params.text_document.uri);
    let items = complete(
        text,
        std::path::Path::new(&path),
        params.position.line,
        params.position.character,
    );
    let items = items
        .into_iter()
        .map(|item| completion_item(text, &item))
        .collect::<Vec<_>>();
    Response::new_ok(
        request.id.clone(),
        serde_json::json!({ "isIncomplete": false, "items": items }),
    )
}

fn completion_item(text: &str, item: &plylang::Completion) -> serde_json::Value {
    let (start_line, start_character) = utf16_position(text, item.replace.start);
    let (end_line, end_character) = utf16_position(text, item.replace.end);
    let mut value = serde_json::json!({
        "label": item.label,
        "kind": kind_code(item.kind),
        "filterText": item.label,
        "sortText": item.sort,
        "textEdit": {
            "range": {
                "start": { "line": start_line, "character": start_character },
                "end": { "line": end_line, "character": end_character },
            },
            "newText": item.insert,
        }
    });
    if !item.detail.is_empty() {
        value["detail"] = serde_json::json!(item.detail);
    }
    if !item.documentation.is_empty() {
        value["documentation"] = serde_json::json!({
            "kind": "markdown",
            "value": item.documentation,
        });
    }
    value
}

fn kind_code(kind: CompletionKind) -> u32 {
    match kind {
        CompletionKind::Keyword => 14,
        CompletionKind::Word => 3,
        CompletionKind::Constructor => 4,
        CompletionKind::Type => 22,
        CompletionKind::TypeVar => 25,
        CompletionKind::Module => 9,
        CompletionKind::Field => 5,
        CompletionKind::Class => 7,
    }
}

fn closed_uri(notification: &Notification) -> Option<String> {
    if notification.method != "textDocument/didClose" {
        return None;
    }
    let params: DidClose = serde_json::from_value(notification.params.clone()).ok()?;
    Some(params.text_document.uri)
}

fn publish(
    uri: &str,
    text: &str,
    notes: &plylang::BufferNotes,
    docs: &HashMap<String, Opened>,
) -> Notification {
    let mut diagnostics = Vec::new();
    for error in &notes.errors {
        let (start_line, start_character) = utf16_position(text, error.span.start);
        let (end_line, end_character) = utf16_position(text, error.span.end);
        let mut value = serde_json::json!({
            "range": {
                "start": { "line": start_line, "character": start_character },
                "end": { "line": end_line, "character": end_character },
            },
            "severity": 1,
            "source": "plylang",
            "code": error.code,
            "codeDescription": { "href": error.url() },
            "message": error.reported(),
        });
        attach_related(&mut value, docs, error.related.as_ref());
        diagnostics.push(value);
    }
    for hint in &notes.hints {
        let (start_line, start_character) = utf16_position(text, hint.span.start);
        let (end_line, end_character) = utf16_position(text, hint.span.end);
        let mut value = serde_json::json!({
            "range": {
                "start": { "line": start_line, "character": start_character },
                "end": { "line": end_line, "character": end_character },
            },
            "severity": 4,
            "source": "plylang",
            "message": hint.message,
        });
        if hint.unnecessary {
            value["tags"] = serde_json::json!([1]);
        }
        attach_related(&mut value, docs, hint.related.as_ref());
        diagnostics.push(value);
    }
    Notification {
        method: "textDocument/publishDiagnostics".to_string(),
        params: serde_json::json!({
            "uri": uri,
            "diagnostics": diagnostics,
        }),
    }
}

fn attach_related(
    value: &mut serde_json::Value,
    docs: &HashMap<String, Opened>,
    related: Option<&Related>,
) {
    let Some(related) = related else {
        return;
    };
    value["relatedInformation"] = serde_json::json!([related_information(docs, related)]);
}

fn related_information(docs: &HashMap<String, Opened>, related: &Related) -> serde_json::Value {
    let path = PathBuf::from(&related.path);
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    let text = docs
        .iter()
        .find_map(|(uri, opened)| (canonical_path(uri) == canonical).then(|| opened.text.clone()))
        .or_else(|| std::fs::read_to_string(&canonical).ok())
        .unwrap_or_default();
    let (start_line, start_character) = utf16_position(&text, related.span.start);
    let (end_line, end_character) = utf16_position(&text, related.span.end);
    let uri = url::Url::from_file_path(&canonical)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| related.path.clone());
    serde_json::json!({
        "location": {
            "uri": uri,
            "range": {
                "start": { "line": start_line, "character": start_character },
                "end": { "line": end_line, "character": end_character },
            }
        },
        "message": related.message,
    })
}

/// Publish this buffer. Callers recheck an importer when this file's surface
/// changes, including the step from a surface to none and back.
fn refresh(
    connection: &Connection,
    docs: &mut HashMap<String, Opened>,
    uri: &str,
) -> Result<(), String> {
    let notes = notes_of(docs, uri);
    let notification = {
        let text = &docs[uri].text;
        publish(uri, text, &notes, docs)
    };
    if let Some(opened) = docs.get_mut(uri) {
        opened.sites = notes.sites;
        opened.deps = notes.deps;
        if let Some(surface) = notes.surface {
            opened.checked = Some(opened.text.clone());
            opened.surface = Some(surface);
            opened.fault = None;
        } else {
            opened.surface = None;
            opened.fault = notes.error.map(|error| OpenFault {
                span: error.span,
                message: error.message,
            });
        }
    }
    connection
        .sender
        .send(notification.into())
        .map_err(|error| error.to_string())
}

fn notes_of(docs: &HashMap<String, Opened>, uri: &str) -> plylang::BufferNotes {
    let path = uri_path(uri);
    let mut held = Vec::new();
    for (other, opened) in docs {
        let broken = opened.surface.is_none();
        held.push((
            uri_path(other),
            opened.text.as_str(),
            if broken {
                opened.checked.as_deref()
            } else {
                None
            },
            if broken { opened.fault.as_ref() } else { None },
        ));
    }
    let open: Vec<OpenText<'_>> = held
        .iter()
        .map(|(file, text, checked, fault)| OpenText {
            path: Path::new(file.as_str()),
            text,
            checked: *checked,
            fault: *fault,
        })
        .collect();
    buffer_notes_editor(Path::new(&path), &docs[uri].text, &open)
}

fn importers_of(docs: &HashMap<String, Opened>, path: &Path, skip: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (uri, opened) in docs {
        if uri == skip {
            continue;
        }
        if opened.deps.iter().any(|dep| dep == path) {
            found.push(uri.clone());
        }
    }
    found
}

fn canonical_path(uri: &str) -> PathBuf {
    let path = PathBuf::from(uri_path(uri));
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn uri_path(uri: &str) -> String {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let mut bytes = Vec::new();
    let chars = raw.as_bytes();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == b'%' && index + 2 < chars.len() {
            let hex = std::str::from_utf8(&chars[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                bytes.push(byte);
                index += 3;
                continue;
            }
        }
        bytes.push(chars[index]);
        index += 1;
    }
    String::from_utf8(bytes).unwrap_or_else(|_| raw.to_string())
}

struct Opened {
    text: String,
    sites: Vec<StackAt>,
    /// Canonical paths loaded by the last check of this buffer.
    deps: Vec<PathBuf>,
    /// Last text whose check produced a surface.
    checked: Option<String>,
    /// Fingerprint of that surface. `None` while the file does not check.
    surface: Option<String>,
    /// The error in `text` while `surface` is `None`.
    fault: Option<OpenFault>,
}

impl Opened {
    fn new(text: String) -> Self {
        Self {
            text,
            sites: Vec::new(),
            deps: Vec::new(),
            checked: None,
            surface: None,
            fault: None,
        }
    }
}

#[derive(Deserialize)]
struct DidClose {
    #[serde(rename = "textDocument")]
    text_document: HoverDocument,
}

#[derive(Deserialize)]
struct DidOpen {
    #[serde(rename = "textDocument")]
    text_document: OpenDocument,
}

#[derive(Deserialize)]
struct OpenDocument {
    uri: String,
    text: String,
}

#[derive(Deserialize)]
struct DidChange {
    #[serde(rename = "textDocument")]
    text_document: ChangeDocument,
    #[serde(rename = "contentChanges")]
    content_changes: Vec<Change>,
}

#[derive(Deserialize)]
struct ChangeDocument {
    uri: String,
}

#[derive(Deserialize)]
struct Change {
    text: String,
}

#[derive(Deserialize)]
struct HoverRequest {
    #[serde(rename = "textDocument")]
    text_document: HoverDocument,
    position: Position,
}

#[derive(Deserialize)]
struct HoverDocument {
    uri: String,
}

#[derive(Deserialize)]
struct Position {
    line: u32,
    character: u32,
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use lsp_server::{Connection, Message, Notification, Request, RequestId};
    use serde_json::json;

    use super::serve;

    #[test]
    fn publishes_the_parse_diagnostic() {
        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({
                    "processId": null,
                    "capabilities": {},
                }),
            }))
            .unwrap();
        let initialize = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        assert!(matches!(initialize, Message::Response(_)));
        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        let source = ": f ( Int -- Int )\n";
        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: json!({
                    "textDocument": {
                        "uri": "file:///tmp/M.ply",
                        "languageId": "ply",
                        "version": 1,
                        "text": source,
                    }
                }),
            }))
            .unwrap();
        let published = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("diagnostics");
        let Message::Notification(notification) = published else {
            panic!("expected diagnostics, got {published:?}");
        };
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        let message = notification.params["diagnostics"][0]["message"]
            .as_str()
            .unwrap();
        assert_eq!(
            message,
            "PL0030: missing body\nhttps://github.com/algosail/plylang/blob/main/docs/Errors.md#pl0030"
        );

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn publishes_the_type_diagnostic() {
        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({
                    "processId": null,
                    "capabilities": {},
                }),
            }))
            .unwrap();
        let initialize = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        assert!(matches!(initialize, Message::Response(_)));
        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        let source = ": f ( Str -- Int )\n  dup drop\n";
        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: json!({
                    "textDocument": {
                        "uri": "file:///tmp/M.ply",
                        "languageId": "ply",
                        "version": 1,
                        "text": source,
                    }
                }),
            }))
            .unwrap();
        let published = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("diagnostics");
        let Message::Notification(notification) = published else {
            panic!("expected diagnostics, got {published:?}");
        };
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        let message = notification.params["diagnostics"][0]["message"]
            .as_str()
            .unwrap();
        assert_eq!(
            message,
            "PL0061: type mismatch: expected Int, found Str\nhttps://github.com/algosail/plylang/blob/main/docs/Errors.md#pl0061"
        );

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn hovers_the_stack_scheme() {
        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({
                    "processId": null,
                    "capabilities": {},
                }),
            }))
            .unwrap();
        let initialize = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        assert!(matches!(initialize, Message::Response(_)));
        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        let source = ": double ( Int -- Int )\n  dup drop\n";
        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: json!({
                    "textDocument": {
                        "uri": "file:///tmp/M.ply",
                        "languageId": "ply",
                        "version": 1,
                        "text": source,
                    }
                }),
            }))
            .unwrap();
        let published = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("diagnostics");
        let Message::Notification(notification) = published else {
            panic!("expected diagnostics, got {published:?}");
        };
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        assert!(notification.params["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty());

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "textDocument/hover".to_string(),
                params: json!({
                    "textDocument": { "uri": "file:///tmp/M.ply" },
                    "position": { "line": 1, "character": 2 },
                }),
            }))
            .unwrap();
        let hovered = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("hover");
        let Message::Response(response) = hovered else {
            panic!("expected hover, got {hovered:?}");
        };
        let contents = response.result.unwrap()["contents"].clone();
        assert_eq!(contents["kind"].as_str().unwrap(), "markdown");
        assert_eq!(
            contents["value"].as_str().unwrap(),
            "```ply\n: dup ( ..s a -- ..s a a )\n```\n\nCopies the top value. On `x y` the result is `x y y`.\n\n```ply\nInt\nInt Int\n```"
        );

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(3),
                method: "textDocument/hover".to_string(),
                params: json!({
                    "textDocument": { "uri": "file:///tmp/M.ply" },
                    "position": { "line": 0, "character": 4 },
                }),
            }))
            .unwrap();
        let hovered = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("hover def");
        let Message::Response(response) = hovered else {
            panic!("expected hover, got {hovered:?}");
        };
        assert_eq!(
            response.result.unwrap()["contents"]["value"]
                .as_str()
                .unwrap(),
            "```ply\n: double ( Int -- Int )\n```"
        );

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(4),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn completes_a_word() {
        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({
                    "processId": null,
                    "capabilities": {},
                }),
            }))
            .unwrap();
        let initialize = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        assert!(matches!(initialize, Message::Response(_)));
        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        let source = ": double ( Int -- Int )\n  du\n";
        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: json!({
                    "textDocument": {
                        "uri": "file:///tmp/M.ply",
                        "languageId": "ply",
                        "version": 1,
                        "text": source,
                    }
                }),
            }))
            .unwrap();
        let published = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("diagnostics");
        let Message::Notification(notification) = published else {
            panic!("expected diagnostics, got {published:?}");
        };
        assert_eq!(notification.method, "textDocument/publishDiagnostics");

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "textDocument/completion".to_string(),
                params: json!({
                    "textDocument": { "uri": "file:///tmp/M.ply" },
                    "position": { "line": 1, "character": 4 },
                }),
            }))
            .unwrap();
        let completed = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("completion");
        let Message::Response(response) = completed else {
            panic!("expected completion, got {completed:?}");
        };
        let items = response.result.unwrap()["items"]
            .as_array()
            .unwrap()
            .clone();
        let dup = items.iter().find(|item| item["label"] == "dup").unwrap();
        assert_eq!(dup["kind"], 3);
        assert_eq!(dup["detail"], "..s a -- ..s a a");
        assert_eq!(dup["textEdit"]["newText"], "dup");
        assert!(items.iter().any(|item| item["label"] == "double"));

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(3),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn completes_a_record_field() {
        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({
                    "processId": null,
                    "capabilities": {},
                }),
            }))
            .unwrap();
        let initialize = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        let Message::Response(response) = initialize else {
            panic!("expected initialize, got {initialize:?}");
        };
        let triggers = response.result.unwrap()["capabilities"]["completionProvider"]
            ["triggerCharacters"]
            .as_array()
            .unwrap()
            .clone();
        assert!(triggers.iter().any(|item| item == "="));
        assert!(triggers.iter().any(|item| item == ":"));
        assert!(triggers.iter().any(|item| item == "#"));
        assert!(triggers.iter().any(|item| item == "@"));

        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        let source =
            "; Person = Person name : Str, age : Int\n\n: get ( Person -- Person )\n  1 >>\n";
        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: json!({
                    "textDocument": {
                        "uri": "file:///tmp/M.ply",
                        "languageId": "ply",
                        "version": 1,
                        "text": source,
                    }
                }),
            }))
            .unwrap();
        let published = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("diagnostics");
        let Message::Notification(notification) = published else {
            panic!("expected diagnostics, got {published:?}");
        };
        assert_eq!(notification.method, "textDocument/publishDiagnostics");

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "textDocument/completion".to_string(),
                params: json!({
                    "textDocument": { "uri": "file:///tmp/M.ply" },
                    "position": { "line": 3, "character": 6 },
                }),
            }))
            .unwrap();
        let completed = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("completion");
        let Message::Response(response) = completed else {
            panic!("expected completion, got {completed:?}");
        };
        let items = response.result.unwrap()["items"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            items
                .iter()
                .map(|item| item["label"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["age", "name"]
        );
        let name = items.iter().find(|item| item["label"] == "name").unwrap();
        assert_eq!(name["kind"], 5);
        assert_eq!(name["detail"], "Str");
        assert_eq!(name["textEdit"]["newText"], "name");
        assert_eq!(name["textEdit"]["range"]["start"]["character"], 6);
        assert_eq!(name["textEdit"]["range"]["end"]["character"], 6);

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(3),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn jumps_to_the_definition() {
        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({
                    "processId": null,
                    "capabilities": {},
                }),
            }))
            .unwrap();
        let initialize = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        let Message::Response(response) = initialize else {
            panic!("expected initialize, got {initialize:?}");
        };
        assert_eq!(
            response.result.unwrap()["capabilities"]["definitionProvider"],
            json!(true)
        );
        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        let source = ": double ( Int -- Int )\n  dup double\n";
        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: json!({
                    "textDocument": {
                        "uri": "file:///tmp/M.ply",
                        "languageId": "ply",
                        "version": 1,
                        "text": source,
                    }
                }),
            }))
            .unwrap();
        let published = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("diagnostics");
        let Message::Notification(notification) = published else {
            panic!("expected diagnostics, got {published:?}");
        };
        assert_eq!(notification.method, "textDocument/publishDiagnostics");

        let use_at = source.rfind("double").unwrap();
        let (line, character) = position(source, use_at);
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "textDocument/definition".to_string(),
                params: json!({
                    "textDocument": { "uri": "file:///tmp/M.ply" },
                    "position": { "line": line, "character": character },
                }),
            }))
            .unwrap();
        let defined = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("definition");
        let Message::Response(response) = defined else {
            panic!("expected definition, got {defined:?}");
        };
        let result = response.result.unwrap();
        assert_eq!(result["uri"], "file:///tmp/M.ply");
        let decl = source.find("double").unwrap();
        let (start_line, start_character) = position(source, decl);
        let (end_line, end_character) = position(source, decl + "double".len());
        assert_eq!(result["range"]["start"]["line"], start_line);
        assert_eq!(result["range"]["start"]["character"], start_character);
        assert_eq!(result["range"]["end"]["line"], end_line);
        assert_eq!(result["range"]["end"]["character"], end_character);

        let dup_at = source.find("dup").unwrap();
        let (line, character) = position(source, dup_at);
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(3),
                method: "textDocument/definition".to_string(),
                params: json!({
                    "textDocument": { "uri": "file:///tmp/M.ply" },
                    "position": { "line": line, "character": character },
                }),
            }))
            .unwrap();
        let defined = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("builtin");
        let Message::Response(response) = defined else {
            panic!("expected definition, got {defined:?}");
        };
        assert!(response.result.unwrap().is_null());

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(4),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
    }

    fn position(source: &str, offset: usize) -> (u32, u32) {
        let line = source[..offset]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u32;
        let line_start = source[..offset]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let character = source[line_start..offset].encode_utf16().count() as u32;
        (line, character)
    }

    #[test]
    fn republishes_an_importer_when_an_export_disappears() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("plylang-ls-export-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join("src/Data")).unwrap();
        std::fs::write(
            root.join("package.json"),
            "{\n  \"ply\": { \"language\": 1, \"source\": \"src\", \"out\": \"dist\" }\n}\n",
        )
        .unwrap();
        let lib = "@ kept\n\n: kept ( Int -- Int )\n  dup drop\n";
        let hidden = "@\n\n: kept ( Int -- Int )\n  dup drop\n";
        let app = "@ go\n~ \"Data/Lib\" kept\n\n: go ( Int -- Int )\n  kept\n";
        let other = "@ id\n\n: id ( Int -- Int )\n  dup drop\n";
        std::fs::write(root.join("src/Data/Lib.ply"), lib).unwrap();
        std::fs::write(root.join("src/App.ply"), app).unwrap();
        std::fs::write(root.join("src/Other.ply"), other).unwrap();
        let lib_uri = url::Url::from_file_path(root.join("src/Data/Lib.ply"))
            .unwrap()
            .to_string();
        let app_uri = url::Url::from_file_path(root.join("src/App.ply"))
            .unwrap()
            .to_string();
        let other_uri = url::Url::from_file_path(root.join("src/Other.ply"))
            .unwrap()
            .to_string();

        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({ "processId": null, "capabilities": {} }),
            }))
            .unwrap();
        client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();

        for (uri, text) in [(&lib_uri, lib), (&app_uri, app), (&other_uri, other)] {
            client
                .sender
                .send(Message::Notification(Notification {
                    method: "textDocument/didOpen".to_string(),
                    params: json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": "ply",
                            "version": 1,
                            "text": text,
                        }
                    }),
                }))
                .unwrap();
            let opened = drain_diagnostics(&client);
            assert_eq!(opened.len(), 1, "{opened:?}");
            assert_eq!(opened[0]["uri"], *uri);
            assert!(
                opened[0]["diagnostics"].as_array().unwrap().is_empty(),
                "{opened:?}"
            );
        }

        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didChange".to_string(),
                params: json!({
                    "textDocument": { "uri": lib_uri },
                    "contentChanges": [{ "text": hidden }],
                }),
            }))
            .unwrap();
        let published = drain_diagnostics(&client);
        assert_eq!(published.len(), 2, "{published:?}");
        assert_eq!(published[0]["uri"], lib_uri);
        assert!(
            published[0]["diagnostics"].as_array().unwrap().is_empty(),
            "{published:?}"
        );
        assert_eq!(published[1]["uri"], app_uri);
        let message = published[1]["diagnostics"][0]["message"].as_str().unwrap();
        assert!(message.starts_with("PL0091:"), "{message}");
        assert!(message.contains("does not export kept"), "{message}");
        assert!(!published.iter().any(|item| item["uri"] == other_uri));

        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didChange".to_string(),
                params: json!({
                    "textDocument": { "uri": lib_uri },
                    "contentChanges": [{ "text": lib }],
                }),
            }))
            .unwrap();
        let cleared = drain_diagnostics(&client);
        assert_eq!(cleared.len(), 2, "{cleared:?}");
        assert_eq!(cleared[0]["uri"], lib_uri);
        assert_eq!(cleared[1]["uri"], app_uri);
        assert!(
            cleared[1]["diagnostics"].as_array().unwrap().is_empty(),
            "{cleared:?}"
        );

        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didChange".to_string(),
                params: json!({
                    "textDocument": { "uri": lib_uri },
                    "contentChanges": [{ "text": hidden }],
                }),
            }))
            .unwrap();
        let hidden_again = drain_diagnostics(&client);
        assert_eq!(hidden_again.len(), 2, "{hidden_again:?}");
        assert!(
            hidden_again[1]["diagnostics"][0]["message"]
                .as_str()
                .unwrap()
                .starts_with("PL0091:"),
            "{hidden_again:?}"
        );
        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didClose".to_string(),
                params: json!({ "textDocument": { "uri": lib_uri } }),
            }))
            .unwrap();
        let closed = drain_diagnostics(&client);
        assert_eq!(closed.len(), 1, "{closed:?}");
        assert_eq!(closed[0]["uri"], app_uri);
        assert!(
            closed[0]["diagnostics"].as_array().unwrap().is_empty(),
            "{closed:?}"
        );

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn publishes_every_word_error_and_hovers_the_next_word() {
        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({ "processId": null, "capabilities": {} }),
            }))
            .unwrap();
        client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        let source = ": broken ( Int -- Int )\n  dup\n: later ( Int -- Int )\n  dup drop\n: also ( Int -- Int )\n  swap\n";
        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: json!({
                    "textDocument": {
                        "uri": "file:///tmp/Words.ply",
                        "languageId": "ply",
                        "version": 1,
                        "text": source,
                    }
                }),
            }))
            .unwrap();
        let published = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("diagnostics");
        let Message::Notification(notification) = published else {
            panic!("expected diagnostics, got {published:?}");
        };
        let diagnostics = notification.params["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
        assert!(
            diagnostics[0]["message"]
                .as_str()
                .unwrap()
                .contains("type mismatch"),
            "{diagnostics:?}"
        );
        assert!(
            diagnostics[1]["message"]
                .as_str()
                .unwrap()
                .contains("stack underflow"),
            "{diagnostics:?}"
        );

        let drop_at = source.rfind("drop").unwrap();
        let (line, character) = position(source, drop_at);
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "textDocument/hover".to_string(),
                params: json!({
                    "textDocument": { "uri": "file:///tmp/Words.ply" },
                    "position": { "line": line, "character": character },
                }),
            }))
            .unwrap();
        let hovered = client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("hover");
        let Message::Response(response) = hovered else {
            panic!("expected hover, got {hovered:?}");
        };
        let contents = response.result.unwrap()["contents"]["value"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(contents.contains("Int Int\nInt"), "{contents}");

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(3),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
    }

    #[test]
    fn fades_an_unused_import() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("plylang-ls-unused-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("package.json"),
            "{\n  \"ply\": { \"language\": 1, \"source\": \"src\", \"out\": \"dist\" }\n}\n",
        )
        .unwrap();
        let host = "@ spare, kept\n\n: spare ( -- Num )\n  1\n\n: kept ( -- Num )\n  1\n";
        let app = "@ go\n~ \"Host\" spare, kept\n\n: go ( -- Num )\n  kept\n";
        std::fs::write(root.join("src/Host.ply"), host).unwrap();
        std::fs::write(root.join("src/App.ply"), app).unwrap();
        let app_uri = url::Url::from_file_path(root.join("src/App.ply"))
            .unwrap()
            .to_string();

        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({ "processId": null, "capabilities": {} }),
            }))
            .unwrap();
        client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "textDocument/didOpen".to_string(),
                params: json!({
                    "textDocument": {
                        "uri": app_uri,
                        "languageId": "ply",
                        "version": 1,
                        "text": app,
                    }
                }),
            }))
            .unwrap();
        let published = drain_diagnostics(&client);
        assert_eq!(published.len(), 1, "{published:?}");
        let diagnostics = published[0]["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0]["severity"], 4);
        assert_eq!(diagnostics[0]["tags"], json!([1]));
        assert_eq!(diagnostics[0]["message"], "unused word spare");
        assert_eq!(diagnostics[0]["range"]["start"]["line"], 1);
        let column = app.lines().nth(1).unwrap().find("spare").unwrap() as u64;
        let start = diagnostics[0]["range"]["start"]["character"]
            .as_u64()
            .unwrap();
        let end = diagnostics[0]["range"]["end"]["character"]
            .as_u64()
            .unwrap();
        assert_eq!((start, end), (column, column + 5));

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_body_edit_does_not_republish_importers() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("plylang-ls-surface-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(root.join("src/Data")).unwrap();
        std::fs::write(
            root.join("package.json"),
            "{\n  \"ply\": { \"language\": 1, \"source\": \"src\", \"out\": \"dist\" }\n}\n",
        )
        .unwrap();
        let good = "@ answer\n\n: answer ( -- Num )\n  1\n";
        let body = "@ answer\n\n: answer ( -- Num )\n  2\n";
        let scheme = "@ answer\n\n: answer ( -- Str )\n  \"a\"\n";
        let hidden = "@\n\n: answer ( -- Num )\n  1\n";
        let broken = "@ answer\n\n:::\n";
        let broken_more = "@ answer\n\n::::\n";
        let app = "@ go\n~ \"Data/Lib\" answer\n\n: go ( -- Num )\n  answer\n";
        std::fs::write(root.join("src/Data/Lib.ply"), good).unwrap();
        std::fs::write(root.join("src/App.ply"), app).unwrap();
        let lib_uri = url::Url::from_file_path(root.join("src/Data/Lib.ply"))
            .unwrap()
            .to_string();
        let app_uri = url::Url::from_file_path(root.join("src/App.ply"))
            .unwrap()
            .to_string();

        let (server, client) = Connection::memory();
        let server = thread::spawn(move || serve(server));
        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(1),
                method: "initialize".to_string(),
                params: json!({ "processId": null, "capabilities": {} }),
            }))
            .unwrap();
        client
            .receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("initialize response");
        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        for (uri, text) in [(&lib_uri, good), (&app_uri, app)] {
            client
                .sender
                .send(Message::Notification(Notification {
                    method: "textDocument/didOpen".to_string(),
                    params: json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": "ply",
                            "version": 1,
                            "text": text,
                        }
                    }),
                }))
                .unwrap();
            let opened = drain_diagnostics(&client);
            assert_eq!(opened.len(), 1, "{opened:?}");
            assert_eq!(opened[0]["uri"], *uri);
            assert!(
                opened[0]["diagnostics"].as_array().unwrap().is_empty(),
                "{opened:?}"
            );
        }

        let change = |text: &str| {
            client
                .sender
                .send(Message::Notification(Notification {
                    method: "textDocument/didChange".to_string(),
                    params: json!({
                        "textDocument": { "uri": lib_uri },
                        "contentChanges": [{ "text": text }],
                    }),
                }))
                .unwrap();
        };

        change(body);
        let published = drain_diagnostics(&client);
        assert_eq!(published.len(), 1, "{published:?}");
        assert_eq!(published[0]["uri"], lib_uri);

        change(scheme);
        let published = drain_diagnostics(&client);
        assert_eq!(published.len(), 2, "{published:?}");
        assert_eq!(published[0]["uri"], lib_uri);
        assert_eq!(published[1]["uri"], app_uri);
        let message = published[1]["diagnostics"][0]["message"].as_str().unwrap();
        assert!(message.contains("type mismatch"), "{message}");

        change(hidden);
        let published = drain_diagnostics(&client);
        assert_eq!(published.len(), 2, "{published:?}");
        assert_eq!(published[1]["uri"], app_uri);
        let diagnostic = &published[1]["diagnostics"][0];
        let message = diagnostic["message"].as_str().unwrap();
        assert!(message.starts_with("PL0091:"), "{message}");
        assert!(message.contains("does not export answer"), "{message}");
        let related = &diagnostic["relatedInformation"][0];
        assert!(
            related["location"]["uri"]
                .as_str()
                .unwrap()
                .contains("Lib.ply"),
            "{related}"
        );
        assert_eq!(related["message"], "does not export answer");
        assert_eq!(related["location"]["range"]["start"]["line"], 0);
        assert_eq!(related["location"]["range"]["start"]["character"], 0);

        change(broken);
        let published = drain_diagnostics(&client);
        assert_eq!(published.len(), 2, "{published:?}");
        let diagnostics = published[1]["diagnostics"].as_array().unwrap();
        let hint = diagnostics
            .iter()
            .find(|item| item["message"].as_str().unwrap().contains("does not check"))
            .expect("stale hint");
        assert_eq!(hint["severity"], 4);
        assert!(hint.get("tags").is_none(), "{hint}");
        assert_eq!(hint["message"], "Data/Lib does not check");
        let jump = &hint["relatedInformation"][0];
        assert!(
            jump["location"]["uri"]
                .as_str()
                .unwrap()
                .contains("Lib.ply"),
            "{jump}"
        );
        assert!(!jump["message"].as_str().unwrap().is_empty());

        change(broken_more);
        let published = drain_diagnostics(&client);
        assert_eq!(published.len(), 1, "{published:?}");
        assert_eq!(published[0]["uri"], lib_uri);

        change(good);
        let published = drain_diagnostics(&client);
        assert_eq!(published.len(), 2, "{published:?}");
        assert_eq!(published[1]["uri"], app_uri);
        assert!(
            published[1]["diagnostics"].as_array().unwrap().is_empty(),
            "{published:?}"
        );

        client
            .sender
            .send(Message::Request(Request {
                id: RequestId::from(2),
                method: "shutdown".to_string(),
                params: json!(null),
            }))
            .unwrap();
        client
            .sender
            .send(Message::Notification(Notification {
                method: "exit".to_string(),
                params: json!(null),
            }))
            .unwrap();
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    fn drain_diagnostics(client: &Connection) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        let mut wait = Duration::from_secs(2);
        while let Ok(message) = client.receiver.recv_timeout(wait) {
            let Message::Notification(notification) = message else {
                panic!("expected diagnostics, got {message:?}");
            };
            assert_eq!(notification.method, "textDocument/publishDiagnostics");
            out.push(notification.params);
            wait = Duration::from_millis(200);
        }
        out
    }
}
