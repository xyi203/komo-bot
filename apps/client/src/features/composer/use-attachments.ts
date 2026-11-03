import { uploadAttachment } from "@komo/api";
import { useCallback, useEffect, useRef, useState } from "react";
import { errorText } from "@/lib/errors";

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

/** 附件的增删与上传。上传失败的留在框里标红，用户移除后才能发（不悄悄丢掉一个附件）。 */
export function useAttachments() {
  const [drafts, setDrafts] = useState<Draft[]>([]);
  const live = useRef(new Map<string, { controller: AbortController; preview?: string }>());

  const release = (id: string) => {
    const entry = live.current.get(id);
    if (!entry) return;
    entry.controller.abort();
    if (entry.preview) URL.revokeObjectURL(entry.preview);
    live.current.delete(id);
  };
  useEffect(() => () => [...live.current.keys()].forEach(release), []);

  const add = useCallback((files: readonly File[]) => {
    for (const file of files) {
      const id = crypto.randomUUID();
      const controller = new AbortController();
      const preview = file.type.startsWith("image/") ? URL.createObjectURL(file) : undefined;
      live.current.set(id, { controller, preview });
      setDrafts((all) => [...all, { id, name: file.name || "file", preview, state: "uploading" }]);
      const patch = (change: Partial<Draft>) => setDrafts((all) => all.map((d) => (d.id === id ? { ...d, ...change } : d)));
      uploadAttachment(file, file.name || undefined, controller.signal).then(
        (receipt) => patch({ state: "ready", note: receipt.note }),
        (error: unknown) => {
          if (controller.signal.aborted) return;
          patch({ state: "failed", error: errorText(error) });
        },
      );
    }
  }, []);

  const remove = useCallback((id: string) => {
    release(id);
    setDrafts((all) => all.filter((d) => d.id !== id));
  }, []);

  /** 发出去之后清空：预览不再需要（消息里的缩略图从 Gateway 取）。 */
  const clear = useCallback(() => {
    [...live.current.keys()].forEach(release);
    setDrafts([]);
  }, []);

  return { drafts, add, remove, clear };
}
