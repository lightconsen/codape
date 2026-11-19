import { ArrowDown } from "lucide-react"

import { HeroTerminal } from "@/components/sections/hero-terminal"
import { CopyCommand } from "@/components/site/copy-command"
import { Button } from "@/components/ui/button"
import { INSTALL_CMD, REPO, STATS, VERSION } from "@/data/site"

const STAT_ITEMS = [
  { value: `${STATS.tools}`, label: "tool types" },
  { value: `${STATS.commands}`, label: "slash commands" },
  { value: `${STATS.hookEvents}`, label: "hook events" },
  { value: `${STATS.crates}`, label: "rust crates" },
]

export function Hero() {
  return (
    <section id="top" className="relative overflow-hidden pt-32 sm:pt-36">
      <div
        aria-hidden="true"
        className="bg-pixel-grid pointer-events-none absolute inset-0 -z-10 mask-fade-b opacity-80"
      />
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-x-0 top-0 -z-10 h-[420px] bg-[radial-gradient(ellipse_at_top,rgba(242,169,59,0.1),transparent_65%)]"
      />

      <div className="mx-auto max-w-[1200px] px-5 lg:px-8">
        <div className="grid grid-cols-1 items-center gap-12 lg:grid-cols-[minmax(0,1.02fr)_minmax(0,1fr)] lg:gap-14">
          <div className="min-w-0 animate-rise">
            <div className="flex flex-wrap items-center gap-2">
              <span className="chip border-primary/50 text-primary">
                <span className="size-1.5 bg-primary" aria-hidden="true" />
                v{VERSION}
              </span>
              <span className="chip">local session</span>
              <span className="chip">apache-2.0</span>
            </div>

            <h1 className="mt-6 font-pixel text-[clamp(2.2rem,5.2vw,3.9rem)] leading-[1.06] font-normal">
              A coding agent
              <br />
              that <span className="text-primary">finishes</span>
              <br />
              the task.
            </h1>

            <p className="mt-6 max-w-[48ch] text-pretty text-[16px] leading-[1.7] text-muted-foreground">
              It runs a model against your repository, decides what to do next,
              and executes it — reading and editing files, running shell
              commands, driving sub-agents — until the work is done or it names
              why it stopped.
            </p>

            <div className="mt-8 flex flex-col gap-3 sm:flex-row sm:items-center">
              <Button asChild size="lg">
                <a href="#install">
                  Install
                  <ArrowDown className="size-3.5" />
                </a>
              </Button>
              <Button asChild variant="outline" size="lg">
                <a href={REPO} target="_blank" rel="noreferrer">
                  Read the source
                </a>
              </Button>
            </div>

            <CopyCommand command={INSTALL_CMD} className="mt-5 max-w-[540px]" />

            <dl className="mt-8 grid max-w-[540px] grid-cols-2 gap-[2px] border-2 border-border bg-border sm:grid-cols-4">
              {STAT_ITEMS.map((stat) => (
                <div key={stat.label} className="bg-card px-3 py-3">
                  <dt className="sr-only">{stat.label}</dt>
                  <dd>
                    <span className="block font-display text-[22px] leading-none text-primary">
                      {stat.value}
                    </span>
                    <span className="mt-2 block font-mono text-[9.5px] tracking-[0.14em] text-muted-foreground uppercase">
                      {stat.label}
                    </span>
                  </dd>
                </div>
              ))}
            </dl>
          </div>

          <div className="min-w-0 animate-rise [animation-delay:100ms]">
            <HeroTerminal />
            <p className="mt-3 flex items-center gap-2 font-mono text-[10px] tracking-[0.14em] text-muted-foreground/70 uppercase">
              <span className="h-2 w-2 bg-primary/60" aria-hidden="true" />
              one real session, replayed
            </p>
          </div>
        </div>
      </div>
    </section>
  )
}
