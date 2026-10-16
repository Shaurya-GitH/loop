//! Permission modes: what Loop lets a tool call do without asking.
//!
//! | Mode | read, search, web | write / edit | bash | MCP tools |
//! |---|---|---|---|---|
//! | `read-only` | allow | deny | deny | deny |
//! | `ask` (default) | per `toolPermissions` (web asks) | ask | ask | per `toolPermissions` |
//! | `accept-edits` | allow | allow in the workspace; ask outside it and for `.git/`, `.loop/`, `.env*` | per `toolPermissions` | per `toolPermissions` |
//! | `full-access` | allow | allow | allow | allow |
//!
//! A `deny` in `toolPermissions` applies in every mode. `read-only` is a
//! ceiling: nothing, not even a hook, lets a call that changes files or runs
//! commands through.

use std::path::{Component, Path, PathBuf};

use loop_protocol::ToolKind;
use serde_json::Value;

use crate::tool_approval::ToolPermission;

/// How much Loop's own tools may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum PermissionMode {
    /// Reading, searching and the web only.
    ReadOnly,
    /// Ask before edits, commands and web requests (`toolPermissions`).
    #[default]
    Ask,
    /// Edit files in the workspace without asking; ask for commands.
    AcceptEdits,
    /// Run every tool call without asking.
    FullAccess,
}

impl PermissionMode {
    /// Every mode, strictest first.
    pub const ALL: [Self; 4] = [
        Self::ReadOnly,
        Self::Ask,
        Self::AcceptEdits,
        Self::FullAccess,
    ];

    /// Parse a settings or flag value.
    pub fn parse(value: &str) -> Option<Self> {
        Some(
            match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
                "read-only" | "readonly" => Self::ReadOnly,
                "ask" | "default" => Self::Ask,
                "accept-edits" | "acceptedits" => Self::AcceptEdits,
                "full-access" | "fullaccess" => Self::FullAccess,
                _ => return None,
            },
        )
    }

    /// The settings and flag name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::AcceptEdits => "accept-edits",
            Self::FullAccess => "full-access",
        }
    }

    /// Short label for the footer.
    pub fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Ask => "ask",
            Self::AcceptEdits => "accept edits",
            Self::FullAccess => "full access",
        }
    }

    /// One line for the `/permissions` picker.
    pub fn description(self) -> &'static str {
        match self {
            Self::ReadOnly => "read, search and the web only; no edits or commands",
            Self::Ask => "ask before edits, commands and web requests",
            Self::AcceptEdits => "edit files in this workspace without asking; ask for commands",
            Self::FullAccess => "run every tool call without asking",
        }
    }
}

/// What the mode says about one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run it without asking.
    Allow,
    /// Ask the user (or refuse, when nobody can be asked).
    Ask,
    /// Refuse it; the reason goes to the model.
    Deny(String),
}

/// Tools that write files, by name.
fn is_file_write(tool: &str) -> bool {
    matches!(tool, "write" | "edit")
}

/// Whether a call can change files or run commands.
fn changes_things(tool: &str, kind: ToolKind) -> bool {
    is_file_write(tool)
        || tool == "bash"
        || tool.starts_with("mcp__")
        || matches!(
            kind,
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move | ToolKind::Execute
        )
}

/// The decision for `tool` in `mode`. `permission` is the tool's
/// `toolPermissions` entry (tools without one are allowed, as before
/// modes existed); `workspace` bounds `accept-edits`.
pub fn decide(
    mode: PermissionMode,
    permission: ToolPermission,
    tool: &str,
    kind: ToolKind,
    args: &Value,
    workspace: &Path,
) -> Decision {
    if permission == ToolPermission::Deny {
        return Decision::Deny(format!(
            "Tool `{tool}` is denied by settings.toolPermissions"
        ));
    }
    let from_setting = || match permission {
        ToolPermission::Allow => Decision::Allow,
        _ => Decision::Ask,
    };
    match mode {
        PermissionMode::ReadOnly if changes_things(tool, kind) => Decision::Deny(format!(
            "`{tool}` is not allowed in read-only mode; the user can switch with /permissions"
        )),
        PermissionMode::ReadOnly => Decision::Allow,
        PermissionMode::Ask => from_setting(),
        PermissionMode::AcceptEdits if is_file_write(tool) => {
            match args.get("path").and_then(Value::as_str) {
                Some(path) if inside_workspace(path, workspace) && !protected(path) => {
                    Decision::Allow
                }
                _ => from_setting(),
            }
        }
        PermissionMode::AcceptEdits if kind == ToolKind::Fetch => Decision::Allow,
        PermissionMode::AcceptEdits => from_setting(),
        PermissionMode::FullAccess => Decision::Allow,
    }
}

/// Paths `accept-edits` still asks about: version control, Loop's own
/// settings and environment files.
fn protected(path: &str) -> bool {
    let path = Path::new(path);
    path.components()
        .any(|c| matches!(c, Component::Normal(name) if name == ".git" || name == ".loop"))
        || path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(".env"))
}

/// Whether `path` (relative to `workspace` when not absolute) resolves inside
/// `workspace`, following symlinks of the parts that exist.
pub fn inside_workspace(path: &str, workspace: &Path) -> bool {
    let joined = workspace.join(path);
    let (Some(target), Some(root)) = (resolve(&joined), resolve(workspace)) else {
        return false;
    };
    target.starts_with(root)
}

/// Canonicalise the longest existing ancestor of `path` and append the
/// rest, rejecting `..` in the part that does not exist yet.
fn resolve(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut out = canonical;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        let name = existing.file_name()?.to_owned();
        if name == ".." {
            return None;
        }
        rest.push(name);
        existing = existing.parent()?.to_path_buf();
    }
}

/// The directory `accept-edits` lets Loop edit: the git root containing
/// `cwd`, or `cwd` itself.
pub fn workspace_root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use PermissionMode::*;

    fn decide_in(mode: PermissionMode, tool: &str, args: Value, ws: &Path) -> Decision {
        let defaults = crate::tool_approval::permissions_from_settings(&Default::default());
        let permission = defaults.get(tool).copied().unwrap_or(ToolPermission::Allow);
        decide(
            mode,
            permission,
            tool,
            loop_agent::protocol_events::tool_kind(tool),
            &args,
            ws,
        )
    }

    #[test]
    fn mode_table() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let inside = json!({"path": "src/main.rs"});
        let cmd = json!({"command": "cargo test"});
        let url = json!({"url": "https://example.com"});
        let deny = |d: Decision| matches!(d, Decision::Deny(_));
        let rows: [(PermissionMode, &str, &Value, &str); 20] = [
            (ReadOnly, "read", &inside, "allow"),
            (ReadOnly, "web_fetch", &url, "allow"),
            (ReadOnly, "write", &inside, "deny"),
            (ReadOnly, "bash", &cmd, "deny"),
            (ReadOnly, "mcp__github__create_issue", &cmd, "deny"),
            (Ask, "read", &inside, "allow"),
            (Ask, "web_fetch", &url, "ask"),
            (Ask, "write", &inside, "ask"),
            (Ask, "bash", &cmd, "ask"),
            (Ask, "mcp__github__create_issue", &cmd, "allow"),
            (AcceptEdits, "read", &inside, "allow"),
            (AcceptEdits, "web_search", &url, "allow"),
            (AcceptEdits, "write", &inside, "allow"),
            (AcceptEdits, "edit", &inside, "allow"),
            (AcceptEdits, "bash", &cmd, "ask"),
            (AcceptEdits, "mcp__github__create_issue", &cmd, "allow"),
            (FullAccess, "write", &inside, "allow"),
            (FullAccess, "bash", &cmd, "allow"),
            (FullAccess, "web_fetch", &url, "allow"),
            (FullAccess, "mcp__github__create_issue", &cmd, "allow"),
        ];
        for (mode, tool, args, want) in rows {
            let got = decide_in(mode, tool, args.clone(), ws);
            let ok = match want {
                "allow" => got == Decision::Allow,
                "ask" => got == Decision::Ask,
                _ => deny(got.clone()),
            };
            assert!(ok, "{mode:?} {tool}: want {want}, got {got:?}");
        }
    }

    #[test]
    fn accept_edits_asks_outside_the_workspace_and_for_protected_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("repo");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        for path in [
            "../elsewhere.txt",
            "/etc/hosts",
            ".git/config",
            "src/.env",
            ".env.local",
            ".loop/settings.json",
        ] {
            assert_eq!(
                decide_in(AcceptEdits, "write", json!({ "path": path }), &ws),
                Decision::Ask,
                "{path}"
            );
        }
        assert_eq!(
            decide_in(
                AcceptEdits,
                "write",
                json!({"path": "src/new/file.rs"}),
                &ws
            ),
            Decision::Allow
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_workspace_is_outside() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("repo");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, ws.join("link")).unwrap();
        assert!(!inside_workspace("link/file.txt", &ws));
        assert!(inside_workspace("file.txt", &ws));
    }

    #[test]
    fn deny_rules_apply_in_every_mode() {
        let ws = std::env::temp_dir();
        for mode in PermissionMode::ALL {
            let got = decide(
                mode,
                ToolPermission::Deny,
                "read",
                ToolKind::Read,
                &json!({}),
                &ws,
            );
            assert!(matches!(got, Decision::Deny(_)), "{mode:?}");
        }
    }

    #[test]
    fn parse_and_names_round_trip() {
        for mode in PermissionMode::ALL {
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(PermissionMode::parse("Accept_Edits"), Some(AcceptEdits));
        assert_eq!(PermissionMode::parse("yolo"), None);
        assert!(ReadOnly < Ask && Ask < AcceptEdits && AcceptEdits < FullAccess);
    }
}
