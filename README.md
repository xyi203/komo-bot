# komo

Rust 写的个人 Agent 框架：一个持有数据目录的 Gateway 进程，加上 TUI 与飞书 /
Telegram / 微信三个聊天入口。

设计文档是唯一真相：[`docs/komo_bot.md`](docs/komo_bot.md)。

```bash
cargo build              # 需要 Fedora 的 openssl-devel（微信渠道的 native-tls）
cargo test --workspace
cargo run -- gateway --foreground
```
