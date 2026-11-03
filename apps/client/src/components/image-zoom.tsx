import * as Primitive from "@radix-ui/react-dialog";
import { useState, type ReactNode } from "react";
import { Icon } from "@/components/icon";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";

/**
 * 点缩略图看大图：`children` 是缩略图本身，包成一个按钮；点开后在铺满视口的暗色遮罩上居中放原图（按比例缩进视口），
 * 点任意处、右上角的关闭或 Esc 都关掉。焦点圈与关闭后焦点回到缩略图由 Radix Dialog 负责。
 */
export function ImageZoom({ src, name, title, className, children }: { src: string; name: string; title?: string; className?: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  return (
    <Primitive.Root open={open} onOpenChange={setOpen}>
      <Primitive.Trigger asChild>
        <button
          type="button"
          aria-label={m.attachment_zoom({ name })}
          title={title}
          className={cn("block cursor-zoom-in rounded-[inherit] focus-visible:outline-2 focus-visible:outline-primary", className)}
        >
          {children}
        </button>
      </Primitive.Trigger>
      <Primitive.Portal>
        <Primitive.Overlay className="fixed inset-0 z-40 bg-[rgb(20_24_16/0.78)] data-[state=open]:animate-[fade-in_180ms_ease]" />
        <Primitive.Content
          aria-describedby={undefined}
          onClick={() => setOpen(false)}
          className="fixed inset-0 z-45 grid cursor-zoom-out place-items-center p-10 outline-none data-[state=open]:animate-[fade-in_180ms_ease] max-[600px]:p-3"
        >
          <Primitive.Title className="sr-only">{name}</Primitive.Title>
          <img src={src} alt={name} className="max-h-full max-w-full rounded-inner object-contain shadow-[0_24px_64px_rgb(0_0_0/0.35)]" />
          <Primitive.Close
            aria-label={m.attachment_zoom_close()}
            title={m.attachment_zoom_close()}
            className="absolute top-3 right-3 grid size-9 cursor-pointer place-items-center rounded-full bg-[rgb(255_255_255/0.14)] text-white transition-colors hover:bg-[rgb(255_255_255/0.26)] max-[600px]:size-11"
          >
            <Icon name="x" className="size-4" />
          </Primitive.Close>
        </Primitive.Content>
      </Primitive.Portal>
    </Primitive.Root>
  );
}
