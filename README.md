# Mise for Zed

A [Zed](https://zed.dev) extension for [mise](https://mise.jdx.dev) — the dev-tool
version manager, environment manager, and task runner.

Zed extensions run as sandboxed WebAssembly and **cannot draw custom UI** (no
panels, buttons, or tree views). So instead of a GUI, this extension delivers
Mise through the three surfaces Zed actually exposes:

| Capability | How |
| --- | --- |
| **Configure Mise** | Schema-aware completion, validation, and hover docs in `mise.toml` (Mise's published JSON schema + the `taplo` language server). |
| **Install / manage tools** | An MCP server the agent uses to run `mise install/use/ls/outdated/…`. |
| **Run tasks** | Mise tasks synced into Zed's built-in task picker (`.zed/tasks.json`), plus an agent `run_task` tool. |

## Components

- **Language + schema (`languages/mise/`, `src/lib.rs`):** registers a `Mise`
  language for `mise.toml` / `.mise.toml` / `mise.local.toml` (reusing the TOML
  grammar) and runs `taplo` against `https://mise.en.dev/schema/mise.json`.
- **`.tool-versions` highlighting (`languages/tool-versions/`):** a vendored
  tree-sitter grammar (`tree-sitter-tool-versions/`) highlighting tool names,
  versions, and comments in asdf/mise `.tool-versions` files.
- **Version intelligence (`mise-lsp/`):** a second language server (alongside
  taplo) that shows the latest available version as an **inlay hint** after each
  pinned tool version, and offers **code actions** to bump the version in-place
  ("Update node to 26") — optionally also running `mise install`. Driven by
  `mise outdated --bump --json`.
- **MCP server (`mise-mcp/`):** a small native binary speaking MCP over stdio that
  shells out to your `mise` CLI. The extension downloads it from this repo's
  GitHub releases on first use.
- **Task sync:** the `mise_sync_tasks` tool writes `.zed/tasks.json`.

## Requirements

- `mise` installed and on `PATH` (https://mise.jdx.dev/getting-started.html).
- Zed (recent enough to support `zed_extension_api` 0.7).

## Install (development)

1. Build the MCP server: `mise run build-server` (or
   `cargo build --release --manifest-path mise-mcp/Cargo.toml`).
   Until you cut a tagged release there's no binary to download, so set
   `"binary_path"` in the context server settings to the local build:
   `mise-mcp/target/release/mise-mcp`.
2. In Zed: **Extensions → Install Dev Extension** → select this folder. Zed
   compiles the extension crate to WebAssembly. (`mise run build-extension`
   compiles it standalone for a quick check.)

## Usage

### Config editing
Open a `mise.toml`. You should get completion and validation for `[tools]`,
`[env]`, `[tasks]`, `[settings]`, etc. If validation doesn't kick in for an
unusual filename (e.g. `mise.<env>.toml`), add a first line:

```toml
#:schema https://mise.en.dev/schema/mise.json
```

### Version checks & upgrades
In `mise.toml` / `.tool-versions`, an inlay hint (e.g. `→ 26.3.1`) appears after
any tool whose pinned version has a newer release available. Put the cursor on
that line and open code actions (the lightbulb) for **"Update <tool> to <ver>"**
(rewrites the version) or **"… and install"** (also runs `mise install`).

Inlay hints must be enabled in Zed — add `"inlay_hints": { "enabled": true }` to
your settings if you don't see them. Checks run on open/save.

For local development, point the LSP at your local build:

```jsonc
{ "lsp": { "mise-lsp": { "binary": { "path": "/abs/zed-mise/mise-lsp/target/release/mise-lsp" } } } }
```

### Agent (MCP)
Open the Agent panel; the `mise` context server provides tools like
`mise_list_tools`, `mise_list_remote_versions`, `mise_install`, `mise_run_task`,
and `mise_config_set`. Read-only tools work out of the box; state-changing tools
require opting in (see Settings).

### Tasks in the picker
Ask the agent to run `mise_sync_tasks` (or run it directly). Your tasks then show
up under `task: spawn` and run in Zed's terminal with mise's environment.

## Settings

Configure the context server in your Zed settings:

```jsonc
{
  "context_servers": {
    "mise": {
      "settings": {
        "allow_write": false,        // set true to allow install/use/run/config edits
        "project_root": null          // optional default cwd when a call omits `cwd`
      }
    }
  }
}
```

## Releasing

Push a `v*` tag. `.github/workflows/release.yml` cross-compiles `mise-mcp` for
macOS (arm64/x64), Linux (arm64/x64), and Windows (x64) and attaches archives
named `mise-mcp-<target>.{tar.gz,zip}` — which `src/lib.rs` downloads at runtime.

## Troubleshooting

**"could not run mise" / "mise not found":** editor-launched processes often run
with a minimal `PATH` that excludes `~/.local/bin`, so the server may not find
mise even though it works in your terminal (common with remote/devcontainer
setups). The server searches `PATH`, common install locations, and your login
shell automatically; if that still fails, set `mise_path` to the absolute path
from `which mise`:

```jsonc
{ "context_servers": { "mise": { "settings": { "mise_path": "/home/you/.local/bin/mise" } } } }
```

## Known limitations / TODO

- **taplo schema association** is configured via `language_server_workspace_configuration`;
  verify the exact `schema.associations` shape against a running taplo. The
  `#:schema` directive is a reliable fallback.
- **`MISE_MCP_REPO`** in `src/lib.rs` is a placeholder for the release repo; set it
  to wherever the `mise-mcp` binaries are published before relying on auto-download.
