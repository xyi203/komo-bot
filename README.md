# komo

Rust 写的个人 Agent 框架：一个持有数据目录的 Gateway 进程，加上 TUI 与飞书 /
Telegram / 微信三个聊天入口。

设计文档是唯一真相：[`docs/komo_bot.md`](docs/komo_bot.md)。

## 安装

从 GitHub release 装（macOS 与 Linux，arm64 与 amd64）：

```bash
curl -fsSL https://raw.githubusercontent.com/xyi203/komo-bot/main/install.sh | bash
```

或者自己编（不需要 openssl-devel：微信渠道走 vendored rustls，见 [`docs/komo_bot.md`](docs/komo_bot.md) §13.4）：

```bash
cargo build --release
cargo test --workspace
cargo run -- gateway --foreground
```

升级用 `komo update`：它从 GitHub release 下同一套发布包，核对 sha256、解包、试跑
`--version`，最后才把磁盘上那份换掉（详见 [`docs/komo_bot.md`](docs/komo_bot.md) §13.6）。
