use crate::backend::SharedBackend;
use crate::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    pub backend: SharedBackend,
    pub config: Arc<Config>,
    pub tasks: Arc<Mutex<HashMap<String, Value>>>,
    pub sessions: Arc<Mutex<HashMap<String, ContextRecord>>>,
    pub running: Arc<Mutex<HashMap<String, RunningTask>>>,
    pub write_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ContextRecord {
    pub codex_session_id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub backend_kind: String,
    pub resume_source: String,
}

#[derive(Clone)]
pub struct RunningTask {
    pub pid: u32,
    pub context_id: String,
    pub started_at: u64,
}

#[derive(Clone)]
pub struct TaskOptions {
    pub sandbox_mode: String,
    pub working_directory: String,
    pub write_enabled: bool,
}
