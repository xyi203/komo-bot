//! 假的 [`PythonHost`]。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::traits::*;

use crate::types::plan::EnvVersion;
use crate::types::refs::ToolResultStatus;
use crate::types::tool::{CancelToken, PyError, PythonJob, PythonResult};

/// 假的 [`PythonHost`]：按 `(module, function)` 或"任意 code"给脚本化结果，并记下每
/// 次调用。
#[derive(Debug, Clone, Default)]
pub struct FakePythonHost {
    state: Arc<Mutex<PyState>>,
}

#[derive(Debug, Default)]
struct PyState {
    responses: std::collections::VecDeque<Result<PythonResult, PyError>>,
    /// 每次调用的 job 与它写进 sink 的 stdout。
    calls: Vec<PythonJob>,
    stdout: Vec<String>,
    env_version: String,
}

impl FakePythonHost {
    pub fn new() -> Self {
        let host = Self::default();
        host.state.lock().expect("假宿主").env_version = "py-test-1".into();
        host
    }

    /// 排一个返回值。
    pub fn push_result(&self, result: PythonResult) {
        self.state
            .lock()
            .expect("假宿主")
            .responses
            .push_back(Ok(result));
    }

    /// 排一个失败。
    pub fn push_error(&self, error: PyError) {
        self.state
            .lock()
            .expect("假宿主")
            .responses
            .push_back(Err(error));
    }

    /// 让下一次调用往 sink 里写这段 stdout。
    pub fn push_stdout(&self, text: impl Into<String>) {
        self.state.lock().expect("假宿主").stdout.push(text.into());
    }

    /// 到目前为止收到过哪些调用。
    pub fn calls(&self) -> Vec<PythonJob> {
        self.state.lock().expect("假宿主").calls.clone()
    }

    pub fn set_env_version(&self, version: impl Into<String>) {
        self.state.lock().expect("假宿主").env_version = version.into();
    }
}

#[async_trait]
impl PythonHost for FakePythonHost {
    async fn run(
        &self,
        job: PythonJob,
        sink: &mut dyn OutputWriter,
        cancel: CancelToken,
    ) -> Result<PythonResult, PyError> {
        if cancel.is_cancelled() {
            return Err(PyError::Cancelled);
        }
        let (response, stdout, env_version) = {
            let mut state = self.state.lock().expect("假宿主");
            state.calls.push(job);
            let response = state.responses.pop_front();
            let stdout = (!state.stdout.is_empty()).then(|| state.stdout.remove(0));
            let env_version = state.env_version.clone();
            (response, stdout, env_version)
        };
        if let Some(text) = stdout {
            sink.write_stdout(text.as_bytes())
                .await
                .map_err(|e| PyError::Protocol(e.to_string()))?;
        }
        response.unwrap_or_else(|| {
            Ok(PythonResult {
                status: ToolResultStatus::Completed,
                result: serde_json::Value::Null,
                error: None,
                artifacts: vec![],
                env_version: EnvVersion(env_version),
            })
        })
    }

    fn env_version(&self) -> EnvVersion {
        EnvVersion(self.state.lock().expect("假宿主").env_version.clone())
    }
}
