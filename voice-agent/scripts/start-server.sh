#!/usr/bin/env bash
# Builds (first time, or after a code change) and starts the voice agent: API + UI on one port.
# Host and port come from config/default.yaml, overridable with HOST / PORT.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ ! -f .env ] && [ -z "${OPENROUTER_API_KEY:-}" ]; then
  echo "No .env file found. Run: cp .env.example .env   and add your API keys." >&2
  exit 1
fi
if ! command -v cargo >/dev/null; then
  echo "Rust is not installed. Get it from https://rustup.rs, or run the app with Docker (see PLAYBOOK.md)." >&2
  exit 1
fi
if [ -z "${ORT_DYLIB_PATH:-}" ]; then
  ./scripts/get-onnxruntime.sh
fi

cargo build --release
exec ./target/release/voice-agent
