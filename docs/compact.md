# Context Compaction

Codape compacts client-side: it rewrites the message list before a request is
sent. Compaction thresholds are derived from the model's context window, so
resolving that window correctly is the difference between compacting early and
hitting a hard context-length error.

## Context Window Resolution

`codape_core::types::ContextWindowResolver` resolves a window from an ordered
chain. The first source that yields a positive value wins.

| Priority | Source | How it is supplied |
|---|---|---|
| 1 (highest) | Environment | `CODAPE_CONTEXT_WINDOW`, `CODAPE_CONTEXT_WINDOW_<SANITIZED_MODEL>` |
| 2 | Settings | `context_window` in `settings.json` |
| 3 | Client data | `~/.codape/context_windows.json` (or `$CLAUDE_CONFIG_DIR/context_windows.json`) |
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
export CODAPE_CONTEXT_WINDOW=128000
```

The per-model form appends the sanitized model name:

```sh
# claude-sonnet-4-6 -> CLAUDE_SONNET_4_6
export CODAPE_CONTEXT_WINDOW_CLAUDE_SONNET_4_6=200000

# my-gateway-alias -> MY_GATEWAY_ALIAS
export CODAPE_CONTEXT_WINDOW_MY_GATEWAY_ALIAS=128000
```

Sanitization upper-cases the name and replaces every non-alphanumeric character
with `_`. Lookup tries the raw name, the lower-cased name, and a normalized name
with common provider/router prefixes removed, so
`CODAPE_CONTEXT_WINDOW_CLAUDE_SONNET_4_6` also matches
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

`~/.codape/context_windows.json` uses the same shape. The config directory is
`$CLAUDE_CONFIG_DIR` when set, otherwise `~/.codape`:

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

Codape writes to this file itself when a provider states a real window in an
overflow error, and only then — it never rewrites values it merely read. See
[Learning the window from the provider](#learning-the-window-from-the-provider).

### Experiment / Feature Flag

Context-window experiments use the existing `FeatureFlags` mechanism. The
`context_window_overrides` flag gates participation; values are supplied by
experiment configuration (programmatically through `FeatureFlags`, or through
`CODAPE_FEATURE_FLAGS` for local testing):

```sh
export CODAPE_FEATURE_FLAGS="context_window_overrides,context_window_default=64000,context_window=my-gateway:128000"
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
`context_length_exceeded`, …). `codape_core::context_overflow::detect` parses
those messages, and the engine adopts the reported size for the rest of the
session (`QueryEngine::context_window`, `learn_context_window`).

An overflow is not a fatal error: the turn learns the real window, compacts,
and re-sends once. The request is retried at most twice, and only after a
compaction that actually removed something — retrying an unchanged request
fails the same way. `ErrorClass::ContextOverflow` is never auto-retried by the
transport; the recovery belongs to the turn.

**The learned value is also written to the client-data file**
(`~/.codape/context_windows.json`, or `$CLAUDE_CONFIG_DIR/context_windows.json`),
keyed by the model name, so the next session starts knowing it instead of
being rejected once to learn it again. That file is the right home for the
value: it is the source whose purpose is facts learned about an endpoint, it is
not a file anyone maintains by hand, and it sits *below* settings and the
environment in the resolution chain above — so an explicit window in either
still wins, and nothing the user wrote is ever edited. The write is
merge-in-place (other models keep their entries), skips a write when the value
is already recorded, and goes through a temporary file and a rename so a
half-written file can never be read. A failed write is a warning, never an
error: the window still applies for the rest of the session.

`QueryEngine::set_persist_learned_context_window` controls this. It is off for
an engine built directly (tests and embedders) and on for the sessions the CLI
runs, so a test suite cannot change the window a real session starts with.

**An accepted request is also evidence.** Before any overflow, the only fact
about the window is the provider's own acceptance rule: a request is sent only
when `input + max_tokens` fits. Every accepted response therefore raises the
learned value to at least `prompt_tokens + max_tokens`
(`QueryEngine::raise_context_window_lower_bound`). That matters for a gateway
alias the table does not know: the 32 000-token placeholder can be four times
too small, and a session that never overflows a 128K model would otherwise
compact against a 16K trigger for its whole life — the failure a ProgramBench
run showed as 241 passes across 393 turns. The bound only raises the value, so
a configured window larger than the bound stays in charge; one the bound
overtakes was demonstrably too small. It stays on the safe side of the
estimate: the bound is at most the real window, so compaction still errs
early, never late. It is remembered through the same client-data entry as a
value an overflow stated.

Because the value persists, a provider that states a misleading number makes it
sticky for that model until the file or an override says otherwise. That is why
the write goes to the *lowest*-priority remembered source: `CODAPE_CONTEXT_WINDOW`,
a `context_window` block in settings, or deleting the entry all take effect
immediately, and a wrong entry is correctable by hand rather than only by
another rejected request.

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

`auto` is the **trigger** and `warn` is the **landing line**
(`CompactThresholds::landing`). A pass is triggered at `auto` but aimed at
`warn`: landing exactly on the trigger buys no headroom, and the next tool
result crosses the line again. Until the landing line existed, a pass could
"meet" its target by removing a few hundred tokens and still count as a
recovery.

Both `auto` and `warn` have small-window floors so a 32K unknown model still
compacts sensibly rather than reserving the whole window.

The reserve is what keeps a request sent from the `auto` line acceptable: the
provider rejects `input + max_tokens > window`, and `window - auto` is
`output_reserve + trigger_buffer` — 33 000 tokens by default, just above
`DEFAULT_MAX_TOKENS`'s 32 768. Raising the per-request ceiling above that
(`CODAPE_MAX_TOKENS`) buys output room at the cost of a possible rejection at
the line, which the session recovers from by compacting and retrying. See
[`docs/config.md`](config.md) for the variable and its clamp.

The same arithmetic decides whether a turn that produced nothing can be
re-asked. A `max_tokens` stop with no text and no tool call is re-sent once at
a doubled ceiling, but only when `input + 2 × max_tokens` fits inside the
window *and* the doubled ceiling stays at or below half the window — above
half, the `auto` line itself would sit below the request, so compaction could
not clear a path for it. Both bounds fail together at the default 32 768 on any
window up to 64 K, so a session that never sets `CODAPE_MAX_TOKENS` never
issues the second request. The reasoning is the same as the clamp's: a ceiling
the provider will refuse is not a larger ceiling.

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

## Measuring the Context

The lines above are compared against a number of tokens, and that number is
the only unobserved input in the decision. Counting characters and dividing by
four is close for English prose and badly wrong elsewhere: a Chinese character
is roughly one token but three UTF-8 bytes (so the heuristic undercounts it
about four-fold), and a JSON tool result is denser than prose. Under a
2–3× error the compaction line can be crossed long after the provider's window
was actually spent.

The provider already measures the exact request it received, so the engine
anchors on that. Each request records the shape of the history it carries, and
the response's usage anchors the estimate to the provider's count:

```
context_tokens = measured_prompt_tokens
               + estimate(messages appended since the measurement)
               + max(0, prefix growth since the measurement)
```

The anchor lives on the engine rather than the turn: a new turn's history
begins with the messages the measurement covered, so the same measurement
still describes its prefix. That is the case that matters — the request that
overflows is the one after the measurement, not the one that produced it.

Not all of the request is messages. The serialized tool schemas ride in a
separate `tools` array that the API client builds from the registry;
`build_context` never puts them in `messages`, so a message-only estimate
described a request several thousand tokens smaller than the one the provider
counted — on every request of the session, and worst on exactly the
tool-heavy agentic traffic that decides when compaction fires.
`TurnRuntime::tool_schema_tokens` estimates them from the same `ToolInfo`
values and the same JSON shape the client serializes, and
`context_prefix_tokens` adds them to the prefix, so the whole-request count
(the trigger, the progress line, the targets) covers them. A provider
measurement covers them too: an anchored estimate adds only growth since the
measurement, so the schemas are never counted twice.

`measured_prompt_tokens` is `input_tokens + cache_read_input_tokens +
cache_creation_input_tokens`: Anthropic reports only the uncached remainder in
`input_tokens`, while OpenAI-compatible endpoints put everything in
`prompt_tokens` and leave the cache fields at zero, so the sum is the request
in both cases (`UsageSummary::prompt_tokens`).

Two reports are ignored rather than believed: a zero count, and a count below
the system prompt the request definitely carried. Both describe something
other than the whole request, and anchoring on either would make the context
look permanently small. A compaction that replaces the measured messages also
invalidates the anchor, because the measurement no longer describes what is
there.

### One ruler for the decision

The thresholds are stated in provider tokens, so everything that *compares* a
context to them is computed from the measurement: the level (`Ok` / `Warn` /
`Compact` / `Blocked`), the triggered trigger's strategy, and the progress
line a pass is judged against. The rule-based strategies (`Auto`, `Snip`) count
`chars / 4` internally, and their targets are provider-token budgets. On
JSON- and code-heavy tool traffic the provider's tokenizer reports 30–40 % more
than that heuristic, so a budget handed over unchanged is one the history may
already meet on its own ruler: the pass removes nothing and spends a recovery
attempt on a no-op. A debug run of `gpt2-codegolf` crossed the line at 99 242
measured tokens (72 389 by the heuristic), selected `Micro` because the
heuristic still looked like `Ok`, and repeated passes whose 80 000-token
`Snip` target the history already met.

Every provider-token budget is therefore translated into heuristic units
before a strategy sees it:

```
heuristic_budget = budget × heuristic / measured     (when measured > heuristic)
heuristic_budget = budget                            (otherwise)
```

The line and the targets are re-derived on every pass, because the ratio is a
property of the traffic, not of the model. A heuristic at or above the
measurement is not evidence of a larger real context — the heuristic overcounts
prose — so the budget is left as stated in that case.

A second translation follows the first: the budget is stated for the whole
request, but a rule-based strategy can only trim the *history*. Everything
that is not history — the system prompt, the preamble, the per-turn
injections, the tool schemas and the request scaffolding — is subtracted from
the translated target, with a floor of a quarter of the history so a pass
never deletes working state to meet a number. Without the subtraction a pass
could "reach" a whole-request target while removing nothing from history at
all.

What is subtracted is the *measured* overhead when a provider count exists:
`UsageAnchor::fixed_tokens` is `prompt_tokens` minus the estimated history of
the request that count describes. The message-level prefix alone misses the
tool schemas and the scaffolding, and a target translated against the smaller
number asks history for room the fixed part has already spent. Without a
measurement the prefix is the best available estimate.

The progress line is the translated landing line, not the trigger. A result
that lands exactly on `auto` has not recovered the context and is not counted
as progress, even though the raw heuristic may call it a success.

### When no pass can reach the line

If the fixed part alone fills the landing line, no history rewrite can get the
request under it: even a pass that kept a quarter of the history would leave
the request above the line. That is a property of the request, not of the
strategy, so every rung of the ladder would fail identically — and the next
turn would re-derive the same target and fail again. `CompactOutcome::Uncompactable`
reports the situation instead of running the arithmetic out; it carries the
fixed overhead, the landing line and the smallest request a pass could leave.

It is a report, not a failure. Above the `blocked` line the request still goes
out, so the turn continues; only `blocked` ends the turn. It is not reported at
the `blocked` line either, where a heavy pass that leaves the history above
the landing line can still bring the request below the blocked one — skipping
there would fail a recoverable turn. The same applies after a provider
rejection: an overflow proves the request does not fit, so compaction is not
speculative and runs whatever the arithmetic says.

## Background Summary

Compaction's visible cost is latency: a turn that crosses the line cannot send
until a summarization round-trip finishes. The warning line is the early
signal, and the interval a summary would cover is known there and only grows,
so the summary is generated on a background task as soon as the context is
past `warn`.

The result is kept with the interval it summarized. When compaction later
needs a summary, it is used only if its end still falls inside the interval
about to be summarized — the messages after that point are kept verbatim
rather than elided unseen, so a summary made early never claims to cover more
than it read. If the preserved head moved (a new first user message, or a
different `keep_recent_messages`), the digest is discarded and the
synchronous path runs.

A failed pre-generation is recorded and not retried: a provider that refused
the summary once will refuse it again a turn later, and the synchronous path
is still there. `CompactConfig::background_summary` (on by default) turns the
whole thing off.

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

A `<context_summary>` message is preserved the same way the head is. It is the
whole record of everything the compaction before it elided, so a later pass
that drops it deletes that history a second time with nothing left to
reconstruct it. `Reactive` and `Snip` therefore carry any summary that sits
inside the span they remove across to the compacted side, next to the boundary
marker. Without that, escalating from a summary to `Snip` — the ladder does
exactly that when a summary removes too little — left a boundary marker saying
history had been "summarized" and no summary describing it.

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

The message floor is not a size bound: eight recent messages can hold most of
the context when the tail is tool-heavy. `summary_split_bounded` adds a token
ceiling to the preserved tail — an eighth of the window, clamped between 8 000
and 40 000 tokens — and only ever makes the tail smaller than the message floor
allows. The tail starts at a user message so a tool call is not separated from
its result.

The summarizer's own input has the same problem in reverse. A fixed 20 000-token
cap was sized for 128K windows: on a much larger window it dropped exactly the
task state that lives at the start of the interval, and on a 32K window it was
a quarter of the window. The budget is now a quarter of the window, capped at
60 000 tokens and floored at `CompactConfig::summary_input_max_tokens`
(`resolve_summary_input_budget`). Within that budget `build_summary_input`
keeps the first two renders and the newest one, dropping the largest middle
messages first — dropping strictly oldest deleted the task state that opens the
interval — and never drops the newest render, which is the state the model is
about to continue from.

## Progress and the Circuit Breaker

A compaction attempt counts as progress only when it reaches the landing line
(`warn`) or removes at least a tenth of the context — not merely when the
context shrank, and not when it merely lands on the trigger line (`auto`).
Both judgements are made on the provider-anchored count, with the line
translated into the strategies' heuristic units (see [One ruler for the
decision](#one-ruler-for-the-decision)). An earlier build reported success for
a pass that removed 35 tokens of 20,000 and grew the message list, then spent
every recovery attempt on passes that never approached the line; a later one
judged a pass against the raw heuristic line and called a context that the
provider still measured over it a recovery.

- Progress resets the consecutive-failure counter and the escalation ladder.
  A threshold-triggered pass that removes a meaningful share but leaves the
  context over the line spends one recovery retry. A pass that brings the
  context back under the landing line ends the recovery episode and restores
  the budget: the budget bounds the passes spent returning under the line, not
  the number of times a session may compact. A headless run is one engine turn
  for the whole task, and charging every successful pass made such a run die
  after its third compaction with the message below, with work still to do and
  thousands of tokens of room under the window.
- No progress increments the counter and records the strategy that failed; at
  `max_consecutive_failures` the circuit breaker opens.
- The failed rung survives the turn boundary. A new turn continues from it
  rather than restarting at the strategy the thresholds name, so three turns
  cannot spend the whole breaker on the same ineffective pass.
- A summary that leaves the context above the line and removes less than a
  tenth of it falls through to the rule-based pass at the next rung instead of
  ending the attempt.
- When a threshold-triggered pass does not reach the line, the next attempt
  escalates one rung down the strategy ladder — `Micro` → `Auto` → `Snip` →
  `LastResort` — instead of repeating the pass that just failed. The requested
  rung is recorded separately from the pass that actually ran: an `Auto`
  request that stopped after its micro step used to report `Micro`, so the
  ladder selected `Auto` again and `Snip` was unreachable.
- `LastResort` is local and free: it runs `Snip`, then repeatedly truncates
  the largest remaining tool result (head and tail kept) until the target is
  met or nothing can shrink. It makes no model call and cannot fail the way a
  summary can.
- A cache-TTL pass is maintenance. It is never charged to the recovery budget
  and never touches the failure counter or the escalation ladder — a no-op
  maintenance pass has no bearing on whether the summarizer is failing. A pass
  that changes nothing does not replace the history, rewrite the transcript or
  reset the loop detector.
- A pass that removed nothing is not repeated at the same size. Retrying it
  every turn spent a whole two-hour benchmark run on compaction — 241 passes,
  73 of them no-progress, no source written — because each turn re-crossed the
  line and re-derived an unreachable target. After a no-progress threshold
  pass, the next attempt waits until the context is `blocked` (where a request
  cannot be sent at all) or has grown by a twentieth of the auto line (at
  least 2 000 tokens). The wait is reported as `BackedOff` with the size at
  which a retry becomes worth it. A cache-TTL no-op neither sets nor clears
  this state: a maintenance pass below the line says nothing about whether a
  threshold pass can work.

An open breaker, or a spent recovery budget, no longer pauses compaction at the
token line. Both are stop signals for the *named* strategies, and they select
`LastResort` instead. Refusing to try turned a context 357 tokens over the
`blocked` line — after two hours and 125 productive turns of a figlet run —
into a failed run; the last resort would have removed a tool result and
continued. A `blocked` context is therefore never fatal before that attempt.
Once the attempt has run and the context is still `blocked`, the turn stops
rather than sending a request that cannot fit. The dead-loop Stop hook is not
allowed to pre-empt the forced last resort either: it exists to stop paid
retries, not the free attempt the exhausted budget leaves available.

The cache-TTL path is the exception: with the breaker open it still returns
`CircuitBreakerOpen` rather than running another named pass, because a
maintenance tidy-up is not worth a model call when the summarizer has been
failing.

Each pass reports what it was asked to do and what it actually did. The
compaction event carries the trigger, the requested and effective strategies,
the sizes before and after, the landing target, the non-compactable prefix and
the tokens removed; `NoProgress` carries the same strategy fields. Two
outcomes report a pass that did not run and why: `BackedOff` (the size, the
last failed size, and the size at which a retry becomes worth it) and
`Uncompactable` (the fixed overhead, the landing line, and the smallest
request a pass could leave). That is enough for a replay to explain a run
without the engine's log file.
