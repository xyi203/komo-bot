# AGENTS.md

Guidance for coding agents working in this repository. `CLAUDE.md` is a
symlink to this file — edit `AGENTS.md` only.

komo v0.8 is a **from-scratch** Rust personal-agent framework. The design is
`docs/komo_bot.md` (its §9 Memory lives in `docs/memory.md`, same section
numbers) and it is the single source of truth: when this file and
the design disagree, the design wins and this file is wrong. Nothing here
carries over from the implementation on `main`; do not read that branch for
patterns.

## Commands

```bash
cargo build                    # cold build budget ≤ 60s (§13.4)
cargo test --workspace         # ALWAYS --workspace; ~10 min cold — from komo's
                               # bash tool pass timeout_secs ≥ 900
scripts/install-local.sh       # release build → ~/.local/bin/komo (atomic) → gateway restart;
                               # worktrees share the main checkout's target/

cargo fmt --check
cargo tree -d                  # duplicate versions must be attributed per channel
cargo build --timings          # rerun at the end of every phase, record in docs

komo init                      # write a minimal commented config.toml + 0600 .env (never overwrites)
komo                           # start Gateway if needed, new session, TUI
komo resume SESSION_ID
komo gateway [--foreground] | gateway status|stop|restart
komo config check | config reload
komo channel list|probe | channel wechat login
komo session list|delete|purge|export SESSION_ID [-o FILE]   # export = pi v3 树形 JSONL
komo intervention list|show|answer
komo cron … | komo memory … | komo goal … | komo area … | komo skills … | komo doctor
komo update                    # swap the on-disk binary from the GitHub release (§13.6)
```

The full command table is §3 of the design.

## Crate layout (§13.4)

```text
komo-kernel    value types, `durable/` (entry · conversation · task · submission ·
               document records, context derivation, fork, Task state machine,
               compaction logic, `Storage` trait + in-memory impl + conformance,
               `pi_tree` / `legacy_events` projections), cron math, wire protocol, ALL traits. No tokio, no I/O.
               lib.rs is module declarations only.
komo-store     `durable/`: `Line` (the single commit line), Turso `Storage`, DDL,
               blob writes, id high-water mark — raw `turso`, no toasty.
               Plus `aux.db`: toasty models + *_TABLE_DDL · repositories (cron,
               deliveries, memory) · tool_output · session_dir / purge (Session directory
               paths, pre-purge reference check). The only crate that sees
               toasty/turso.
komo-runtime   `durable/`: Task engine + harness (generation / tool / inbox /
               compaction / orchestration / codemode, the agent loop organised
               by pi's hooks) · tools/{read,write,edit,rg,bash} ·
               codemode (Monty worker) · memory · llm (stateless `ModelStream`) ·
               embedding · cron scheduler · config.
komo-agent     AgentProfile → 能力面选择 · skills 发现与目录 · 编排操作名 ·
               系统段装配（context：system_sections、task_contract_block、project_update_block，压缩摘要提示）。
               只依赖 kernel，不执行；runtime 不依赖它，Gateway 组装两边。
               值类型（AgentProfile/AgentSurface）留在 kernel。
komo-gateway   axum routes (`http/v2`) · `service/durable` (DurableCore: hooks,
               sessions/tree, feed, lifecycle) · SSE · auth · lock/discovery ·
               launchd/systemd · Dispatcher · channels/{feishu,telegram,wechat} (feature-gated) ·
               Notifier · deliveries · intervention rendering · config reload.
komo-client    HTTP + SSE client · discovery · ratatui TUI · command output.
               Depends on kernel only.
komo (bin)     clap dispatch only (incl. the hidden `__codemode-worker`, whose
               body is `komo_runtime::codemode::worker::run`).
```

Dependencies point downward only: `kernel ← store ← runtime ← gateway`,
`kernel ← agent ← gateway`, and `kernel ← client`, meeting in the bin.
`gateway` never sees toasty or ratatui; `runtime` never sees axum and never
depends on `agent`; `client` knows only protocol types.

## Rules that are structural, not stylistic

- **Gateway is the only process that opens state.** CLI and TUI go over
  HTTP/SSE; there is no direct-db path (§3).
- **`durable.db` is the single authority** (§8.2): conversation content
  (entries), Run/call state (tasks, submissions), mutable state (documents),
  lifecycle — one commit line, one transaction per commit, nothing observable
  before it commits. Big bodies (tool output, images, artifacts) live in the
  Session directory as *side-effect products*: write the file and sync it
  first, then commit the entry that references it. There is no second
  authority and no events.jsonl (that is only the export format).
- **Entries are immutable.** Cancellation, interjection, compaction, decay are
  appended entries; rewriting is a write entry's `edits` (replace/omit a
  target's model view) or `head` (where context starts), never an update
  (§8.1, §8.3).
- **There is no Policy and no approval** (§7, removed 2026-10-11). What the
  model asks, komo executes once the tool's own `prepare` and the capability
  surface accept it; `Tool::execute` takes the `ExecutionPlan`. One exception
  (`docs/confirm.md`): a Run started by a trigger (cron, monitor, task result,
  task session) calling a tool declared `outward` waits in the `confirm`
  checkpoint for the operator's `approve` / `reject` — no rule table, no
  "always allow". Nothing
  migrates from the old store (§8): old `sessions/*/events.jsonl` and
  `state.db*` stay on disk, unread.
- **The only hard limit lives in the tools:** `write`/`edit` refuse komo's own
  state (`sessions/`, `durable.db*`, `aux.db*`, `runtime/`, `.env` under the data dir; the
  list also keeps the old `state.db*`).
  bash cannot be stopped; real boundaries come only from an execution
  environment (codemode's Monty engine: no network or subprocesses, files
  read-only with the same protected list denied even for reads, except
  `sessions/`, which `read` and bash can read anyway; it runs in a
  `komo __codemode-worker` child the host can kill).
- **Side effects are an effect sandwich** (§8.5): commit the intent (the
  `komo.live` slot + the `execute` checkpoint carrying the full
  `ExecutionPlan`), do the effect, finish the blob (sync → rename → sync dir),
  then commit the result entry. No intent, no effect; no blob before its
  reference. Effects that are not tools (task spawn, monitor registration, cron
  firing) do the same: effect and checkpoint advance in one commit, the result
  in the next.
- **Recovery follows the §5 table of `docs/storage-durable.md` (§8.4, §8.6):**
  `running` tasks go back to `pending` and resume at their checkpoint phase;
  a tool task at `execute` first reuses a complete, hash-checked `output.json`,
  then reruns only when both the stored replay policy and the tool's current
  `Tool::replay` are `Safe` (default `Unsafe`), otherwise it settles as
  `uncertain` with the `.partial` tail and goes to the model — the engine never
  silently re-runs it. A recorded live child process is `probe`d every beat
  (`Unknown` counts as alive) before that decision. Startup reconcile only
  checks that referenced blobs are readable; one that is not faults that task.
- **Tool result 的模型视图只有一处投影**（`komo-kernel/src/projection.rs`），**收尾时算一次、
  存进结果 entry 的 `model`**，所以"刚跑完"与"重启后回放"逐字节相同（存下的就是那份）。
  输入只有落盘的事实（完整 `output.json` / stdout / stderr blob）。full 视图放得下
  （30 KiB 且 2000 行以内；30 KiB 也是上限，配置只能往小调——模型后端超过约 25000 token 会把整条
  结果换掉）就给全文，放不下每个工具都只给开头 2 KiB 预览并写明裁了多少；超限时必须给出可
  `read` 的完整 `artifact://<conversation>/<task>/<attempt>/result` 引用。`[execution] model_result_bytes`
  只影响之后落的结果；当前 Session 的 `tool-output/` 与 `artifacts/` 是只读根。衰减写成一条
  `komo.decay` entry，只带 `edits`（`replace` 成 `data.decayed` 的短视图），回放按 edits 选视图；
  decay 只改模型视图，不改完整 blob，换不换由缓存账决定（`compaction::decide`），前缀默认只追加。
- **Channel identity lives in config, not the db** (§11.2): `allow_from` /
  `home_chat` / `groups` in `config.toml`, credentials in `.env`. No pairing
  table, no `/sethome`.
- **The release convention is one string in three places** (§13.6): the repo
  name, `komo-<os>-<arch>.tar.gz`, and `SHA256SUMS` appear in
  `.github/workflows/release.yml`, `scripts/install.sh`, and
  `crates/komo/src/update.rs` — change one, change all three. The tag must
  equal `[workspace.package].version`, or `komo update` refuses the package.
- **Config hot-reloads** (§3): one `Arc<ConfigSnapshot>` swapped atomically;
  readers read the current snapshot per use (every phase resolves Agent/model by
  name; an already-started `request` keeps the model/effort frozen in its checkpoint). An invalid file is never installed. A short list of keys
  is start-only and is *reported* as such, never ignored.
- **Two databases, two regimes.** `durable.db` (new file, Turso **WAL**, single
  write line): bare `turso` SQL only in `komo-store/src/durable/{schema,write,line,storage}.rs`,
  `PRAGMA synchronous=FULL` per structural commit and `OFF` for explicit progress commits,
  `data_sync_retry=1`, ids are global monotonic numbers minted on the line with a
  persisted high-water mark (never reused; not AUTOINCREMENT), closures never `await`
  models/subprocesses/files/users, a storage error fails the whole line. Schema is a
  `*_TABLE_DDL` constant per table with a byte-parity test; additive after the first
  version. `aux.db` (cron, deliveries, memory) is still toasty + Turso MVCC: DDL
  before the pool on a plain connection (MVCC DDL persists nothing), writes in a
  transaction inside `with_write_retry`, string UUIDv7 keys, retired columns keep
  being written empty.

## Coding style

- `dyn` at seams (`Arc<dyn Storage>`, `Arc<dyn ModelStream>`, `Vec<Box<dyn Tool>>`), no generic
  executors. `async_trait` for async traits.
- Derive macros: `serde`, `thiserror::Error`, `toasty::Model` (store only),
  `clap::Parser` (bin only). No `strum`, `derive_more`, `bon`.
- Tests beside code in `#[cfg(test)] mod tests`, named by behavior.
  Cross-crate test doubles live behind `komo-kernel`'s `test-support` feature,
  enabled only as a dev-dependency.
- No new dependencies without updating §13.4's dependency table (features
  pinned; `reqwest` rustls-no-provider, `toasty` turso-only, `tokio` per-need).
- `cargo fmt` defaults; small modules, one responsibility; short verb-based
  CLI subcommands.

## Commit & PR style

Short imperative commits (`add ledger coordinator`). PRs: what changed,
commands run, terminal output when CLI behavior changes.

## Repo docs

- Design: `docs/komo_bot.md` (§14 has the acceptance table and the
  "pending verification" list — write verified facts back there).
- Storage and agent loop (the pi-durable commit model): `docs/storage-durable.md`
  (derivation, deviations, progress), `docs/storage-durable-d5.md` (gateway wiring) and
  `docs/storage-durable-perf.md` (P1 implemented; later optimizations pending);
  `docs/session-tree.md`, `docs/background-tasks.md`, `docs/monitor.md`,
  `docs/codemode.md` and `docs/codemode-bash.md` describe the pieces on top of it
  (including implemented nested bash calls and live slots). `docs/handoff.md`
  proposes a model-initiated reset that keeps the run going; not implemented yet.
- Memory: `docs/memory.md` — §9 spec, planned memory designs, and memory
  research/analysis appendices. Memory changes update this file, not a new one.
- Observability: `docs/observability.md` — OpenTelemetry traces/metrics, logs,
  operator views. Telemetry is never a source of truth; nothing reads it back.
- Goals and areas: `docs/goals.md` — PARA/GTD-style goals (next action,
  waiting, parked) and areas, memory belongs-to and archive on close; spec
  approved 2026-10-29, implemented in slices P1a–P1e.
- Direction: `docs/roadmap-muse.md` — gaps and phases toward a Muse-style
  proactive personal agent (PARA-style goals and areas, proactivity,
  outward-action confirmation);
  draft, not approved. Approved pieces move into the specs above.
- Work plan: `.scratch/komo-v08-rewrite/PLAN.md`, progress in `STATUS.md`.
