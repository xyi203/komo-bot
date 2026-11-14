import { createFileRoute, redirect } from "@tanstack/react-router";

// 旧的事项页已并进目标页（docs/redesign.md §1.4）；存量链接跳过去。
export const Route = createFileRoute("/_app/matters/")({
  beforeLoad: () => {
    throw redirect({ to: "/goals", replace: true });
  },
});
