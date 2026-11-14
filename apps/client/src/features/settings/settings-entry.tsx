import { useNavigate } from "@tanstack/react-router";
import { useEffect } from "react";
import { openSettings, settingsSectionOf } from "./store";

/** `/settings[/节]` 的入口：开弹窗，再把地址换回 `/`（弹窗压在底下那一页上，不换地址、不断事件流）。 */
export function SettingsEntry({ section }: { section?: string }) {
  const navigate = useNavigate();
  useEffect(() => {
    openSettings(settingsSectionOf(section) ?? "general");
    void navigate({ to: "/", replace: true });
  }, [section, navigate]);
  return null;
}
