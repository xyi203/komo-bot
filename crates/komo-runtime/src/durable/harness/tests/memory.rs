//! 输入 Run 身份与产物命名空间分开；真实 bash → CLI 请求身份 → 记忆证据接缝。

use std::sync::Arc;

use komo_kernel::protocol::config::MemoryConfig;
use komo_kernel::protocol::http::MemoryAddRequest;
use komo_kernel::test_support::ScriptedLlm;
use komo_kernel::traits::Clock;
use komo_kernel::types::agent::MemoryMode;
use komo_kernel::types::ids::{RunId, SessionId};
use komo_kernel::types::memory::{Confirmation, MemoryKind, MemoryScope};
use komo_store::{Db, TursoMemoryRepo};
use serde_json::json;
use time::OffsetDateTime;

use super::super::SubmissionDraft;
use super::support::*;
use crate::codemode::Worker;
use crate::durable::tx::ConversationOwnership;
use crate::memory::durable::{DurableMemoryWork, DurableSessionEvents};
use crate::memory::{MemoryManager, MemoryParts};
use crate::tools::{bash::BashTool, codemode::CodemodeTool, write::WriteTool};

struct WallClock;
impl Clock for WallClock {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }
}

/// 同一个 conversation 连跑三个 Run，覆盖普通 bash、脚本子 bash 和 then_run。
/// 用实际子进程的环境组成 MemoryAddRequest，完整走用户原话证据查询与写库。
#[tokio::test]
async fn bash_input_run_finds_user_evidence_without_changing_output_paths() {
    let dir = tempfile::tempdir().unwrap();
    let world = World::new();
    world.hooks.set_cwd(dir.path().to_owned());
    world.hooks.set_tools(vec![
        Arc::new(BashTool::new()),
        Arc::new(CodemodeTool::new()),
        Arc::new(WriteTool::new()),
    ]);
    world
        .harness
        .deps()
        .set_codemode(Some(Worker::in_process()));
    let engine = Arc::new(world.open().await);
    let conversation = engine
        .commit(|tx| {
            Box::pin(async move {
                Ok(tx
                    .create_conversation_in("s1", ConversationOwnership::Ownerless)
                    .await?
                    .id)
            })
        })
        .await
        .unwrap();
    let repo = Arc::new(TursoMemoryRepo::new(
        Db::connect(dir.path().join("aux.db")).await.unwrap(),
    ));
    let clock: Arc<dyn Clock> = Arc::new(WallClock);
    let memory = MemoryManager::new(MemoryParts {
        config: MemoryConfig {
            enabled: true,
            model: model_config(),
            embedding: None,
            retrieval: Default::default(),
            consolidate: Default::default(),
            extract_idle_secs: 600,
            max_observations: 8,
        },
        repo: repo.clone(),
        catalog: repo,
        embeddings: None,
        reranker: None,
        llm: Arc::new(ScriptedLlm::new(vec![])),
        events: Arc::new(DurableSessionEvents::new(world.storage_handle())),
        work: Arc::new(DurableMemoryWork::new(
            world.storage_handle(),
            engine.clone(),
            clock.clone(),
            Arc::new(|_| (MemoryMode::Shared, "test".into())),
        )),
        clock,
        render: |_, _| Default::default(),
    });
    let command = "printf '%s\\n%s\\n' \"$KOMO_SESSION\" \"$KOMO_RUN\" > identity";
    for kind in ["bash", "codemode", "then_run"] {
        let (tool, arguments) = match kind {
            "bash" => ("bash", json!({"command": command})),
            "codemode" => (
                "codemode",
                json!({"code": format!("r = tools.bash(command={})\ntext(r)", serde_json::to_string(command).unwrap())}),
            ),
            _ => (
                "write",
                json!({"path": "note", "content": "saved", "then_run": {"command": command}}),
            ),
        };
        world.script.calls(&[(kind, tool, arguments)]);
        world.script.reply("ok");
        let content = format!("发布工单 Platform 以后都选 saas（{kind}）");
        let submission = world
            .harness
            .submit(&engine, conversation, SubmissionDraft::input(&content))
            .await
            .unwrap();
        world.harness.wait(&engine, submission).await.unwrap();
        let identity = std::fs::read_to_string(dir.path().join("identity")).unwrap();
        let mut lines = identity.lines();
        let session = SessionId::from_raw(lines.next().unwrap());
        let run = RunId::from_raw(lines.next().unwrap());
        assert_eq!(session.as_str(), "s1");
        assert_eq!(run.as_str(), submission.to_string());
        assert_ne!(run.as_str(), conversation.to_string());
        let request = MemoryAddRequest {
            content,
            kind: MemoryKind::Preference,
            scope: MemoryScope::Personal,
            session: Some(session),
            run: Some(run.clone()),
            request_key: None,
        };
        let (item, created) = memory.add(&request).await.unwrap();
        assert!(created);
        assert_eq!(item.confirmation, Confirmation::Unconfirmed);
        assert!(!item.evidence.is_empty());
        assert!(
            item.evidence
                .iter()
                .all(|evidence| evidence.extracted_from_run.as_ref() == Some(&run))
        );
        let mut wrong = request;
        wrong.run = Some(RunId::from_raw(conversation.to_string()));
        assert!(
            memory.add(&wrong).await.is_err(),
            "conversation id cannot masquerade as a Run"
        );
    }
    for task in world.tasks("komo.tool").await {
        let path = format!("tool-output/{conversation}/{}/1/output.json", task.id);
        assert!(world.mem.published_body(&path).is_some(), "{path}");
    }
    drop(memory);
    Arc::try_unwrap(engine).ok().unwrap().close().await.unwrap();
}

/// 插话已经并入后再调用工具，也必须保留宿主 Run 的首个输入，而不是取最新输入。
#[tokio::test(start_paused = true)]
async fn a_tool_after_a_steer_keeps_the_hosts_input_run() {
    let world = World::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    let slow = Arc::new(TestTool::writer("slow", &world.log).with_gate(&gate));
    let read = Arc::new(TestTool::reader("read", &world.log));
    world.tools(vec![slow, read.clone()]);
    world.script.calls(&[("before", "slow", json!({}))]);
    world.script.calls(&[("after", "read", json!({}))]);
    world.script.reply("ok");
    let engine = world.open().await;
    let first = world.submit(&engine, "first statement").await;
    world.until_log("start slow #1").await;
    let steer = world
        .harness
        .submit(
            &engine,
            CONV,
            SubmissionDraft::input("newer statement").steer(),
        )
        .await
        .unwrap();
    gate.notify_one();
    world.harness.wait(&engine, first).await.unwrap();
    assert_ne!(first, steer);
    let runs = read.input_runs.lock().unwrap().clone();
    assert_eq!(
        runs,
        vec![Some(komo_kernel::durable::legacy_ids::run_id(first))]
    );
    engine.close().await.unwrap();
}
