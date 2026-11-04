# Context Compaction

CodeRex compacts client-side: it rewrites the message list before a request is
sent. Compaction thresholds are derived from the model's context window, so
resolving that window correctly is the difference between compacting early and
hitting a hard context-length error.

## Context Window Resolution

`coderex_core::types::ContextWindowResolver` resolves a window from an ordered
chain. The first source that yields a positive value wins.

| Priority | Source | How it is supplied |
|---|---|---|
| 1 (highest) | Environment | `CODEREX_CONTEXT_WINDOW`, `CODEREX_CONTEXT_WINDOW_<SANITIZED_MODEL>` |
| 2 | Settings | `context_window` in `settings.json` |
| 3 | Client data | `~/.coderex/context_windows.json` (or `$CLAUDE_CONFIG_DIR/context_windows.json`) |
| 4 | Experiment / feature flag | `FeatureFlags` context-window values |
| 5 | Built-in model table | `MODEL_CONTEXT_WINDOWS` |
| 6 (lowest) | Unknown floor | `UNKNOWN_CONTEXT_WINDOW` (32,000) |

Source order is authoritative: a global default in a higher-priority source
beats a per-model entry in a lower-priority source. Within a single source, a
per-model value beats that source's global default.

Invalid values (non-numeric, zero, blank model names) are dropped at each level,
so resolution falls through to the next source rather than disabling
compaction. A window of zero is never used.

### Environment

The global environment variable applies to every model:

```sh
export CODEREX_CONTEXT_WINDOW=128000
```

The per-model form appends the sanitized model name:

```sh
# claude-sonnet-4-6 -> CLAUDE_SONNET_4_6
export CODEREX_CONTEXT_WINDOW_CLAUDE_SONNET_4_6=200000

# my-gateway-alias -> MY_GATEWAY_ALIAS
export CODEREX_CONTEXT_WINDOW_MY_GATEWAY_ALIAS=128000
```

Sanitization upper-cases the name and replaces every non-alphanumeric character
with `_`. Lookup tries the raw name, the lower-cased name, and a normalized name
with common provider/router prefixes removed, so
`CODEREX_CONTEXT_WINDOW_CLAUDE_SONNET_4_6` also matches
`anthropic/claude-sonnet-4-6`. Sanitization is lossy (`a.b` and `a-b` collide),
so prefer conventional model ids for per-model environment variables.

### Settings

Add a `context_window` object to user, project, or managed settings:

```json
{
  "context_window": {
    "default": 64000,
    "models": {
      "my-gateway-alias": 200000,
      "claude-sonnet-4-6": 200000
    }
  }
}
```

The settings value participates in the normal merge chain (defaults -> user ->
project -> managed). Like every other key, a higher layer replaces the whole
`context_window` value rather than deep-merging it. Zero windows, blank model
names, and non-positive defaults are rejected by settings validation.

### Client Data

`~/.coderex/context_windows.json` uses the same shape. The config directory is
`$CLAUDE_CONFIG_DIR` when set, otherwise `~/.coderex`:

```json
{
  "default": 64000,
  "models": {
    "my-gateway-alias": 128000
  }
}
```

A missing or malformed file is ignored (with a warning for malformed input), so
it can never disable compaction.

### Experiment / Feature Flag

Context-window experiments use the existing `FeatureFlags` mechanism. The
`context_window_overrides` flag gates participation; values are supplied by
experiment configuration (programmatically through `FeatureFlags`, or through
`CODEREX_FEATURE_FLAGS` for local testing):

```sh
export CODEREX_FEATURE_FLAGS="context_window_overrides,context_window_default=64000,context_window=my-gateway:128000"
```

Supported assignment forms:

- `context_window_default=<tokens>` - experiment fallback.
- `context_window=<model>:<tokens>` - one per-model entry; split from the right
  so Bedrock version suffixes such as `-v1:0` stay part of the model name.
- `context_window.<model>=<tokens>` - unambiguous per-model form.

A valid assignment enables `context_window_overrides` automatically. The
`experiment_context_windows()` conversion drops zero and blank entries.

### Built-in Model Table

Known families resolve without configuration. Entries are deliberately
conservative: under-reporting costs some usable context, over-reporting loses
the conversation.

| Family / prefix | Window (tokens) |
|---|---|
| `claude` (all Claude models) | 200,000 |
| `gpt-4o` | 128,000 |
| `gpt-4.1` | 1,000,000 |
| `gpt-4-turbo` | 128,000 |
| `gpt-4` | 8,192 |
| `gpt-3.5-turbo` | 16,000 |
| `o1-mini` | 128,000 |
| `o1`, `o3`, `o4-mini` | 200,000 |
| `gemini-2.5-pro`, `gemini-2.5-flash`, `gemini-3` | 1,000,000 |
| `deepseek-chat`, `deepseek-reasoner`, `deepseek-r1` | 64,000 |
| `llama-3.3`, `llama-3.1` | 128,000 |
| `qwen3`, `qwen2.5` | 128,000 |
| `mistral-large` | 128,000 |
| `k3-256k` | 262,144 |
| `k3` | 1,048,576 |
| `kimi` | 262,144 |
| `glm-5`, `glm-4.6` | 200,000 |
| `glm-4` | 128,000 |
| `minimax-m2`, `minimax` | 204,800 |

Anything else gets `UNKNOWN_CONTEXT_WINDOW` (32,000). Claude is intentionally
kept at 200,000 as a conservative floor across the family; raise a specific
model through one of the override sources if it has a larger window.

### Where the window came from

The resolved value carries a `ContextWindowSource` (`Env`, `Settings`,
`ClientData`, `Experiment`, `Table`, or `Unknown`). The source decides whether
the `blocked` line is enforceable at all: a value someone supplied or
documented is a limit, while `Unknown` is a guess. A guessed window still
compacts early — that is cheap — but never refuses to send a request, because
being wrong about it turns a working session into a failed one. Debug logs
record the source alongside the lines.

### Learning the window from the provider

Every provider enforces the window on its side and says so in its error
(`prompt is too long: 213462 tokens > 200000 maximum`,
`input length and max_tokens exceed context limit: 195000 + 8192 > 200000`,
`context_length_exceeded`, …). `coderex_core::context_overflow::detect` parses
those messages, and the engine adopts the reported size for the rest of the
session (`QueryEngine::context_window`, `learn_context_window`).

An overflow is not a fatal error: the turn learns the real window, compacts,
and re-sends once. The request is retried at most twice, and only after a
compaction that actually removed something — retrying an unchanged request
fails the same way. `ErrorClass::ContextOverflow` is never auto-retried by the
transport; the recovery belongs to the turn.

Matching is exact or family-prefix after normalization. Normalization
lower-cases the name, drops a trailing `[1m]`, and strips provider/router
prefixes such as `models/`, `anthropic/`, `openai/`, `bedrock/`, and regional
`us.` / `eu.` / `apac.` prefixes. Dated variants
(`claude-sonnet-4-6-20260101`, `gpt-4o-2024-08-06`) therefore match their
family.

## Thresholds

`CompactThresholds` turns the resolved window into three lines:

- `auto = window - min(output_reserve, 20,000) - trigger_buffer`
- `warn = auto - min(warning_buffer, auto / 2)`
- `blocked = window - blocked_margin`

Both `auto` and `warn` have small-window floors so a 32K unknown model still
compacts sensibly rather than reserving the whole window.

`CompactThresholds::resolve` and `select_compact_strategy` use the resolver
installed on the engine. `QueryEngine::set_context_window_resolver` installs
one; the CLI builds it from environment, settings, client data, and experiment
flags (`ReplLoop::apply_cli_config`, and the server engine factory), so all four
sources participate in normal sessions. Callers embedding the engine directly
can use `CompactThresholds::resolve_with` and
`select_compact_strategy_with`, or install a resolver on the engine the same
way.

Example: an unknown gateway alias defaults to 32K (auto line 16K). Overriding
it to 64K moves the auto line to 31K, so 20K tokens compact before the override
and do not after it.

## What Is Compacted

`build_context` assembles each request from three parts: the preamble
(CLAUDE.md, injected as a user message), the system prompt, and the session
history. Only the history is compactable. The engine records where it sits in
the request (`TurnRuntime::history_range`), and compaction replaces exactly
that span, leaving the preamble, system prompt, and per-turn injections in
place — they are rebuilt every turn, and rewriting the whole request would both
distort the token estimate and delete the instructions the model runs under.

Compaction also writes the compacted history back to the session. Writing only
to the turn made an automatic compaction last exactly one request: the next
turn rebuilt the full history from the session and undid it.

The preserved head is the first user message of the history — the task prompt.
Preserving "the first user message in the request" instead captured the
CLAUDE.md system-reminder wrapper and dropped the actual request.

## Summary Split

The LLM summary path keeps the head verbatim, summarizes the middle, and keeps
a recent tail. The tail has two floors and starts at the earlier one:

- turn-counted: after `keep_recent_turns` (3) user messages;
- message-counted: back up `keep_recent_messages` (8) messages, then move
  forward to the nearest user message so a tool call is never split from its
  result.

The message floor is what makes the summary path reachable for an agentic
session: those have one user message and hundreds of tool exchanges, so a
turn-counted tail alone covers everything and there is never a range to
summarize.

## Progress and the Circuit Breaker

A compaction attempt counts as progress only when it reaches the `auto` line or
removes at least a tenth of the context — not merely when the context shrank.
An earlier build reported success for a pass that removed 35 tokens of 20,000
and grew the message list, then spent every recovery attempt on passes that
never approached the line.

- Progress resets the consecutive-failure counter, and a
  threshold-triggered pass spends one recovery retry.
- No progress increments the counter; at `max_consecutive_failures` the
  circuit breaker opens and the turn ends with a diagnosable message.
- A cache-TTL pass is maintenance and is never charged to the recovery budget.
- When a threshold-triggered pass does not reach the line, the next attempt
  escalates one rung down the strategy ladder (`Micro` → `Auto` → `Snip`)
  instead of repeating the pass that just failed.

The check is repeated after compaction, and a turn whose context is still past
the `blocked` line — on a window that is actually known — stops rather than
sending a request that cannot fit.
