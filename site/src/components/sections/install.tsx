import { ArrowUpRight, Cpu, ShieldCheck, Terminal } from "lucide-react"

import { CopyCommand } from "@/components/site/copy-command"
import { Button } from "@/components/ui/button"
import { SectionHead } from "@/components/site/section-head"
import { DOCS, INSTALL_CMD, PLATFORMS, RELEASES, REPO } from "@/data/site"

const ROUTES = [
  {
    label: "sh",
    title: "Install script",
    body: "Fetches the release for your target, verifies it against SHA256SUMS, and installs to ~/.local/bin or /usr/local/bin.",
    command: INSTALL_CMD,
    href: `${REPO}/blob/main/scripts/install.sh`,
    hrefLabel: "scripts/install.sh",
  },
  {
    label: "cargo",
    title: "From source",
    body: "Builds the workspace from the repository. Expect a C toolchain, roughly 8 GB at peak, and tens of gigabytes of disk.",
    command: "cargo install --git https://github.com/lightconsen/codape.git",
    href: `${DOCS}/build.md`,
    hrefLabel: "docs/build.md",
  },
  {
    label: "bin",
    title: "Prebuilt archives",
    body: "Every supported target is published with a checksum. Updates replace an existing binary atomically.",
    command: "codape update",
    href: RELEASES,
    hrefLabel: "releases",
  },
]

export function Install() {
  return (
    <section id="install" className="relative overflow-hidden border-t-2 border-border py-16 sm:py-20 lg:py-24">
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-x-0 top-0 h-64 bg-[radial-gradient(ellipse_at_top,rgba(242,169,59,0.08),transparent_70%)]"
      />
      <div className="relative mx-auto max-w-[1200px] px-5 lg:px-8">
        <SectionHead
          index="02"
          label="Install"
          align="center"
          title={
            <>
              One curl. Then it is
              <br />
              your terminal.
            </>
          }
          lede="The installer refuses a download it cannot verify. Nothing runs in the background, and there is no hosted account behind the binary."
        />

        <div className="mx-auto mt-9 max-w-[720px]">
          <CopyCommand command={INSTALL_CMD} label="terminal" />
        </div>

        <div className="mt-12 grid gap-px border-2 border-border bg-border md:grid-cols-3">
          {ROUTES.map((route) => (
            <div key={route.label} className="flex flex-col bg-card p-5">
              <span className="chip w-fit border-primary/40 text-primary">
                {route.label}
              </span>
              <h3 className="mt-4 text-[15.5px] font-semibold text-foreground">
                {route.title}
              </h3>
              <p className="mt-1.5 flex-1 text-[13px] leading-relaxed text-muted-foreground">
                {route.body}
              </p>
              <CopyCommand command={route.command} className="mt-4" compact />
              <a
                href={route.href}
                target="_blank"
                rel="noreferrer"
                className="mt-3 inline-flex items-center gap-1 font-mono text-[10.5px] text-muted-foreground transition-colors hover:text-primary"
              >
                {route.hrefLabel}
                <ArrowUpRight className="size-3" />
              </a>
            </div>
          ))}
        </div>

        <div className="mt-10 border-2 border-border bg-card">
          <div className="flex items-center gap-2 border-b-2 border-border bg-[#1d1710] px-4 py-2">
            <Cpu className="size-3.5 text-primary" aria-hidden="true" />
            <span className="font-mono text-[10px] tracking-[0.16em] text-muted-foreground uppercase">
              Supported targets
            </span>
          </div>
          <div className="grid gap-px bg-border sm:grid-cols-3">
            {PLATFORMS.map((platform) => (
              <div key={platform.name} className="bg-card px-4 py-3.5">
                <div className="flex items-baseline justify-between gap-2">
                  <span className="text-[13.5px] font-semibold text-foreground">
                    {platform.name}
                  </span>
                  <span className="font-mono text-[10px] text-muted-foreground">
                    {platform.detail}
                  </span>
                </div>
                <div className="mt-1.5 flex items-center gap-1.5 font-mono text-[10px] text-muted-foreground/80">
                  <ShieldCheck className="size-3 text-primary/70" aria-hidden="true" />
                  {platform.sandbox}
                </div>
              </div>
            ))}
          </div>
        </div>

        <div className="mt-10 flex flex-wrap items-center justify-center gap-3">
          <Button asChild>
            <a href={`${DOCS}/cli.md`} target="_blank" rel="noreferrer">
              <Terminal className="size-3.5" />
              CLI reference
            </a>
          </Button>
          <Button asChild variant="outline">
            <a href={DOCS} target="_blank" rel="noreferrer">
              All 36 documents
              <ArrowUpRight className="size-3.5" />
            </a>
          </Button>
        </div>
      </div>
    </section>
  )
}
