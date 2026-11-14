import { useConfigCheck, useConfigModels, useHealth, useMcp } from "@komo/api";
import { useState, type ReactNode } from "react";
import { Icon, type IconName } from "@/components/icon";
import { Dialog, DialogContent, DialogDescription, DialogTitle } from "@/components/ui/dialog";
import { IconButton } from "@/components/ui/icon-button";
import { cn } from "@/lib/utils";
import { m } from "@/paraglide/messages.js";
import { AboutSection } from "./sections/about-section";
import { ConnectSection } from "./sections/connect-section";
import { GeneralSection } from "./sections/general-section";
import { ModelsSection } from "./sections/models-section";
import { closeSettings, SETTINGS_SECTIONS, selectSettingsSection, useSettingsState, type SettingsSection } from "./store";

const ICONS: Record<SettingsSection, IconName> = {
  general: "sliders",
  models: "think",
  connect: "plug",
  about: "details",
};

const LABELS: Record<SettingsSection, () => string> = {
  general: () => m.settings_sec_general(),
  models: () => m.settings_sec_models(),
  connect: () => m.settings_sec_connect(),
  about: () => m.settings_sec_about(),
};

// 左栏搜索：分节名加这一节里各项的名字，输入的字出现在里面就留着
const SEARCH: Record<SettingsSection, () => string[]> = {
  general: () => [m.language_label(), m.config_memory_enabled(), m.config_compaction(), m.settings_index_title()],
  models: () => [m.models_default(), m.models_main(), m.config_memory_model(), m.config_memory_embedding(), m.models_providers(), m.models_add_provider()],
  connect: () => [m.connect_tab_tools(), m.connect_tab_skills(), "MCP"],
  about: () => [m.conn_url(), m.gateway_version(), m.gateway_started(), m.config_check_ok(), m.settings_sec_env(), "Gateway"],
};

const BODIES: Record<SettingsSection, () => ReactNode> = {
  general: () => <GeneralSection />,
  models: () => <ModelsSection />,
  connect: () => <ConnectSection />,
  about: () => <AboutSection />,
};

/**
 * 设置（docs/web.md §15）：尺寸固定的弹窗，照 Claude 桌面设置页：顶栏放标题与关闭，下面左栏分节、右边内容，
 * 两栏同一张白纸，只靠细线分开。换节时外框不动，只换右边；开着它底下的页面照旧。
 * 弹窗关着时不挂载内容，所以每次打开都重读、没提交的表单随关闭丢掉。手机上铺满全屏，分节变成顶上一条横滑的药丸。
 */
export function SettingsDialog() {
  const { open, section } = useSettingsState();
  return (
    <Dialog open={open} onOpenChange={(next) => !next && closeSettings()}>
      {open && (
        <DialogContent
          aria-describedby={undefined}
          className="grid h-[min(680px,calc(100dvh-32px))] w-[min(960px,calc(100vw-32px))] grid-cols-[216px_minmax(0,1fr)] grid-rows-[auto_minmax(0,1fr)] max-[900px]:grid-cols-[180px_minmax(0,1fr)] max-[600px]:h-dvh max-[600px]:w-screen max-[600px]:grid-cols-1 max-[600px]:grid-rows-[auto_auto_minmax(0,1fr)] max-[600px]:rounded-none max-[600px]:shadow-none"
        >
          <DialogDescription className="sr-only">{m.settings_title()}</DialogDescription>
          <header className="col-span-full flex h-14 items-center justify-between border-b border-border pr-3 pl-5 max-[600px]:h-12 max-[600px]:pr-2 max-[600px]:pl-4">
            <DialogTitle className="text-section-title font-semibold">{m.settings_title()}</DialogTitle>
            <IconButton name="x" label={m.settings_close()} onClick={closeSettings} />
          </header>
          <Side section={section} />
          <div
            id="settings-main"
            key={section}
            className="min-h-0 min-w-0 overflow-y-auto overscroll-contain px-(--pad) pb-(--pad) [--pad:28px] max-[900px]:[--pad:20px] max-[600px]:[--pad:16px] max-[900px]:pb-6"
          >
            {BODIES[section]()}
          </div>
        </DialogContent>
      )}
    </Dialog>
  );
}

/** 哪几节有一项要处理：模型里有提供商缺密钥、连接里有服务器要登录或连不上、关于里配置有提醒或连不上 Gateway。 */
function useSectionWarnings(): Record<SettingsSection, boolean> {
  const check = useConfigCheck();
  const models = useConfigModels();
  const mcp = useMcp();
  const health = useHealth();
  return {
    general: false,
    models: !!models.data?.providers.some((p) => !p.key_set && !p.auth),
    connect: !!mcp.data?.servers.some((s) => s.enabled && (s.state === "needs_auth" || s.state === "failed")),
    about: (check.data?.issues.length ?? 0) > 0 || health.isError,
  };
}

function Side({ section }: { section: SettingsSection }) {
  const [query, setQuery] = useState("");
  const warnings = useSectionWarnings();
  const q = query.trim().toLowerCase();
  const shown = SETTINGS_SECTIONS.filter((key) => !q || [LABELS[key](), ...SEARCH[key]()].join("\n").toLowerCase().includes(q));
  return (
    <div className="flex min-h-0 flex-col gap-3 border-r border-border px-3 pt-3 pb-4 max-[600px]:border-r-0 max-[600px]:border-b max-[600px]:px-4 max-[600px]:pt-2.5 max-[600px]:pb-0">
      <label className="flex h-8 items-center gap-2 rounded-control bg-surface px-2.5 text-faint shadow-ring-control focus-within:shadow-[0_0_0_2px_var(--color-focus)] max-[600px]:hidden">
        <Icon name="search" className="size-4" />
        <span className="sr-only">{m.settings_search()}</span>
        <input
          type="search"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder={m.settings_search()}
          autoComplete="off"
          className="min-w-0 flex-1 border-0 bg-transparent text-body text-text outline-0 placeholder:text-faint"
        />
      </label>
      <nav
        aria-label={m.settings_nav()}
        className="flex flex-col gap-0.5 max-[600px]:-mx-4 max-[600px]:flex-row max-[600px]:overflow-x-auto max-[600px]:px-4 max-[600px]:pb-2.5 max-[600px]:[mask-image:linear-gradient(to_right,#000_calc(100%-32px),transparent)] max-[600px]:[scrollbar-width:none]"
      >
        {shown.map((key) => (
          <button
            key={key}
            type="button"
            aria-current={key === section ? "page" : undefined}
            onClick={() => selectSettingsSection(key)}
            className={cn(
              "flex h-9 cursor-pointer items-center gap-2.5 rounded-control px-2.5 text-left transition-colors hover:bg-hover-on-white max-[600px]:h-8 max-[600px]:shrink-0",
              key === section && "bg-active font-medium",
            )}
          >
            <Icon name={ICONS[key]} className="shrink-0 text-muted" />
            <span className="truncate">{LABELS[key]()}</span>
            {warnings[key] && <span role="img" aria-label={m.settings_needs_attention()} title={m.settings_needs_attention()} className="ml-auto size-2 shrink-0 rounded-full bg-wait-dot" />}
          </button>
        ))}
        {shown.length === 0 && <p className="px-2.5 py-2 text-label text-muted">{m.settings_search_none()}</p>}
      </nav>
    </div>
  );
}
