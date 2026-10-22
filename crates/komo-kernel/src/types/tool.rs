//! 工具的定义、上下文与结果（§4）。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use super::ids::{AttemptId, RunId, SessionId, ToolCallId};
use super::plan::PlanSource;
use super::refs::{ContentRef, ToolResultStatus};
use super::resource::ResourceMounts;
use super::status::ToolCallState;

/// 交给模型的工具 Schema。六个基础工具的这份定义是固定的（§4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema。
    pub parameters: serde_json::Value,
}

/// 模型怎么够到一个工具（`docs/mcp.md` §3，照 pi 的 exposure）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExposure {
    /// schema 直接交给模型。
    Direct,
    /// 不声明给模型，只在 codemode 脚本里调；脚本用 `search_tools` / `describe_tool` 找它。
    Codemode,
}

/// 一组工具（一台 MCP 服务器）的说明，给系统提示的服务器段与脚本的 `describe_namespace`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolNamespace {
    /// 脚本里的写法，`mcp__<server>`，`-` 换成 `_`。
    pub name: String,
    /// 服务器原名（`[mcp.servers.<name>]`）；`describe_namespace` 也认它。
    pub server: String,
    /// 操作者在配置里写的一句话。
    pub description: Option<String>,
    /// 服务器握手时交回的 instructions（外部文本，只作说明，不进系统指令）。
    pub instructions: Option<String>,
}

impl ToolNamespace {
    /// 系统提示里那一行：配置的说明，没有就是 instructions 的第一行（pi 的 `serverSummary`）。
    pub fn summary(&self) -> String {
        let text = self
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .or(self.instructions.as_deref())
            .unwrap_or_default();
        text.lines().next().unwrap_or_default().trim().to_string()
    }
}

/// 工具执行的结果正文（还没有发布成 `output.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub status: ToolResultStatus,
    #[serde(default)]
    pub result: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ContentRef>,
    /// 要交给模型看的图片。只在内存里走到执行器：发布时写成这次尝试的输出文件，结果里只留
    /// [`ImageRef`](super::refs::ImageRef)——不序列化，base64 不进任何一份账。
    #[serde(skip)]
    pub images: Vec<ToolImage>,
    /// 给模型看的简短预览；完整正文在 `output.json` 里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// 工具交出来的一张图片（原始字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolImage {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// 一张图片最多多大才交给模型。Anthropic 单张上限 5 MB（按 base64 计），3.5 MiB 的原图编码后
/// 约 4.7 MiB。不引入 `image`（§13.4），没法缩图：超了就报错，让模型自己想办法（比如调 `sips`）。
pub const IMAGE_LIMIT_BYTES: usize = 3584 * 1024;

/// 按文件头认图片格式。只认三家接口都收的四种：PNG、JPEG、GIF、WebP。
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// 图片文件的扩展名。
pub fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// 工具失败。普通失败作为结果交给模型修正；只有驱动 / LLM 错误中断整轮（§6）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolError {
    /// 参数不合法，模型可以改了再来。
    #[error("参数不合法：{message}")]
    InvalidArguments { message: String },
    /// 文件版本与预期不符（§4：覆盖现有文件需检查版本）。
    #[error("版本冲突：{path}")]
    VersionConflict { path: String },
    /// `edit` 的匹配内容没找到——不做模糊猜测。
    #[error("没有找到要替换的内容：{path}")]
    NoMatch { path: String },
    /// 被拒绝：`write` / `edit` 要写 komo 自己的状态（§7、§8.10），或 codemode 脚本里调
    /// 了不支持的本地写入或编排工具。作为明确结果交回模型。旧事件里的 Policy 拒绝也是它。
    #[error("被拒绝：{reason}")]
    Denied { reason: String },
    /// 超过活动执行时限。
    #[error("超时：{after_secs}s")]
    Timeout { after_secs: u64 },
    /// 用户取消。
    #[error("已取消")]
    Cancelled,
    /// 远端写入结果不明：停止自动重试，转为结果核对（§6、§8.6）。
    #[error("结果不明：{message}")]
    Uncertain { message: String },
    /// 其他执行失败。
    #[error("{message}")]
    Failed { message: String },
}

impl ToolError {
    /// 结果不明的失败不能被当成失败重试掉。
    pub fn is_uncertain(&self) -> bool {
        matches!(self, ToolError::Uncertain { .. })
    }
}

/// 取消信号。
///
/// kernel 不依赖 tokio（§13.4），所以这里只有一个 `AtomicBool`：**查询**取消状态在
/// 哪一层都能用，**等待**取消发生是 runtime 的事（它把这个 flag 和自己的 notify
/// 包在一起）。工具在长循环里查询它，子进程的回收由 runtime 负责。
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// 用户插话了：前台还在跑的长命令别再挡着他（`docs/monitor.md` §8）。
///
/// Gateway 在受理 TUI 插话时举起它，`bash` 看到就把前台命令提前转后台。一次插话只换一次
/// 转后台（[`Self::take`]）——插话没能并进宿主时，旗子也不会让这一段里后面的每条命令都转。
/// 每一段装配时换一个新的，所以并进去以后自然落下。
#[derive(Debug, Clone, Default)]
pub struct InterjectSignal {
    flag: Arc<AtomicBool>,
}

impl InterjectSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn raise(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_raised(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 举着就放下并返回 `true`。
    pub fn take(&self) -> bool {
        self.flag.swap(false, Ordering::SeqCst)
    }
}

/// 工具执行的上下文。**由 Gateway 创建，不能由模型参数提升**（§4）。
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub session: SessionId,
    pub run: RunId,
    pub call: ToolCallId,
    pub attempt: AttemptId,
    pub source: PlanSource,
    /// 本次执行的工作目录。
    pub cwd: PathBuf,
    /// 已知的根目录：workspace、artifacts、skills（只读）等。
    pub roots: Vec<WorkspaceRoot>,
    /// 资源命名空间的挂载点（§六）：`skill://` 的根、这个会话的内容目录、这次的能力面。
    ///
    /// 它是**纯数据**，解析（含 I/O）在运行时；工具拿它决定一个资源 URI 指向哪里。
    pub mounts: ResourceMounts,
    /// 这次 `execute` 是不是一次**恢复执行**（§8.4 第 6 / 7 行）。`None` = 首次。
    pub resumed: Option<ResumedCall>,
    pub cancel: CancelToken,
    /// 活动执行时限（`[execution] call_timeout_secs`）。自带超时的工具把自己的超时收紧到
    /// 它以内：执行器的兜底只在工具不守时限时才触发，而那一刻只能报"结果不明"。
    pub call_timeout: std::time::Duration,
    /// 跑过这么久还没完就把这次调用转到后台（`[execution] detach_after_secs`）。`None` =
    /// 不转：任务会话、Cron 的 Run、子 Run、codemode 里的内层调用，或配置为 0。
    pub detach_after: Option<std::time::Duration>,
    /// 用户插话了：给了 `detach_after` 的 `bash` 不等阈值，提前转后台。
    pub interjected: InterjectSignal,
    /// 这是对话会话里直接受理的 Run（不是任务会话、Cron、子 Run）。`bash` 在这里拒绝
    /// 10 秒以上的 `sleep`——等状态是 `monitor` 的事。
    pub conversation: bool,
}

/// 这次执行是在接一次没有收尾的调用（§8.4、§8.6）。
///
/// **它在 `ToolContext` 上，不在 `ExecutionPlan` 里**，理由是硬的：账本按计划哈希核对
/// `tool.planned` 与 `tool.started`，而恢复执行用的必须是**同一份**计划——把"这是第二
/// 次"写进计划，哈希就变了，同一个调用前后对不上。计划回答"这个动作是什么、重启后能不能重跑"
/// （[`RecoveryMode`](super::plan::RecoveryMode)），上下文回答"这一次是第几次"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumedCall {
    /// 上一次尝试。计划已落盘但 `tool.started` 从未写过时为 `None`——那种情形下
    /// 一次尝试都还没有过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_attempt: Option<AttemptId>,
    /// 上一次这个调用停在哪个状态。`Planned` = §8.4 第 6 行（**确定尚未执行**）；
    /// `Started` / `Uncertain` = 第 7 行（**先核对外部效果**）。
    pub previous_state: ToolCallState,
    /// 这个调用之前已经尝试过几次。重复尝试计入原任务预算（§8.6），所以它是个数字而
    /// 不是一个布尔。
    #[serde(default)]
    pub attempts_so_far: u32,
    // 2026-10-13 之前这里还有 `verification`（核对梯子的结论）。旧线格式带着它也解得出来。
}

impl ResumedCall {
    /// 确定这次动作还没发生过——§8.4 第 6 行（planned 而没有 started）。
    pub fn is_known_not_to_have_run(&self) -> bool {
        self.previous_state == ToolCallState::Planned
    }
}

impl ToolContext {
    /// 这个路径落在哪个已知的根里。落不进任何一个 → None（工具按需决定怎么说，例如
    /// 标成"根外"）。
    pub fn root_for(&self, path: &std::path::Path) -> Option<&WorkspaceRoot> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(&root.path))
            .max_by_key(|root| root.path.as_os_str().len())
    }
}

/// 一个已知的根目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRoot {
    pub path: PathBuf,
    pub writable: bool,
    /// 这个根是干什么的，只为界面可读。
    pub label: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_recognised_by_their_magic_bytes() {
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_mime(b"GIF89a.."), Some("image/gif"));
        assert_eq!(image_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_mime(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert_eq!(image_mime(b"hello"), None);
        assert_eq!(image_extension("image/jpeg"), "jpg");
    }

    #[test]
    fn a_cancel_token_is_shared_by_its_clones() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn an_interjection_is_taken_once() {
        let signal = InterjectSignal::new();
        let clone = signal.clone();
        assert!(!clone.take());
        signal.raise();
        assert!(clone.is_raised());
        assert!(clone.take());
        assert!(!signal.is_raised());
        assert!(!signal.take());
    }

    #[test]
    fn the_longest_matching_root_wins() {
        let ctx = ToolContext {
            session: SessionId::from_raw("s"),
            run: RunId::from_raw("r"),
            call: ToolCallId::from_raw("c"),
            attempt: AttemptId::from_raw("a"),
            source: PlanSource::Interactive {
                session: SessionId::from_raw("s"),
            },
            cwd: PathBuf::from("/home/u/ws"),
            call_timeout: std::time::Duration::from_secs(60),
            detach_after: None,
            interjected: InterjectSignal::new(),
            conversation: false,
            mounts: ResourceMounts::default(),
            roots: vec![
                WorkspaceRoot {
                    path: PathBuf::from("/home/u"),
                    writable: false,
                    label: "home".into(),
                },
                WorkspaceRoot {
                    path: PathBuf::from("/home/u/ws"),
                    writable: true,
                    label: "workspace".into(),
                },
            ],
            resumed: None,
            cancel: CancelToken::new(),
        };
        let root = ctx
            .root_for(std::path::Path::new("/home/u/ws/a.txt"))
            .unwrap();
        assert!(root.writable);
        assert!(ctx.root_for(std::path::Path::new("/etc/passwd")).is_none());
    }

    #[test]
    fn a_first_attempt_has_no_resumed_marker() {
        let json = r#"{"previous_state":"planned"}"#;
        let resumed: ResumedCall = serde_json::from_str(json).unwrap();
        assert_eq!(resumed.attempts_so_far, 0);
        assert!(resumed.previous_attempt.is_none());
        assert!(
            resumed.is_known_not_to_have_run(),
            "planned 就是 §8.4 第 6 行的「确定尚未执行」"
        );
    }

    #[test]
    fn a_started_call_is_never_known_not_to_have_run() {
        // 旧线格式带着已经退役的 `verification` 也解得出来，而且它不再改变判断。
        let json = r#"{"previous_attempt":"attempt-1","previous_state":"started",
            "attempts_so_far":1,"verification":{"kind":"not_performed","evidence":"x"}}"#;
        let resumed: ResumedCall = serde_json::from_str(json).unwrap();
        assert_eq!(
            resumed.previous_attempt,
            Some(AttemptId::from_raw("attempt-1"))
        );
        assert!(
            !resumed.is_known_not_to_have_run(),
            "started 本身证明不了什么"
        );
    }
}
