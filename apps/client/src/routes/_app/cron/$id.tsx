import { createFileRoute } from "@tanstack/react-router";
import { CronPage } from "@/features/cron/cron-page";

// 选中的任务放在地址里；⌘K 与事项页跳到这里，同一页内选中这一个。
export const Route = createFileRoute("/_app/cron/$id")({
  component: function Page() {
    const { id } = Route.useParams();
    return <CronPage selected={id} />;
  },
});
