# komo

Rust 写的个人 Agent 框架：一个持有数据目录的 Gateway 进程，加上网页（`komo web`）作为对话入口（飞书 / Telegram /
微信聊天渠道已于 2026-11-13 删除）。

设计文档是唯一真相：[`docs/komo_bot.md`](docs/komo_bot.md)。

## 安装

从 GitHub release 装（macOS 与 Linux，arm64 与 amd64）：

```bash
curl -fsSL https://raw.githubusercontent.com/xyi203/komo-bot/main/scripts/install.sh | bash
```

默认装到 `~/.local/bin/komo`（不用 sudo；`--prefix` 可改）。安装脚本在
[`scripts/install.sh`](scripts/install.sh)；2026-10-15 之前的地址 `…/main/install.sh` 已经失效。

或者自己编（不需要 openssl-devel，见 [`docs/komo_bot.md`](docs/komo_bot.md) §13.4）：

```bash
cargo build --release
cargo test --workspace
cargo run -- gateway --foreground
```

改完本地代码要换上正在用的 komo：`scripts/install-local.sh`（编译 release、原子替换
`~/.local/bin/komo`、重启 Gateway；`--no-restart` 只装不重启）。

装好之后先 `komo init`：在数据目录（`KOMO_HOME`，默认 `~/.komo`）生成一份带注释的最小
`config.toml` 和权限 600 的 `.env` 模板，已有 `config.toml` 不会被覆盖。照着改 `base_url` 与
模型名，在 `.env` 里填 API key，再 `komo config check` 校验，通过后直接 `komo`（等同 `komo web`：确认 Gateway 在跑，打开网页地址，没登录时由网页走 OIDC 登录，在浏览器里接着聊）。

升级用 `komo update`：它从 GitHub release 下同一套发布包，核对 sha256、解包、试跑
`--version`，最后才把磁盘上那份换掉（详见 [`docs/komo_bot.md`](docs/komo_bot.md) §13.6）。
