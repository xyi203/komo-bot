//! openlark 的 ws 长连接，跑在**独立的 std 线程**上（§11.1）。
//!
//! 为什么是一条自己的线程而不是一个 tokio 任务：`LarkWsClient::open` 是一个跑到连接断
//! 开才返回的 future，它内部又 `spawn_blocking` 调用户的 handler；把它放进 gateway 的
//! 运行时，一个长连接就常驻占着一个工作线程，而它的生命周期与 gateway 的任何一次请求
//! 都无关。事件经 `tokio::sync::mpsc` 交给 tokio 侧的 `Channel::serve`。
//!
//! **重连是我们的事。** openlark 明说自己不实现重连策略（`ClientConfig` 里的
//! `Reconnect*` 字段被它忽略，见 openlark-client 0.20.0 `ws_client/client.rs` 的注释
//! "本 crate 不实现重连策略（#421）"），而且**正常断开也返回 `Err(ConnectionClosed)`**。
//! 所以这里是一个退避重连的循环：断了就重连，重连期间飞书按 15s / 5min / 1h / 6h 重推
//! 未确认的事件（spike callbacks.md 1b/1f），重推靠 `event_id` 去重挡住（§11.1）。
//!
//! **ack 的实际语义（与 §11.1 的偏差，写在这里免得有人以为已经对齐了）**：飞书的 ws
//! 数据帧由 openlark 在 `FrameHandler::handle_data_frame` 里应答，`EventAck { code }`
//! 取自 `EventDispatcherHandler::do_without_validation` 的返回；而我们用的
//! `payload_sender` 分支只是把负载 `send` 进一个**无界 channel** 就返回 `Ok`。所以
//! **ack 早于 `Inbound::handle`**，不是 §11.1 要求的"之后"。
//!
//! 为什么不改：`EventHandler::handle` 是**同步**的，要在里面等 Dispatcher 就得阻塞
//! openlark 的 handler worker——那条 worker 是串行的，一个跑三分钟的 Run 会把它后面所
//! 有事件一起堵住，而飞书只给 3 秒，超时照样重推。代价换来的也不是正确性：飞书的重推
//! 只有 4 次，跨进程重启的那条路本来就得靠 §8.5 的请求键（durable 去重 + 同一 Run）兜
//! 底。所以这里保持"收到即入队"，并在 [`super`] 里加一层 `event_id` 去重兜住同进程的
//! 重推。真正的后果只有一条：**从 ack 到 `handle` 之间进程死掉，这条事件就没了**——
//! 飞书认为它已送达，不会再推。

use std::sync::Arc;

use open_lark::Config;
use open_lark::ws_client::{EventDispatcherHandler, LarkWsClient};
use tokio::sync::mpsc;

use komo_kernel::traits::Shutdown;

use super::{RETRY_BASE, RETRY_CAP, race_cancel, sleep_or_cancel};

/// ws 线程的把手。停机靠共享的 [`Shutdown`]，这里只负责等它走完。
pub struct WsThread {
    handle: Option<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for WsThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsThread").finish_non_exhaustive()
    }
}

impl WsThread {
    /// 等 ws 线程结束。`join` 会阻塞，所以搬到 blocking 池上等。
    pub async fn join(mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        let joined = tokio::task::spawn_blocking(move || handle.join()).await;
        match joined {
            Ok(Ok(())) => tracing::debug!("飞书：ws 线程已退出"),
            Ok(Err(_)) => tracing::warn!("飞书：ws 线程 panic 了"),
            Err(error) => tracing::warn!(%error, "飞书：等 ws 线程时出错"),
        }
    }
}

impl Drop for WsThread {
    fn drop(&mut self) {
        // 没被 `join` 掉也不要让线程变成孤儿：`Shutdown` 已经置位，它自己会走完。
        if self.handle.is_some() {
            tracing::debug!("飞书：ws 线程未被显式等待，交给停机标志");
        }
    }
}

/// 起一条 ws 线程，把原始事件负载交出来。
pub fn spawn(
    app_id: String,
    app_secret: String,
    shutdown: Shutdown,
) -> (mpsc::UnboundedReceiver<Vec<u8>>, WsThread) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = std::thread::Builder::new()
        .name("komo-feishu-ws".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "飞书：ws 线程建不起运行时");
                    return;
                }
            };
            runtime.block_on(reconnect_loop(app_id, app_secret, tx, shutdown));
        })
        .expect("起 ws 线程");
    (
        rx,
        WsThread {
            handle: Some(handle),
        },
    )
}

/// 断了就重连，直到停机。
async fn reconnect_loop(
    app_id: String,
    app_secret: String,
    tx: mpsc::UnboundedSender<Vec<u8>>,
    shutdown: Shutdown,
) {
    let mut backoff = RETRY_BASE;
    while !shutdown.is_cancelled() {
        let config = Arc::new(
            Config::builder()
                .app_id(app_id.clone())
                .app_secret(app_secret.clone())
                .build(),
        );
        let handler = EventDispatcherHandler::builder()
            .payload_sender(tx.clone())
            .build();

        tracing::info!("飞书：建立 ws 长连接");
        // 停机时把 future 丢掉就是断开：Session 没有别的收尾动作。
        let Some(outcome) = race_cancel(LarkWsClient::open(config, handler), &shutdown).await
        else {
            break;
        };
        match outcome {
            // openlark 的 `open` 在生产路径上几乎总是 Err——正常断开也是
            // `ConnectionClosed`，所以两个分支都只是"该重连了"。
            Ok(()) => tracing::info!("飞书：ws 会话结束，准备重连"),
            Err(error) => tracing::warn!(%error, "飞书：ws 会话断开，准备重连"),
        }
        if tx.is_closed() {
            // `serve` 已经不收了：再连上去也没人处理。
            tracing::debug!("飞书：事件接收端已关闭，ws 线程退出");
            break;
        }
        if sleep_or_cancel(backoff, &shutdown).await {
            break;
        }
        backoff = (backoff * 2).min(RETRY_CAP);
    }
    tracing::info!("飞书：ws 线程结束");
}

/// 一次连通性核对用的 ws 端点探测是**没有的**：`komo channel probe` 拿 tenant token
/// 就够了（§11.5），而建一次 ws 长连接会真的占用一个连接名额。
#[cfg(test)]
mod tests {
    /// openlark 的两个类型名在这里被引用过，就说明 feature `websocket` 下它们存在。
    /// 真正的 ws 行为只能在真机上验证（本机无飞书凭证），见 `mod.rs` 的模块注释。
    #[test]
    fn the_ws_entry_points_exist() {
        fn _assert_open_is_callable(
            config: std::sync::Arc<open_lark::Config>,
            handler: open_lark::ws_client::EventDispatcherHandler,
        ) -> impl std::future::Future<Output = open_lark::ws_client::WsClientResult<()>> {
            open_lark::ws_client::LarkWsClient::open(config, handler)
        }
    }
}
