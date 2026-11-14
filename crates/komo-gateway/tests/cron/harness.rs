//! 这一组测试要的那一台 Gateway：共用件在
//! `komo_gateway::service::test_support::harness`（真数据目录、真 `service::start`、
//! 脚本化模型），这里只留 Cron 特有的那几个——扫一轮、把槽位拨到过去、读触发
//! 记录、停住一次触发。

#![allow(dead_code, unused_imports)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use komo_kernel::cron::CronJob;
use komo_kernel::types::ids::{CronJobId, RunId};
use komo_kernel::types::turn::{LlmError, Round};

pub use komo_gateway::service::test_support::harness::*;

/// 一条 bash 调用：往计数文件里追加一行。**断言的是次数**（§14 最后一段）。
pub fn shell_append(
    n: u32,
    provider_id: &str,
    counter: &Path,
    what: &str,
) -> Result<Round, LlmError> {
    call_round(
        n,
        provider_id,
        "bash",
        serde_json::json!({
            "command": format!("printf '{what}\\n' >> {}", counter.display())
        }),
    )
}

/// 一次"结果不明"的调用（测试工具 `unsure`）：Run 当场停在 `verify` 上等人核对
/// （§7.5）——审批删掉之后（§7），"一次触发一直没结束"靠的就是它。
pub fn unsure_round(n: u32, provider_id: &str) -> Result<Round, LlmError> {
    call_round(n, provider_id, "unsure", serde_json::json!({}))
}

/// 一次 `gate` 调用（测试工具，门不开就一直等）：Durable 里"一次触发一直没结束"靠它——
/// 新语义没有停在等人核对上的 `waiting`（见 `durable/firing.rs`）。
pub fn gate_round(n: u32, provider_id: &str) -> Result<Round, LlmError> {
    call_round(n, provider_id, "gate", serde_json::json!({}))
}

/// 计数文件里有几行 = 副作用发生了几次。
pub fn lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// 目标清单里带定时的那些，按旧 `GET /v1/cron` 的形状摆：`jobs` 与按 `job` 对齐的运行面。
pub struct CronList {
    pub jobs: Vec<CronJob>,
    pub status: Vec<CronJobStatus>,
}

/// 一个定时的运行面：下一次，上一次。
pub struct CronJobStatus {
    pub job: CronJobId,
    pub next_run_at: Option<time::OffsetDateTime>,
    pub last: Option<komo_kernel::cron::CronFiring>,
}

/// 旧 `POST /v1/cron` 的请求体 → `POST /v1/matters`（没有终点的目标 = 领域，带定时）。
///
/// 定时并进目标之后，「建一个定时任务」就是「建一个带定时的目标」：`name` 是目标标题，其余是
/// [`komo_kernel::protocol::ScheduleSpec`] 的字段。空的 `prompt`（命令任务那时写的空串）不给。
pub fn create_body(old: serde_json::Value) -> serde_json::Value {
    let mut spec = old;
    let name = spec
        .as_object_mut()
        .and_then(|map| map.remove("name"))
        .unwrap_or(serde_json::json!("定时"));
    if spec.get("prompt").and_then(|p| p.as_str()) == Some("") {
        spec.as_object_mut().unwrap().remove("prompt");
    }
    serde_json::json!({ "kind": "area", "title": name, "schedule": spec })
}

/// 旧 `PATCH /v1/cron/{id}` 的请求体 → `schedule` 动词（`status` 是 `enabled`）。
pub fn schedule_body(old: serde_json::Value) -> serde_json::Value {
    let mut spec = old;
    if let Some(status) = spec.as_object_mut().and_then(|map| map.remove("status")) {
        spec["enabled"] = serde_json::json!(status == "active");
    }
    serde_json::json!({ "schedule": spec })
}

/// Cron 特有的那几个动作。挂在共用的 [`Gw`] 上，测试照旧写 `gw.tick()`。
#[async_trait]
pub trait CronOps {
    async fn add_job(&self, body: serde_json::Value) -> CronJob;
    /// 旧 `POST /v1/cron`：成功时正文是那个 `CronJob` 的 JSON。
    async fn post_cron(&self, body: serde_json::Value) -> (u16, String);
    /// 旧 `PATCH /v1/cron/{id}`：成功时正文是改完的 `CronJob` 的 JSON。
    async fn patch_cron(&self, id: &CronJobId, body: serde_json::Value) -> (u16, String);
    /// 旧 `POST /v1/cron/{id}/run`：成功时正文是 `ManualCronRunResponse` 的 JSON。
    async fn run_cron(&self, id: &CronJobId) -> (u16, String);
    async fn cron_list(&self) -> CronList;
    async fn job(&self, id: &CronJobId) -> CronJob;
    async fn firings(&self, id: &CronJobId) -> Vec<komo_kernel::cron::CronFiring>;
    async fn make_due(&self, id: &CronJobId);
    async fn tick(&self) -> komo_runtime::scheduler::CronTick;
}

#[async_trait]
impl CronOps for Gw {
    /// 造一个带定时的目标，`schedule` 按 UTC。
    async fn add_job(&self, body: serde_json::Value) -> CronJob {
        let (status, text) = self.post_cron(body).await;
        assert_eq!(status, 200, "{text}");
        serde_json::from_str(&text).expect("cron job")
    }

    async fn post_cron(&self, body: serde_json::Value) -> (u16, String) {
        let (status, text) = self.post("/v1/matters", create_body(body)).await;
        if status != 200 {
            return (status, text);
        }
        let detail: komo_kernel::protocol::MatterDetail =
            serde_json::from_str(&text).expect("matter detail");
        let job = detail.schedule.expect("带定时").job;
        (200, serde_json::to_string(&job).unwrap())
    }

    async fn patch_cron(&self, id: &CronJobId, body: serde_json::Value) -> (u16, String) {
        let (status, text) = self
            .post(&format!("/v1/matters/{id}/schedule"), schedule_body(body))
            .await;
        if status != 200 {
            return (status, text);
        }
        let detail: komo_kernel::protocol::MatterDetail =
            serde_json::from_str(&text).expect("matter detail");
        (
            200,
            serde_json::to_string(&detail.schedule.expect("带定时").job).unwrap(),
        )
    }

    async fn run_cron(&self, id: &CronJobId) -> (u16, String) {
        let (status, text) = self
            .post(&format!("/v1/matters/{id}/run"), serde_json::json!({}))
            .await;
        if status != 200 {
            return (status, text);
        }
        let detail: komo_kernel::protocol::MatterDetail =
            serde_json::from_str(&text).expect("matter detail");
        (
            200,
            serde_json::to_string(&detail.fired.expect("带 fired")).unwrap(),
        )
    }

    async fn cron_list(&self) -> CronList {
        let (status, text) = self.get("/v1/matters?all=true").await;
        assert_eq!(status, 200, "{text}");
        let list: komo_kernel::protocol::MatterListResponse =
            serde_json::from_str(&text).expect("matter list");
        CronList {
            status: list
                .schedules
                .iter()
                .map(|schedule| CronJobStatus {
                    job: schedule.job.id.clone(),
                    next_run_at: schedule.job.next_run_at,
                    last: schedule.last.clone(),
                })
                .collect(),
            jobs: list.schedules.into_iter().map(|s| s.job).collect(),
        }
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
    /// 走 `advance` 而不是 `put`：**推进槽位不是定义变更**，用 `put` 会让版本 +1（§10）。
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
        tick
    }
}
