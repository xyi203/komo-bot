//! Bidirectional, length-prefixed JSON. Responses have a separate path from requests,
//! so a platform callback waiting for an inbound ack cannot block a reply send.
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, oneshot},
};

pub const MAX_FRAME: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(60);
#[derive(Debug, Serialize, Deserialize)]
pub struct Frame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default)]
    pub payload: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
struct Inner {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>,
    pending: Pending,
    next: AtomicU64,
    reader: tokio::task::AbortHandle,
    closed: Arc<AtomicBool>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
#[derive(Clone)]
pub struct Rpc(Arc<Inner>);

struct CallGuard {
    rpc: Rpc,
    id: u64,
    completed: bool,
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        self.rpc
            .0
            .pending
            .lock()
            .expect("RPC pending")
            .remove(&self.id);
        // Cancellation can interrupt a frame write; never reuse that byte stream.
        if !self.completed {
            self.rpc.close();
        }
    }
}

impl Rpc {
    pub fn connect<R, W>(mut reader: R, writer: W) -> (Self, mpsc::Receiver<Frame>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let waiting = pending.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let reader_closed = closed.clone();
        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(async move {
            loop {
                let Ok(size) = reader.read_u32().await else {
                    break;
                };
                if size as usize > MAX_FRAME {
                    break;
                }
                let mut data = vec![0; size as usize];
                if reader.read_exact(&mut data).await.is_err() {
                    break;
                }
                let Ok(frame) = serde_json::from_slice::<Frame>(&data) else {
                    break;
                };
                if frame.method.is_some() {
                    if tx.try_send(frame).is_err() {
                        break;
                    }
                } else if let Some(reply) = waiting.lock().expect("RPC pending").remove(&frame.id) {
                    let _ = reply.send(match frame.error {
                        Some(e) => Err(e),
                        None => Ok(frame.payload),
                    });
                }
            }
            reader_closed.store(true, Ordering::Release);
            for (_, reply) in waiting.lock().expect("RPC pending").drain() {
                let _ = reply.send(Err("渠道插件连接已关闭".into()));
            }
        });
        (
            Self(Arc::new(Inner {
                writer: tokio::sync::Mutex::new(Box::new(writer)),
                pending,
                next: AtomicU64::new(1),
                reader: task.abort_handle(),
                closed,
            })),
            rx,
        )
    }
    pub fn close(&self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.reader.abort();
        for (_, reply) in self.0.pending.lock().expect("RPC pending").drain() {
            let _ = reply.send(Err("渠道插件连接已关闭".into()));
        }
    }
    async fn write(&self, frame: &Frame) -> Result<(), String> {
        let bytes = serde_json::to_vec(frame).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_FRAME {
            return Err("渠道 RPC 帧超过 1 MiB".into());
        }
        let mut out = self.0.writer.lock().await;
        out.write_u32(bytes.len() as u32)
            .await
            .map_err(|e| e.to_string())?;
        out.write_all(&bytes).await.map_err(|e| e.to_string())?;
        out.flush().await.map_err(|e| e.to_string())
    }
    pub async fn call<T: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &T,
    ) -> Result<R, String> {
        if self.0.closed.load(Ordering::Acquire) {
            return Err("渠道插件连接已关闭".into());
        }
        let id = self.0.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        let frame = Frame {
            id,
            method: Some(method.into()),
            payload: serde_json::to_value(params).map_err(|e| e.to_string())?,
            error: None,
        };
        {
            let mut pending = self.0.pending.lock().expect("RPC pending");
            if pending.len() >= 64 {
                return Err("渠道 RPC 在途请求已满".into());
            }
            pending.insert(id, tx);
        }
        let mut guard = CallGuard {
            rpc: self.clone(),
            id,
            completed: false,
        };
        let result = tokio::time::timeout(TIMEOUT, async {
            self.write(&frame).await.map_err(|error| {
                self.close();
                error
            })?;
            rx.await.map_err(|_| "渠道插件未返回回执".to_string())?
        })
        .await
        .map_err(|_| format!("渠道 RPC {method} 超时"));
        guard.completed = result.is_ok();
        let value = result??;
        serde_json::from_value(value).map_err(|e| e.to_string())
    }
    pub async fn reply(&self, id: u64, value: Result<Value, String>) -> Result<(), String> {
        let (payload, error) = match value {
            Ok(v) => (v, None),
            Err(e) => (Value::Null, Some(e)),
        };
        self.write(&Frame {
            id,
            method: None,
            payload,
            error,
        })
        .await
    }
}
