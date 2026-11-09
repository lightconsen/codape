# Codape

**Codape** is an AI-powered coding assistant for the terminal. It connects to large language models (Claude, OpenAI-compatible APIs) and provides an interactive REPL for code generation, file editing, shell commands, code search, web research, and multi-turn conversation — all from your command line.

Built in Rust for performance, reliability, and cross-platform support.
Project homepage: [codape.com](https://codape.com).

> Codape is an independent project. It is not affiliated with, endorsed by,
> or sponsored by Anthropic; "Claude" refers to the models it can connect to.

## Features

- **Interactive REPL** — ratatui-based terminal UI with Vim mode, syntax highlighting, themes, and overlays
- **LLM Integration** — Supports Anthropic Claude and OpenAI-compatible API providers
- **Tool Execution** — 45+ built-in tools for reading, writing, searching, executing code, and browsing the web
- **Slash Commands** — 50+ built-in commands (`/commit`, `/compact`, `/review`, `/bridge`, and more)
- **Sub-Agents** — Specialized agents (Explore, Plan, Verification) that can be spawned for parallel tasks
- **Session Memory** — Persistent transcripts, long-term memory with fuzzy search, and team memory sync
- **MCP Support** — Model Context Protocol for dynamically discovered tools
- **Plugin System** — WASM and process-based plugins with marketplace
- **Remote Sessions** — Bridge/SSH for working on remote workspaces
- **Server Mode** — WebSocket daemon for editor/IDE clients built on the SDK, and headless consumers
- **Permission System** — Granular permission control with platform-native sandboxing
- **Enterprise** — Remote managed settings, org-scoped team memory, security audits
- **Cross-Platform** — Linux, macOS, and Windows

## Quick Start

### Installation

**From source:**

```bash
git clone https://github.com/lightconsen/codape.git
cd codape
cargo build --release
./target/release/codape
```

Or use the helper script, which checks the toolchain and the platform
dependencies (C compiler, pkg-config, dbus, lld) before building. It builds
the optimized profile by default and runs `cargo clean` first; use `--debug`
for a faster compile and `--no-clean` to keep the build cache:

```bash
scripts/build.sh
```

**From GitHub Releases:** download the archive for your platform from
[github.com/lightconsen/codape/releases](https://github.com/lightconsen/codape/releases)
(Linux x86_64 gnu/musl, macOS x86_64/aarch64, Windows x86_64).

**With the install script** (Linux/macOS; installs to `~/.local/bin` or
`/usr/local/bin`). It verifies the download against the release's
`SHA256SUMS` and refuses to install when the file is missing or does not
match:

```bash
curl -fsSL https://raw.githubusercontent.com/lightconsen/codape/main/scripts/install.sh | sh
```

**Keeping it current:** `codape update --check` reports whether a newer
release exists; `codape update` downloads it, verifies it the same way, and
replaces the binary atomically. Installations owned by Homebrew, Nix or
`cargo install` are refused with the command that upgrades them instead. See
[`docs/self-update.md`](docs/self-update.md).

**With Cargo:**

```bash
cargo install --git https://github.com/lightconsen/codape.git
```

### Usage

```bash
# Start interactive REPL
codape

# Single prompt (non-interactive)
codape "write a fibonacci function in Python"

# Continue last session
codape -c

# Specify model
codape -m claude-sonnet-4-6

# Server/daemon mode
codape serve --port 8080

# With sub-agents
codape --agent explore "find all API route definitions"
```

## Architecture

Codape is organized as a Rust workspace with 19 crates (plus the `evals` harness):

| Crate | Responsibility |
|-------|--------------|
| `cli` | Binary entry point, startup orchestrator, REPL, server |
| `core` | QueryEngine, turn state machine, permission engine, API client |
| `tools` | Tool trait, registry, executor, 44 baseline + 6 feature-gated tools |
| `commands` | Slash command registry, 76 commands |
| `ui` | ratatui terminal UI, statusline, Vim mode, themes |
| `memory` | Transcript storage, session index, long-term memory |
| `agents` | Sub-agent manager, spawn/fork/send message |
| `mcp` | MCP server lifecycle, tool/resource discovery, OAuth |
| `plugins` | Plugin manager, marketplace, manifest loading |
| `bridge` | Remote session orchestration, transport, heartbeat |
| `server` | WebSocket daemon, session auth, idle cleanup |
| `hooks` | Hook event system (22+ events), sync/async execution |
| `config` | Settings loading/merging, feature flags, enterprise |
| `events` | Line-delimited stream-json protocol frames and session control messages (there is no in-process bus) |
| `telemetry` | Crash reporting and usage analytics |
| `artifacts` | Local artifact store, ArtifactDB and the artifact tool family |
| `insights` | Session insight extraction, aggregation and offline HTML report |
| `workflow` | Deterministic JavaScript workflow runtime and the Workflow tool |
| `test-utils` | Test rigs, snapshot helpers, wire client |

### Data Flow

```
User Input → REPL/UI → QueryEngine → Provider API
                                        ↓
                              [SSE Stream / Tool Calls]
                                        ↓
                              ToolExecutor → Read/Write/Bash/etc.
                                        ↓
                              Results → Finalize → Output
```

## Configuration

Settings are loaded from multiple sources (in order of precedence):
1. CLI flags and environment variables
2. Project-local `.codape/settings.toml`
3. User-level `~/.config/codape/settings.toml`
4. Remote managed settings (enterprise)

## Development

### Prerequisites

- Rust 1.85+ (stable toolchain)
- Linux, macOS, or Windows

### Build

```bash
# Core feature set (default)
cargo build --workspace

# Release build with optimizations
cargo build --release

# Feature tiers
cargo build --features core         # QueryEngine + baseline tools
cargo build --features full         # core + bridge/server/plugins
cargo build --features enterprise   # full + managed settings/team
```

### Test

```bash
# Unit tests
cargo test --workspace --lib

# All tests
cargo test --workspace

# Specific crate
cargo test -p codape-core

# Provider-backed E2E tests (replay recorded LLM cassettes)
cargo test -p codape-cli --test e2e_code_agent
```

### Lint

```bash
cargo fmt --all
cargo clippy --workspace -- -D warnings
```

## Documentation

- [Architecture](docs/arch.md)
- [CLI Reference](docs/cli.md)
- [Specification](docs/spec.md) (Chinese)
- [Core Engine](docs/core.md)
- [Tools](docs/tools.md)
- [Commands](docs/commands.md)
- [Telemetry and privacy](docs/telemetry.md)

## License

Apache License 2.0 — see [LICENSE](LICENSE).
