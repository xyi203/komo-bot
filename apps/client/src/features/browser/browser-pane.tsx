import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";
import { Icon } from "@/components/icon";
import { Button } from "@/components/ui/button";
import { IconButton } from "@/components/ui/icon-button";
import { addAttachments } from "@/features/composer/use-attachments";
import { readStored, writeStored } from "@/lib/storage";
import { showToast } from "@/lib/toast";
import { m } from "@/paraglide/messages.js";

type DesktopBrowser = NonNullable<NonNullable<Window["komoDesktop"]>["browser"]>;
type DevServer = Awaited<ReturnType<DesktopBrowser["detectDevServers"]>>[number];

const KEY = "komo.browserOpen";

/** 桌面壳的浏览器接口；网页里（没有壳）是 undefined，整个功能不出现。 */
export function desktopBrowser(): DesktopBrowser | undefined {
  return typeof window === "undefined" ? undefined : window.komoDesktop?.browser;
}

/** 右栏浏览器开没开：记在本机，换会话不变（网页本身也是全窗口一份，不跟会话走）。 */
export function useBrowserOpen() {
  const [open, setOpen] = useState(() => !!desktopBrowser() && readStored(KEY, (raw) => raw === 1 || raw === "1", false));
  const set = useCallback((next: boolean) => {
    writeStored(KEY, next ? "1" : "0");
    setOpen(next);
  }, []);
  return [open, set] as const;
}

/** IPC 抛回来的错误带一段 "Error invoking remote method '…': Error: " 前缀，给人看时去掉。 */
function ipcError(error: unknown): string {
  return String((error as Error)?.message ?? error).replace(/^Error invoking remote method '[^']+': (?:Error: )?/, "");
}

/**
 * 右栏浏览器（桌面壳专有）：这里只画工具条和占位框，网页是主进程的原生视图，按占位框的位置盖上来。
 * 原生视图压在所有 DOM 之上，所以开着弹窗（设置、⌘K）时先把它藏起来，不然会挡住弹窗。
 * 截图放进当前会话输入框的附件里，发出去 agent 就能看到。
 */
export function BrowserPane({ api, sessionId, onClose }: { api: DesktopBrowser; sessionId: string; onClose: () => void }) {
  const [state, setState] = useState<KomoBrowserState | null>(null);
  const [address, setAddress] = useState("");
  const [editing, setEditing] = useState(false);
  const [notice, setNotice] = useState("");
  const [overlay, setOverlay] = useState(false);
  const frame = useRef<HTMLDivElement>(null);

  useEffect(() => {
    void api.state().then(setState);
    return api.onState(setState);
  }, [api]);
  useEffect(() => {
    if (!editing) setAddress(state?.url ?? "");
  }, [state?.url, editing]);

  // Radix 的弹窗与下拉菜单挂在 body 下：只看 body 的直接子节点变化，不跟着对话流式输出跑
  useEffect(() => {
    const check = () => setOverlay(!!document.querySelector('[role="dialog"], [role="alertdialog"], [role="menu"]'));
    check();
    const observer = new MutationObserver(check);
    observer.observe(document.body, { childList: true });
    return () => observer.disconnect();
  }, []);

  const showing = !!state?.url && !state.error && !overlay;
  useEffect(() => {
    const el = frame.current;
    if (!showing || !el) {
      void api.setBounds(null);
      return;
    }
    let last = "";
    const report = () => {
      const r = el.getBoundingClientRect();
      const rect = { x: r.left, y: r.top, width: r.width, height: r.height };
      const sig = JSON.stringify(rect);
      if (sig === last) return;
      last = sig;
      void api.setBounds(rect);
    };
    report();
    const observer = new ResizeObserver(report);
    observer.observe(el);
    window.addEventListener("resize", report);
    // 侧栏收起之类只挪位置不改尺寸，ResizeObserver 收不到：再轮询兜底
    const timer = window.setInterval(report, 300);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", report);
      window.clearInterval(timer);
      void api.setBounds(null);
    };
  }, [api, showing]);

  const go = useCallback(
    async (input: string) => {
      setNotice("");
      const result = await api.navigate(input);
      if (!result.ok) setNotice(result.error ?? "");
    },
    [api],
  );

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (!address.trim()) return;
    (document.activeElement as HTMLElement | null)?.blur();
    void go(address);
  };

  const capture = async () => {
    try {
      const shot = await api.capture();
      let host = "page";
      try {
        host = new URL(shot.url).host.replace(/[^\w.-]+/g, "-") || host;
      } catch {
        // 地址解析不了就用默认名
      }
      const stamp = new Date().toISOString().slice(0, 19).replace(/[-:T]/g, "");
      addAttachments(sessionId, [new File([new Uint8Array(shot.png)], `${host}-${stamp}.png`, { type: "image/png" })]);
      showToast(m.browser_captured());
    } catch (error) {
      showToast(m.browser_capture_failed({ message: ipcError(error) }));
    }
  };

  const error = notice || state?.error;
  return (
    <section aria-label={m.browser_title()} className="flex min-h-0 min-w-0 flex-col border-l border-border">
      <div className="flex h-11 shrink-0 items-center gap-0.5 border-b border-border px-2">
        <IconButton name="arrow-left" label={m.browser_back()} disabled={!state?.canGoBack} onClick={() => void api.act("back")} className={DISABLED} />
        <IconButton name="arrow-right" label={m.browser_forward()} disabled={!state?.canGoForward} onClick={() => void api.act("forward")} className={DISABLED} />
        {state?.loading ? (
          <IconButton name="x" label={m.browser_stop()} onClick={() => void api.act("stop")} />
        ) : (
          <IconButton name="reload" label={m.browser_reload()} disabled={!state?.url} onClick={() => void api.act("reload")} className={DISABLED} />
        )}
        <form onSubmit={submit} className="mx-1 min-w-0 flex-1">
          <input
            type="text"
            value={address}
            aria-label={m.browser_address()}
            placeholder={m.browser_address_placeholder()}
            spellCheck={false}
            autoCapitalize="off"
            autoCorrect="off"
            onFocus={(event) => {
              setEditing(true);
              event.currentTarget.select();
            }}
            onBlur={() => setEditing(false)}
            onChange={(event) => setAddress(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Escape") {
                setAddress(state?.url ?? "");
                event.currentTarget.blur();
              }
            }}
            className="h-8 w-full min-w-0 rounded-control bg-ground px-3 text-label text-text outline-none placeholder:text-faint focus-visible:shadow-[0_0_0_2px_var(--color-focus)]"
          />
        </form>
        <IconButton name="camera" label={m.browser_capture()} disabled={!showing} onClick={() => void capture()} className={DISABLED} />
        <IconButton name="x" label={m.browser_close()} onClick={onClose} />
      </div>
      {error && <p className="m-0 shrink-0 border-b border-rule px-4 py-2 text-label text-negative [overflow-wrap:anywhere]">{m.browser_load_failed({ message: error })}</p>}
      <div ref={frame} className="relative min-h-0 flex-1">
        {!state?.url && <Empty onOpen={(url) => void go(url)} detect={api.detectDevServers} />}
      </div>
    </section>
  );
}

const DISABLED = "disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-muted";

function Empty({ onOpen, detect }: { onOpen: (url: string) => void; detect: DesktopBrowser["detectDevServers"] }) {
  const [servers, setServers] = useState<DevServer[] | "loading" | null>(null);
  const scan = async () => {
    setServers("loading");
    try {
      setServers(await detect());
    } catch {
      setServers([]);
    }
  };
  return (
    <div className="absolute inset-0 grid place-items-center overflow-auto p-6">
      <div className="max-w-90 text-center">
        <Icon name="globe" className="mx-auto mb-3 size-6 text-muted" />
        <h3 className="m-0 text-read font-medium">{m.browser_empty_title()}</h3>
        <p className="mt-1.5 mb-4 text-label leading-5 text-muted">{m.browser_empty_body()}</p>
        {Array.isArray(servers) && servers.length > 0 ? (
          <ul className="text-left">
            {servers.map((server) => (
              <li key={server.port} className="border-t border-rule first:border-t-0">
                <button
                  type="button"
                  onClick={() => onOpen(server.url)}
                  className="flex w-full cursor-pointer items-baseline gap-3 rounded-control px-2 py-2 text-left transition-colors hover:bg-hover-on-white"
                >
                  <span className="font-mono text-label">localhost:{server.port}</span>
                  <span className="min-w-0 truncate text-label text-muted">{server.title}</span>
                </button>
              </li>
            ))}
          </ul>
        ) : (
          <>
            <Button size="sm" disabled={servers === "loading"} onClick={() => void scan()}>
              {servers === "loading" ? m.browser_detecting() : m.browser_detect()}
            </Button>
            {Array.isArray(servers) && <p className="mt-3 text-label text-muted">{m.browser_detect_none()}</p>}
          </>
        )}
      </div>
    </div>
  );
}
