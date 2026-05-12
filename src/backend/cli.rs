use super::{
    Backend, BackendCancelRequest, BackendExecutionResult, BackendJob, BackendRequest,
    BackendSessionSummary, wrap_prompt,
};
use crate::config::Config;
use crate::process::{configure_child_process, terminate_process_tree};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

pub struct CliBackend {
    config: Arc<Config>,
}

impl CliBackend {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl Backend for CliBackend {
    fn kind(&self) -> &'static str {
        "codex-cli"
    }

    fn spawn(&self, request: BackendRequest) -> Result<BackendJob, String> {
        let mut cmd = Command::new(&self.config.codex_bin);
        if let Some(session_id) = request.resume_session_id {
            cmd.arg("exec")
                .arg("resume")
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("-c")
                .arg("approval_policy=\"never\"")
                .arg("-c")
                .arg(format!("sandbox_mode=\"{}\"", request.options.sandbox_mode));
            append_write_mode_sandbox_permissions(
                &mut cmd,
                request.options.write_enabled,
                &self.config.write_mode_sandbox_permissions,
            );
            cmd.arg(session_id)
                .arg(wrap_prompt(&request.prompt, &request.options));
        } else {
            cmd.arg("exec")
                .arg("--json")
                .arg("--skip-git-repo-check")
                .arg("-c")
                .arg("approval_policy=\"never\"")
                .arg("-c")
                .arg(format!("sandbox_mode=\"{}\"", request.options.sandbox_mode));
            append_write_mode_sandbox_permissions(
                &mut cmd,
                request.options.write_enabled,
                &self.config.write_mode_sandbox_permissions,
            );
            cmd.arg("-C")
                .arg(&request.options.working_directory)
                .arg(wrap_prompt(&request.prompt, &request.options));
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_child_process(&mut cmd);

        let child = cmd.spawn().map_err(|err| err.to_string())?;
        let pid = child.id();
        Ok(BackendJob {
            pid,
            wait: Box::new(move || {
                let output = match child.wait_with_output() {
                    Ok(output) => output,
                    Err(err) => {
                        return BackendExecutionResult {
                            success: false,
                            message: err.to_string(),
                            resume_session_id: None,
                        };
                    }
                };

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let mut last_message = String::new();
                let mut resume_session_id = None;
                for line in stdout.lines() {
                    let Ok(event) = serde_json::from_str::<Value>(line) else {
                        continue;
                    };
                    if event.get("type").and_then(Value::as_str) == Some("thread.started") {
                        resume_session_id = event
                            .get("thread_id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    if event.get("type").and_then(Value::as_str) == Some("item.completed") {
                        let item = event.get("item").unwrap_or(&Value::Null);
                        if item.get("type").and_then(Value::as_str) == Some("agent_message")
                            && let Some(text) = item.get("text").and_then(Value::as_str)
                        {
                            last_message = text.to_string();
                        }
                    }
                }

                let message = if output.status.success() {
                    last_message
                } else if stderr.trim().is_empty() {
                    stdout.trim().to_string()
                } else {
                    stderr.trim().to_string()
                };

                BackendExecutionResult {
                    success: output.status.success(),
                    message,
                    resume_session_id,
                }
            }),
        })
    }

    fn cancel(&self, request: &BackendCancelRequest) -> Result<(), String> {
        terminate_process_tree(request.pid)
    }

    fn list_sessions(&self, limit: usize) -> Vec<BackendSessionSummary> {
        let mut files = Vec::new();
        collect_jsonl(&self.config.sessions_dir, &mut files);
        files.sort_by_key(|path| {
            fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
        files.reverse();
        files
            .into_iter()
            .take(limit)
            .map(|path| session_summary(&path))
            .collect()
    }
}

fn append_write_mode_sandbox_permissions(
    cmd: &mut Command,
    write_enabled: bool,
    permissions: &[String],
) {
    if !write_enabled || permissions.is_empty() {
        return;
    }
    cmd.arg("-c").arg(format!(
        "sandbox_permissions={}",
        shell_double_quoted_toml_string_array(permissions)
    ));
}

fn toml_string_array(values: &[String]) -> String {
    let values = values
        .iter()
        .map(|value| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{values}]")
}

fn shell_double_quoted_toml_string_array(values: &[String]) -> String {
    let inner = toml_string_array(values).replace("\"", "\\\"");
    format!("\"{inner}\"")
}

fn collect_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn session_summary(path: &Path) -> BackendSessionSummary {
    let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let id = session_id_from_filename(filename);
    let modified_unix = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut cwd = Value::Null;
    let mut last_message_preview = Value::Null;

    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines().rev().take(400) {
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if cwd.is_null() && event.get("type").and_then(Value::as_str) == Some("turn_context") {
                cwd = event
                    .get("payload")
                    .and_then(|p| p.get("cwd"))
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            if last_message_preview.is_null()
                && event.get("type").and_then(Value::as_str) == Some("response_item")
                && let Some(text) = event
                    .get("payload")
                    .and_then(|p| p.get("content"))
                    .and_then(Value::as_array)
                    .and_then(|parts| {
                        parts
                            .iter()
                            .find_map(|p| p.get("text").and_then(Value::as_str))
                    })
            {
                last_message_preview = Value::String(text.chars().take(240).collect());
            }
            if !cwd.is_null() && !last_message_preview.is_null() {
                break;
            }
        }
    }

    BackendSessionSummary {
        id,
        path: path.to_string_lossy().to_string(),
        modified_unix,
        cwd,
        last_message_preview,
    }
}

fn session_id_from_filename(filename: &str) -> String {
    let stem = filename.trim_end_matches(".jsonl");
    if stem.len() >= 36 {
        let candidate = &stem[stem.len() - 36..];
        let valid = candidate.chars().enumerate().all(|(idx, ch)| match idx {
            8 | 13 | 18 | 23 => ch == '-',
            _ => ch.is_ascii_hexdigit(),
        });
        if valid {
            return candidate.to_string();
        }
    }
    stem.to_string()
}
