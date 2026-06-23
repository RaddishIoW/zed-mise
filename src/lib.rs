//! Zed extension entry point for Mise.
//!
//! Provides three things (all that Zed's sandboxed extension API allows):
//!   1. A `taplo` language server for Mise config files, configured to validate
//!      against Mise's published JSON schema (schema-aware completion + diagnostics).
//!   2. An MCP context server (`mise-mcp`) exposing tool/task/config operations to
//!      Zed's agent. The native binary is downloaded from GitHub releases on demand.
//!   3. (via the MCP server) syncing Mise tasks into `.zed/tasks.json` so they show
//!      up in Zed's built-in task picker.

use std::fs;

use zed_extension_api::{
    self as zed, serde_json, settings::ContextServerSettings, Architecture, Command,
    ContextServerConfiguration, ContextServerId, DownloadedFileType, GithubReleaseOptions,
    LanguageServerId, Os, Project, Result, Worktree,
};

/// Mise's hosted JSON schema for `mise.toml`. See https://mise.jdx.dev/configuration.html
const MISE_SCHEMA_URL: &str = "https://mise.en.dev/schema/mise.json";
const TAPLO_REPO: &str = "tamasfe/taplo";
/// TODO: point this at the repository that publishes prebuilt `mise-mcp` binaries
/// (the GitHub Actions release workflow in `.github/workflows/release.yml`).
const MISE_MCP_REPO: &str = "raddishiow/zed-mise";

#[derive(Default)]
struct MiseExtension {
    taplo_path: Option<String>,
    mise_mcp_path: Option<String>,
}

impl MiseExtension {
    /// Locate `taplo` on PATH, or download a cached copy from GitHub releases.
    fn taplo_binary(&mut self, worktree: &Worktree) -> Result<String> {
        if let Some(path) = worktree.which("taplo") {
            return Ok(path);
        }
        if let Some(path) = &self.taplo_path {
            if fs::metadata(path).is_ok() {
                return Ok(path.clone());
            }
        }

        let release = zed::latest_github_release(
            TAPLO_REPO,
            GithubReleaseOptions { require_assets: true, pre_release: false },
        )?;
        let (os, arch) = zed::current_platform();
        let os_part = match os {
            Os::Mac => "darwin",
            Os::Linux => "linux",
            Os::Windows => "windows",
        };
        let arch_part = match arch {
            Architecture::Aarch64 => "aarch64",
            Architecture::X8664 => "x86_64",
            Architecture::X86 => "x86",
        };
        let asset_name = format!("taplo-{os_part}-{arch_part}.gz");
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| format!("no taplo release asset named `{asset_name}`"))?;

        let dir = format!("taplo-{}", release.version);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let binary = format!(
            "{dir}/taplo{}",
            if matches!(os, Os::Windows) { ".exe" } else { "" }
        );
        if fs::metadata(&binary).is_err() {
            zed::download_file(&asset.download_url, &binary, DownloadedFileType::Gzip)?;
            zed::make_file_executable(&binary)?;
        }
        self.taplo_path = Some(binary.clone());
        Ok(binary)
    }

    /// Download (and cache) the `mise-mcp` server binary for this platform.
    fn mise_mcp_binary(&mut self) -> Result<String> {
        if let Some(path) = &self.mise_mcp_path {
            if fs::metadata(path).is_ok() {
                return Ok(path.clone());
            }
        }

        let release = zed::latest_github_release(
            MISE_MCP_REPO,
            GithubReleaseOptions { require_assets: true, pre_release: false },
        )?;
        let (os, arch) = zed::current_platform();
        // Asset names use Rust target triples; see release.yml.
        let target = match (&os, &arch) {
            (Os::Mac, Architecture::Aarch64) => "aarch64-apple-darwin",
            (Os::Mac, Architecture::X8664) => "x86_64-apple-darwin",
            (Os::Linux, Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
            (Os::Linux, Architecture::X8664) => "x86_64-unknown-linux-gnu",
            (Os::Windows, Architecture::X8664) => "x86_64-pc-windows-msvc",
            (os, arch) => return Err(format!("unsupported platform: {os:?}/{arch:?}")),
        };
        let is_windows = matches!(os, Os::Windows);
        let ext = if is_windows { "zip" } else { "tar.gz" };
        let asset_name = format!("mise-mcp-{target}.{ext}");
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| format!("no mise-mcp release asset named `{asset_name}`"))?;

        let dir = format!("mise-mcp-{}", release.version);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let binary = format!(
            "{dir}/mise-mcp{}",
            if is_windows { ".exe" } else { "" }
        );
        if fs::metadata(&binary).is_err() {
            let file_type = if is_windows {
                DownloadedFileType::Zip
            } else {
                DownloadedFileType::GzipTar
            };
            zed::download_file(&asset.download_url, &dir, file_type)?;
            zed::make_file_executable(&binary)?;
        }
        self.mise_mcp_path = Some(binary.clone());
        Ok(binary)
    }
}

impl zed::Extension for MiseExtension {
    fn new() -> Self {
        Self::default()
    }

    fn language_server_command(
        &mut self,
        _id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        let taplo = self.taplo_binary(worktree)?;
        Ok(Command {
            command: taplo,
            args: vec!["lsp".into(), "stdio".into()],
            env: Default::default(),
        })
    }

    fn language_server_workspace_configuration(
        &mut self,
        _id: &LanguageServerId,
        _worktree: &Worktree,
    ) -> Result<Option<serde_json::Value>> {
        // Tell taplo to validate Mise config files against the Mise schema.
        // The regex keys match `mise.toml`, `mise.local.toml`, `.mise.toml`,
        // `mise.<env>.toml`, and `**/mise/config.toml`.
        // NOTE: verify taplo's exact `schema.associations` shape against a running
        // taplo; the `#:schema <url>` first-line directive is a reliable fallback.
        Ok(Some(serde_json::json!({
            "taplo": {
                "schema": {
                    "enabled": true,
                    "associations": {
                        ".*mise(\\.[^/]+)?\\.toml$": MISE_SCHEMA_URL,
                        ".*mise/config\\.toml$": MISE_SCHEMA_URL
                    }
                }
            }
        })))
    }

    fn context_server_command(
        &mut self,
        _id: &ContextServerId,
        project: &Project,
    ) -> Result<Command> {
        let mut allow_write = false;
        let mut project_root: Option<String> = None;
        // `binary_path` lets you point at a locally built `mise-mcp` (useful for
        // development before any GitHub release exists). When unset, the binary is
        // downloaded from GitHub releases.
        let mut binary_path: Option<String> = None;
        // `mise_path` overrides how the server locates the `mise` executable
        // (forwarded as MISE_BIN). Useful when mise isn't on the editor's PATH.
        let mut mise_path: Option<String> = None;

        if let Ok(settings) = ContextServerSettings::for_project("mise", project) {
            if let Some(value) = settings.settings {
                allow_write = value
                    .get("allow_write")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                project_root = value
                    .get("project_root")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                binary_path = value
                    .get("binary_path")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                mise_path = value
                    .get("mise_path")
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
        }

        let command = match binary_path {
            Some(path) => path,
            None => self.mise_mcp_binary()?,
        };

        let mut env: Vec<(String, String)> = Vec::new();
        if allow_write {
            env.push(("MISE_MCP_ALLOW_WRITE".into(), "1".into()));
        }
        if let Some(root) = project_root {
            env.push(("MISE_MCP_CWD".into(), root));
        }
        if let Some(path) = mise_path {
            env.push(("MISE_BIN".into(), path));
        }

        Ok(Command { command, args: Vec::new(), env })
    }

    fn context_server_configuration(
        &mut self,
        _id: &ContextServerId,
        _project: &Project,
    ) -> Result<Option<ContextServerConfiguration>> {
        Ok(Some(ContextServerConfiguration {
            installation_instructions: include_str!(
                "../configuration/installation_instructions.md"
            )
            .into(),
            default_settings: include_str!("../configuration/default_settings.jsonc").into(),
            settings_schema: include_str!("../configuration/settings_schema.json").into(),
        }))
    }
}

zed::register_extension!(MiseExtension);
