# Surfaces

A **surface** is the product a session is presented in: the terminal, an IDE
panel, a desktop window, a web page, a chat product. It is not a widget toolkit
and not a rendering API. The abstraction in `codape-core::surface` answers one
question — *which front end is showing this session, and what can it present?* —
and gives the engine, the mod runtime and every host the same answer.

Codape ships **one** surface implementation, the terminal. The abstraction
exists so that adding a second one is a host-side project, not a fork of the
engine or the mod runtime.

## The types

| Type | Meaning |
| --- | --- |
| `SurfaceId` | Validated stable name (`[a-z0-9_-]`, at most 32 characters). `SurfaceId::TERMINAL` is `"terminal"`. This is what a mod sees as `e.surface`. |
| `SurfaceCapabilities` | `{ interactive, mouse, images }`. The terminal constant is `{ true, true, false }` (the floor for a surface nobody probed); a terminal with truecolor reports `images: true`. `TEXT_ONLY` is `{ false, false, false }`. Mods see it as `e.surfaceCapabilities`. |
| `SurfaceDescriptor` | `SurfaceId` + `SurfaceCapabilities`. The host passes it to the runtime; `SurfaceDescriptor::terminal()` is the default. |
| `UiSurface` | The trait a host implements: `descriptor()` and `render_text(elements, width)`. |
| `SurfaceRegistry` | The surfaces a host can present, plus the active one. Registering does not change the active surface; `set_active` does. The first registration becomes active. |

`render_text` is the lowest-common-denominator presentation used by headless
transcripts, wire clients and tests. A rich surface keeps it as a fallback and
renders `UiElement`s natively for its own medium.

## How a mod contribution reaches a surface

```
mod hook returns $.ui.render(...)
        │
        ▼
HookFireResult.ui ──► EngineEvent::ModUi(Vec<UiElement>)
        │
        ├─ in-process host: the active surface renders the tree
        │     terminal: ratatui block + hit regions (TUI), or render_text (headless)
        │
        └─ out-of-process host:
              core::wire_protocol  → WireEvent::ModUi { elements }
              stream-json          → {"type":"system","subtype":"mod_ui","elements":[...]}
```

Activation is surface-agnostic: a surface delivers the button `id` to
`ModUiHandle::activate(id)`, which runs the mod's `$.ui.onActivate` handler and
returns the elements to render in its place. The terminal routes both `Enter`
and mouse clicks through it.

A mod has a second, unrelated contribution path. `$.ui.panel(slot, …)` does
not render a block: it names a slot the host keeps, so the surface holds the
tree until the mod replaces or clears it.

```
mod hook returns $.ui.panel(slot, ...) / $.ui.clearPanel(slot)
        │
        ▼
HookFireResult.panel_updates ──► EngineEvent::ModPanel(Vec<ModPanelUpdate>)
        │
        ├─ in-process host: the active surface applies them to its own slots
        │     terminal: codape_ui::panel::PanelStore, drawn in its own band
        │
        └─ out-of-process host:
              core::wire_protocol  → WireEvent::ModPanel { updates }
              stream-json          → {"type":"system","subtype":"mod_panel","updates":[...]}
```

The operations (`set` / `clear`, each naming its slot) are what cross the
boundary, so a host can hold the same slots across turns and no mod can erase
another's panel by rendering its own. The store is plain data by design: it
owns which slots exist and in what order, and the drawing belongs to whichever
surface shows it.

The descriptor also travels to mods: `$.session.surface` is the active id and
`$.session.surfaceCapabilities` is the capability object. A mod that renders a
button can check `interactive` first, and a mod that changes behaviour per
surface branches on the id — with no assumption that the surface is the
terminal.

## What is implemented

- `codape-core::surface` — the ids, capabilities, descriptor, trait and
  registry, with tests for validation, duplicate registration and selection.
- `codape-ui::surface::TerminalSurface` — the terminal implementation. It
  reports `interactive: true, mouse: true`, takes `images` from the live
  terminal (`TerminalSurface::new` asks the same `ImageProtocol` detection the
  transcript uses, so the capability and the renderer cannot disagree), and
  renders through the same text renderer the headless transcript uses.
  `TerminalSurface::with_images(bool)` is the pure seam for a caller that
  already knows.
- The CLI registers exactly that surface when it builds the mod session, and
  passes `active_descriptor()` into `ModSession`.
- `core::wire_protocol` and the stream-json protocol both carry mod UI, so a
  surface outside this process receives the same tree.
- Mod panels carry the same way: `WireEvent::ModPanel` and the `mod_panel`
  frame. The terminal draws them from `codape_ui::panel::PanelStore` in a band
  between the transcript and the prompt; a headless run ignores them, having no
  surface to hold a slot.

Only the terminal is registered as a `UiSurface`. An editor reaches a running
session through a different path — `codape ide bridge`, a stdio MCP server that
talks to the daemon over WebSocket (see [mcp.md](mcp.md#reaching-a-running-session-from-an-editor)).
That path carries text tool calls, not the mod element tree, so an IDE host for
`UiSurface` is still unbuilt: a surface that renders `UiElement`s inside an
editor needs an implementation this repository does not have.

## Adding a surface

1. Implement `UiSurface` for the host's type — `descriptor()` and
   `render_text()`; render `UiElement`s with the host's own widgets.
2. Register it in a `SurfaceRegistry` and `set_active` when that host is the
   one presenting the session.
3. Pass `active_descriptor()` as `ModSession.surface`.
4. Route button activation to `ModUiHandle::activate`.
5. For an out-of-process host, consume `WireEvent::ModUi` (or the stream-json
   `system`/`mod_ui` frame) and send activation back through the same protocol
   the host already uses for control requests.
6. If the host has somewhere to keep a panel, consume `WireEvent::ModPanel`
   (the `system`/`mod_panel` frame) and apply each `set`/`clear` to a slot of
   its own. Skipping this step is fine — panels are the one contribution a host
   may decline without losing anything else.

No change is needed in the engine, `crates/mods`, or the element schema. The
element tree is the shared contract; the surface decides how it looks.

## Boundaries

- `images` means "this surface can put a picture in front of a person", and the
  terminal answers it from what it can actually draw: truecolor gets `true`
  (the transcript draws attachments as half-block cells — `--image`,
  `@shot.png`, Ctrl+V), a terminal without it gets `false`. The *element tree*
  still has no image element, so a mod cannot put a picture into a `UiElement`
  on any surface; the flag describes the surface, not the mod API. See
  [ui.md](ui.md#pictures-in-the-transcript).
- A surface that reports `interactive: false` still receives hooks and UI; it is
  the host's job to hide affordances it cannot honour.
- The registry is in-memory and per session. Persisting a preference for a
  surface is a host concern, not part of this contract.
