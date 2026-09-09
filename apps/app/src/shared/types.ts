// Mirror DTOs for the gateway's `/api/*` responses. Kept loose (enum fields as
// plain strings) — the GUI only displays them, so exact variant typing isn't
// worth coupling to the Rust definitions.

export interface StatusSnapshot {
  ok: boolean;
  version: string;
  channels: string[];
  home_chat: string | null;
  provider?: string;
  model?: string;
  context_window?: number | null;
  token_usage?: number | null;
  sessions: number;
}

/** A long-term memory, verbatim from `GET /api/memories`.
 *
 *  The gateway serializes the whole domain struct, so the usage signals the
 *  memory system runs on — how often a memory was recalled, and when it was last
 *  pulled into a prompt — arrive with every row. They are what the canopy
 *  encodes as light. */
export interface Memory {
  id: string;
  kind: string;
  content: string;
  status: string;
  confidence: string;
  pinned: boolean;
  importance: number;
  /** Times this memory was injected into a prompt by recall. */
  recall_count: number;
  /** Unix seconds of the last recall injection; null if never recalled. */
  last_used_at: number | null;
  created_at: number;
  updated_at: number;
  /** Session id the memory was extracted from. */
  source: string;
}

export interface Run {
  id: string;
  session_id: string;
  input: string;
  plan: string;
  status: string;
  recoverable: boolean;
  started_at: number;
  ended_at: number | null;
  final_output: string;
  error: string;
}

export interface RunStep {
  seq: number;
  tool_name: string;
  args: string;
  result: string;
  error: string;
  ok: boolean;
  /** Measured call duration. 0 on steps recorded before the column existed, and
   *  absent entirely from a gateway older than the field — both mean "unknown",
   *  never "instant". */
  elapsed_ms?: number;
}

export interface RunDetail {
  run: Run;
  steps: RunStep[];
}

export interface SessionMessage {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  timestamp: number;
}

export interface SessionSummary {
  id: string;
  /** Canonical absolute directories this session is bound to, `roots[0]` first.
   *  Empty (or absent, from a gateway older than the field) = no binding: the
   *  home session, a chat channel's session, a remote caller's. The gateway
   *  binds them on the session's first turn and ignores the header after. */
  roots?: string[];
  created_at: number;
  messages: number;
  user_turns: number;
  title?: string;
  /** "active" | "archive" (deleted sessions are omitted from the list). */
  status?: string;
  /** Model this session last ran on; empty/absent = the gateway default. Unlike
   *  `roots` this is switchable mid-conversation. */
  model?: string;
  /** Reasoning effort; empty/absent = the provider default. */
  effort?: string;
}

/** One selectable model.
 *
 *  `id` is what the client sends back and what the session stores — it may be
 *  provider-qualified (`deepseek:deepseek-chat`), which is what routes the turn
 *  to another backend; `model` is the bare id the provider sees.
 *  `context_window` is null for ids the gateway has no known capacity for — it
 *  must read as "unknown", never as zero. `efforts` is **per model**, because a
 *  cross-provider menu mixes models that have an effort scale with ones that
 *  don't (codex has three levels, deepseek none). */
export interface ModelOption {
  id: string;
  provider: string;
  model: string;
  context_window: number | null;
  efforts: string[];
}

/** What a session may be switched to (`GET /api/models`). Models whose provider
 *  has no configured credential are absent — the gateway never offers one that
 *  would error on every turn. */
export interface ModelMenu {
  /** The default provider's name; each entry names its own. */
  provider: string;
  default_model: string;
  models: ModelOption[];
}

export interface PendingApproval {
  summary: string;
  detail: string | null;
  risk: string;
}

export interface Interactions {
  approval: PendingApproval | null;
  question: string | null;
}

/** Turn trust mode: interactive suspends on approval/clarify, trusted
 *  auto-approves side-effecting tools (what `komo chat` does). */
export type Mode = "interactive" | "trusted";

export interface WorkspaceInfo {
  id: string;
  name: string;
  path: string;
}

/** A folder the host picked through its native directory dialog. Only a host
 *  with OS access supplies one (the desktop shell); the web build has none, and
 *  the picker entry then never renders. */
export type FolderPicker = () => Promise<{ name: string; path: string } | null>;
