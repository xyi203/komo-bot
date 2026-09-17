//! 测试用的假 Gateway：一个手写 HTTP/1.1 的 `tokio::net::TcpListener`。
//!
//! 不引 mock 库——要测的是**线格式**（协议类型序列化出来的 JSON、`text/event-stream` 的
//! 帧、重连时带的 `?from=`），而一个 mock 库只会挡在中间。这个服务端只做三件事：
//! 记下每一个请求、按脚本回一份响应、需要时把连接**断掉**。

#![cfg(test)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Once};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::api::KomoClient;

/// reqwest 的 `rustls-no-provider`：正式构建里 provider 由 `komo` 的 main 安装，测试进程
/// 没有 main，所以在这里装一次。装不上（已经有人装了）就算了。
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 服务端记下的一个请求。
#[derive(Debug, Clone, Default)]
pub struct Recorded {
    pub method: String,
    /// 不含查询串。
    pub path: String,
    /// `?` 之后的部分（不含 `?`）。
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// 查询参数的值。
    pub fn param(&self, name: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then(|| value.to_string())
        })
    }
}

/// 服务端要回什么。
#[derive(Debug, Clone)]
pub enum Reply {
    Json {
        status: u16,
        body: String,
    },
    /// 一段 `text/event-stream`，**写完就断连**——重连路径要的就是这一下。
    Sse(Vec<String>),
    /// 一段 `text/event-stream`，写完**挂着不断**，直到客户端自己走。
    SseOpen(Vec<String>),
    /// 什么都不回，直接断。
    Hangup,
}

impl Reply {
    pub fn ok(body: impl serde::Serialize) -> Reply {
        Reply::Json {
            status: 200,
            body: serde_json::to_string(&body).expect("样本可序列化"),
        }
    }

    pub fn error(status: u16, code: &str, message: &str) -> Reply {
        Reply::Json {
            status,
            body: format!(r#"{{"error":{{"code":"{code}","message":"{message}"}}}}"#),
        }
    }
}

type Handler = Arc<dyn Fn(&Recorded, usize) -> Reply + Send + Sync>;

/// 一个跑着的假 Gateway。
pub struct FakeGateway {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeGateway {
    /// 起一个。`handler` 拿到请求和它是第几个（从 0 数）。
    pub async fn spawn<F>(handler: F) -> FakeGateway
    where
        F: Fn(&Recorded, usize) -> Reply + Send + Sync + 'static,
    {
        install_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("能监听回环");
        let addr = listener.local_addr().expect("有地址");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Handler = Arc::new(handler);

        let task = {
            let requests = requests.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let requests = requests.clone();
                    let handler = handler.clone();
                    tokio::spawn(async move {
                        serve_one(stream, requests, handler).await;
                    });
                }
            })
        };

        FakeGateway {
            addr,
            requests,
            task,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn client(&self) -> KomoClient {
        install_crypto_provider();
        KomoClient::new(&self.base_url(), Some("test-token".into())).expect("地址合法")
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("锁没中毒").clone()
    }
}

async fn serve_one(
    mut stream: tokio::net::TcpStream,
    requests: Arc<Mutex<Vec<Recorded>>>,
    handler: Handler,
) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    // 读到请求头结束。
    let head_end = loop {
        match stream.read(&mut chunk).await {
            Ok(0) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(_) => return,
        }
        if let Some(index) = find(&buffer, b"\r\n\r\n") {
            break index + 4;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target, String::new()),
    };
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();

    let length: usize = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < length {
        match stream.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }

    let recorded = Recorded {
        method,
        path,
        query,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    };
    let ordinal = {
        let mut requests = requests.lock().expect("锁没中毒");
        requests.push(recorded.clone());
        requests.len() - 1
    };

    match handler(&recorded, ordinal) {
        Reply::Json { status, body } => {
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
        Reply::Sse(chunks) => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(head.as_bytes()).await;
            for chunk in chunks {
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                let _ = stream.flush().await;
            }
            // 写完就断——客户端应当自己按最后一个 id 重连。
        }
        Reply::SseOpen(chunks) => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\n";
            let _ = stream.write_all(head.as_bytes()).await;
            for chunk in chunks {
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                let _ = stream.flush().await;
            }
            // 客户端关连接时 read 返回 0；上限兜底，免得测试进程被挂住。
            let mut probe = [0u8; 1];
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(10), stream.read(&mut probe))
                    .await;
        }
        Reply::Hangup => {}
    }
    let _ = stream.shutdown().await;
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
