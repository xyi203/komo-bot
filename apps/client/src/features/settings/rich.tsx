import { useQueryClient } from "@tanstack/react-query";
import { Fragment, type ReactNode } from "react";
import { Notice, TextButton } from "./controls";
import type { EditNotice } from "./use-config-edit";
import { m } from "@/paraglide/messages.js";

/** 译文里的命令与配置键按代码样式写：译文是一整句字符串，这里把已知的机器名切出来包 `<code>`。 */
const CODE = /(`[^`]+`|komo gateway restart|komo auth mcp login [\w.-]+|\.env)/;

export function withCode(text: string): ReactNode {
  return text.split(CODE).map((part, i) =>
    i % 2 ? (
      <code key={i} className="font-mono text-[0.92em]">
        {part.replace(/^`|`$/g, "")}
      </code>
    ) : (
      <Fragment key={i}>{part}</Fragment>
    ),
  );
}

/** 写完之后的回执；409 时给"重新读取"（重取所有配置视图）。 */
export function EditNoticeView({ notice, onReread }: { notice: EditNotice | null; onReread?: () => void }) {
  const queryClient = useQueryClient();
  if (!notice) return null;
  return (
    <Notice bad={notice.bad} className="mt-0 mb-3">
      <p>{withCode(notice.text)}</p>
      {notice.reread && (
        <TextButton
          className="mt-1.5"
          onClick={() => {
            onReread?.();
            void queryClient.invalidateQueries({ queryKey: ["config"] });
          }}
        >
          {m.config_reread()}
        </TextButton>
      )}
    </Notice>
  );
}
