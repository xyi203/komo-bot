import { createFileRoute } from "@tanstack/react-router";
import { InboxPage } from "@/features/inbox/inbox-page";

// 选中的待处理放在地址里（句柄就是 Run ID）；刷新或分享链接都回到同一条。
export const Route = createFileRoute("/_app/inbox/$id")({ component: Page });

function Page() {
  const { id } = Route.useParams();
  return <InboxPage selected={id} />;
}
