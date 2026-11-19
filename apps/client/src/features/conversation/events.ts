import type { WireEvent } from "@komo/protocol";

// `WireEvent.data` 是 unknown（负载随 `type` 变）：这里给对话与轨迹用到的几种事件写下它们的形状，
// 字段都是可选的——旧 Gateway 少给一格不该让界面崩。

export interface ToolCallPayload {
  call_id: string;
  name: string;
  arguments?: Record<string, unknown>;
}

export interface AssistantPayload {
  text?: string;
  tool_calls?: ToolCallPayload[];
  provider_blocks?: unknown[];
  input_tokens?: number;
  output_tokens?: number;
  cache_read_tokens?: number;
  reasoning_tokens?: number;
}

export interface ToolResultPayload {
  call_id: string;
  status?: string;
  preview?: string;
  elapsed_ms?: number;
}

export interface MemoryUse {
  memory: string;
  /** `convention`（约定）或 `reference`（线索）。 */
  application?: string;
  revision?: number;
  /** 外部提供方条目注入时的正文（记忆库里没有它，见 {@link injectedUses}）。 */
  text?: string;
}

/** 外部提供方条目的 id 带 `<提供方>:` 前缀（`hindsight:…`，docs/memory.md 附录 G）；内置条目没有冒号。 */
export function externalProvider(id: string): string | null {
  const at = id.indexOf(":");
  return at > 0 ? id.slice(0, at) : null;
}

/**
 * `memory.injected` 带上的条目。外部条目不在记忆库里，正文在同一事件的 `external` 里（注入时记下的那句），
 * 并进对应的 use；旧事件没有 `external`。
 */
export function injectedUses(event: WireEvent): MemoryUse[] {
  const { uses = [], external = [] } = payload<{ uses: MemoryUse[]; external: { memory: string; text: string }[] }>(event);
  if (!external.length) return uses;
  const texts = new Map(external.map((x) => [x.memory, x.text]));
  return uses.map((use) => (texts.has(use.memory) ? { ...use, text: texts.get(use.memory) } : use));
}

export interface RunPayload {
  into?: string;
  reason?: string;
}

/** 事件负载按需收窄；没有负载当空对象。 */
export function payload<T>(event: WireEvent): Partial<T> {
  return (event.data && typeof event.data === "object" ? event.data : {}) as Partial<T>;
}

/**
 * 推理摘要：Responses 的 `reasoning.summary`，Anthropic 的 `thinking` 块。
 */
export function thinkingOf(blocks: unknown[] | undefined): string {
  return (Array.isArray(blocks) ? blocks : [])
    .map((block) => {
      const b = block as { type?: string; summary?: { text?: string }[]; thinking?: string };
      if (b.type === "reasoning") return (b.summary ?? []).map((x) => x.text ?? "").join("");
      if (b.type === "thinking") return b.thinking ?? "";
      return "";
    })
    .join("")
    .trim();
}

export const isTerminalRun = (type: string) => /^run\.(completed|failed|cancelled|abandoned)$/.test(type);
