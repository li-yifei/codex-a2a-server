use super::{
    Backend, BackendCancelRequest, BackendExecutionResult, BackendJob, BackendRequest,
    BackendSessionSummary, wrap_prompt,
};
use crate::config::Config;
use crate::process::{configure_child_process, terminate_process_tree};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

pub struct NativeBackend {
    config: Arc<Config>,
    controls: Arc<Mutex<HashMap<u32, NativeControl>>>,
}

#[derive(Clone)]
struct NativeControl {
    stdin: Arc<Mutex<ChildStdin>>,
    thread_id: Arc<Mutex<Option<String>>>,
    turn_id: Arc<Mutex<Option<String>>>,
}

struct NativeTurn {
    child: Child,
    stdout: std::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    shared_thread_id: Arc<Mutex<Option<String>>>,
    shared_turn_id: Arc<Mutex<Option<String>>>,
    config: Arc<Config>,
    request: BackendRequest,
    controls: Arc<Mutex<HashMap<u32, NativeControl>>>,
    pid: u32,
}

impl NativeBackend {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            controls: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Backend for NativeBackend {
    fn kind(&self) -> &'static str {
        "codex-native"
    }

    fn spawn(&self, request: BackendRequest) -> Result<BackendJob, String> {
        let mut command = Command::new(&self.config.codex_bin);
        command
            .arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_child_process(&mut command);
        let mut child = command.spawn().map_err(|err| err.to_string())?;

        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to open native app-server stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to open native app-server stdout".to_string())?;

        let stdin = Arc::new(Mutex::new(stdin));
        let thread_id = Arc::new(Mutex::new(None));
        let turn_id = Arc::new(Mutex::new(None));
        self.controls.lock().unwrap().insert(
            pid,
            NativeControl {
                stdin: Arc::clone(&stdin),
                thread_id: Arc::clone(&thread_id),
                turn_id: Arc::clone(&turn_id),
            },
        );

        let controls = Arc::clone(&self.controls);
        let config = Arc::clone(&self.config);
        Ok(BackendJob {
            pid,
            wait: Box::new(move || {
                run_native_turn(NativeTurn {
                    child,
                    stdout,
                    stdin: Arc::clone(&stdin),
                    shared_thread_id: thread_id,
                    shared_turn_id: turn_id,
                    config,
                    request,
                    controls,
                    pid,
                })
            }),
        })
    }

    fn cancel(&self, request: &BackendCancelRequest) -> Result<(), String> {
        let control = self.controls.lock().unwrap().get(&request.pid).cloned();
        let Some(control) = control else {
            return kill_pid(request.pid);
        };

        let thread_id = control.thread_id.lock().unwrap().clone();
        let turn_id = control.turn_id.lock().unwrap().clone();
        match (thread_id, turn_id) {
            (Some(thread_id), Some(turn_id)) => write_request(
                &control.stdin,
                "turn/interrupt",
                json!({"threadId": thread_id, "turnId": turn_id}),
            )
            .map(|_| ()),
            _ => kill_pid(request.pid),
        }
    }

    fn list_sessions(&self, limit: usize) -> Vec<BackendSessionSummary> {
        native_list_sessions(&self.config, limit).unwrap_or_default()
    }
}

fn run_native_turn(turn: NativeTurn) -> BackendExecutionResult {
    let NativeTurn {
        child,
        stdout,
        stdin,
        shared_thread_id,
        shared_turn_id,
        config,
        request,
        controls,
        pid,
    } = turn;
    let mut reader = BufReader::new(stdout);
    if let Err(err) = initialize(&mut reader, &stdin) {
        finish_child(child, stdin, controls, pid, true);
        return failed(err);
    }

    let thread_method;
    let mut thread_params;
    if let Some(resume_session_id) = request.resume_session_id.clone() {
        thread_method = "thread/resume";
        thread_params = json!({
            "threadId": resume_session_id,
            "cwd": request.options.working_directory,
            "approvalPolicy": "never",
            "sandbox": request.options.sandbox_mode,
            "excludeTurns": true,
            "persistExtendedHistory": true
        });
    } else {
        thread_method = "thread/start";
        thread_params = json!({
            "cwd": request.options.working_directory,
            "approvalPolicy": "never",
            "sandbox": request.options.sandbox_mode,
            "experimentalRawEvents": false,
            "persistExtendedHistory": true
        });
    }
    if let Some(config_overrides) = native_config_overrides(
        request.options.write_enabled,
        &config.write_mode_sandbox_permissions,
    ) && let Some(params) = thread_params.as_object_mut()
    {
        params.insert("config".to_string(), config_overrides);
    }

    let thread_response = match call(&mut reader, &stdin, thread_method, thread_params) {
        Ok(response) => response,
        Err(err) => {
            finish_child(child, stdin, controls, pid, true);
            return failed(err);
        }
    };
    let Some(thread_id) = thread_response
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        finish_child(child, stdin, controls, pid, true);
        return failed(format!(
            "native {thread_method} response did not include thread.id"
        ));
    };
    *shared_thread_id.lock().unwrap() = Some(thread_id.clone());

    let turn_response = match call(
        &mut reader,
        &stdin,
        "turn/start",
        json!({
            "threadId": thread_id,
            "input": [{
                "type": "text",
                "text": wrap_prompt(&request.prompt, &request.options),
                "text_elements": []
            }],
            "cwd": request.options.working_directory,
            "approvalPolicy": "never"
        }),
    ) {
        Ok(response) => response,
        Err(err) => {
            finish_child(child, stdin, controls, pid, true);
            return failed(err);
        }
    };
    let Some(turn_id) = turn_response
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        finish_child(child, stdin, controls, pid, true);
        return failed("native turn/start response did not include turn.id".to_string());
    };
    *shared_turn_id.lock().unwrap() = Some(turn_id.clone());

    let mut last_message = String::new();
    let mut success = false;
    let mut failure_message = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) => {
                failure_message = err.to_string();
                break;
            }
        }

        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(error) = event.get("error") {
            failure_message = error.to_string();
            break;
        }
        if event.get("method").and_then(Value::as_str) == Some("item/completed")
            && let Some(text) = agent_message_text(event.get("params").unwrap_or(&Value::Null))
        {
            last_message = text;
        }
        if event.get("method").and_then(Value::as_str) == Some("turn/completed") {
            let params = event.get("params").unwrap_or(&Value::Null);
            let completed_turn = params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str);
            if completed_turn == Some(turn_id.as_str()) {
                let status = params
                    .get("turn")
                    .and_then(|turn| turn.get("status"))
                    .and_then(Value::as_str);
                success = status == Some("completed");
                if !success {
                    failure_message = params
                        .get("turn")
                        .and_then(|turn| turn.get("error"))
                        .map(Value::to_string)
                        .unwrap_or_else(|| format!("native turn ended with status {status:?}"));
                }
                break;
            }
        }
    }

    finish_child(child, stdin, controls, pid, !success);
    BackendExecutionResult {
        success,
        message: if success {
            last_message
        } else {
            failure_message
        },
        resume_session_id: Some(thread_id),
    }
}

fn finish_child(
    mut child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    controls: Arc<Mutex<HashMap<u32, NativeControl>>>,
    pid: u32,
    force_kill: bool,
) {
    controls.lock().unwrap().remove(&pid);
    drop(stdin);
    if !force_kill {
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = terminate_process_tree(pid);
    let _ = child.wait();
}

fn initialize(
    reader: &mut BufReader<std::process::ChildStdout>,
    stdin: &Arc<Mutex<ChildStdin>>,
) -> Result<(), String> {
    call(
        reader,
        stdin,
        "initialize",
        json!({
            "clientInfo": {
                "name": "codex-a2a-rs",
                "title": "Codex A2A RS",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "experimentalApi": true,
                "optOutNotificationMethods": [
                    "command/exec/outputDelta",
                    "item/agentMessage/delta",
                    "item/plan/delta",
                    "item/fileChange/outputDelta",
                    "item/reasoning/summaryTextDelta",
                    "item/reasoning/textDelta"
                ]
            }
        }),
    )?;
    write_notification(stdin, "initialized", Value::Null)
}

fn call(
    reader: &mut BufReader<std::process::ChildStdout>,
    stdin: &Arc<Mutex<ChildStdin>>,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let id = write_request(stdin, method, params)?;
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line).map_err(|err| err.to_string())?;
        if bytes == 0 {
            return Err(format!("native app-server closed before {method} response"));
        }
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if event.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            continue;
        }
        if let Some(error) = event.get("error") {
            return Err(format!("native {method} failed: {error}"));
        }
        return Ok(event.get("result").cloned().unwrap_or(Value::Null));
    }
}

fn write_request(
    stdin: &Arc<Mutex<ChildStdin>>,
    method: &str,
    params: Value,
) -> Result<String, String> {
    let id = next_request_id(method);
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    });
    write_json_line(stdin, &request)?;
    Ok(id)
}

fn write_notification(
    stdin: &Arc<Mutex<ChildStdin>>,
    method: &str,
    params: Value,
) -> Result<(), String> {
    let request = if params.is_null() {
        json!({"jsonrpc": "2.0", "method": method})
    } else {
        json!({"jsonrpc": "2.0", "method": method, "params": params})
    };
    write_json_line(stdin, &request)
}

fn write_json_line(stdin: &Arc<Mutex<ChildStdin>>, request: &Value) -> Result<(), String> {
    let mut stdin = stdin.lock().unwrap();
    serde_json::to_writer(&mut *stdin, request).map_err(|err| err.to_string())?;
    stdin.write_all(b"\n").map_err(|err| err.to_string())?;
    stdin.flush().map_err(|err| err.to_string())
}

fn next_request_id(method: &str) -> String {
    let seq = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("native-{}-{seq}", method.replace('/', "-"))
}

fn native_config_overrides(write_enabled: bool, permissions: &[String]) -> Option<Value> {
    if !write_enabled || permissions.is_empty() {
        return None;
    }
    Some(json!({"sandbox_permissions": permissions}))
}

fn agent_message_text(params: &Value) -> Option<String> {
    let item = params.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
        return None;
    }
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    item.get("content")
        .and_then(Value::as_array)
        .and_then(|parts| {
            parts
                .iter()
                .find_map(|part| part.get("text").and_then(Value::as_str))
        })
        .map(str::to_string)
}

fn native_list_sessions(
    config: &Config,
    limit: usize,
) -> Result<Vec<BackendSessionSummary>, String> {
    let mut child = Command::new(&config.codex_bin)
        .arg("app-server")
        .arg("--listen")
        .arg("stdio://")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| err.to_string())?;
    let stdin =
        Arc::new(Mutex::new(child.stdin.take().ok_or_else(|| {
            "failed to open native app-server stdin".to_string()
        })?));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open native app-server stdout".to_string())?;
    let mut reader = BufReader::new(stdout);
    initialize(&mut reader, &stdin)?;
    let response = call(
        &mut reader,
        &stdin,
        "thread/list",
        json!({
            "limit": limit,
            "sortKey": "updated_at",
            "sortDirection": "desc",
            "sourceKinds": ["appServer", "exec", "cli"],
            "archived": false,
            "useStateDbOnly": false
        }),
    )?;
    let sessions = response
        .get("data")
        .and_then(Value::as_array)
        .map(|threads| {
            threads
                .iter()
                .filter_map(thread_summary)
                .take(limit)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    Ok(sessions)
}

fn thread_summary(thread: &Value) -> Option<BackendSessionSummary> {
    Some(BackendSessionSummary {
        id: thread.get("id")?.as_str()?.to_string(),
        path: thread
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        modified_unix: thread.get("updatedAt").and_then(Value::as_u64).unwrap_or(0),
        cwd: thread.get("cwd").cloned().unwrap_or(Value::Null),
        last_message_preview: thread
            .get("preview")
            .and_then(Value::as_str)
            .map(|preview| Value::String(preview.chars().take(240).collect()))
            .unwrap_or(Value::Null),
    })
}

fn failed(message: String) -> BackendExecutionResult {
    BackendExecutionResult {
        success: false,
        message,
        resume_session_id: None,
    }
}

fn kill_pid(pid: u32) -> Result<(), String> {
    terminate_process_tree(pid)
}
