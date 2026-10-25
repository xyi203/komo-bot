//! Durable 存储下的 cron 套件（`docs/storage-durable-d5.md` W3b）：测试名与 Legacy 版本
//! （`../{command,definition,firing,notify}.rs`）一一对应，走同一个 `Home` / `Gw`，只是
//! 用 `start_durable` 起 Gateway。断言改过的地方在各测试的注释里写了理由：新存储没有 `runs` 表与
//! `state.db`，也没有"停在等人核对上"的 `waiting`。

mod command;
mod definition;
mod firing;
mod notify;
