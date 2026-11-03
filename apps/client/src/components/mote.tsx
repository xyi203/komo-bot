import { cn } from "@/lib/utils";

/** "在场"光点：约定是实心，线索是空心环。叶绿只用在这里和"运行中"。 */
export function Mote({ convention, className }: { convention: boolean; className?: string }) {
  return (
    <span
      aria-hidden="true"
      className={cn("inline-block size-2 shrink-0 rounded-full", convention ? "bg-accent" : "border-[1.5px] border-accent", className)}
    />
  );
}
