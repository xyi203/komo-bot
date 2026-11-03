import { createFileRoute } from "@tanstack/react-router";
import { MattersPage } from "@/features/matters/matters-page";

// 选中的事项放在地址里；⌘K 与右栏跳到这里，同一页内选中这一件。
export const Route = createFileRoute("/_app/matters/$id")({
  component: function Page() {
    const { id } = Route.useParams();
    return <MattersPage selected={id} />;
  },
});
