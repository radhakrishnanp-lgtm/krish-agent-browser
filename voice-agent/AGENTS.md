# Voice Agent: instructions for coding agents

Read `PLAYBOOK.md` in this folder before changing anything. Part 8 is written for you: ground
rules, file map, WebSocket protocol, recipes and the definition of done.

The short version:

- Run `./scripts/check.sh` before every commit. It is free and needs no API keys.
- Never commit secrets (`.env`) or user data (`data/`, `logs/sessions/`).
- Models, prices, prompts and thresholds live in `config/*.yaml`, not in code.
- Do not say something works unless you ran it. Record what you could not test in
  `docs/checkpoints.md`.
- Work only inside `voice-agent/`. The repository root is a different project.
