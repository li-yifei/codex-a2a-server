use crate::backend::{self, BackendCancelRequest, BackendRequest, BackendSessionSummary};
use crate::config::{CODEX_SESSIONS_EXTENSION_URI, Config, load_config};
use crate::state::{AppState, ContextRecord, RunningTask, TaskOptions};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static TASK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
    peer_ip: IpAddr,
}

pub fn run() {
    let config = Arc::new(load_config().expect("load codex a2a config"));
    let bind = format!("{}:{}", config.host, config.port);
    let listener = TcpListener::bind(&bind).expect("bind codex a2a server");
    let backend = backend::from_config(config.clone());
    let state = AppState {
        backend,
        config: config.clone(),
        tasks: Arc::new(Mutex::new(load_task_registry(&config))),
        sessions: Arc::new(Mutex::new(load_context_registry(&config))),
        running: Arc::new(Mutex::new(HashMap::new())),
        write_locks: Arc::new(Mutex::new(HashMap::new())),
    };

    warn_if_git_missing();
    eprintln!(
        "codex-a2a-rs listening on {bind} with backend={} ({})",
        config.backend.as_str(),
        state.backend.kind()
    );
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = state.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_stream(stream, state) {
                        eprintln!("request failed: {err}");
                    }
                });
            }
            Err(err) => eprintln!("accept failed: {err}"),
        }
    }
}

fn handle_stream(mut stream: TcpStream, state: AppState) -> Result<(), String> {
    let peer_ip = stream.peer_addr().map_err(|e| e.to_string())?.ip();
    stream
        .set_read_timeout(Some(Duration::from_secs(state.config.read_timeout_secs)))
        .map_err(|e| e.to_string())?;
    let max_body_bytes = state.config.max_body_bytes;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before headers".to_string());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_header_end(&buf) {
            header_end = pos;
            break;
        }
        if buf.len() > max_body_bytes {
            return Err("request headers too large".to_string());
        }
    }

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or("missing request line")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts
        .next()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > max_body_bytes {
        let payload = json!({"error": "request body too large"});
        write_json(&mut stream, 413, &payload).map_err(|e| e.to_string())?;
        return Ok(());
    }
    let body_start = header_end + 4;
    let mut body = buf[body_start..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    let req = Request {
        method,
        path,
        headers,
        body,
        peer_ip,
    };
    if handle_streaming_jsonrpc(&mut stream, &req, state.clone())? {
        return Ok(());
    }
    let (status, payload) = route(req, state);
    write_json(&mut stream, status, &payload).map_err(|e| e.to_string())
}

fn route(req: Request, state: AppState) -> (u16, Value) {
    if !allowed_source(req.peer_ip, &state.config) {
        return (403, json!({"error": "forbidden source"}));
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => (
            200,
            json!({"status": "ok", "agent": "codex-a2a-rs", "version": env!("CARGO_PKG_VERSION")}),
        ),
        ("GET", "/.well-known/agent.json") | ("GET", "/.well-known/agent-card.json") => {
            (200, agent_card(&state.config.public_url))
        }
        ("POST", "/") | ("POST", "/a2a/jsonrpc") => {
            if !authorized(&req, &state.config) {
                return (
                    401,
                    json!({"jsonrpc": "2.0", "error": {"code": -32000, "message": "Unauthorized"}, "id": null}),
                );
            }
            handle_jsonrpc(req, state)
        }
        _ => (404, json!({"error": "not found"})),
    }
}

fn handle_jsonrpc(req: Request, state: AppState) -> (u16, Value) {
    let parsed: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => {
            return (
                400,
                json!({"jsonrpc": "2.0", "error": {"code": -32700, "message": "Parse error"}, "id": null}),
            );
        }
    };
    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
    let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));

    match method {
        "message/send" => send_task(id, params, state, false),
        "message/stream" => (
            200,
            json!({"jsonrpc": "2.0", "error": {"code": -32003, "message": "message/stream requires the streaming HTTP response path"}, "id": id}),
        ),
        "tasks/send" => send_task(id, params, state, true),
        "tasks/get" => get_task(id, params, state),
        "tasks/list" => list_tasks(id, params, state),
        "sessions/list" => list_sessions(id, params, state),
        "sessions/resume" => resume_session(id, params, state),
        "tasks/cancel" => cancel_task(id, params, state),
        _ => (
            200,
            json!({"jsonrpc": "2.0", "error": {"code": -32601, "message": format!("Method not found: {method}")}, "id": id}),
        ),
    }
}

fn handle_streaming_jsonrpc(
    stream: &mut TcpStream,
    req: &Request,
    state: AppState,
) -> Result<bool, String> {
    if req.method != "POST" || (req.path != "/" && req.path != "/a2a/jsonrpc") {
        return Ok(false);
    }
    if !allowed_source(req.peer_ip, &state.config) {
        return Ok(false);
    }
    let parsed: Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    if parsed.get("method").and_then(Value::as_str) != Some("message/stream") {
        return Ok(false);
    }
    if !authorized(req, &state.config) {
        let payload = json!({"jsonrpc": "2.0", "error": {"code": -32000, "message": "Unauthorized"}, "id": parsed.get("id").cloned().unwrap_or(Value::Null)});
        write_json(stream, 401, &payload).map_err(|err| err.to_string())?;
        return Ok(true);
    }

    let id = parsed.get("id").cloned().unwrap_or(Value::Null);
    let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));
    let (_, submitted_response) = send_task(id.clone(), params, state.clone(), false);
    if submitted_response.get("error").is_some() {
        write_json(stream, 200, &submitted_response).map_err(|err| err.to_string())?;
        return Ok(true);
    }
    let task = submitted_response
        .get("result")
        .cloned()
        .unwrap_or_else(|| json!({"status": {"state": "failed"}}));
    let task_id = task
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    write_chunked_headers(stream).map_err(|err| err.to_string())?;
    write_chunk(
        stream,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "kind": "task",
                "task": task
            }
        }),
    )
    .map_err(|err| err.to_string())?;

    let mut last_state = String::new();
    loop {
        thread::sleep(std::time::Duration::from_millis(500));
        let current = { state.tasks.lock().unwrap().get(&task_id).cloned() };
        let Some(task) = current else {
            break;
        };
        let state_name = task
            .get("status")
            .and_then(|status| status.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        if state_name != last_state {
            last_state = state_name.clone();
            write_chunk(
                stream,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "tasks/status-update",
                    "params": {
                        "taskId": task_id,
                        "contextId": task.get("contextId").cloned().unwrap_or(Value::Null),
                        "status": task.get("status").cloned().unwrap_or(Value::Null),
                        "final": is_terminal_state(&state_name)
                    }
                }),
            )
            .map_err(|err| err.to_string())?;
        }
        if is_terminal_state(&state_name) {
            write_chunk(
                stream,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "tasks/final",
                    "params": {"task": task}
                }),
            )
            .map_err(|err| err.to_string())?;
            break;
        }
    }
    write_final_chunk(stream).map_err(|err| err.to_string())?;
    Ok(true)
}

fn send_task(
    id: Value,
    params: Value,
    state: AppState,
    used_legacy_send_alias: bool,
) -> (u16, Value) {
    let task_id = params
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            params
                .get("message")
                .and_then(|m| m.get("messageId"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(new_id);

    let (context_id, used_legacy_context_alias) = match context_key_from_params(&params) {
        Ok(context) => context,
        Err(message) => return invalid_params(id, message),
    };

    if task_id_exists(&state, &task_id) {
        return invalid_params(id, format!("Duplicate task id: {task_id}"));
    }
    if let Some(resume_id) = params
        .get("metadata")
        .and_then(|m| m.get("resumeSessionId"))
        .or_else(|| params.get("resumeSessionId"))
        .and_then(Value::as_str)
    {
        upsert_context_record(
            &state,
            &context_id,
            resume_id,
            if used_legacy_context_alias {
                "deprecated-sessionId-alias"
            } else {
                "message-send"
            },
        );
    }

    let task_options = match task_options(&params, &state.config) {
        Ok(options) => options,
        Err(message) => {
            let mut task = task_result(&task_id, "failed", &message, &context_id);
            annotate_task(
                &mut task,
                &task_id,
                &context_id,
                used_legacy_send_alias,
                used_legacy_context_alias,
            );
            persist_task(&state, &task_id, task.clone());
            return (200, json!({"jsonrpc": "2.0", "result": task, "id": id}));
        }
    };

    let text = extract_text(params.get("message").unwrap_or(&Value::Null));
    if text.trim().is_empty() {
        let mut task = task_result(&task_id, "failed", "Empty message", &context_id);
        annotate_task(
            &mut task,
            &task_id,
            &context_id,
            used_legacy_send_alias,
            used_legacy_context_alias,
        );
        persist_task(&state, &task_id, task.clone());
        return (200, json!({"jsonrpc": "2.0", "result": task, "id": id}));
    }

    let mut submitted = json!({
        "id": task_id,
        "contextId": context_id,
        "status": {"state": "working"},
        "artifacts": [{"parts": [{"type": "text", "text": "(processing - poll with tasks/get)"}], "index": 0}]
    });
    annotate_task(
        &mut submitted,
        &task_id,
        &context_id,
        used_legacy_send_alias,
        used_legacy_context_alias,
    );
    if let Err(err) = persist_new_working_task(&state, &task_id, submitted.clone()) {
        return match err {
            SubmitTaskError::Duplicate => {
                invalid_params(id, format!("Duplicate task id: {task_id}"))
            }
            SubmitTaskError::Busy => server_busy(id),
        };
    }

    let worker_state = state.clone();
    let worker_task_id = task_id.clone();
    let worker_context_id = context_id.clone();
    thread::spawn(move || {
        run_codex(
            worker_state,
            worker_task_id,
            worker_context_id,
            text,
            task_options,
            used_legacy_send_alias,
            used_legacy_context_alias,
        )
    });

    (
        200,
        json!({"jsonrpc": "2.0", "result": submitted, "id": id}),
    )
}

fn get_task(id: Value, params: Value, state: AppState) -> (u16, Value) {
    let Some(task_id) = params.get("id").and_then(Value::as_str) else {
        return (
            200,
            json!({"jsonrpc": "2.0", "error": {"code": -32602, "message": "Missing task id"}, "id": id}),
        );
    };
    let tasks = state.tasks.lock().unwrap();
    match tasks.get(task_id) {
        Some(task) => (200, json!({"jsonrpc": "2.0", "result": task, "id": id})),
        None => (
            200,
            json!({"jsonrpc": "2.0", "error": {"code": -32001, "message": "Task not found"}, "id": id}),
        ),
    }
}

fn cancel_task(id: Value, params: Value, state: AppState) -> (u16, Value) {
    let Some(task_id) = params
        .get("id")
        .or_else(|| params.get("taskId"))
        .and_then(Value::as_str)
    else {
        return (
            200,
            json!({"jsonrpc": "2.0", "error": {"code": -32602, "message": "Missing task id"}, "id": id}),
        );
    };

    let running = state.running.lock().unwrap().remove(task_id);
    let Some(running) = running else {
        let task = state.tasks.lock().unwrap().get(task_id).cloned();
        return match task {
            Some(task) => (200, json!({"jsonrpc": "2.0", "result": task, "id": id})),
            None => (
                200,
                json!({"jsonrpc": "2.0", "error": {"code": -32001, "message": "Task not found or already finished"}, "id": id}),
            ),
        };
    };

    let _ = state
        .backend
        .cancel(&BackendCancelRequest { pid: running.pid });

    let mut task = task_result(
        task_id,
        "canceled",
        "Task canceled by tasks/cancel",
        &running.context_id,
    );
    annotate_task(&mut task, task_id, &running.context_id, false, false);
    if let Some(metadata) = task.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.insert("canceledPid".to_string(), json!(running.pid));
        metadata.insert("startedAt".to_string(), json!(running.started_at));
        metadata.insert("canceledAt".to_string(), json!(now_unix_secs()));
    }
    persist_task(&state, task_id, task.clone());
    (200, json!({"jsonrpc": "2.0", "result": task, "id": id}))
}

fn list_tasks(id: Value, params: Value, state: AppState) -> (u16, Value) {
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .min(200) as usize;
    let cursor = params.get("cursor").and_then(Value::as_u64).unwrap_or(0) as usize;
    let (context_filter, used_legacy_context_alias) = context_filter_from_params(&params);

    let tasks = state.tasks.lock().unwrap();
    let mut entries = tasks.values().cloned().collect::<Vec<_>>();
    if let Some(context_id) = context_filter.as_deref() {
        entries.retain(|task| task.get("contextId").and_then(Value::as_str) == Some(context_id));
    }
    entries.sort_by_key(|task| std::cmp::Reverse(task_updated_at(task)));

    let total = entries.len();
    let results = entries
        .into_iter()
        .skip(cursor)
        .take(limit)
        .collect::<Vec<_>>();
    let next_cursor = if cursor + results.len() < total {
        Some((cursor + results.len()) as u64)
    } else {
        None
    };

    (
        200,
        json!({
            "jsonrpc": "2.0",
            "result": {
                "tasks": results,
                "nextCursor": next_cursor,
                "metadata": {
                    "deprecatedSessionIdAliasUsed": used_legacy_context_alias
                }
            },
            "id": id
        }),
    )
}

fn list_sessions(id: Value, params: Value, state: AppState) -> (u16, Value) {
    let limit = params
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10)
        .min(50) as usize;
    let sessions = if state.config.expose_local_sessions {
        state
            .backend
            .list_sessions(limit)
            .into_iter()
            .map(session_summary_value)
            .collect::<Vec<_>>()
    } else {
        managed_session_summaries(&state, limit)
    };
    (
        200,
        json!({"jsonrpc": "2.0", "result": {"sessions": sessions}, "id": id}),
    )
}

fn resume_session(id: Value, params: Value, state: AppState) -> (u16, Value) {
    let (context_id, used_legacy_context_alias) = match context_key_from_params(&params) {
        Ok(context) => context,
        Err(message) => return invalid_params(id, message),
    };
    let Some(resume_id) = params
        .get("resumeSessionId")
        .or_else(|| params.get("codexSessionId"))
        .and_then(Value::as_str)
    else {
        return (
            200,
            json!({"jsonrpc": "2.0", "error": {"code": -32602, "message": "Missing resumeSessionId"}, "id": id}),
        );
    };

    upsert_context_record(
        &state,
        &context_id,
        resume_id,
        if used_legacy_context_alias {
            "deprecated-sessionId-alias"
        } else {
            "sessions-resume"
        },
    );

    if let Some(message) = params.get("message") {
        let text = extract_text(message);
        if !text.trim().is_empty() {
            let task_id = params
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(new_id);
            if task_id_exists(&state, &task_id) {
                return invalid_params(id, format!("Duplicate task id: {task_id}"));
            }
            let mut submitted = json!({
                "id": task_id,
                "contextId": context_id,
                "status": {"state": "working"},
                "artifacts": [{"parts": [{"type": "text", "text": "(processing - poll with tasks/get)"}], "index": 0}]
            });
            annotate_task(
                &mut submitted,
                &task_id,
                &context_id,
                false,
                used_legacy_context_alias,
            );
            if let Err(err) = persist_new_working_task(&state, &task_id, submitted.clone()) {
                return match err {
                    SubmitTaskError::Duplicate => {
                        invalid_params(id, format!("Duplicate task id: {task_id}"))
                    }
                    SubmitTaskError::Busy => server_busy(id),
                };
            }
            let worker_state = state.clone();
            let worker_context_id = context_id.clone();
            let task_options = match task_options(&params, &state.config) {
                Ok(options) => options,
                Err(message) => {
                    let mut task = task_result(&task_id, "failed", &message, &context_id);
                    annotate_task(
                        &mut task,
                        &task_id,
                        &context_id,
                        false,
                        used_legacy_context_alias,
                    );
                    persist_task(&state, &task_id, task.clone());
                    return (200, json!({"jsonrpc": "2.0", "result": task, "id": id}));
                }
            };
            thread::spawn(move || {
                run_codex(
                    worker_state,
                    task_id,
                    worker_context_id,
                    text,
                    task_options,
                    false,
                    used_legacy_context_alias,
                )
            });
            return (
                200,
                json!({"jsonrpc": "2.0", "result": submitted, "id": id}),
            );
        }
    }

    (
        200,
        json!({
            "jsonrpc": "2.0",
            "result": {
                "contextId": context_id,
                "resumeSessionId": resume_id,
                "status": "bound",
                "metadata": {
                    "deprecatedSessionIdAliasUsed": used_legacy_context_alias
                }
            },
            "id": id
        }),
    )
}

fn run_codex(
    state: AppState,
    task_id: String,
    context_id: String,
    text: String,
    options: TaskOptions,
    deprecated_send_alias_used: bool,
    deprecated_context_alias_used: bool,
) {
    if options.write_enabled {
        let lock = write_lock(&state, &options.working_directory);
        let _guard = lock.lock().unwrap();
        run_codex_locked(
            state,
            task_id,
            context_id,
            text,
            options,
            deprecated_send_alias_used,
            deprecated_context_alias_used,
        );
    } else {
        run_codex_locked(
            state,
            task_id,
            context_id,
            text,
            options,
            deprecated_send_alias_used,
            deprecated_context_alias_used,
        );
    }
}

fn run_codex_locked(
    state: AppState,
    task_id: String,
    context_id: String,
    text: String,
    options: TaskOptions,
    deprecated_send_alias_used: bool,
    deprecated_context_alias_used: bool,
) {
    let resume_session_id = state
        .sessions
        .lock()
        .unwrap()
        .get(&context_id)
        .map(|record| record.codex_session_id.clone());
    let before = trace_before(&options);
    let job = match state.backend.spawn(BackendRequest {
        prompt: text,
        options: options.clone(),
        resume_session_id,
    }) {
        Ok(job) => job,
        Err(err) => {
            let mut task = task_result_with_trace(
                &task_id,
                "failed",
                &err.to_string(),
                &context_id,
                &options,
                before,
            );
            annotate_task(
                &mut task,
                &task_id,
                &context_id,
                deprecated_send_alias_used,
                deprecated_context_alias_used,
            );
            set_task(&state, &task_id, task);
            return;
        }
    };
    state.running.lock().unwrap().insert(
        task_id.clone(),
        RunningTask {
            pid: job.pid,
            context_id: context_id.clone(),
            started_at: now_unix_secs(),
        },
    );
    let output = (job.wait)();
    state.running.lock().unwrap().remove(&task_id);
    if task_is_canceled(&state, &task_id) {
        return;
    }
    if let Some(resume_session_id) = output.resume_session_id {
        upsert_context_record(
            &state,
            &context_id,
            &resume_session_id,
            "codex-thread-started",
        );
    }

    if output.success {
        let text = if output.message.trim().is_empty() {
            "(empty response)".to_string()
        } else {
            output.message
        };
        let mut task =
            task_result_with_trace(&task_id, "completed", &text, &context_id, &options, before);
        annotate_task(
            &mut task,
            &task_id,
            &context_id,
            deprecated_send_alias_used,
            deprecated_context_alias_used,
        );
        set_task(&state, &task_id, task);
    } else {
        let mut task = task_result_with_trace(
            &task_id,
            "failed",
            &output.message,
            &context_id,
            &options,
            before,
        );
        annotate_task(
            &mut task,
            &task_id,
            &context_id,
            deprecated_send_alias_used,
            deprecated_context_alias_used,
        );
        set_task(&state, &task_id, task);
    }
}

fn default_task_options(config: &Config) -> TaskOptions {
    TaskOptions {
        sandbox_mode: "read-only".to_string(),
        working_directory: config.default_working_directory.clone(),
        write_enabled: false,
    }
}

fn task_options(params: &Value, config: &Config) -> Result<TaskOptions, String> {
    let metadata = params.get("metadata").unwrap_or(&Value::Null);
    let write_requested = metadata
        .get("writeMode")
        .and_then(Value::as_str)
        .map(|v| v == "workspace-write")
        .unwrap_or(false);
    if !write_requested {
        return Ok(default_task_options(config));
    }

    let Some(cwd) = metadata.get("workingDirectory").and_then(Value::as_str) else {
        return Err("writeMode requires metadata.workingDirectory".to_string());
    };
    let Ok(canonical) = fs::canonicalize(cwd) else {
        return Err(format!("workingDirectory does not exist: {cwd}"));
    };
    if !write_root_allowed(&canonical, config) {
        return Err(format!(
            "workingDirectory is outside configured write_roots: {}",
            canonical.to_string_lossy()
        ));
    }
    if !is_git_repo(&canonical) {
        return Err(format!(
            "workingDirectory must be inside a git repo for traceable writes: {}",
            canonical.to_string_lossy()
        ));
    }
    Ok(TaskOptions {
        sandbox_mode: "workspace-write".to_string(),
        working_directory: canonical.to_string_lossy().to_string(),
        write_enabled: true,
    })
}

fn write_lock(state: &AppState, working_directory: &str) -> Arc<Mutex<()>> {
    let mut locks = state.write_locks.lock().unwrap();
    locks
        .entry(working_directory.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn write_root_allowed(path: &Path, config: &Config) -> bool {
    config.write_roots.iter().any(|root| {
        fs::canonicalize(root)
            .map(|allowed| path.starts_with(allowed))
            .unwrap_or(false)
    })
}

fn trace_before(options: &TaskOptions) -> Value {
    json!({
        "writeEnabled": options.write_enabled,
        "sandboxMode": options.sandbox_mode,
        "workingDirectory": options.working_directory,
        "gitHeadBefore": git_capture(&options.working_directory, &["rev-parse", "HEAD"]),
        "gitStatusBefore": git_capture(&options.working_directory, &["status", "--short"]),
    })
}

fn trace_after(options: &TaskOptions, before: Value) -> Value {
    json!({
        "before": before,
        "after": {
            "gitHeadAfter": git_capture(&options.working_directory, &["rev-parse", "HEAD"]),
            "gitStatusAfter": git_capture(&options.working_directory, &["status", "--short"]),
            "gitDiffAfter": git_capture(&options.working_directory, &["diff", "--binary"]),
            "gitDiffStatAfter": git_capture(&options.working_directory, &["diff", "--stat"]),
        }
    })
}

fn git_capture(cwd: &str, args: &[&str]) -> Value {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            Value::String(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(output) => json!({
            "error": String::from_utf8_lossy(&output.stderr).trim().to_string(),
            "exitCode": output.status.code()
        }),
        Err(err) => json!({"error": err.to_string()}),
    }
}

fn session_summary_value(summary: BackendSessionSummary) -> Value {
    json!({
        "id": summary.id,
        "path": summary.path,
        "modifiedUnix": summary.modified_unix,
        "cwd": summary.cwd,
        "lastMessagePreview": summary.last_message_preview
    })
}

fn managed_session_summaries(state: &AppState, limit: usize) -> Vec<Value> {
    let mut records = state
        .sessions
        .lock()
        .unwrap()
        .clone()
        .into_iter()
        .collect::<Vec<_>>();
    records.sort_by_key(|(_, record)| std::cmp::Reverse(record.updated_at));
    records
        .into_iter()
        .take(limit)
        .map(|(context_id, record)| {
            json!({
                "id": record.codex_session_id,
                "contextId": context_id,
                "path": "",
                "modifiedUnix": record.updated_at,
                "cwd": Value::Null,
                "lastMessagePreview": Value::Null,
                "metadata": {
                    "backendKind": record.backend_kind,
                    "resumeSource": record.resume_source,
                    "createdAt": record.created_at,
                    "updatedAt": record.updated_at
                }
            })
        })
        .collect()
}

fn set_task(state: &AppState, task_id: &str, task: Value) {
    persist_task(state, task_id, task);
}

fn task_result(task_id: &str, state: &str, text: &str, context_id: &str) -> Value {
    json!({
        "id": task_id,
        "contextId": context_id,
        "status": {"state": state},
        "artifacts": [{"parts": [{"type": "text", "text": text}], "index": 0}]
    })
}

fn task_result_with_trace(
    task_id: &str,
    state: &str,
    text: &str,
    context_id: &str,
    options: &TaskOptions,
    before: Value,
) -> Value {
    if !options.write_enabled {
        let mut task = task_result(task_id, state, text, context_id);
        task.as_object_mut()
            .expect("task values are objects")
            .insert(
                "metadata".to_string(),
                json!({
                    "writeEnabled": options.write_enabled,
                    "sandboxMode": options.sandbox_mode,
                    "workingDirectory": options.working_directory
                }),
            );
        return task;
    }
    json!({
        "id": task_id,
        "contextId": context_id,
        "status": {"state": state},
        "artifacts": [
            {"parts": [{"type": "text", "text": text}], "index": 0},
            {"parts": [{"type": "data", "data": trace_after(options, before)}], "index": 1}
        ],
        "metadata": {
            "writeEnabled": options.write_enabled,
            "sandboxMode": options.sandbox_mode,
            "workingDirectory": options.working_directory
        }
    })
}

fn extract_text(message: &Value) -> String {
    message
        .get("parts")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn agent_card(public_url: &str) -> Value {
    json!({
        "name": "codex-a2a-rs",
        "description": "Rust Codex A2A server for Hermes with long-lived session resume and controlled traceable writes",
        "url": public_url,
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "a2a",
        "protocolVersion": "1.0.0",
        "preferredTransport": "JSONRPC",
        "defaultInputModes": ["text"],
        "defaultOutputModes": ["text"],
        "securitySchemes": {
            "bearerAuth": {
                "type": "http",
                "scheme": "bearer"
            }
        },
        "security": [{"bearerAuth": []}],
        "capabilities": {
            "streaming": true,
            "pushNotifications": false,
            "multiTurn": true,
            "structuredMetadata": true,
            "extensions": [{
                "uri": CODEX_SESSIONS_EXTENSION_URI,
                "description": "Codex session continuity extension for sessions/list and sessions/resume.",
                "required": false,
                "methods": ["sessions/list", "sessions/resume"]
            }]
        },
        "interfaces": ["jsonrpc"],
        "skills": [{
            "id": "codex",
            "name": "Codex",
            "description": "Ask Codex through a local A2A bridge. Use message/send as the primary method, message/stream for chunked task updates, tasks/get for polling, tasks/list for persisted task history, and tasks/cancel for best-effort cancellation. Default mode is read-only. Controlled write mode requires metadata.writeMode=workspace-write and an allowlisted git metadata.workingDirectory; write tasks are serialized per worktree and results include before/after git trace artifacts."
        }, {
            "id": "sessions",
            "name": "Codex Sessions",
            "description": "Codex session extension. List recent Codex sessions with sessions/list, bind an A2A contextId to a Codex session with sessions/resume, or pass metadata.resumeSessionId to message/send. sessionId is accepted only as a deprecated compatibility alias for contextId.",
            "extensions": [CODEX_SESSIONS_EXTENSION_URI]
        }]
    })
}

fn authorized(req: &Request, config: &Config) -> bool {
    let Some(expected) = expected_auth_token(config) else {
        return false;
    };
    let Some(header) = req.headers.get("authorization") else {
        return false;
    };
    let Some(actual) = header.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(actual.as_bytes(), expected.as_bytes())
}

fn expected_auth_token(config: &Config) -> Option<String> {
    if let Some(env_name) = &config.auth_token_env
        && let Ok(token) = std::env::var(env_name)
    {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }
    if let Some(path) = &config.auth_token_file
        && let Ok(token) = fs::read_to_string(path)
    {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(token);
        }
    }
    let Some(service) = &config.auth_token_keychain_service else {
        return None;
    };
    keychain_auth_token(service)
}

#[cfg(target_os = "macos")]
fn keychain_auth_token(service: &str) -> Option<String> {
    let Ok(output) = Command::new("security")
        .arg("find-generic-password")
        .arg("-s")
        .arg(service)
        .arg("-w")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!token.is_empty()).then_some(token)
}

#[cfg(not(target_os = "macos"))]
fn keychain_auth_token(_service: &str) -> Option<String> {
    None
}

fn warn_if_git_missing() {
    let git_available = Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !git_available {
        eprintln!(
            "warning: git was not found in PATH; write-mode repository checks and git trace artifacts will fail until git is installed"
        );
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a
        .iter()
        .zip(b.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    diff == 0
}

fn allowed_source(ip: IpAddr, config: &Config) -> bool {
    match ip {
        IpAddr::V4(v4) if v4.is_loopback() => true,
        IpAddr::V6(v6) if v6.is_loopback() => true,
        _ => config
            .allowed_sources
            .iter()
            .any(|source| source.contains(&ip)),
    }
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = TASK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("task-{nanos}-{seq}")
}

fn write_json(stream: &mut TcpStream, status: u16, payload: &Value) -> std::io::Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "OK",
    };
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    write!(
        stream,
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)
}

fn write_chunked_headers(stream: &mut TcpStream) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson; charset=utf-8\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    )
}

fn write_chunk(stream: &mut TcpStream, payload: &Value) -> std::io::Result<()> {
    let mut body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    body.push(b'\n');
    write!(stream, "{:x}\r\n", body.len())?;
    stream.write_all(&body)?;
    write!(stream, "\r\n")?;
    stream.flush()
}

fn write_final_chunk(stream: &mut TcpStream) -> std::io::Result<()> {
    write!(stream, "0\r\n\r\n")?;
    stream.flush()
}

fn is_terminal_state(state: &str) -> bool {
    matches!(state, "completed" | "failed" | "canceled")
}

fn task_is_canceled(state: &AppState, task_id: &str) -> bool {
    state
        .tasks
        .lock()
        .unwrap()
        .get(task_id)
        .and_then(|task| task.get("status"))
        .and_then(|status| status.get("state"))
        .and_then(Value::as_str)
        == Some("canceled")
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn context_filter_from_params(params: &Value) -> (Option<String>, bool) {
    if let Some(context_id) = params.get("contextId").and_then(Value::as_str) {
        return (Some(context_id.to_string()), false);
    }
    if let Some(session_id) = params.get("sessionId").and_then(Value::as_str) {
        return (Some(session_id.to_string()), true);
    }
    (None, false)
}

fn context_key_from_params(params: &Value) -> Result<(String, bool), String> {
    let (context_id, used_legacy_alias) = context_filter_from_params(params);
    context_id
        .map(|context_id| (context_id, used_legacy_alias))
        .ok_or_else(|| "contextId is required".to_string())
}

fn annotate_task(
    task: &mut Value,
    task_id: &str,
    context_id: &str,
    deprecated_send_alias_used: bool,
    deprecated_context_alias_used: bool,
) {
    let now = now_unix_secs();
    let metadata = task
        .as_object_mut()
        .expect("task values are objects")
        .entry("metadata")
        .or_insert_with(|| json!({}));
    let metadata_obj = metadata.as_object_mut().expect("task metadata is object");
    if !metadata_obj.contains_key("createdAt") {
        metadata_obj.insert("createdAt".to_string(), json!(now));
    }
    metadata_obj.insert("updatedAt".to_string(), json!(now));
    metadata_obj.insert(
        "deprecatedTasksSendAliasUsed".to_string(),
        json!(deprecated_send_alias_used),
    );
    metadata_obj.insert(
        "deprecatedSessionIdAliasUsed".to_string(),
        json!(deprecated_context_alias_used),
    );
    metadata_obj.insert("taskId".to_string(), json!(task_id));
    metadata_obj.insert("contextId".to_string(), json!(context_id));
}

fn task_updated_at(task: &Value) -> u64 {
    task.get("metadata")
        .and_then(|metadata| metadata.get("updatedAt"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn task_id_exists(state: &AppState, task_id: &str) -> bool {
    state.tasks.lock().unwrap().contains_key(task_id)
        || state.running.lock().unwrap().contains_key(task_id)
}

fn invalid_params(id: Value, message: String) -> (u16, Value) {
    (
        200,
        json!({"jsonrpc": "2.0", "error": {"code": -32602, "message": message}, "id": id}),
    )
}

fn server_busy(id: Value) -> (u16, Value) {
    (
        200,
        json!({"jsonrpc": "2.0", "error": {"code": -32004, "message": "Server busy"}, "id": id}),
    )
}

fn persist_task(state: &AppState, task_id: &str, task: Value) {
    {
        state
            .tasks
            .lock()
            .unwrap()
            .insert(task_id.to_string(), task);
    }
    if let Err(err) = save_task_registry(state) {
        eprintln!("failed to persist task registry: {err}");
    }
}

enum SubmitTaskError {
    Duplicate,
    Busy,
}

fn persist_new_working_task(
    state: &AppState,
    task_id: &str,
    task: Value,
) -> Result<(), SubmitTaskError> {
    {
        let mut tasks = state.tasks.lock().unwrap();
        if tasks.contains_key(task_id) {
            return Err(SubmitTaskError::Duplicate);
        }
        let active_count = tasks
            .values()
            .filter(|task| {
                task.get("status")
                    .and_then(|status| status.get("state"))
                    .and_then(Value::as_str)
                    == Some("working")
            })
            .count();
        if active_count >= state.config.max_concurrent_tasks {
            return Err(SubmitTaskError::Busy);
        }
        tasks.insert(task_id.to_string(), task);
    }
    if let Err(err) = save_task_registry(state) {
        eprintln!("failed to persist task registry: {err}");
    }
    Ok(())
}

fn upsert_context_record(
    state: &AppState,
    context_id: &str,
    codex_session_id: &str,
    resume_source: &str,
) {
    {
        let now = now_unix_secs();
        let mut sessions = state.sessions.lock().unwrap();
        let created_at = sessions
            .get(context_id)
            .map(|record| record.created_at)
            .unwrap_or(now);
        sessions.insert(
            context_id.to_string(),
            ContextRecord {
                codex_session_id: codex_session_id.to_string(),
                created_at,
                updated_at: now,
                backend_kind: state.backend.kind().to_string(),
                resume_source: resume_source.to_string(),
            },
        );
    }
    if let Err(err) = save_context_registry(state) {
        eprintln!("failed to persist context registry: {err}");
    }
}

fn registry_dir(config: &Config) -> PathBuf {
    config.sessions_dir.join(".codex-a2a")
}

fn task_registry_path(config: &Config) -> PathBuf {
    registry_dir(config).join("tasks.json")
}

fn context_registry_path(config: &Config) -> PathBuf {
    registry_dir(config).join("contexts.json")
}

fn load_task_registry(config: &Config) -> HashMap<String, Value> {
    let path = task_registry_path(config);
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<HashMap<String, Value>>(&content).ok())
        .unwrap_or_default()
}

fn load_context_registry(config: &Config) -> HashMap<String, ContextRecord> {
    let path = context_registry_path(config);
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<HashMap<String, ContextRecord>>(&content).ok())
        .unwrap_or_default()
}

fn save_task_registry(state: &AppState) -> Result<(), String> {
    let tasks = state.tasks.lock().unwrap().clone();
    save_json_file(&task_registry_path(&state.config), &tasks)
}

fn save_context_registry(state: &AppState) -> Result<(), String> {
    let sessions = state.sessions.lock().unwrap().clone();
    save_json_file(&context_registry_path(&state.config), &sessions)
}

fn save_json_file<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let content = serde_json::to_vec_pretty(value).map_err(|err| err.to_string())?;
    let tmp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("json")
    ));
    fs::write(&tmp_path, content).map_err(|err| err.to_string())?;
    fs::rename(&tmp_path, path).map_err(|err| err.to_string())
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
