import { useEffect, useMemo, useRef, useState } from "react"

import { VERSION } from "@/data/site"
import { cn } from "@/lib/utils"

type Tone = "prompt" | "tool" | "result" | "note" | "muted" | "input"

type Line = {
  tone: Tone
  text: string
  meta?: string
  /** ms to wait before this line appears, relative to the previous one */
  pause?: number
}

const SCRIPT: Line[] = [
  { tone: "muted", text: `codape v${VERSION} · sandbox: seatbelt · permissions: default`, pause: 600 },
  { tone: "input", text: 'add retry with backoff to the upload client', pause: 1100 },
  { tone: "tool", text: "read", meta: "crates/net/src/upload.rs", pause: 700 },
  { tone: "tool", text: "grep", meta: "'backoff|retry'  3 files, 11 matches", pause: 640 },
  { tone: "tool", text: "read", meta: "crates/net/src/retry.rs", pause: 540 },
  { tone: "note", text: "three call sites share one client; the retry belongs in it", pause: 980 },
  { tone: "tool", text: "edit", meta: "crates/net/src/upload.rs  +61 -9", pause: 780 },
  { tone: "tool", text: "edit", meta: "crates/net/src/retry.rs  +35 -5", pause: 620 },
  { tone: "tool", text: "bash", meta: "cargo test -p codape-net", pause: 760 },
  { tone: "result", text: "test result: ok. 34 passed; 0 failed", pause: 900 },
  { tone: "note", text: "stopped: task complete. changes are unstaged.", pause: 0 },
]

const TONE_COLOR: Record<Tone, string> = {
  prompt: "text-primary",
  input: "text-foreground",
  tool: "text-primary",
  result: "text-[#b9d17e]",
  note: "text-foreground/80",
  muted: "text-muted-foreground",
}

export function HeroTerminal() {
  const [visible, setVisible] = useState(0)
  const [running, setRunning] = useState(true)
  const scroller = useRef<HTMLDivElement>(null)
  const reduced = usePrefersReducedMotion()
  const count = reduced ? SCRIPT.length : visible

  useEffect(() => {
    if (!running || reduced) return

    if (visible >= SCRIPT.length) {
      const reset = setTimeout(() => setVisible(0), 5600)
      return () => clearTimeout(reset)
    }

    const next = setTimeout(
      () => setVisible((v) => v + 1),
      SCRIPT[visible].pause ?? 600
    )
    return () => clearTimeout(next)
  }, [visible, running, reduced])

  useEffect(() => {
    if (typeof scroller.current?.scrollTo !== "function") return
    const node = scroller.current
    if (!node) return
    node.scrollTo({ top: node.scrollHeight, behavior: "smooth" })
  }, [count])

  const shown = useMemo(() => SCRIPT.slice(0, count), [count])
  const done = count >= SCRIPT.length

  return (
    <figure
      className="relative w-full border-2 border-border bg-[#100d09] shadow-[8px_8px_0_0_#000]"
      aria-label="Example Codape session"
    >
      <div className="flex items-center gap-2 border-b-2 border-border bg-[#1d1710] px-3 py-2">
        <span className="size-2.5 border border-[#d2542f] bg-[#d2542f]" aria-hidden="true" />
        <span className="size-2.5 border border-primary bg-primary" aria-hidden="true" />
        <span className="size-2.5 border border-[#5c6b3a] bg-[#7d8f4b]" aria-hidden="true" />
        <span className="ml-1.5 truncate font-mono text-[10.5px] tracking-[0.06em] text-muted-foreground">
          ~/src/uploader — codape
        </span>
        <button
          type="button"
          onClick={() => setRunning((r) => !r)}
          className="ml-auto border border-border px-1.5 py-0.5 font-mono text-[9px] tracking-[0.14em] text-muted-foreground uppercase transition-colors hover:border-primary/60 hover:text-primary"
          aria-label={running ? "Pause the replay" : "Resume the replay"}
        >
          {running ? "pause" : "play"}
        </button>
      </div>

      <div className="scanlines relative">
        <div
          ref={scroller}
          className="relative h-[318px] overflow-hidden px-3.5 py-3 sm:h-[338px]"
        >
          <div className="font-mono text-[11.5px] leading-[1.6] sm:text-[12px]">
            {shown.map((line, i) => (
              <Row key={i} line={line} fresh={i === shown.length - 1} />
            ))}
          </div>

          {done ? (
            <div className="mt-1 flex items-center gap-2 font-mono text-[11.5px] text-primary sm:text-[12px]">
              <span aria-hidden="true">❯</span>
              <span className="caret" />
            </div>
          ) : null}

          <div
            aria-hidden="true"
            className="scanline pointer-events-none absolute inset-x-0 top-0 h-14 bg-gradient-to-b from-transparent via-primary/[0.045] to-transparent"
          />
        </div>
      </div>

      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 border-t-2 border-border bg-[#1d1710] px-3 py-1.5 font-mono text-[9.5px] tracking-[0.1em] text-muted-foreground uppercase">
        <span className="flex items-center gap-1.5">
          <span className="size-1.5 bg-primary" aria-hidden="true" />
          local session
        </span>
        <span className="hidden sm:inline">seatbelt</span>
        <span className="hidden sm:inline">default</span>
        <span className="ml-auto text-muted-foreground/60">shift-tab · cycle mode</span>
      </div>
    </figure>
  )
}

function Row({ line, fresh }: { line: Line; fresh: boolean }) {
  return (
    <div className={cn("flex items-start gap-2 py-[2px]", fresh && "animate-stream")}>
      <span
        aria-hidden="true"
        className={cn("w-3 shrink-0 text-center text-[10px]", TONE_COLOR[line.tone])}
      >
        {line.tone === "input" ? "❯" : line.tone === "tool" ? "▸" : line.tone === "result" ? "✓" : line.tone === "note" ? "·" : ""}
      </span>
      <div className="min-w-0 flex-1">
        <span className={cn(TONE_COLOR[line.tone], line.tone === "note" && "italic")}>
          {line.text}
        </span>
        {line.meta ? (
          <span className="ml-2 text-muted-foreground">{line.meta}</span>
        ) : null}
      </div>
    </div>
  )
}

function usePrefersReducedMotion() {
  const [reduced, setReduced] = useState(false)
  useEffect(() => {
    const query = window.matchMedia("(prefers-reduced-motion: reduce)")
    const update = () => setReduced(query.matches)
    update()
    query.addEventListener("change", update)
    return () => query.removeEventListener("change", update)
  }, [])
  return reduced
}
