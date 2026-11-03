import * as Primitive from "@radix-ui/react-dialog";
import type { ComponentProps } from "react";
import { cn } from "@/lib/utils";

// 居中的模态弹窗（设置）：遮罩、焦点圈在里面、Esc 与点遮罩关闭都由 Radix 负责。
const Dialog = Primitive.Root;
const DialogClose = Primitive.Close;
const DialogTitle = Primitive.Title;
const DialogDescription = Primitive.Description;

function DialogContent({ className, children, ...props }: ComponentProps<typeof Primitive.Content>) {
  return (
    <Primitive.Portal>
      <Primitive.Overlay className="fixed inset-0 z-40 bg-[rgb(0_0_0/0.32)] data-[state=open]:animate-[fade-in_180ms_ease]" />
      <Primitive.Content
        className={cn(
          "fixed top-1/2 left-1/2 z-45 -translate-x-1/2 -translate-y-1/2 overflow-hidden rounded-dialog bg-surface text-text shadow-[var(--shadow-ring),0_24px_64px_rgb(0_0_0/0.18)] outline-none data-[state=open]:animate-[dialog-in_180ms_var(--ease-out-expo)]",
          className,
        )}
        {...props}
      >
        {children}
      </Primitive.Content>
    </Primitive.Portal>
  );
}

export { Dialog, DialogClose, DialogContent, DialogDescription, DialogTitle };
