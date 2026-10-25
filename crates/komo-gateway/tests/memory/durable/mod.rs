//! `tests/memory` 在 Durable 存储上（`docs/storage-durable-d5.md` W3c）：同样 27 个行为，换成新存储与新
//! harness。测试名与 Legacy 版本（上一层的各文件）一一对应，Legacy 版本原样留到 D7。
//!
//! 与 Legacy 不同的地方（各测试里另有理由）：
//!
//! - 模型替身还是 `ScriptedLlm`：同一个实例既当聊天（经 `DriverStream` 演成 `ModelStream`）又当记忆模型
//!   （`LlmClient`），脚本按调用先后消费，与旧版一样；
//! - 等一轮结束用 `TestGateway::wait_run`（输入 submission 结算），不再读 `runs` 表；
//! - 记忆处理进度（旧 `runs.memory_work` / `memory_cursor`）读 conversation 级 document
//!   `komo.memory_cursor`（`progress`）；
//! - 模型收到的"注入了哪些记忆"（旧 `TurnRequest::memories`）在新 harness 里没有这个字段，改看系统提示
//!   正文与 `komo.memory` document；
//! - "重启"是停机后在同一个数据目录上重起一台 Durable（`restart`），共用件的 `restart_with_*` 只会起
//!   Legacy。

#![allow(dead_code)]

use std::sync::Arc;

use komo_gateway::service::test_support::driver_stream::{DriverStream, core};
use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::traits::{EmbeddingClient, LlmClient};
use komo_kernel::types::ids::{RunId, SessionId};
use komo_runtime::memory::durable::{RunCursor, read_cursor_doc};

use crate::harness::*;

mod extraction;
mod governance;
mod index;
mod inspect;
mod recall;
mod startup;

/// 带 `[memory]` 三把钥匙的 Durable builder。
pub fn gateway(config: &str) -> GatewayBuilder {
    memory_gateway(config).durable()
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
