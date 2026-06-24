# aitop

`aitop` is a btop-inspired terminal dashboard for local AI agent activity. It reads native Claude and Codex state directly, so you can see what is active, what recently ran, and what happened in the focused session without wrapping your agent commands.

<img width="1703" height="999" alt="Screenshot 2026-06-23 at 11 59 12 PM" src="https://github.com/user-attachments/assets/d8beadcb-d84a-4ec6-b8fe-5223ce148ce0" />


<img width="1478" height="1024" alt="Screenshot 2026-06-21 at 5 49 53 AM" src="https://github.com/user-attachments/assets/4ada2420-6a3d-4d06-879c-e4d251b8082e" />

## What It Does

- Discovers Claude CLI sessions from `~/.claude/sessions/*.json`.
- Reads Claude project journals from `~/.claude/projects`.
- Discovers Codex work from `~/.codex/process_manager/chat_processes.json`.
- Reads recent Codex threads from `~/.codex/state_5.sqlite`.
- Shows live sessions only when the native process is actually alive.
- Groups recent historical rows by project, while keeping genuinely live sessions distinct.
- Hides stale missing-path sessions from the default overview.
- Shows repo, branch, dirty files, PID, CPU, memory, model, token totals, and recent activity where available.
- Provides a focused tail view with normalized user, assistant, thinking, tool, result, and usage events.
- In the tail view, code edits render as inline syntax-highlighted diffs with line numbers.

## Install

Requirements:

- Rust and Cargo
- macOS or another Unix-like system with `kill`, `lsof`, and `git`

From this repo:

```bash
./scripts/install.sh
```

By default, this builds a release binary and installs it to:

```bash
~/.local/bin/aitop
```

If `~/.local/bin` is not on your `PATH`, add this to your shell profile:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

Choose another install directory:

```bash
AITOP_INSTALL_DIR=/some/bin ./scripts/install.sh
```

## Usage

Open the dashboard:

```bash
aitop
```

Print one text snapshot:

```bash
aitop --once
```

Run with simulated demo data:

```bash
aitop --demo
aitop --once --demo
```

## Controls

Monitor view:

- `up` / `down` or `j` / `k`: select a session
- `enter`: open the focused tail view
- `s`: open the stream view (cross-project activity preview)
- `v`: toggle top panel (graph/timeline)
- `a`: cycle overview, active, and all views
- `r`: refresh
- `q`: quit

Tail view (opens at bottom and follows new events):

- `j`: scroll down (only when not following)
- `k`: scroll up (freezes auto-follow)
- `page up` / `page down`: scroll by larger jumps (freeze auto-follow)
- `g`: jump to top (frozen)
- `G`: jump to bottom and resume auto-follow
- `up` / `down`: select another session (resets to auto-follow)
- `esc`: return to monitor
- `a`: return to monitor and cycle views
- `q`: quit

Stream view (cross-project activity):

- `j` / `k`: select event (k freezes auto-follow)
- `enter` / `→`: expand selected event (show diff, result, or text)
- `←`: collapse selected event
- `p`: cycle project filter
- `e`: toggle errors-only mode
- `G`: jump to bottom and resume auto-follow
- `g`: jump to top
- `page up` / `page down`: scroll by larger jumps (freeze auto-follow)
- `esc`: return to monitor
- `q`: quit

## Development

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo run -- --once
cargo run
```

## License

`aitop` is released under the MIT License. See [LICENSE](LICENSE).
