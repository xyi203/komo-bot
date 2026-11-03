import { createFileRoute } from "@tanstack/react-router";
import { SettingsEntry } from "@/features/settings/settings-entry";

// 设置是弹窗，不是页（docs/web.md §15）：`/settings` 只当入口，打开弹窗后地址换回底下那一页（没有就回 `/`）。
export const Route = createFileRoute("/_app/settings/")({ component: () => <SettingsEntry /> });
