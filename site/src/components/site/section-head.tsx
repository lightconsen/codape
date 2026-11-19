import type { ReactNode } from "react"

import { cn } from "@/lib/utils"

/**
 * A section header drawn like a window title bar: a bordered strip with an
 * index, a label and dashes, then the statement.
 */
export function SectionHead({
  index,
  label,
  title,
  lede,
  className,
  align = "left",
}: {
  index: string
  label: string
  title: ReactNode
  lede?: ReactNode
  className?: string
  align?: "left" | "center"
}) {
  return (
    <div
      className={cn(
        "flex flex-col gap-6",
        align === "center" && "items-center text-center",
        className
      )}
    >
      <div
        className={cn(
          "inline-flex w-fit max-w-full items-stretch border border-border font-mono text-[10px] tracking-[0.18em] uppercase",
          align === "center" && "mx-auto"
        )}
      >
        <span className="flex items-center bg-primary px-2 py-1.5 text-primary-foreground">
          {index}
        </span>
        <span className="flex items-center px-2.5 py-1.5 text-muted-foreground">
          {label}
        </span>
        <span
          aria-hidden="true"
          className="hidden w-10 items-center overflow-hidden text-border sm:flex"
        >
          <span className="font-mono text-[10px] tracking-[-0.1em] whitespace-nowrap">
            ------
          </span>
        </span>
      </div>

      <h2 className="text-balance font-pixel text-[clamp(1.75rem,4vw,3.1rem)] leading-[1.1] font-normal text-foreground">
        {title}
      </h2>

      {lede ? (
        <p
          className={cn(
            "max-w-[56ch] text-pretty text-[15.5px] leading-[1.75] text-muted-foreground",
            align === "center" && "mx-auto"
          )}
        >
          {lede}
        </p>
      ) : null}
    </div>
  )
}
