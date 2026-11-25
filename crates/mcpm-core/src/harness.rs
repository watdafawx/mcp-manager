//! Harness registry with explicit profile roots.
use crate::model::Scope;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
/// Supported config serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Format {
    Json,
    Jsonc,
    Toml,
}
/// One harness configuration source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessFile {
    pub path: PathBuf,
    pub format: Format,
    pub container: String,
    pub scope: Scope,
    pub entry_style: String,
}
/// Harness definition and its alternative files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessDef {
    pub id: String,
    pub label: String,
    pub files: Vec<HarnessFile>,
}
/// Lists all supported global harness sources without accessing user files.
pub fn registry(home: &Path, appdata: &Path) -> Vec<HarnessDef> {
    let rows = [
        (
            "claude",
            "Claude Code",
            home.join(".claude.json"),
            Format::Json,
            "mcpServers",
        ),
        (
            "claude-desktop",
            "Claude Desktop",
            appdata.join("Claude/claude_desktop_config.json"),
            Format::Json,
            "mcpServers",
        ),
        (
            "codex",
            "Codex",
            home.join(".codex/config.toml"),
            Format::Toml,
            "mcp_servers",
        ),
        (
            "opencode",
            "OpenCode",
            home.join(".config/opencode/opencode.jsonc"),
            Format::Jsonc,
            "mcp",
        ),
        (
            "cursor",
            "Cursor",
            home.join(".cursor/mcp.json"),
            Format::Json,
            "mcpServers",
        ),
        (
            "vscode",
            "VS Code",
            appdata.join("Code/User/mcp.json"),
            Format::Jsonc,
            "servers",
        ),
        (
            "antigravity",
            "Antigravity",
            home.join(".gemini/antigravity/mcp_config.json"),
            Format::Json,
            "mcpServers",
        ),
        (
            "gemini-cli",
            "Gemini CLI",
            home.join(".gemini/settings.json"),
            Format::Json,
            "mcpServers",
        ),
        (
            "windsurf",
            "Windsurf",
            home.join(".codeium/windsurf/mcp_config.json"),
            Format::Json,
            "mcpServers",
        ),
    ];
    rows.into_iter()
        .map(|(id, label, path, format, container)| {
            let mut files = vec![HarnessFile {
                path,
                format,
                container: container.into(),
                scope: Scope::Global,
                entry_style: id.into(),
            }];
            if id == "opencode" {
                let mut f = files[0].clone();
                f.path = home.join(".config/opencode/opencode.json");
                files.push(f);
            }
            if id == "antigravity" {
                for p in [
                    ".gemini/antigravity-ide/mcp_config.json",
                    ".gemini/config/mcp_config.json",
                ] {
                    let mut f = files[0].clone();
                    f.path = home.join(p);
                    files.push(f);
                }
            }
            HarnessDef {
                id: id.into(),
                label: label.into(),
                files,
            }
        })
        .collect()
}
/// Detects existing config files or an existing harness profile/install directory.
pub fn detected(h: &HarnessDef) -> bool {
    h.files.iter().any(|f| {
        if f.path.exists() {
            return true;
        }
        let profile = match h.id.as_str() {
            "claude" => f.path.parent().map(|p| p.join(".claude")),
            "vscode" => f
                .path
                .parent()
                .and_then(|p| p.parent())
                .map(Path::to_path_buf),
            _ => f.path.parent().map(Path::to_path_buf),
        };
        profile.is_some_and(|p| p.is_dir())
    })
}
/// Lists read-only project config files.
pub fn project_files(project: &Path) -> Vec<HarnessDef> {
    [
        ("claude", ".mcp.json", "mcpServers"),
        ("cursor", ".cursor/mcp.json", "mcpServers"),
        ("vscode", ".vscode/mcp.json", "servers"),
    ]
    .into_iter()
    .map(|(id, p, c)| HarnessDef {
        id: id.into(),
        label: id.into(),
        files: vec![HarnessFile {
            path: project.join(p),
            format: Format::Jsonc,
            container: c.into(),
            scope: Scope::Project,
            entry_style: id.into(),
        }],
    })
    .collect()
}
#[cfg(test)]
mod tests {
    #[test]
    fn registry() {
        let d = tempfile::tempdir().unwrap();
        let r = super::registry(d.path(), d.path());
        assert_eq!(r.len(), 9);
        assert_eq!(
            r.iter()
                .find(|h| h.id == "antigravity")
                .unwrap()
                .files
                .len(),
            3
        );
    }
}
