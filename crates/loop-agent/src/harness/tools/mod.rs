//! Built-in coding tools over [`ExecutionEnv`](crate::harness::types::ExecutionEnv).

mod file_mutation_queue;
#[cfg(feature = "web")]
pub mod web;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use similar::{ChangeTag, TextDiff};

use crate::harness::tools::file_mutation_queue::with_file_mutation_queue;
use crate::harness::types::{ExecutionEnv, ShellExecOptions};
use crate::harness::utils::{execute_shell_with_capture, truncate_head, DEFAULT_MAX_BYTES};
use crate::types::{AgentTool, AgentToolResult};
use loop_ai::{ImageContent, TextContent, ToolResultContent};

pub(crate) fn schema_object(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

pub(crate) fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string field `{key}`"))
}

/// Create a read tool bound to `env`.
pub fn create_read_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool::simple(
        "read",
        "Read",
        "Read a file's contents (text or image).",
        schema_object(
            json!({"path": {"type": "string", "description": "File path"}}),
            &["path"],
        ),
        move |_id, args, _cancel, _on_update| {
            let env = Arc::clone(&env);
            async move {
                let path = arg_str(&args, "path")?;
                let abs = env
                    .absolute_path(Path::new(&path))
                    .map_err(|e| e.to_string())?;
                let info = env.file_info(&abs).await.map_err(|e| e.to_string())?;
                if info.is_dir {
                    return Err(format!("path is a directory: {}", abs.display()));
                }
                let mime = mime_guess::from_path(&abs).first_or_octet_stream();
                if mime.type_() == mime_guess::mime::IMAGE {
                    let bytes = env
                        .read_binary_file(&abs)
                        .await
                        .map_err(|e| e.to_string())?;
                    use base64::Engine as _;
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    return Ok(AgentToolResult {
                        content: vec![ToolResultContent::Image(ImageContent {
                            data: b64,
                            mime_type: mime.essence_str().to_string(),
                        })],
                        details: json!({"path": abs, "bytes": bytes.len()}),
                        usage: None,
                        added_tool_names: None,
                        terminate: None,
                    });
                }
                let text = env.read_text_file(&abs).await.map_err(|e| e.to_string())?;
                let (text, truncated) = truncate_head(&text, DEFAULT_MAX_BYTES);
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent {
                        text,
                        text_signature: None,
                    })],
                    details: json!({"path": abs, "truncated": truncated}),
                    usage: None,
                    added_tool_names: None,
                    terminate: None,
                })
            }
        },
    )
}

/// Create a write tool bound to `env`.
pub fn create_write_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool::simple(
        "write",
        "Write",
        "Write contents to a file (create or overwrite).",
        schema_object(
            json!({
                "path": {"type": "string"},
                "content": {"type": "string"}
            }),
            &["path", "content"],
        ),
        move |_id, args, _cancel, _on_update| {
            let env = Arc::clone(&env);
            async move {
                let path = arg_str(&args, "path")?;
                let content = arg_str(&args, "content")?;
                let abs = env
                    .absolute_path(Path::new(&path))
                    .map_err(|e| e.to_string())?;
                with_file_mutation_queue(abs.clone(), || {
                    let env = Arc::clone(&env);
                    let content = content.clone();
                    async move {
                        let (created, previous) = match env.read_text_file(&abs).await {
                            Ok(text) => (false, text),
                            Err(_) => (true, String::new()),
                        };
                        let previous_path =
                            write_review_snapshot(&abs, &previous).map_err(|e| e.to_string())?;
                        let diff = unified_diff(&previous, &content, &abs);
                        env.write_file(&abs, content.as_bytes())
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(AgentToolResult {
                            content: vec![ToolResultContent::Text(TextContent {
                                text: format!("Wrote {} bytes to {}", content.len(), abs.display()),
                                text_signature: None,
                            })],
                            details: json!({
                                "path": abs,
                                "bytes": content.len(),
                                "created": created,
                                "previousPath": previous_path,
                                "diff": diff,
                            }),
                            usage: None,
                            added_tool_names: None,
                            terminate: None,
                        })
                    }
                })
                .await
            }
        },
    )
}

/// Create an edit tool (exact/fuzzy replace) bound to `env`.
pub fn create_edit_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    AgentTool::simple(
        "edit",
        "Edit",
        "Replace text in a file. Prefers exact match; falls back to fuzzy match.",
        schema_object(
            json!({
                "path": {"type": "string"},
                "oldText": {"type": "string"},
                "newText": {"type": "string"}
            }),
            &["path", "oldText", "newText"],
        ),
        move |_id, args, _cancel, _on_update| {
            let env = Arc::clone(&env);
            async move {
                let path = arg_str(&args, "path")?;
                let old_text = args
                    .get("oldText")
                    .and_then(|v| v.as_str())
                    .ok_or("missing oldText")?
                    .to_string();
                let new_text = args
                    .get("newText")
                    .and_then(|v| v.as_str())
                    .ok_or("missing newText")?
                    .to_string();
                let abs = env
                    .absolute_path(Path::new(&path))
                    .map_err(|e| e.to_string())?;
                with_file_mutation_queue(abs.clone(), || {
                    let env = Arc::clone(&env);
                    async move {
                        let original = env.read_text_file(&abs).await.map_err(|e| e.to_string())?;
                        let updated = if original.contains(&old_text) {
                            original.replacen(&old_text, &new_text, 1)
                        } else {
                            fuzzy_replace(&original, &old_text, &new_text)?
                        };
                        let previous_path =
                            write_review_snapshot(&abs, &original).map_err(|e| e.to_string())?;
                        let diff = unified_diff(&original, &updated, &abs);
                        env.write_file(&abs, updated.as_bytes())
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(AgentToolResult {
                            content: vec![ToolResultContent::Text(TextContent {
                                text: format!("Edited {}", abs.display()),
                                text_signature: None,
                            })],
                            details: json!({
                                "path": abs,
                                "diff": diff,
                                "created": false,
                                "previousPath": previous_path,
                            }),
                            usage: None,
                            added_tool_names: None,
                            terminate: None,
                        })
                    }
                })
                .await
            }
        },
    )
}

/// Persist pre-edit contents so the CLI can open a diff and revert on reject.
fn write_review_snapshot(path: &Path, contents: &str) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join("loop-file-review");
    std::fs::create_dir_all(&dir)?;
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let id = loop_ai::new_id();
    let out = dir.join(format!("{id}-{stem}.before"));
    std::fs::write(&out, contents)?;
    Ok(out)
}

fn fuzzy_replace(original: &str, old_text: &str, new_text: &str) -> Result<String, String> {
    // Strategy 1: whitespace-normalized match.
    // Build a map from normalized-string byte offset back to original byte offset.
    let needle: String = old_text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !needle.is_empty() {
        let mut norm_buf = String::new();
        let mut norm_to_orig: Vec<usize> = Vec::new();
        let mut in_ws = true;
        for (byte_idx, ch) in original.char_indices() {
            if ch.is_whitespace() {
                if !in_ws && !norm_buf.is_empty() {
                    norm_buf.push(' ');
                    norm_to_orig.push(byte_idx);
                    in_ws = true;
                }
            } else {
                norm_buf.push(ch);
                norm_to_orig.push(byte_idx);
                in_ws = false;
            }
        }
        norm_to_orig.push(original.len());

        if let Some(norm_start) = norm_buf.find(&needle) {
            let norm_end = norm_start + needle.len();
            let orig_start = norm_to_orig.get(norm_start).copied().unwrap_or(0);
            // Find the original end: skip any trailing whitespace in the original that was
            // collapsed during normalization.
            let raw_orig_end = norm_to_orig
                .get(norm_end)
                .copied()
                .unwrap_or(original.len());
            let _orig_end = original[..raw_orig_end]
                .trim_end()
                .len()
                .max(raw_orig_end.min(original.len()));

            let mut out = String::with_capacity(original.len() + new_text.len());
            out.push_str(&original[..orig_start]);
            out.push_str(new_text);
            out.push_str(&original[raw_orig_end..]);
            return Ok(out);
        }
    }

    // Strategy 2: trimmed match (ignore leading/trailing whitespace).
    let old_norm = old_text.trim();
    if !old_norm.is_empty() {
        if let Some(pos) = original.find(old_norm) {
            let mut out = String::with_capacity(original.len() + new_text.len());
            out.push_str(&original[..pos]);
            out.push_str(new_text);
            out.push_str(&original[pos + old_norm.len()..]);
            return Ok(out);
        }
    }

    Err("oldText not found (exact or fuzzy)".into())
}

fn unified_diff(old: &str, new: &str, path: &Path) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("--- a/{}\n+++ b/{}\n", path.display(), path.display());
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Default patterns that are always blocked regardless of user configuration.
const DEFAULT_BLOCKED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "rm -rf ~",
    "mkfs.",
    ":(){ :|:& };:",
    "> /dev/sda",
    "dd if=/dev/zero of=/dev/",
    "chmod -r 777 /",
    "chown -r",
    "format c:",
];

/// Check a command against the blocklist. Returns `Err` with a rejection message
/// if the command matches a blocked pattern.
pub fn check_command_policy(command: &str, extra_blocked: &[String]) -> Result<(), String> {
    let lower = command.to_lowercase();
    let trimmed = lower.trim();

    for pattern in DEFAULT_BLOCKED_PATTERNS {
        if trimmed.contains(pattern) {
            return Err(format!(
                "Command blocked by safety policy: matches blocked pattern `{pattern}`"
            ));
        }
    }

    for pattern in extra_blocked {
        let pat_lower = pattern.to_lowercase();
        if trimmed.contains(&pat_lower) {
            return Err(format!(
                "Command blocked by safety policy: matches custom blocked pattern `{pattern}`"
            ));
        }
    }

    Ok(())
}

/// Create a bash tool bound to `env`.
pub fn create_bash_tool(env: Arc<dyn ExecutionEnv>) -> AgentTool {
    create_bash_tool_with_prepare(env, None)
}

/// Optional prepare hook for bash.
pub type BashPrepare = Arc<
    dyn Fn(
            String,
            serde_json::Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(String, Option<PathBuf>), String>> + Send>,
        > + Send
        + Sync,
>;

/// Create bash tool with optional prepare hook returning (command, cwd).
pub fn create_bash_tool_with_prepare(
    env: Arc<dyn ExecutionEnv>,
    prepare: Option<BashPrepare>,
) -> AgentTool {
    AgentTool::simple(
        "bash",
        "Bash",
        "Run a shell command.",
        schema_object(
            json!({
                "command": {"type": "string"},
                "cwd": {"type": "string"}
            }),
            &["command"],
        ),
        move |_id, args, cancel, on_update| {
            let env = Arc::clone(&env);
            let prepare = prepare.clone();
            async move {
                let mut command = arg_str(&args, "command")?;
                let mut cwd = args.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from);

                check_command_policy(&command, &[])?;

                if let Some(prepare) = &prepare {
                    let (c, d) = prepare(command.clone(), args.clone()).await?;
                    command = c;
                    if d.is_some() {
                        cwd = d;
                    }
                }
                const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;
                let mut options = ShellExecOptions {
                    cwd,
                    cancel,
                    timeout_ms: Some(DEFAULT_BASH_TIMEOUT_MS),
                    ..Default::default()
                };
                // Ensure relative cwd resolves against env.cwd
                if let Some(ref c) = options.cwd {
                    if c.is_relative() {
                        options.cwd = Some(env.cwd().join(c));
                    }
                }
                if let Some(on_update) = on_update {
                    options.on_output = Some(Arc::new(move |text: &str| {
                        on_update(AgentToolResult {
                            content: vec![ToolResultContent::Text(TextContent {
                                text: text.to_string(),
                                text_signature: None,
                            })],
                            details: json!({ "partial": true }),
                            usage: None,
                            added_tool_names: None,
                            terminate: None,
                        });
                    }));
                }
                let captured = execute_shell_with_capture(env, &command, options, None).await?;
                Ok(AgentToolResult {
                    content: vec![ToolResultContent::Text(TextContent {
                        text: captured.text.clone(),
                        text_signature: None,
                    })],
                    details: json!({
                        "exitCode": captured.exit_code,
                        "truncated": captured.truncated,
                        "spillPath": captured.spill_path,
                    }),
                    usage: None,
                    added_tool_names: None,
                    terminate: None,
                })
            }
        },
    )
}
