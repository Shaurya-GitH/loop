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
//!
//! `toolPermissions` keys may carry a pattern on the call's main argument:
//! `bash(git status*)`, `write(~/secrets/*)`, `web_fetch(https://docs.rs/*)`.
//! A pattern rule beats the plain tool rule; among matching pattern rules
//! `deny` beats `ask` beats `allow`. A bash command is split at `;`, `&&`,
//! `||`, `|` and `&`: an `allow` pattern applies only when every part
//! matches one, and a command with `$(…)` or backticks never matches an
//! `allow` pattern. A pattern `ask` still asks in `accept-edits`.

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

/// Whether a call can change files or run commands. An MCP tool counts
/// unless its server marks it read-only (the gate then passes
/// [`ToolKind::Read`]).
fn changes_things(tool: &str, kind: ToolKind) -> bool {
    is_file_write(tool)
        || tool == "bash"
        || (tool.starts_with("mcp__") && kind != ToolKind::Read)
        || matches!(
            kind,
            ToolKind::Edit | ToolKind::Delete | ToolKind::Move | ToolKind::Execute
        )
}

/// What `toolPermissions` says about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    /// The permission.
    pub permission: ToolPermission,
    /// Whether a pattern rule (`bash(git *)`) decided, not the plain tool rule.
    pub specific: bool,
}

impl Rule {
    /// A plain tool rule.
    pub fn plain(permission: ToolPermission) -> Self {
        Self {
            permission,
            specific: false,
        }
    }
}

fn strictness(p: ToolPermission) -> u8 {
    match p {
        ToolPermission::Allow => 0,
        ToolPermission::Ask => 1,
        ToolPermission::Deny => 2,
    }
}

/// Split `tool(pattern)` keys; plain keys have no pattern.
fn split_key(key: &str) -> (&str, Option<&str>) {
    match key.split_once('(') {
        Some((tool, rest)) if rest.ends_with(')') => (tool.trim(), Some(&rest[..rest.len() - 1])),
        _ => (key, None),
    }
}

/// The call argument patterns match: the command, path, URL or query.
fn subject(tool: &str, args: &Value) -> Option<String> {
    let key = match tool {
        "bash" => "command",
        "read" | "write" | "edit" => "path",
        "web_fetch" => "url",
        "web_search" => "query",
        _ => return Some(args.to_string()),
    };
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// The parts of a shell command, split at `;`, `&&`, `||`, `|`, `&` and
/// newlines outside quotes. `None` when it substitutes commands (`$(…)`,
/// backticks, `<(…)`) or has unbalanced quotes.
pub fn split_command(command: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let (mut single, mut double) = (false, false);
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '`' if !single => return None,
            '$' | '<' | '>' if !single && chars.peek() == Some(&'(') => return None,
            ';' | '\n' | '&' | '|' if !single && !double => {
                if matches!(chars.peek(), Some('&' | '|')) {
                    chars.next();
                }
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if single || double {
        return None;
    }
    parts.push(current);
    let parts: Vec<String> = parts
        .into_iter()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// Commands `read-only` runs: they print what they read and change nothing
/// (with the options [`read_only_part`] refuses).
const READ_ONLY_COMMANDS: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "grep", "rg", "find", "tree", "file", "stat", "du",
    "which", "echo", "sort", "uniq", "diff",
];

/// `git` subcommands `read-only` runs.
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "branch",
    "blame",
    "ls-files",
    "rev-parse",
];

/// Whether every part of `command` only reads: a command from
/// [`READ_ONLY_COMMANDS`] or a read-only `git` subcommand, with no output
/// redirect, command substitution or variable assignment.
pub fn read_only_command(command: &str) -> bool {
    split_command(command).is_some_and(|parts| {
        parts
            .iter()
            .all(|part| words(part).is_some_and(|w| read_only_part(&w)))
    })
}

/// The words of one command part, with quotes removed. `None` when it
/// redirects output (an unquoted `>`).
fn words(part: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let (mut single, mut double, mut started) = (false, false, false);
    let mut chars = part.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => {
                single = !single;
                started = true;
            }
            '"' if !single => {
                double = !double;
                started = true;
            }
            '\\' if !single => current.extend(chars.next()),
            '>' if !single && !double => return None,
            c if c.is_whitespace() && !single && !double => {
                if started || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                started = false;
            }
            c => current.push(c),
        }
    }
    if started || !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn read_only_part(words: &[String]) -> bool {
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    let has = |options: &[&str]| {
        args.iter().any(|a| {
            options
                .iter()
                .any(|o| a == o || (o.starts_with("--") && a.starts_with(&format!("{o}="))))
        })
    };
    let positional = || args.iter().filter(|a| !a.starts_with('-')).count();
    match program.as_str() {
        "git" => {
            let Some((sub, rest)) = args.split_first() else {
                return false;
            };
            if !READ_ONLY_GIT.contains(&sub.as_str()) || has(&["--output"]) {
                return false;
            }
            // `git branch NAME` creates a branch; only listing is a read.
            sub != "branch"
                || rest.iter().all(|a| {
                    a.starts_with('-')
                        && !matches!(
                            a.as_str(),
                            "-d" | "-D"
                                | "-m"
                                | "-M"
                                | "-c"
                                | "-C"
                                | "-f"
                                | "-u"
                                | "--delete"
                                | "--move"
                                | "--copy"
                                | "--force"
                                | "--edit-description"
                                | "--unset-upstream"
                        )
                        && !a.starts_with("--set-upstream-to")
                })
        }
        "find" => !has(&[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ]),
        // `-o FILE` writes FILE, also inside a cluster such as `-uo`.
        "sort" => !args.iter().any(|a| {
            a.starts_with("--output")
                || (a.starts_with('-') && !a.starts_with("--") && a.contains('o'))
        }),
        // `file -C` compiles a magic file.
        "file" => !has(&["-C", "--compile"]),
        "rg" => !has(&["--pre"]),
        "tree" => !has(&["-o"]),
        // `uniq IN OUT` writes OUT.
        "uniq" => positional() <= 1,
        program => READ_ONLY_COMMANDS.contains(&program),
    }
}

/// Whether a path pattern matches `path` as written, with `~` expanded, or
/// as an absolute path under `workspace`.
fn path_matches(pattern: &str, path: &str, workspace: &Path) -> bool {
    let expand = |p: &str| match (p.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => p.to_owned(),
    };
    let pattern = expand(pattern);
    let path = expand(path);
    let absolute = workspace.join(&path).to_string_lossy().into_owned();
    crate::hooks_load::glob_match(&pattern, &path)
        || crate::hooks_load::glob_match(&pattern, &absolute)
}

/// The `toolPermissions` rule for one call; tools with no rule are allowed.
pub fn rule_for(
    rules: &std::collections::BTreeMap<String, ToolPermission>,
    tool: &str,
    args: &Value,
    workspace: &Path,
) -> Rule {
    let glob = crate::hooks_load::glob_match;
    let subject = subject(tool, args);
    let mut specific: Option<ToolPermission> = None;
    let mut bash_allows = Vec::new();
    for (key, permission) in rules {
        let (tool_pattern, Some(pattern)) = split_key(key) else {
            continue;
        };
        if !glob(tool_pattern, tool) {
            continue;
        }
        let Some(subject) = subject.as_deref() else {
            continue;
        };
        let hit = if tool == "bash" {
            if *permission == ToolPermission::Allow {
                bash_allows.push(pattern);
                false
            } else {
                match split_command(subject) {
                    Some(parts) => parts.iter().any(|part| glob(pattern, part)),
                    None => glob(pattern, subject) || subject.contains(pattern.trim_matches('*')),
                }
            }
        } else if matches!(tool, "read" | "write" | "edit") {
            path_matches(pattern, subject, workspace)
        } else {
            glob(pattern, subject)
        };
        if hit && specific.is_none_or(|s| strictness(*permission) > strictness(s)) {
            specific = Some(*permission);
        }
    }
    if specific.is_none() && !bash_allows.is_empty() {
        let all_allowed = subject
            .as_deref()
            .and_then(split_command)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .all(|part| bash_allows.iter().any(|pattern| glob(pattern, part)))
            });
        if all_allowed {
            specific = Some(ToolPermission::Allow);
        }
    }
    if let Some(permission) = specific {
        return Rule {
            permission,
            specific: true,
        };
    }
    if let Some(permission) = rules.get(tool) {
        return Rule::plain(*permission);
    }
    // Plain glob keys such as `mcp__github__*`; the strictest match wins.
    let plain = rules
        .iter()
        .filter(|(key, _)| split_key(key).1.is_none() && key.contains('*') && glob(key, tool))
        .map(|(_, p)| *p)
        .max_by_key(|p| strictness(*p));
    Rule::plain(plain.unwrap_or(ToolPermission::Allow))
}

/// The decision for `tool` in `mode`. `rule` is what `toolPermissions` says
/// about this call ([`rule_for`]); `workspace` bounds `accept-edits`.
pub fn decide(
    mode: PermissionMode,
    rule: Rule,
    tool: &str,
    kind: ToolKind,
    args: &Value,
    workspace: &Path,
) -> Decision {
    let permission = rule.permission;
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
        PermissionMode::ReadOnly
            if tool == "bash"
                && args
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(read_only_command) =>
        {
            Decision::Allow
        }
        PermissionMode::ReadOnly if tool == "bash" => Decision::Deny(
            "read-only mode runs only commands that read (ls, cat, grep, rg, find, git \
             status/log/diff/show, …), without output redirects; the user can switch with \
             /permissions"
                .into(),
        ),
        PermissionMode::ReadOnly if changes_things(tool, kind) => Decision::Deny(format!(
            "`{tool}` is not allowed in read-only mode; the user can switch with /permissions"
        )),
        PermissionMode::ReadOnly => Decision::Allow,
        PermissionMode::Ask => from_setting(),
        PermissionMode::AcceptEdits if rule.specific && permission == ToolPermission::Ask => {
            Decision::Ask
        }
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
        let rule = rule_for(&defaults, tool, &args, ws);
        decide(
            mode,
            rule,
            tool,
            loop_agent::protocol_events::tool_kind(tool),
            &args,
            ws,
        )
    }

    #[test]
    fn read_only_runs_only_read_only_commands() {
        let allowed = [
            "ls -la",
            "ls | grep x",
            "cat a.txt",
            "grep -rn 'a > b' src",
            "rg --files | wc -l",
            "find . -name '*.rs'",
            "git status && git log --oneline -5",
            "git diff HEAD~1 -- src",
            "git branch -a",
            "head -20 README.md; tail -n 5 log.txt",
            "sort -u names.txt",
            "uniq names.txt",
        ];
        for command in allowed {
            assert!(read_only_command(command), "{command}");
        }
        let refused = [
            "cat a > b",
            "echo hi >> notes",
            "find . -delete",
            "find . -name x -exec rm {} ;",
            "git log && rm x",
            "git push",
            "git branch new-branch",
            "git branch -D old",
            "git -c core.pager=evil log",
            "git log --output=out.txt",
            "sort -o out.txt in.txt",
            "sort -uo out.txt in.txt",
            "file -C -m magic",
            "rg --pre ./script x",
            "tree -o out.txt",
            "uniq in.txt out.txt",
            "ls $(pwd)",
            "cat `which ls`",
            "FOO=1 ls",
            "/bin/ls",
            "cargo test",
            "tee out.txt",
            "",
        ];
        for command in refused {
            assert!(!read_only_command(command), "{command}");
        }
        let ws = std::env::temp_dir();
        let bash = |command: &str| decide_in(ReadOnly, "bash", json!({ "command": command }), &ws);
        assert_eq!(bash("git log"), Decision::Allow);
        assert!(matches!(bash("rm -rf x"), Decision::Deny(_)));
        assert!(matches!(
            decide(
                ReadOnly,
                Rule::plain(ToolPermission::Deny),
                "bash",
                ToolKind::Execute,
                &json!({"command": "ls"}),
                &ws
            ),
            Decision::Deny(_)
        ));
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
            (Ask, "web_fetch", &url, "allow"),
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
    fn read_only_allows_mcp_tools_marked_read_only() {
        let ws = std::env::temp_dir();
        let mcp = |kind| {
            decide(
                ReadOnly,
                Rule::plain(ToolPermission::Allow),
                "mcp__github__list_issues",
                kind,
                &json!({}),
                &ws,
            )
        };
        assert_eq!(mcp(ToolKind::Read), Decision::Allow);
        assert!(matches!(mcp(ToolKind::Other), Decision::Deny(_)));
    }

    #[test]
    fn deny_rules_apply_in_every_mode() {
        let ws = std::env::temp_dir();
        for mode in PermissionMode::ALL {
            let got = decide(
                mode,
                Rule::plain(ToolPermission::Deny),
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

    fn rules(entries: &[(&str, &str)]) -> std::collections::BTreeMap<String, ToolPermission> {
        crate::tool_approval::permissions_from_settings(
            &entries
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn bash_rule(rules: &std::collections::BTreeMap<String, ToolPermission>, cmd: &str) -> Rule {
        rule_for(rules, "bash", &json!({ "command": cmd }), Path::new("/ws"))
    }

    #[test]
    fn splitting_commands() {
        assert_eq!(
            split_command("git status && rm -rf x; ls | wc -l").unwrap(),
            ["git status", "rm -rf x", "ls", "wc -l"]
        );
        assert_eq!(
            split_command("echo 'a && b' \"c;d\"").unwrap(),
            ["echo 'a && b' \"c;d\""]
        );
        assert!(split_command("echo $(whoami)").is_none());
        assert!(split_command("echo `whoami`").is_none());
        assert!(split_command("echo 'unclosed").is_none());
        assert_eq!(
            split_command("echo '$(literal)'").unwrap(),
            ["echo '$(literal)'"]
        );
    }

    #[test]
    fn bash_allow_patterns_need_every_part() {
        let r = rules(&[
            ("bash", "ask"),
            ("bash(git status*)", "allow"),
            ("bash(ls*)", "allow"),
        ]);
        let allow = Rule {
            permission: ToolPermission::Allow,
            specific: true,
        };
        assert_eq!(bash_rule(&r, "git status --short"), allow);
        assert_eq!(bash_rule(&r, "git status && ls -la"), allow);
        assert_eq!(
            bash_rule(&r, "git status && rm -rf x"),
            Rule::plain(ToolPermission::Ask)
        );
        assert_eq!(
            bash_rule(&r, "git status $(rm -rf x)"),
            Rule::plain(ToolPermission::Ask)
        );
    }

    #[test]
    fn deny_patterns_match_any_part_and_beat_allow() {
        let r = rules(&[("bash(git *)", "allow"), ("bash(rm -rf *)", "deny")]);
        assert_eq!(
            bash_rule(&r, "git status; rm -rf build").permission,
            ToolPermission::Deny
        );
        assert_eq!(
            bash_rule(&r, "echo $(rm -rf build)").permission,
            ToolPermission::Deny,
            "an unparseable command is still checked against deny patterns"
        );
    }

    #[test]
    fn path_url_and_glob_tool_rules() {
        let home = dirs::home_dir().unwrap();
        let r = rules(&[
            ("write(~/secrets/*)", "deny"),
            ("write(src/generated/*)", "ask"),
            ("web_fetch(https://docs.rs/*)", "allow"),
            ("mcp__github__*", "ask"),
        ]);
        let ws = Path::new("/ws");
        let secret = home.join("secrets/key").to_string_lossy().into_owned();
        assert_eq!(
            rule_for(&r, "write", &json!({ "path": secret }), ws).permission,
            ToolPermission::Deny
        );
        let generated = rule_for(&r, "write", &json!({"path": "src/generated/a.rs"}), ws);
        assert_eq!(generated.permission, ToolPermission::Ask);
        assert!(generated.specific);
        assert_eq!(
            decide(
                AcceptEdits,
                generated,
                "write",
                ToolKind::Edit,
                &json!({"path": "src/generated/a.rs"}),
                ws
            ),
            Decision::Ask,
            "a pattern ask still asks in accept-edits"
        );
        assert_eq!(
            rule_for(
                &r,
                "web_fetch",
                &json!({"url": "https://docs.rs/serde"}),
                ws
            )
            .permission,
            ToolPermission::Allow
        );
        assert_eq!(
            rule_for(&r, "web_fetch", &json!({"url": "https://evil.example"}), ws).permission,
            ToolPermission::Allow,
            "the default web_fetch rule"
        );
        assert_eq!(
            rule_for(&r, "mcp__github__create_issue", &json!({}), ws).permission,
            ToolPermission::Ask
        );
        assert_eq!(
            rule_for(&r, "mcp__slack__post", &json!({}), ws).permission,
            ToolPermission::Allow
        );
    }
}
