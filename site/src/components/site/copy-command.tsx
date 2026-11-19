import { useEffect, useRef, useState } from "react"
import { Check, Copy } from "lucide-react"

import { cn } from "@/lib/utils"

/** A terminal-styled command block with a copy affordance. */
export function CopyCommand({
  command,
  className,
  label,
  compact = false,
}: {
  command: string
  className?: string
  label?: string
  compact?: boolean
}) {
  const [copied, setCopied] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current)
    },
    []
  )

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(command)
    } catch {
      return
    }
    setCopied(true)
    if (timer.current) clearTimeout(timer.current)
    timer.current = setTimeout(() => setCopied(false), 1800)
  }

  return (
    <div
      className={cn(
        "flex items-stretch border-2 border-border bg-[#120f0a] font-mono text-foreground",
        compact ? "text-[11px]" : "text-[11.5px] sm:text-[12.5px]",
        className
      )}
    >
      {label ? (
        <span className="hidden items-center border-r border-border bg-secondary px-2.5 text-[9.5px] tracking-[0.16em] text-muted-foreground uppercase sm:flex">
          {label}
        </span>
      ) : (
        <span
          aria-hidden="true"
          className="flex items-center pr-0.5 pl-2.5 text-primary select-none"
        >
          $
        </span>
      )}
      <code
        className={cn(
          "min-w-0 flex-1",
          compact
            ? "px-2 py-2 break-all whitespace-pre-wrap"
            : "overflow-x-auto px-2 py-2.5 whitespace-nowrap [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
        )}
      >
        {command}
      </code>
      <button
        type="button"
        onClick={copy}
        aria-label={copied ? "Copied" : "Copy command"}
        className="flex w-9 shrink-0 items-center justify-center border-l border-border text-muted-foreground transition-colors hover:bg-secondary hover:text-primary"
      >
        {copied ? (
          <Check className="size-3.5 text-primary" />
        ) : (
          <Copy className="size-3.5" />
        )}
      </button>
    </div>
  )
}
