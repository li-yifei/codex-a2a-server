use super::{
    Backend, BackendCancelRequest, BackendExecutionResult, BackendJob, BackendRequest,
    BackendSessionSummary, wrap_prompt,
};
use crate::config::Config;
use crate::process::{configure_child_process, terminate_process_tree};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);
const NATIVE_INIT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct NativeBackend {
    config: Arc<Config>,
    controls: Arc<Mutex<HashMap<u32, NativeControl>>>,
}

#[derive(Clone, Copy)]
struct NativeWire {
    include_jsonrpc: bool,
}

#[derive(Clone)]
struct NativeControl {
    stdin: Arc<Mutex<ChildStdin>>,
    wire: NativeWire,
    thread_id: Arc<Mutex<Option<String>>>,
    turn_id: Arc<Mutex<Option<String>>>,
}

struct NativeTurn {
    child: Child,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
    wire: NativeWire,
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
        let attempt = spawn_native_child(&self.config)?;
        let pid = attempt.child.id();
        let stdin = Arc::new(Mutex::new(attempt.stdin));
        let thread_id = Arc::new(Mutex::new(None));
        let turn_id = Arc::new(Mutex::new(None));
        self.controls.lock().unwrap().insert(
            pid,
            NativeControl {
                stdin: Arc::clone(&stdin),
                wire: attempt.wire,
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
                    child: attempt.child,
                    stdout: attempt.stdout,
                    stderr: attempt.stderr,
                    wire: attempt.wire,
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
                control.wire,
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
        mut stderr,
        wire,
        stdin,
        shared_thread_id,
        shared_turn_id,
        config,
        request,
        controls,
        pid,
    } = turn;
    let mut reader = BufReader::new(stdout);
    eprintln!("native worker pid={pid} started");

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

    let thread_response = match call(&mut reader, &stdin, wire, thread_method, thread_params) {
        Ok(response) => response,
        Err(err) => {
            let stderr_text = finish_child(child, stderr.take(), stdin, controls, pid, true);
            return failed(with_stderr(err, &stderr_text));
        }
    };
    let Some(thread_id) = thread_response
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let stderr_text = finish_child(child, stderr.take(), stdin, controls, pid, true);
        let message = format!("native {thread_method} response did not include thread.id");
        return failed(with_stderr(message, &stderr_text));
    };
    *shared_thread_id.lock().unwrap() = Some(thread_id.clone());

    let turn_response = match call(
        &mut reader,
        &stdin,
        wire,
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
            let stderr_text = finish_child(child, stderr.take(), stdin, controls, pid, true);
            return failed(with_stderr(err, &stderr_text));
        }
    };
    let Some(turn_id) = turn_response
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let stderr_text = finish_child(child, stderr.take(), stdin, controls, pid, true);
        return failed(with_stderr(
            "native turn/start response did not include turn.id".to_string(),
            &stderr_text,
        ));
    };
    *shared_turn_id.lock().unwrap() = Some(turn_id.clone());

    let mut last_message = String::new();
    let mut success = false;
    let mut failure_message = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                failure_message = "native child closed stdout before turn/completed".to_string();
                break;
            }
            Ok(_) => {}
            Err(err) => {
                failure_message = format!("native child stdout read failed: {err}");
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

    let stderr_text = finish_child(child, stderr, stdin, controls, pid, !success);
    if !success {
        failure_message = with_stderr(failure_message, &stderr_text);
        eprintln!("native worker pid={pid} failed: {failure_message}");
    }
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
    mut stderr: Option<ChildStderr>,
    stdin: Arc<Mutex<ChildStdin>>,
    controls: Arc<Mutex<HashMap<u32, NativeControl>>>,
    pid: u32,
    force_kill: bool,
) -> String {
    controls.lock().unwrap().remove(&pid);
    drop(stdin);
    if !force_kill {
        for _ in 0..20 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return read_stderr_text(&mut stderr);
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = terminate_process_tree(pid);
    let _ = child.wait();
    if let Some(mut err) = stderr.take() {
        let mut buf = String::new();
        let _ = err.read_to_string(&mut buf);
        return buf.trim().to_string();
    }
    String::new()
}

fn initialize(
    reader: &mut BufReader<ChildStdout>,
    stdin: &Arc<Mutex<ChildStdin>>,
    wire: NativeWire,
) -> Result<(), String> {
    call(
        reader,
        stdin,
        wire,
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
    write_notification(stdin, wire, "initialized", Value::Null)
}

fn call(
    reader: &mut BufReader<ChildStdout>,
    stdin: &Arc<Mutex<ChildStdin>>,
    wire: NativeWire,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let id = write_request(stdin, wire, method, params)?;
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
    wire: NativeWire,
    method: &str,
    params: Value,
) -> Result<String, String> {
    let id = next_request_id(method);
    let mut request = json!({
        "id": id,
        "method": method,
        "params": params
    });
    if wire.include_jsonrpc {
        request
            .as_object_mut()
            .expect("request object")
            .insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    }
    write_json_line(stdin, &request)?;
    Ok(id)
}

fn write_notification(
    stdin: &Arc<Mutex<ChildStdin>>,
    wire: NativeWire,
    method: &str,
    params: Value,
) -> Result<(), String> {
    let mut request = if params.is_null() {
        json!({"method": method})
    } else {
        json!({"method": method, "params": params})
    };
    if wire.include_jsonrpc {
        request
            .as_object_mut()
            .expect("request object")
            .insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    }
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
    initialize(
        &mut reader,
        &stdin,
        NativeWire {
            include_jsonrpc: false,
        },
    )?;
    let response = call(
        &mut reader,
        &stdin,
        NativeWire {
            include_jsonrpc: false,
        },
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

struct SpawnedNativeChild {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: Option<ChildStderr>,
    wire: NativeWire,
}

fn spawn_native_child(config: &Config) -> Result<SpawnedNativeChild, String> {
    let attempts = [
        (
            vec!["remote-control".to_string()],
            NativeWire {
                include_jsonrpc: false,
            },
        ),
        (
            vec![
                "app-server".to_string(),
                "--listen".to_string(),
                "stdio://".to_string(),
            ],
            NativeWire {
                include_jsonrpc: false,
            },
        ),
        (
            vec![
                "app-server".to_string(),
                "--listen".to_string(),
                "stdio://".to_string(),
            ],
            NativeWire {
                include_jsonrpc: true,
            },
        ),
    ];
    let mut errors = Vec::new();
    for (args, wire) in attempts {
        let args_label = args.join(" ");
        let mut command = Command::new(&config.codex_bin);
        for arg in &args {
            command.arg(arg);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_child_process(&mut command);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                errors.push(format!("{args_label}: {err}"));
                continue;
            }
        };
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = initialize_spawned_child(child, wire);
            let _ = tx.send(result);
        });
        match rx.recv_timeout(NATIVE_INIT_TIMEOUT) {
            Ok(Ok(spawned)) => return Ok(spawned),
            Ok(Err(err)) => errors.push(format!("{args_label}: {err}")),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = terminate_process_tree(pid);
                errors.push(format!(
                    "{args_label}: timed out waiting for native initialize response"
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                errors.push(format!("{args_label}: initialize worker disconnected"));
            }
        }
    }
    Err(format!(
        "failed to start native backend: {}",
        errors.join(" | ")
    ))
}

fn initialize_spawned_child(
    mut child: Child,
    wire: NativeWire,
) -> Result<SpawnedNativeChild, String> {
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open native app-server stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to open native app-server stdout".to_string())?;
    let mut stderr = child.stderr.take();
    let stdin_arc = Arc::new(Mutex::new(stdin));
    let mut reader = BufReader::new(stdout);
    match initialize(&mut reader, &stdin_arc, wire) {
        Ok(()) => {
            let stdin = Arc::into_inner(stdin_arc)
                .ok_or_else(|| "native stdin still shared unexpectedly".to_string())?
                .into_inner()
                .map_err(|_| "native stdin mutex poisoned".to_string())?;
            let stdout = reader.into_inner();
            Ok(SpawnedNativeChild {
                child,
                stdin,
                stdout,
                stderr,
                wire,
            })
        }
        Err(err) => {
            let stderr_text = read_stderr_text(&mut stderr);
            let _ = terminate_process_tree(child.id());
            let _ = child.wait();
            Err(format!("{err}{}", format_stderr_suffix(&stderr_text)))
        }
    }
}

fn read_stderr_text(stderr: &mut Option<ChildStderr>) -> String {
    let Some(stderr) = stderr.as_mut() else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = stderr.read_to_string(&mut buf);
    buf.trim().to_string()
}

fn format_stderr_suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(" | stderr: {stderr}")
    }
}

fn with_stderr(message: String, stderr: &str) -> String {
    if stderr.trim().is_empty() {
        message
    } else {
        format!("{message} | stderr: {}", stderr.trim())
    }
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
