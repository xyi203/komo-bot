// Database memory tiers; L1 is maintained separately in MEMORY.md.

import type { Memory } from "@/shared/types";

export type Tier = "active" | "candidate" | "archived" | "rejected";

export interface TierStyle {
  /** Short Chinese name, used as the row's standing label. */
  label: string;
  /** One line explaining what the tier means for the model's prompt. */
  meaning: string;
  /** Ring + ground for the row, tuned so each tier reads at a glance. */
  row: string;
  /** The light mote that carries the tier, drawn in `MemoryLight`. */
  mote: string;
}

export const TIER_ORDER: Tier[] = ["active", "candidate", "archived", "rejected"];

export const TIERS: Record<Tier, TierStyle> = {
  active: {
    label: "受光",
    meaning: "可以被回忆检索到",
    row: "border-success/35 bg-success/6",
    mote: "bg-success shadow-[0_0_7px_1px_color-mix(in_oklch,var(--success)_40%,transparent)]",
  },
  candidate: {
    label: "新芽",
    meaning: "刚抽取出来，等待你确认",
    row: "border-border bg-card",
    mote: "bg-muted-foreground/55",
  },
  archived: {
    label: "荫影",
    meaning: "不再进入 prompt，仍留在库里",
    row: "border-border/50 bg-transparent",
    mote: "bg-muted-foreground/25",
  },
  rejected: {
    label: "落叶",
    meaning: "已被否决",
    row: "border-border/40 bg-transparent",
    mote: "bg-muted-foreground/20",
  },
};

export function tierOf(memory: Memory): Tier {
  const status = memory.status.toLowerCase();
  if (status === "active") return "active";
  if (status === "candidate") return "candidate";
  if (status === "rejected") return "rejected";
  return "archived";
}

/** Chinese labels for every `MemoryKind` variant (komo-core's domain::memory). */
const KINDS: Record<string, string> = {
  profile: "画像",
  preference: "偏好",
  feedback: "反馈",
  project: "项目",
  person: "人物",
  fact: "事实",
  decision: "决定",
  reference: "线索",
};

export function kindLabel(kind: string): string {
  return KINDS[kind.toLowerCase()] ?? kind;
}

/** How komo came by this memory — every `MemoryConfidence` variant. Worth
 *  showing plainly: a guess the model made and something you wrote yourself
 *  deserve different trust when you are deciding whether to keep it. */
const CONFIDENCE: Record<string, string> = {
  extracted: "对话中抽取",
  inferred: "模型推断",
  confirmed: "已确认",
  user_written: "你亲手写的",
};

export function confidenceLabel(confidence: string): string {
  return CONFIDENCE[confidence.toLowerCase()] ?? confidence;
}

/** Actions that make sense for a memory in this tier, in the order shown.
 *
 *  Offering every verb on every row is what the old panel did — a `reject`
 *  button on an already-rejected memory is noise the operator has to read past. */
export function actionsFor(tier: Tier): ("promote" | "reject")[] {
  switch (tier) {
    case "active":
      return ["reject"];
    case "candidate":
      return ["promote", "reject"];
    default:
      return ["promote"];
  }
}

export const ACTION_LABELS: Record<"promote" | "reject", string> = {
  promote: "转为受光",
  reject: "否决",
};
