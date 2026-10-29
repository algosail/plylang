use std::collections::HashMap;

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::{
    CompletionOptions, HoverProviderCapability, OneOf, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind,
};
use plylang::{check_buffer, complete, definition, hover, utf16_position, CompletionKind};
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
                if let Some((uri, text)) = document_text(&notification) {
                    docs.insert(uri, text);
                }
                if let Some(published) = publish_for(&notification) {
                    connection
                        .sender
                        .send(published.into())
                        .map_err(|error| error.to_string())?;
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

fn hover_response(docs: &HashMap<String, String>, request: &Request) -> Response {
    let params: HoverRequest = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(_) => return Response::new_ok(request.id.clone(), serde_json::Value::Null),
    };
    let Some(text) = docs.get(&params.text_document.uri) else {
        return Response::new_ok(request.id.clone(), serde_json::Value::Null);
    };
    let path = uri_path(&params.text_document.uri);
    let Some(found) = hover(
        text,
        std::path::Path::new(&path),
        params.position.line,
        params.position.character,
    ) else {
        return Response::new_ok(request.id.clone(), serde_json::Value::Null);
    };
    let (start_line, start_character) = utf16_position(text, found.span.start);
    let (end_line, end_character) = utf16_position(text, found.span.end);
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

fn definition_response(docs: &HashMap<String, String>, request: &Request) -> Response {
    let params: HoverRequest = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(_) => return Response::new_ok(request.id.clone(), serde_json::Value::Null),
    };
    let Some(text) = docs.get(&params.text_document.uri) else {
        return Response::new_ok(request.id.clone(), serde_json::Value::Null);
    };
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

fn completion_response(docs: &HashMap<String, String>, request: &Request) -> Response {
    let params: HoverRequest = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(_) => {
            return Response::new_ok(
                request.id.clone(),
                serde_json::json!({ "isIncomplete": false, "items": [] }),
            );
        }
    };
    let Some(text) = docs.get(&params.text_document.uri) else {
        return Response::new_ok(
            request.id.clone(),
            serde_json::json!({ "isIncomplete": false, "items": [] }),
        );
    };
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

fn publish_for(notification: &Notification) -> Option<Notification> {
    let (uri, text) = match notification.method.as_str() {
        "textDocument/didOpen" => {
            let params: DidOpen = serde_json::from_value(notification.params.clone()).ok()?;
            (params.text_document.uri, params.text_document.text)
        }
        "textDocument/didChange" => {
            let params: DidChange = serde_json::from_value(notification.params.clone()).ok()?;
            let text = params.content_changes.last()?.text.clone();
            (params.text_document.uri, text)
        }
        _ => return None,
    };
    Some(publish(&uri, &text))
}

fn publish(uri: &str, text: &str) -> Notification {
    let path = uri_path(uri);
    let error = check_buffer(std::path::Path::new(&path), text).err();
    let diagnostics = match error {
        None => Vec::new(),
        Some(error) => {
            let (start_line, start_character) = utf16_position(text, error.span.start);
            let (end_line, end_character) = utf16_position(text, error.span.end);
            vec![serde_json::json!({
                "range": {
                    "start": { "line": start_line, "character": start_character },
                    "end": { "line": end_line, "character": end_character },
                },
                "severity": 1,
                "source": "plylang",
                "code": error.code,
                "codeDescription": { "href": error.url() },
                "message": error.reported(),
            })]
        }
    };
    Notification {
        method: "textDocument/publishDiagnostics".to_string(),
        params: serde_json::json!({
            "uri": uri,
            "diagnostics": diagnostics,
        }),
    }
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
        assert!(notification.params["diagnostics"].as_array().unwrap().is_empty());

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
            "```ply\n: dup ( ..s a -- ..s a a )\n```\n\nCopies the top value. On `x y` the result is `x y y`.\n\n```ply\nInt -- Int Int\n```"
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
            response.result.unwrap()["contents"]["value"].as_str().unwrap(),
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
        let items = response.result.unwrap()["items"].as_array().unwrap().clone();
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

        client
            .sender
            .send(Message::Notification(Notification {
                method: "initialized".to_string(),
                params: json!({}),
            }))
            .unwrap();
        let source = "; Person = Person name : Str, age : Int\n\n: get ( Person -- Person )\n  1 >>\n";
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
        let items = response.result.unwrap()["items"].as_array().unwrap().clone();
        assert_eq!(
            items.iter().map(|item| item["label"].as_str().unwrap()).collect::<Vec<_>>(),
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
        let line = source[..offset].bytes().filter(|byte| *byte == b'\n').count() as u32;
        let line_start = source[..offset].rfind('\n').map(|index| index + 1).unwrap_or(0);
        let character = source[line_start..offset].encode_utf16().count() as u32;
        (line, character)
    }
}
