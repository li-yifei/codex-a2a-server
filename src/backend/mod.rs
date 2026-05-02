pub mod cli;
pub mod native;

use crate::config::{BackendKind, Config};
use crate::state::TaskOptions;
use serde_json::Value;
use std::sync::Arc;

pub type SharedBackend = std::sync::Arc<dyn Backend>;

pub struct BackendRequest {
    pub prompt: String,
    pub options: TaskOptions,
    pub resume_session_id: Option<String>,
}

pub struct BackendJob {
    pub pid: u32,
    pub wait: Box<dyn FnOnce() -> BackendExecutionResult + Send>,
}

pub struct BackendExecutionResult {
    pub success: bool,
    pub message: String,
    pub resume_session_id: Option<String>,
}

pub struct BackendCancelRequest {
    pub pid: u32,
}

pub struct BackendSessionSummary {
    pub id: String,
    pub path: String,
    pub modified_unix: u64,
    pub cwd: Value,
    pub last_message_preview: Value,
}

pub trait Backend: Send + Sync {
    fn kind(&self) -> &'static str;
    fn spawn(&self, request: BackendRequest) -> Result<BackendJob, String>;
    fn cancel(&self, request: &BackendCancelRequest) -> Result<(), String>;
    fn list_sessions(&self, limit: usize) -> Vec<BackendSessionSummary>;
}

pub fn from_config(config: Arc<Config>) -> SharedBackend {
    match config.backend {
        BackendKind::Cli => Arc::new(cli::CliBackend::new(config)),
        BackendKind::Native => Arc::new(native::NativeBackend::new(config)),
    }
}

pub fn wrap_prompt(text: &str, options: &TaskOptions) -> String {
    if options.write_enabled {
        format!(
            "You are Codex answering an A2A request from Hermes. Be direct and concise. \
You may read source material available through the Codex sandbox, including configured read-only disk access. \
You are allowed to write files only inside the configured workspace through the Codex sandbox. \
Keep changes minimal, preserve git traceability, and report changed files.\n\nRequest:\n{text}"
        )
    } else {
        format!(
            "You are Codex answering an A2A request from Hermes. Be direct and concise. \
You are running in read-only mode for this service. Preserve context across this session.\n\nRequest:\n{text}"
        )
    }
}
