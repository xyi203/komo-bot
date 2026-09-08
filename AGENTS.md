# AGENTS.md

Guidance for coding agents working in this repository.
`CLAUDE.md` is a symlink to this file — edit `AGENTS.md` only.

komo is a personal-agent framework in Rust (DDD-style layers) plus a bun
workspace of JS/TS clients under `apps/`. Building needs `protoc`
(`brew install protobuf` — feishu websocket frames are protobuf).

## Commands

```bash
cargo check / build / fmt
cargo test --workspace             # REQUIRED: bare `cargo test` skips komo-core's ~70 tests
cargo test tools::time             # single module

komo init                          # scaffold ~/.komo (config.toml/.env/SOUL.md/USER.md; never overwrites)
cargo run -- chat                  # full-screen TUI (needs a terminal; scripts use the api channel)
cargo run -- gateway               # always-on process: sweeps + channels (feishu/telegram/wechat)
komo gateway start|stop|restart|status   # macOS launchd supervision
komo upgrade [--no-restart]        # git pull --ff-only + cargo install + restart gateway
komo logs [-n N] [-f] [--stdout]   # tail gateway tracing log
komo doctor                        # config & gateway health
komo health                        # liveness probe (exit 0 = healthy; Docker HEALTHCHECK)

komo memory list|search|promote|reject|triage|backfill
komo wiki index [--rebuild]|search|status   # note-vault index (needs `[wiki]`; index is incremental)
komo dream [--apply]               # evidence-driven candidate consolidation (preview by default)
komo cron list|add|add-agent [--skill NAME] [--workspace DIR] [--grant c:m:v]|run|enable|disable|remove
komo run list|inspect|prune        # run ledger (⟲ = interrupted and unclaimed)
komo session list|resume|clean     # stored sessions (`komo resume <id>` is the shortcut)
komo skills list|install|inspect|enable|disable
komo policy list|check|saved       # permission policy: config rules + job grants + saved grants
komo channel list|probe            # channel inventory / verification
komo channel wechat login          # provision WeChat creds via QR (on the host)
komo pair approve|revoke|list      # admit chat senders
```

**Anything that touches komo's state goes through the gateway**, which the
command starts if none is running (see "Gateway is the process" below). The
gateway-free commands are `init`, `gateway *`, `logs`, `upgrade`, `health`,
`skills *`, `model *`, `channel list|probe|wechat login` and
`policy check|saved`; `doctor` probes for a gateway but never starts one.

Logs: `init_tracing` in `main.rs` installs the subscriber (without it every
`info!` is a no-op). Gateway tees stderr into daily-rotated
`~/.komo/logs/gateway.YYYY-MM-DD.log` (what `komo logs` reads). Level via
`KOMO_LOG` (default `info,toasty=warn`; set `KOMO_LOG=debug` to see full tool
results). Turns run in `run` spans, tool calls in `tool` spans,
matching the run ledger. At `info` a turn tells its whole story inside that
span: `turn started` (origin, channel platform, kind, prompt size, history
depth) → `memory recall` (fetched/injected, from prompt assembly) → one
`model round completed` per round (provider/model, `tool_calls`, `text_chars`,
tokens, `elapsed_ms` — a round 1 with `tool_calls=0` is a model answering out of
its own head) → `tool ok` / `tool failed` per call → `run done` with `rounds`,
`tool_calls`, tokens and `elapsed_ms`. Counts, sizes and durations only — no message text, memory text or
tool arguments at any level. The chat TUI logs to `~/.komo/logs/chat-tui.log`
instead (stderr would tear the alternate screen) and registers that path with
`komo_infra::logs::set_active`, which is how the `logs` tool finds the current
process's own log mid-conversation.

## Data & storage rules

**One database, table-level durability** (docs/adr/0004). `~/.komo/komo.db`
holds everything Turso stores; "disposable" and "durable" are properties of each
*table*, not of which file it sits in. The three files it replaced
(`state.db`, `memory.db`, `cron.db`) were imported into it once; that import
code is gone — an absent `komo.db` is created, a present one is used.

| Where | Contents | Durability |
|---|---|---|
| `komo.db` · `session_records`, `session_todo_records`, `pairing_records`, `setting_records`, `inbox_records`, run ledger (`run_records`, `run_step_records`) | one turn's execution record and the session metadata around it | disposable **by row** — `komo run prune`, `komo session clean`; never by dropping the table |
| `komo.db` · `memory_records` | long-term memories | durable — **additive changes only** |
| `komo.db` · `cron_job_records` | routines: a `Trigger`, an action, and the last 20 `RoutineRun`s | durable — **additive changes only**; `schedule` / `last_*` are retired columns kept (and written empty) because dropping one is not additive |
| `komo.db` · `wakeup_records` | standing wakeups — one row per suspended turn's wait | durable — same retired-column rule: `at` / `task_id` / `filter` outlived the timer and event waits and are written `0` / `""` |
| `komo.db` · `chunk_records` | the hybrid search index over the note vault and over komo's own transcripts, split by a `collection` column | disposable **by collection** — every row is reproducible from its source file or transcript |
| `~/.komo/sessions/<id>/` | transcripts — a manifest plus append-only `.jsonl` segments | disposable |
| `~/.komo/permissions.json` | saved approval grants | durable |
| `~/.komo/tool-output/` | over-limit tool results + per-session `index.jsonl` (7-day retention) | disposable |
| `~/.komo/artifacts/<session>/` | what a turn *produced* — reports, scripts, downloads | durable — never swept; a writable workspace root |
| `~/.komo/MEMORY.md` | operator-edited global L1; never generated from database pins or changed by Dream | durable |
| `~/.komo/skills/` | skill files (filesystem is the source of truth) | durable |
| `~/.komo/plugins/` | `*.py` plugin tools the operator (or the agent, under approval) authored | durable — a writable workspace root, and the only one where a write is `Risk::Dangerous` |

Transcripts are **files, not rows** (`persistence/session_log.rs`), because they
are the one thing here that is purely appended — so they pay no schema cost: a
field added later reads as its default on every line written before it existed,
and a change deeper than that dispatches on the line's `v`. Session *metadata*
stays a row because it is *updated* (title, status, model).
`MessageRepository` is the log; `SessionRepository` reads the two together.
Anything that used to count messages in SQL must now go through the log — the review
sweep and `mark_reviewed`'s clamp are the two that do, and a missed one pins
every watermark at zero.

**The log records what happened; `fold` decides what it means.** A cancelled
turn and a mid-turn interjection used to rewrite the file (delete the user
message / edit it); both are now lines appended at the end, and one pure
function resolves them on read. That is also where the invariant a reader
depends on lives — user and assistant must alternate, because several providers
reject two consecutive user messages on replay. Keeping that true at each write
site took three separate patches; it is now one function, testable without a
database. **Add a new read path through `projected`, never `entries`.**

An event `type` this build has no vocabulary for decodes to an inert
`SessionEventKind::Unknown` that **keeps its seq** and means nothing — a
downgrade, or a line a since-retired mechanism wrote, must not renumber every
event after it or make the session unreadable. A *known* type whose payload will
not parse still refuses: that is corruption, not vocabulary.

**A transcript and a replay are different reads of one surface.**
`SurfaceProjection::messages()` is the whole conversation — what `komo run
inspect`, episodic indexing, the reviewer and a client hydrating a window all
read. `replayed()` / `replay()` is what the *model* is handed, and the only
thing that separates them is the newest `conversation/boundary`: `/new` appends
one (it does not rotate the session, archive anything, or end a turn), and the
replay starts after the last assistant node at or before it. The cut lands
there rather than on the boundary itself so a turn that was still open — one
suspended on an approval — keeps the unanswered user message its continuation
replies to. `find_windowed` and compaction both read `replayed()`; a summary
that reached across a boundary would put the shadowed stretch straight back in
front of the model. `RetentionBase::cut` keeps the newest boundary alongside
the header/context envelope, because it is not a surface node and losing it
loses the line.

A **windowed** read (`find_windowed`, every turn) is served from the session's
**surface checkpoint** (`sessions/<id>/surface.json`, `SurfaceProjection`) plus
the events appended since — reading a whole conversation to discard all but its
last few messages costs IO and parsing on the reply path that grows for as long
as the session lives. The checkpoint is a **cache, never an authority**: a
version mismatch, a retention cut that moved `truncated_before`, an unparseable
file, or a tail that will not fold onto it all mean "re-fold the log", and the
log always wins. It is refreshed at `turn_boundary` (best-effort), and folding a
prefix then the rest provably gives the same history as folding everything —
at *any* split point, which is why `SurfaceContent` carries the turn that put
each node on the surface.

Schema-change rules (toasty's `push_schema` runs only for **new** db files, and
is not idempotent — and there is one file now, so "delete it to reset" is no
longer available for anything):

- **Column additions never need a reset**: `komo-infra/src/persistence/mod.rs::ensure_columns`
  ALTERs in place on connect. Extend the list next to the model — `EXPECTED` in
  `memory_db.rs` and `cron.rs`, `SESSION_COLUMNS` / `RUN_COLUMNS` /
  `STEP_COLUMNS` in `db.rs::connect` — and add an `ensure_schema` for a table
  that has none yet. Columns must be NOT NULL + DEFAULT, or
  nullable.
- **A new table** is added with `ensure_table` (its DDL kept beside the model,
  byte-parity locked by a test — see `INBOX_TABLE_DDL`), because an existing
  `komo.db` will not re-run `push_schema`. `chunk_records` is the one table komo
  talks to in SQL rather than through the ORM, so it owns its own
  `CHUNK_TABLE_DDL` and needs no parity test.
- **A non-additive change** to a durable table (`memory_records`,
  `cron_job_records`, `wakeup_records`) is not available: those may only ever
  change additively. On a disposable table it is a **row-level** migration,
  never a dropped file. A retired column keeps being written (empty) rather
  than dropped — see `cron_job_records.schedule`.
- **A `Message` field change needs neither**: it is a JSONL line, not a column.

Turso/toasty invariants (`komo-infra`'s `persistence/`, `memory/memory_db.rs` —
the only places the ORM appears; one model struct per file, one `Db` holding
them all, each domain's repository impl in its own module):

- Backend is Turso in MVCC `concurrent_writes` mode; no `rusqlite`. DB URL is
  `turso:<path>` / `turso::memory:`.
- MVCC rejects `AUTOINCREMENT` → every key is a `String` UUIDv7, never `#[auto]`.
- Conflicting commits fail and must be retried: wrap single-write mutations in
  `with_write_retry`; multi-write sequences in a real transaction *inside*
  `with_write_retry` (rollback + clean re-run, never double-apply).

## Gateway is the process

Turso holds an exclusive cross-process lock per db file, so **the gateway is the
only process that opens komo's state — and therefore the only process that has
an agent.** "No gateway" is not a mode with its own behaviour; it is a process
that has to exist before the command can mean anything.
`GatewayClient::connect_or_start` (`infra/gateway_client.rs`) probes
`~/.komo/gateway.json` (rendezvous file) + `/health`, and finding nothing starts
one: on macOS it loads the launchd job and polls `/health` for up to 60s,
elsewhere it says who is supposed to (`komo gateway`, or the container's own
main process). `komo chat`, the TUI and every state-touching CLI command are
clients of the loopback api channel (`infra/messaging/api.rs`); there is no
direct-db fallback.

- **The whole operator surface is one endpoint.** `POST /api/operator` carries a
  serialized `OperatorQuery` / `OperatorCommand` (`OperatorRequest` /
  `OperatorReply` on the wire) and the gateway matches it against the same
  definitions the CLI names a variant from, so a shape cannot drift between the
  two sides. `OperatorControl` (`services/operator_control/`) is the CLI's whole
  view of it — resolve once per command, then issue queries and commands and
  never touch HTTP or a route path. The business result comes from
  `operator_control/actions.rs::OperatorActions`, which the gateway's own
  handlers call too, so logic can't fork — **add a new operator action there and
  a variant to the two enums**, never a route.
- The remaining `/api/*` routes exist for the clients that are not the CLI: the
  web/desktop apps (`/api/status`, `/api/models`, `/api/workspaces`,
  `/api/home-session`, `/api/sessions*`, `/api/memories*`, `/api/runs*`,
  `/api/dream*`, `/api/interactions/*`) and OpenAI-compatible callers
  (`/v1/models`, `/v1/chat/completions`). Writes sit behind a `require_loopback`
  *layer* (`operator_writes`), so a route added there is gated by construction.
- `komo chat` → `POST /v1/chat/completions` with `X-Komo-Trusted` (loopback
  only): side-effecting tools auto-approve for the host operator.
  `X-Komo-Session-Id` **must be a UUID** and is the session id verbatim — 400
  otherwise. It used to be wrapped in an `api:` namespace that every client then
  stripped back off, but that wrapper was doing one undocumented thing: keeping
  a caller from addressing another ingress's session and inheriting its
  permission and memory scope. The UUID requirement is what replaces it.
- **Every api turn takes the same per-session slot a chat turn takes**
  (`GatewayDispatcher::claim_session`), so "one turn per session" holds *across*
  ingresses. Two clients on one session — a second TUI resuming it, the desktop
  app beside the terminal — used to run concurrent turns, and the later one
  assembled its history before the earlier had written a word of its answer, so
  it started over from the original question and re-ran everything still in
  flight. A chat channel queues; an HTTP caller waits, because it is owed its
  own reply. The wait is unbounded on purpose: refusing the message at a
  deadline throws away what the user typed.
- Cancel: `POST /api/interactions/{session}/cancel` flips **every** signal
  registered for the session; `run_agent_loop` races every await against it. A
  session holds a *set* of registrations (`CancelState::register` hands back an
  RAII `CancelTicket` that retires only its own), because Stop is pressed on a
  conversation, not on one of the turns in it: an ingress parked in
  `claim_session` registers before it waits and races the wait against its own
  signal, so a stopped caller gives up instead of running, once the turn ahead
  of it finishes, the work the user just stopped. A running tool
  stops only if it claims `ToolContext::cancelled()` (shell kills its process
  group; web_fetch/web_search drop the request; fs tools deliberately run to
  completion so `apply_patch` never half-applies). Cancelled runs are Failed,
  **not** recoverable.
- **A turn can also stop to wait** (docs/bot-runtime.md §4.1): an approver that
  answers `Decision::Suspend` — not a denial, the absence of an answer — makes
  the gate record what the turn is waiting for, the executor leave that call
  **unsettled** (a call that stopped to wait did not happen), the loop end the
  turn as `RunStatus::Suspended`, and the runtime append `turn/suspended` plus a
  `wakeup_records` registration carrying the turn's job grants.
  `approval/requested` with no `resolved` beside it is already recovery's
  "asked for, never ran", so the re-dispatch after the answer arrives is the
  call's first and only run — and `rebuild_from_events` re-dispatches a gated
  call regardless of idempotency for exactly that reason. A suspended turn
  appends **no assistant message**: it has not answered, and the surface has to
  still end on the user message for the continuation to be one (retention's
  floor widens from `recoverable` to `!is_terminal()` for the same reason). The
  gate consults the log before asking, across the whole `attempt_chain` — the
  answer was recorded against the turn that *asked*, and the turn asking now is
  its continuation — so nobody approves the same action twice. `TurnWaker` is
  the other side: record `wakeup/fired`, retire every other wait that turn was
  holding, claim the session slot, continue (`interaction::record_wake` writes
  those events — one writer, so a second continuation path cannot forget the
  `wakeup/fired`). **A continuation runs on the runtime the turn ran on**: the
  dispatcher holds one handler per `SessionOrigin` (`with_runtime`, wired in
  `cli/gateway.rs`) and both `continue_turn_with` and `start_turn_with` pick by
  the session record's origin, so a routine comes back as a routine — on the
  conversation's runtime its next ungranted action would be *refused* by
  `ChatApprover` instead of stopping to ask, which is the opposite of §4.2, and
  it would be handed a wider tool set and the user's memory library besides. A
  `Delegate` session is never continued at all: the `delegate` call that would
  have read the answer is gone. A woken routine that stops *again* has no sweep
  behind it to deliver the new `wk-` id, so the dispatcher sends that prompt
  itself (`announce_new_wait`, `/approve <id>` only).
- **A tool can raise the same wait** (`ToolContext::wait_for`): `ask_user` fills
  in the *same* `PendingSuspension` the approval gate does, so the executor, the
  loop and the runtime need no second
  path. What differs is only the way back: `turn/suspended` carries the
  **`call_id`** that stopped, which puts it in `rebuild_from_events`' `gated`
  set (re-dispatched regardless of idempotency, for the same "it never ran"
  reason), and the runtime folds the chain's waits onto the `RunContext`
  (`fold_turn_waits`) as the continuation opens — so `ctx.resumed_wait()` hands
  the call its own wake, and `ctx.waits_taken()` is a per-turn budget counted
  from the log rather than from memory it would lose. A tool reading `Some`
  from `resumed_wait` must return it, never wait again.
- api channel is loopback/ephemeral by default; `[channels.api] enabled = true`
  + `API_SERVER_KEY` widens it. `web_dir` serves the built SPA same-origin;
  `remote_interactive = true` lets keyed remote callers run interactive turns
  (`X-Komo-Trusted` stays loopback-only regardless). CORS grants loopback
  origins + Electron's `null` origin; bearer key remains the gate.

## Config

`~/.komo/config.toml` = runtime settings (provider/model/`models`/aux_model,
`aux_effort` — the aux backend's reasoning effort; defaults to `none` on
DeepSeek (thinking off), the provider's own default elsewhere —
`schedule`, `dream_schedule`
(default nightly `0 3 * * *`, `"off"` disables) plus its kill switch
`dream_schedule_enabled` (default true; `false`
disables the sweep while leaving its cron in place, so
`KOMO_DREAM_SCHEDULE_ENABLED=false`
silences a deployment without rewriting config.toml), `pyhost_enabled`
(default true; `KOMO_PYHOST_ENABLED=false` keeps the python plugin host — and
with it `run_code` and every `py__` tool — out of the process),
`[channels.*]`, `[policy]`
— `default_normal`, the `[[policy.rule]]` list, and `mode` (`ask` default /
`auto`, which routes an escalation through the aux reviewer first; an
unparseable value warns and stays `ask`, since a typo must never widen the
gate) —
`[memory]` — `embedding_model`/`embedding_url` for the Ollama backend behind
cross-language recall; no model = lexical-only —
`[wiki]` — `vault` (the note directory; absent = no `wiki_search`/`wiki_read`/`wiki_index`)
and its own `embedding_model`/`embedding_url` (falling back to
`[memory]`'s when unset); the index itself has no configuration, because it is a
table in `komo.db` —
and `[mcp.servers.<name>]` — external MCP servers: `url`, `token_env` (names
the `.env` var, never the token), and a **required** `tools` allowlist
(or `all_tools = true`), closed by default because every mounted tool's schema
is re-sent every round).
`~/.komo/.env` = credentials only. Precedence: defaults < config.toml <
`KOMO_*` env. `KOMO_HOME` relocates the directory.

Resolution happens **once** in `crates/komo-config` into a `ConfigSnapshot`; problems
become `ConfigIssue`s (never abort resolution) checked by `validate_agent` /
`validate_gateway`. One deliberate warning, not a fatal: a missing model API key
(boots with `UnconfiguredLlm` that errors per call). **Never re-read config.toml
or call `std::env::var` in callers** — the only exception is `KOMO_HOME`.

Operator-authored prompt files (`komo-bot`'s `system_prompt`, main agent only):
persona `~/.komo/SOUL.md`, profile `~/.komo/USER.md`, L1 context
`~/.komo/MEMORY.md` (sole L1 source, main agent only, 8,000 characters with
visible truncation), and **one instruction file
per scope, first found wins** — machine-wide `~/.komo/AGENTS.md` else
`~/.agents/AGENTS.md` (the latter under the real home, not `KOMO_HOME`, since
other agents share it), plus project `AGENTS.md` else `CLAUDE.md` else
`.cursorrules` from the working directory. Taking only the first match per scope
is what keeps a `CLAUDE.md`→`AGENTS.md` symlink from being injected twice. All of
them are head-capped and re-read on mtime change (no restart needed).

Channels (`[channels.feishu|telegram|wechat]`): behavior keys in
the table, credentials in `.env`. `allow_from` pre-trusts senders; everyone
else must pair (`komo pair approve <code>`; codes stored salted-hashed,
rate-limited, expire in 1h). WeChat is QR-login (creds in
`~/.komo/wechat/credentials.json`), DM-only, and can't deliver proactive output
until the user messages the bot after process start. `home_chat` is the
fallback for proactive output; a `/sethome` chat command override (db) wins.

Model menu: `models = [...]` declares what a session may switch to; entries may
be provider-qualified (`deepseek:deepseek-v4-flash`) and `ModelConfig::menu()`
drops entries whose provider has no key (except the running `model`).
**A DeepSeek entry must name the model, not an alias**: `deepseek-chat` is a
deprecated alias for `deepseek-v4-flash` in **non-thinking** mode (and
`deepseek-reasoner` for its thinking mode), so name `deepseek-v4-flash` /
`deepseek-v4-pro` directly — thinking is on by default there, and
`X-Komo-Effort` for DeepSeek is `low` / `high` / `max` (no `medium`, which the
server would only alias onto `high`). Choice is
carried per turn in `X-Komo-Model`/`X-Komo-Effort`, validated against the menu,
stored on the session; `RoutingLlm` dispatches across providers. Effort levels
are per-provider (`Provider::efforts` ↔ `reasoning_params` must agree — there
is a test). **Invariant: every aux path (reviewer, delegate, recall, sweeps)
builds a synthetic `Session` with empty overrides** — that's what keeps a
conversation's model from leaking onto the aux model; preserve it when adding
aux callers. Empty overrides also mean an aux turn's effort can only come from
the backend, so `ModelConfig::aux_variant()` carries one (`ModelConfig::effort`,
`aux_effort` else `Provider::aux_default_effort`) and a session's own choice
still wins over it. On DeepSeek that default is `"none"` — thinking off, a real
wire value deliberately kept off `Provider::efforts` because it is not a level
anyone picks per turn; without it every short aux call (the `mode = "auto"`
reviewer has 20s) would run the server's default full thinking.

The `codex` provider authenticates from the Codex CLI's OAuth file
(`~/.codex/auth.json`, auto-refreshed) instead of an env key, and requires
streaming — see `komo-infra/src/codex.rs`. `$KOMO_HOME/.codex/auth.json` and
`$KOMO_HOME/codex/auth.json` are accepted as fallbacks, in that order
(`$CODEX_HOME` overrides them all): a container has no CLI
and no browser to log in with, so the login is copied into the volume that
already holds `.env`. A real `~/.codex/auth.json` still wins, since that is the
one the CLI itself rotates.

## Architecture

```
CLI/channel → AgentRuntime ─ run_agent_loop ─┬→ LlmClient::begin_turn → TurnDriver (ONE provider completion / round)
                                             └→ ToolExecutor::execute_round → tools   (loop until Step::Final)
                          ↘ MessageRepository · RunRepository (ledger) → Response
```

komo owns the tool loop **and its provider layer** (`crates/komo-provider`, no
LLM crate): one completion per round, `run_agent_loop` (`komo-bot`'s `runtime`) is where
round-level control lives (`max_turns` budget, cancellation, suspension). Tool
errors return as outcome content the model can recover from; only a driver/LLM
error aborts the turn.

**Crate layout.** The lower half of the tree is split out of the binary so it
compiles in parallel and so an edit there does not rebuild everything (`src/` was
one 50k-line crate). Depend downward only:

```
komo-core      the domain layer only: value types plus repository and port trait
               signatures. No I/O, no runtime, no `dirs`; its lib is one line,
               `pub mod domain;` — anything that reads a path, a file or a clock
               belongs a layer up (that is where `paths`, `rendezvous`,
               `operator_view` and `Notifier` went)
komo-config    config.toml + .env + KOMO_* → one ConfigSnapshot; owns `komo_home`
               and is the only `KOMO_HOME` reader                     (→ core)
komo-provider  wire formats + HTTP/SSE; references nothing else in komo
komo-mcp       MCP client over rmcp (Streamable HTTP); ditto — nothing komo
komo-pyhost    the python plugin host's protocol and child process; nothing komo
komo-infra     persistence · chunk_index · memory · skills · logs ·
               permissions_store · codex · embedding         (→ core, config, provider)
komo-services  tool_execution · tool_output_store · memory_query ·
               memory_consolidation · memory_enrichment ·
               skill_registry · cron_actions · wiki_indexing ·
               session_indexing · episode · artifact_store ·
               diff/patch/search/file_mutation                (→ core, config)
komo-tools     every tool                      (→ core, infra, mcp, pyhost, services)
komo-bot     runtime · gateway · daemon · interaction · system_prompt ·
               policy_approver · reviewer · llm · delegate · notify
                                            (→ core, config, provider, infra, services)
komo (bin)     cli · tui · pyhost (lifecycle) · `infra/messaging` (channels) ·
               `infra/gateway_client` · `infra/rendezvous` ·
               `services/operator_control` — the wiring layer, plus what needs
               the agent above it; each `mod.rs` says why it stayed
```

Test-only constructors a dependent crate's tests need — `persistence::reset_test_db`,
`SkillRegistry::new`, `komo-tools`' fixtures — are behind each crate's
`test-support` feature, enabled only as a dev-dependency so they never ship.

Cron scheduling math (`next_occurrence_local`) lives in `komo-core`'s
`domain::cron`, and every job mutation goes through `komo-services`'
`cron_actions` — the `cron` tool, the gateway handlers and the CLI adapter all
call the same functions, which is what keeps validation from forking.

**Module map** (one line each; read the module for details):

- `domain/` — pure traits + value types, no I/O, no external crates
  (`Tool`, `LlmClient`/`TurnDriver`, repositories, policy engine, pairing).
- `komo-bot`'s `runtime` — session lifecycle + the tool loop; loads only a recent
  transcript window per turn (`find_windowed`); wraps each turn in a ledger
  `Run` (all ledger writes best-effort, never fail the turn).
- `crates/komo-provider` — komo's own provider layer, its own crate because it
  references nothing else in komo (so it compiles in parallel with the rest).
  One module per **wire format**
  (`Wire`), not per provider: `responses` (OpenAI / Codex / DeepSeek /
  OpenRouter) and `messages` (Anthropic, which serves no Responses endpoint).
  `transport` is the HTTP+SSE boundary where `error::LlmError` is built while the
  status, headers and provider error `code` are all still intact — retryability
  is `LlmError::is_retryable()` (exhaustive match) and the server's own
  `Retry-After` beats any local backoff. Every request streams; a stream that
  ends without its terminal frame is a retryable failure, never a short answer.
  A new provider is a base URL + auth mode, not new code.
- `komo-bot`'s `llm` — `ProviderLlm` over that layer; `assemble` builds the tiered
  system prompt once per turn (stable tier incl. `~/.komo/USER.md` and the
  machine-wide instruction file, then memory
  prefix from `MemoryEnricher` — main agent only). `RoutingLlm` = cross-provider
  dispatch. Reasoning blocks are echoed back verbatim each round, which is what
  carries a reasoning model's chain of thought across a tool loop.
- `services/tool_execution/` — `ToolExecutor::execute_round`: per call, claim
  ledger seq → redact args → run with panic catch + `tool` span →
  transient-retry (connection errors retry anything; ambiguous only
  `Tool::idempotent()`) → **settle**: an ambiguous failure the classifier
  declined to retry becomes `ToolError::Uncertain`, not `Failed` — "we don't
  know whether it landed" has to reach the *model*, or it re-issues the call
  itself and applies the effect twice. A wall-clock abort on a non-idempotent
  tool is the same case. `Uncertain` is never retried structurally (the retry
  arm matches `Failed` alone), and rides to the ledger as `RunStep.uncertain`
  via an `UncertainOutcome` marker in the error chain (the variant is gone by
  then) — `komo run inspect` prints `??` for it, because "did that go
  through?" has three answers, not two → bound the LLM-facing result via
  `services/tool_output_store.rs` (full text on disk, head+tail preview) →
  record `RunStep`. Policy is instance-owned `ToolExecutionConfig`;
  `Tool::max_duration()` overrides the per-call timeout (approval-gated tools
  must outlast the 5-min approval prompt, `APPROVAL_BOUND`).
  `Tool::call(Value, &ToolContext)` is the **only** tool entry point; the
  `SESSION` task-local serves the approvers only — tools take `ctx.session`.
- `komo-tools` — `time`, `shell` (own process group, hardline floor no approval
  unlocks, nested timeouts), `grep`/`glob` (ripgrep libraries in-process;
  policy runs over paths **before** content is read), `read`/`write` +
  `fs_common` (confined to the workspace's roots **plus `~/.komo/artifacts`**
  and **`~/.komo/plugins`** — komo's own two writable roots: artifacts is where a
  turn puts what it *made* rather than what it changed, one directory per
  session, named to the model at the tail of each turn's user message; plugins is
  where it authors a tool, and a write there is `Risk::Dangerous` —
  `write_if_unchanged` guards the approval
  window), `edit` (exact match only, no fuzzy) / `apply_patch` (v2 envelope,
  one approval per batch, no rollback — reports exactly what landed),
  `web_fetch` (content-type gated, 256 KB download cap, deny-only network
  policy) / `web_search`, `homeassistant` (`call_service` approval-gated;
  `BLOCKED_DOMAINS` hardline), `todo` (session-scoped, dies at a `/new`
  boundary — the only thing that does), `memory`,
  `skill` (`list` / `view` / `install`), `cron`, `ask_user` (the one sentinel
  tool: it stops the
  turn through `ToolContext::wait_for` and comes back with the wake as its
  result — no process waits, and a restart loses nothing), `logs` (tail of komo's own
  tracing log — file lookup shared with `komo logs` via `komo-infra`'s `logs`, same
  deny-only file-read gate as `read`), `wiki_read` (vault-confined by
  canonicalized prefix, `Risk::Safe` deny-only; reads the markdown, not the
  index, so a note edited since the last index run is served current),
  `run_code` and `plugin`'s `PyTool` (see `src/pyhost.rs` below).
- `session` + `komo-services`' `session_indexing` — **episodic memory**:
  hybrid search over komo's own transcripts, the third memory beside `memory_records`
  (semantic) and skills (procedural). `search` spans **every** stored
  conversation by default, because "why did we decide against rig?" is a
  question about *some* past session and requiring its id up front is requiring
  the answer as the input. It matches meaning as well as wording, over the same
  `ChunkIndex` the vault uses but its own collection (`komo_sessions`) —
  transcripts are komo's own corpus and must not depend on `[wiki]` being
  configured. **A chunk is a turn, not a message**: "那就不用 rig 了" embeds into
  nothing alone, and its `ordinal` is its opening user message's `show` offset,
  so a hit is readable in full without translating coordinates. Indexing is
  incremental (append past the indexed chunk count) and happens **on the search
  path**, newest session first and budget-capped — a turn that never searches
  pays nothing, and a first search after a long gap does useful work instead of
  hanging. **Every failure degrades to the substring scan, never to "no
  matches"** — an empty answer reads as *the conversation never happened*, which
  is the one wrong thing this can say. Without `[memory] embedding_model` there
  is no index and `search` is the single-session scan it always was.
- `komo-bot`'s `reviewer` + `learning_coordinator`, `komo-core`'s
  `domain::episode`, `komo-services`' `episode` — the post-run extraction pass
  (docs/episode-learning-framework.md). Its unit is an **episode**: one finished
  `Run` plus its `RunStep`s, assembled on demand (`episode::assemble`) and never
  stored — the ledger is already the authority on both. A transcript alone could
  not say whether a command ran or what it returned (tool results are never
  persisted as messages), so an extractor reading one learns from the agent's
  own account of itself.
  **`Done` is not `Success`.** Execution status and goal outcome are separate
  axes (`OutcomeVerdict`); the deterministic assessment never reports success,
  because nothing observable at the end of a turn distinguishes "the goal was
  met" from "the agent stopped talking". Evidence carries its own strength and
  only the strongest kind present decides — a disagreement among peers resolves
  to `Unknown`, never a majority.
  Memory extractions leave here as `Observation`s and are applied by
  `MemoryConsolidator`, never written directly — the reviewer holds no memory
  store. **Memory is the only thing the pass extracts**: skills are written and
  installed by a human, so nothing here proposes one.
  **The watermark is per run, not a per-session turn count**: a count says how
  many turns there were, never which ones were new. It lives in the log as one
  `learning/completed` / `learning/skipped(reason)` per turn, durably flushed
  *before* `Run.learned` moves — the row is the index over those events, so a
  row that read learned over a log that never said so would come back unlearned
  the moment the ledger is rebuilt (`run_projection` folds it). A run the pass
  deliberately skips (cancelled turns, sweep sessions) is marked too, since
  "considered and declined" and "not yet considered" have to be different states
  or every sweep re-examines it forever. A *failed* pass — including one whose
  watermark event could not be written — marks nothing, so the next sweep
  retries it.
  **Learning is dispatched after `runs.finish`, never from inside the turn** —
  an episode assembled while its run is still open has no decided status, and
  `unlearned` would not offer it at all, so the turn would silently never be
  learned from. There is a regression test for exactly that.
  **Cancelled turns are audit, not lessons**: the work stopped part-way by the
  user's choice, so its silence is not evidence and its half-done steps are not
  a procedure worth keeping.
  **Sweep and delegate sessions are exempt** (`SessionOrigin::is_learnable`,
  matched exhaustively so a new origin must decide) — a sweep restates facts the
  agent already knows, and each run's session counts as a fresh "independent
  occasion" to the consolidator, so extracting there would let the memory
  library corroborate itself on a timer; a delegation is the *parent* turn's own
  work, so learning from both counts one occasion twice. The guard lives in
  `LearningCoordinator` (`learning_exemption`, which also names the reason the
  watermark event records), covering both triggers.
- `komo-bot`'s `compaction` — the oldest messages of a long conversation,
  replaced by a summary of them. The trigger *is* the history window: a surface
  longer than `max_history_messages` is one whose oldest nodes the model has
  already stopped seeing, and compaction turns that silent loss into a note to
  its future self. The summary is one `user/message` carrying a
  `surfaceOp: replace` over the range it shadows — an append like any other, so
  **nothing is rewritten** and a human transcript still shows what it covered.
  Three rules keep it safe: the cut lands where the surface still **alternates**
  (a summary is a user message, so an assistant message has to follow it); the
  summary plus what stays verbatim has to **fit the window**, or the same window
  would drop the summary too; and the replacement is **validated against the
  surface it will land on**, immediately before the append — the fold fails
  closed, so a replacement citing a node that has left the surface would not
  lose a summary, it would make every later read of that session an error. It
  runs at turn settle, inside that turn's session slot (two compactions planning
  against one surface is the race that check exists for), and every failure
  means no compaction: the window keeps trimming, which is the behaviour this
  improves on rather than depends on. Wired per `CapabilityProfile`
  (`compacts`) — conversations only; a sweep or a delegation never outlives its
  window.
- `komo-bot`'s `delegate` — sub-agent as a real agent turn on its own session
  (`Session.origin = delegate`, which is what keeps it out of the session list); inherits the parent's ambient session context (approvals prompt the
  real conversation, cancel propagates); recursion blocked structurally
  (sub-agent tool set has `delegate: None`); each delegation is its own ledger
  run. The unattended cron runtime gets no `delegate`.
- `src/pyhost.rs` + `komo-pyhost` + `komo-tools`' `run_code` / `plugin` — the
  **dynamic-tool loop**, and the one thing in komo that mounts tools into a
  *running* process rather than at wiring. `run_code` runs a one-off Python
  program in an out-of-process host; a `@tool`-annotated function in a
  `$KOMO_HOME/plugins/*.py` file becomes a `py__<name>` tool within seconds of
  the file being written, and goes away when it is deleted. `PLUGIN_GUIDANCE`
  (gated on `run_code` plus a known plugins directory) is what tells the model
  that loop: prototype with `run_code`, persist as a `@tool`. Both directions go
  through the same broker — a plugin's `tools.<name>(...)` call is dispatched by
  the runtime's own `WeakToolExecutor` (`PyTool`), so it pays the same approval
  gate, retry classification and ledger step an ordinary call does. Two
  structural rules: the plugins directory is a **writable workspace root whose
  writes are `Risk::Dangerous` with no scope key**, so a human approves each
  file and no grant ever widens it (a `.py` there runs unsandboxed on every
  later turn, unattended routines included); and `available()` decides *before*
  the executors are built whether a host can run at all (`pyhost_enabled`, and a
  `[policy]` that wholly denies `Category::Plugin`), because that answer decides
  whether `run_code` is registered — `start()` comes after, once there is
  something for the host to mount into.
- `domain/policy.rs` + `komo-bot`'s `policy_approver` — permission policy. Ladder,
  strongest first: **tool hardline floor > config deny > saved grant > config
  allow / `default_normal` > ask**. Saved grants (`permissions.json`, written
  only by `PolicyApprover`) are never read unattended. **A `Risk::Dangerous`
  action is approved for the one call it was asked about and no further** —
  `/approve session` and `/approve always` narrow to `Once`, in
  `ApprovalState::resolve_scoped`, and the user is told. Widening an
  irreversible action pre-approves a
  *later* deletion nobody has seen. Unattended contexts (cron/sweeps) grant only through
  `unattended = true` allow rules **or the running job's own `grants`**
  (`CronJob.grants`, approved in the same prompt that created the job; carried
  into the turn by `with_job_grants`, scoped to that turn, revoked with the job)
  — everything else escalates to the runtime's own inner approver
  (`komo-bot`'s `unattended`), which **stops** rather than refuses
  (`UnattendedSuspend` → `Decision::Suspend`) so the sweep can tell the operator
  which wait to answer in the home chat. It never lets a `Risk::Dangerous`
  action through, however long the operator takes.
  Full ladder: **tool hardline floor > config deny > job grant > saved grant >
  config allow / `default_normal` > ask**. **What marks a turn unattended is
  `SessionContext::origin`** (`SessionOrigin::Cron`, set by the
  sweep that starts the turn), *not* the absence of an ambient session — those
  turns have a real session id, and reading a channel off it is what used to
  make the engine's unattended branch unreachable. Read-only actions (`read`, `web_fetch`) are
  deny-only — never prompted. Wholly-denied tools are dropped from the catalog
  at wiring (`drop_policy_denied`). Policy only tightens; hardline floors
  short-circuit inside the tool.
  **Every rung names itself**: `Approver::decide_reported` answers
  `(Decision, rung)` and the gate writes it to `approval/resolved`, which the
  ledger folds onto the call as `RunStep::approved_by` / `approval_waited_ms`
  (`komo run inspect` prints `allowed by human after 4.2s`) — "why did this go
  through?" is asked long after the fact, and every rung produces the same
  `true`. The trait's default answers `approver`, so an implementation that does
  not know its rung never claims one.
- `komo-bot`'s `auto_reviewer` — the `[policy] mode = "auto"` rung, sitting
  between the engine's `Ask` and the human (attended runtimes only; `mode =
  "ask"` is the default and omits the decorator entirely). An aux-model reviewer
  judges whether the action is plainly authorized by the operator's own latest
  message, and **may only allow or hand over — never deny**; refusal stays the
  operator's. Four structural properties, each a test: no deny;
  `Risk::Dangerous` never reviewed; unattended turns never reviewed (the cron
  runtime keeps the "shrink the action set in advance" contract and doesn't wire
  it at all); fail-closed — model error, 20s timeout, unparseable verdict, or no
  operator message to judge against all mean "ask". Verdict parsing is
  deliberately strict: the word must lead the first line **and** be the only
  verdict named on it, because a line saying "ALLOW would be wrong; ASK" has
  not decided. The reviewer's trust boundary (only the operator's message
  authorizes; tool output and agent text never do) is the same rule the main
  prompt states in `system_prompt::TRUST_BOUNDARY_GUIDANCE` — one rule, so the
  agent and its reviewer cannot disagree on what authorization is. This reopens
  ADR 0002's "no LLM approver" half under that ADR's own stated trigger (MCP
  landed); the sandbox and credential-broker halves stand. See
  `docs/adr/0003-auto-policy-llm-reviewer.md`.
- `komo-mcp` + `komo-tools`' `mcp` — external MCP servers over Streamable HTTP
  (rmcp, client features only). `[mcp.servers.*]` is connected **once at
  wiring**: the catalog is immutable after that (`register` takes
  `Arc::get_mut`, and its byte-stable order is what keeps the provider prompt
  cache valid), so a server that is down at boot has no tools for the process's
  lifetime — and an unreachable one is a warning, never a fatal. Each mounted
  tool becomes `mcp__<server>__<tool>` (leaked to satisfy `Tool::name`'s
  `&'static str`; built once and `Arc`-shared across every executor). **Every
  MCP call is approval-gated** — `annotations.readOnlyHint` is server-authored,
  and the server is the party being gated; grant specific tools with
  `category = "mcp"`, `value = "<server>.<tool>"` rules. A `tools/call` that
  comes back with `isError` is returned as *content*, not a `ToolError`: the
  message is remote-controlled and the retry classifier falls back to substring
  matching, so an echoed "connection refused" must not re-fire a mutation.
- `domain/memory.rs` + `services/memory_query.rs` + `services/memory_consolidation.rs`
  + `services/memory_enrichment.rs` — three surfaces:
  L1 operator-edited `MEMORY.md` via `SystemPromptBuilder`, L2 `memory` tool + operator CLI,
  L3 recall (fetch 15, inject ≤5, aux-screened above 5).
  **Truth and utility are different axes, on purpose.** `support_count` /
  `contradiction_count` / `last_confirmed_at` / `evidence` say whether a memory is
  *true*; `recall_count` / `last_used_at` say whether it keeps being *useful*.
  Promotion reads only the first set (`dream_verdict`: an explicit confirmation, or
  `DREAM_MIN_SUPPORT` independent occasions, and no unresolved conflict) and
  retention only the second (30-day-cold candidates archive). Deciding promotion on
  recall — as it once did — lets a wrong memory confirm itself by being retrieved:
  the thing retrieved is not the thing tested.
  **Refutation is not symmetric with support.** Support has to accumulate across
  independent occasions to promote; a candidate carrying an unresolved
  contradiction (`unresolved_refutation_at` — a conflict with no confirmation
  after it) is archived once nobody has ruled on it for
  `DREAM_REFUTED_FORGET_AGE_DAYS`, *regardless* of how warm retrieval keeps it.
  It can never promote anyway, so warmth would only keep a claim the user spoke
  against occupying a recall slot in every search about it.
  **`BeliefState` is a separate column from `status`**, not a new status value.
  Status is the triage pipeline every operator surface is built on; belief is
  `current` / `contested` / `superseded`, and only `current` may be injected
  (`is_injectable`, checked by `enrich`). This governs database L3, not the
  operator-edited L1 file. Retrieval stays
  belief-agnostic — an explicit `memory search` must surface a contested memory or
  the model cannot help settle it.
  **Every extracted observation goes through one seam** (`MemoryConsolidator`):
  find related claims, classify via aux as same/supports/contradicts/supersedes/
  unrelated, then record evidence, contest, supersede, or write a candidate. Every
  failure path lands a plain candidate — the pre-seam behavior. Evidence
  independence is **per learning occasion** — one `LearningCoordinator` pass,
  which `record_evidence` drops if it already counted it. That is what stops one
  talkative pass from corroborating itself, and what a permanent **home
  session** made session-keying unable to do: every private conversation is one
  session, so support never reached `DREAM_MIN_SUPPORT` there and nothing
  extracted on the main ingress could promote.
  An occasion is the **whole batch** of runs that pass read (`Occasion`), not
  one run of it: new evidence is stamped with the batch's oldest run id as its
  canonical name, and `Memory::witnessed_on` asks whether *any* run in the batch
  already appears. That second half is why it is a set — a memory the model saved
  mid-turn through the `memory` tool is founded on that turn's own run, which
  sits somewhere in the middle of the batch whose review reads it later, and
  comparing canonical names alone would let that review "support" what the turn
  had already recorded. Legacy evidence carries no
  occasion and falls back to its session (`Evidence::occasion_key`); the list is
  capped at `EVIDENCE_CAP` while the counts keep rising.
  Its related-claim lookup uses `select_related` — recall's set **plus rejected
  claims** — because re-observing something the user rejected is that "no"
  coming round again, and filing it as a fresh candidate is how a rejection is
  forgotten. Injection still reads `select_recall`, and `dream_verdict` promotes
  only candidates, so a rejected claim can accumulate evidence and still never
  reach a prompt.
  Reviewer extractions are always `candidate`, never active; they never edit L1.
  **Provenance is a separate axis again** (`MemoryProvenance`: `user` / `tool`,
  additive column, default `user`). A turn reads pages, files and MCP replies,
  and a page that says "the user prefers X" is a page saying so — indistinguishable
  from the user saying it once it is a claim. The extractor labels each one
  (`said_by`) and anything that is not plainly `user` reads as `tool`; a
  tool-derived observation may only land as its own candidate (never support,
  contest or supersede what the user said), never promotes on accumulated
  support (`dream_verdict` / `is_supported` want an explicit confirmation), and
  carries `/from-tool` on its injected line.
  **Both read paths share `MemoryQueryService`** — automatic recall and the
  model's own `memory search` build the same hybrid query, so a memory the model
  was handed is a memory it can find again (candidates included). Matching is
  **lexical ∪ semantic** (`RecallQuery`): shared terms, or
  cosine ≥ `RECALL_SEMANTIC_FLOOR` against the memory's embedding. The semantic
  arm is not optional polish — CJK bigrams and ASCII words can never be equal,
  so lexical-only recall structurally cannot match a Chinese question to an
  English memory. Embeddings come from `[memory] embedding_model` via
  `komo-infra`'s `embedding` (Ollama; a *multilingual* model, or the gap
  returns), are stored per memory with the model that produced them
  (`embedding_for` rejects a foreign vector), and are backfilled in the
  background from the read path — so every write path is covered by one
  implementation. Every embedding failure degrades to lexical, never to worse.
  Injected lines carry `/supported` and `/stale:Nd` markers off
  `vouched_at()` (last confirmation, else newest evidence, else creation — *not*
  `updated_at`, which is an edit clock), and the block header tells the model to
  confirm a stale memory before letting it drive an action.
  **Scope**: `write_scope()` channel-scopes a turn that has a correspondent
  (`SessionContext::channel`, filled from the session record in
  `run_agent_loop`), else writes `Global`. A local surface (TUI/desktop/web) has
  no correspondent, so it writes `Global` — it used to be modelled as a chat on
  an `api` platform whose chat id was a fresh uuid per conversation, which made
  every automated write unrecallable from the next turn and needed an
  `is_durable_channel` exception to undo.
- `domain/chunk_index.rs` + `komo-infra`'s `chunk_index` + `komo-services`'
  `wiki_indexing` +
  `komo-tools`' `wiki_search` / `wiki_read` / `wiki_index` — semantic search over the note vault
  (`[wiki] vault`), **pulled on demand, never auto-injected** like memory recall:
  a vault dwarfs the memory store, so a turn that does not search pays nothing.
  `ChunkIndex` is the corpus-neutral index trait (shared with session search) and
  `TursoChunkIndex` is the only implementation: **one `chunk_records` table in
  `komo.db`**, split by a `collection` column (`komo_wiki`, `komo_sessions`), so
  a corpus is disposable by collection and there is no second store to configure,
  lock or keep in sync. Both arms are SQL. The **dense** arm is
  `vector_distance_cos` over a raw little-endian `f32` BLOB — brute force, no
  ANN index, because Turso has none and a few thousand chunks scan in tens of
  milliseconds (the same trade memory recall already makes in Rust). The
  **lexical** arm is a `terms` column written at index time and matched with
  `instr`, IDF-weighted: Turso *has* an FTS index method, but it refuses to be
  created under MVCC (`Custom index modules are not supported in MVCC mode`) and
  MVCC is what lets the gateway's sessions write concurrently — so the
  tokenizing moves to index time, paid once per chunk instead of once per query.
  Tokens come from the splitter memory recall uses
  (`chunk_index::lexical_terms`, CJK bigrams plus ASCII words): a Chinese query
  against an English note is the same problem in both places, and two answers to
  it would be a bug in one of them. The two arms fuse through the existing RRF
  and `diversify`, capped per note so one long file cannot crowd out a result
  set. `Db` holds a second raw `turso::Database` handle for this, since it is the
  one table komo talks to in SQL rather than through the ORM.
  **`wiki_search` finds, `wiki_read` widens**: a
  search hit is an isolated chunk, and a turn that needs the whole section asks
  for it by `path` + `heading` rather than making every query pay the context
  cost of the few that do. `wiki_read` shares the chunker's heading parser
  (`is_fence` / `parse_heading`), so it can never miss a heading search reported,
  and needs no index handle at all.
  The gateway holds the only handle, so
  `komo wiki` borrows it through `operator_control` rather than opening its own.
  Indexing is **incremental by mtime** (embedding is the whole cost of a run, so
  an unchanged file costs nothing) and `--rebuild` is the opt-out. **Nothing
  reindexes on a schedule** — there is no wiki sweep; a cron job with a
  `wiki:exact:refresh` grant is how you get one. Every indexing caller goes
  through one `WikiIndexRunner`: `wiki_index`, `komo wiki index`, and any job.
  Its `claim` is an RAII guard, so an abandoned run frees the slot instead of
  locking indexing out for the process's life. `wiki_index`'s three actions are
  three risk levels — `status` `Safe` (the diagnosis surface: an `indexed_by`
  that differs from the configured model is *the* index anomaly), `refresh`
  `Normal` and synchronous, `rebuild` `Dangerous` and **detached**: a rebuild
  `reset()`s the store before refilling it and outlives any `max_duration`, so
  running it inside the call would let a timeout abort it with the store already
  emptied. Its outcome is read back with `status`.
- `domain/run.rs` + `domain/run_projection.rs` — run ledger: one `Run` per
  turn, one `RunStep` per call, and **all of it a projection of the session
  event log**. Nothing writes a run or a step directly: `project_runs` folds the
  log and `RunProjectionStore::commit` upserts the rows — once when the turn
  opens (from the opening events alone, so a crash leaves a `running` row for
  `run list` to find) and once when it closes, from the same
  read of the log that computes retention's floor. Two authoritative records of
  one turn disagreed after exactly the crash they were meant to survive; now the
  fold-vs-row cross-check (`assert_ledger_matches_log`) holds to the second.
  Three things stay row-held because the log cannot state them: `outcome` (a
  verdict a *later* turn gave), `learned` (merged, only ever advancing), and the
  startup reconciler's ruling on whether an open turn is running or dead —
  which the fold's silence must never overturn. `run prune` writes a
  `projection:runs:pruned_before` fence in the same transaction as its deletes,
  bounded by the newest run actually deleted, so a rebuild
  (`Db::rebuild_run_projection`) cannot resurrect what an operator removed.
  A turn's steps reach its closing tool note from `RunContext`, not from the
  rows: mid-turn there are no step rows to read.
  `Run.memories` records **which stored memories reached that turn's prompt**
  (L3 recall; the retired pinned list remains readable for historical runs),
  carried out of prompt assembly on
  `TurnDriver::memories()` the same way `usage()` carries tokens. It answers
  the question `recall_count` cannot: not "is this memory useful" but "which
  memory produced *this* answer" — and, read the other way, which turns a
  memory you just corrected had already shaped. Ids only; the store stays the
  authority on content.
  `elapsed_ms` is the duration field (`started_at`/`ended_at` are whole
  seconds); 0 / empty `structured` read as *unknown/absent*, never
  instant/empty-object. Args redacted per-tool (`Tool::redact_args`); results
  truncated not scrubbed. `recoverable` folds as *no terminal event and
  unclaimed* and prints as `⟲` in `run list` — an audit marker, not a resume
  button: nothing re-dispatches an interrupted turn, because the ledger is a
  record of what happened and not a checkpoint to replay from.
- `domain/skill.rs` + `komo-infra`'s `skills` + `services/skill_registry.rs` —
  skills are `SKILL.md` files under `~/.komo/skills/<name>/`, and **skills are
  written and installed by a human**: nothing here writes one on the agent's
  behalf, so there is no candidate pool and no governance pipeline. The registry
  scans an ordered search path (`runtime_skill_dirs`: the configured dirs, the
  workspace's own `skills` / `.claude/skills`, `~/.komo/skills`, then the shared
  read-only `~/.agents/skills` and `~/.claude/skills` that Codex and other local
  agents use — earlier wins on a name clash), and re-scans on every query, so a
  file edited or added needs no restart. Only the capped prompt catalog is a
  startup snapshot (cache stability). That catalog — and **only** that catalog —
  is gated by
  `SkillOffer` (frontmatter `platforms:` / `requires_tools:`, evaluated per
  runtime at wiring against its own registered tool set): an always-on prompt
  line is the one place an irrelevant skill costs tokens every turn. It is never
  a load gate; the `skill` tool's `view`/`list` and every `komo skills` command
  ignore it. `komo skills disable` hides one without deleting it; an agent job
  names the skills it wants preloaded (`CronAction::Agent`'s `skills`).
- `komo-bot`'s `daemon` — `Maintenance` sweeps under `supervise` (circuit breaker
  after 5 failures). Sweep cron expressions are matched against **local time**
  via the same `next_occurrence_local` cron jobs use — never `Utc::now()`
  straight into croner, which silently shifts every schedule by the UTC offset.
  Three sweeps, no more: `ReviewSweep` (via the shared `LearningCoordinator`,
  which
  also serves the post-run trigger — the per-run watermark + in-flight guard
  prevent duplicate extraction), `CronJobSweep` (the **only**
  ingress for routines: it holds an `Arc<RoutineEventSource>` — everything a
  firing needs — and adds "which slot has come".
  Claim-before-run: a
  crash never re-fires a slot; a slot missed by more than the job's **own
  interval** is abandoned rather than fired at the wrong hour — `is_due` has no
  upper bound on lateness, and the host is a laptop. `--skip-missed` opts a job
  out of running late at all; the same tick also fires **standing wakeups** —
  `WakeupWiring`, docs/bot-runtime.md §3.3's one scheduler — claiming each
  registration before firing it (`take` answers `false` when it is already
  gone, so two sweeps or a sweep racing an arriving `/approve` wake it once)
  and checking the **log** before it does: a registration pointing at a turn
  that already resumed is stale and is dropped, because firing it would re-run
  the continuation's work. `reregister_suspended_turns` closes the loop the
  other way at startup — a turn the log says is waiting with nothing watching
  it gets its wait back, read out of its own `turn/suspended`), and `DreamSweep`
  (the memory governance cycle — candidates promote or archive by evidence,
  previewed by `komo dream`).
  `RoutineEventSource` holds everything a firing needs (the jobs, the notifier,
  the wakeup wiring, the cron runtime) and owns the running itself, so a firing
  lives in one place and the sweep only decides when.
- `komo-bot`'s `gateway` + `interaction` — gateway hosts channels +
  sweeps. `GatewayDispatcher` owns turns (spawned per turn so `/approve` can
  arrive mid-turn; one turn per session). **`handle` is the only entry a channel
  may use**: it claims the message in the durable inbox (`domain/inbox.rs`,
  keyed `<platform>:<message_id>`) and drops redeliveries before anything else
  runs — chat platforms deliver at-least-once, and the gate has to cover
  commands too, since a redelivered `/approve` would approve twice. `dispatch`
  is the un-gated inner routine and stays private. Channels that have no
  platform message id use `InboundOrigin::local()`, which is never a duplicate.
  **A row is `completed` when the work is, not when it was dispatched**: a chat
  command completes as soon as it has been answered, a plain message only when
  its turn settles (success, failure or suspension) — so a message queued behind
  a busy session stays `claimed` until its own turn has run, and the rows a
  running turn absorbs as interjections settle with that turn. A shutdown that
  discards a session's queue closes those rows too — the senders are told to
  resend, and a resend plus a startup re-delivery is two answers to one
  message. "Completed" used to mean "handed to `spawn_turn`", which left a
  crash window nothing closed: the claim was already enough to make the
  platform's redelivery a duplicate, so a process that died before the turn
  wrote anything dropped the message for good.
  `GatewayDispatcher::recover_inbox` (`INBOX_RECOVERY_LIMIT` rows, called
  from `cli/gateway.rs` after `reregister_suspended_turns`, before the channels
  serve) is the scan that closes it: a
  row whose text is already a user message in the session's transcript at or
  after `claimed_at` belongs to the ledger and is only completed, and everything
  else goes back through `dispatch` — the command-honouring path, so a lost
  `/approve` still approves. One thing never re-runs: a **command** on a row claimed
  before the peer columns existed, which names no sender — `/sethome` reads
  the peer, and an empty address is not a home chat. Plain text on such a row
  still does. Recovery re-claims nothing (the row is already claimed) and
  answers on **no sink**: nothing here addresses an arbitrary `ChannelPeer`
  (`HomeNotifier` writes to the *home* chat), so a recovered turn's reply lands
  in the transcript and the chat that wrote sees nothing back. A `local` origin
  is closed rather than re-run — nothing can redeliver it and its caller owns
  its own retry story.
  **Which conversation a message belongs to is resolved in two steps**
  (docs/bot-runtime.md §3.8, D6). Principal first, off the channel's own
  admission gate: `PairingGuard` already checks `allow_from` before pairing
  rows, and those two branches *are* "the operator" and "somebody they paired
  with", so `Gate::Allowed` carries a `Principal`. Then conversation: the
  operator writing **privately** — TUI, desktop, web, Telegram/Feishu DM,
  WeChat — is always the one **home session** (`HomeRepository::home_session()`,
  a `setting_records` row minted on first ask); anything with other people in it
  keys on the correspondent through `find_by_peer` as before. The home session
  has no `channel`, so its memory writes are `Global` — which is the right
  scope for it.
  Chat commands: `/new` (append `conversation/boundary`),
  `/approve [session|always]`,
  `/deny`, `/skip` (decline an `ask_user` question — the turn continues on its
  own assumptions instead of standing for a week), `/sethome`,
  `/wechat login`. A plain message answers a pending question
  (`answer_question`, which is also the GUI's inline reply and the api's
  cancel), and the answer rides back on `wakeup/fired{reply, payload}`. `ChatApprover` sends the prompt and
  answers `Decision::Suspend` — the turn gives up its slot and comes back when
  `/approve` (or the GUI modal, through the same
  `GatewayDispatcher::answer_approval`) writes the answer into the log, in this
  process or the next one. How long an unanswered approval may stand is the
  *wait's* own lifetime (a day), not a timeout somebody sits through. A plain
  message while an approval is parked **replaces** it: the message joins the
  suspended turn as an interjection, the approval resolves as refused citing it,
  and the turn continues — one turn, not two. No session in context ⇒ deny. `HomeNotifier`
  delivers all proactive output (sethome override > config `home_chat`, feishu
  first among the config candidates), routing by the target's
  `{platform}:{chat_id}` prefix. Nothing resolves ⇒ the caller is told, never
  silence.
- `infra/messaging/` — channels: feishu (ws long connection on a dedicated
  thread), telegram (long polling, Markdown with plain-text fallback), wechat
  (iLink, DM-only, shared `WeChatBot` instance, in-memory reply tokens).
  A channel hands `GatewayDispatcher::handle` an **`InboundPeer`** — a
  `ChannelPeer` (platform + that platform's chat id), plus whether the chat is
  private and whether the sender is the operator — never a session id: which
  conversation that is belongs to the dispatcher and the store (see the two-step
  resolution above). Session ids are UUIDs and carry nothing — they used to *be* the
  address (`feishu:{chat_id}`), which made every consumer re-derive it by
  splitting a string. Home Assistant is **not** a channel —
  it is reachable only through the `homeassistant` tool (agent pulls on
  demand); recurring device reactions belong in an HA automation written via
  the tool's `save_automation`, not in an event stream that costs an LLM turn
  per sensor tick.
- `cli/wiring.rs` — the **only** place an `AgentRuntime` is constructed, and the
  gateway is its only caller: the gateway is the only process that opens komo's
  state, so it is the only process that has an agent. Every tool komo mounts is
  built here, once, and registered into one executor per runtime — register a
  new tool here. Which runtime an executor is for is a 3-variant `Runtime` enum
  (`Main` / `Subagent` / `Cron`) that picks a `ToolCatalog` out of `Catalogs`;
  the catalogs are separate rather than shared because the runtimes deliberately
  differ in what they mount (`delegate` is the conversation's alone) and because
  a catalog is what the plugin host mounts into, so each runtime's view of the
  plugins dies with its own executor. Each runtime is a
  **`CapabilityProfile`** — llm, tools, `max_turns`, `learns`, `compacts` —
  built by `RuntimeParts`, which holds what all of them share (the stores, the
  compactor, the history window, the learning coordinator). Adding a runtime is
  a profile, not a struct literal whose three real differences hide among nine
  identical fields.
- `tui/` — ratatui chat front end, a pure gateway client (`GatewayClient`; there
  is no in-process backend); state +
  key handling terminal-free in `tui/app.rs`. `komo chat` opens the operator's
  **home conversation** — not a fresh id per launch — so closing the terminal
  and reopening it continues the same thread the morning's Telegram DM is in.
  `komo resume <id>` (or the compatible `komo session resume <id>`) is what
  opens some *other* session by its UUID: a correspondent's, or an old one being
  looked into. A turn's workspace is the **process's** startup directory, not
  the session's: one conversation is entered from wherever the operator is
  standing, so `Session.workspace` is descriptive only (the log manifest and the
  session list read it) and nothing rewrites a turn's tool root from it. Input:
  Enter sends, Shift/Alt-Enter (kitty protocol) or Ctrl-J newline, **Esc stops
  the turn in flight** (nothing when idle — a stop key that sometimes discards the
  draft is worse than one extra keystroke; under the approval modal Esc keeps
  meaning "deny"). An approval or an `ask_user` question reaches it by polling
  `GET /api/interactions/{session}` every `INTERACTION_POLL` (2s) and is answered
  by `POST`ing to `/approval` or `/answer`; the modal offers **once / session /
  deny** and no "always", since a saved grant is the CLI's business, not a
  keystroke's. Cancel is
  `POST /api/interactions/{session}/cancel`, which also denies a pending approval
  and answers a pending `ask_user` — a turn parked on either never reaches
  another await, so the signal alone would not reach it. `tui/paste.rs`
  holds both paste mechanisms — a chip folds a ≥4-line / >10 KB paste to a label
  (`input` still holds the full text; the chip's byte range is what keeps
  rendering off the folded content) and `coalesce_rapid_keys` rebuilds a paste
  that a terminal without bracketed paste delivered as keystrokes. Input events
  go through a channel so a batch can be collected before it is interpreted.
- `cron` (`cron_job_records`, `CronJobSweep`) — **routines**: a `Trigger`, an
  action, and a `runs` history. Three job modes: **command**
  (operator-authored, runs directly, no approver), **agent** (unattended
  turn on `cron_runtime`: a side effect needs an `unattended = true` policy
  rule or one of the job's own grants, else the turn **suspends** and the
  operator answers `/approve wk-<id>` in the home chat — the run's status is
  then `waiting`, which is neither ran nor failed), and
  **message** (`CronAction::Message { text }` — deliver a fixed text, no process
  and no LLM: this is what a reminder is, one-shot via `@at` or recurring on a
  cron expression like any other job).
  **`Trigger` is what makes it fire**, replacing the
  schedule string, and it names a moment or nothing: `Cron` (a 5-field
  expression in local time) and `At` (the `@at` one-shot, resolved to its
  instant at creation) are the only two, so `next_run_at` always holds a slot the
  sweep can find due. `cron_actions::parse_schedule` is the one place a trigger
  *string* becomes a `Trigger` — the CLI and the `cron` tool both call it, so
  both accept exactly a cron expression or `@at YYYY-MM-DD HH:MM`, and a past
  `@at` is refused while the person who typed it is still there. A **stored** job
  whose trigger no longer parses is skipped with a `warn!`, never fatal: a row
  written by an older or newer komo must not take the sweep down.
  The `cron` tool's `after` (a relative `30m` / `2h`, through `parse_after`) is
  the same `@at` moment computed from now.
  **One firing is one `RoutineRun`**, claimed `running` in the same write as the
  slot (a crash mid-run leaves the record of what was in flight) and settled
  `ok`/`error`/`waiting` after; `runs` keeps the newest 20 and `runs.last()` is
  what every "how did that job go?" surface reads. `last_error` stays reserved
  for trigger/config problems.
  **`notify`** (`always` default / `on_error` / `never`) filters *delivery*, never
  the record — and never a `waiting` run, which is the routine asking for
  something rather than reporting.
  Chat-created jobs (`komo-tools`' `cron`) are approval-gated at creation; a
  command job from chat is `Risk::Dangerous`. An agent job declares the actions
  it needs as `grants`, approved in that **same** prompt (which is why a
  grant-carrying `add` drops the `cron:add` scope key) — narrower than a global
  `unattended` rule and revoked when the job is removed. A job's lifecycle is a
  **stored status** (`active`/`paused`/`done` — the sole authority, no enabled
  flag); a `@at YYYY-MM-DD HH:MM` schedule is a one-shot that completes (`done`)
  at claim time and keeps its row as the queryable record — each run holds its
  delivered body and, for an agent run, the session linking it to its ledger
  transcript, so "what did that job do" outlives the notification.
  `enable`/`run` refuse a `done` job. An agent job may also name a
  **`workspace`** — the directory its file and shell tools are confined to,
  installed on the turn's `SessionContext::workspace_root` by the sweep. It is
  canonicalized and proven to exist **when the job is created**, while the
  person who typed it is still there: resolved late it would fail at 03:00 as a
  permission refusal on every file the turn touches, which reads like a policy
  problem rather than a typo. It shares the `workdir` column with a command
  job's cwd (same question of a process and of a turn, and the table is durable)
  but is a different guarantee — a confinement boundary, not a convenience.
  Recurring *work* = an agent or command job, a recurring or one-shot *message*
  = a `message` job, one-shot scheduled work = `@at`.
- `apps/` — bun workspace: `apps/app` (shared React renderer) mounted by
  `apps/desktop` (Electron) and `apps/web` (SPA served via `web_dir`). Talks
  to the gateway over HTTP only (`HttpKomoClient`); feature-first layout;
  react-query for server state, zustand for client state; thread is
  assistant-ui over an async-generator adapter. Components may only use
  semantic theme tokens — `bun run lint` fails on raw colors. Commands:
  `cd apps && bun install`, `bun run check` (typecheck + lint + fmt + test).
  Conventions: `apps/app/README.md`.

## Extension points

- **Add a tool**: implement `Tool` in `crates/komo-tools/src/`, register in `cli/wiring.rs`
  (and add it to `tool_execution::policy_scope` if it should be policy-filterable).
- **Add an MCP server**: config only — an `[mcp.servers.<name>]` table with a
  `tools` allowlist. No code; that is the point of `komo-mcp` being generic.
- **Add a tool at run time**: a `@tool` function in a `$KOMO_HOME/plugins/*.py`
  file, no code in komo and no restart. That write is `Risk::Dangerous`.
- **Add an operator action**: a variant on `OperatorQuery` / `OperatorCommand`
  plus its reply variant and the arm in `operator_control/actions.rs` — never a
  new route.
- **Swap LLM provider**: implement `LlmClient` (`domain/llm.rs`), construct in
  `komo-bot`'s `llm::build_llm`.
- **Swap persistence**: implement the repository traits; `agent/`/`domain/`
  need no changes.
- **Add a provider**: an entry in `Provider` plus its base URL / auth / wire in
  `komo-bot`'s `llm` (`wire_for`, `endpoint_url`, `build_provider_llm`). A new *wire
  format* — only if it speaks neither Responses nor Messages — is a module in
  `crates/komo-provider` and a `Wire` variant.
- **Agent-loop control**: add round-level control points in `komo-bot`'s `run_agent_loop`;
  extend `TurnDriver`/`Step`. `komo-tools`' `ask_user.rs` is the
  sentinel-tool reference: a tool stops its turn with
  `ToolContext::wait_for(wakeup, …)` and reads `ctx.resumed_wait()` on the way
  back, which the executor and the loop treat exactly like a gated call that
  stopped for an approval.
- **Scheduled action**: implement `Maintenance`, construct in `cli/gateway.rs`.
- **Gateway ingress**: implement `Channel`, `add_channel` in `cli/gateway.rs`,
  gate behind a `[channels.*]` declaration — feishu is the reference.
- **Routine trigger**: a variant on `Trigger` plus its arm in `next_slot`
  (`komo-core`'s `domain::cron`) and a written form in
  `cron_actions::parse_schedule`. Nothing about *running* the routine
  changes — that is `RoutineEventSource`'s, and there is one of it.

## Testing

Tests live beside the code (`#[cfg(test)] mod tests`, `#[tokio::test]` for
async), named by behavior. **Always `cargo test --workspace`** — the bare root
command skips `crates/komo-core`.

## Coding style

`cargo fmt` defaults; `snake_case` modules/functions, `PascalCase` types. Small
modules, one responsibility; keep async db code in the layer that owns it. CLI
subcommands short and verb-based.

## Commit & PR style

Short imperative commits (`add file tool`). PRs: concise description, commands
run for verification, terminal output when CLI behavior changes.

## Repo docs

- Issues/PRDs: local markdown under `.scratch/<feature-slug>/` — `docs/agents/issue-tracker.md`
- Triage labels: `needs-triage` / `needs-info` / `ready-for-agent` / `ready-for-human` / `wontfix` — `docs/agents/triage-labels.md`
- Domain docs: `CONTEXT.md` + `docs/adr/` — `docs/agents/domain.md`
