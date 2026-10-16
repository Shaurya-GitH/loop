//! Starting an agent: its configuration and how Loop connects to it.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use loop_protocol::BackendInstanceId;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::process::{Child, Command};

/// Environment variables an agent inherits from Loop. Everything else,
/// including provider API keys, is withheld unless the agent's config sets it.
const INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "TEMP",
    "TMP",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    // Windows essentials.
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "HOMEDRIVE",
    "HOMEPATH",
];

/// One configured external agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    /// Backend instance id, such as `codex`.
    pub id: BackendInstanceId,
    /// Display name.
    pub name: String,
    /// Program to run.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Extra environment variables for the agent.
    pub env: Vec<(String, String)>,
    /// How long a cancelled turn may take to end before the agent process is
    /// killed.
    pub cancel_timeout: Duration,
    /// How long starting the agent and the ACP handshake may take. The first
    /// start of an `npx` agent includes downloading it.
    pub start_timeout: Duration,
}

impl AgentConfig {
    /// A config with no extra environment and a 10 second cancel timeout.
    pub fn new(id: impl Into<BackendInstanceId>, command: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.to_string(),
            id,
            command: command.into(),
            args: Vec::new(),
            env: Vec::new(),
            cancel_timeout: Duration::from_secs(10),
            start_timeout: Duration::from_secs(180),
        }
    }
}

/// A byte stream to an agent, plus control over the process behind it.
pub struct AgentLink {
    /// Agent output (its stdout).
    pub reader: Box<dyn AsyncRead + Unpin + Send>,
    /// Agent input (its stdin).
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    /// The process, when there is one.
    pub process: Option<AgentProcess>,
}

/// How Loop reaches an agent. Production uses [`ProcessConnector`]; tests
/// connect to an in-memory fake agent.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Start the agent and connect to it.
    async fn connect(&self, config: &AgentConfig, cwd: &Path) -> std::io::Result<AgentLink>;
}

/// Starts the agent as a child process speaking over stdio.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessConnector;

#[async_trait]
impl Connector for ProcessConnector {
    async fn connect(&self, config: &AgentConfig, cwd: &Path) -> std::io::Result<AgentLink> {
        let mut command = Command::new(resolve_program(&config.command));
        command
            .args(&config.args)
            .current_dir(cwd)
            .env_clear()
            .envs(
                INHERITED_ENV
                    .iter()
                    .filter_map(|name| std::env::var_os(name).map(|value| (*name, value))),
            )
            .envs(config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group, so killing the agent also kills what it started.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn()?;
        let missing = || std::io::Error::other("agent stdio was not captured");
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        if let Some(stderr) = child.stderr.take() {
            let agent = config.id.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "loop_acp::agent_stderr", %agent, "{line}");
                }
            });
        }
        Ok(AgentLink {
            reader: Box::new(stdout),
            writer: Box::new(stdin),
            process: Some(AgentProcess { child }),
        })
    }
}

/// On Windows, `npx` and similar tools are `.cmd` scripts that
/// `CreateProcess` cannot run by bare name.
fn resolve_program(command: &str) -> String {
    #[cfg(windows)]
    {
        let path = std::path::Path::new(command);
        if path.extension().is_none() && !command.contains(['/', '\\']) {
            for ext in [".exe", ".cmd", ".bat"] {
                let candidate = format!("{command}{ext}");
                if which_on_path(&candidate) {
                    return candidate;
                }
            }
        }
    }
    command.to_owned()
}

#[cfg(windows)]
fn which_on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// A running agent process.
pub struct AgentProcess {
    child: Child,
}

impl AgentProcess {
    /// Kill the agent and every process it started.
    pub async fn kill_tree(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child.id() {
            // The child leads its own process group (see `process_group(0)`).
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        #[cfg(windows)]
        if let Some(pid) = self.child.id() {
            let _ = Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        let _ = self.child.kill().await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kill_tree_stops_the_agent_and_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child.pid");
        let mut config = AgentConfig::new("t", "sh");
        config.args = vec![
            "-c".into(),
            format!("sleep 30 & echo $! > {}; wait", marker.display()),
        ];
        let mut link = ProcessConnector.connect(&config, dir.path()).await.unwrap();
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    break pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        link.process.as_mut().unwrap().kill_tree().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "grandchild {pid} survived");
    }

    #[tokio::test]
    async fn api_keys_are_not_inherited() {
        std::env::set_var("LOOP_ACP_TEST_SECRET_API_KEY", "sekrit");
        let dir = tempfile::tempdir().unwrap();
        let mut config = AgentConfig::new("t", "sh");
        config.args = vec!["-c".into(), "env".into()];
        config.env = vec![("EXPLICIT".into(), "yes".into())];
        let link = ProcessConnector.connect(&config, dir.path()).await.unwrap();
        let mut out = String::new();
        let mut reader = link.reader;
        tokio::io::AsyncReadExt::read_to_string(&mut reader, &mut out)
            .await
            .unwrap();
        assert!(!out.contains("sekrit"));
        assert!(out.contains("EXPLICIT=yes"));
        assert!(out.contains("PATH="));
    }
}
