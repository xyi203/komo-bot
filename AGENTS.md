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
komo intervention list|show|answer
komo cron … | komo memory … | komo skills … | komo doctor
komo update                    # swap the on-disk binary from the GitHub release (§13.6)
```

The full command table is §3 of the design.

## Crate layout (§13.4)

```text
komo-kernel    value types, state machines, events + fold, cron math,
               §8.4 recovery decision table, wire protocol, ALL traits.
               No tokio, no I/O. lib.rs is module declarations only.
komo-store     session_log (JSONL) · payloads · tool_output · Turso Db + toasty
               models + *_TABLE_DDL · repositories · Coordinator (impl Ledger).
               The only crate that sees toasty/turso.
komo-runtime   agent loop · executor · tools/{read,write,edit,rg,bash} ·
               codemode (Monty worker) · memory · llm · embedding ·
               scheduler · recovery · config.
komo-agent     AgentProfile → 能力面选择 · skills 发现与目录 · 编排操作名 ·
               上下文装配（context：ContextInput → AgentContext，系统提示、回放、记忆段）。
               只依赖 kernel，不执行；runtime 不依赖它，Gateway 组装两边。
               值类型（AgentProfile/AgentSurface/RunSnapshot）留在 kernel。
komo-gateway   axum routes · SSE · auth · lock/discovery · launchd/systemd ·
               Dispatcher · channels/{feishu,telegram,wechat} (feature-gated) ·
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
- **Content authority is the Session directory, scheduling authority
  is state.db** (§8.2). Write order per §8.5: external body → JSONL
  append + `sync_all` → db transaction. Never the other way round.
- **Nothing rewrites a JSONL line.** Cancellation, interjection, compaction are
  appended events; `fold` decides what they mean. Unknown event types keep
  their seq and mean nothing (§8.3).
- **There is no Policy and no approval** (§7, removed 2026-10-11). What the
  model asks, komo executes once the tool's own `prepare` and the capability
  surface accept it; `Tool::execute` takes the `ExecutionPlan`. Old approval
  traces must still decode (§7.4): `approval.*` events mean nothing, a Run left
  `waiting + approval` resumes as approved, retired delivery/outbox rows are
  skipped and never block a flush.
- **The only hard limit lives in the tools:** `write`/`edit` refuse komo's own
  state (`sessions/`, `state.db*`, `durable.db*`, `aux.db*`, `runtime/`, `.env` under the data dir).
  bash cannot be stopped; real boundaries come only from an execution
  environment (codemode's Monty engine: no network or subprocesses, files
  read-only with the same protected list denied even for reads, except
  `sessions/`, which `read` and bash can read anyway; it runs in a
  `komo __codemode-worker` child the host can kill).
- **Recovery decides "did it happen" before "retry"** (§8.4 table, §8.6).
  `started` alone proves nothing. A started call without a result reruns only
  when both the plan's stored replay policy and the tool's current
  `Tool::replay` are `Safe` (pi-durable's rule; default `Unsafe`); otherwise it
  settles as `uncertain` with the partial output it left on disk and goes to
  the model — the executor never silently re-runs it.
- **Tool result 的模型视图只有一处投影**（`komo-kernel/src/projection.rs`）。输入只有落盘的
  事实（完整 JSONL 事件 + `output.json` / stdout / stderr 引用；旧行的 `text_ref` 与 ≤1 KiB
  `preview` 只作兼容读取），所以"刚跑完"与"重启后回放"必须逐字节相同。full 视图默认最多
  50 KiB 或 2000 行（二者先到）：`bash` 留尾（旧记录里叫 `shell`，两个名字都认），其他工具留头；超限时必须给出可
  `read` 的完整 `artifact://` 引用。`[execution] model_result_bytes` 可收紧字节预算；当前
  Session 的 `tool-output/` 与 `artifacts/` 是只读根。大结果给哪个视图由 `projection.rs` 的
  `select` 按 `context.compacted` 的 `decayed` 记下的集合选；decay 只改模型视图，不改完整
  记录，换不换仍由缓存账决定（`compaction::decide`），前缀默认只追加。
- **Channel identity lives in config, not the db** (§11.2): `allow_from` /
  `home_chat` / `groups` in `config.toml`, credentials in `.env`. No pairing
  table, no `/sethome`.
- **The release convention is one string in three places** (§13.6): the repo
  name, `komo-<os>-<arch>.tar.gz`, and `SHA256SUMS` appear in
  `.github/workflows/release.yml`, `scripts/install.sh`, and
  `crates/komo/src/update.rs` — change one, change all three. The tag must
  equal `[workspace.package].version`, or `komo update` refuses the package.
- **Config hot-reloads** (§3): one `Arc<ConfigSnapshot>` swapped atomically;
  readers read the current snapshot per use, running Runs keep the snapshot
  they started with. An invalid file is never installed. A short list of keys
  is start-only and is *reported* as such, never ignored.
- **Schema changes are additive.** `*_TABLE_DDL` beside each toasty model,
  byte-parity test against what toasty generates; a file DB is migrated *before*
  the pool is built (`Db::connect` → `migrate_file`, a plain non-MVCC connection
  — DDL on an MVCC connection returns `Ok` and persists nothing), and
  `ensure_schema` then only guards (errors if a file DB still misses a column)
  and repairs memory DBs; retired columns keep being written empty (§8.2).
- **Turso MVCC**: string UUIDv7 keys, never AUTOINCREMENT; single writes in
  `with_write_retry`, multi-write in a transaction inside it.

## Coding style

- `dyn` at seams (`Arc<dyn Ledger>`, `Vec<Box<dyn Tool>>`), no generic
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
- Memory: `docs/memory.md` — §9 spec, planned memory designs, and memory
  research/analysis appendices. Memory changes update this file, not a new one.
- Observability: `docs/observability.md` — OpenTelemetry traces/metrics, logs,
  operator views. Telemetry is never a source of truth; nothing reads it back.
- Work plan: `.scratch/komo-v08-rewrite/PLAN.md`, progress in `STATUS.md`.
