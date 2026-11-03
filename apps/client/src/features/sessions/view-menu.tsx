import { Icon, type IconName } from "@/components/icon";
import { IconButton } from "@/components/ui/icon-button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { m } from "@/paraglide/messages.js";
import type { FilterMode, GroupMode, SortMode } from "./model";

export interface ViewPrefs {
  group: GroupMode;
  sort: SortMode;
  filter: FilterMode;
  setGroup: (value: GroupMode) => void;
  setSort: (value: SortMode) => void;
  setFilter: (value: FilterMode) => void;
}

/** 会话区头的"分组、排序与筛选"菜单；选择存 localStorage（`komo.group` / `komo.sort` / `komo.filter`）。 */
export function ViewMenu({ prefs }: { prefs: ViewPrefs }) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <IconButton
          name="sliders"
          label={m.view_menu()}
          size="compact"
          className="hover:bg-hover aria-expanded:bg-hover aria-expanded:text-text"
        />
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" aria-label={m.view_menu()}>
        <Section
          title={m.view_group()}
          value={prefs.group}
          onChange={(v) => prefs.setGroup(v as GroupMode)}
          items={[
            ["tree", m.view_group_tree(), "branch"],
            ["workdir", m.view_group_workdir(), "folder"],
            ["flat", m.view_group_flat(), "list"],
          ]}
        />
        <DropdownMenuSeparator />
        <Section
          title={m.view_sort()}
          value={prefs.sort}
          onChange={(v) => prefs.setSort(v as SortMode)}
          items={[
            ["updated", m.view_sort_updated(), "clock"],
            ["created", m.view_sort_created(), "calendar"],
          ]}
        />
        <DropdownMenuSeparator />
        <Section
          title={m.view_filter()}
          value={prefs.filter}
          onChange={(v) => prefs.setFilter(v as FilterMode)}
          items={[
            ["active", m.view_filter_active(), "eye-off"],
            ["all", m.view_filter_all(), "chat"],
            ["archived", m.view_filter_archived(), "archive"],
          ]}
        />
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

function Section({
  title,
  value,
  onChange,
  items,
}: {
  title: string;
  value: string;
  onChange: (value: string) => void;
  items: [string, string, IconName][];
}) {
  return (
    <DropdownMenuRadioGroup value={value} onValueChange={onChange}>
      <DropdownMenuLabel>{title}</DropdownMenuLabel>
      {items.map(([id, label, icon]) => (
        <DropdownMenuRadioItem key={id} value={id}>
          <Icon name={icon} className="text-muted" />
          <span>{label}</span>
        </DropdownMenuRadioItem>
      ))}
    </DropdownMenuRadioGroup>
  );
}
