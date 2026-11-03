import * as Primitive from "@radix-ui/react-dialog";
import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

// shadcn/ui 的 Sheet：窄屏下侧栏（左）与右栏（右）变成抽屉，带遮罩；焦点圈在抽屉里、Esc 关，由 Radix 负责。
const Sheet = Primitive.Root;
const SheetClose = Primitive.Close;
const SheetTitle = Primitive.Title;
const SheetDescription = Primitive.Description;

function SheetContent({
  side,
  className,
  children,
  ...props
}: ComponentProps<typeof Primitive.Content> & { side: "left" | "right" }) {
  return (
    <Primitive.Portal>
      <Primitive.Overlay className="fixed inset-0 z-20 bg-[rgb(0_0_0/0.28)] data-[state=open]:animate-[fade-in_220ms_ease]" />
      <Primitive.Content
        className={cn(
          "fixed inset-y-0 z-25 flex flex-col outline-none",
          side === "left"
            ? "left-0 w-[min(300px,88%)] data-[state=open]:animate-[sheet-in-left_220ms_var(--ease-out-expo)]"
            : "right-0 w-[min(360px,100%)] data-[state=open]:animate-[sheet-in-right_220ms_var(--ease-out-expo)] max-[600px]:w-full",
          className,
        )}
        {...props}
      >
        {children}
      </Primitive.Content>
    </Primitive.Portal>
  );
}

export { Sheet, SheetClose, SheetContent, SheetDescription, SheetTitle };
