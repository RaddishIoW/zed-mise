//! `mise-mcp` — a minimal Model Context Protocol (MCP) server over stdio that
//! exposes `mise` operations as agent tools.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout (the MCP stdio
//! transport). We implement just what Zed's agent needs: `initialize`,
//! `tools/list`, and `tools/call` (plus `ping` and the `initialized` notice).
//!
//! Every tool shells out to the real `mise` CLI in a resolved working directory
//! and returns its output. State-changing tools require `MISE_MCP_ALLOW_WRITE`.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2024-11-05";

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);

        match handle(method, params) {
            // Notifications (no id) get no response.
            None => {}
            Some(result) => {
                let Some(id) = id else { continue };
                let msg = match result {
                    Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
                    Err((code, message)) => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                    }
                };
                if writeln!(out, "{msg}").is_err() {
                    break;
                }
                let _ = out.flush();
            }
        }
    }
}

/// Returns `None` for notifications (no reply expected).
fn handle(method: &str, params: Value) -> Option<Result<Value, (i64, String)>> {
    match method {
        "initialize" => Some(Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "serverInfo": {"name": "mise-mcp", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"tools": {}}
        }))),
        "notifications/initialized" => None,
        "ping" => Some(Ok(json!({}))),
        "tools/list" => Some(Ok(json!({"tools": tool_definitions()}))),
        "tools/call" => Some(Ok(handle_tool_call(params))),
        _ => Some(Err((-32601, format!("method not found: {method}")))),
    }
}

fn handle_tool_call(params: Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let cwd = resolve_cwd(&args);

    match dispatch(name, &args, &cwd) {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(text) => json!({"content": [{"type": "text", "text": text}], "isError": true}),
    }
}

fn dispatch(name: &str, args: &Value, cwd: &Path) -> Result<String, String> {
    let s = |key: &str| args.get(key).and_then(Value::as_str);
    let flag = |key: &str| args.get(key).and_then(Value::as_bool).unwrap_or(false);

    match name {
        // ---- read-only ----
        "mise_list_tools" => {
            let mut a = vec!["ls", "--json"];
            if flag("current") {
                a.push("--current");
            }
            run_mise(&a, cwd)
        }
        "mise_list_remote_versions" => {
            let tool = s("tool").ok_or("`tool` is required")?;
            run_mise(&["ls-remote", tool], cwd)
        }
        "mise_outdated" => run_mise(&["outdated", "--json"], cwd),
        "mise_current" => match s("tool") {
            Some(tool) => run_mise(&["current", tool], cwd),
            None => run_mise(&["current"], cwd),
        },
        "mise_list_tasks" => run_mise(&["tasks", "ls", "--json"], cwd),
        "mise_task_info" => {
            let task = s("task").ok_or("`task` is required")?;
            run_mise(&["tasks", "info", task, "--json"], cwd)
        }
        "mise_env" => run_mise(&["env"], cwd),
        "mise_doctor" => run_mise(&["doctor"], cwd),

        // ---- state-changing (gated) ----
        "mise_install" => {
            require_write()?;
            match tool_spec(args) {
                Some(spec) => run_mise(&["install", &spec], cwd),
                None => run_mise(&["install"], cwd),
            }
        }
        "mise_use" => {
            require_write()?;
            let spec = tool_spec(args).ok_or("`tool` is required")?;
            let mut a = vec!["use"];
            if flag("global") {
                a.push("-g");
            }
            a.push(&spec);
            run_mise(&a, cwd)
        }
        "mise_uninstall" => {
            require_write()?;
            let spec = tool_spec(args).ok_or("`tool` is required")?;
            run_mise(&["uninstall", &spec], cwd)
        }
        "mise_upgrade" => {
            require_write()?;
            match s("tool") {
                Some(tool) => run_mise(&["upgrade", tool], cwd),
                None => run_mise(&["upgrade"], cwd),
            }
        }
        "mise_run_task" => {
            require_write()?;
            let task = s("task").ok_or("`task` is required")?;
            let mut a = vec!["run", task];
            let extra: Vec<String> = args
                .get("args")
                .and_then(Value::as_array)
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            for e in &extra {
                a.push(e);
            }
            run_mise(&a, cwd)
        }
        "mise_config_set" => {
            require_write()?;
            let key = s("key").ok_or("`key` is required")?;
            let value = s("value").ok_or("`value` is required")?;
            run_mise(&["config", "set", key, value], cwd)
        }
        "mise_set_env" => {
            require_write()?;
            let key = s("key").ok_or("`key` is required")?;
            let value = s("value").ok_or("`value` is required")?;
            let assignment = format!("{key}={value}");
            run_mise(&["set", &assignment], cwd)
        }
        "mise_trust" => {
            require_write()?;
            run_mise(&["trust"], cwd)
        }
        "mise_sync_tasks" => {
            require_write()?;
            sync_tasks(cwd)
        }

        other => Err(format!("unknown tool: {other}")),
    }
}

/// Build a `tool[@version]` spec from `tool` + optional `version` arguments.
fn tool_spec(args: &Value) -> Option<String> {
    let tool = args.get("tool").and_then(Value::as_str)?;
    Some(match args.get("version").and_then(Value::as_str) {
        Some(v) => format!("{tool}@{v}"),
        None => tool.to_string(),
    })
}

fn require_write() -> Result<(), String> {
    if std::env::var("MISE_MCP_ALLOW_WRITE").is_ok() {
        Ok(())
    } else {
        Err("This tool changes state and is disabled. Set `allow_write: true` in the \
             mise context server settings to enable it."
            .into())
    }
}

fn resolve_cwd(args: &Value) -> PathBuf {
    if let Some(cwd) = args.get("cwd").and_then(Value::as_str) {
        return PathBuf::from(cwd);
    }
    if let Ok(cwd) = std::env::var("MISE_MCP_CWD") {
        return PathBuf::from(cwd);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolve (once) the absolute path to the `mise` binary. Editor-launched
/// processes frequently run with a minimal PATH, so relying on `mise` being on
/// PATH is not enough. Resolution order:
///   1. `MISE_BIN` env (set from the `mise_path` extension setting).
///   2. `mise` found on the current PATH.
///   3. Common install locations (`~/.local/bin`, Homebrew, `/usr/local/bin`…).
///   4. Ask the user's login/interactive shell (`command -v mise`).
/// Falls back to the bare name `mise` so `run_mise` can emit a helpful error.
fn mise_bin() -> &'static str {
    static BIN: OnceLock<String> = OnceLock::new();
    BIN.get_or_init(resolve_mise_bin)
}

fn resolve_mise_bin() -> String {
    if let Ok(p) = std::env::var("MISE_BIN") {
        if !p.is_empty() && is_file(&p) {
            return p;
        }
    }
    if let Some(p) = find_on_path("mise") {
        return p;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{home}/.local/bin/mise"),
        format!("{home}/.local/share/mise/bin/mise"),
        "/opt/homebrew/bin/mise".to_string(),
        "/usr/local/bin/mise".to_string(),
        "/usr/bin/mise".to_string(),
        "/home/linuxbrew/.linuxbrew/bin/mise".to_string(),
    ];
    for c in candidates {
        if is_file(&c) {
            return c;
        }
    }
    if let Some(p) = resolve_via_shell() {
        return p;
    }
    "mise".to_string()
}

fn is_file(path: &str) -> bool {
    std::fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

fn find_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

/// Ask the user's shell to resolve `mise`, so we pick up PATH set in shell rc
/// files / mise activation. Tries login (`-lc`) then interactive (`-ic`).
fn resolve_via_shell() -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    for flag in ["-lc", "-ic"] {
        let Ok(output) = Command::new(&shell).arg(flag).arg("command -v mise").output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        // Interactive shells may print banners first; take the last non-empty line.
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(line) = stdout.lines().rev().find(|l| !l.trim().is_empty()) {
            let path = line.trim().to_string();
            if is_file(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn run_mise(args: &[&str], cwd: &Path) -> Result<String, String> {
    let bin = mise_bin();
    let output = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| {
            format!(
                "could not run mise (resolved to `{bin}`): {e}.\n\
                 Install mise (https://mise.jdx.dev/getting-started.html), or set the \
                 `mise_path` setting (or MISE_BIN env) to mise's absolute path. Editor-launched \
                 processes often have a minimal PATH that excludes ~/.local/bin."
            )
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if output.status.success() {
        Ok(if stdout.trim().is_empty() { stderr } else { stdout })
    } else {
        let detail = if stderr.trim().is_empty() { &stdout } else { &stderr };
        Err(format!("`mise {}` failed:\n{detail}", args.join(" ")))
    }
}

/// Read `mise tasks ls --json` and merge the tasks into `.zed/tasks.json` so they
/// appear in Zed's built-in task picker. Idempotent: previously generated entries
/// (label prefix `mise: `) are replaced; other entries are preserved.
fn sync_tasks(cwd: &Path) -> Result<String, String> {
    let raw = run_mise(&["tasks", "ls", "--json"], cwd)?;
    let tasks: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("could not parse `mise tasks ls --json`: {e}"))?;
    let list = tasks
        .as_array()
        .ok_or("unexpected `mise tasks ls --json` output (expected an array)")?;

    let generated: Vec<Value> = list
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(|name| {
            json!({
                "label": format!("mise: {name}"),
                "command": "mise",
                "args": ["run", name],
                "tags": ["mise"]
            })
        })
        .collect();

    let zed_dir = cwd.join(".zed");
    let tasks_path = zed_dir.join("tasks.json");

    let mut merged: Vec<Value> = Vec::new();
    if let Ok(content) = std::fs::read_to_string(&tasks_path) {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&strip_line_comments(&content))
        {
            merged = items
                .into_iter()
                .filter(|item| {
                    !item
                        .get("label")
                        .and_then(Value::as_str)
                        .map(|l| l.starts_with("mise: "))
                        .unwrap_or(false)
                })
                .collect();
        }
    }
    let count = generated.len();
    merged.extend(generated);

    std::fs::create_dir_all(&zed_dir).map_err(|e| e.to_string())?;
    let pretty = serde_json::to_string_pretty(&Value::Array(merged)).map_err(|e| e.to_string())?;
    std::fs::write(&tasks_path, pretty).map_err(|e| e.to_string())?;

    Ok(format!("Synced {count} mise task(s) into {}", tasks_path.display()))
}

/// Best-effort removal of `//` line comments so we can re-parse an existing
/// JSONC `.zed/tasks.json`. Does not handle `/* */` or inline comments.
fn strip_line_comments(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_definitions() -> Vec<Value> {
    let cwd_prop = json!({
        "type": "string",
        "description": "Absolute path of the directory to run mise in (the project/worktree root). Strongly recommended."
    });
    let tool = |name: &str, description: &str, mut props: Value, required: Value| -> Value {
        // Every tool accepts an optional `cwd`.
        props
            .as_object_mut()
            .unwrap()
            .insert("cwd".into(), cwd_prop.clone());
        json!({
            "name": name,
            "description": description,
            "inputSchema": {"type": "object", "properties": props, "required": required}
        })
    };

    vec![
        tool(
            "mise_list_tools",
            "List installed mise tools and versions (`mise ls --json`). Set `current` to show only versions active for the directory.",
            json!({"current": {"type": "boolean", "description": "Only show currently active versions."}}),
            json!([]),
        ),
        tool(
            "mise_list_remote_versions",
            "List installable versions for a tool (`mise ls-remote <tool>`).",
            json!({"tool": {"type": "string", "description": "Tool name, e.g. `node`, `python`, `go`."}}),
            json!(["tool"]),
        ),
        tool(
            "mise_outdated",
            "Show installed tools that have newer versions available (`mise outdated --json`).",
            json!({}),
            json!([]),
        ),
        tool(
            "mise_current",
            "Show the active version(s) for the directory (`mise current [tool]`).",
            json!({"tool": {"type": "string", "description": "Optional tool to filter by."}}),
            json!([]),
        ),
        tool(
            "mise_list_tasks",
            "List defined mise tasks (`mise tasks ls --json`).",
            json!({}),
            json!([]),
        ),
        tool(
            "mise_task_info",
            "Show details for one task (`mise tasks info <task> --json`).",
            json!({"task": {"type": "string", "description": "Task name."}}),
            json!(["task"]),
        ),
        tool(
            "mise_env",
            "Print the environment mise would set for the directory (`mise env`).",
            json!({}),
            json!([]),
        ),
        tool(
            "mise_doctor",
            "Run mise diagnostics (`mise doctor`).",
            json!({}),
            json!([]),
        ),
        tool(
            "mise_install",
            "[write] Install tool(s). With `tool` installs that tool; otherwise installs everything in config (`mise install [tool@version]`).",
            json!({
                "tool": {"type": "string", "description": "Optional tool name."},
                "version": {"type": "string", "description": "Optional version (requires `tool`)."}
            }),
            json!([]),
        ),
        tool(
            "mise_use",
            "[write] Install a tool and record it in config (`mise use [-g] tool[@version]`).",
            json!({
                "tool": {"type": "string", "description": "Tool name."},
                "version": {"type": "string", "description": "Optional version."},
                "global": {"type": "boolean", "description": "Write to the global config instead of the local one."}
            }),
            json!(["tool"]),
        ),
        tool(
            "mise_uninstall",
            "[write] Uninstall a tool version (`mise uninstall tool[@version]`).",
            json!({
                "tool": {"type": "string", "description": "Tool name."},
                "version": {"type": "string", "description": "Optional version."}
            }),
            json!(["tool"]),
        ),
        tool(
            "mise_upgrade",
            "[write] Upgrade outdated tools (`mise upgrade [tool]`).",
            json!({"tool": {"type": "string", "description": "Optional specific tool to upgrade."}}),
            json!([]),
        ),
        tool(
            "mise_run_task",
            "[write] Run a mise task (`mise run <task> [args...]`).",
            json!({
                "task": {"type": "string", "description": "Task name."},
                "args": {"type": "array", "items": {"type": "string"}, "description": "Extra arguments passed to the task."}
            }),
            json!(["task"]),
        ),
        tool(
            "mise_config_set",
            "[write] Set a config value (`mise config set <key> <value>`), e.g. key `tools.node` value `22`.",
            json!({
                "key": {"type": "string", "description": "Dotted config key, e.g. `tools.node` or `settings.jobs`."},
                "value": {"type": "string", "description": "Value to set."}
            }),
            json!(["key", "value"]),
        ),
        tool(
            "mise_set_env",
            "[write] Set a project environment variable in config (`mise set KEY=VALUE`).",
            json!({
                "key": {"type": "string", "description": "Environment variable name."},
                "value": {"type": "string", "description": "Value."}
            }),
            json!(["key", "value"]),
        ),
        tool(
            "mise_trust",
            "[write] Trust the config file(s) in the directory (`mise trust`).",
            json!({}),
            json!([]),
        ),
        tool(
            "mise_sync_tasks",
            "[write] Write mise tasks into `.zed/tasks.json` so they appear in Zed's task picker.",
            json!({}),
            json!([]),
        ),
    ]
}
