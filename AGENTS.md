# AGENTS.md

Guidance for coding agents working in this repository. `CLAUDE.md` is a
symlink to this file — edit `AGENTS.md` only.

komo v0.8 is a **from-scratch** Rust personal-agent framework. The design is
`docs/komo_bot.md` and it is the single source of truth: when this file and
the design disagree, the design wins and this file is wrong. Nothing here
carries over from the implementation on `main`; do not read that branch for
patterns.

## Commands

```bash
cargo build                    # cold build budget ≤ 60s (§13.4)
cargo test --workspace         # ALWAYS --workspace
cargo fmt --check
cargo tree -d                  # duplicate versions must be attributed per channel
cargo build --timings          # rerun at the end of every phase, record in docs

komo                           # start Gateway if needed, new session, TUI
komo resume SESSION_ID
komo gateway [--foreground] | gateway status|stop|restart
komo config check | config reload
komo channel list|probe | channel wechat login
komo approval list|show|approve|reject
komo cron … | komo memory … | komo skills … | komo doctor
komo update                    # swap the on-disk binary from the GitHub release (§13.6)
```

The full command table is §3 of the design.

## Crate layout (§13.4)

```text
komo-kernel    value types, state machines, events + fold, Policy engine,
               cron math, §8.4 recovery decision table, wire protocol, ALL traits.
               No tokio, no I/O. lib.rs is module declarations only.
komo-store     session_log (JSONL) · payloads · tool_output · Turso Db + toasty
               models + *_TABLE_DDL · repositories · Coordinator (impl Ledger).
               The only crate that sees toasty/turso.
komo-runtime   agent loop · executor · tools/{read,write,edit,shell,python} ·
               python_runtime · policy · approvals · memory · llm · embedding ·
               scheduler · recovery · skills · config.
komo-gateway   axum routes · SSE · auth · lock/discovery · launchd/systemd ·
               Dispatcher · channels/{feishu,telegram,wechat} (feature-gated) ·
               Notifier · deliveries · approval rendering · config reload.
komo-client    HTTP + SSE client · discovery · ratatui TUI · command output.
               Depends on kernel only.
komo (bin)     clap dispatch only.
```

Dependencies point downward only: `kernel ← store ← runtime ← gateway` and
`kernel ← client`, meeting in the bin. `gateway` never sees toasty or ratatui;
`runtime` never sees axum; `client` knows only protocol types.

## Rules that are structural, not stylistic

- **Gateway is the only process that opens state.** CLI and TUI go over
  HTTP/SSE; there is no direct-db path (§3).
- **Content authority is the Session directory, scheduling/authorization
  authority is state.db** (§8.2). Write order per §8.5: external body → JSONL
  append + `sync_all` → db transaction. Never the other way round.
- **Nothing rewrites a JSONL line.** Cancellation, interjection, compaction are
  appended events; `fold` decides what they mean. Unknown event types keep
  their seq and mean nothing (§8.3).
- **Policy is a pure function** on an `ExecutionPlan` + `PolicyContext`. Grants
  are passed in, never looked up inside. `Ask` suspends the Run; `Deny` cannot
  be overridden by any grant. You cannot reach `execute` without a `Proof`
  (type state, §7).
- **Approvals bind a plan hash.** Repeated answers are idempotent; a consumed
  approval is not a retry license (§7.4).
- **Recovery decides "did it happen" before "retry"** (§8.4 table, §8.6).
  `started` alone proves nothing. Unknown outcome → `uncertain`, surfaced to
  the operator, never silently re-run.
- **Channel identity lives in config, not the db** (§11.2): `allow_from` /
  `home_chat` / `groups` in `config.toml`, credentials in `.env`. No pairing
  table, no `/sethome`.
- **The release convention is one string in three places** (§13.6): the repo
  name, `komo-<os>-<arch>.tar.gz`, and `SHA256SUMS` appear in
  `.github/workflows/release.yml`, `install.sh`, and
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
- Work plan: `.scratch/komo-v08-rewrite/PLAN.md`, progress in `STATUS.md`.
