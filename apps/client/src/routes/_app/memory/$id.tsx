import { createFileRoute } from "@tanstack/react-router";
import { MemoryPage } from "@/features/memory/memory-page";

// 选中的记忆放在地址里；刷新或分享链接都回到同一条。
export const Route = createFileRoute("/_app/memory/$id")({ component: Page });

function Page() {
  const { id } = Route.useParams();
  return <MemoryPage selected={id} />;
}
