#!/usr/bin/env bash
# Fast checks that need no API keys and make no paid calls. Run before every commit.
set -euo pipefail
cd "$(dirname "$0")/.."
echo "== config loads and every preset resolves"
python3 -c "from server.config import Config; c = Config(); print('  presets:', ', '.join(c.presets)); print('  models:', {k: len(v) for k, v in c.catalog.items()})"
echo "== every catalog model has a price"
python3 -c "
from server.config import Config, KINDS
c = Config(); missing = [f'{k}/{i}' for k in KINDS for i in c.catalog[k] if c.rates(c.stage(k, i)) is None]
print('  missing:', missing or 'none'); raise SystemExit(1 if missing else 0)"
echo "== unit tests"
python3 -m unittest discover tests 2>&1 | tail -3
echo "== UI scripts parse"
if command -v node >/dev/null; then node --check ui/app.js && node --check ui/theme.js && node --check ui/mic-worklet.js && echo "  ok"; else echo "  skipped (node not installed)"; fi
echo "== server imports"
python3 -c "import server.main" 2>&1 | grep -iE "error|traceback" && exit 1 || echo "  ok"
echo "All checks passed."
