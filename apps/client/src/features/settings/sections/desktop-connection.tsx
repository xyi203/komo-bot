import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { CardRow, Field, FormError, KState, Segmented, SettingsCard, TextInput } from "../controls";

type DesktopConnection = NonNullable<NonNullable<Window["komoDesktop"]>["connection"]>;
type Full = DesktopConnection & Required<Pick<DesktopConnection, "get" | "test" | "saveRemote" | "useLocal">>;

/** 桌面壳给了完整的连接接口才算（老版本的壳只有 openSettings，退回跳页）。 */
export function desktopConnection(): Full | undefined {
  const connection = typeof window === "undefined" ? undefined : window.komoDesktop?.connection;
  return connection?.get && connection.test && connection.saveRemote && connection.useLocal ? (connection as Full) : undefined;
}

/** IPC 抛回来的错误带一段 "Error invoking remote method '…': Error: " 前缀，给人看时去掉。 */
function ipcError(error: unknown): string {
  return String((error as Error)?.message ?? error).replace(/^Error invoking remote method '[^']+': (?:Error: )?/, "");
}

/**
 * 桌面端的"连接"一行：右边本机 / 远程切换，选远程就在下面展开地址、令牌、请求头，不再跳到壳的提示页。
 * 切换成功后主进程整页重载（旧 Gateway 的缓存一起丢掉），所以这里只处理失败。
 */
export function DesktopConnectionRow({ api, status }: { api: Full; status: React.ReactNode }) {
  const [info, setInfo] = useState<KomoConnectionInfo | null>(null);
  const [mode, setMode] = useState<"local" | "remote">("local");
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const [headers, setHeaders] = useState("");
  const [busy, setBusy] = useState<"" | "test" | "save">("");
  const [error, setError] = useState("");
  const [test, setTest] = useState<KomoTestResult | null>(null);

  useEffect(() => {
    void api.get().then((next) => {
      setInfo(next);
      setMode(next.mode);
      setUrl(next.remote?.url ?? "");
    });
  }, [api]);

  const run = async (kind: "test" | "save", work: () => Promise<void>) => {
    if (busy) return;
    setBusy(kind);
    setError("");
    setTest(null);
    try {
      await work();
    } catch (failure) {
      setError(ipcError(failure));
    } finally {
      setBusy("");
    }
  };
  const input = () => ({ url, token, headers });
  const doTest = () => run("test", async () => setTest(await api.test(input())));
  const doSave = () =>
    run("save", async () => {
      const saved = await api.saveRemote(input());
      if (!saved.ok) {
        setError(saved.error ?? "");
        setTest(saved.test ?? null);
      }
    });
  const doLocal = () =>
    run("save", async () => {
      const switched = await api.useLocal();
      if (!switched.ok) setError(switched.error ?? "");
    });

  // 选的和正在用的不一样，才需要一个"确认切换"的动作
  const switching = !!info && mode !== info.mode;
  const savedHeaders = info?.remote?.headerNames ?? [];
  return (
    <SettingsCard>
      <CardRow
        title={m.gateway_connection()}
        desc={
          <>
            <span className="font-mono">{info?.baseUrl ?? location.origin}</span>
            <span className="mx-1.5 text-faint">·</span>
            {status}
          </>
        }
        control={
          <Segmented
            label={m.conn_mode()}
            value={mode}
            disabled={!info || !!busy}
            onChange={(next) => {
              setMode(next);
              setError("");
              setTest(null);
            }}
            options={[
              { value: "local", label: m.conn_local() },
              { value: "remote", label: m.conn_remote() },
            ]}
          />
        }
      />
      {mode === "local" && switching && (
        <div className="mt-3 flex items-center justify-between gap-3 rounded-card bg-ground px-4 py-3 max-[600px]:flex-col max-[600px]:items-start">
          <p className="text-label text-muted">{m.conn_local_hint()}</p>
          <Button variant="primary" size="sm" disabled={!!busy} onClick={() => void doLocal()}>
            {busy ? m.conn_switching() : m.conn_use_local()}
          </Button>
        </div>
      )}
      {mode === "remote" && (
        <form
          className="mt-3 grid gap-3 rounded-card bg-ground p-4 max-[600px]:p-3"
          onSubmit={(event) => {
            event.preventDefault();
            void doSave();
          }}
        >
          <Field label={m.conn_url()} htmlFor="conn-url">
            <TextInput id="conn-url" mono value={url} placeholder="https://komo.example.com" spellCheck={false} onChange={(event) => setUrl(event.target.value)} />
          </Field>
          <Field label={m.conn_token()} htmlFor="conn-token" hint={m.conn_token_hint()}>
            <TextInput id="conn-token" type="password" autoComplete="off" value={token} onChange={(event) => setToken(event.target.value)} />
          </Field>
          <Field
            label={m.conn_headers()}
            htmlFor="conn-headers"
            hint={savedHeaders.length ? m.conn_headers_saved({ names: savedHeaders.join(", ") }) : m.conn_headers_hint()}
          >
            <textarea
              id="conn-headers"
              value={headers}
              spellCheck={false}
              placeholder={"CF-Access-Client-Id: …\nCF-Access-Client-Secret: …"}
              onChange={(event) => setHeaders(event.target.value)}
              className="min-h-18 min-w-0 resize-y rounded-control bg-surface px-3 py-2 font-mono text-small text-text shadow-ring-control outline-none placeholder:text-faint focus:shadow-[0_0_0_2px_var(--color-primary)]"
            />
          </Field>
          <div className="flex flex-wrap gap-2">
            <Button type="submit" variant="primary" size="sm" disabled={!!busy || !url.trim()}>
              {busy === "save" ? m.conn_testing() : m.conn_save()}
            </Button>
            <Button size="sm" disabled={!!busy || !url.trim()} onClick={() => void doTest()}>
              {busy === "test" ? m.conn_testing() : m.conn_test()}
            </Button>
          </div>
        </form>
      )}
      {error && <FormError>{error}</FormError>}
      {test && (
        <ul className="mt-3">
          {test.steps.map((step) => (
            <li key={step.name} className={cn("border-t border-rule py-1.5 text-label first:border-t-0", step.ok ? "text-text" : "text-negative")}>
              <KState tone={step.ok ? "ok" : "bad"}>{step.message}</KState>
            </li>
          ))}
        </ul>
      )}
    </SettingsCard>
  );
}
