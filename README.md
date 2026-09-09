# Komo

<p align="center">
  <img src="docs/images/komo_logo.png" alt="Komo mascot, wordmark, and Light through your days slogan" width="520">
</p>

A personal agent framework in Rust. One binary gives you interactive LLM chat,
local tools, long-term memories, scheduled routines, and an always-on gateway
for chat channels and proactive background work. State lives locally under
`~/.komo`.

## Brand

**Komo** is inspired by the Japanese word *komorebi* (木漏れ日): sunlight
filtering through leaves. The image feels warm and clear, while suggesting how
small moments gather into something lasting—a natural fit for a personal agent
built around memory accumulating over time. The short, two-syllable name is
easy to say and remember, and adapts naturally to logos and domain names.

- **Candidate slogans:** 「记住每一缕光」 / 「陪你把日子攒成光」 / *Light through your days*
- **Visual language:** soft green, cream white, and sunlight yellow, with
  dappled-light shapes inspired by gaps between leaves
- **Personality:** a quiet friend beside you in the shade—warm and
  unobtrusive, attentive without being noisy, and able to remember the details
  entrusted to it

## Install

From GitHub release binaries (macOS):

```bash
curl -fsSL https://raw.githubusercontent.com/solren7/komo/main/install.sh | bash
```

Or build from source:

```bash
cargo build --release
```

## Quick start

```bash
komo init                       # scaffold ~/.komo/config.toml + .env + SOUL.md (never overwrites)
# then fill the DEEPSEEK_API_KEY= line in ~/.komo/.env

komo                            # start a task session in this directory (full-screen TUI)
komo home                       # the ongoing daily conversation, shared with every private channel
komo resume                     # continue the newest task bound to this directory (or `komo resume <id>`)
komo model list                 # show current provider/model
komo model set anthropic        # switch provider (persists to config.toml)
```

Everything boots without a key — the gateway starts and channels serve — but
agent turns reply with a "key not set" pointer until one is configured.

One task is one session, and its workspace is the task's environment: a task is
bound to the directory it was started in, so resuming it from somewhere else
continues the task rather than moving where its tools write. `/workspace add
<path>` widens it when the work spans projects. The home conversation is not
bound — it runs each turn in whatever directory it was entered from.

Inside chat, `/new` (or `/clear` / `/reset`) draws a conversation boundary: the
model's replay starts fresh, nothing is deleted. Transcripts are append-only
segment files under `~/.komo/sessions/<id>/`; session metadata and the run
ledger are tables in `~/.komo/komo.db`.

Every command that touches komo's state talks to the gateway, starting one if
none is running (on macOS through launchd; elsewhere start `komo gateway`
yourself).

```bash
komo session list               # stored sessions with their workspace and message counts
komo session clean              # delete empty sessions
komo cron list                  # routines (cron / @at) and next fire times
komo memory list                # memory candidates/active items
komo run list                   # recent agent turns (⟲ marks interrupted ones)
komo skills list                # managed ~/.komo/skills + the shared ~/.agents/skills
komo skills install <source>    # install a skill from a git repo or a SKILL.md URL
```

## Gateway (always-on background process)

The gateway is the process that owns komo's state: it hosts the agent, the chat
channels and the scheduled maintenance, and every other surface (the TUI, the
CLI, the desktop and web apps) is a client of its loopback HTTP api.

- reflective review sweeps over finished turns (memory extraction)
- routine firing: command, agent and message jobs on a cron slot or an `@at`
- nightly memory consolidation (the "dream" sweep)
- Feishu, Telegram, and WeChat channels when configured

```bash
komo gateway start              # macOS only: install + start under launchd
komo gateway status             # macOS only: launchd state
komo gateway restart            # macOS only: pick up a reinstalled binary
komo gateway stop               # macOS only: stop and remove from launchd
```

Bare `komo gateway` runs in the foreground (this is what launchd
invokes, and what Docker should run as the container process). In chat channels,
side-effecting tools can ask for approval in the conversation; reply `/approve`,
`/approve session`, or `/deny`.

## Built-in tools

The agent can call these during a chat turn:

| Tool | What it does |
|---|---|
| `shell` | Run shell commands — safe commands auto-approved, dangerous ones blocked, the rest prompt for approval |
| `read` / `write` / `edit` / `apply_patch` | File tools confined to the workspace roots plus `~/.komo/artifacts` and `~/.komo/plugins` |
| `grep` / `glob` | ripgrep in-process; policy runs over paths before content is read |
| `web_fetch` / `web_search` | Fetch pages and search the web |
| `cron` | Routines: command, agent or message jobs fired by a cron slot or an `@at` moment |
| `todo` | The current conversation's working focus list (the one thing `/new` clears) |
| `memory` | Govern long-term memories (candidates, pins, search) |
| `session` | Search komo's own past conversations (episodic memory) |
| `wiki_search` / `wiki_read` / `wiki_index` | Semantic search over a note vault when `[wiki]` is configured |
| `ask_user` | Suspend the turn until the user answers — across restarts |
| `delegate` | Run a sub-agent turn on the aux model |
| `run_code` | Run a Python program that calls the other tools through the same gates |
| `py__<name>` | A `@tool` function from `~/.komo/plugins/*.py`, mounted live |
| `homeassistant` | Read and control Home Assistant entities when configured |
| `skill` | Load skills: `~/.komo/skills` + the shared `~/.agents/skills` |
| `logs` | Tail komo's own tracing log |
| `time` | Current time (RFC 3339 UTC) |
| `mcp__<server>__<tool>` | Tools mounted from `[mcp.servers.*]`; every call is approval-gated |

## Data Layout

Everything lives in `~/.komo/` by default, or under `KOMO_HOME` when set.

| Path | Purpose | Durability |
|---|---|---|
| `komo.db` | one Turso database: sessions, run ledger, pairings, settings, search index (disposable by row or collection); memories, routines, wakeups (durable, additive schema only) | per table |
| `sessions/<id>/` | transcripts: a manifest plus append-only `.jsonl` segments | disposable |
| `artifacts/<session>/` | what a turn produced: reports, scripts, downloads | durable |
| `skills/` | skills (`SKILL.md`), written and installed by a human | durable |
| `permissions.json` | saved approval grants | durable |
| `plugins/` | Python plugin tools served by `komo-pyhost`; writing here needs an approval every time | durable |
| `tool-output/` | over-limit tool results (7-day retention) | disposable |
| `logs/` | daily-rotated gateway log (`komo logs`) | disposable |
| `config.toml` | provider/model/channel behavior | — |
| `.env` | API keys and channel credentials | — |
| `MEMORY.md` | operator-edited global L1 context, loaded next turn (8,000 characters; truncation is marked); no database pin injection | durable |
| `SOUL.md` · `USER.md` · `AGENTS.md` | persona, operator profile, machine-wide instructions (re-read on change) | — |

There is no database file to delete for a reset: disposable state is pruned by
row (`komo run prune`, `komo session clean`), and durable tables only ever change
additively. See `docs/adr/0004-single-database.md`.

## Configuration

Priority: built-in defaults < `config.toml` < `KOMO_*` env vars. API keys go
only in `~/.komo/.env`, never in `config.toml`.

`~/.komo/config.toml`:

```toml
provider = "deepseek"        # deepseek | openai | anthropic | openrouter | codex
# model = "..."             # optional; defaults per provider. DeepSeek entries must name a v4-or-later model
models = ["anthropic:claude-sonnet-5", "openai:gpt-5.5"]   # optional: what a session may switch to
base_url = "https://..."     # optional override for OpenAI-compatible endpoints
aux_model = "..."            # optional cheaper model for delegated sub-tasks
schedule = "0 * * * *"       # gateway maintenance cron (5-field, default hourly)
dream_schedule = "0 3 * * *"          # nightly memory governance sweep ("off" disables)
max_turns = 30               # max tool-calling round-trips per user turn
# pyhost_enabled = false     # optional: keep the Python plugin host (and run_code) out

[memory]
embedding_model = "bge-m3"   # optional Ollama model; enables cross-language recall and episodic search

[wiki]
vault = "~/notes"            # optional note vault behind wiki_search / wiki_read / wiki_index

[mcp.servers.github]
url = "https://..."
token_env = "GITHUB_MCP_TOKEN"        # names the .env var, never the token
tools = ["search_issues"]             # required allowlist (or all_tools = true)

[channels.telegram]
enabled = true
allow_from = ["123456789"]
home_chat = "123456789"

[channels.feishu]
enabled = true
allow_from = ["ou_xxx"]
home_chat = "oc_xxx"

[channels.wechat]
enabled = true
allow_from = ["wxid_xxx"]

# Permission policy: auto-allow / hard-deny side-effecting actions instead of
# prompting for each one. Deny beats allow; anything unmatched falls back to
# `default_normal` (ask). Read-only actions (web fetches, file reads) are
# deny-only: a deny rule can block them, nothing ever prompts for them.
[policy]
default_normal = "ask"       # ask | deny | allow — fallback for unmatched Normal actions

[[policy.rule]]              # let cargo/git run without prompting…
category = "shell"           # shell | file | network | homeassistant | mcp | wiki | plugin
match = "prefix"             # prefix | suffix | exact | contains
value = "cargo "
effect = "allow"

[[policy.rule]]              # …but never talk to the internal network
category = "network"
match = "suffix"             # network matches the URL host, on dot boundaries
value = "internal.corp"
effect = "deny"

[[policy.rule]]              # and keep key material unreadable even in-workspace
category = "file"
match = "contains"
value = ".ssh"
access = "read"              # file rules can scope to read | write
effect = "deny"
```

Verify with `komo policy list` (resolved rules) and
`komo policy check <category> <target>` (dry-run one action, shows the
matching rule). Rules can also scope to channels
(`channels = ["telegram"]`), and an allow rule only covers
`Risk::Dangerous` actions when it sets `include_dangerous = true`.

`read`, `grep`, and `glob` may inspect any local path. `write`, `edit`,
`apply_patch`, and `shell` workdirs remain confined to the session workspace.
Use a `[policy]` rule with `category = "file"` and `access = "read"` to deny
specific sensitive paths (for example `.ssh` or credential directories).

| Provider | API key env var |
|---|---|
| `deepseek` | `DEEPSEEK_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |
| `codex` | none — reads the Codex CLI's OAuth file (`~/.codex/auth.json`, else `~/.komo/.codex/auth.json` or `~/.komo/codex/auth.json`; `$CODEX_HOME` overrides them all) |

`codex` is the exception to the `.env` rule: it authenticates with the OAuth
tokens the Codex CLI writes, so there is nothing to paste. On a host with no
Codex CLI (a container), copy `auth.json` from a machine that does have one to
`$KOMO_HOME/.codex/auth.json` (or `$KOMO_HOME/codex/auth.json`) — komo refreshes
it in place from there, and
`komo doctor` prints the file it actually chose.

Channel credentials live in `.env`, for example:

```bash
FEISHU_APP_ID=cli_xxx
FEISHU_APP_SECRET=xxx
TELEGRAM_BOT_TOKEN=xxx
HASS_TOKEN=xxx
HASS_URL=http://homeassistant.local:8123
```

Use `komo channel list` to see resolved configuration and the channels loaded by
the running gateway; add `--json` for scripts. `komo channel probe <channel>`
validates a configured provider without sending a message. The API channel
remains loopback-only by default and must be exposed manually in `config.toml`.

WeChat is QR-based: run `komo channel wechat login` on the host, or send `/wechat login`
from an already-working chat channel.

## Architecture

DDD-style layers with domain traits at the center:

```
CLI/channel → AgentRuntime ─ run_agent_loop ─┬→ LlmClient::begin_turn → TurnDriver (one provider completion / round)
                                             └→ ToolExecutor::execute_round → tools   (loop until Step::Final)
                          ↘ MessageRepository · RunRepository (ledger) → Response
```

komo owns the tool loop: `AgentRuntime::run_agent_loop` drives the model one
round at a time and hands each round of requested tool calls to the
`ToolExecutor`, where every call is isolated, retried on transient failures,
traced, and recorded in the run ledger.

### Project layout

A Cargo workspace; crates depend downward only.

```
crates/
├── komo-core        traits + value types (Tool, LlmClient, repositories, policy, run ledger); no I/O
├── komo-config      config.toml + .env + KOMO_* → one ConfigSnapshot
├── komo-provider    LLM wire formats (Responses / Messages) + HTTP/SSE transport
├── komo-mcp         MCP client (Streamable HTTP)
├── komo-pyhost      out-of-process Python plugin host behind run_code and ~/.komo/plugins
├── komo-infra       persistence (Turso/toasty) · chunk index · memory store · skills · logs · embedding · codex auth
├── komo-services    tool execution · memory query/consolidation · skill registry · cron actions · wiki indexing
├── komo-tools       every built-in tool
└── komo-bot         runtime (run_agent_loop) · gateway · daemon sweeps · interaction · system prompt · policy approver · reviewer
src/                 the binary: cli/ · tui/ · pyhost · infra/messaging (channels) · infra/gateway_client · services/operator_control
apps/                bun workspace: shared React renderer mounted by the Electron desktop app and the web SPA
```

## Development

```bash
cargo check          # fast compile check
cargo test --workspace   # bare `cargo test` skips the komo-core tests
cargo fmt            # format
cargo run -- chat    # run from source
cargo run -- gateway # foreground gateway
```

Building requires `protoc` (`brew install protobuf`) because the Feishu websocket
dependency compiles protobuf frames at build time.

Schema changes need no reset: new columns are added in place on connect
(`ensure_columns`), new tables with `ensure_table`. Durable tables
(memories, routines, wakeups) only ever change additively — see `AGENTS.md`.

## Docs

- [AGENTS.md](AGENTS.md) — the live architecture guide: commands, storage rules, module map, extension points.
- [CONTEXT.md](CONTEXT.md) + [docs/adr/](docs/adr/) — glossary and architecture decision records.
- [docs/bot-runtime.md](docs/bot-runtime.md) — suspended turns, wakeups and routines.
- [docs/episode-learning-framework.md](docs/episode-learning-framework.md) — the post-run learning pass.

### L1 memory

Edit `$KOMO_HOME/MEMORY.md` (default `~/.komo/MEMORY.md`) to change the
global context carried into each main-agent turn. Creation, edits, clearing
and deletion take effect on the next turn without restarting the gateway.
The file is background knowledge, not executable instructions. It is shared
across main-agent conversations; keep channel-specific information in the
scoped database memory library.

`komo init` creates an empty file without overwriting an existing one.
The old database pin command, tool parameter and API are removed. Existing
database memories remain available for relevant L3 recall; they are not
automatically exported into this file. Dream and automatic extraction do not
edit the file. Database conflict handling cannot edit or retire its contents;
the operator maintains it explicitly.

`config.toml` and credentials still require a gateway restart; prompt-file
refresh does not reload runtime configuration.
