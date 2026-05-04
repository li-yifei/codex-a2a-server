# codex-a2a-rs

A small, single-binary Rust server that exposes [Codex](https://github.com/openai/codex) over the [A2A protocol](https://github.com/google/A2A). Drop it on your dev box, point any A2A-compatible agent at it, and Codex runs locally with your tools, your sandbox, and your git repos.

**What you get**

- Local Codex execution from a remote agent over A2A JSON-RPC
- Session resume across reconnects (`sessions/resume`)
- Streaming responses via NDJSON (`message/stream`)
- Controlled write mode with git trace artifacts before/after each task
- Bearer auth from env var, file, or macOS Keychain, fail-closed by default
- Concurrent task admission control and per-connection read timeouts
- Cross-platform: macOS, Linux (glibc and musl), Windows

## Quickstart

```bash
# 1. Build (requires Rust 1.88+)
cargo build --release

# 2. Configure
cp config.example.toml config.toml
$EDITOR config.toml   # set sessions_dir, default_working_directory, public_url

# 3. Set the auth token
export A2A_AUTH_TOKEN="$(openssl rand -base64 32)"

# 4. Run
./target/release/codex-a2a-server

# 5. Verify
curl -s -H "Authorization: Bearer $A2A_AUTH_TOKEN" \
     http://127.0.0.1:18081/.well-known/agent.json | head -5
```

If the agent card returns, you are up. Point your A2A client at the same URL with the same bearer token.

## How it works

```
+----------------+   A2A JSON-RPC   +----------------+   stdio   +----------+
| Remote agent   | ---- HTTPS ----> | codex-a2a-rs   | --------> |  codex   |
| (Hermes Agent, | <-- NDJSON ----- | (this server)  | <-------- |  CLI /   |
|  or any A2A    |                  |                |           |  app-    |
|  client)       |                  +----------------+           |  server  |
+----------------+                         |                     +----------+
                                           | persists
                                           v
                                    tasks.json
                                    contexts.json
```

Two backend modes:

| Backend | Command | Best for |
|---------|---------|----------|
| `cli` (default) | `codex exec` | Simple setups, one-shot tasks |
| `native` | `codex app-server --listen stdio://` | Production use, session resume, turn interruption |

Set `backend = "native"` or `backend = "cli"` in `config.toml`.

## Configuration

Create a local config from the tracked example:

```bash
cp config.example.toml config.toml
```

`config.toml` is gitignored. Key fields:

| Field | Purpose | Default |
|-------|---------|---------|
| `public_url` | Advertised URL in the agent card | required |
| `sessions_dir` | Where Codex sessions live on disk | required |
| `default_working_directory` | Workspace root for read-only tasks | required |
| `backend` | `"cli"` or `"native"` | `"cli"` |
| `auth_token_env` | Env var name holding the bearer token | `"A2A_AUTH_TOKEN"` |
| `auth_token_file` | Path to a token file (chmod 600 on Unix) | none |
| `auth_token_keychain_service` | macOS Keychain service name | `"A2A_AUTH_TOKEN"` |
| `allowed_sources` | IP/CIDR allowlist | Tailscale ranges |
| `write_roots` | Directories where write mode is allowed | `[]` |
| `max_concurrent_tasks` | Admission control limit | `4` |
| `read_timeout_secs` | Per-connection read timeout | `30` |
| `expose_local_sessions` | Expose all local Codex sessions in `sessions/list` | `false` |

Override the config path with `CODEX_A2A_CONFIG=/path/to/config.toml`.

## Endpoints

Agent metadata:

```
GET /.well-known/agent.json
GET /.well-known/agent-card.json
```

`agent-card.json` is the A2A spec name; `agent.json` is the legacy alias. Both return the same card.

JSON-RPC:

```
POST /
POST /a2a/jsonrpc
```

## Authentication

All requests require a bearer token:

```
Authorization: Bearer <token>
```

The server reads the token from the first configured source that returns a non-empty value:

1. Environment variable named by `auth_token_env`
2. File at `auth_token_file` (use `chmod 600` on Unix; a current-user-private directory or ACL on Windows)
3. macOS Keychain service named by `auth_token_keychain_service` (macOS only; skipped on other platforms)

If no source produces a token, all requests are rejected. This is intentional: fail-closed.

```bash
# Option A: env var (simplest)
export A2A_AUTH_TOKEN="$(openssl rand -base64 32)"

# Option B: macOS Keychain
security add-generic-password -s A2A_AUTH_TOKEN -a "$USER" -w "$(openssl rand -base64 32)"
```

## Write Mode

Default `message/send` is read-only. To enable writes, include metadata:

```json
{
  "contextId": "my-context",
  "message": { "role": "user", "parts": [{"type": "text", "text": "refactor auth module"}] },
  "metadata": {
    "writeMode": "workspace-write",
    "workingDirectory": "/path/to/allowed/repo"
  }
}
```

The working directory must be inside a configured `write_roots` entry and inside a git worktree. Write tasks are serialized per working directory. Results include before/after git trace artifacts (HEAD, status, diff).

Write mode requires `git` in `PATH`. On Windows, install [Git for Windows](https://gitforwindows.org/) and make `git.exe` visible to the service account.

For private copy-and-work workflows, set `write_mode_sandbox_permissions = ["disk-full-read-access"]` in your local config. The tracked example keeps this empty for public-facing defaults.

## Sessions

`sessions/list` and `sessions/resume` are declared through the Codex sessions extension (`urn:codex-a2a:extensions:codex-sessions:v1`).

By default, `sessions/list` returns only A2A-managed context bindings from `contexts.json`. Set `expose_local_sessions = true` for private deployments that intentionally expose recent local Codex sessions.

List recent sessions:

```json
{"jsonrpc":"2.0","id":"1","method":"sessions/list","params":{"limit":10}}
```

Resume a previous Codex session under a new A2A context:

```json
{"jsonrpc":"2.0","id":"2","method":"sessions/resume","params":{
  "contextId":"hermes-thread-1",
  "resumeSessionId":"<codex-session-uuid>"
}}
```

`message/send`, `message/stream`, and `sessions/resume` accept `contextId`. When omitted, the server creates a fresh context. `sessionId` is accepted as a deprecated alias.

## Tasks

Send a task:

```json
{"jsonrpc":"2.0","id":"3","method":"message/send","params":{
  "contextId":"hermes-thread-1",
  "message":{"role":"user","parts":[{"type":"text","text":"list files in src/"}]}
}}
```

Response:

```json
{"jsonrpc":"2.0","id":"3","result":{
  "id":"task-1","contextId":"hermes-thread-1",
  "status":{"state":"completed"},
  "artifacts":[{"parts":[{"type":"text","text":"src/main.rs\nsrc/server.rs\n..."}]}]
}}
```

`tasks/send` is accepted as a deprecated alias for `message/send`.

List and poll:

```json
{"jsonrpc":"2.0","id":"4","method":"tasks/list","params":{"limit":20,"contextId":"hermes-thread-1"}}
{"jsonrpc":"2.0","id":"5","method":"tasks/get","params":{"id":"task-1"}}
```

Task and context registries are persisted under `<sessions_dir>/.codex-a2a/`.

## Streaming

`message/stream` returns an HTTP chunked `application/x-ndjson` response. Events include the initial task, status updates, and a final task payload.

```json
{"jsonrpc":"2.0","id":"6","method":"message/stream","params":{
  "contextId":"hermes-thread-1",
  "message":{"role":"user","parts":[{"type":"text","text":"hello"}]}
}}
```

## Cancellation

Cancel an in-flight task:

```json
{"jsonrpc":"2.0","id":"7","method":"tasks/cancel","params":{"id":"task-1"}}
```

Cancellation is best-effort. The CLI backend terminates the Codex child process. The native backend sends `turn/interrupt` to the active app-server turn first, then falls back to process termination. The server marks the task `canceled` and persists the terminal state.

## Build

Requires Rust 1.88 or newer.

```bash
cargo build --release
```

Cross-build release targets with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild):

```bash
cargo zigbuild --release --target x86_64-unknown-linux-gnu
cargo zigbuild --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target aarch64-unknown-linux-gnu
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target x86_64-pc-windows-gnu
```

The Windows GNU build is a native Win32 console executable. Its runtime dependencies are Windows system DLLs plus UCRT API-set DLLs present on Windows 10 1709+ and Windows 11. Cygwin and MSYS2 runtimes are not required. This profile depends on `cargo-zigbuild` linking against UCRT; classic mingw-w64 toolchains link `msvcrt.dll` instead.

## Running as a Service

**macOS** (launchd): install the template plist:

```bash
cp launchd/local.codex-a2a-rs.plist ~/Library/LaunchAgents/
# Edit the plist: set CODEX_A2A_CONFIG to your config.toml path
launchctl bootout gui/$(id -u)/local.codex-a2a-rs 2>/dev/null || true
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/local.codex-a2a-rs.plist
```

**Linux** (systemd): create a user service:

```ini
# ~/.config/systemd/user/codex-a2a.service
[Unit]
Description=Codex A2A Server

[Service]
Environment=CODEX_A2A_CONFIG=%h/.codex/a2a-server/config.toml
Environment=A2A_AUTH_TOKEN=your-token-here
ExecStart=%h/.codex/a2a-server/target/release/codex-a2a-server
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now codex-a2a
```

**Windows**: use Task Scheduler to run the binary at logon, or wrap it with [NSSM](https://nssm.cc/) as a Windows service. Set `CODEX_A2A_CONFIG` and `A2A_AUTH_TOKEN` as environment variables for the service account.

## Regression Tests

```bash
./scripts/regression_phase3_task1.sh
```

The script rebuilds the crate, launches a local server with mock `security` and Codex commands, and verifies:
- agent card discovery
- `message/send` and `tasks/get`
- `tasks/list`
- `sessions/list` and `sessions/resume`
- `message/stream`
- `tasks/cancel`
- duplicate task-id rejection and omitted contextId context creation
- deprecated alias continuity (`tasks/send`, `sessionId`)

## License

MIT
