//! Harness: sessions, tools, compaction, sandbox, and orchestration.

pub mod agent_harness;
pub mod compaction;
pub mod env;
pub mod hooks;
pub mod prompt_templates;
pub mod sandbox;
pub mod session;
pub mod skills;
pub mod system_prompt;
pub mod tools;
pub mod types;
pub mod utils;

#[cfg(feature = "orchestration")]
pub mod orchestration;

#[cfg(feature = "mcp")]
pub mod mcp;

pub use prompt_templates::{format_prompt_template_invocation, load_prompt_templates};
pub use skills::{format_skill_invocation, load_skills};
pub use system_prompt::format_skills_for_system_prompt;

pub use agent_harness::{AgentHarness, AgentHarnessOptions, TurnSnapshot};
pub use compaction::{
    default_compaction_settings, find_cut_point, prepare_compaction, should_compact,
    CompactionSettings,
};
pub use env::HostExecutionEnv;
pub use hooks::{HarnessHookEvent, HookOutcome, HookRegistry};
pub use sandbox::{
    check_krun_deps, check_local_sandbox_deps, KrunIsolation, KrunSandbox, KrunSandboxFactory,
    LocalSandboxRuntime, PodmanClient, PodmanExecOpts, PodmanRunOpts, Sandbox, SandboxConfig,
    SandboxError, SandboxFactory, SandboxInfo, SandboxMode, SandboxRegistry, SandboxStatus,
    KRUN_DEFAULT_IMAGE, KRUN_DEFAULT_RUNTIME, LOCAL_DEFAULT_RUNTIME,
};
pub use session::{
    compute_session_stats, create_in_memory_session_store, create_jsonl_session_store,
    create_scanning_session_search, create_session_repository, fork_points_from_branch,
    format_session_stats, Session, SessionContext, SessionForkPoint, SessionForkSelection,
    SessionRepository, SessionStats, SessionStore, SessionTreeEntry,
};
#[cfg(feature = "web")]
pub use tools::web::{
    create_web_fetch_tool, create_web_search_tool, verify_web_search_key, web_search_backend,
    SoketWebApi, WebFetchConfig, WebFetchProvider, WebSearchBackendInfo, WebSearchConfig,
    DEFAULT_WEB_SEARCH_PROVIDER, WEB_SEARCH_BACKENDS,
};
pub use tools::{create_bash_tool, create_edit_tool, create_read_tool, create_write_tool};
pub use types::{
    AgentHarnessError, AgentHarnessPhase, AgentHarnessResources, CompactResult, ExecutionEnv,
    ExecutionToolContext, FileSystem, NavigateTreeResult, PromptTemplate, Shell, Skill,
};

#[cfg(feature = "sqlite")]
pub use session::{create_sqlite_session_search, create_sqlite_session_store};
