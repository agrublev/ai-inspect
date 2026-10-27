# iris

> Local-first observability for Claude Code and the OpenRouter Agent SDK: a live terminal supervisor and an interactive trace explorer. **`cargo install iris-tui`** (the binary is `iris`).

[![crates.io](https://img.shields.io/crates/v/iris-tui?logo=rust&color=e6b800)](https://crates.io/crates/iris-tui)
[![CI](https://img.shields.io/github/actions/workflow/status/itzenata/iris-tui/ci.yml?branch=main&label=CI)](https://github.com/itzenata/iris-tui/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/github/license/itzenata/iris-tui?color=blue)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-dea584?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Made for Claude Code](https://img.shields.io/badge/made%20for-Claude%20Code-c678dd)](https://claude.com/claude-code)
[![Stars](https://img.shields.io/github/stars/itzenata/iris-tui?style=social)](https://github.com/itzenata/iris-tui/stargazers)
[![Last commit](https://img.shields.io/github/last-commit/itzenata/iris-tui?color=green)](https://github.com/itzenata/iris-tui/commits/main)

🌐 **Landing page:** [itzenata.github.io/iris-tui](https://itzenata.github.io/iris-tui/)

## What it does

A fast terminal dashboard that watches **all your running Claude Code sessions at once**: what each one is doing right now, its model, tokens and estimated cost, an AI "doing / done / next" summary, and one-key approval of pending tool calls routed from any session into a single pane.

It reads the transcripts Claude Code already writes under `~/.claude/projects/`. **No daemon** is required; install the optional Claude Code hooks to capture lifecycle events and tool boundaries.

**Hard rules:** the Iris app is local-first (only on-demand Anthropic summaries/risk reads make outbound requests), read-only over transcripts, and uses a heartbeat so sessions never hang waiting on Iris. The optional Node wrapper records OpenRouter requests your host app already makes; event forwarding, if enabled, is restricted to loopback.

![iris supervising two live Claude Code sessions: session list on the left; model, cost, token and tool-call breakdown plus the live activity feed on the right](https://raw.githubusercontent.com/itzenata/iris-tui/main/docs/assets/iris-white.png)

## What's working today

A single live pane, refreshed every second:

| Panel | What it shows |
|---|---|
| **Sessions list** | Every session active in the last N minutes, grouped and sorted, color-coded by state |
| **Aggregate cost** | The header totals the estimated USD spend of everything on screen (also in `iris ls`) |
| **Status glyphs** | `⚠` pending approval · `●` running · `✓` done / awaiting you · `○` idle |
| **Per-session meta** | Model (`opus-4-8`, `sonnet`, `haiku`, `fable`), token total, estimated USD cost |
| **Activity feed** | The latest prompt, thinking, tool call, and result of the entered session, tailed live |
| **Tool timeline** | A histogram of which tools a session leans on, so you can spot the one stuck in a build loop |
| **AI summary** | `s` for a Haiku-generated "doing / done / next" briefing of any session |
| **Approval modal** | `⏎` opens the full tool input with an `x` AI risk read; `a`/`d` allow or deny |

**Views & navigation:** vim motions (`j`/`k`, `g`/`G`, `Ctrl-d`/`Ctrl-u`) on both the session list and the activity feed, foldable groups (`space`/`z`), `D` to remove a stale session from the view (the transcript on disk is untouched), and an `ls` subcommand that prints a one-shot table with no TUI.

**Remote approvals:** `iris install-hook` registers a `PreToolUse` hook in `settings.json`. With gating armed (`A`), any session's permission prompt routes into iris: approve or deny it, for one session or a whole group, from one place.

**Trace history:** the hooks capture supported prompt, model, tool, instruction, task, and subagent lifecycle events. Transcript events are also normalized into per-turn traces and correlated tool spans. Everything stays in the local SQLite store at `~/.claude/iris/traces.db` (owner-only directory and 0600 database); nothing is uploaded. The full prompts and tool payloads may contain sensitive data, so protect or remove this file as you would the Claude transcripts.

**OpenRouter Agent SDK:** Node apps using [`@openrouter/agent`](https://www.npmjs.com/package/@openrouter/agent) can use the optional [wrapper](./openrouter-agent/README.md) to capture each response-stream event, model turn, tool call/result, async task, lifecycle hook, and aggregate usage. It writes redacted JSONL locally by default and can optionally forward events to the loopback-only web explorer.

Press `t` in the live TUI to browse recent traces, their chronological event timelines, and each event's full JSON payload. `Tab` changes panes; `j`/`k` move through traces and events or scroll the payload.

For a browser-based explorer with a searchable trace table, status and event filters, run summaries, span hierarchy, event payloads, and evaluation scores, run `iris web` and open the local address it prints (`http://127.0.0.1:4318`). The Datasets page can create datasets, rename them, edit descriptions and examples, and add expected outputs. Prompts can be created and revised as immutable versions; older versions remain unchanged. The CLI workflows below remain available. The server imports existing transcripts on startup, refreshes captured hook data every five seconds, and accepts normalized events from the optional OpenRouter wrapper. It binds to loopback only; the page and assets are served locally with no external fonts, analytics, or telemetry. Prompts, tool payloads, and templates are sensitive, so do not expose the local server beyond a trusted tunnel.

**Stash trace interchange:** the Traces page can import and export [Stash Trace Format](https://www.joinstash.ai/docs/trace-format) JSONL. Each line is validated before the batch is stored; re-importing an existing `id` replaces that trace and its evaluations. Imported conversation steps retain their exact fields, including tool-call links. Exports of native Iris traces include reconstructed conversation steps and the original Iris event records in `metadata.iris_events` so richer hook and runtime details are not discarded. This implements Stash's canonical `stash` JSONL format; the other source-specific adapters listed in Stash's documentation (such as OTLP, Langfuse, and Codex) are not yet import formats.

```bash
iris traces                               # recent turns, newest first
iris trace <trace-id-prefix>              # full event timeline and payloads
iris search "tool input or result text"   # search captured event data
iris web                                  # local browser-based trace explorer
```

**Local evaluation and prompt versions:** manage JSON datasets, add expected outputs, run exact/contains checks against completed traces, add manual quality scores, and keep immutable prompt versions. These commands do not call an LLM or send data over the network:

```bash
iris dataset create smoke "smoke-test examples"
iris dataset add smoke '"hello"' '"hi"'
iris eval run smoke exact
iris evals
iris prompt save greeting "Answer briefly: {{input}}"
iris prompt list
```

Dataset evaluation scores existing captured traces; it does not launch agent runs. Prompt versions are stored for review/use by local workflows; automatic prompt optimization and online LLM-as-judge evaluation are not implemented.

**Cost model:** per-model pricing (input / output / cache-write / cache-read) kept as editable constants in [`src/cost.rs`](./src/cost.rs). Figures are estimates; adjust them to your plan.

## Hard rules

- **Read-only over your data.** iris tails the transcript files Claude Code writes; it never edits them.
- **Local-first.** Iris itself makes only on-demand Anthropic summary / risk requests (`s` / `x`). The separate Node wrapper only observes OpenRouter calls initiated by the host app and can deliver trace events to the local Iris server; it sends no remote telemetry.
- **Never hangs a session.** iris touches a heartbeat file while running. If it's stale (iris not up) or gating is disarmed, the hook instantly defers to Claude Code's normal permission flow, so your sessions are never blocked on a dashboard that isn't there.
- **Opt-in interception.** Approval gating is off until you arm it with `A`, and it disarms automatically when iris exits.
- **Your key, your machine.** The Anthropic API key for summaries is entered in-app (`K`) and saved `0600` in your home directory.

## Install

```bash
cargo install iris-tui   # installs the `iris` binary in ~/.cargo/bin
```

(The crate is `iris-tui` because `iris` is a reserved name on crates.io, but the command you run is `iris`.) Prebuilt Linux and macOS binaries are on the [releases page](https://github.com/itzenata/iris-tui/releases), or build from source with `cargo install --path .` after cloning.

Then:

```bash
iris                     # live dashboard
iris ls                  # one-shot table, no TUI
iris install-hook        # capture lifecycle events; approval gating remains opt-in
iris uninstall-hook      # remove Iris lifecycle hooks
iris traces               # inspect local trace history
iris web                  # open the local trace explorer
```

Single static binary, built with Rust + [ratatui](https://ratatui.rs). Reads `~/.claude/projects/`; override with `-d <path>`.

## Keys

| Key | Action |
|---|---|
| `j` `k` | move between sessions |
| `g` `G` | jump to first / last · `Ctrl-d` `Ctrl-u` half-page |
| `space` `z` | fold a group / fold all |
| `D` | remove the selected session from the view (transcript on disk untouched) |
| `⏎` | open the approval detail (full input + AI risk read), or enter a session's feed |
| `a` `d` | allow / deny the pending tool call (whole group when a header is selected) |
| `s` | AI summary of the selected session (`g` to regenerate) |
| `t` | open the trace explorer (traces, event timeline, full payload) |
| `x` | AI risk read of the pending tool call |
| `i` | open the approval-interception proposal |
| `A` | arm / disarm approval gating |
| `K` | set your Anthropic API key (saved `0600`) |
| `r` | force refresh |
| `q` | quit |

## Progress

- [x] MIT-licensed, single static Rust binary
- [x] [Landing page](https://itzenata.github.io/iris-tui/) on GitHub Pages
- [x] [Issue templates](.github/ISSUE_TEMPLATE) for bugs, features, integration ideas
- [x] Live session discovery + tailing from `~/.claude/projects/`
- [x] Dashboard: status glyphs, model, tokens, estimated cost
- [x] Activity feed with vim navigation and foldable groups
- [x] Tool-usage histogram per session
- [x] AI "doing / done / next" summaries (Haiku)
- [x] `PreToolUse` hook bridge: remote approve / deny from one pane
- [x] AI risk read on a pending tool call
- [x] Heartbeat fallback so sessions never block on iris
- [x] `ls` one-shot table mode
- [x] Loopback web trace explorer with event filters, span hierarchy, and payload detail
- [x] Remove sessions from the view (`D`) without touching transcripts
- [x] Unit tests for the heartbeat and transcript parsing
- [x] CI on every push and PR (clippy, build, test)
- [x] [CONTRIBUTING guide](./CONTRIBUTING.md) + PR template
- [x] crates.io publish metadata in `Cargo.toml`
- [x] 60s demo video + validation post
- [x] Published to crates.io as [`iris-tui`](https://crates.io/crates/iris-tui) + prebuilt binaries on [releases](https://github.com/itzenata/iris-tui/releases)

## Get involved

- ⭐ Star to follow progress
- 💡 [Suggest an integration or signal](https://github.com/itzenata/iris-tui/issues/new?template=integration_suggestion.md)
- 💬 [Open an issue](https://github.com/itzenata/iris-tui/issues/new/choose) for any session-supervision problem you'd want solved
- 🔧 Want to contribute code? Read [CONTRIBUTING.md](./CONTRIBUTING.md): small fixes go straight to PR, features start as an issue

## Star History

<a href="https://www.star-history.com/?repos=itzenata%2Firis-tui&type=date&legend=top-left">
 <picture>
   <source media="(prefers-color-scheme: dark)" srcset="https://api.star-history.com/chart?repos=itzenata/iris-tui&type=date&theme=dark&legend=top-left&sealed_token=q6BDEoKW2M_oldfhlA5PWQM74KooKfmr6jf43ZMo7WQeWbFCCdsoNEufHeN6XjHY_B7hp-Z453_Wj1e-NPNOXwnyi6etvRFL08LF7IJ350hEr-JkxEEetGf97hJqDjFicz9ILkIPD7gWWazOsCztWysK5xeaSvGwwbacLKyAphqM1lM1Z6tOT95x4C0s" />
   <source media="(prefers-color-scheme: light)" srcset="https://api.star-history.com/chart?repos=itzenata/iris-tui&type=date&legend=top-left&sealed_token=q6BDEoKW2M_oldfhlA5PWQM74KooKfmr6jf43ZMo7WQeWbFCCdsoNEufHeN6XjHY_B7hp-Z453_Wj1e-NPNOXwnyi6etvRFL08LF7IJ350hEr-JkxEEetGf97hJqDjFicz9ILkIPD7gWWazOsCztWysK5xeaSvGwwbacLKyAphqM1lM1Z6tOT95x4C0s" />
   <img alt="Star History Chart" src="https://api.star-history.com/chart?repos=itzenata/iris-tui&type=date&legend=top-left&sealed_token=q6BDEoKW2M_oldfhlA5PWQM74KooKfmr6jf43ZMo7WQeWbFCCdsoNEufHeN6XjHY_B7hp-Z453_Wj1e-NPNOXwnyi6etvRFL08LF7IJ350hEr-JkxEEetGf97hJqDjFicz9ILkIPD7gWWazOsCztWysK5xeaSvGwwbacLKyAphqM1lM1Z6tOT95x4C0s" />
 </picture>
</a>

## Development

```bash
cargo run                 # live TUI
cargo run -- ls           # one-shot table
cargo test                # heartbeat + transcript parsing tests
cargo clippy --all-targets -- -D warnings   # CI gates on this
cargo build --release     # optimized binary (LTO, stripped)
npm ci --prefix openrouter-agent
npm test --prefix openrouter-agent
```

CI runs clippy (warnings as errors), build, and tests on every push and PR; see [`.github/workflows/ci.yml`](./.github/workflows/ci.yml). Contribution guidelines live in [CONTRIBUTING.md](./CONTRIBUTING.md).

Code layout: `main.rs` is the CLI entry and event loop; modules under [`src/`](./src) split as `app` (state), `ui` (rendering), `session` (transcript parsing), `bridge` (the hook + heartbeat), `anthropic` (summaries / risk reads), and `cost` (estimation).

License: [MIT](./LICENSE)
