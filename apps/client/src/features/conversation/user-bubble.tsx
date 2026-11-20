import { CopyButton } from "@/components/copy-button";
import { formatStamp } from "@/lib/format";
import { cn } from "@/lib/utils";
import { AttachmentList, splitAttachments } from "./attachments";

export function UserBubble({ text, at, standalone = false }: { text: string; at?: string; standalone?: boolean }) {
  const { attachments, text: words } = splitAttachments(text);
  return (
    <div className={cn("flex flex-col items-end gap-1", standalone && "mt-6 mb-2")}>
      <AttachmentList attachments={attachments} />
      {words.trim() && (
        <div className="max-w-[80%] rounded-[16px] bg-raised px-3.5 py-2 text-read leading-6 break-words whitespace-pre-wrap max-[600px]:max-w-[88%]">{words}</div>
      )}
      <div className="mt-0.5 flex items-center gap-1.5 text-small text-muted">
        <span className="tabular-nums">{at ? formatStamp(at) : ""}</span>
        {/* 复制的是用户写的话；只有附件时复制整段（含存放路径） */}
        <CopyButton text={words.trim() ? words : text} />
      </div>
    </div>
  );
}
