//! Check process stderr as well as debug files: logs must not overwrite the TUI.
use std::process::Command;

#[test]
fn interactive_warnings_use_the_ui_not_stderr() {
    const CHILD_MODE: &str = "LOOP_TEST_WARNING_MODE";
    const MESSAGE: &str = "reasoning-fallback-diagnostic";
    if let Ok(mode) = std::env::var(CHILD_MODE) {
        let interactive = mode.ends_with("tui");
        let debug = mode.starts_with("debug");
        loop_cli::debug_log::init_tracing(
            debug,
            &std::env::current_dir().unwrap(),
            interactive,
            None,
        )
        .unwrap();
        tracing::warn!(model = "test-model", "{MESSAGE}");
        return;
    }
    for mode in ["tui", "print", "debug-tui", "debug-print"] {
        let dir = tempfile::tempdir().unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "interactive_warnings_use_the_ui_not_stderr",
                "--nocapture",
            ])
            .env(CHILD_MODE, mode)
            .env("RUST_LOG", "warn")
            .current_dir(dir.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{mode}: {stderr}");
        assert_eq!(
            stderr.contains(MESSAGE),
            !mode.ends_with("tui"),
            "{mode}: {stderr}"
        );
        let log_dir = dir.path().join("target/debug/logs");
        if mode.starts_with("debug") {
            let file = std::fs::read_dir(log_dir).unwrap().next().unwrap().unwrap();
            let log = std::fs::read_to_string(file.path()).unwrap();
            assert!(log.contains(MESSAGE), "{mode}");
            assert!(log.contains("test-model"), "{mode}");
        } else {
            assert!(!log_dir.exists());
        }
    }
}
