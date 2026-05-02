#!/usr/bin/env bash
set -euo pipefail

if [[ "${1-}" == "find-generic-password" && "${2-}" == "-s" && -n "${3-}" && "${4-}" == "-w" ]]; then
  printf 'test-token\n'
  exit 0
fi

printf 'unsupported mock security invocation: %s\n' "$*" >&2
exit 1
