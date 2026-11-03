import { m } from "@/paraglide/messages.js";

/** effort 档位的显示名；Gateway 给了不认识的档位就原样写。 */
export function effortLabel(effort: string): string {
  switch (effort) {
    case "none":
      return m.effort_none();
    case "minimal":
      return m.effort_minimal();
    case "low":
      return m.effort_low();
    case "medium":
      return m.effort_medium();
    case "high":
      return m.effort_high();
    case "xhigh":
      return m.effort_xhigh();
    case "max":
      return m.effort_max();
    default:
      return effort;
  }
}

/** 模型显示名去掉括号里的补充说明（`GPT（旗舰）` → `GPT`），下拉里短一点。 */
export const modelLabel = (name: string): string => name.replace(/（.*）/, "");
