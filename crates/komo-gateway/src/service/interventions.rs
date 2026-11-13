//! §7.5 的 Intervention：待处理清单、详情、答复。
//!
//! 新存储里它是**派生视图**：一条 Intervention = 一个 faulted 的终态任务，只有 `abandon` 一个结论
//! （`service::durable::interventions`，`docs/storage-durable-d5.md` W4a）。这里是 HTTP 的入口，只把 [`DurableCore`](super::durable::DurableCore) 的错误换成 [`GatewayError`]。

use std::sync::Arc;

use komo_kernel::protocol::http::{
    InterventionAnswerResponse, InterventionListQuery, InterventionSummary, InterventionVerdict,
};
use komo_kernel::traits::GatewayError;
use komo_kernel::types::chat::PeerId;

use super::state::GatewayState;

impl GatewayState {
    /// 待处理清单（§7.5）：faulted 的任务。
    pub async fn interventions(
        &self,
        query: &InterventionListQuery,
    ) -> Result<Vec<InterventionSummary>, GatewayError> {
        self.durable
            .interventions(query)
            .await
            .map_err(|error| GatewayError::Internal(error.to_string()))
    }

    /// 答复一条 Intervention（§7.5）。**结论与种类不符**是一句明确的失败
    /// （[`GatewayError::InvalidRequest`]，HTTP 那一层映射成 422），**已经答过**则回原结论、
    /// 不报错（§11.3）。`by` 是答复者（渠道删除前是渠道 peer），没有 = CLI / 网页（经 HTTP）；确认卡的答复记下它。
    pub async fn answer_intervention(
        self: &Arc<Self>,
        handle: &str,
        verdict: InterventionVerdict,
        by: Option<PeerId>,
    ) -> Result<InterventionAnswerResponse, GatewayError> {
        let by = by.map_or_else(|| "cli".to_owned(), |peer| peer.to_string());
        self.durable.answer_intervention(handle, verdict, by).await
    }
}
