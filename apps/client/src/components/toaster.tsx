import { Icon } from "@/components/icon";
import { dismissToast, useToasts } from "@/lib/toast";
import { m } from "@/paraglide/messages.js";

/** 瞬时提示（操作失败的一句话），固定在窗口底部中间。 */
export function Toaster() {
  const toasts = useToasts();
  if (!toasts.length) return null;
  return (
    <div role="status" className="fixed inset-x-0 bottom-20 z-50 flex flex-col items-center gap-2 px-4">
      {toasts.map((toast) => (
        <div key={toast.id} className="flex max-w-lg items-start gap-2 rounded-card bg-surface py-2.5 pr-2 pl-3.5 text-label leading-5 shadow-[0_0_0_1px_rgb(34_40_28/0.12),0_8px_24px_rgb(34_40_28/0.12)]">
          <span className="min-w-0 break-words">{toast.text}</span>
          <button
            type="button"
            aria-label={m.dismiss()}
            onClick={() => dismissToast(toast.id)}
            className="grid size-5 shrink-0 cursor-pointer place-items-center rounded-xs text-muted hover:bg-hover-on-white hover:text-text"
          >
            <Icon name="x" className="size-3.5" />
          </button>
        </div>
      ))}
    </div>
  );
}
