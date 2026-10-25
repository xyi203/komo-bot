//! MCP（`docs/mcp.md`）：真 Gateway + 一个真的 stdio MCP 服务器（`server.py`）。
//!
//! 钉住的是"MCP 工具走与内置工具同一条路"：模型要的就执行（§7 没有 Policy，声明只读与否
//! 影响恢复时能不能重做），而且只执行一次；子进程只拿到声明过的 `.env` 变量；一个
//! 起不来的服务器不拖垮启动。

use std::path::Path;

use komo_gateway::service::test_support::harness::{DEFAULT_ENV, FakeLlm, Gw, Home, config_toml};
use komo_kernel::types::turn::RoundInput;

fn home_with_demo(extra_servers: &str) -> (Home, std::path::PathBuf) {
    let home = Home::with_config(&config_toml(""));
    let counter = home.path().join("bump.count");
    std::fs::write(
        home.path().join("config.toml"),
        demo_config(&home, extra_servers),
    )
    .unwrap();
    std::fs::write(
        home.path().join(".env"),
        format!("{DEFAULT_ENV}DEMO_TOKEN=s3cret\n"),
    )
    .unwrap();
    (home, counter)
}

/// 配着 `demo` 服务器的整份 config.toml。
fn demo_config(home: &Home, extra_servers: &str) -> String {
    demo_config_with(home, "python3", &[], extra_servers)
}

/// 同上，`demo` 的命令与 `server.py` 之后的参数自己给（第一个是握手前睡几秒）。
fn demo_config_with(
    home: &Home,
    command: &str,
    extra_args: &[&str],
    extra_servers: &str,
) -> String {
    let server = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp/server.py");
    let counter = home.path().join("bump.count");
    let extra_args: String = extra_args
        .iter()
        .map(|arg| format!(", \"{arg}\""))
        .collect();
    config_toml(&format!(
        r#"
[mcp.servers.demo]
command = "{command}"
args = ["{server}", "{counter}"{extra_args}]
env = ["DEMO_TOKEN"]
read_only = ["echo"]
{extra_servers}
"#,
        server = server.display(),
        counter = counter.display(),
    ))
}

fn tool_results(llm: &FakeLlm) -> Vec<String> {
    llm.inputs
        .lock()
        .unwrap()
        .iter()
        .filter_map(|input| match input {
            RoundInput::ToolResults { results, .. } => Some(
                results
                    .iter()
                    .map(|result| result.content.clone())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect()
}

/// 一台要 OAuth 的 HTTP MCP 服务器（`oauth_server.py`）。服务器此刻认哪个 token 写在一个文件里，
/// 改它就是"token 作废"；`calls` 每真的执行一次 `whoami` 记一行。
struct OAuthServer {
    child: std::process::Child,
    port: u16,
    valid: std::path::PathBuf,
    calls: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl OAuthServer {
    fn start(token: &str) -> OAuthServer {
        use std::io::BufRead as _;
        let dir = tempfile::tempdir().unwrap();
        let valid = dir.path().join("valid");
        let calls = dir.path().join("calls");
        std::fs::write(&valid, token).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mcp/oauth_server.py");
        let mut child = std::process::Command::new("python3")
            .arg(script)
            .arg(&valid)
            .arg(&calls)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        OAuthServer {
            child,
            port: line.trim().parse().unwrap(),
            valid,
            calls,
            _dir: dir,
        }
    }

    fn home(&self) -> Home {
        Home::with_config(&config_toml(&format!(
            r#"
[mcp.servers.secure]
url = "http://127.0.0.1:{port}/mcp"
oauth = true
exposure = "direct"
"#,
            port = self.port
        )))
    }

    /// 服务器换了一个 token：手里的那个作废。
    fn rotate(&self, token: &str) {
        std::fs::write(&self.valid, token).unwrap();
    }

    /// `komo auth mcp login secure` 落盘的样子：一份带 access token、没有 refresh token 的凭证。
    fn log_in(&self, home: &Home, token: &str) {
        let path = komo_runtime::mcp_auth::credentials_path(home.path(), "secure");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = serde_json::json!({
            "client_id": "komo-test",
            "token_response": { "access_token": token, "token_type": "Bearer" },
            "granted_scopes": [],
            "token_received_at": null,
        });
        std::fs::write(path, serde_json::to_vec(&body).unwrap()).unwrap();
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }
}

impl Drop for OAuthServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 有没有 `mcp__demo__` 开头的工具。
fn has_demo(names: &[String]) -> bool {
    names.iter().any(|name| name.starts_with("mcp__demo__"))
}

/// 受理 Run 时目录里有什么：启动不等 MCP 握手，先做一遍受理时那一步（等服务器、挂工具）再看。
async fn mounted(gw: &Gw) -> Vec<String> {
    gw.state().refresh_mcp().await;
    gw.state().tool_names()
}

mod durable;
