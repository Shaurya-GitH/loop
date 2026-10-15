//! Slash command registry and handlers.

use std::sync::Arc;

/// Built-in slash command descriptor.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    /// Name without leading `/`.
    pub name: &'static str,
    /// Short description.
    pub description: &'static str,
    /// Optional argument hint.
    pub args_hint: Option<&'static str>,
}

/// All built-in slash commands.
pub fn builtin_commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "theme",
            description: "Select or set UI theme",
            args_hint: Some("[name]"),
        },
        SlashCommand {
            name: "sandbox",
            description: "Show or change sandbox mode",
            args_hint: Some("[status|off|local [--full|--partial] [--crun|--runc|--runsc|--krun]]"),
        },
        #[cfg(feature = "telemetry")]
        SlashCommand {
            name: "tracing",
            description: "Langfuse tracing status, on/off, or setup",
            args_hint: Some("[status|enable|disable|setup [langfuse|otlp]]"),
        },
        SlashCommand {
            name: "settings",
            description: "Open settings overview",
            args_hint: None,
        },
        SlashCommand {
            name: "model",
            description: "Select model",
            args_hint: Some("[provider/model]"),
        },
        SlashCommand {
            name: "scoped-models",
            description: "Manage models for Ctrl+P cycling",
            args_hint: None,
        },
        SlashCommand {
            name: "export",
            description: "Export session to JSONL/HTML path",
            args_hint: Some("[path]"),
        },
        SlashCommand {
            name: "import",
            description: "Import a JSONL session",
            args_hint: Some("<path>"),
        },
        SlashCommand {
            name: "copy",
            description: "Copy last assistant message",
            args_hint: None,
        },
        SlashCommand {
            name: "name",
            description: "Set session display name",
            args_hint: Some("<name>"),
        },
        SlashCommand {
            name: "session",
            description: "Show session info and token stats",
            args_hint: None,
        },
        SlashCommand {
            name: "changelog",
            description: "Show changelog",
            args_hint: None,
        },
        SlashCommand {
            name: "hotkeys",
            description: "Show keyboard shortcuts",
            args_hint: None,
        },
        SlashCommand {
            name: "fork",
            description: "Edit an earlier user message in a forked session",
            args_hint: None,
        },
        SlashCommand {
            name: "clone",
            description: "Duplicate current session",
            args_hint: None,
        },
        SlashCommand {
            name: "tree",
            description: "Navigate session tree",
            args_hint: None,
        },
        SlashCommand {
            name: "trust",
            description: "Trust or untrust this project",
            args_hint: Some("[yes|no]"),
        },
        SlashCommand {
            name: "login",
            description: "Connect a model provider (Soket, OpenRouter, OpenAI, custom)",
            args_hint: Some("[soket|openrouter|openai|custom]"),
        },
        SlashCommand {
            name: "logout",
            description: "Disconnect a model provider",
            args_hint: Some("<provider>"),
        },
        SlashCommand {
            name: "new",
            description: "Start a new session",
            args_hint: None,
        },
        SlashCommand {
            name: "review",
            description: "Tool approval policy (files + bash)",
            args_hint: Some("[newSession|always|never]"),
        },
        SlashCommand {
            name: "ttfb",
            description: "Model response-header wait (off for long-running)",
            args_hint: Some("[off|on|60s|120000]"),
        },
        SlashCommand {
            name: "compact",
            description: "Compact conversation context",
            args_hint: Some("[prompt]"),
        },
        SlashCommand {
            name: "resume",
            description: "Resume a previous session",
            args_hint: None,
        },
        SlashCommand {
            name: "reload",
            description: "Reload config, skills, themes, extensions",
            args_hint: None,
        },
        SlashCommand {
            name: "skills",
            description: "Enable or disable skills (SKILL.md)",
            args_hint: Some("[list|enable <name>|disable <name>]"),
        },
        SlashCommand {
            name: "mcp",
            description: "Enable, disable, or authorize MCP server connections",
            args_hint: Some(
                "[list|reload|login <name>|logout <name>|enable <name>|disable <name>]",
            ),
        },
        SlashCommand {
            name: "quit",
            description: "Quit Loop",
            args_hint: None,
        },
        SlashCommand {
            name: "help",
            description: "List commands",
            args_hint: None,
        },
        #[cfg(feature = "orchestration")]
        SlashCommand {
            name: "workflow",
            description: "Run a multi-agent workflow from a goal",
            args_hint: Some("<goal>"),
        },
    ]
}

/// Autocomplete match with description (for the bottom dock).
#[derive(Debug, Clone)]
pub struct AutocompleteEntry {
    /// Display name including leading `/`.
    pub name: String,
    /// Short description.
    pub description: String,
}

/// Autocomplete matches for a partial slash command (names only).
pub fn autocomplete(prefix: &str, extra: &[(String, String)]) -> Vec<String> {
    autocomplete_entries(prefix, extra)
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// Autocomplete matches with descriptions for the bottom dock.
///
/// `extra` holds dynamic `(name, description)` entries (skills, prompt templates).
pub fn autocomplete_entries(prefix: &str, extra: &[(String, String)]) -> Vec<AutocompleteEntry> {
    let p = prefix.trim_start_matches('/').to_lowercase();
    let mut out: Vec<AutocompleteEntry> = builtin_commands()
        .into_iter()
        .filter(|c| c.name.starts_with(&p))
        .map(|c| AutocompleteEntry {
            name: format!("/{}", c.name),
            description: match c.args_hint {
                Some(h) => format!("{h} — {}", c.description),
                None => c.description.to_string(),
            },
        })
        .collect();
    for (e, desc) in extra {
        let name = e.trim_start_matches('/');
        if name.to_lowercase().starts_with(&p) {
            let label = format!("/{name}");
            if !out.iter().any(|x| x.name == label) {
                out.push(AutocompleteEntry {
                    name: label,
                    description: desc.clone(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Result of parsing a slash line.
#[derive(Debug, Clone)]
pub struct ParsedCommand {
    /// Command name.
    pub name: String,
    /// Remainder args.
    pub args: String,
}

/// Parse `/cmd args`.
pub fn parse_command(line: &str) -> Option<ParsedCommand> {
    let line = line.trim();
    if !line.starts_with('/') {
        return None;
    }
    let rest = &line[1..];
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n.to_string(), a.trim().to_string()),
        None => (rest.to_string(), String::new()),
    };
    if name.is_empty() {
        return None;
    }
    Some(ParsedCommand { name, args })
}

/// Parse flags after `/sandbox local` (`--full|--partial`, `--crun|--runc|--runsc|--krun`).
///
/// Defaults: `--full` and `--runc`. Positional `full`/`partial`/`runc`/… are accepted too.
pub fn parse_local_sandbox_flags(
    args: &[&str],
) -> Result<
    (
        loop_agent::harness::KrunIsolation,
        loop_agent::harness::LocalSandboxRuntime,
    ),
    String,
> {
    use loop_agent::harness::{KrunIsolation, LocalSandboxRuntime};

    let mut isolation: Option<KrunIsolation> = None;
    let mut runtime: Option<LocalSandboxRuntime> = None;

    for tok in args {
        let raw = tok.trim();
        if raw.is_empty() {
            continue;
        }
        let key = raw.trim_start_matches('-').to_ascii_lowercase();
        match key.as_str() {
            "full" => {
                if isolation.is_some() {
                    return Err("specify only one of --full / --partial".into());
                }
                isolation = Some(KrunIsolation::Full);
            }
            "partial" => {
                if isolation.is_some() {
                    return Err("specify only one of --full / --partial".into());
                }
                isolation = Some(KrunIsolation::Partial);
            }
            "crun" | "runc" | "runsc" | "gvisor" | "krun" => {
                if runtime.is_some() {
                    return Err("specify only one runtime (--crun|--runc|--runsc|--krun)".into());
                }
                runtime = LocalSandboxRuntime::parse(&key);
            }
            other => {
                return Err(format!(
                    "unknown sandbox option '{other}' (use --full|--partial and --crun|--runc|--runsc|--krun)"
                ));
            }
        }
    }

    Ok((
        isolation.unwrap_or(KrunIsolation::Full),
        runtime.unwrap_or(LocalSandboxRuntime::Runc),
    ))
}

/// Shared command side-effect messages for the UI.
#[derive(Debug, Clone)]
pub enum CommandEffect {
    /// Quit the app.
    Quit,
    /// Show a status/system line.
    Status(String),
    /// Replace theme.
    SetTheme(String),
    /// Open model picker (empty = picker, Some = set).
    SelectModel(Option<String>),
    /// Sandbox mode change.
    SetSandbox(String),
    /// Langfuse tracing control.
    #[cfg(feature = "telemetry")]
    Tracing(TracingCommand),
    /// Login flow.
    Login(Option<String>),
    /// Logout.
    Logout(Option<String>),
    /// New session.
    NewSession,
    /// File edit review policy.
    SetFileReview(Option<String>),
    /// Response-header (TTFB) timeout: `None` shows current; `Some` sets it.
    SetResponseHeaderTimeout(Option<String>),
    /// Compact.
    Compact(Option<String>),
    /// Copy last assistant.
    CopyLast,
    /// Show hotkeys.
    Hotkeys,
    /// Show help.
    Help,
    /// Show session info.
    SessionInfo,
    /// Show settings summary.
    Settings,
    /// Trust decision.
    Trust(Option<String>),
    /// Reload resources.
    Reload,
    /// `/skills` sub-command.
    Skills(SkillsCommand),
    /// Resume picker.
    Resume,
    /// Tree view.
    Tree,
    /// Set session name.
    SetName(String),
    /// Export path.
    Export(Option<String>),
    /// Import path.
    Import(String),
    /// Changelog.
    Changelog,
    /// Scoped models help.
    ScopedModels,
    /// Fork.
    Fork,
    /// Clone.
    CloneSession,
    /// MCP sub-command.
    Mcp(McpCommand),
    /// Multi-agent workflow from a goal string.
    #[cfg(feature = "orchestration")]
    Workflow {
        /// Goal description.
        goal: String,
        /// Optional concurrency override.
        concurrency: Option<usize>,
    },
    /// Skill invoke.
    /// Skill activate (does not send a prompt).
    Skill {
        /// Skill name.
        name: String,
        /// Optional args to place in the input buffer.
        args: String,
    },
    /// Prompt template.
    Template {
        /// Template name.
        name: String,
        /// Args.
        args: String,
    },
}

/// `/skills` subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillsCommand {
    /// Open the enable/disable picker.
    Picker,
    /// Print every discovered skill with its state.
    List,
    /// Enable a skill by name.
    Enable(String),
    /// Disable a skill by name.
    Disable(String),
}

const SKILLS_USAGE: &str =
    "Usage: /skills [list|enable <name>|disable <name>] — no args opens a picker";

/// Parse `/skills` arguments; `Err` carries the usage text.
pub fn parse_skills(args: &str) -> Result<SkillsCommand, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => Ok(SkillsCommand::Picker),
        ["list" | "ls"] => Ok(SkillsCommand::List),
        ["enable" | "on", name] => Ok(SkillsCommand::Enable(skill_arg(name))),
        ["disable" | "off", name] => Ok(SkillsCommand::Disable(skill_arg(name))),
        _ => Err(SKILLS_USAGE.into()),
    }
}

fn skill_arg(name: &str) -> String {
    name.strip_prefix("skill:").unwrap_or(name).to_string()
}

/// `/mcp` subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommand {
    /// Open the enable/disable picker.
    Picker,
    /// Print every configured server with its state.
    List,
    /// Disconnect and reconnect enabled servers.
    Reload,
    /// Enable a server by name.
    Enable(String),
    /// Disable a server by name.
    Disable(String),
    /// Open a browser and save OAuth tokens for a remote server.
    Login(String),
    /// Delete saved OAuth tokens for a remote server.
    Logout(String),
}

const MCP_USAGE: &str = "Usage: /mcp [list|reload|login <name>|logout <name>|enable <name>|disable <name>] — no args opens a picker";

/// Parse `/mcp` arguments; `Err` carries the usage text.
pub fn parse_mcp(args: &str) -> Result<McpCommand, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] => Ok(McpCommand::Picker),
        ["list" | "ls"] => Ok(McpCommand::List),
        ["reload"] => Ok(McpCommand::Reload),
        ["enable" | "on", name] => Ok(McpCommand::Enable((*name).to_string())),
        ["disable" | "off", name] => Ok(McpCommand::Disable((*name).to_string())),
        ["login", name] => Ok(McpCommand::Login((*name).to_string())),
        ["logout", name] => Ok(McpCommand::Logout((*name).to_string())),
        _ => Err(MCP_USAGE.into()),
    }
}

#[cfg(feature = "telemetry")]
use loop_app_core::config::TracingBackend;

/// `/tracing` subcommands.
#[cfg(feature = "telemetry")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TracingCommand {
    /// Show whether traces are exported, and where.
    Status,
    /// Turn exporting on (persisted).
    Enable,
    /// Turn exporting off (persisted).
    Disable,
    /// Open the setup wizard, optionally skipping the backend picker.
    Setup {
        /// Backend chosen on the command line.
        backend: Option<TracingBackend>,
    },
}

#[cfg(feature = "telemetry")]
const TRACING_USAGE: &str =
    "Usage: /tracing [status|enable|disable|setup [langfuse|otlp]] — setup opens a guided wizard";

/// Parse `/tracing` arguments; `Err` carries the usage text.
#[cfg(feature = "telemetry")]
pub fn parse_tracing(args: &str) -> Result<TracingCommand, String> {
    match args.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => Ok(TracingCommand::Status),
        ["enable" | "on"] => Ok(TracingCommand::Enable),
        ["disable" | "off"] => Ok(TracingCommand::Disable),
        ["setup"] => Ok(TracingCommand::Setup { backend: None }),
        ["setup", backend] => TracingBackend::parse(backend)
            .map(|backend| TracingCommand::Setup {
                backend: Some(backend),
            })
            .ok_or_else(|| TRACING_USAGE.to_string()),
        _ => Err(TRACING_USAGE.into()),
    }
}

/// Dispatch a built-in or dynamic command to an effect.
pub fn dispatch(
    cmd: &ParsedCommand,
    skill_names: &[String],
    template_names: &[String],
) -> CommandEffect {
    match cmd.name.as_str() {
        "quit" | "exit" | "q" => CommandEffect::Quit,
        "theme" => {
            if cmd.args.is_empty() {
                CommandEffect::Status("Usage: /theme [name] — use without args to list".into())
            } else {
                CommandEffect::SetTheme(cmd.args.clone())
            }
        }
        "sandbox" => CommandEffect::SetSandbox(cmd.args.clone()),
        #[cfg(feature = "telemetry")]
        "tracing" => match parse_tracing(&cmd.args) {
            Ok(command) => CommandEffect::Tracing(command),
            Err(usage) => CommandEffect::Status(usage),
        },
        "model" => CommandEffect::SelectModel(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "login" => CommandEffect::Login(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "logout" => CommandEffect::Logout(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "new" => CommandEffect::NewSession,
        "review" => CommandEffect::SetFileReview(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "ttfb" => CommandEffect::SetResponseHeaderTimeout(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "compact" => CommandEffect::Compact(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "copy" => CommandEffect::CopyLast,
        "hotkeys" => CommandEffect::Hotkeys,
        "help" => CommandEffect::Help,
        "session" => CommandEffect::SessionInfo,
        "settings" => CommandEffect::Settings,
        "trust" => CommandEffect::Trust(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "reload" => CommandEffect::Reload,
        "skills" => match parse_skills(&cmd.args) {
            Ok(command) => CommandEffect::Skills(command),
            Err(usage) => CommandEffect::Status(usage),
        },
        "resume" => CommandEffect::Resume,
        "tree" => CommandEffect::Tree,
        "name" => CommandEffect::SetName(cmd.args.clone()),
        "export" => CommandEffect::Export(if cmd.args.is_empty() {
            None
        } else {
            Some(cmd.args.clone())
        }),
        "import" => {
            if cmd.args.is_empty() {
                CommandEffect::Status("Usage: /import <path.jsonl>".into())
            } else {
                CommandEffect::Import(cmd.args.clone())
            }
        }
        "changelog" => CommandEffect::Changelog,
        "scoped-models" => CommandEffect::ScopedModels,
        "fork" => CommandEffect::Fork,
        "clone" => CommandEffect::CloneSession,
        "mcp" => match parse_mcp(&cmd.args) {
            Ok(command) => CommandEffect::Mcp(command),
            Err(usage) => CommandEffect::Status(usage),
        },
        #[cfg(feature = "orchestration")]
        "workflow" => {
            if cmd.args.is_empty() {
                CommandEffect::Status("Usage: /workflow <goal> — describe a multi-step task".into())
            } else {
                let (goal, concurrency) = parse_workflow_args(&cmd.args);
                CommandEffect::Workflow { goal, concurrency }
            }
        }
        other => {
            if let Some(name) = other.strip_prefix("skill:") {
                return CommandEffect::Skill {
                    name: name.to_string(),
                    args: cmd.args.clone(),
                };
            }
            if skill_names.iter().any(|s| s == other) {
                return CommandEffect::Skill {
                    name: other.to_string(),
                    args: cmd.args.clone(),
                };
            }
            if template_names.iter().any(|t| t == other) {
                return CommandEffect::Template {
                    name: other.to_string(),
                    args: cmd.args.clone(),
                };
            }
            CommandEffect::Status(format!("Unknown command: /{other}. Try /help"))
        }
    }
}

/// Format /help text.
pub fn help_text(extra: &[(String, String)]) -> String {
    let mut lines = vec!["Slash commands:".to_string()];
    for c in builtin_commands() {
        let hint = c.args_hint.unwrap_or("");
        lines.push(format!("  /{} {} — {}", c.name, hint, c.description));
    }
    if !extra.is_empty() {
        lines.push("Skill / prompt template commands:".into());
        for (e, desc) in extra {
            if desc.is_empty() {
                lines.push(format!("  /{e}"));
            } else {
                lines.push(format!("  /{e} — {desc}"));
            }
        }
    }
    lines.join("\n")
}

/// Parse `/workflow [--concurrency N] <goal>`.
#[cfg(feature = "orchestration")]
fn parse_workflow_args(args: &str) -> (String, Option<usize>) {
    let parts: Vec<&str> = args.splitn(3, char::is_whitespace).collect();
    if parts.len() >= 2 && (parts[0] == "--concurrency" || parts[0] == "-c") {
        if let Ok(n) = parts[1].parse::<usize>() {
            let goal = if parts.len() > 2 {
                parts[2].to_string()
            } else {
                String::new()
            };
            return (goal, Some(n));
        }
    }
    (args.to_string(), None)
}

/// Shared Arc alias for command lists in UI.
pub type SharedCommands = Arc<Vec<SlashCommand>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mcp_matches_skills_shape() {
        assert_eq!(parse_mcp(""), Ok(McpCommand::Picker));
        assert_eq!(parse_mcp("list"), Ok(McpCommand::List));
        assert_eq!(parse_mcp("ls"), Ok(McpCommand::List));
        assert_eq!(parse_mcp("reload"), Ok(McpCommand::Reload));
        assert_eq!(
            parse_mcp("enable filesystem"),
            Ok(McpCommand::Enable("filesystem".into()))
        );
        assert_eq!(
            parse_mcp("off remote"),
            Ok(McpCommand::Disable("remote".into()))
        );
        assert_eq!(
            parse_mcp("login notion"),
            Ok(McpCommand::Login("notion".into()))
        );
        assert_eq!(
            parse_mcp("logout notion"),
            Ok(McpCommand::Logout("notion".into()))
        );
        assert!(parse_mcp("enable").is_err());
        assert!(parse_mcp("login").is_err());
        assert!(parse_mcp("nope").is_err());
    }
}
