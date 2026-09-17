//! 这一组测试要的那一台 Gateway：共用件在
//! `komo_gateway::service::test_support::harness`（真数据目录、真 `service::start`、内存
//! 发送口、脚本化模型），这里只留 Cron 特有的那几个——扫一轮、把槽位拨到过去、读触发
//! 记录、在聊天里答审批。
//!
//! 数据目录的默认配置（[`Home`]）里 Telegram 是**配了 home chat 的**：无人值守的审批
//! 请求只有这一个出口（§11.4）。

#![allow(dead_code, unused_imports)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use komo_kernel::cron::CronJob;
use komo_kernel::traits::Inbound;
use komo_kernel::types::chat::ChannelPlatform;
use komo_kernel::types::ids::{CronJobId, RunId};
use komo_kernel::types::turn::{LlmError, Round};

pub use komo_gateway::service::test_support::harness::*;

/// 一条 shell 调用：往计数文件里追加一行。**断言的是次数**（§14 最后一段）。
pub fn shell_append(
    n: u32,
    provider_id: &str,
    counter: &Path,
    what: &str,
) -> Result<Round, LlmError> {
    call_round(
        n,
        provider_id,
        "shell",
        serde_json::json!({
            "command": format!("printf '{what}\\n' >> {}", counter.display())
        }),
    )
}

/// 计数文件里有几行 = 副作用发生了几次。
pub fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Cron 特有的那几个动作。挂在共用的 [`Gw`] 上，测试照旧写 `gw.tick()`。
#[async_trait]
pub trait CronOps {
    async fn add_job(&self, body: serde_json::Value) -> CronJob;
    async fn cron_list(&self) -> komo_kernel::protocol::http::CronListResponse;
    async fn job(&self, id: &CronJobId) -> CronJob;
    async fn firings(&self, id: &CronJobId) -> Vec<komo_kernel::cron::CronFiring>;
    async fn make_due(&self, id: &CronJobId);
    async fn tick(&self) -> komo_runtime::scheduler::CronTick;
    async fn approve_in_chat(&self, short_id: &str, scope: &str) -> String;
}

#[async_trait]
impl CronOps for Gw {
    /// 造一个 Job，`schedule` 按 UTC。
    async fn add_job(&self, body: serde_json::Value) -> CronJob {
        let (status, text) = self.post("/v1/cron", body).await;
        assert_eq!(status, 200, "{text}");
        serde_json::from_str(&text).expect("cron job")
    }

    async fn cron_list(&self) -> komo_kernel::protocol::http::CronListResponse {
        let (status, text) = self.get("/v1/cron").await;
        assert_eq!(status, 200, "{text}");
        serde_json::from_str(&text).expect("cron list")
    }

    async fn job(&self, id: &CronJobId) -> CronJob {
        self.cron_list()
            .await
            .jobs
            .into_iter()
            .find(|job| &job.id == id)
            .expect("这个 Job 还在")
    }

    async fn firings(&self, id: &CronJobId) -> Vec<komo_kernel::cron::CronFiring> {
        self.state().cron.firings(id, 50).await.expect("读得出")
    }

    /// 把一个 Job 的槽位拨到过去，再扫一轮——"到点了"在测试里就是这个意思。
    ///
    /// 走 `advance` 而不是 `put`：**推进槽位不是定义变更**，用 `put` 会让版本 +1，
    /// 把「授权按版本绑」那几条测试的前提悄悄改掉（§10）。
    async fn make_due(&self, id: &CronJobId) {
        let job = self.job(id).await;
        self.state()
            .cron
            .advance(
                id,
                Some(time::OffsetDateTime::now_utc() - time::Duration::seconds(1)),
                job.status,
                None,
            )
            .await
            .expect("拨得动");
    }

    /// 扫一轮 Cron，并**像生产那样**给投出去的每一次触发挂上盯梢
    /// （`service::spawn_background` 里那两行）。
    async fn tick(&self) -> komo_runtime::scheduler::CronTick {
        let tick = self.state().cron_scheduler().tick().await.expect("扫得动");
        self.state().watch_fired(&tick.fired).await;
        self.state().waker().wake();
        tick
    }

    /// 在聊天里答一条审批（`/approve <short_id> [run|cron]` 那条路，§11.3）。
    async fn approve_in_chat(&self, short_id: &str, scope: &str) -> String {
        let text = if scope.is_empty() {
            format!("/approve {short_id}")
        } else {
            format!("/approve {short_id} {scope}")
        };
        let ack = self
            .dispatcher()
            .handle(inbound(
                ChannelPlatform::Telegram,
                "111",
                "111",
                &text,
                &format!("telegram:{}", uuid_like()),
                true,
            ))
            .await
            .expect("Dispatcher 处理");
        format!("{ack:?}")
    }
}

fn uuid_like() -> String {
    static N: AtomicUsize = AtomicUsize::new(1);
    format!("k{}", N.fetch_add(1, Ordering::SeqCst))
}
