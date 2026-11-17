# Telegram 备用入口

> 状态：已实现（2026-11-17）。只用测试里的假 Bot API 跑过，还没对真 bot 实测。决定见 `docs/redesign.md` P17 的修订。

## 1. 为什么要有

聊天渠道在 2026-11-13 整体删掉（P17）之后，网页是唯一的入口。下面这些情况网页进不去，但 Gateway 本身还活着：

- Cloudflare 隧道或反向代理断了；
- 登录服务（Pocket ID）挂了，或者 OIDC 配置出错；
- 网页壳发版发坏了。

Telegram 长轮询是 Gateway **主动往外连**，不需要公开地址，这几种情况都绕得过去。Gateway 进程自己挂了它帮不上：那靠 launchd / systemd 拉起，外加外部的存活监控。

## 2. 做什么、不做什么

| 做 | 不做 |
|---|---|
| 只认 `operator` 那个人的私聊；别人、群里的消息不理也不回 | 群聊、话题、多个操作者 |
| 一句话进主对话（与网页发消息同一条路），这一轮结算后把文字回复发回来，长的切段 | 图片、文件（回一句"回网页发"） |
| 主动通知：与浏览器推送同一组时机（停住的运行在问你、定时结果、不是你发起的那一轮说了话），带全文和回网页的链接 | 卡片、按钮、MarkdownV2 渲染（纯文本） |
| 命令：`/status`、`/pending`、`/approve` `/reject` `/resolve` `/abandon`（只有一条能答时可省句柄） | 渠道命令 `/new` 之类，旧渠道的投递账 |

## 3. 实现

- 配置（启动时读，改了要重启）：

  ```toml
  [telegram]
  operator = 123456789               # 你的 Telegram 用户 id
  # token_env = "KOMO_TELEGRAM_BOT_TOKEN"   # .env 里放 bot token 的变量，默认这个
  # endpoint = "https://api.telegram.org"   # 自建代理时改
  ```

  `.env` 里没有 token 时记一条警告、不开，不拦 Gateway 启动。`operator` 不是正整数、`endpoint` 不是 http(s) 地址拒绝加载。
- 代码在 `crates/komo-gateway/src/telegram/`，直接跑在 Gateway 进程里，没有插件进程。`api.rs` 是从删掉的
  `komo-channel` 里拿回来的长轮询与 `sendMessage`（错误里不带 URL，URL 里有 token）。
- 收：`getUpdates` 长轮询 25 秒；失败从 2 秒退避到 1 分钟；401 / 404（token 不对、bot 被删）停下并记错误。
  输入的请求键是 `telegram:<update_id>`，重启后同一个 update 再来一次只受理一次，也不重复回复。
  entry 是普通的 `cli.input`，`data.via = "telegram"`，所以算操作者发起的一轮（确认卡照常由操作者答复）。
- 回：后台等这条 submission 结算，取那一轮的文字回复（`watch::completed_text`）；没正常完成就回一句"回网页看过程"。
- 主动通知：`GatewayState::notify_operator` 同时走浏览器推送和 Telegram，两个触发点（`chats/say.rs`、`background/watch.rs`）都改成调它。
- 测试：`crates/komo-gateway/tests/telegram/`（假 Bot API：进主对话并回复、别人的消息不理、重投去重、命令、通知全文），
  `telegram` 模块里的命令解析与切段单测，`komo-config` 的配置读取与校验。
