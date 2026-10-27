# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

**iris** is a live terminal supervisor (TUI) for every active Claude Code session. It reads the transcript files Claude Code writes under `~/.claude/projects/<slug>/<uuid>.jsonl`, tails the active ones, and renders a single dashboard: per-session status, model, tokens, estimated cost, a live activity feed, a tool-usage histogram, opt-in AI summaries, and remote approve/deny of pending tool calls.

The repository also includes `openrouter-agent/`, a TypeScript wrapper around OpenRouter's Node Agent SDK that captures local traces and can optionally forward them to the loopback Iris explorer.

It is published on crates.io as **`iris-tui`** (the binary is `iris`) — single static Rust binary, no daemon, no config beyond an optional approval hook. Releases are tag-driven: bump `Cargo.toml`, add a `CHANGELOG.md` section, push a `v*` tag.

- [README.md](./README.md) is the 1-minute pitch, the key map, and the progress checklist.
- [docs/index.html](./docs/index.html) is the GitHub Pages landing page.

## Build & run

```bash
cargo run                 # live TUI
cargo run -- ls           # one-shot table, no TUI
cargo build --release     # optimized binary (LTO, codegen-units=1, stripped)
cargo install --path .    # install `iris` to ~/.cargo/bin
```

## Architecture

`main.rs` is the CLI entry, arg parser, and the crossterm event loop (all key handling lives there). Modules under `src/`:

- `app.rs` — application state and all the actions the event loop calls (selection, focus, summaries, approvals, key entry).
- `ui.rs` — all ratatui rendering: sessions list, detail pane, modals, status glyphs, token/cost formatting.
- `session.rs` — discovers and parses transcript `.jsonl` files into session models (usage, model, activity, pending tool calls).
- `bridge.rs` — the `PreToolUse` hook bridge, the heartbeat file, and `install-hook` / `uninstall-hook` settings.json editing.
- `anthropic.rs` — Rust network code for AI "doing / done / next" summaries and tool-call risk reads.
- `cost.rs` — per-model USD pricing constants and cost estimation.
- `openrouter-agent/src/index.ts` — Node wrapper that records OpenRouter response streams, tool events, SDK lifecycle hooks, and usage into local JSONL or the Iris loopback event endpoint.

When adding code, match this module split rather than inventing a new structure. Key handling stays in `main.rs`'s `event_loop`; state mutations stay as methods on `App`.

## Non-negotiable product principles

These are the product's contract — any change must respect them:

- **Read-only over transcripts.** iris tails the files Claude Code writes; it never modifies them.
- **Local-first.** The Rust Iris app only makes on-demand Anthropic summary (`s`) and risk-read (`x`) requests. The optional Node wrapper observes OpenRouter model calls explicitly initiated by its host app; trace delivery is local JSONL or loopback-only. No remote telemetry or background network calls.
- **Never hang a session.** The hook checks a heartbeat file; if iris isn't running (stale heartbeat) or gating is disarmed, it must instantly defer to Claude Code's normal permission flow. Never let the hook block on a dashboard that isn't up.
- **Opt-in interception.** Approval gating is off until armed (`A`) and disarms automatically when iris exits.
- **Key safety.** The Anthropic API key is entered in-app (`K`) and written `0600`. Never log it or commit it.

If a task seems to require violating one of these, stop and confirm with the user — they are hard constraints, not defaults.

## Conventions

- Cost figures are **estimates** by design. Pricing lives as editable constants in `cost.rs`; adjust there, don't hardcode numbers elsewhere.
- `Cargo.lock` is committed on purpose — iris is a binary, not a library.

## Parent context

This project lives under `_tooling/` in the Itzenata workspace. The parent [../CLAUDE.md](../CLAUDE.md) describes unrelated standalone scripts in that directory; they share no build system with iris. The sibling `vault-doctor/` project established the repo conventions this project follows (README badges, `.github/ISSUE_TEMPLATE`, GitHub Pages landing under `docs/`, MIT license).
