import { Icon } from "@/components/icon";
import { ImageZoom } from "@/components/image-zoom";
import { m } from "@/paraglide/messages.js";

/**
 * 附件在消息里只是正文开头的几行说明（Gateway 不认识附件，docs/web.md §5）：网页输入框的附件写成这个样子
 * （聊天渠道删除之前收进来的图片在旧会话里也是这个写法，仍要认得出来）。形状与
 * `crates/komo-gateway/src/inbox.rs` 的 `image_note` / `file_note` 逐字对应，改了要一起改。
 */
const IMAGE_NOTE = /^\[用户发来一张图片，已存到 (.+)，用 read 查看\]$/;
const FILE_NOTE = /^\[用户发来一个文件 (.+?)，已存到 (.+)，用 read 查看\]$/;
/** 收件箱里按内容哈希命名的图片才取得回来（`GET /v1/attachments/{收件箱}/{文件名}`）。 */
const INBOX_IMAGE = /[/\\]inbox[/\\]([a-z0-9_-]{1,32})[/\\]([0-9a-f]{16}\.(?:png|jpg|gif|webp))$/;

export type Attachment = { kind: "image"; path: string; url?: string } | { kind: "file"; path: string; name: string };

/** 把正文开头的附件说明拆出来；剩下的是用户自己写的话。 */
export function splitAttachments(text: string): { attachments: Attachment[]; text: string } {
  const lines = text.split("\n");
  const attachments: Attachment[] = [];
  let at = 0;
  for (; at < lines.length; at++) {
    const image = IMAGE_NOTE.exec(lines[at]);
    if (image) {
      const hit = INBOX_IMAGE.exec(image[1]);
      attachments.push({ kind: "image", path: image[1], url: hit ? `/v1/attachments/${hit[1]}/${hit[2]}` : undefined });
      continue;
    }
    const file = FILE_NOTE.exec(lines[at]);
    if (file) {
      attachments.push({ kind: "file", name: file[1], path: file[2] });
      continue;
    }
    break;
  }
  return { attachments, text: lines.slice(at).join("\n") };
}

/** 用户消息里的附件：图片是缩略图（取不回来的退成文件名），点开看大图；其他文件是一枚带文件名的标签；悬停看完整路径。 */
export function AttachmentList({ attachments }: { attachments: readonly Attachment[] }) {
  if (!attachments.length) return null;
  return (
    <div className="flex max-w-[80%] flex-wrap justify-end gap-1.5 max-[600px]:max-w-[88%]">
      {attachments.map((a, i) =>
        a.kind === "image" && a.url ? (
          <ImageZoom key={i} src={a.url} name={a.path.split(/[/\\]/).pop() ?? m.attachment_image()} title={a.path} className="rounded-[12px]">
            <img
              src={a.url}
              alt={m.attachment_image()}
              loading="lazy"
              className="max-h-40 max-w-60 rounded-[12px] bg-raised object-cover shadow-ring"
            />
          </ImageZoom>
        ) : (
          <span key={i} title={a.path} className="inline-flex h-8 max-w-60 items-center gap-1.5 rounded-control bg-raised px-2.5 text-label text-text">
            <Icon name="file" className="size-3.5 shrink-0 text-muted" />
            <span className="truncate">{a.kind === "file" ? a.name : (a.path.split(/[/\\]/).pop() ?? a.path)}</span>
          </span>
        ),
      )}
    </div>
  );
}
