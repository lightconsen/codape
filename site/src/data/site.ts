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
export const VERSION = "0.2.2"

export const NAV = [
  { label: "Capabilities", href: "#capabilities" },
  { label: "Install", href: "#install" },
  { label: "Docs", href: DOCS },
]

export const CAPABILITIES = [
  {
    id: "tools",
    kicker: "Tools",
    value: "54",
    title: "A toolbelt, not a chat box",
    body: "Read, write, edit, glob, grep, shell, web fetch and search, LSP, notebooks, tasks, cron, MCP and the Chrome DevTools family — registered at startup.",
    detail: "read · edit · bash · grep · lsp · notebook · mcp",
  },
  {
    id: "commands",
    kicker: "Commands",
    value: "78",
    title: "The daily-driver set",
    body: "Slash commands for the work around the work: /commit, /compact, /review, /diff, /resume and the rest of the set /help prints.",
    detail: "/commit  /review  /diff  /resume  /compact",
  },
  {
    id: "agents",
    kicker: "Sub-agents",
    value: "6",
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
    value: "22",
    title: "Hooks on the seams you care about",
    body: "MCP servers, WASM and process plugins, hooks across 22 lifecycle events, and an embeddable WebSocket server for editors and SDKs.",
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
