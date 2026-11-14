import { useAuthMe } from "@komo/api";
import type { ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { m } from "@/paraglide/messages.js";

/**
 * 登录守卫（docs/web.md §19）：启动时取 `/auth/me`。
 * - 已登录（含本机令牌、桌面端设备令牌）：放行。
 * - 取不到（连不上、旧 Gateway 没有这个接口）：也放行，各页自己的「连不上」提示会说话。
 * - 未登录：桌面壳里只提示一句（登录由桌面端自己的连接设置页负责）；浏览器里配了 OIDC 就是登录页，没配就是说明页。
 * 不自动跳转登录：登录失败时会来回循环，按钮由人点。
 */
export function AuthGate({ children }: { children: ReactNode }) {
  const me = useAuthMe();
  if (me.isPending) return <div aria-busy="true" className="min-h-dvh bg-surface" />;
  if (me.isError || me.data.authenticated) return <>{children}</>;
  if (typeof window !== "undefined" && window.komoDesktop) return <DesktopSignedOut />;
  return me.data.oidc_configured ? <LoginPage /> : <NotConfigured onRecheck={() => void me.refetch()} />;
}

function Logo() {
  return (
    <svg viewBox="0 0 28 28" aria-hidden="true" className="size-12">
      <circle cx="11" cy="12" r="8" className="fill-accent" />
      <circle cx="20.5" cy="18.5" r="4.5" className="fill-accent" fillOpacity="0.55" />
      <circle cx="20" cy="7" r="2.5" className="fill-accent" fillOpacity="0.3" />
    </svg>
  );
}

function Centered({ title, body, children }: { title: string; body: string; children?: ReactNode }) {
  return (
    <main className="grid min-h-dvh place-items-center bg-surface px-6 py-10">
      <div className="flex w-full max-w-sm flex-col items-center gap-3 text-center">
        <Logo />
        <h1 className="mt-2 text-dialog-title font-semibold">{title}</h1>
        <p className="text-read text-muted [overflow-wrap:anywhere]">{body}</p>
        {children && <div className="mt-3 flex flex-wrap justify-center gap-2">{children}</div>}
      </div>
    </main>
  );
}

function LoginPage() {
  // `next` 只带路径与查询串（Gateway 只收站内的 `/路径`）。
  const login = () => location.assign(`/auth/login?next=${encodeURIComponent(location.pathname + location.search)}`);
  return (
    <Centered title={m.login_title()} body={m.login_body()}>
      <Button variant="primary" onClick={login}>
        {m.login_button()}
      </Button>
    </Centered>
  );
}

function NotConfigured({ onRecheck }: { onRecheck: () => void }) {
  return (
    <Centered title={m.login_unconfigured_title()} body={m.login_unconfigured_body()}>
      <Button onClick={onRecheck}>{m.login_recheck()}</Button>
    </Centered>
  );
}

/** 桌面壳里请求带的是设备令牌，正常不会走到这里；走到说明令牌失效，桌面端自己会回它的连接设置页。 */
function DesktopSignedOut() {
  const open = window.komoDesktop?.connection?.openSettings;
  return (
    <Centered title={m.login_desktop_title()} body={m.login_desktop_body()}>
      {open && <Button onClick={() => void open()}>{m.desktop_open_connection()}</Button>}
    </Centered>
  );
}
