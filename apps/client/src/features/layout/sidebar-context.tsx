import { createContext, useContext } from "react";

/** 窄屏（≤900）侧栏是抽屉：顶栏的按钮开它，点了会话或入口就关它。宽屏下这两个函数不做事。 */
export interface SidebarContext {
  narrow: boolean;
  openDrawer: () => void;
  closeDrawer: (() => void) | undefined;
}

const Context = createContext<SidebarContext>({ narrow: false, openDrawer: () => {}, closeDrawer: undefined });
export const SidebarProvider = Context.Provider;
export const useSidebarContext = () => useContext(Context);
