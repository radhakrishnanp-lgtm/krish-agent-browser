#!/usr/bin/env bash
# Starts the voice agent (API + UI on one port). Host/port come from config/default.yaml,
# overridable with HOST / PORT environment variables.
set -euo pipefail
cd "$(dirname "$0")/.."

if [ ! -f .env ] && [ -z "${OPENROUTER_API_KEY:-}" ]; then
  echo "No .env file found. Run: cp .env.example .env   and add your API keys." >&2
  exit 1
fi

read -r CFG_HOST CFG_PORT < <(python3 -c "
import yaml
s = (yaml.safe_load(open('config/default.yaml')) or {}).get('server', {})
print(s.get('host', '0.0.0.0'), s.get('port', 8080))")

# No access log: request URLs carry the access token. 2 MB is far above any audio frame the UI sends.
exec python3 -m uvicorn server.main:app --host "${HOST:-$CFG_HOST}" --port "${PORT:-$CFG_PORT}" --no-access-log --ws-max-size 2097152
