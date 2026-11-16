# server Module Inspection

## Goal

Daemon server mode — provides WebSocket sessions and structured I/O for headless/SDK consumers. Enables Codape to run as a background service that external tools can connect to.

This crate is the **external protocol**: out-of-process clients (IDEs, web/mobile, scripts) connect here. It is distinct from `codape-events` (`docs/events.md`), which is the **line-delimited stream protocol** on stdout (`--output-format stream-json`) plus session bookkeeping — there is no in-process event bus in either crate.

## Design

### Key Types

- **`DaemonServer`** (`session.rs`) — Main server accepting WebSocket connections with rate limiting and session persistence.
- **`Session`** (`session.rs`) — Per-client session with authentication, activity tracking, and idle timeout.
- **`ServerConfig`** (`session.rs`) — Host, port, auth keys, rate limits, health port, persistence path.
- **`CommandFilter`** (`filter.rs`) — Filters and validates incoming commands by safety level.
- **`SafetyLevel`** (`filter.rs`) — Remote (read-only), Bridge (limited writes).
- **`WsRequest`/`WsResponse`/`WsEvent`** (`session.rs`) — Structured JSON protocol types.
- **`SessionStore`** (`persistence.rs`) — SQLite-backed session persistence.
- **`TokenBucket`** (`session.rs`) — Per-connection rate limiter.

### Architecture Patterns

- **WebSocket per client** — Each connection gets a dedicated session with auth, rate limiting, and message routing.
- **Structured JSON protocol** — Request/response with correlation IDs, typed events, and streaming support.
- **Session persistence** — SQLite storage with restore on server restart. `codape serve` persists to `config_home()/server-sessions.db` by default; `CODAPE_SERVER_PERSIST_PATH` selects another file and `CODAPE_SERVER_NO_PERSIST=1` disables it. A failure to open the store is non-fatal (the server warns and runs in memory).
- **Command filtering** — Safety levels restrict available commands/tools.
- **Health endpoint** — Optional HTTP `/health` on a separate port.
- **Permission and elicitation round-trip** — A turn that needs a person sends
  `permission_request` and waits; `permission_response` answers it through the
  session's `ConfirmationBus`. `permission_recorded` says the answer reached the
  prompt, not that the call was allowed. A client that disconnects has its
  pending prompts denied. Elicitation has the frame shape but no handler: a
  served session never reads the daemon's stdin, so `elicitation_response` is
  refused with `unsupported`. See `docs/server-protocol.md`.
- **Reachability** — The daemon records `{pid, address, token}` in
  `<config_home>/daemon.pid` (mode 0600) once it is listening, because the port
  is chosen at bind time. On loopback with no `CODAPE_API_KEYS` it mints
  `local_<uuid>` and records it, so a local client has something to
  authenticate with; a daemon given an explicit key list records no token.
  `codape ide bridge` is the client that uses this, and `codape ide` prints the
  registration snippet.

## Current Status

The `codape-server` crate has no stubs. Served engines now receive the resolved permission
mode and the classifier/explanation/background-deny hooks plus rule exemptions, and session
persistence is reachable from `codape serve`. Remaining boundary: Windows runtime behaviour
is not verified on this host.
