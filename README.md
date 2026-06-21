# aitop

`aitop` is a small btop-inspired terminal dashboard for local AI agent activity.

The primary flow is ambient:

```bash
aitop
```

It discovers native Claude and Codex activity without requiring wrapper commands.

## What the MVP Shows

- Live Claude CLI sessions from `~/.claude/sessions/*.json`
- Live Codex work from `~/.codex/process_manager/chat_processes.json`
- Recent Codex threads from `~/.codex/state_5.sqlite`
- Claude journal metadata from `~/.claude/projects`
- PID, CPU, memory, repo, branch, dirty files, model, and token totals where available
- A btop-inspired activity skyline showing recent ambient agent activity over time
- Focused session tails with normalized user/assistant/thinking/tool/usage events
- Error, file-edit, command, and token-spike annotations where they can be inferred

`aitop` keeps the monitor view metadata-first. Tail view intentionally renders the selected native journal so you can inspect a focused session.

## Install

From this repo:

```bash
./scripts/install.sh
```

By default, this installs to `~/.local/bin/aitop`.

Choose another install directory:

```bash
AITOP_INSTALL_DIR=/some/bin ./scripts/install.sh
```

## Usage

Open the dashboard. By default, the session list shows active sessions only:

```bash
aitop
```

Print a one-shot text snapshot:

```bash
aitop --once
```

Monitor controls:

- `up`/`down` or `j`/`k`: select a session
- `enter`: open the focused tail view
- `a`: toggle active-only and all sessions
- `r`: refresh
- `q`: quit

Tail controls:

- `up`/`down` or `j`/`k`: select a session
- `page up` / `page down`: scroll the focused session feed
- `g` / `G`: jump toward top or bottom
- `esc`: return to monitor
- `a`: return to monitor and toggle active-only/all sessions
- `q`: quit

Future idea: an ask-style footer REPL for questions about visible sessions, processes, git state, and recent events.

## Development

```bash
cargo test
cargo run -- --once
cargo run
```
