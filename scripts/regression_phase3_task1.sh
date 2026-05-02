#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cargo build
cargo test
target_dir="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"

tmpdir="$(mktemp -d)"
server_pid=""
cleanup() {
  if [[ -n "$server_pid" ]] && kill -0 "$server_pid" 2>/dev/null; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
  fi
  rm -rf "$tmpdir"
}
trap cleanup EXIT

port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
mkdir -p "$tmpdir/sessions" "$tmpdir/workspace"
git -C "$tmpdir/workspace" init >/dev/null 2>&1
python3 - <<'PY' "$tmpdir/sessions" "$tmpdir/workspace" "$tmpdir/config.toml" "$port" "$repo_root/scripts/mock_codex.sh"
import json, pathlib, sys
sessions_dir = pathlib.Path(sys.argv[1])
workspace = pathlib.Path(sys.argv[2])
config_path = pathlib.Path(sys.argv[3])
port = sys.argv[4]
codex_bin = sys.argv[5]
(sessions_dir / 'seed-12345678-1234-1234-1234-123456789abc.jsonl').write_text(
    '\n'.join([
        json.dumps({"type": "turn_context", "payload": {"cwd": str(workspace)}}),
        json.dumps({"type": "response_item", "payload": {"content": [{"text": "seed prompt"}]}}),
        '',
    ]),
    encoding='utf-8',
)
config_path.write_text(
    '\n'.join([
        'host = "127.0.0.1"',
        f'port = {port}',
        f'public_url = "http://127.0.0.1:{port}"',
        f'sessions_dir = "{sessions_dir}"',
        f'default_working_directory = "{workspace}"',
        f'codex_bin = "{codex_bin}"',
        'auth_token_keychain_service = "A2A_AUTH_TOKEN"',
        'max_body_bytes = 1048576',
        'allowed_sources = ["127.0.0.1/32"]',
        f'write_roots = ["{workspace}"]',
        '',
    ]),
    encoding='utf-8',
)
PY

export PATH="$repo_root/scripts:$PATH"
export CODEX_A2A_CONFIG="$tmpdir/config.toml"

"$target_dir/debug/codex-a2a-rs" >"$tmpdir/server.log" 2>&1 &
server_pid="$!"

for _ in $(seq 1 50); do
  if curl -fsS "http://127.0.0.1:$port/health" >/dev/null 2>&1; then
    break
  fi
  sleep 0.2
done
curl -fsS "http://127.0.0.1:$port/health" >/dev/null

auth_header="Authorization: Bearer test-token"
json_header="Content-Type: application/json"

agent_json="$(curl -fsS "http://127.0.0.1:$port/.well-known/agent.json")"
python3 - <<'PY' "$agent_json"
import json, sys
agent = json.loads(sys.argv[1])
assert agent['name'] == 'codex-a2a-rs'
assert agent['url'].startswith('http://127.0.0.1:')
assert any(skill.get('id') == 'codex' for skill in agent.get('skills', []))
assert any(ext.get('uri') == 'urn:codex-a2a:extensions:codex-sessions:v1' for ext in agent.get('capabilities', {}).get('extensions', []))
PY

send_response="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"send-1","method":"message/send","params":{"contextId":"ctx-send","message":{"parts":[{"type":"text","text":"hello send"}]}}}' "http://127.0.0.1:$port/")"
send_task_id="$(python3 - <<'PY' "$send_response"
import json, sys
payload = json.loads(sys.argv[1])
assert payload['result']['status']['state'] == 'working'
print(payload['result']['id'])
PY
)"

for _ in $(seq 1 50); do
  get_response="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"get-1","method":"tasks/get","params":{"id":"'"$send_task_id"'"}}' "http://127.0.0.1:$port/")"
  state="$(python3 - <<'PY' "$get_response"
import json, sys
payload = json.loads(sys.argv[1])
print(payload.get('result', {}).get('status', {}).get('state', ''))
PY
)"
  if [[ "$state" == "completed" ]]; then
    break
  fi
  sleep 0.2
done
python3 - <<'PY' "$get_response"
import json, sys
payload = json.loads(sys.argv[1])
assert payload['result']['status']['state'] == 'completed'
PY

tasks_list="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"list-1","method":"tasks/list","params":{"limit":20,"contextId":"ctx-send"}}' "http://127.0.0.1:$port/")"
python3 - <<'PY' "$tasks_list" "$send_task_id"
import json, sys
payload = json.loads(sys.argv[1])
ids = [task['id'] for task in payload['result']['tasks']]
assert sys.argv[2] in ids
PY

sessions_list="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"sessions-1","method":"sessions/list","params":{"limit":10}}' "http://127.0.0.1:$port/")"
python3 - <<'PY' "$sessions_list"
import json, sys
payload = json.loads(sys.argv[1])
assert payload['result']['sessions']
assert payload['result']['sessions'][0]['contextId'] == 'ctx-send'
assert payload['result']['sessions'][0]['path'] == ''
PY

missing_context="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"missing-context","method":"message/send","params":{"message":{"parts":[{"type":"text","text":"missing context"}]}}}' "http://127.0.0.1:$port/")"
python3 - <<'PY' "$missing_context"
import json, sys
payload = json.loads(sys.argv[1])
assert payload['error']['code'] == -32602
assert payload['error']['message'] == 'contextId is required'
PY

duplicate_id="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"duplicate-id","method":"message/send","params":{"id":"'"$send_task_id"'","contextId":"ctx-send","message":{"parts":[{"type":"text","text":"duplicate id"}]}}}' "http://127.0.0.1:$port/")"
python3 - <<'PY' "$duplicate_id"
import json, sys
payload = json.loads(sys.argv[1])
assert payload['error']['code'] == -32602
assert payload['error']['message'].startswith('Duplicate task id:')
PY

resume_response="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"resume-1","method":"sessions/resume","params":{"contextId":"ctx-resume","resumeSessionId":"12345678-1234-1234-1234-123456789abc"}}' "http://127.0.0.1:$port/")"
python3 - <<'PY' "$resume_response"
import json, sys
payload = json.loads(sys.argv[1])
assert payload['result']['status'] == 'bound'
assert payload['result']['resumeSessionId'] == '12345678-1234-1234-1234-123456789abc'
PY

stream_response="$(curl -fsS -N -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"stream-1","method":"message/stream","params":{"contextId":"ctx-stream","message":{"parts":[{"type":"text","text":"hello stream"}]}}}' "http://127.0.0.1:$port/")"
python3 - <<'PY' "$stream_response"
import json, sys
lines = [json.loads(line) for line in sys.argv[1].splitlines() if line.strip()]
assert any(line.get('result', {}).get('kind') == 'task' for line in lines)
assert any(line.get('method') == 'tasks/status-update' for line in lines)
assert any(line.get('method') == 'tasks/final' for line in lines)
PY

cancel_submit="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"cancel-submit","method":"message/send","params":{"contextId":"ctx-cancel","message":{"parts":[{"type":"text","text":"please run SLOW cancel test"}]}}}' "http://127.0.0.1:$port/")"
cancel_task_id="$(python3 - <<'PY' "$cancel_submit"
import json, sys
payload = json.loads(sys.argv[1])
print(payload['result']['id'])
PY
)"
cancel_response="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"cancel-1","method":"tasks/cancel","params":{"id":"'"$cancel_task_id"'"}}' "http://127.0.0.1:$port/")"
python3 - <<'PY' "$cancel_response"
import json, sys
payload = json.loads(sys.argv[1])
assert payload['result']['status']['state'] == 'canceled'
PY

alias_send="$(curl -fsS -H "$auth_header" -H "$json_header" -d '{"jsonrpc":"2.0","id":"alias-1","method":"tasks/send","params":{"sessionId":"legacy-session","message":{"parts":[{"type":"text","text":"alias send"}]}}}' "http://127.0.0.1:$port/")"
python3 - <<'PY' "$alias_send"
import json, sys
payload = json.loads(sys.argv[1])
metadata = payload['result']['metadata']
assert metadata['deprecatedTasksSendAliasUsed'] is True
assert metadata['deprecatedSessionIdAliasUsed'] is True
PY

echo "regression gate passed"
