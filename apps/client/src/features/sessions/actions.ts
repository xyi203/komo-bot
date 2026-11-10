import { archiveSession, createSession, forkSession, keys, purgeSession, renameSession, type SessionPages } from "@komo/api";
import type { SessionSummary } from "@komo/protocol";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { errorText } from "@/lib/errors";
import { showToast } from "@/lib/toast";
import { m } from "@/paraglide/messages.js";
import { isArchived } from "./model";

/** 会话的写操作（新建、分叉、改名、归档、永久删除）：做完让会话列表重取，失败说一声。 */
export function useSessionActions() {
  const queryClient = useQueryClient();
  const navigate = useNavigate();

  const refresh = () => queryClient.refetchQueries({ queryKey: keys.sessions.all });
  const open = (session: string) => void navigate({ to: "/sessions/$sessionId", params: { sessionId: session } });

  return {
    open,
    /** 新建；`workdir` 给了就在那个目录新建（工作目录分组头的 +）。 */
    async create(workdir?: string) {
      try {
        const session = await createSession({ request_key: `web:new:${crypto.randomUUID()}`, ...(workdir ? { workdir } : {}) });
        // 先放进列表缓存，目标页马上认得它的标题，再让列表重取
        for (const all of [true, false]) {
          queryClient.setQueryData<SessionPages>(keys.sessions.list(all), (previous) =>
            previous
              ? {
                  ...previous,
                  pages: previous.pages.map((page, i) => ({
                    ...page,
                    sessions: [...(i === 0 ? [session] : []), ...page.sessions.filter((s) => s.session !== session.session)],
                  })),
                }
              : previous,
          );
        }
        void refresh();
        open(session.session);
      } catch (error) {
        showToast(m.session_error_create({ message: errorText(error) }));
      }
    },
    async fork(session: string) {
      try {
        const forked = await forkSession(session);
        await refresh();
        open(forked.session);
      } catch (error) {
        showToast(m.session_error_fork({ message: errorText(error) }));
      }
    },
    /** 改名；空名或没变不发请求。失败说一声。 */
    async rename(session: SessionSummary, name: string) {
      const trimmed = name.trim();
      if (!trimmed || trimmed === session.title) return;
      // 先改列表缓存里的标题（顶栏、侧栏马上换上新名，不闪回旧的），失败了重取回到原样
      for (const all of [true, false]) {
        queryClient.setQueryData<SessionPages>(keys.sessions.list(all), (previous) =>
          previous
            ? {
                ...previous,
                pages: previous.pages.map((page) => ({
                  ...page,
                  sessions: page.sessions.map((s) => (s.session === session.session ? { ...s, title: trimmed } : s)),
                })),
              }
            : previous,
        );
      }
      try {
        await renameSession(session.session, trimmed);
        await refresh();
      } catch (error) {
        void refresh();
        showToast(m.session_error_rename({ message: errorText(error) }));
      }
    },
    /** 归档（`/delete`）；失败抛出，由菜单写在菜单里。 */
    async archive(session: string) {
      await archiveSession(session);
      await refresh();
    },
    /**
     * 永久删除。purge 只认已归档的会话（Gateway 不替人做上一步）：没归档的先立即归档，停掉在跑的。
     * 失败抛出（引用检查不过的原因在错误里），由菜单写在菜单里。
     */
    async purge(session: SessionSummary) {
      if (!isArchived(session)) await archiveSession(session.session, { now: true });
      await purgeSession(session.session);
      await refresh();
    },
  };
}
