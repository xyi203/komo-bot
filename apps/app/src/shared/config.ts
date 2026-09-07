// Every timing knob in one place. Polling is how the renderer learns about
// state it can't be pushed (gateway liveness, pending approvals) — the gateway
// streams only tool-call frames over SSE.

/** Refetch/poll intervals, milliseconds. */
export const POLL = {
  /** Gateway liveness probe (attach when it starts, offline when it stops). */
  connection: 3_000,
  /** Session list in the sidebar. */
  sessions: 6_000,
  /** Settings dashboard tabs (status / memories / runs). */
  dashboard: 6_000,
  /** Pending approval + clarify question, while a turn is in flight. */
  interactions: 1_000,
} as const;

/** Request timeouts, milliseconds. */
export const TIMEOUT = {
  /** `/health` probe — must fail fast so the UI flips to offline promptly. */
  probe: 2_000,
  /** Any real request. Long, because an interactive turn blocks server-side
   *  while a human approves a tool (the gateway's approval timeout is 5 min). */
  request: 600_000,
} as const;

/** Interactions polling: a single failure is transient (the gateway is busy
 *  running the turn), so back off and retry rather than giving up — losing the
 *  poll would leave an approval prompt invisible for the rest of the turn. */
export const INTERACTIONS_BACKOFF_MS = [1_000, 2_000, 4_000, 8_000, 8_000] as const;
