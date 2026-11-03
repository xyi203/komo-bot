import { IconButton } from "@/components/ui/icon-button";
import { m } from "@/paraglide/messages.js";
import { useSidebarContext } from "./sidebar-context";

/** 窄屏（≤900）页头左边的"打开导航"按钮，开侧栏抽屉；宽屏不渲染。 */
export function NavToggle() {
  const { narrow, openDrawer } = useSidebarContext();
  if (!narrow) return null;
  return <IconButton name="sidebar" label={m.nav_open()} onClick={openDrawer} className="traffic-clear-toggle max-[600px]:size-11" />;
}
