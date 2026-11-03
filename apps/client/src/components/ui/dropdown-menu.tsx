import * as Primitive from "@radix-ui/react-dropdown-menu";
import type { ComponentProps } from "react";
import { Icon } from "@/components/icon";
import { cn } from "@/lib/utils";

// shadcn/ui 的 DropdownMenu，样式是原型的 `.menu`：白底、环形边线、浮起的阴影（菜单是唯一浮起来的东西）。
// 焦点、方向键、typeahead、Esc 都由 Radix 负责。

const DropdownMenu = Primitive.Root;
const DropdownMenuTrigger = Primitive.Trigger;
const DropdownMenuRadioGroup = Primitive.RadioGroup;

function DropdownMenuContent({
  className,
  size = "default",
  sideOffset = 4,
  ...props
}: ComponentProps<typeof Primitive.Content> & { size?: "default" | "compact" }) {
  return (
    <Primitive.Portal>
      <Primitive.Content
        sideOffset={sideOffset}
        collisionPadding={8}
        className={cn(
          "z-30 rounded-card bg-surface p-1.5 shadow-[0_0_0_1px_rgb(0_0_0/0.12),0_8px_24px_rgb(0_0_0/0.12)]",
          size === "default" ? "w-62" : "min-w-42 p-1",
          className,
        )}
        {...props}
      />
    </Primitive.Portal>
  );
}

function DropdownMenuItem({
  className,
  danger,
  size = "default",
  ...props
}: ComponentProps<typeof Primitive.Item> & { danger?: boolean; size?: "default" | "compact" }) {
  return (
    <Primitive.Item
      className={cn(
        "flex w-full cursor-pointer items-center rounded-sm text-left outline-none select-none",
        size === "default" ? "h-8.5 gap-2.5 px-2.5" : "h-7.5 gap-6 pr-2 pl-2.5 text-body",
        danger
          ? "text-negative data-highlighted:bg-negative-pale"
          : "text-text data-highlighted:bg-hover-on-white",
        className,
      )}
      {...props}
    />
  );
}

function DropdownMenuRadioItem({ className, children, ...props }: ComponentProps<typeof Primitive.RadioItem>) {
  return (
    <Primitive.RadioItem
      className={cn(
        "flex h-8.5 w-full cursor-pointer items-center gap-2.5 rounded-sm px-2.5 text-left text-text outline-none select-none data-highlighted:bg-hover-on-white",
        className,
      )}
      {...props}
    >
      {children}
      <Primitive.ItemIndicator className="ml-auto">
        <Icon name="check" />
      </Primitive.ItemIndicator>
    </Primitive.RadioItem>
  );
}

function DropdownMenuLabel({ className, ...props }: ComponentProps<typeof Primitive.Label>) {
  return <Primitive.Label className={cn("px-2.5 pt-2 pb-1 text-small text-muted", className)} {...props} />;
}

function DropdownMenuSeparator({ className, ...props }: ComponentProps<typeof Primitive.Separator>) {
  return <Primitive.Separator className={cn("mx-1 my-1.5 h-px bg-rule", className)} {...props} />;
}

export {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
};
