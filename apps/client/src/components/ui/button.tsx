import { Slot } from "@radix-ui/react-slot";
import { cva, type VariantProps } from "class-variance-authority";
import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

// shadcn/ui 的 Button，配色换成 DESIGN.md 的 token（primary 是墨，secondary 是白底环）。
const buttonVariants = cva(
  "inline-flex h-8 shrink-0 items-center justify-center gap-2 rounded-control px-3 text-label font-medium whitespace-nowrap transition-colors disabled:pointer-events-none disabled:opacity-50",
  {
    variants: {
      variant: {
        primary: "bg-primary text-surface hover:bg-primary-hover",
        secondary: "bg-surface text-text shadow-ring-control hover:bg-hover-on-white",
        ghost: "text-muted hover:bg-hover-on-white hover:text-text",
      },
    },
    defaultVariants: { variant: "secondary" },
  },
);

function Button({
  className,
  variant,
  asChild = false,
  ...props
}: ComponentProps<"button"> & VariantProps<typeof buttonVariants> & { asChild?: boolean }) {
  const Comp = asChild ? Slot : "button";
  return <Comp data-slot="button" className={cn(buttonVariants({ variant }), className)} {...props} />;
}

export { Button, buttonVariants };
