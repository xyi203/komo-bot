//! codemode 的沙箱与子进程协议（`docs/codemode.md`）。
//!
//! 这里只管"在沙箱里跑一段 Python、把它的工具调用交出去"。工具调用怎么判、怎么执行在
//! executor（`run_codemode` / `nested_call`）——这里拿到的只是一个回调。
//!
//! **沙箱起不来就没有 codemode**：[`Sandbox::probe`] 生成配置后跑一遍自检（写文件、联网、
//! 起子进程三样都必须被拒），不过就返回错误，调用方不注册这个工具。不退化成裸 Python。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const DRIVER: &str = include_str!("driver.py");

/// 脚本输出（`text()` + console）的上限。
pub const OUTPUT_LIMIT: usize = 1024 * 1024;

/// 一份可用的沙箱：解释器 + 为它生成、自检通过的 Seatbelt 配置。
#[derive(Debug, Clone)]
pub struct Sandbox {
    interpreter: PathBuf,
    profile: String,
}

/// 一段脚本跑完的样子。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptOutcome {
    pub outputs: Vec<String>,
    pub console: String,
    /// 脚本抛出的异常（traceback）或驱动层的失败。`None` = 正常跑完。
    pub error: Option<String>,
}

#[derive(serde::Deserialize)]
struct Paths {
    executable: PathBuf,
    prefix: PathBuf,
    base_prefix: PathBuf,
}

#[derive(serde::Deserialize)]
struct DoneBody {
    #[serde(default)]
    outputs: Vec<String>,
    #[serde(default)]
    console: String,
    error: Option<String>,
}

impl Sandbox {
    /// 探解释器、生成配置、自检。任何一步不过都是 `Err`（说得清哪一步）。
    pub async fn probe(interpreter: &Path) -> Result<Sandbox, String> {
        if !cfg!(target_os = "macos") {
            return Err("这个平台还没有 codemode 沙箱（docs/codemode.md §4：Linux 待做）".into());
        }
        let output = tokio::process::Command::new(interpreter)
            .args([
                "-I",
                "-c",
                "import json,sys;print(json.dumps({'executable':sys.executable,'prefix':sys.prefix,'base_prefix':sys.base_prefix}))",
            ])
            .output()
            .await
            .map_err(|error| format!("{} 起不来：{error}", interpreter.display()))?;
        if !output.status.success() {
            return Err(format!(
                "{} 探测失败：{}",
                interpreter.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let paths: Paths = serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("解释器探测结果读不懂：{error}"))?;
        let sandbox = Sandbox {
            profile: profile(&paths),
            interpreter: paths.executable,
        };
        sandbox.self_test().await?;
        Ok(sandbox)
    }

    /// 写文件、联网、起子进程三样都必须被拒，协议必须通。
    async fn self_test(&self) -> Result<(), String> {
        let probe_file = std::env::temp_dir().join(format!("komo-codemode-{}", std::process::id()));
        let code = format!(
            r#"
import os, socket, subprocess
def blocked(f):
    try:
        f()
        return False
    except Exception:
        return True
text({{
    "write": blocked(lambda: open({path:?}, "w").write("x")),
    "net": blocked(lambda: socket.create_connection(("1.1.1.1", 80), timeout=2)),
    "exec": blocked(lambda: subprocess.run(["/bin/echo", "x"], check=True)),
}})
"#,
            path = probe_file.display().to_string(),
        );
        let outcome = self
            .run(&code, &[], Duration::from_secs(20), |_, _| async {
                Err("自检不调工具".to_string())
            })
            .await;
        let leaked = probe_file.exists();
        let _ = std::fs::remove_file(&probe_file);
        if let Some(error) = outcome.error {
            return Err(format!("沙箱自检没跑通：{error}"));
        }
        let verdict: serde_json::Value = outcome
            .outputs
            .first()
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or("沙箱自检没有输出")?;
        let all_blocked = ["write", "net", "exec"]
            .iter()
            .all(|key| verdict[key] == serde_json::Value::Bool(true));
        if !all_blocked || leaked {
            return Err(format!("沙箱没拦住：{verdict}"));
        }
        Ok(())
    }

    /// 跑一段脚本。`on_call(name, args)` 回答脚本里的每一次工具调用。
    ///
    /// 时限到了杀掉进程组；外层 future 被丢掉（取消）时子进程随之被杀（`kill_on_drop`）。
    pub async fn run<F, Fut>(
        &self,
        code: &str,
        tools: &[String],
        timeout: Duration,
        mut on_call: F,
    ) -> ScriptOutcome
    where
        F: FnMut(String, serde_json::Value) -> Fut,
        Fut: Future<Output = Result<serde_json::Value, String>>,
    {
        let mut child = match tokio::process::Command::new("/usr/bin/sandbox-exec")
            .arg("-p")
            .arg(&self.profile)
            .arg(&self.interpreter)
            .args(["-I", "-c", DRIVER])
            .env_clear()
            .env("LANG", "C.UTF-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(error) => return failed(format!("沙箱进程起不来：{error}")),
        };
        let mut stdin = child.stdin.take().expect("stdin 是 piped");
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout 是 piped")).lines();
        let mut stderr = child.stderr.take().expect("stderr 是 piped");

        let conversation = async {
            let first = serde_json::json!({ "code": code, "tools": tools });
            if let Err(error) = write_line(&mut stdin, &first).await {
                return Err(format!("脚本交不进沙箱：{error}"));
            }
            loop {
                let line = match stdout.next_line().await {
                    Ok(Some(line)) => line,
                    Ok(None) => return Err("沙箱进程提前退出".to_string()),
                    Err(error) => return Err(format!("读不到沙箱输出：{error}")),
                };
                let message: serde_json::Value = serde_json::from_str(&line)
                    .map_err(|error| format!("沙箱输出不是协议：{error}"))?;
                if let Some(done) = message.get("done") {
                    let done: DoneBody = serde_json::from_value(done.clone())
                        .map_err(|error| format!("结束消息读不懂：{error}"))?;
                    return Ok(done);
                }
                let Some(name) = message.get("call").and_then(|name| name.as_str()) else {
                    return Err(format!("沙箱输出不是协议：{line}"));
                };
                let args = message.get("args").cloned().unwrap_or_default();
                let reply = match on_call(name.to_string(), args).await {
                    Ok(value) => serde_json::json!({ "ok": value }),
                    Err(error) => serde_json::json!({ "error": error }),
                };
                write_line(&mut stdin, &reply)
                    .await
                    .map_err(|error| format!("答复交不进沙箱：{error}"))?;
            }
        };

        let result = tokio::time::timeout(timeout, conversation).await;
        let outcome = match result {
            Err(_elapsed) => {
                let _ = child.start_kill();
                failed(format!("脚本超时（{}s），已终止", timeout.as_secs()))
            }
            Ok(Err(error)) => {
                let _ = child.start_kill();
                let mut tail = String::new();
                let _ =
                    tokio::time::timeout(Duration::from_secs(1), stderr.read_to_string(&mut tail))
                        .await;
                let tail = tail.trim();
                failed(if tail.is_empty() {
                    error
                } else {
                    format!("{error}\n{tail}")
                })
            }
            Ok(Ok(done)) => ScriptOutcome {
                outputs: done.outputs,
                console: done.console,
                error: done.error,
            },
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        outcome
    }
}

fn failed(error: String) -> ScriptOutcome {
    ScriptOutcome {
        error: Some(error),
        ..ScriptOutcome::default()
    }
}

async fn write_line(
    stdin: &mut tokio::process::ChildStdin,
    value: &serde_json::Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_string(value).unwrap_or_default();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await
}

/// Seatbelt 配置（`docs/codemode.md` §4）：默认全拒，只放解释器与它的安装目录。
fn profile(paths: &Paths) -> String {
    let quote = |path: &Path| format!("{:?}", path.display().to_string());
    let real =
        std::fs::canonicalize(&paths.executable).unwrap_or_else(|_| paths.executable.clone());
    let prefix = quote(&paths.prefix);
    let base = quote(&paths.base_prefix);
    format!(
        r#"(version 1)
(deny default)
(allow process-exec (literal {exe}) (literal {real}) (subpath {prefix}) (subpath {base}))
(allow process-fork)
(allow file-map-executable)
(allow file-read-metadata)
(allow file-read* (subpath {prefix}) (subpath {base}) (subpath "/System") (subpath "/usr/lib")
                  (subpath "/private/var/db/dyld") (literal "/dev/null") (literal "/dev/urandom")
                  (literal "/"))
(allow sysctl-read)
(allow file-write-data (literal "/dev/null"))
"#,
        exe = quote(&paths.executable),
        real = quote(&real),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python() -> Option<PathBuf> {
        let candidate = PathBuf::from("/usr/bin/python3");
        (cfg!(target_os = "macos") && candidate.exists()).then_some(candidate)
    }

    #[tokio::test]
    async fn the_sandbox_blocks_writes_network_and_exec_and_still_talks() {
        let Some(python) = python() else { return };
        let sandbox = Sandbox::probe(&python).await.expect("macOS 上沙箱该起得来");
        let outcome = sandbox
            .run(
                "r = tools.read(path='a.txt')\ntext(r['text'].upper())\nprint('done')",
                &["read".to_string()],
                Duration::from_secs(20),
                |name, args| async move {
                    assert_eq!(name, "read");
                    assert_eq!(args["path"], "a.txt");
                    Ok(serde_json::json!({ "status": "completed", "text": "hello" }))
                },
            )
            .await;
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.outputs, vec!["HELLO".to_string()]);
        assert_eq!(outcome.console, "done\n");
    }

    #[tokio::test]
    async fn a_refused_call_raises_inside_the_script() {
        let Some(python) = python() else { return };
        let sandbox = Sandbox::probe(&python).await.unwrap();
        let outcome = sandbox
            .run(
                "try:\n    tools.write(path='x')\nexcept ToolError as e:\n    text('拒了：' + str(e))",
                &["write".to_string()],
                Duration::from_secs(20),
                |_, _| async { Err("会改东西".to_string()) },
            )
            .await;
        assert_eq!(outcome.outputs, vec!["拒了：会改东西".to_string()]);
    }

    #[tokio::test]
    async fn a_runaway_script_is_killed_at_the_deadline() {
        let Some(python) = python() else { return };
        let sandbox = Sandbox::probe(&python).await.unwrap();
        let started = std::time::Instant::now();
        let outcome = sandbox
            .run(
                "while True:\n    pass",
                &[],
                Duration::from_secs(1),
                |_, _| async { Err(String::new()) },
            )
            .await;
        assert!(outcome.error.unwrap().contains("超时"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn an_exception_keeps_the_partial_output() {
        let Some(python) = python() else { return };
        let sandbox = Sandbox::probe(&python).await.unwrap();
        let outcome = sandbox
            .run(
                "text('前半')\nraise ValueError('坏了')",
                &[],
                Duration::from_secs(20),
                |_, _| async { Err(String::new()) },
            )
            .await;
        assert_eq!(outcome.outputs, vec!["前半".to_string()]);
        assert!(outcome.error.unwrap().contains("ValueError: 坏了"));
    }
}
