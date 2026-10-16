//! Minimal default system prompt (Loop by Soket AI).

use std::path::{Path, PathBuf};

use crate::config::paths::{append_system_md_path, get_agent_dir, get_project_dir, system_md_path};

/// Context file candidates per directory. Every group is loaded; within a group the
/// first existing spelling wins.
const CONTEXT_NAMES: &[&[&str]] = &[
    &["AGENTS.md", "AGENTS.MD"],
    &["CLAUDE.md", "CLAUDE.MD"],
    &["CLAUDE.local.md"],
    &[".claude/CLAUDE.md"],
];

/// A loaded project context file.
#[derive(Debug, Clone)]
pub struct ContextFile {
    /// Absolute path.
    pub path: PathBuf,
    /// File contents.
    pub content: String,
}

/// Options for building the system prompt.
pub struct BuildSystemPromptOptions<'a> {
    /// Custom replace prompt.
    pub custom_prompt: Option<&'a str>,
    /// Append text.
    pub append_system_prompt: Option<&'a str>,
    /// Working directory.
    pub cwd: &'a Path,
    /// Tool names selected.
    pub selected_tools: &'a [&'a str],
    /// One-line snippets.
    pub tool_snippets: &'a [(&'a str, &'a str)],
    /// Context files.
    pub context_files: &'a [ContextFile],
}

/// Default tool snippets.
pub fn default_tool_snippets() -> Vec<(&'static str, &'static str)> {
    vec![
        ("read", "Read file contents"),
        ("bash", "Run shell commands"),
        ("edit", "Apply exact string replacements in a file"),
        ("write", "Create or overwrite a file"),
        (
            "web_fetch",
            "Fetch a URL and return its text content (HTML becomes plain text)",
        ),
        (
            "web_search",
            "Search the web and return titles, URLs and snippets",
        ),
    ]
}

/// Build the system prompt.
///
/// Skills are not embedded here: `AgentHarness` appends the
/// `<available_skills>` block each turn (with paths) when `read` is available.
pub fn build_system_prompt(opts: BuildSystemPromptOptions<'_>) -> String {
    let cwd = opts.cwd.display().to_string().replace('\\', "/");
    let append = opts
        .append_system_prompt
        .map(|s| format!("\n\n{s}"))
        .unwrap_or_default();

    let mut prompt = if let Some(custom) = opts.custom_prompt {
        custom.to_string()
    } else {
        let tools_list = opts
            .tool_snippets
            .iter()
            .filter(|(name, _)| opts.selected_tools.contains(name))
            .map(|(n, s)| format!("- {n}: {s}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tools_list = if tools_list.is_empty() {
            "(none)".into()
        } else {
            tools_list
        };

        let mut guidelines = vec![
            "Be concise and direct. Prefer action over lengthy explanation.".to_string(),
            "Use tools to inspect the codebase before making changes.".to_string(),
        ];
        if opts.selected_tools.contains(&"web_search") {
            guidelines.push(
                "Discover unfamiliar URLs with web_search before using web_fetch; do not guess domains."
                    .into(),
            );
        }
        if opts.selected_tools.contains(&"bash")
            && !opts.selected_tools.contains(&"grep")
            && !opts.selected_tools.contains(&"find")
        {
            guidelines.push(
                "Use bash (rg/fd/find/grep) to explore when specialized search tools are unavailable."
                    .into(),
            );
        }

        format!(
            "You are an expert coding assistant in Loop, created by Soket AI.\n\n\
             # Available tools\n{tools_list}\n\n\
             # Guidelines\n{}\n",
            guidelines
                .iter()
                .map(|g| format!("- {g}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    prompt.push_str(&append);

    if !opts.context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for cf in opts.context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                cf.path.display(),
                cf.content
            ));
        }
        prompt.push_str("</project_context>\n");
    }

    prompt.push_str(&format!("\nCurrent working directory: {cwd}"));
    prompt
}

/// Resolve SYSTEM.md / APPEND_SYSTEM.md (project trusted > agent dir > CLI).
pub fn resolve_system_prompt_files(
    cwd: &Path,
    project_trusted: bool,
    cli_system: Option<&str>,
    cli_append: Option<&str>,
) -> (Option<String>, Option<String>) {
    let agent_dir = get_agent_dir();
    let mut custom = cli_system.map(|s| s.to_string());
    let mut append = cli_append.map(|s| s.to_string());

    let agent_sys = system_md_path(&agent_dir);
    if custom.is_none() && agent_sys.exists() {
        custom = std::fs::read_to_string(agent_sys).ok();
    }
    let agent_append = append_system_md_path(&agent_dir);
    if let Ok(extra) = std::fs::read_to_string(agent_append) {
        append = Some(match append {
            Some(a) => format!("{a}\n\n{extra}"),
            None => extra,
        });
    }

    if project_trusted {
        let project = get_project_dir(cwd);
        let p_sys = system_md_path(&project);
        if p_sys.exists() {
            if let Ok(s) = std::fs::read_to_string(p_sys) {
                custom = Some(s);
            }
        }
        let p_append = append_system_md_path(&project);
        if let Ok(extra) = std::fs::read_to_string(p_append) {
            append = Some(match append {
                Some(a) => format!("{a}\n\n{extra}"),
                None => extra,
            });
        }
    }

    (custom, append)
}

/// Load AGENTS.md / CLAUDE.md from the agent dir, `~/.claude`, and ancestors of cwd.
pub fn load_context_files(cwd: &Path, agent_dir: &Path) -> Vec<ContextFile> {
    let mut files = Vec::new();
    let mut seen_files = std::collections::HashSet::new();
    read_context_in_dir(agent_dir, &mut seen_files, &mut files);
    if let Some(home) = dirs::home_dir() {
        let path = home.join(".claude").join("CLAUDE.md");
        push_context_file(path, &mut seen_files, &mut files);
    }

    let mut dir = cwd.to_path_buf();
    let mut seen_dirs = std::collections::HashSet::new();
    loop {
        let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if seen_dirs.insert(key) {
            read_context_in_dir(&dir, &mut seen_files, &mut files);
        }
        if !dir.pop() {
            break;
        }
    }
    files
}

fn read_context_in_dir(
    dir: &Path,
    seen: &mut std::collections::HashSet<PathBuf>,
    out: &mut Vec<ContextFile>,
) {
    for group in CONTEXT_NAMES {
        for name in *group {
            if push_context_file(dir.join(name), seen, out) {
                break;
            }
        }
    }
}

/// Returns `true` when `path` is a readable file (even if skipped as a duplicate).
fn push_context_file(
    path: PathBuf,
    seen: &mut std::collections::HashSet<PathBuf>,
    out: &mut Vec<ContextFile>,
) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    // Symlinks (CLAUDE.md -> AGENTS.md) and case-insensitive filesystems.
    let key = path.canonicalize().unwrap_or_else(|_| path.clone());
    if seen.insert(key) && !out.iter().any(|f| f.content == content) {
        out.push(ContextFile { path, content });
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_agents_and_claude_files_without_duplicates() {
        let agent = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("sub");
        std::fs::create_dir_all(cwd.join(".claude")).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root agents").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "agents").unwrap();
        std::fs::write(cwd.join("CLAUDE.md"), "claude").unwrap();
        std::fs::write(cwd.join("CLAUDE.local.md"), "local").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(cwd.join("CLAUDE.md"), cwd.join(".claude").join("CLAUDE.md"))
            .unwrap();

        let contents: Vec<_> = load_context_files(&cwd, agent.path())
            .into_iter()
            .map(|f| f.content)
            .filter(|c| ["root agents", "agents", "claude", "local"].contains(&c.as_str()))
            .collect();
        assert_eq!(contents, ["agents", "claude", "local", "root agents"]);
    }
}
