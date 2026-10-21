//! Host (local machine) execution environment.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::harness::types::{
    ExecutionError, ExecutionErrorCode, FileError, FileErrorCode, FileInfo, FileSystem, Shell,
    ShellExecOptions, ShellOutput,
};
use crate::harness::utils::sanitize_binary_output;

/// Local filesystem + shell execution environment.
pub struct HostExecutionEnv {
    cwd: PathBuf,
}

impl HostExecutionEnv {
    /// Create with an absolute or relative cwd (canonicalized best-effort).
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        Self { cwd }
    }

    /// Create rooted at the process current directory.
    pub fn from_current_dir() -> Result<Self, FileError> {
        let cwd = std::env::current_dir()
            .map_err(|e| FileError::new(FileErrorCode::Io, format!("current_dir: {e}")))?;
        Ok(Self::new(cwd))
    }

    fn map_io(err: std::io::Error) -> FileError {
        let code = match err.kind() {
            std::io::ErrorKind::NotFound => FileErrorCode::NotFound,
            std::io::ErrorKind::PermissionDenied => FileErrorCode::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => FileErrorCode::AlreadyExists,
            _ => FileErrorCode::Io,
        };
        FileError::new(code, err.to_string())
    }
}

#[async_trait]
impl FileSystem for HostExecutionEnv {
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            Ok(self.cwd.join(path))
        }
    }

    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        base.join(child)
    }

    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::read_to_string(&path).await.map_err(Self::map_io)
    }

    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::read(&path).await.map_err(Self::map_io)
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        let path = self.absolute_path(path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(Self::map_io)?;
        }
        tokio::fs::write(&path, data).await.map_err(Self::map_io)
    }

    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        use tokio::io::AsyncWriteExt;
        let path = self.absolute_path(path)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(Self::map_io)?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(Self::map_io)?;
        file.write_all(data).await.map_err(Self::map_io)
    }

    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        let path = self.absolute_path(path)?;
        let meta = tokio::fs::metadata(&path).await.map_err(Self::map_io)?;
        Ok(FileInfo {
            path,
            is_dir: meta.is_dir(),
            is_file: meta.is_file(),
            size: meta.len(),
        })
    }

    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        let path = self.absolute_path(path)?;
        let mut rd = tokio::fs::read_dir(&path).await.map_err(Self::map_io)?;
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await.map_err(Self::map_io)? {
            let meta = entry.metadata().await.map_err(Self::map_io)?;
            out.push(FileInfo {
                path: entry.path(),
                is_dir: meta.is_dir(),
                is_file: meta.is_file(),
                size: meta.len(),
            });
        }
        Ok(out)
    }

    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        let path = self.absolute_path(path)?;
        Ok(tokio::fs::try_exists(&path).await.map_err(Self::map_io)?)
    }

    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::create_dir_all(&path).await.map_err(Self::map_io)
    }

    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        let path = self.absolute_path(path)?;
        let meta = tokio::fs::metadata(&path).await.map_err(Self::map_io)?;
        if meta.is_dir() {
            tokio::fs::remove_dir_all(&path).await.map_err(Self::map_io)
        } else {
            tokio::fs::remove_file(&path).await.map_err(Self::map_io)
        }
    }

    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        let path = self.absolute_path(path)?;
        tokio::fs::canonicalize(&path).await.map_err(Self::map_io)
    }

    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        let base = std::env::temp_dir();
        let name = format!("{prefix}-{}", loop_ai::new_id());
        let path = base.join(name);
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(Self::map_io)?;
        Ok(path)
    }
}

/// Minimum interval between live output callbacks.
const OUTPUT_EMIT_INTERVAL: Duration = Duration::from_millis(50);

#[async_trait]
impl Shell for HostExecutionEnv {
    async fn exec(
        &self,
        command: &str,
        options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        let cwd = options.cwd.clone().unwrap_or_else(|| self.cwd.clone());

        let mut cmd = if cfg!(target_os = "windows") {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", command]);
            c
        };
        cmd.current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a timeout or cancel can stop everything
        // the command started, not just the shell.
        #[cfg(unix)]
        cmd.process_group(0);

        if !options.inherit_env {
            cmd.env_clear();
        }
        if let Some(env) = &options.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| ExecutionError::new(ExecutionErrorCode::SpawnFailed, e.to_string()))?;
        // Kills the tree if this future is dropped before the command ends.
        let mut tree = TreeKiller(child.id());

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ExecutionError::new(ExecutionErrorCode::Io, "missing stdout pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ExecutionError::new(ExecutionErrorCode::Io, "missing stderr pipe"))?;

        let combined = Arc::new(Mutex::new(String::new()));
        let stdout_acc = Arc::new(Mutex::new(String::new()));
        let stderr_acc = Arc::new(Mutex::new(String::new()));
        let last_emit = Arc::new(Mutex::new(
            Instant::now()
                .checked_sub(OUTPUT_EMIT_INTERVAL)
                .unwrap_or_else(Instant::now),
        ));
        let on_output = options.on_output.clone();

        let stdout_task = {
            let combined = Arc::clone(&combined);
            let stdout_acc = Arc::clone(&stdout_acc);
            let last_emit = Arc::clone(&last_emit);
            let on_output = on_output.clone();
            tokio::spawn(async move {
                read_stream(stdout, combined, stdout_acc, last_emit, on_output).await;
            })
        };
        let stderr_task = {
            let combined = Arc::clone(&combined);
            let stderr_acc = Arc::clone(&stderr_acc);
            let last_emit = Arc::clone(&last_emit);
            let on_output = on_output.clone();
            tokio::spawn(async move {
                read_stream(stderr, combined, stderr_acc, last_emit, on_output).await;
            })
        };

        let status = match (&options.cancel, options.timeout_ms) {
            (Some(cancel), Some(ms)) => {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        tree.kill();
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        let _ = tokio::join!(stdout_task, stderr_task);
                        return Err(ExecutionError::new(
                            ExecutionErrorCode::Aborted,
                            "operation aborted",
                        ));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => {
                        tree.kill();
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        let _ = tokio::join!(stdout_task, stderr_task);
                        return Err(ExecutionError::new(
                            ExecutionErrorCode::TimedOut,
                            "command timed out",
                        ));
                    }
                    res = child.wait() => res.map_err(|e| {
                        ExecutionError::new(ExecutionErrorCode::Io, e.to_string())
                    })?,
                }
            }
            (Some(cancel), None) => {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        tree.kill();
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        let _ = tokio::join!(stdout_task, stderr_task);
                        return Err(ExecutionError::new(
                            ExecutionErrorCode::Aborted,
                            "operation aborted",
                        ));
                    }
                    res = child.wait() => res.map_err(|e| {
                        ExecutionError::new(ExecutionErrorCode::Io, e.to_string())
                    })?,
                }
            }
            (None, Some(ms)) => {
                match tokio::time::timeout(Duration::from_millis(ms), child.wait()).await {
                    Ok(res) => {
                        res.map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?
                    }
                    Err(_) => {
                        tree.kill();
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        let _ = tokio::join!(stdout_task, stderr_task);
                        return Err(ExecutionError::new(
                            ExecutionErrorCode::TimedOut,
                            "command timed out",
                        ));
                    }
                }
            }
            (None, None) => child
                .wait()
                .await
                .map_err(|e| ExecutionError::new(ExecutionErrorCode::Io, e.to_string()))?,
        };

        // A command that ended normally keeps what it left running.
        tree.0 = None;
        let _ = tokio::join!(stdout_task, stderr_task);

        let stdout_text = stdout_acc.lock().await.clone();
        let stderr_text = stderr_acc.lock().await.clone();
        let combined_text = combined.lock().await.clone();

        if let Some(cb) = &on_output {
            cb(&combined_text);
        }

        Ok(ShellOutput {
            stdout: stdout_text,
            stderr: stderr_text,
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

/// Kills a process and everything in its process group (Unix) or tree
/// (Windows), unless disarmed. Without it, a grandchild such as the `sleep`
/// in `sh -c "sleep 60; echo"` keeps the output pipes open after a timeout.
struct TreeKiller(Option<u32>);

impl TreeKiller {
    fn kill(&mut self) {
        let Some(pid) = self.0.take() else { return };
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

impl Drop for TreeKiller {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn read_stream<R: AsyncReadExt + Unpin>(
    mut reader: R,
    combined: Arc<Mutex<String>>,
    stream_acc: Arc<Mutex<String>>,
    last_emit: Arc<Mutex<Instant>>,
    on_output: Option<crate::harness::types::ShellOutputCallback>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                let chunk = sanitize_binary_output(&buf[..n]);
                {
                    let mut acc = stream_acc.lock().await;
                    acc.push_str(&chunk);
                }
                let snapshot = {
                    let mut c = combined.lock().await;
                    c.push_str(&chunk);
                    c.clone()
                };
                if let Some(cb) = &on_output {
                    let should_emit = {
                        let mut last = last_emit.lock().await;
                        if last.elapsed() >= OUTPUT_EMIT_INTERVAL {
                            *last = Instant::now();
                            true
                        } else {
                            false
                        }
                    };
                    if should_emit {
                        cb(&snapshot);
                    }
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    async fn run(
        command: &str,
        timeout_ms: Option<u64>,
        cancel: Option<CancellationToken>,
    ) -> Result<ShellOutput, ExecutionError> {
        HostExecutionEnv::new(std::env::temp_dir())
            .exec(
                command,
                ShellExecOptions {
                    timeout_ms,
                    cancel,
                    ..Default::default()
                },
            )
            .await
    }

    #[tokio::test]
    async fn a_timeout_stops_what_the_command_started() {
        let started = Instant::now();
        let err = run("sleep 5; echo done", Some(200), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, ExecutionErrorCode::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_cancel_stops_what_the_command_started() {
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let started = Instant::now();
        let err = run("sleep 5; echo done", Some(60_000), Some(cancel))
            .await
            .unwrap_err();
        assert_eq!(err.code, ExecutionErrorCode::Aborted);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn commands_do_not_read_loops_stdin() {
        let out = run("cat; echo end", Some(5_000), None).await.unwrap();
        assert_eq!(out.stdout.trim(), "end");
    }
}
