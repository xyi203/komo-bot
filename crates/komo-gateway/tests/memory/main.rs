//! W6 端到端验收：docs/komo_bot.md §14「Memory 与模型验收覆盖」那九条，走真 Gateway
//! （真数据目录、真 durable.db / aux.db、真 HTTP、真 MemoryManager），模型与向量端点是替身。
//!
//! 模型替身还是 `ScriptedLlm`：同一个实例既当聊天（经 `DriverStream` 演成 `ModelStream`）又当记忆
//! 模型（`LlmClient`），脚本按调用先后消费；等一轮结束用 `TestGateway::wait_run`；记忆处理进度读
//! conversation 级 document `komo.memory_cursor`（`progress`）；模型收到的"注入了哪些记忆"看请求
//! 消息里的召回块（[`said`]，§9.4：每个 Run 一块，追加在输入之后）与 run 详情。
//!
//! 验收列逐句 → 测试：
//!
//! | 验收列 | 测试 |
//! |---|---|
//! | 同一偏好换一种中文表述仍可语义召回，保留原始来源 | `recall::a_paraphrase_finds_it_over_http` |
//! | 中文两字查询直接命中 bigram | `recall::a_two_character_chinese_query_hits_the_bigram_arm` |
//! | 四种来源在展示与上下文里分得开 | `extraction::the_four_provenances_stay_apart_end_to_end` |
//! | 候选不因反复提取而升级 | `extraction::a_candidate_stays_a_candidate_however_often_it_is_extracted` |
//! | 切换聊天模型不影响记忆模型 | `extraction::the_memory_model_is_used_for_extraction_not_the_chat_model` |
//! | 未设 effort 不发字段 / 不支持的 effort 请求前拒绝 | `effort::*`（`komo-runtime` 的 `llm` / `embedding` 两套单测在各自 crate 里） |
//! | 同维度不同代次的向量不混用 | `index::an_old_generation_is_never_mixed_in`（store 的 `vectors_from_another_generation_are_never_compared` 是它的单元一面） |
//! | 修改记忆时旧 revision 不召回 | `index::editing_a_memory_retires_its_vector` |
//! | 重建中崩溃可接续 | `index::a_rebuild_survives_a_restart` |
//! | 向量不可用时 hybrid 降级 / vector 报不可用 | `recall::a_dead_endpoint_degrades_hybrid_and_fails_vector_only` |
//! | confirm / forget 带 revision 且幂等 | `governance::confirm_and_forget_are_idempotent_over_http` |
//! | resume 与旧检查点不重新注入已遗忘内容 | `governance::a_forgotten_memory_never_comes_back_into_a_turn` |
//! | 提取写库路径全测 | `extraction::*`（脚本化模型给固定 JSON） |
//! | 1 千 / 1 万条的 P95 | `komo-store` 的 `repos::memory::bench`，`#[ignore]` |
//!
//! **失败即缺陷**：与文档不符的地方留成 `#[ignore]` + `// BUG(n):`。

mod harness;

mod belongs;
mod extraction;
mod governance;
mod index;
mod inspect;
mod matters;
mod recall;
mod session_search;
mod startup;

use std::sync::Arc;

use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::types::ids::{MemoryId, RunId, SessionId};
use komo_kernel::types::memory::{
    Confirmation, ExtractionMetadata, MemoryItem, MemoryKind, MemoryScope, MemoryState,
    MemoryUsage, Provenance,
};
use komo_kernel::types::turn::Round;
use time::OffsetDateTime;

use komo_gateway::service::test_support::driver_stream::{DriverStream, core};
use komo_kernel::traits::{EmbeddingClient, LlmClient};
use komo_runtime::memory::durable::{RunCursor, read_cursor_doc};

use harness::*;

/// 模型这一次请求收到的全部消息正文（召回块是其中一条宿主附上的 user 消息，§9.4）。
pub fn said(request: &komo_kernel::types::turn::TurnRequest) -> String {
    request
        .messages
        .iter()
        .filter_map(|message| message.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一段只说话、不调工具的模型回复。
pub fn text_round(round: u32, text: &str) -> Round {
    Round {
        round,
        text: Some(text.into()),
        tool_calls: vec![],
        provider_blocks: None,
        usage: Default::default(),
        truncated: false,
    }
}

/// 直接写进库的一条记忆（测检索与治理时不必先跑一遍提取）。
pub fn seeded(id: &str, content: &str, provenance: Provenance, state: MemoryState) -> MemoryItem {
    scoped_with(id, content, provenance, state, MemoryScope::Personal)
}

/// 同上，但指定作用域（§9.2 的那个过滤器：多 Agent 各自只看自己那一份）。
pub fn scoped(id: &str, content: &str, scope: MemoryScope) -> MemoryItem {
    scoped_with(
        id,
        content,
        Provenance::UserStatement,
        MemoryState::Active,
        scope,
    )
}

fn scoped_with(
    id: &str,
    content: &str,
    provenance: Provenance,
    state: MemoryState,
    scope: MemoryScope,
) -> MemoryItem {
    let now = OffsetDateTime::now_utc();
    MemoryItem {
        id: MemoryId::from_raw(id),
        revision: 1,
        content: content.into(),
        kind: MemoryKind::Preference,
        scope,
        provenance,
        confirmation: Confirmation::Unconfirmed,
        state,
        evidence: vec![],
        observed_at: now,
        valid_until: None,
        created_at: now,
        updated_at: now,
        extraction: ExtractionMetadata::new("memory-test", None, "v1"),
        usage: MemoryUsage::default(),
        supersedes: None,
        superseded_by: None,
        application: None,
        applies_when: None,
        belongs_to: None,
    }
}

/// 带 `[memory]` 三把钥匙的 Durable builder。
pub fn gateway(config: &str) -> GatewayBuilder {
    memory_gateway(config)
}

pub trait Scripted {
    /// 一个 `ScriptedLlm` 同时当聊天模型（`ModelStream`）与记忆模型（`LlmClient`）。
    fn scripted(self, llm: ScriptedLlm) -> Self;
}

impl Scripted for GatewayBuilder {
    fn scripted(self, llm: ScriptedLlm) -> Self {
        let llm = Arc::new(llm);
        self.llm(Arc::clone(&llm) as Arc<dyn LlmClient>)
            .stream(DriverStream::new(llm as Arc<dyn LlmClient>))
    }
}

/// 等这条 run 的输入结算。
pub async fn wait_for_terminal(gateway: &TestGateway, run: &RunId) {
    gateway.wait_run(run).await;
}

/// 起一台带记忆的 Gateway，写几条记忆、建好索引（同 `crate::with_memories`）。
pub async fn with_memories(
    mode: &str,
    items: Vec<komo_kernel::types::memory::MemoryItem>,
) -> (TestGateway, Arc<ConceptEmbeddings>) {
    let embeddings = ConceptEmbeddings::new();
    let gateway = gateway(&memory_config(mode, true))
        .embeddings(Arc::clone(&embeddings) as Arc<dyn EmbeddingClient>)
        .scripted(ScriptedLlm::new(vec![]))
        .start()
        .await;
    for item in items {
        gateway
            .state()
            .memory
            .put(item, None)
            .await
            .expect("写得下");
    }
    gateway
        .state()
        .memories
        .build_index()
        .await
        .expect("索引建得起来");
    (gateway, embeddings)
}

/// 断言用：会话与 Run 的那一对（走 HTTP v2）。
pub async fn a_run(gateway: &TestGateway, key: &str, text: &str) -> (SessionId, RunId) {
    let (status, body) = gateway
        .post_json("/v1/sessions", serde_json::json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    let session: SessionId = serde_json::from_value(body["session"].clone()).expect("会话 id");
    let run = run_in(gateway, &session, key, text).await;
    (session, run)
}

/// 往已有会话再提交一条输入。
pub async fn run_in(gateway: &TestGateway, session: &SessionId, key: &str, text: &str) -> RunId {
    let (status, body) = gateway
        .post_json(
            &format!("/v1/sessions/{session}/runs"),
            serde_json::json!({"request_key": key, "text": text}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_value(body["run"].clone()).expect("run id")
}

/// 一条 run 的记忆处理进度（没有记录 = pending、游标 0）。
pub async fn progress(gateway: &TestGateway, run: &RunId) -> RunCursor {
    let core = core(gateway);
    let id = komo_kernel::durable::legacy_ids::submission_of(run).expect("十进制 RunId");
    let submission = core
        .storage
        .submission(id)
        .await
        .expect("读得出")
        .expect("有 submission");
    read_cursor_doc(&*core.storage, submission.conversation)
        .await
        .runs
        .get(run.as_str())
        .copied()
        .unwrap_or(RunCursor {
            work: komo_kernel::types::memory::MemoryWork::Pending,
            cursor: 0,
        })
}

/// 停机后在**同一个数据目录**上重起一台 Durable。返回新的那台；旧的那台要留着直到测试结束
/// （它的 `Drop` 会删临时数据目录）。
pub async fn restart(
    old: &mut TestGateway,
    config: &str,
    llm: Option<ScriptedLlm>,
    embeddings: Option<Arc<dyn EmbeddingClient>>,
) -> TestGateway {
    old.stop().await;
    let mut builder = gateway(config).at(&old.home);
    if let Some(llm) = llm {
        builder = builder.scripted(llm);
    }
    if let Some(embeddings) = embeddings {
        builder = builder.embeddings(embeddings);
    }
    builder.start().await
}
