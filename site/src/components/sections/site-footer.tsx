import { ArrowUpRight } from "lucide-react"

import { GithubMark } from "@/components/site/github-mark"
import { Mark } from "@/components/site/mark"
import { Button } from "@/components/ui/button"
import { DOCS, REPO } from "@/data/site"

const COLUMNS = [
  {
    title: "Product",
    links: [
      { label: "Capabilities", href: "#capabilities" },
      { label: "Install", href: "#install" },
      { label: "Releases", href: `${REPO}/releases` },
    ],
  },
  {
    title: "Docs",
    links: [
      { label: "Architecture", href: `${DOCS}/arch.md` },
      { label: "CLI reference", href: `${DOCS}/cli.md` },
      { label: "Configuration", href: `${DOCS}/config.md` },
      { label: "Sandbox", href: `${DOCS}/sandbox.md` },
      { label: "Hooks", href: `${DOCS}/hooks.md` },
    ],
  },
  {
    title: "Project",
    links: [
      { label: "Source", href: REPO },
      { label: "Contributing", href: `${REPO}/blob/main/CONTRIBUTING.md` },
      { label: "Security", href: `${REPO}/blob/main/SECURITY.md` },
      { label: "License", href: `${REPO}/blob/main/LICENSE` },
    ],
  },
]

export function SiteFooter() {
  return (
    <footer className="border-t-2 border-border bg-[#0a0806]">
      <div className="mx-auto max-w-[1200px] px-5 py-14 lg:px-8">
        <div className="grid gap-10 lg:grid-cols-[minmax(0,1.25fr)_minmax(0,2fr)]">
          <div>
            <a href="#top" className="inline-flex items-center gap-2.5" aria-label="Codape">
              <Mark className="size-8" />
              <span className="font-display text-[16px] tracking-[0.04em] lowercase">
                codape
              </span>
            </a>
            <p className="mt-4 max-w-[34ch] text-[13.5px] leading-relaxed text-muted-foreground">
              A coding agent for your terminal. Single native binary, local
              session, Apache-2.0.
            </p>
            <div className="mt-5">
              <Button asChild variant="outline" size="sm">
                <a href={REPO} target="_blank" rel="noreferrer">
                  <GithubMark className="size-3.5" />
                  lightconsen/codape
                  <ArrowUpRight className="size-3 text-muted-foreground" />
                </a>
              </Button>
            </div>
          </div>

          <div className="grid gap-8 sm:grid-cols-3">
            {COLUMNS.map((column) => (
              <nav key={column.title} aria-label={column.title}>
                <h2 className="flex items-center gap-2 font-mono text-[10px] tracking-[0.18em] text-muted-foreground/70 uppercase">
                  <span className="size-1.5 bg-primary/60" aria-hidden="true" />
                  {column.title}
                </h2>
                <ul className="mt-3 space-y-2 border-l border-border pl-3">
                  {column.links.map((link) => (
                    <li key={link.label}>
                      <a
                        href={link.href}
                        className="text-[13px] text-muted-foreground transition-colors hover:text-primary"
                        {...(link.href.startsWith("http")
                          ? { target: "_blank", rel: "noreferrer" }
                          : {})}
                      >
                        {link.label}
                      </a>
                    </li>
                  ))}
                </ul>
              </nav>
            ))}
          </div>
        </div>

        <div className="mt-12 flex flex-col gap-4 border-t border-border pt-6 sm:flex-row sm:items-center sm:justify-between">
          <p className="font-mono text-[10.5px] tracking-[0.08em] text-muted-foreground/70 uppercase">
            © 2026 Codape · Apache-2.0
          </p>
          <p className="max-w-[62ch] font-mono text-[10px] leading-relaxed text-muted-foreground/50">
            Codape is an independent project. It is not affiliated with, endorsed
            by or sponsored by Anthropic. “Claude” refers only to the models it
            can connect to.
          </p>
        </div>
      </div>
    </footer>
  )
}
