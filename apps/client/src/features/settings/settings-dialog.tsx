import { useConfigCheck, useHealth } from "@komo/api";
import type { ReactNode } from "react";
import { LanguageSwitch } from "@/components/language-switch";
import { Icon, type IconName } from "@/components/icon";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { IconButton } from "@/components/ui/icon-button";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { ChannelsSection } from "./sections/channels-section";
import { ConfigSection } from "./sections/config-section";
import { EnvSection } from "./sections/env-section";
import { GatewaySection } from "./sections/gateway-section";
import { IndexSection } from "./sections/index-section";
import { McpSection } from "./sections/mcp-section";
import { ModelsSection } from "./sections/models-section";
import { closeSettings, SETTINGS_SECTIONS, selectSettingsSection, useSettingsState, type SettingsSection } from "./store";

const ICONS: Record<SettingsSection, IconName> = {
  models: "leaf",
  config: "sliders",
  env: "key",
  index: "search",
  mcp: "plug",
  gateway: "term",
  channels: "chat",
};

const LABELS: Record<SettingsSection, () => string> = {
  models: () => m.settings_sec_models(),
  config: () => m.settings_sec_config(),
  env: () => m.settings_sec_env(),
  index: () => m.settings_sec_index(),
  mcp: () => m.settings_sec_mcp(),
  gateway: () => m.settings_sec_gateway(),
  channels: () => m.settings_sec_channels(),
};

const BODIES: Record<SettingsSection, () => ReactNode> = {
  models: () => <ModelsSection />,
  config: () => <ConfigSection />,
  env: () => <EnvSection />,
  index: () => <IndexSection />,
  mcp: () => <McpSection />,
  gateway: () => <GatewaySection />,
  channels: () => <ChannelsSection />,
};

/**
 * 设置（docs/web.md §15）：尺寸固定的弹窗，换节时外框不动，只换右边；开着它底下的页面照旧。
 * 弹窗关着时不挂载内容，所以每次打开都重读、没提交的表单随关闭丢掉。手机上铺满全屏，分节变成顶上一条横滑的药丸。
 */
export function SettingsDialog() {
  const { open, section } = useSettingsState();
  return (
    <Dialog open={open} onOpenChange={(next) => !next && closeSettings()}>
      {open && (
        <DialogContent
          aria-describedby={undefined}
          className="grid h-[min(680px,calc(100dvh-32px))] w-[min(960px,calc(100vw-32px))] grid-cols-[216px_minmax(0,1fr)] max-[900px]:grid-cols-[180px_minmax(0,1fr)] max-[600px]:h-dvh max-[600px]:w-screen max-[600px]:grid-cols-1 max-[600px]:grid-rows-[auto_minmax(0,1fr)] max-[600px]:rounded-none max-[600px]:shadow-none"
        >
          <DialogDescription className="sr-only">{m.settings_title()}</DialogDescription>
          <Side section={section} />
          <div
            id="settings-main"
            key={section}
            className="min-h-0 min-w-0 overflow-y-auto overscroll-contain px-(--pad) pb-(--pad) [--pad:28px] max-[900px]:[--pad:20px] max-[600px]:[--pad:16px] max-[900px]:pb-6"
          >
            {BODIES[section]()}
            <div className="mt-6 hidden max-[600px]:block">
              <LanguageSwitch />
            </div>
          </div>
          <IconButton
            name="x"
            label={m.settings_close()}
            onClick={closeSettings}
            className="absolute top-4 right-4 z-2 max-[600px]:top-3 max-[600px]:right-3"
          />
        </DialogContent>
      )}
    </Dialog>
  );
}

function Side({ section }: { section: SettingsSection }) {
  const check = useConfigCheck();
  const issues = check.data?.issues.length ?? 0;
  return (
    <div className="flex min-h-0 flex-col gap-3 border-r border-border bg-ground px-3 pt-5 pb-4 max-[600px]:gap-2 max-[600px]:border-r-0 max-[600px]:border-b max-[600px]:px-4 max-[600px]:pt-3.5 max-[600px]:pb-0">
      <DialogTitle className="mx-2.5 mt-0 mb-1 text-dialog-title font-semibold max-[600px]:mt-0.5">{m.settings_title()}</DialogTitle>
      <nav
        aria-label={m.settings_nav()}
        className="flex flex-col gap-0.5 max-[600px]:-mx-4 max-[600px]:flex-row max-[600px]:overflow-x-auto max-[600px]:px-4 max-[600px]:pb-2.5 max-[600px]:[mask-image:linear-gradient(to_right,#000_calc(100%-32px),transparent)] max-[600px]:[scrollbar-width:none]"
      >
        {SETTINGS_SECTIONS.map((key) => (
          <button
            key={key}
            type="button"
            aria-current={key === section ? "page" : undefined}
            onClick={() => selectSettingsSection(key)}
            className={cn(
              "flex h-9 cursor-pointer items-center gap-2.5 rounded-control px-2.5 text-left transition-colors hover:bg-hover max-[600px]:h-8 max-[600px]:shrink-0",
              key === section && "bg-active font-medium",
            )}
          >
            <Icon name={ICONS[key]} className="text-muted" />
            {LABELS[key]()}
            {key === "config" && issues > 0 && (
              <span className="ml-auto h-4.5 min-w-5 rounded-[9px] bg-pill-wait-bg px-1.5 text-center text-[11px] leading-[18px] font-semibold text-pill-wait-ink tabular-nums">
                {issues}
              </span>
            )}
          </button>
        ))}
      </nav>
      <div className="mx-2.5 mt-auto flex flex-col gap-2 max-[600px]:hidden">
        <LanguageSwitch />
        <Status checked={check.isSuccess} failed={check.isError} issues={issues} />
      </div>
    </div>
  );
}

function Status({ checked, failed, issues }: { checked: boolean; failed: boolean; issues: number }) {
  const health = useHealth();
  const version = health.data?.version ?? "";
  const text = failed
    ? m.settings_status_error()
    : checked
      ? issues
        ? m.settings_status_issues({ version, count: issues })
        : m.settings_status_ok({ version })
      : " ";
  return <p className="text-small text-muted">{text}</p>;
}
