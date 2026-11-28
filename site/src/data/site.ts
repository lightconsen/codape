export const REPO = "https://github.com/lightconsen/codape"
export const DOCS = `${REPO}/tree/main/docs`
export const RELEASES = `${REPO}/releases`

/**
 * The installer is served from this site as well as from raw.githubusercontent,
 * so the command a visitor copies never leaves codape.com. The file itself is
 * copied out of the repository by scripts/sync-install-script.mjs at build time.
 */
export const INSTALL_URL = "https://codape.com/install.sh"
export const INSTALL_CMD = `curl -fsSL ${INSTALL_URL} | sh`
/**
 * The release the site advertises, in the order `codape --version` prints it.
 * `scripts/check-site-version.py` compares it with `crates/cli/Cargo.toml`, so
 * the landing page cannot outlive a bump — the hero and the terminal animation
 * both read this constant rather than repeating the string.
 */
export const VERSION = "0.2.4"

/**
 * Counts the code asserts, not counts to remember. Each one has a test in the
 * crate that would have to change for the number here to become wrong:
 *
 * - tools     `crates/tools/src/builtins.rs` — `descriptors.len()`
 * - commands  `crates/commands/tests/slash_integration.rs` — `reg.len()`
 * - hooks     `crates/hooks/src/events.rs` — the triggered-event list
 * - agents    the built-in `AgentType` variants in `crates/agents/src/types.rs`
 * - crates    `ls crates`
 *
 * The README's summary table can lag behind these; when the two disagree, this
 * file should follow the tests.
 */
export const STATS = {
  tools: 59,
  commands: 80,
  hookEvents: 23,
  subAgents: 6,
  crates: 20,
} as const

export const NAV = [
  { label: "Capabilities", href: "#capabilities" },
  { label: "Install", href: "#install" },
  { label: "Docs", href: DOCS },
]

export const CAPABILITIES = [
  {
    id: "tools",
    kicker: "Tools",
    value: `${STATS.tools}`,
    title: "A toolbelt, not a chat box",
    body: "Read, write, edit, glob, grep, shell, web fetch and search, LSP, notebooks, tasks, cron, MCP and the Chrome DevTools family — registered at startup.",
    detail: "read · edit · bash · grep · lsp · notebook · mcp",
  },
  {
    id: "commands",
    kicker: "Commands",
    value: `${STATS.commands}`,
    title: "The daily-driver set",
    body: "Slash commands for the work around the work: /commit, /compact, /review, /diff, /resume and the rest of the set /help prints.",
    detail: "/commit  /review  /diff  /resume  /compact",
  },
  {
    id: "agents",
    kicker: "Sub-agents",
    value: `${STATS.subAgents}`,
    title: "Parallel, isolated, yours",
    body: "General, explore, plan, verification and two setup helpers ship in the binary. Add your own; run them in parallel or in the background inside their own worktrees.",
    detail: "general · explore · plan · verification",
  },
  {
    id: "memory",
    kicker: "Memory",
    value: "∞",
    title: "It remembers the work",
    body: "JSONL transcripts, a searchable session index, and long-term memory that survives across sessions — so you stop re-explaining the task.",
    detail: "transcripts · session index · long-term memory",
  },
  {
    id: "extensibility",
    kicker: "Extensibility",
    value: `${STATS.hookEvents}`,
    title: "Hooks on the seams you care about",
    body: `MCP servers, WASM and process plugins, hooks across ${STATS.hookEvents} lifecycle events, and an embeddable WebSocket server for editors and SDKs.`,
    detail: "mcp · wasm · process plugins · websocket",
  },
  {
    id: "isolation",
    kicker: "Isolation",
    value: "3",
    title: "The kernel enforces it",
    body: "bubblewrap or Landlock on Linux, Seatbelt on macOS, AppContainer on Windows — plus a granular permission engine layered on top.",
    detail: "bubblewrap · landlock · seatbelt",
  },
] as const

export const PLATFORMS = [
  { name: "Linux", detail: "x86_64, gnu + musl", sandbox: "bubblewrap / Landlock" },
  { name: "macOS", detail: "Apple silicon + Intel", sandbox: "Seatbelt" },
  { name: "Windows", detail: "x86_64", sandbox: "AppContainer" },
] as const
