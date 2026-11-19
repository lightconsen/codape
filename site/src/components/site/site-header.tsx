import { useEffect, useState } from "react"
import { Menu, X } from "lucide-react"

import { NAV, REPO } from "@/data/site"
import { cn } from "@/lib/utils"
import { Wordmark } from "@/components/site/mark"

const TICKER = [
  "54 TOOL TYPES",
  "78 SLASH COMMANDS",
  "22 HOOK EVENTS",
  "6 SUB-AGENTS",
  "SANDBOX ON BY DEFAULT",
  "APACHE-2.0",
  "ONE NATIVE BINARY",
  "LOCAL SESSION ONLY",
]

export function SiteHeader() {
  const [open, setOpen] = useState(false)
  const [scrolled, setScrolled] = useState(false)

  useEffect(() => {
    const onScroll = () => setScrolled(window.scrollY > 12)
    onScroll()
    window.addEventListener("scroll", onScroll, { passive: true })
    return () => window.removeEventListener("scroll", onScroll)
  }, [])

  useEffect(() => {
    if (!open) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false)
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [open])

  return (
    <header className="fixed inset-x-0 top-0 z-50">
      <div
        aria-hidden="true"
        className="overflow-hidden border-b border-border/70 bg-[#100d09]"
      >
        <div className="ticker-track flex w-max items-center py-1">
          {[0, 1].map((copy) => (
            <div key={copy} className="flex shrink-0 items-center">
              {TICKER.map((item) => (
                <span
                  key={item}
                  className="flex items-center gap-3 px-4 font-mono text-[9.5px] tracking-[0.2em] whitespace-nowrap text-muted-foreground/80"
                >
                  <span className="text-primary" aria-hidden="true">
                    ◆
                  </span>
                  {item}
                </span>
              ))}
            </div>
          ))}
        </div>
      </div>

      <div
        className={cn(
          "border-b transition-colors duration-200",
          scrolled
            ? "border-border bg-background/95 backdrop-blur"
            : "border-transparent bg-gradient-to-b from-background via-background/80 to-transparent"
        )}
      >
        <div className="mx-auto flex h-14 max-w-[1200px] items-center gap-6 px-5 lg:px-8">
          <a
            href="#top"
            className="text-foreground"
            aria-label="Codape, back to top"
          >
            <Wordmark />
          </a>

          <nav aria-label="Sections" className="ml-auto hidden items-center md:flex">
            {NAV.map((item) => (
              <a
                key={item.href}
                href={item.href}
                className="border border-transparent px-2.5 py-1.5 font-mono text-[11px] tracking-tight text-muted-foreground transition-colors hover:border-border hover:bg-secondary hover:text-foreground"
              >
                {item.label}
              </a>
            ))}
          </nav>

          <div className="ml-auto flex items-center gap-2 md:ml-2">
            <a
              href={REPO}
              target="_blank"
              rel="noreferrer"
              className="hidden h-8 items-center border-2 border-border px-2.5 font-display text-[9.5px] tracking-[0.1em] text-muted-foreground uppercase transition-colors hover:border-primary/60 hover:text-foreground sm:inline-flex"
            >
              GitHub
              <span className="ml-1.5" aria-hidden="true">
                ↗
              </span>
            </a>
            <a
              href="#install"
              className="inline-flex h-8 items-center border-2 border-primary bg-primary px-3 font-display text-[9.5px] tracking-[0.1em] text-primary-foreground uppercase transition-transform hover:-translate-y-px active:translate-y-px"
            >
              Install
            </a>
            <button
              type="button"
              onClick={() => setOpen((v) => !v)}
              aria-expanded={open}
              aria-controls="mobile-nav"
              aria-label={open ? "Close menu" : "Open menu"}
              className="inline-flex size-8 items-center justify-center border-2 border-border text-muted-foreground md:hidden"
            >
              {open ? <X className="size-4" /> : <Menu className="size-4" />}
            </button>
          </div>
        </div>
      </div>

      <div
        id="mobile-nav"
        className={cn(
          "grid overflow-hidden border-border bg-background transition-[grid-template-rows] duration-200 md:hidden",
          open ? "grid-rows-[1fr] border-b" : "grid-rows-[0fr] border-b-0"
        )}
      >
        <div className="min-h-0">
          <nav aria-label="Sections" className="flex flex-col px-5 py-2">
            {NAV.map((item) => (
              <a
                key={item.href}
                href={item.href}
                onClick={() => setOpen(false)}
                className="border-b border-border py-3 font-mono text-[12px] text-muted-foreground last:border-b-0 hover:text-foreground"
              >
                <span className="mr-2 text-primary" aria-hidden="true">
                  ▸
                </span>
                {item.label}
              </a>
            ))}
          </nav>
        </div>
      </div>
    </header>
  )
}
