#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PY="${NAVI_PYTHON:-$HOME/.local/share/navi/venv/bin/python}"
if [ ! -x "$PY" ]; then
  PY="python3"
fi

cd "$ROOT"

echo "==> Checking OpenCode contract against live server..."
"$PY" workers/opencode_contract.py check

echo "==> Running gateway, assistant, and streaming suites..."
NAVI_ISOLATED=1 "$PY" scripts/test_opencode_gateway.py
NAVI_ISOLATED=1 NAVI_TEST_BINARY="${NAVI_TEST_BINARY:-/tmp/navi-cargo-target/debug/navid}" "$PY" scripts/test_assistant.py
NAVI_ISOLATED=1 NAVI_TEST_BINARY="${NAVI_TEST_BINARY:-/tmp/navi-cargo-target/debug/navid}" "$PY" scripts/test_gui_isolated.py --chat
