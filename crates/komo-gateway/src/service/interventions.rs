//! §7.5 的 Intervention：待处理清单、详情、答复。
//!
//! 新存储里它是**派生视图**：一条 Intervention = 一个 faulted 的终态任务，只有 `abandon` 一个结论
//! （`service::durable::interventions`，`docs/storage-durable-d5.md` W4a）。这里是 HTTP 与聊天渠道共用
//! 的入口，只把 [`DurableCore`](super::durable::DurableCore) 的错误换成 [`GatewayError`]。

use std::sync::Arc;

use komo_kernel::protocol::http::{
    InterventionAnswerResponse, InterventionDetail, InterventionListQuery, InterventionSummary,
    InterventionVerdict,
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

    /// 按句柄取一条详情。
    pub async fn intervention(
        &self,
        handle: &str,
    ) -> Result<Option<InterventionDetail>, GatewayError> {
        self.durable
            .intervention(handle)
            .await
            .map_err(|error| GatewayError::Internal(error.to_string()))
    }

    /// 答复一条 Intervention（§7.5）。**结论与种类不符**是一句明确的失败
    /// （[`GatewayError::InvalidRequest`]，HTTP 那一层映射成 422），**已经答过**则回原结论、
    /// 不报错（§11.3）。`by` 是聊天渠道的答复者；新存储的答复记录不带它。
    pub async fn answer_intervention(
        self: &Arc<Self>,
        handle: &str,
        verdict: InterventionVerdict,
        by: Option<PeerId>,
    ) -> Result<InterventionAnswerResponse, GatewayError> {
        let _ = by;
        self.durable.answer_intervention(handle, verdict).await
    }
}
