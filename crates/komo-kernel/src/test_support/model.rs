//! 脚本化的 [`LlmClient`] / [`TurnDriver`] 与固定向量的 [`EmbeddingClient`]。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::traits::*;

use crate::types::model::{EmbeddingSpace, InputKind, TokenUsage, Vector};
use crate::types::turn::{LlmError, Round, RoundInput, TurnRequest};

/// 按回合给定回复的 [`TurnDriver`]（§13.5「脚本化 driver」）。
pub struct ScriptedTurnDriver {
    rounds: std::collections::VecDeque<Result<Round, LlmError>>,
    usage: TokenUsage,
    /// 收到过哪些输入——断言"上一轮结果按 call_id 回传了"。
    pub seen: Vec<RoundInput>,
}

impl ScriptedTurnDriver {
    pub fn new(rounds: Vec<Round>) -> Self {
        Self {
            rounds: rounds.into_iter().map(Ok).collect(),
            usage: TokenUsage::default(),
            seen: Vec::new(),
        }
    }

    /// 脚本里插一个失败。
    pub fn with_error(mut self, error: LlmError) -> Self {
        self.rounds.push_back(Err(error));
        self
    }
}

#[async_trait]
impl TurnDriver for ScriptedTurnDriver {
    async fn next(&mut self, input: RoundInput) -> Result<Round, LlmError> {
        self.seen.push(input);
        self.rounds
            .pop_front()
            .unwrap_or(Err(LlmError::Unknown("脚本已经演完了".into())))?
            .pipe_ok(&mut self.usage)
    }

    fn usage(&self) -> TokenUsage {
        self.usage
    }
}

trait PipeOk {
    fn pipe_ok(self, usage: &mut TokenUsage) -> Result<Round, LlmError>;
}

impl PipeOk for Round {
    fn pipe_ok(self, usage: &mut TokenUsage) -> Result<Round, LlmError> {
        usage.input = Some(usage.input.unwrap_or(0) + self.usage.input.unwrap_or(0));
        usage.output = Some(usage.output.unwrap_or(0) + self.usage.output.unwrap_or(0));
        Ok(self)
    }
}

/// 交出一串脚本化 driver 的 [`LlmClient`]。
#[derive(Clone)]
pub struct ScriptedLlm {
    scripts: Arc<Mutex<std::collections::VecDeque<Vec<Round>>>>,
    /// 收到过的请求——断言"每个 Run 固定了自己的模型配置快照"。
    pub requests: Arc<Mutex<Vec<TurnRequest>>>,
}

impl ScriptedLlm {
    /// 每次 `begin_turn` 取一段脚本；用完之后的 turn 一开口就报错。
    pub fn new(scripts: Vec<Vec<Round>>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 只有一段脚本的常用情形。
    pub fn once(rounds: Vec<Round>) -> Self {
        Self::new(vec![rounds])
    }
}

#[async_trait]
impl LlmClient for ScriptedLlm {
    async fn begin_turn(&self, req: TurnRequest) -> Result<Box<dyn TurnDriver>, LlmError> {
        self.requests.lock().expect("脚本模型").push(req);
        let rounds = self
            .scripts
            .lock()
            .expect("脚本模型")
            .pop_front()
            .unwrap_or_default();
        Ok(Box::new(ScriptedTurnDriver::new(rounds)))
    }
}

/// 固定向量的 [`EmbeddingClient`]：向量由文本的哈希确定性地生成，**同一段文本永远是
/// 同一个向量**，所以召回排序可以在无网络下断言。
#[derive(Debug, Clone)]
pub struct FixedEmbeddingClient {
    space: EmbeddingSpace,
    /// 置位后每次 `embed` 都失败——测 hybrid 的降级路径。
    down: Arc<Mutex<bool>>,
}

impl FixedEmbeddingClient {
    pub fn new(dimensions: u32) -> Self {
        Self {
            space: EmbeddingSpace {
                provider: "test".into(),
                endpoint: "memory://test".into(),
                model: "fixed".into(),
                revision: Some("1".into()),
                dimensions,
                preprocessing: "v1".into(),
                document_prefix: String::new(),
                query_prefix: String::new(),
                normalized: true,
                distance: crate::types::model::DistanceRule::Cosine,
                effort: None,
            },
            down: Arc::new(Mutex::new(false)),
        }
    }

    pub fn set_down(&self, down: bool) {
        *self.down.lock().expect("向量替身") = down;
    }
}

#[async_trait]
impl EmbeddingClient for FixedEmbeddingClient {
    fn space(&self) -> &EmbeddingSpace {
        &self.space
    }

    async fn embed(&self, _kind: InputKind, texts: &[String]) -> Result<Vec<Vector>, EmbedError> {
        if *self.down.lock().expect("向量替身") {
            return Err(EmbedError::Unavailable("测试里把端点关掉了".into()));
        }
        let dimensions = self.space.dimensions as usize;
        Ok(texts
            .iter()
            .map(|text| {
                let digest = crate::types::digest::sha256(text.as_bytes());
                let mut values = Vec::with_capacity(dimensions);
                for i in 0..dimensions {
                    values.push((digest[i % 32] as f32 - 127.5) / 127.5);
                }
                // 归一化，和 space 的声明一致。
                let norm: f32 = values.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for value in &mut values {
                        *value /= norm;
                    }
                }
                Vector(values)
            })
            .collect())
    }
}
