import * as React from "react"
import { Slot } from "@radix-ui/react-slot"
import { cva, type VariantProps } from "class-variance-authority"

import { cn } from "@/lib/utils"

const buttonVariants = cva(
  "inline-flex items-center justify-center gap-2 whitespace-nowrap font-display text-[11px] uppercase leading-none tracking-[0.08em] transition-[transform,box-shadow,background-color,color,border-color] outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg:not([class*='size-'])]:size-3.5 shrink-0",
  {
    variants: {
      variant: {
        default:
          "bg-primary text-primary-foreground shadow-[4px_4px_0_0_#8c6a2f] hover:-translate-x-0.5 hover:-translate-y-0.5 hover:shadow-[6px_6px_0_0_#8c6a2f] active:translate-x-0.5 active:translate-y-0.5 active:shadow-[2px_2px_0_0_#8c6a2f]",
        outline:
          "border-2 border-border bg-card text-foreground shadow-[4px_4px_0_0_#000] hover:-translate-x-0.5 hover:-translate-y-0.5 hover:border-primary/70 hover:shadow-[6px_6px_0_0_#000] active:translate-x-0.5 active:translate-y-0.5 active:shadow-[2px_2px_0_0_#000]",
        ghost:
          "font-mono text-[11px] normal-case tracking-tight text-muted-foreground hover:bg-secondary hover:text-foreground",
        link: "font-mono text-[11px] normal-case tracking-tight text-primary underline underline-offset-4 hover:text-primary/80",
      },
      size: {
        default: "h-11 px-5",
        sm: "h-9 px-3.5 text-[10px]",
        lg: "h-12 px-6 text-[12px]",
        icon: "size-10",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  }
)

function Button({
  className,
  variant,
  size,
  asChild = false,
  ...props
}: React.ComponentProps<"button"> &
  VariantProps<typeof buttonVariants> & {
    asChild?: boolean
  }) {
  const Comp = asChild ? Slot : "button"

  return (
    <Comp
      data-slot="button"
      className={cn(buttonVariants({ variant, size, className }))}
      {...props}
    />
  )
}

// eslint-disable-next-line react-refresh/only-export-components
export { Button, buttonVariants }
