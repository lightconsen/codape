<pre>
                          ▄
                        ▄███▄
                       ██████▄
                     ▄███▄▄▄███
                   ▄██████████▄
                 ▄███████████▀▀
            ▄▄▄▄█████████████▀▀
          ▄█████████████████
         ███████████████████▄
        ██████████████████████▄
        ████████████▀▀██████████
        ██████████▄   ███████████
       ████▀  █████   ▀███▀ ▀█████
      ████▀    ████    ███     ▀███▄
      ████     ▀████  ▄██        ███
</pre>

# Codape

**Codape is a coding agent for your terminal.** It runs a model against your
repository, decides what to do next, and executes it: reading and editing files,
running shell commands, searching code, driving sub-agents, and carrying the
work forward across turns without you re-explaining the task.

It ships as a single native binary. The default is a local session: the model
talks to the machine you are sitting at, and the workspace you point it at is
the workspace it works in, under a permission and sandbox model you control.
A server mode is available for editor and SDK clients, but it is opt-in — there
is nothing hosted behind this repository.

[codape.com](https://codape.com) · [Releases](https://github.com/lightconsen/codape/releases) · [Docs](docs/) · Apache-2.0

> Codape is an independent project. It is not affiliated with, endorsed by or
> sponsored by Anthropic. "Claude" refers only to the models it can connect to.

## What it does

**It acts.** The agent loop is the product. Codape plans, calls tools, reads the
results and continues until the task is done or it stops for a reason it can
name — rather than emitting a suggestion and handing control back.

**It runs on the models you have.** Anthropic Claude and any
OpenAI-compatible endpoint, configured by environment or settings file, with
model, endpoint and credentials swappable per project.

**It works in the terminal you already use.** A ratatui REPL with Vim
bindings, syntax highlighting and themes; or one-shot non-interactive runs for
scripts and CI; or a server mode for editors and other clients.

| | |
|---|---|
| **Tools** | 54 tool types registered at startup — read, write, edit, glob, grep, shell, web fetch and search, LSP, notebook editing, tasks, cron, MCP, and the Chrome DevTools family |
| **Commands** | 76 slash commands — `/commit`, `/compact`, `/review`, `/diff`, `/resume` and the rest of the daily-driver set (the count is what `/help` prints) |
| **Sub-agents** | Six built-in agents (general, explore, plan, verification, and two setup helpers), plus your own; parallel and background execution in isolated worktrees |
| **Memory** | JSONL transcripts, a searchable session index, and long-term memory that survives across sessions |
| **Extensibility** | MCP servers, WASM and process plugins, hooks on 22 lifecycle events, and an embeddable WebSocket server |
| **Isolation** | Platform-native sandboxing — bubblewrap or Landlock on Linux, Seatbelt on macOS — plus a granular permission engine |
| **Portability** | Linux (gnu and musl), macOS (Intel and Apple silicon), Windows |

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/lightconsen/codape/main/scripts/install.sh | sh
```

The script installs to `~/.local/bin` or `/usr/local/bin` and verifies the
download against the release's `SHA256SUMS`, refusing to install a file that is
missing or does not match.

Other routes:

```bash
# Cargo, from source
cargo install --git https://github.com/lightconsen/codape.git
```

Prebuilt archives for every supported target are on the
[releases page](https://github.com/lightconsen/codape/releases).

`codape update --check` reports whether a newer release exists; `codape update`
installs it with the same verification and replaces the binary atomically.
Installations owned by Homebrew, Nix or `cargo install` are refused, with the
command that upgrades them named instead — see [docs/self-update.md](docs/self-update.md).

## Use

```bash
codape                          # interactive REPL in the current directory
codape "add retry to the upload client"   # one prompt, then exit
codape -c                       # continue the most recent session
codape -m claude-sonnet-4-6     # pick a model
codape serve --port 8080        # WebSocket server for editor and SDK clients
codape --agent reviewer "audit the diff on this branch"   # run as a named custom agent
```

Permissions are the part worth configuring first. Codape asks before it writes
or runs anything, unless you tell it otherwise:

```bash
codape                          # default: ask before anything dangerous
codape --permissions accept-edits   # edits without asking; dangerous tools still prompt
codape --permissions plan           # read-only; no mutations
codape --permissions auto           # default plus a classifier that pre-approves safe calls
codape --dangerously-skip-permissions   # allow everything
```

They are ordered by how much they trust the model. `auto` is not "allow more":
it is `default` with a hook that short-circuits operations a classifier judges
safe, so anything it is unsure about asks exactly as before. Settings files can
also set `permission_mode`, but only user, flag and managed layers may select
`auto` or `bypass-permissions` — a committed project file cannot grant itself
either.

The sandbox is a second, independent layer, and it is on by default: commands
run confined to the workspace, and a command that tries to reach outside it is
stopped by the kernel rather than by a string check. On Linux that is bubblewrap
when installed and Landlock otherwise; macOS uses Seatbelt. `CODAPE_BWRAP=0` and
`CODAPE_LANDLOCK=0` switch a backend off, and `CODAPE_REQUIRE_SANDBOX=1` turns
"no backend available" from a warning into a refusal. Details in
[docs/sandbox.md](docs/sandbox.md) and [docs/core.md](docs/core.md).

## Configure

Settings merge in this order, later layers winning:

1. Defaults
2. User — `~/.codape/settings.json`
3. Project — `.codape/settings.json`
4. Local — `.codape/settings.local.json` (gitignored)
5. Managed — an admin-controlled file or platform policy, applied last

CLI flags and environment variables override the resolved value. Some keys are
deliberately not writable from every layer: a repository-controlled file cannot
turn telemetry on, redirect the provider endpoint that receives your API key, or
select the `auto` / `bypass-permissions` modes. The usual pieces — model, base
URL, permission defaults, hooks, MCP servers, plugin sources — live in that
file. See [docs/config.md](docs/config.md).

## Build

There is no two-line build instruction for this project, and pretending there
is only costs you time when it fails. This workspace has a C toolchain in its
dependency graph, links with `lld` on Linux, needs roughly 8 GB of memory at
peak and tens of gigabytes of disk.

```bash
git clone https://github.com/lightconsen/codape.git
cd codape && scripts/build.sh
```

That script checks the toolchain and platform prerequisites before compiling
anything, then builds the release profile. Everything else — each build route,
the exact prerequisites, what a build costs and how to run the suite — is in
**[docs/build.md](docs/build.md)**.

## Architecture

A Rust workspace of 20 crates plus the `evals` harness. The layout:

```
cli ──▶ core ──▶ provider API
 │       │
 │       ├─▶ tools ──▶ sandbox ──▶ OS
 │       ├─▶ commands
 │       ├─▶ memory
 │       └─▶ agents
 ├─▶ ui ──▶ events ──▶ server ──▶ editor clients
 ├─▶ mcp    ├─▶ plugins    ├─▶ hooks
 └─▶ bridge ──▶ remote sessions
```

`core` holds the query engine, the turn state machine and the permission
engine; `tools` owns the tool trait, registry and executor; `cli` is the binary
and its startup orchestration. [docs/arch.md](docs/arch.md) has the full crate
table and the data flow.

## Documentation

| | |
|---|---|
| [Architecture](docs/arch.md) | Crate layout, engine, data flow |
| [CLI reference](docs/cli.md) | Every flag and subcommand |
| [Configuration](docs/config.md) | Settings files, precedence, environment |
| [Tools](docs/tools.md) | The tool catalogue and protocol |
| [Commands](docs/commands.md) | Slash command reference |
| [Sandbox](docs/sandbox.md) | Backends, guarantees, escapes |
| [Build](docs/build.md) | Prerequisites, routes, resource cost |
| [Permissions](docs/core.md) | Modes and the decision engine |
| [Hooks](docs/hooks.md) | Lifecycle events and hook execution |
| [Sub-agents](docs/agents.md) | Spawning, isolation, messaging |
| [MCP](docs/mcp.md) | Server lifecycle and discovery |
| [Plugins](docs/plugins.md) | WASM and process plugins |
| [Server](docs/server.md) | WebSocket protocol for SDK clients |
| [Verification](docs/verification.md) | The non-`cargo test` gate |
| [Benchmarks](docs/benchmark.md) | Agent benchmark harness |
| [All docs](docs/) | 36 documents |
| [Specification](docs/spec.md) | Full design (Chinese) |

## Contributing

`main` accepts nothing but pull requests, and a change is mergeable only once
the checks in [docs/verification.md](docs/verification.md) pass.
Read [CONTRIBUTING.md](CONTRIBUTING.md) before your first one.

## License

Apache License 2.0 — see [LICENSE](LICENSE). Third-party notices in
[NOTICE](NOTICE).
