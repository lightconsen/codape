import { cn } from "@/lib/utils"

/**
 * The mark is the repository's gorilla, reduced to a pixel grid and upscaled
 * with nearest-neighbour sampling by scripts/generate-brand.py.
 */
export function Mark({
  className,
  tone = "amber",
}: {
  className?: string
  tone?: "amber" | "bone" | "ink"
}) {
  const file =
    tone === "bone" ? "mark-light" : tone === "ink" ? "mark-ink" : "mark-amber"

  return (
    <img
      src={`/brand/${file}.png`}
      alt=""
      aria-hidden="true"
      decoding="async"
      className={cn("object-contain select-none", className)}
    />
  )
}

export function Wordmark({ className }: { className?: string }) {
  return (
    <span className={cn("group inline-flex items-center gap-2.5", className)}>
      <Mark className="size-7 shrink-0 transition-transform duration-150 group-hover:-translate-y-0.5" />
      <span className="font-display text-[15px] tracking-[0.04em] lowercase">
        codape
      </span>
    </span>
  )
}
