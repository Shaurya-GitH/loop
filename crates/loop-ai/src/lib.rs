//! Unified LLM API with provider collections, auth resolution, token/cost tracking,
//! and mid-session model hand-off.
//!
//! # Quick start
//!
//! ```no_run
//! use loop_ai::{
//!     providers::{custom_provider, CustomModelSpec, CustomProviderConfig},
//!     Context, Message, Models, SimpleStreamOptions,
//! };
//! use futures::StreamExt;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let models = Models::new();
//! models.set_provider(custom_provider(CustomProviderConfig {
//!     id: "ollama".into(),
//!     name: Some("Ollama".into()),
//!     base_url: "http://localhost:11434/v1".into(),
//!     api_key_env: vec![],
//!     models: vec![CustomModelSpec::new("llama3.2")],
//!     headers: None,
//! }));
//!
//! let model = models.get_model("ollama", "llama3.2").unwrap();
//! let context = Context {
//!     system_prompt: Some("You are helpful.".into()),
//!     messages: vec![Message::user_text("Hello")],
//!     tools: None,
//! };
//!
//! let stream = models.stream_simple(&model, &context, SimpleStreamOptions::default());
//! let mut stream = stream;
//! while let Some(event) = stream.next().await {
//!     let _ = event;
//! }
//! let assistant = stream.result().await;
//! # let _ = assistant;
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]

pub mod api;
/// Whether a model's server rejected reasoning parameters in this process.
pub use api::openai_completions::reasoning_rejected;
pub mod auth;
pub mod models;
pub mod models_store;
pub mod providers;
pub mod stream;
pub mod types;
pub mod utils;

pub use api::{
    detect_compat, http_client, list_openai_models, map_remote_model, resolve_compat,
    streaming_http_client, transform_messages, verify_api_key, ListModelsError,
    MapRemoteModelOptions, OpenAICompletionsAdapter,
};
pub use auth::{
    env_api_key_auth, AuthCheck, AuthResult, Credential, CredentialStore, InMemoryCredentialStore,
    ModelAuth, ModelsError, ModelsErrorCode, ProviderAuth,
};
pub use models::{
    calculate_cost, clamp_thinking_level, create_provider, models_are_equal, ApiAdapter,
    CreateModelsOptions, CreateProviderApi, CreateProviderOptions, FetchModelsFn, Models,
    ModelsRefreshOptions, ModelsRefreshResult, Provider, RefreshModelsContext, SharedApiAdapter,
};
pub use models_store::{
    FileModelsStore, InMemoryModelsStore, ModelsStore, ModelsStoreEntry, ModelsStoreError,
    SharedModelsStore,
};
pub use stream::{
    create_assistant_message_event_stream, AssistantMessageEventStream,
    AssistantMessageEventStreamHandle, EventStream, EventStreamHandle,
};
pub use types::*;
pub use utils::{
    calculate_context_tokens, estimate_context_tokens, estimate_message_tokens,
    is_context_overflow, new_id, now_ms, parse_streaming_json, validate_tool_arguments,
    validate_tool_call, ToolValidationError,
};
