//! `komo` 二进制冒烟：真进程、真监听、真发现文件（§3 的启动顺序、§13.1 的认证）。
//!
//! **默认 `#[ignore]`**，因为 `cargo test -p komo-gateway` 不会去编 `komo` 这个 bin。
//! 跑法：
//!
//! ```text
//! cargo build -p komo
//! cargo test -p komo-gateway --test chat -- --ignored smoke
//! ```

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// `target/debug/komo`。
fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/komo")
        .canonicalize()
        .expect("先 `cargo build -p komo`")
}

/// 一个临时数据目录：config.toml + .env + policy.toml，监听端口交给内核挑。
fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("临时数据目录");
    std::fs::write(
        home.path().join("config.toml"),
        r#"
[gateway]
listen = "127.0.0.1:0"

[model]
provider = "openai_responses"
base_url = "https://llm.example.com/v1"
model = "gpt-test"
api_key_env = "KOMO_LLM_API_KEY"

[memory]
enabled = false

[channels.telegram]
enabled = false
"#,
    )
    .expect("写 config.toml");
    std::fs::write(home.path().join(".env"), "KOMO_LLM_API_KEY=test-key\n").expect("写 .env");
    std::fs::write(home.path().join("policy.toml"), "rules = []\n").expect("写 policy.toml");
    home
}

/// 起一个前台 Gateway，等发现文件出现。
struct Foreground {
    child: Child,
    base_url: String,
    token: String,
}

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(home: &std::path::Path) -> Foreground {
    let child = Command::new(binary())
        .args(["gateway", "--foreground"])
        .env("KOMO_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("起得来");
    // **先建出这个句柄**：它的 `Drop` 负责 kill + wait，所以下面任何一条 panic 都不会
    // 留下一个还在跑的 Gateway（进而占着数据目录的锁）。
    let mut gateway = Foreground {
        child,
        base_url: String::new(),
        token: String::new(),
    };

    let discovery = home.join("runtime/gateway.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(&discovery)
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            && let (Some(base_url), Some(token)) =
                (value["base_url"].as_str(), value["token"].as_str())
        {
            gateway.base_url = base_url.to_string();
            gateway.token = token.to_string();
            return gateway;
        }
        assert!(Instant::now() < deadline, "30 秒内没写出发现文件");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `/healthz` 不要认证；别的一律 Bearer；`komo session list` 连得上同一台。
#[tokio::test]
#[ignore = "要先 `cargo build -p komo`；起的是一个真进程"]
async fn the_binary_serves_health_refuses_anonymous_and_answers_session_list() {
    crate::harness::install_crypto();
    let home = home();
    let gateway = start(home.path());

    let health = reqwest::get(format!("{}/healthz", gateway.base_url))
        .await
        .expect("健康检查不要认证");
    assert_eq!(health.status(), 200);

    let refused = reqwest::get(format!("{}/v1/sessions", gateway.base_url))
        .await
        .expect("发得出去");
    assert_eq!(refused.status(), 401, "除 /healthz 外统一 Bearer");

    let allowed = reqwest::Client::new()
        .get(format!("{}/v1/sessions", gateway.base_url))
        .bearer_auth(&gateway.token)
        .send()
        .await
        .expect("发得出去");
    assert_eq!(allowed.status(), 200);

    // CLI 走发现文件找到同一台实例。
    let listed = Command::new(binary())
        .args(["session", "list"])
        .env("KOMO_HOME", home.path())
        .output()
        .expect("跑得起来");
    assert!(
        listed.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&listed.stdout),
        String::from_utf8_lossy(&listed.stderr)
    );

    // 第二台 Gateway 拿不到同一个数据目录的锁（§3）。
    let second = Command::new(binary())
        .args(["gateway", "--foreground"])
        .env("KOMO_HOME", home.path())
        .output()
        .expect("跑得起来");
    assert!(!second.status.success(), "第二台不该起得来");
}
