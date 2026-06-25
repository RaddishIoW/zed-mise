//! `mise-lsp` — a small language server that adds version intelligence to
//! `mise.toml` / `.tool-versions` files:
//!
//!   * **Inlay hints** showing the latest available version after a pinned tool
//!     version (driven by `mise outdated --bump --json`).
//!   * **Code actions** to bump a tool's version in-place ("Update node to 26"),
//!     and a variant that additionally runs `mise install`.
//!
//! It runs alongside taplo (which handles schema validation); Zed merges the
//! capabilities of both servers for the same language.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use lsp_server::{Connection, Message, Request as ServerRequest, Response};
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionProviderCapability, CodeActionResponse, Command as LspCommand, ExecuteCommandOptions,
    ExecuteCommandParams, InlayHint, InlayHintKind, InlayHintLabel, InlayHintParams, InlayHintTooltip,
    OneOf, Position, Range as LspRange, ServerCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextEdit, Url, WorkspaceEdit,
};
use serde_json::Value;

const INSTALL_COMMAND: &str = "mise-lsp.install";

/// One outdated tool, as reported by `mise outdated --bump --json`.
#[derive(Clone)]
struct Outdated {
    bump: String,    // version string to write (user granularity, e.g. "26")
    latest: String,  // exact latest, e.g. "26.3.1"
    current: Option<String>,
}

#[derive(Default)]
struct State {
    docs: HashMap<Url, String>,
    analysis: HashMap<Url, HashMap<String, Outdated>>,
}

fn main() -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    let (connection, io_threads) = Connection::stdio();

    let capabilities = serde_json::to_value(ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        inlay_hint_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec![INSTALL_COMMAND.to_string()],
            ..Default::default()
        }),
        ..Default::default()
    })?;

    connection.initialize(capabilities)?;
    main_loop(connection)?;
    io_threads.join()?;
    Ok(())
}

fn main_loop(connection: Connection) -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    let state = Arc::new(Mutex::new(State::default()));
    let mise = Arc::new(resolve_mise_bin());
    let next_id = Arc::new(AtomicI32::new(1));

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                let response = handle_request(&req, &state, &mise);
                connection.sender.send(Message::Response(response))?;
            }
            Message::Notification(note) => {
                handle_notification(&note, &state, &mise, &connection.sender, &next_id);
            }
            Message::Response(_) => {} // responses to our inlayHint/refresh — ignore
        }
    }
    Ok(())
}

fn handle_request(
    req: &ServerRequest,
    state: &Arc<Mutex<State>>,
    mise: &str,
) -> Response {
    match req.method.as_str() {
        "textDocument/inlayHint" => {
            let params: InlayHintParams = match serde_json::from_value(req.params.clone()) {
                Ok(p) => p,
                Err(e) => return Response::new_err(req.id.clone(), -32602, e.to_string()),
            };
            let hints = inlay_hints(state, &params.text_document.uri);
            Response::new_ok(req.id.clone(), hints)
        }
        "textDocument/codeAction" => {
            let params: CodeActionParams = match serde_json::from_value(req.params.clone()) {
                Ok(p) => p,
                Err(e) => return Response::new_err(req.id.clone(), -32602, e.to_string()),
            };
            let actions = code_actions(state, &params);
            Response::new_ok(req.id.clone(), actions)
        }
        "workspace/executeCommand" => {
            let params: ExecuteCommandParams = match serde_json::from_value(req.params.clone()) {
                Ok(p) => p,
                Err(e) => return Response::new_err(req.id.clone(), -32602, e.to_string()),
            };
            let result = execute_command(mise, &params);
            Response::new_ok(req.id.clone(), result)
        }
        _ => Response::new_err(req.id.clone(), -32601, format!("unhandled: {}", req.method)),
    }
}

fn handle_notification(
    note: &lsp_server::Notification,
    state: &Arc<Mutex<State>>,
    mise: &Arc<String>,
    sender: &crossbeam_channel::Sender<Message>,
    next_id: &Arc<AtomicI32>,
) {
    match note.method.as_str() {
        "textDocument/didOpen" => {
            if let Some((uri, text)) = open_params(&note.params) {
                state.lock().unwrap().docs.insert(uri.clone(), text);
                spawn_analysis(uri, state.clone(), mise.clone(), sender.clone(), next_id.clone());
            }
        }
        "textDocument/didChange" => {
            if let Some((uri, text)) = change_params(&note.params) {
                state.lock().unwrap().docs.insert(uri, text);
            }
        }
        "textDocument/didSave" => {
            if let Some(uri) = uri_param(&note.params) {
                spawn_analysis(uri, state.clone(), mise.clone(), sender.clone(), next_id.clone());
            }
        }
        "textDocument/didClose" => {
            if let Some(uri) = uri_param(&note.params) {
                let mut s = state.lock().unwrap();
                s.docs.remove(&uri);
                s.analysis.remove(&uri);
            }
        }
        _ => {}
    }
}

// ---- analysis (runs off the main loop so `mise` latency never blocks editing) ----

fn spawn_analysis(
    uri: Url,
    state: Arc<Mutex<State>>,
    mise: Arc<String>,
    sender: crossbeam_channel::Sender<Message>,
    next_id: Arc<AtomicI32>,
) {
    std::thread::spawn(move || {
        let Some(dir) = uri.to_file_path().ok().and_then(|p| p.parent().map(Path::to_path_buf))
        else {
            return;
        };
        let map = run_outdated(&mise, &dir);
        state.lock().unwrap().analysis.insert(uri, map);
        // Ask the client to re-request inlay hints now that data is available.
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let _ = sender.send(Message::Request(ServerRequest {
            id: id.into(),
            method: "workspace/inlayHint/refresh".to_string(),
            params: Value::Null,
        }));
    });
}

fn run_outdated(mise: &str, dir: &Path) -> HashMap<String, Outdated> {
    let mut map = HashMap::new();
    let Ok(output) = Command::new(mise)
        .args(["outdated", "--bump", "--json"])
        .current_dir(dir)
        .output()
    else {
        return map;
    };
    if !output.status.success() {
        return map;
    }
    let Ok(json): Result<Value, _> = serde_json::from_slice(&output.stdout) else {
        return map;
    };
    let Some(obj) = json.as_object() else { return map };
    for (tool, entry) in obj {
        // `bump` is null when the tool is already at the newest version.
        let bump = entry.get("bump").and_then(Value::as_str);
        let Some(bump) = bump else { continue };
        let latest = entry
            .get("latest")
            .and_then(Value::as_str)
            .unwrap_or(bump)
            .to_string();
        let current = entry.get("current").and_then(Value::as_str).map(String::from);
        map.insert(
            tool.clone(),
            Outdated { bump: bump.to_string(), latest, current },
        );
    }
    map
}

// ---- inlay hints ----

fn inlay_hints(state: &Arc<Mutex<State>>, uri: &Url) -> Vec<InlayHint> {
    let s = state.lock().unwrap();
    let (Some(text), Some(analysis)) = (s.docs.get(uri), s.analysis.get(uri)) else {
        return Vec::new();
    };
    let mut hints = Vec::new();
    for (tool, range) in parse_tool_versions(uri, text) {
        let Some(out) = analysis.get(&tool) else { continue };
        let position = offset_to_position(text, range.end);
        let tooltip = match &out.current {
            Some(c) => format!("mise: {c} installed · {} available", out.latest),
            None => format!("mise: {} available", out.latest),
        };
        hints.push(InlayHint {
            position,
            label: InlayHintLabel::String(format!("→ {}", out.latest)),
            kind: Some(InlayHintKind::TYPE),
            text_edits: None,
            tooltip: Some(InlayHintTooltip::String(tooltip)),
            padding_left: Some(true),
            padding_right: None,
            data: None,
        });
    }
    hints
}

// ---- code actions ----

fn code_actions(state: &Arc<Mutex<State>>, params: &CodeActionParams) -> CodeActionResponse {
    let uri = &params.text_document.uri;
    let s = state.lock().unwrap();
    let (Some(text), Some(analysis)) = (s.docs.get(uri), s.analysis.get(uri)) else {
        return Vec::new();
    };

    let is_toml = !is_tool_versions(uri);
    let mut actions: CodeActionResponse = Vec::new();

    for (tool, range) in parse_tool_versions(uri, text) {
        let Some(out) = analysis.get(&tool) else { continue };
        let edit_range = LspRange {
            start: offset_to_position(text, range.start),
            end: offset_to_position(text, range.end),
        };
        // Only offer actions when the version overlaps the requested range/cursor.
        if !line_overlaps(&params.range, &edit_range) {
            continue;
        }
        let new_text = if is_toml {
            format!("\"{}\"", out.bump)
        } else {
            out.bump.clone()
        };
        let workspace_edit = WorkspaceEdit {
            changes: Some(HashMap::from([(
                uri.clone(),
                vec![TextEdit { range: edit_range, new_text }],
            )])),
            ..Default::default()
        };

        // Action 1: edit only.
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("Update {tool} to {}", out.bump),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(workspace_edit.clone()),
            ..Default::default()
        }));

        // Action 2: edit + install.
        let dir = uri
            .to_file_path()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
            .unwrap_or_default();
        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
            title: format!("Update {tool} to {} and install", out.bump),
            kind: Some(CodeActionKind::QUICKFIX),
            edit: Some(workspace_edit),
            command: Some(LspCommand {
                title: "Install".to_string(),
                command: INSTALL_COMMAND.to_string(),
                arguments: Some(vec![
                    Value::String(tool.clone()),
                    Value::String(out.bump.clone()),
                    Value::String(dir),
                ]),
            }),
            ..Default::default()
        }));
    }
    actions
}

fn execute_command(mise: &str, params: &ExecuteCommandParams) -> Option<Value> {
    if params.command != INSTALL_COMMAND {
        return None;
    }
    let args = &params.arguments;
    let tool = args.first().and_then(Value::as_str)?;
    let version = args.get(1).and_then(Value::as_str)?;
    let dir = args.get(2).and_then(Value::as_str).unwrap_or(".");
    let spec = format!("{tool}@{version}");
    let _ = Command::new(mise)
        .args(["install", &spec])
        .current_dir(dir)
        .output();
    None
}

// ---- document parsing ----

/// Returns `(tool_name, byte_range_of_version_value)` for every tool entry.
/// The byte range is what a code action replaces; its end is the inlay-hint anchor.
fn parse_tool_versions(uri: &Url, text: &str) -> Vec<(String, Range<usize>)> {
    if is_tool_versions(uri) {
        parse_dot_tool_versions(text)
    } else {
        parse_mise_toml(text)
    }
}

fn is_tool_versions(uri: &Url) -> bool {
    uri.path().rsplit('/').next().unwrap_or("").ends_with("tool-versions")
}

fn parse_mise_toml(text: &str) -> Vec<(String, Range<usize>)> {
    let mut out = Vec::new();
    let Ok(doc) = toml_edit::ImDocument::parse(text) else {
        return out;
    };
    let Some(tools) = doc.get("tools").and_then(|i| i.as_table()) else {
        return out;
    };
    for (key, item) in tools.iter() {
        let span = match item {
            toml_edit::Item::Value(toml_edit::Value::String(s)) => s.span(),
            toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => {
                t.get("version").and_then(|v| v.span())
            }
            _ => None,
        };
        if let Some(span) = span {
            out.push((key.to_string(), span));
        }
    }
    out
}

fn parse_dot_tool_versions(text: &str) -> Vec<(String, Range<usize>)> {
    let mut out = Vec::new();
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let code = match line.find('#') {
            Some(i) => &line[..i],
            None => line,
        };
        let tokens = whitespace_tokens(code, line_start);
        if tokens.len() >= 2 {
            out.push((tokens[0].0.clone(), tokens[1].1.clone()));
        }
    }
    out
}

/// Tokenize on whitespace, returning `(text, absolute_byte_range)` per token.
fn whitespace_tokens(s: &str, base: usize) -> Vec<(String, Range<usize>)> {
    let mut tokens = Vec::new();
    let bytes = s.as_bytes();
    let is_ws = |b: u8| b == b' ' || b == b'\t' || b == b'\r' || b == b'\n';
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && is_ws(bytes[i]) {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && !is_ws(bytes[i]) {
            i += 1;
        }
        if i > start {
            tokens.push((s[start..i].to_string(), base + start..base + i));
        }
    }
    tokens
}

// ---- helpers ----

fn offset_to_position(text: &str, offset: usize) -> Position {
    let mut line = 0u32;
    let mut character = 0u32;
    let mut idx = 0usize;
    for ch in text.chars() {
        if idx >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
        idx += ch.len_utf8();
    }
    Position { line, character }
}

fn line_overlaps(request: &LspRange, target: &LspRange) -> bool {
    request.start.line <= target.end.line && target.start.line <= request.end.line
}

// ---- notification param extraction ----

fn open_params(params: &Value) -> Option<(Url, String)> {
    let doc = params.get("textDocument")?;
    let uri = doc.get("uri").and_then(Value::as_str)?.parse().ok()?;
    let text = doc.get("text").and_then(Value::as_str)?.to_string();
    Some((uri, text))
}

fn change_params(params: &Value) -> Option<(Url, String)> {
    let uri: Url = params.get("textDocument")?.get("uri").and_then(Value::as_str)?.parse().ok()?;
    // Full sync: last content change holds the whole document.
    let text = params
        .get("contentChanges")?
        .as_array()?
        .last()?
        .get("text")?
        .as_str()?
        .to_string();
    Some((uri, text))
}

fn uri_param(params: &Value) -> Option<Url> {
    params.get("textDocument")?.get("uri").and_then(Value::as_str)?.parse().ok()
}

// ---- mise binary resolution (mirrors mise-mcp; MISE_BIN is set by the extension) ----

fn resolve_mise_bin() -> String {
    if let Ok(p) = std::env::var("MISE_BIN") {
        if !p.is_empty() && Path::new(&p).is_file() {
            return p;
        }
    }
    if let Some(p) = find_on_path("mise") {
        return p;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for c in [
        format!("{home}/.local/bin/mise"),
        format!("{home}/.local/share/mise/bin/mise"),
        "/opt/homebrew/bin/mise".to_string(),
        "/usr/local/bin/mise".to_string(),
        "/usr/bin/mise".to_string(),
    ] {
        if Path::new(&c).is_file() {
            return c;
        }
    }
    "mise".to_string()
}

fn find_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|c| c.is_file())
        .map(|c| c.to_string_lossy().into_owned())
}
