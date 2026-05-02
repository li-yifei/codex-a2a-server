#!/usr/bin/env bash
set -euo pipefail

prompt="${*: -1}"
resume_thread=""
prev=""
for arg in "$@"; do
  if [[ "$prev" == "resume" ]]; then
    resume_thread="$arg"
    prev=""
    continue
  fi
  prev="$arg"
done

thread_id="$resume_thread"
if [[ -z "$thread_id" ]]; then
  thread_id="00000000-0000-0000-0000-$(date +%012s)"
fi

printf '{"type":"thread.started","thread_id":"%s"}\n' "$thread_id"

if [[ "$prompt" == *"SLOW"* ]]; then
  sleep 5
fi

escaped_prompt=$(python3 -c 'import json, sys; print(json.dumps(sys.argv[1]))' "$prompt")
printf '{"type":"item.completed","item":{"type":"agent_message","text":%s}}\n' "$escaped_prompt"
