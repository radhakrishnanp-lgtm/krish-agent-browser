#!/usr/bin/env bash
# Fast checks that need no API keys and make no paid calls. Run before every commit.
set -euo pipefail
cd "$(dirname "$0")/.."
echo "== build"
cargo build --release 2>&1 | tail -1
echo "== unit tests"
cargo test --release 2>&1 | grep -E "test result|FAILED|panicked|error" || true
cargo test --release >/dev/null 2>&1
echo "== config"
./target/release/voice-agent --check
echo "== UI scripts parse"
if command -v node >/dev/null; then node --check ui/app.js && node --check ui/theme.js && node --check ui/mic-worklet.js && echo "  ok"; else echo "  skipped (node not installed)"; fi
echo "All checks passed."
