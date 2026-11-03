import type { MemoryItem } from "@komo/protocol";
import { cn } from "@/lib/utils";
import { moteKind, type MoteKind } from "./model";

/** 记忆页的"在场"光点：实心 = 约定，空心环 = 线索，虚线 = 候选，红环 = 有争议，灰点 = 不再带。 */
export function MemoryMote({ kind, className }: { kind: MemoryItem | MoteKind; className?: string }) {
  const k = typeof kind === "string" ? kind : moteKind(kind);
  return (
    <span
      aria-hidden="true"
      className={cn(
        "inline-block shrink-0 rounded-full",
        k === "off" ? "size-1.75 bg-control" : "size-2",
        k === "full" && "bg-accent",
        k === "ring" && "border-[1.5px] border-accent",
        k === "pale" && "border-[1.5px] border-dashed border-faint",
        k === "alert" && "border-[1.5px] border-negative",
        className,
      )}
    />
  );
}
