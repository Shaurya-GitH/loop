//! Loop — interactive coding agent CLI by Soket AI.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use loop_cli::print_mode::{self, TraceArgs};
use loop_cli::{bootstrap_cli, config, debug_log, BootstrapOpts};
#[cfg(feature = "telemetry")]
use loop_telemetry::TelemetryHandle;
#[cfg(feature = "telemetry")]
use tracing_subscriber::Layer;

/// Loop — interactive coding agent by Soket AI.
#[derive(Debug, Parser)]
#[command(name = "loop", version, about = "Interactive coding agent by Soket AI")]
struct Cli {
    /// Working directory.
    #[arg(long, global = true)]
    cwd: Option<PathBuf>,

    /// Provider id (default: soket).
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Model id (default: qwen3-30b).
    #[arg(long, global = true)]
    model: Option<String>,

    /// Theme name.
    #[arg(long, global = true)]
    theme: Option<String>,

    /// Replace system prompt.
    #[arg(long, global = true)]
    system_prompt: Option<String>,

    /// Append to system prompt.
    #[arg(long, global = true)]
    append_system_prompt: Option<String>,

    /// Do not load AGENTS.md / CLAUDE.md context files.
    #[arg(long, short = 'c', global = true)]
    no_context_files: bool,

    /// Resume a session by id (Loop's own or an external agent's).
    #[arg(long, global = true)]
    resume: Option<String>,

    /// Run without the web_search and web_fetch tools.
    #[arg(long, global = true)]
    no_web: bool,

    /// Print mode: send one prompt and exit (non-interactive).
    #[arg(long, global = true)]
    print: Option<String>,

    /// Write verbose session logs to `target/debug/logs/` (also: `LOOP_DEBUG=1`).
    #[arg(long, global = true)]
    debug: bool,

    /// Start as an MCP server (streamable HTTP) instead of interactive mode.
    #[arg(long)]
    serve_mcp: bool,

    /// Interface for the MCP server (default: 127.0.0.1).
    ///
    /// A non-loopback address requires `--mcp-token`.
    #[arg(long, default_value = "127.0.0.1")]
    mcp_host: std::net::IpAddr,

    /// Port for the MCP server (default: 3100).
    #[arg(long, default_value = "3100")]
    mcp_port: u16,

    /// Bearer token for MCP server authentication. When set, all HTTP requests
    /// to the MCP endpoint must include `Authorization: Bearer <token>`.
    /// Required when `--mcp-host` is not a loopback address.
    #[arg(long)]
    mcp_token: Option<String>,

    #[command(flatten)]
    trace: TraceArgs,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Show config directory paths.
    Config,
    /// Print version (same as --version).
    Version,
    /// Check for or install a verified GitHub release.
    Update {
        /// Show the available version without changing the installation.
        #[arg(long, conflicts_with = "version")]
        check: bool,
        /// Install a specific release, for example 0.3.5.
        #[arg(long)]
        version: Option<String>,
        /// Reinstall the same version or allow a downgrade.
        #[arg(long, conflicts_with = "check")]
        force: bool,
    },
    /// Restore the previous installed binary.
    Rollback,
    /// Remove the installed program; keep settings, credentials and sessions.
    Uninstall,
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("loop: {e:#}");
        std::process::exit(1);
    }
}

async fn real_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Distribution commands do not load agent settings, providers or sessions.
    if let Some(command) = &cli.command {
        match command {
            Commands::Update {
                check,
                version,
                force,
            } => {
                // Establish ownership before network traffic. The download has
                // no installation lock; replacement rechecks ownership later.
                loop_update::Installation::managed_executable()?;
                loop_update::cancellable(async {
                let client =
                    loop_update::ReleaseClient::github(std::time::Duration::from_secs(300))?;
                let release = match version {
                    Some(value) => loop_update::ReleaseVersion::parse(value)?,
                    None => { eprintln!("Checking GitHub for the latest release..."); client.latest().await? },
                };
                let current = env!("CARGO_PKG_VERSION");
                println!("Installed: {current}\nAvailable: {}", release.version);
                if *check {
                    if loop_update::is_newer(current, &release)? {
                        println!("Run `loop update` to install the new release.");
                    }
                    return Ok(());
                }
                match loop_update::plan(current, &release, *force)? {
                    loop_update::UpdatePlan::UpToDate => {
                        println!("Loop is up to date.");
                        return Ok(());
                    },
                    loop_update::UpdatePlan::DowngradeBlocked => return Err(loop_update::Error::Invalid("Use --force for an explicit downgrade, or `loop rollback` to restore the previous binary".into())),
                    loop_update::UpdatePlan::Install => (),
                }
                let staging = tempfile::Builder::new().prefix("loop-update-").tempdir()?;
                eprintln!("Downloading Loop {}...", release.version);
                let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
                let binary = client.download_with_progress(&release, staging.path(), |bytes, total| {
                    if last.elapsed() >= std::time::Duration::from_secs(1) || total == Some(bytes) {
                        eprintln!("Downloaded {} KiB{}", bytes / 1024, total.map(|n| format!(" / {} KiB", n / 1024)).unwrap_or_default());
                        last = std::time::Instant::now();
                    }
                }).await?;
                eprintln!("Verifying and replacing the installed binary...");
                let installation = loop_update::Installation::current()?;
                installation
                    .install(&binary, &release.version.to_string())
                    .await?;
                println!("Installed Loop {}. Run `loop` to start it, or `loop rollback` to restore the previous binary.", release.version);
                Ok(())
                }).await?;
                return Ok(());
            }
            Commands::Rollback => {
                let version = loop_update::cancellable(async {
                    loop_update::Installation::current()?.rollback().await
                })
                .await?;
                println!("Restored Loop {version}. Run `loop` to start it.");
                return Ok(());
            }
            Commands::Uninstall => {
                loop_update::Installation::current()?.uninstall()?;
                println!("Loop program files removed. User settings, credentials, skills and sessions kept.");
                return Ok(());
            }
            _ => (),
        }
    }
    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    let debug = debug_log::debug_enabled(cli.debug);
    let interactive = cli.print.is_none() && !cli.serve_mcp && cli.command.is_none();
    // Exports nothing until Langfuse credentials are attached below.
    #[cfg(feature = "telemetry")]
    let telemetry = TelemetryHandle::new(env!("CARGO_PKG_VERSION"), true);
    #[cfg(feature = "telemetry")]
    let export_layer = Some(telemetry.layer().boxed());
    #[cfg(not(feature = "telemetry"))]
    let export_layer = None;
    let debug_log_path = debug_log::init_tracing(debug, &cwd, interactive, export_layer)?;

    if let Some(Commands::Config) = cli.command {
        let agent = config::paths::get_agent_dir();
        println!("agent dir:     {}", agent.display());
        println!(
            "settings:      {}",
            config::paths::settings_path(&agent).display()
        );
        println!(
            "auth:          {}",
            config::paths::auth_path(&agent).display()
        );
        println!(
            "models store:  {}",
            config::paths::models_store_path(&agent).display()
        );
        println!(
            "sessions db:   {}",
            config::paths::sessions_db_path(&agent).display()
        );
        return Ok(());
    }
    if let Some(Commands::Version) = cli.command {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let interactive = cli.print.is_none() && !cli.serve_mcp;
    let agent_dir = config::paths::get_agent_dir();
    // `--resume` may name a session Loop opened with a subscription, and
    // `--provider` may name a subscription (such as `chatgpt`).
    let agent_resume = cli
        .resume
        .as_deref()
        .and_then(|id| loop_app_core::agents::find_recorded_session(&agent_dir, id));
    let subscription = cli
        .provider
        .clone()
        .filter(|p| loop_app_core::agents::subscription_ids(&agent_dir).contains(p));
    let pending_agent = match (&agent_resume, &subscription) {
        (Some(record), _) => Some(loop_cli::PendingAgent::Resume(record.clone())),
        (None, Some(id)) => Some(loop_cli::PendingAgent::New {
            id: id.clone(),
            model: cli.model.clone(),
        }),
        (None, None) => None,
    };
    let (provider, model) = if subscription.is_some() {
        (None, None)
    } else {
        (cli.provider, cli.model)
    };
    let explicit_model = provider.is_some() || model.is_some() || cli.resume.is_some();
    if cli.serve_mcp && pending_agent.is_some() {
        anyhow::bail!("--serve-mcp serves Loop's own tools; it cannot run with an external agent");
    }
    let mut runtime = bootstrap_cli(BootstrapOpts {
        cwd,
        provider,
        model,
        theme: cli.theme,
        system_prompt: cli.system_prompt,
        append_system_prompt: cli.append_system_prompt,
        no_context_files: cli.no_context_files,
        no_web: cli.no_web,
        interactive,
        session_id: if agent_resume.is_some() {
            None
        } else {
            cli.resume
        },
        agent: pending_agent.as_ref().map(|pending| match pending {
            loop_cli::PendingAgent::New { id, .. } => id.clone(),
            loop_cli::PendingAgent::Resume(record) => record.agent.clone(),
        }),
    })
    .await?;
    // With no model asked for, a saved subscription model starts as before.
    let saved_subscription = (pending_agent.is_none() && !explicit_model)
        .then(|| {
            runtime
                .settings
                .selected_model()
                .filter(|(p, _)| runtime.is_subscription(p))
                .map(|(p, m)| loop_cli::PendingAgent::New {
                    id: p.to_owned(),
                    model: Some(m.to_owned()),
                })
        })
        .flatten();
    runtime.pending_agent = pending_agent.or(saved_subscription);
    runtime.debug = debug;
    runtime.debug_log_path = debug_log_path;
    #[cfg(feature = "telemetry")]
    if let Err(err) = runtime.attach_telemetry(telemetry.clone()) {
        tracing::warn!(error = %err, "Langfuse tracing disabled");
    }

    let result = if cli.serve_mcp {
        loop_cli::mcp_serve::run_mcp_server(
            runtime.inner,
            cli.mcp_host,
            cli.mcp_port,
            cli.mcp_token,
        )
        .await
    } else if let Some(prompt) = cli.print {
        let result = print_mode::run_print(&mut runtime, prompt, &cli.trace).await;
        runtime.agents.shutdown().await;
        result
    } else {
        loop_cli::app::run(runtime).await
    };
    #[cfg(feature = "telemetry")]
    print_mode::shutdown_telemetry(&telemetry).await;
    result
}
