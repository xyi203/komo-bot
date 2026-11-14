/**
 * 网页期望的 Gateway 协议版本。与 kernel 的协议版本常量（`crates/komo-kernel/src/protocol/mod.rs`
 * 的 `PROTOCOL_VERSION`）保持一致：服务端改了协议版本，这里同步改。
 * 启动时与 `/healthz` 的 `protocol_version` 比较，不一致时顶部提示刷新或升级。
 */
export const EXPECTED_PROTOCOL_VERSION = 1;
