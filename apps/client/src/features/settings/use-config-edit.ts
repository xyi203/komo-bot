import { applyConfigEdit, configVersion, isApiError } from "@komo/api";
import type { ConfigEditResponse } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useCallback, useState } from "react";
import { errorText } from "@/lib/errors";
import { m } from "@/paraglide/messages.js";

/** 写配置失败的原因：校验不过（422 / 400）的原因是 Gateway 给的那一句，要原样写出来，其余按 `code` 译。 */
export function editErrorText(error: unknown): string {
  if (isApiError(error) && (error.code === "config_invalid" || error.code === "invalid_request") && error.message) return error.message;
  return errorText(error);
}

/** 写完之后的一句回执：`done` 是这次做了什么，后面接重载的结果（变了几个键、哪些键要重启）。 */
export function editReceipt(done: string, response: ConfigEditResponse, note?: string): string {
  const { changed, start_only } = response.reload;
  const reloaded = changed.length ? m.config_receipt_reloaded_changed({ done, count: changed.length }) : m.config_receipt_reloaded({ done });
  const restart = start_only.length ? m.config_receipt_start_only({ keys: start_only.join(", ") }) : "";
  return [reloaded, restart, note].filter(Boolean).join(" ");
}

export interface EditNotice {
  text: string;
  bad?: boolean;
  /** 409：文件在别处被改过，给"重新读取"。 */
  reread?: boolean;
}

/**
 * 配置、环境变量两节共用的写入：带读到的 version 发请求，成功把三份视图写回缓存并给一句回执，失败按 `key` 记在那一项上。
 * 同一时间只有一个写在飞（`busy` 是它的名字）。
 */
export function useConfigEdit() {
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState("");
  const [failure, setFailure] = useState<{ key: string; text: string } | null>(null);
  const [notice, setNotice] = useState<EditNotice | null>(null);

  const run = useCallback(
    async (
      key: string,
      send: (version: string) => Promise<ConfigEditResponse>,
      done: string,
      options: { note?: string; failAsNotice?: boolean } = {},
    ): Promise<boolean> => {
      const version = configVersion(queryClient);
      if (!version) return false;
      setBusy(key);
      setFailure(null);
      try {
        const response = await send(version);
        applyConfigEdit(queryClient, response);
        setNotice({ text: editReceipt(done, response, options.note) });
        return true;
      } catch (error) {
        if (isApiError(error) && error.code === "version_conflict") setNotice({ text: errorText(error), bad: true, reread: true });
        else if (options.failAsNotice) setNotice({ text: editErrorText(error), bad: true });
        else setFailure({ key, text: editErrorText(error) });
        return false;
      } finally {
        setBusy("");
      }
    },
    [queryClient],
  );

  return { busy, failure, notice, setNotice, setFailure, run };
}
