//! Debug session logging to `target/debug/logs`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer, Registry};

/// A layer installed next to the log layer, e.g. the trace exporter.
pub type ExportLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Whether debug mode is enabled via `--debug` or `LOOP_DEBUG=1`.
pub fn debug_enabled(cli_flag: bool) -> bool {
    if cli_flag {
        return true;
    }
    matches!(
        std::env::var("LOOP_DEBUG").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE") | Ok("yes") | Ok("YES")
    )
}

/// Initialize tracing. In debug mode, writes session logs under `cwd/target/debug/logs`.
///
/// Interactive TUI mode never writes logs to stderr; `--debug` retains them in a file.
/// Non-interactive mode keeps stderr logging. `export_layer` (the telemetry exporter, when built with
/// the `telemetry` feature) sees observation spans independently of the log filter.
pub fn init_tracing(
    debug: bool,
    cwd: &Path,
    interactive: bool,
    export_layer: Option<ExportLayer>,
) -> anyhow::Result<Option<PathBuf>> {
    let default_filter = if debug {
        "info,loop_agent=debug,loop_ai=debug,loop_cli=debug,loop_app_core=debug,loop_mcp=debug"
    } else {
        "warn"
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    let (log_layer, path) = if debug {
        let (writer, path) = debug_log_writer(cwd, interactive)?;
        let layer = fmt::layer()
            .with_writer(Mutex::new(writer))
            .with_ansi(false)
            .with_target(true)
            .with_thread_ids(true)
            .boxed();
        (layer, Some(path))
    } else {
        let writer: Box<dyn Write + Send> = if interactive {
            Box::new(io::sink())
        } else {
            Box::new(io::stderr())
        };
        let layer = fmt::layer()
            .with_writer(Mutex::new(writer))
            .with_target(true)
            .boxed();
        (layer, None)
    };

    tracing_subscriber::registry()
        .with(export_layer)
        .with(log_layer.with_filter(filter))
        .init();

    if let Some(path) = &path {
        tracing::info!(
            path = %path.display(),
            interactive,
            "debug logging enabled"
        );
    }
    Ok(path)
}

/// Log file under `cwd/target/debug/logs`, teed to stderr when not interactive.
fn debug_log_writer(
    cwd: &Path,
    interactive: bool,
) -> anyhow::Result<(Box<dyn Write + Send>, PathBuf)> {
    let log_dir = cwd.join("target").join("debug").join("logs");
    fs::create_dir_all(&log_dir)?;
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = log_dir.join(format!("loop-{ts}.log"));
    let file = OpenOptions::new().create(true).append(true).open(&path)?;

    let writer: Box<dyn Write + Send> = if interactive {
        Box::new(file)
    } else {
        Box::new(TeeWriter {
            file: Mutex::new(file),
        })
    };
    Ok((writer, path))
}

/// Append a raw session note to an existing debug log file (best-effort).
pub fn append_note(path: &Path, note: &str) {
    let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let _ = writeln!(f, "{ts}  {note}");
}

struct TeeWriter {
    file: Mutex<File>,
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = io::stderr().write_all(buf);
        self.file
            .lock()
            .map_err(|_| io::Error::other("debug log lock poisoned"))?
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        self.file
            .lock()
            .map_err(|_| io::Error::other("debug log lock poisoned"))?
            .flush()
    }
}
