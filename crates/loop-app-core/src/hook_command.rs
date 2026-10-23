//! Running a command hook: one process per event, JSON in and out.
//!
//! The event goes to the command's stdin as JSON. The command answers with
//! its exit code and, optionally, JSON on stdout:
//!
//! - exit 0, empty stdout: no decision;
//! - exit 0, `{"decision": "allow" | "block", "reason": "…", "appendContext": "…"}`;
//! - exit 2: block, with stderr as the reason;
//! - anything else, output that is not JSON, a timeout or a crash: an error.
//!
//! The command runs in its own process group (Unix) and is killed with
//! everything it started when it times out or the turn is cancelled.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Default time a hook may take.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// Longest time a hook may be given.
pub const MAX_TIMEOUT_MS: u64 = 120_000;
/// Largest stdout a hook may print.
const STDOUT_CAP: u64 = 64 * 1024;
/// Most of stderr used as a block reason.
const REASON_CAP: usize = 4 * 1024;

/// What a command hook answered.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CommandAnswer {
    /// `Some(true)` to block, `Some(false)` to allow, `None` for no decision.
    pub block: Option<bool>,
    /// Why (for a block).
    pub reason: Option<String>,
    /// Text to add to a tool result.
    pub append_context: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StdoutAnswer {
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    append_context: Option<String>,
}

/// Run `argv` with `input` on stdin. `Err` describes a failure (timeout,
/// crash, bad output), which the caller turns into a block or a warning.
pub async fn run(
    argv: &[String],
    input: &serde_json::Value,
    cwd: &Path,
    env: &[(&str, String)],
    timeout: Duration,
) -> Result<CommandAnswer, String> {
    let (program, args) = argv.split_first().ok_or("empty command")?;
    let mut command = Command::new(expand_home(program));
    command
        .args(args.iter().map(|a| expand_home(a)))
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if cwd.is_dir() {
        command.current_dir(cwd);
    }
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {program}: {e}"))?;
    // Kills the whole group if this future is dropped (timeout, cancel).
    let mut guard = TreeKiller(child.id());

    let mut stdin = child.stdin.take().ok_or("stdin not captured")?;
    let mut stdout = child.stdout.take().ok_or("stdout not captured")?;
    let mut stderr = child.stderr.take().ok_or("stderr not captured")?;
    let payload = serde_json::to_vec(input).map_err(|e| e.to_string())?;

    let work = async {
        let write = async move {
            // A hook may exit without reading stdin; that is not an error.
            let _ = stdin.write_all(&payload).await;
            drop(stdin);
        };
        let read_out = async {
            let mut buf = Vec::new();
            let _ = (&mut stdout)
                .take(STDOUT_CAP + 1)
                .read_to_end(&mut buf)
                .await;
            buf
        };
        let read_err = async {
            let mut buf = Vec::new();
            let _ = (&mut stderr)
                .take(REASON_CAP as u64)
                .read_to_end(&mut buf)
                .await;
            buf
        };
        let ((), out, err) = tokio::join!(write, read_out, read_err);
        if out.len() as u64 > STDOUT_CAP {
            return Err(format!("printed more than {} KB", STDOUT_CAP / 1024));
        }
        let status = child.wait().await.map_err(|e| e.to_string())?;
        Ok((status, out, err))
    };
    let (status, out, err) = tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs_f32()))??;
    guard.0 = None;

    match status.code() {
        Some(0) => parse_stdout(&out),
        Some(2) => {
            let reason = String::from_utf8_lossy(&err).trim().to_owned();
            Ok(CommandAnswer {
                block: Some(true),
                reason: (!reason.is_empty()).then_some(reason),
                append_context: None,
            })
        }
        Some(code) => Err(format!("exited with code {code}{}", stderr_suffix(&err))),
        None => Err(format!("killed by a signal{}", stderr_suffix(&err))),
    }
}

fn parse_stdout(out: &[u8]) -> Result<CommandAnswer, String> {
    let text = String::from_utf8_lossy(out);
    if text.trim().is_empty() {
        return Ok(CommandAnswer::default());
    }
    let answer: StdoutAnswer =
        serde_json::from_str(text.trim()).map_err(|e| format!("printed invalid JSON: {e}"))?;
    let block = match answer.decision.as_deref() {
        None => None,
        Some("block") => Some(true),
        Some("allow") => Some(false),
        Some(other) => return Err(format!("unknown decision \"{other}\"")),
    };
    Ok(CommandAnswer {
        block,
        reason: answer.reason,
        append_context: answer.append_context,
    })
}

fn stderr_suffix(err: &[u8]) -> String {
    let text = String::from_utf8_lossy(err);
    let line = text.trim().lines().last().unwrap_or_default();
    if line.is_empty() {
        String::new()
    } else {
        format!(": {line}")
    }
}

fn expand_home(arg: &str) -> String {
    match (arg.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => arg.to_owned(),
    }
}

/// Kills a process and everything it started, unless disarmed.
struct TreeKiller(Option<u32>);

impl Drop for TreeKiller {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        #[cfg(unix)]
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    async fn sh(script: &str, timeout_ms: u64) -> Result<CommandAnswer, String> {
        run(
            &["sh".into(), "-c".into(), script.into()],
            &json!({"event": "tool_call", "tool": "bash"}),
            &std::env::temp_dir(),
            &[("LOOP_HOOK_EVENT", "tool_call".into())],
            Duration::from_millis(timeout_ms),
        )
        .await
    }

    #[tokio::test]
    async fn exit_codes_and_stdout() {
        assert_eq!(sh("exit 0", 5000).await, Ok(CommandAnswer::default()));
        assert_eq!(
            sh(r#"echo '{"decision":"allow"}'"#, 5000)
                .await
                .unwrap()
                .block,
            Some(false)
        );
        let blocked = sh(r#"echo '{"decision":"block","reason":"no"}'"#, 5000)
            .await
            .unwrap();
        assert_eq!(
            (blocked.block, blocked.reason.as_deref()),
            (Some(true), Some("no"))
        );
        let two = sh("echo 'not allowed' >&2; exit 2", 5000).await.unwrap();
        assert_eq!(
            (two.block, two.reason.as_deref()),
            (Some(true), Some("not allowed"))
        );
        let context = sh(r#"echo '{"appendContext":"hi"}'"#, 5000).await.unwrap();
        assert_eq!(context.append_context.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn failures_are_errors() {
        assert!(sh("echo boom >&2; exit 1", 5000)
            .await
            .unwrap_err()
            .contains("code 1: boom"));
        assert!(sh("echo nope", 5000)
            .await
            .unwrap_err()
            .contains("invalid JSON"));
        assert!(sh(r#"echo '{"decision":"maybe"}'"#, 5000)
            .await
            .unwrap_err()
            .contains("unknown decision"));
        assert!(sh("head -c 70000 /dev/zero | tr '\\0' a", 5000)
            .await
            .unwrap_err()
            .contains("more than 64 KB"));
        let missing = run(
            &["/nonexistent/hook".into()],
            &json!({}),
            Path::new("."),
            &[],
            Duration::from_secs(5),
        )
        .await;
        assert!(missing.unwrap_err().contains("cannot start"));
    }

    #[tokio::test]
    async fn the_event_arrives_on_stdin_with_env() {
        let answer = sh(
            r#"input=$(cat); case "$input" in *'"tool":"bash"'*) ;; *) exit 1;; esac;
               [ "$LOOP_HOOK_EVENT" = tool_call ] || exit 1;
               echo '{"decision":"block","reason":"saw it"}'"#,
            5000,
        )
        .await
        .unwrap();
        assert_eq!(answer.reason.as_deref(), Some("saw it"));
    }

    #[tokio::test]
    async fn a_slow_hook_times_out_and_its_children_die() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child.pid");
        let started = std::time::Instant::now();
        let err = sh(
            &format!("sleep 30 & echo $! > {}; wait", marker.display()),
            300,
        )
        .await
        .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let pid = std::fs::read_to_string(&marker).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "grandchild {pid} survived");
    }

    #[tokio::test]
    async fn dropping_the_run_kills_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child.pid");
        let script = format!("sleep 30 & echo $! > {}; wait", marker.display());
        let run = sh(&script, 60_000);
        let _ = tokio::time::timeout(Duration::from_millis(300), run).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let pid = std::fs::read_to_string(&marker).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "cancelled hook's child {pid} survived");
    }
}
