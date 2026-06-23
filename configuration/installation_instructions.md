# Mise context server

This server lets Zed's agent inspect and manage your [mise](https://mise.jdx.dev)
environment — listing/installing tools, listing/running tasks, and editing config.

## Requirements

- `mise` must be installed (see https://mise.jdx.dev/getting-started.html).
  Verify with `mise --version`.

The server tries hard to locate `mise` even when the editor's `PATH` is minimal
(it searches `PATH`, common install locations, and your login shell). If you still
see a "could not run mise" error — common in remote or devcontainer setups — set
`"mise_path"` below to the absolute path from `which mise`.

The server binary itself is downloaded automatically by this extension.

## Read-only by default

Tools that only read state (`mise_list_tools`, `mise_outdated`, `mise_list_tasks`,
`mise_task_info`, `mise_current`, `mise_env`, `mise_doctor`, `mise_list_remote_versions`)
always work.

State-changing tools (`mise_install`, `mise_use`, `mise_uninstall`, `mise_upgrade`,
`mise_run_task`, `mise_config_set`, `mise_set_env`, `mise_trust`, `mise_sync_tasks`)
are **disabled** unless you set `"allow_write": true` in the settings below.

## Working directory

mise resolves config from the current directory upward. The agent should pass the
project root as the `cwd` argument on each tool call. You can also set
`"project_root"` below as a default when `cwd` is omitted.

## Surfacing tasks in Zed's task picker

Call `mise_sync_tasks` to write your mise tasks into `.zed/tasks.json`. They then
appear in Zed's built-in picker via the `task: spawn` command and run in Zed's
terminal with mise's environment.
