import { uploadAttachment } from "@komo/api";
import { useCallback, useSyncExternalStore } from "react";
import { errorText } from "@/lib/errors";
import { markFileDraft } from "./draft";

/** 输入框里一个还没发出去的附件。粘进来就开始上传；传完拿到 Gateway 给的那行说明（`note`），发送时拼在正文前面。 */
export interface Draft {
  id: string;
  name: string;
  /** 图片的本地预览（object URL）；不是图片就没有。 */
  preview?: string;
  state: "uploading" | "ready" | "failed";
  note?: string;
  error?: string;
}

// 待发附件按会话存在模块里，不跟着输入框卸载：切到记忆 / 事项页或别的会话再回来还在，传着的接着传。
// 只活在这个页面里（object URL 与上传请求带不过刷新）；文字草稿另存 localStorage（`draft.ts`）。
interface Box {
  drafts: Draft[];
  live: Map<string, { controller: AbortController; preview?: string }>;
  listeners: Set<() => void>;
}
const boxes = new Map<string, Box>();
const EMPTY: Draft[] = [];

function boxOf(key: string): Box {
  let box = boxes.get(key);
  if (!box) {
    box = { drafts: [], live: new Map(), listeners: new Set() };
    boxes.set(key, box);
  }
  return box;
}

function update(key: string, change: (all: Draft[]) => Draft[]) {
  const box = boxOf(key);
  box.drafts = change(box.drafts);
  markFileDraft(key, box.drafts.length > 0);
  box.listeners.forEach((listener) => listener());
}

function release(box: Box, id: string) {
  const entry = box.live.get(id);
  if (!entry) return;
  entry.controller.abort();
  if (entry.preview) URL.revokeObjectURL(entry.preview);
  box.live.delete(id);
}

/** 往某个会话的输入框里加附件并开始上传。输入框之外（如右栏浏览器的截图）也从这里放进来。 */
export function addAttachments(key: string, files: readonly File[]) {
  const box = boxOf(key);
  for (const file of files) {
    const id = crypto.randomUUID();
    const controller = new AbortController();
    const preview = file.type.startsWith("image/") ? URL.createObjectURL(file) : undefined;
    box.live.set(id, { controller, preview });
    update(key, (all) => [...all, { id, name: file.name || "file", preview, state: "uploading" }]);
    const patch = (change: Partial<Draft>) => update(key, (all) => all.map((d) => (d.id === id ? { ...d, ...change } : d)));
    uploadAttachment(file, file.name || undefined, controller.signal).then(
      (receipt) => patch({ state: "ready", note: receipt.note }),
      (error: unknown) => {
        if (controller.signal.aborted) return;
        patch({ state: "failed", error: errorText(error) });
      },
    );
  }
}

/** 附件的增删与上传（按会话）。上传失败的留在框里标红，用户移除后才能发（不悄悄丢掉一个附件）。 */
export function useAttachments(key: string) {
  const subscribe = useCallback(
    (listener: () => void) => {
      const box = boxOf(key);
      box.listeners.add(listener);
      return () => box.listeners.delete(listener);
    },
    [key],
  );
  const drafts = useSyncExternalStore(subscribe, () => boxes.get(key)?.drafts ?? EMPTY);

  const add = useCallback((files: readonly File[]) => addAttachments(key, files), [key]);

  const remove = useCallback(
    (id: string) => {
      release(boxOf(key), id);
      update(key, (all) => all.filter((d) => d.id !== id));
    },
    [key],
  );

  /** 发出去之后清空：预览不再需要（消息里的缩略图从 Gateway 取）。 */
  const clear = useCallback(() => {
    const box = boxOf(key);
    [...box.live.keys()].forEach((id) => release(box, id));
    update(key, () => []);
  }, [key]);

  return { drafts, add, remove, clear };
}
