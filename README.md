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

`aitop` reads local metadata and avoids displaying full transcript content in the dashboard.

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

Open the dashboard:

```bash
aitop
```

Print a one-shot text snapshot:

```bash
aitop --once
```

Quit the dashboard with `q`. Use up/down or `j`/`k` to select a session.

## Development

```bash
cargo test
cargo run -- --once
cargo run
```
