import { createFileRoute } from "@tanstack/react-router";
import { GoalDetailPage } from "@/features/goals/goal-detail";

// 目标详情：选中的目标放在地址里；⌘K、记忆页、对话页右上角的小旗跳到这里。
export const Route = createFileRoute("/_app/goals/$id")({
  component: function Page() {
    const { id } = Route.useParams();
    return <GoalDetailPage id={id} />;
  },
});
