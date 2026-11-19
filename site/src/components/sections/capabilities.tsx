import { CAPABILITIES } from "@/data/site"
import { SectionHead } from "@/components/site/section-head"

export function Capabilities() {
  return (
    <section
      id="capabilities"
      className="relative border-t-2 border-border bg-[#0f0c08] py-16 sm:py-20 lg:py-24"
    >
      <div
        aria-hidden="true"
        className="bg-dots pointer-events-none absolute inset-0 opacity-30"
      />
      <div className="relative mx-auto max-w-[1200px] px-5 lg:px-8">
        <div className="flex flex-col gap-8 lg:flex-row lg:items-end lg:justify-between">
          <SectionHead
            index="01"
            label="Capabilities"
            title={
              <>
                Everything is in
                <br />
                the binary.
              </>
            }
          />
          <p className="max-w-[38ch] border-l-2 border-primary/50 pl-4 text-pretty text-[14px] leading-relaxed text-muted-foreground">
            No plugin marketplace to bootstrap, no hosted service behind it. The
            catalogue ships compiled in; you opt into what you use.
          </p>
        </div>

        <div className="mt-12 grid gap-px border-2 border-border bg-border md:grid-cols-2 lg:grid-cols-3">
          {CAPABILITIES.map((cap) => (
            <article
              key={cap.id}
              className="group relative flex flex-col bg-card p-5 transition-colors hover:bg-secondary"
            >
              <div className="flex items-baseline justify-between gap-4">
                <span className="font-mono text-[10px] tracking-[0.16em] text-muted-foreground uppercase">
                  {cap.kicker}
                </span>
                <span className="font-display text-[30px] leading-none text-primary/30 transition-colors group-hover:text-primary">
                  {cap.value}
                </span>
              </div>
              <h3 className="mt-5 text-[16px] leading-snug font-semibold text-foreground">
                {cap.title}
              </h3>
              <p className="mt-2 flex-1 text-[13.5px] leading-relaxed text-muted-foreground">
                {cap.body}
              </p>
              <p className="mt-4 truncate border-t border-dashed border-border pt-2.5 font-mono text-[10px] text-muted-foreground/70">
                {cap.detail}
              </p>
            </article>
          ))}
        </div>
      </div>
    </section>
  )
}
