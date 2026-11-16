# Server Protocol — WebSocket JSON Protocol

Codape daemon server communicates over WebSocket using a structured JSON protocol.

## Connection

1. Connect to `ws://host:port` (or `wss://` when TLS is enabled).
2. First message must be authentication: `{"token": "your-api-key"}`.
3. Server responds with `{"session_id": "sess_N"}` or `{"error": "..."}`.
4. After authentication, server sends an `event: "session_ready"` message.

### Session role

The authentication message may name the role the session opens as:

```json
{"token": "your-api-key", "role": "bridge"}
```

| Role | Command surface |
|------|-----------------|
| `remote` (default) | read-only shell commands only |
| `bridge` | read-only plus limited filesystem writes |

The role decides what `command` requests (below) can resolve to. It is a
statement by the client, not a credential: it narrows what the session may do,
never widens it, so a client that asks for `remote` gets no less than it
would have anyway.

## Client → Server Requests

Every request has an `id` (string) for correlation and a `type` field:

### Query
```json
{"id": "req_1", "type": "query", "content": "Hello, how are you?"}
```

### Interrupt
```json
{"id": "req_2", "type": "interrupt"}
```

### Ping
```json
{"id": "req_3", "type": "ping"}
```

### Command
```json
{"id": "req_4", "type": "command", "line": "ls -la"}
```

`line` is what the client typed. A line starting with `/` is a slash command
and is resolved against the command registry's availability matrix for the
session's role; anything else is a shell command and is resolved against the
role's command whitelist (read-only for `remote`, read-only plus limited
writes for `bridge`). A command that is not on the role's whitelist does not
resolve: the server answers `command_not_allowed` and the caller has nothing
to run.

The server enforces the whitelist; it does not run the command. Execution
belongs to the session's own tool executor, which the daemon does not own.

### Permission Response

```json
{"id": "perm_7", "type": "permission_response", "decision": "approved"}
```

Answers the `permission_request` whose `request_id` is `perm_7` — **the
request's own `id` is the request id it answers.** `decision` is one of
`approved`, `approve_session`, `denied`. The turn stays paused until this
arrives: a client that cannot prompt should answer `denied` rather than staying
silent, and a client that disconnects has its pending prompts denied.

The answer is delivered, not obeyed — the engine still decides what the call
becomes. The server answers with `permission_recorded` when it reached the
prompt it named, and with a `no_pending_request` error when no prompt is waiting
under that id.

### Elicitation Response

```json
{"id": "elic_3", "type": "elicitation_response", "response": "staging"}
```

Answers an `elicitation_request` the same way. A served session installs no
elicitation handler — the engine cancels the question rather than reading the
daemon's stdin — so this request currently always answers `unsupported`.

## Server → Client Responses

Every response echoes the request `id` and includes a `type` field:

### Query OK (full response)
```json
{"id": "req_1", "type": "query_ok", "content": "I'm doing well, thanks!"}
```

### Query Done (after interrupt)
```json
{"id": "req_2", "type": "query_done", "reason": "interrupted"}
```

`reason` says how the turn finished — `completed`, `interrupted`,
`token_limit_stop`, `nothing running`. A truncated or interrupted answer is not
the same as a finished one, so a client should not treat `query_done` alone as
success.

### Stream Chunk
```json
{"id": "req_1", "type": "stream_chunk", "chunk": "I'm doing "}
```

### Tool Call Start / End
```json
{"id": "req_1", "type": "tool_call_start", "call_id": "tc_1", "name": "Read", "summary": "Reading src/lib.rs..."}
{"id": "req_1", "type": "tool_call_end", "call_id": "tc_1", "result": {"tool_call_id": "tc_1", "output": "...", "is_error": false}}
```

These arrive during the query that requested the tool, correlated by `call_id`.
Without them a client can only show that the model said something, not that it
ran a command.

### Permission Request
```json
{"id": "req_1", "type": "permission_request", "request_id": "perm_7",
 "tool": "Bash", "args": {"command": "rm -rf build"}, "reason": "...",
 "allow_session": true}
```

The `id` is the query the prompt belongs to; `request_id` is what a
`permission_response` has to name. What shapes hold the call is the tool's own
decision: a session with a confirmation channel never reaches the
scheduler's `Ask` branch, so this frame is the only source of prompts.

### Elicitation Request / Notification
```json
{"id": "req_1", "type": "elicitation_request", "request_id": "elic_3", "message": "Which environment?"}
{"type": "notification", "message": "Context compaction moved 12 messages out of the window"}
```

`elicitation_request` is not produced yet (see *Elicitation Response*).
`notification` is advisory and carries no `id`.

### Permission Recorded
```json
{"id": "perm_7", "type": "permission_recorded", "decision": "approved"}
```

The answer reached the prompt it named. It does not mean the call was allowed —
only that the engine heard the decision.

### Pong
```json
{"id": "req_3", "type": "pong"}
```

### Command Resolved
```json
{"id": "req_4", "type": "command_resolved", "command": "ls -la", "description": "allowed by the command whitelist"}
```

### Error
```json
{"id": "req_X", "type": "error", "code": "parse_error", "message": "invalid JSON request: ..."}
```

### Rate Limited (event, no id)
```json
{"event": "rate_limited"}
```

## Server → Client Events

Events are unsolicited messages (not correlated to a request):

### Session Ready
```json
{"event": "session_ready", "session_id": "sess_1"}
```

### Session Closed
```json
{"event": "session_closed", "reason": "idle timeout"}
```

### Idle Warning
```json
{"event": "idle_warning", "message": "Session will expire in 60s"}
```

## Error Codes

| Code | Description |
|------|-------------|
| `parse_error` | Invalid JSON or missing required fields |
| `rate_limited` | Too many messages per minute |
| `auth_required` | First message was not authentication |
| `auth_timeout` | Authentication message timed out |
| `invalid_token` | API key not recognized |
| `max_sessions` | Server at capacity |
| `no_engine` | Query sent to a server started without an engine |
| `query_failed` | The engine could not finish the query |
| `empty_command` | A `command` request with nothing in `line` |
| `command_not_allowed` | The command is not on the session role's whitelist |
| `no_pending_request` | A `permission_response` that names no prompt waiting — no confirmation channel, or the wrong id |
| `unsupported` | The request kind is understood but this session cannot serve it (`elicitation_response`) |

## Health Endpoint

If `health_port` is configured, an HTTP server runs on that port:

```
GET /health
```

Response:
```json
{"status": "ok", "version": "0.1.0", "sessions": 3}
```

## TypeScript Types

```typescript
interface WsRequest {
  id: string;
  type:
    | "query"
    | "interrupt"
    | "ping"
    | "command"
    | "permission_response"
    | "elicitation_response";
  content?: string;
  line?: string;
  decision?: "approved" | "approve_session" | "denied";
  response?: string;
}

interface WsAuth {
  token: string;
  role?: "remote" | "bridge";
}

interface WsResponse {
  id: string;
  type:
    | "query_ok"
    | "stream_chunk"
    | "query_done"
    | "tool_call_start"
    | "tool_call_end"
    | "permission_request"
    | "permission_recorded"
    | "elicitation_request"
    | "notification"
    | "pong"
    | "command_resolved"
    | "error";
  content?: string;
  chunk?: string;
  reason?: string;
  call_id?: string;
  name?: string;
  summary?: string;
  result?: { tool_call_id: string; output: string; is_error: boolean };
  request_id?: string;
  tool?: string;
  args?: unknown;
  allow_session?: boolean;
  decision?: "approved" | "approve_session" | "denied";
  message?: string;
  command?: string;
  description?: string;
  code?: string;
}

interface WsEvent {
  event: "session_ready" | "session_closed" | "idle_warning" | "rate_limited";
  session_id?: string;
  reason?: string;
  message?: string;
}
```

## Example Client (Python)

```python
import asyncio
import json
import websockets

async def main():
    async with websockets.connect("ws://localhost:8080") as ws:
        # Authenticate
        await ws.send(json.dumps({"token": "my-api-key"}))
        resp = json.loads(await ws.recv())
        print(f"Session: {resp}")

        # Wait for session_ready
        ready = json.loads(await ws.recv())
        print(f"Ready: {ready}")

        # Send a query
        await ws.send(json.dumps({
            "id": "q1",
            "type": "query",
            "content": "Hello!"
        }))
        resp = json.loads(await ws.recv())
        print(f"Response: {resp}")

asyncio.run(main())
```
