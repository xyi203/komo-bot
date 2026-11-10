//! 工具任务共用的结果落地帮手（`durable::harness::tool` 与 codemode / 监听的收尾）：图片落盘、
//! 结果正文、超时与被重启打断时交给模型的那段话。

use std::time::Duration;

use komo_kernel::traits::OutputWriter;
use komo_kernel::types::plan::{ExecutionPlan, Replay};
use komo_kernel::types::refs::{InterruptedOutput, ToolResultBody, ToolResultStatus};
use komo_kernel::types::tool::{IMAGE_LIMIT_BYTES, ToolError, ToolImage, ToolOutput};

/// 把工具交出来的图片写进这次尝试的输出目录，结果里只留引用；返回写下来的那几张，这一次
/// 直接交给模型。超过上限或写不下来的那张不让整次调用失败：正文里补一句，模型知道少了什么。
pub(crate) async fn store_images(
    writer: &mut dyn OutputWriter,
    body: &mut ToolResultBody,
    images: &[ToolImage],
) -> Vec<ToolImage> {
    let mut stored = Vec::new();
    let mut notes = Vec::new();
    for image in images {
        if image.bytes.len() > IMAGE_LIMIT_BYTES {
            notes.push(format!(
                "有一张 {} 图片 {} 字节，超过上限 {} 字节，没有附上",
                image.mime,
                image.bytes.len(),
                IMAGE_LIMIT_BYTES
            ));
            continue;
        }
        match writer.write_image(&image.mime, &image.bytes).await {
            Ok(reference) => {
                body.images.push(reference);
                stored.push(image.clone());
            }
            Err(error) => {
                tracing::warn!(%error, mime = %image.mime, "结果图片没存下来");
                notes.push(format!("有一张 {} 图片没存下来：{error}", image.mime));
            }
        }
    }
    if !notes.is_empty() {
        let preview = body.preview.get_or_insert_with(String::new);
        for note in notes {
            if !preview.is_empty() {
                preview.push('\n');
            }
            preview.push_str(&note);
        }
    }
    stored
}

/// 这份计划的动作重做一遍是安全的吗：可安全重做的读取，和外部接口确实保证去重的幂等键
/// （[`Replay::Safe`]，§8.6）。
fn safe_to_redo(plan: &ExecutionPlan) -> bool {
    plan.recovery.replay() == Replay::Safe
}

/// 中断结果里每条流最多带多少字节的末尾。
pub(crate) const INTERRUPTED_TAIL_BYTES: usize = 2048;

/// 被重启打断、又不能重跑的那次调用交给模型的正文。
pub(crate) fn interrupted_message(tool: &str, left: &InterruptedOutput) -> String {
    let mut out = format!(
        "上一次执行被 Gateway 重启打断，{tool} 重启后不会自动重跑，副作用是否已经发生未知。\
         {UNCERTAIN_ADVICE}。"
    );
    if left.is_empty() {
        out.push_str("\n中断前没有落下任何输出。");
        return out;
    }
    out.push_str("\n中断前已落盘的部分输出（不是完成结果）：");
    for (label, bytes, tail) in [
        ("stdout", left.stdout_bytes, &left.stdout_tail),
        ("stderr", left.stderr_bytes, &left.stderr_tail),
    ] {
        if bytes == 0 {
            continue;
        }
        let shown = tail.len() as u64;
        if shown < bytes {
            out.push_str(&format!("\n[{label} 共 {bytes} 字节，末尾 {shown} 字节]\n"));
        } else {
            out.push_str(&format!("\n[{label} 共 {bytes} 字节]\n"));
        }
        out.push_str(tail);
    }
    out
}

/// 工具守着 `call_timeout` 自己超时之后，还要终止进程组、回收（TERM 与 KILL 各等 5s）、
/// 写完输出。兜底计时器多给这么一段，否则两边同时到点，赢的总是执行器——一个本来
/// 说得清"超时被杀"的结果就成了"结果不明"。
pub(crate) const REAP_GRACE: Duration = Duration::from_secs(15);

/// 活动执行时限到了。
///
/// 读取可以说"失败了，再读一次就是"；其余动作在超时这一刻**不知道**副作用发没发生，
/// 所以是 `Uncertain` 而不是 `Failed`——后者会让模型自己再发一次，把效果做两遍。
pub(crate) fn timeout_error(plan: &ExecutionPlan, limit: Duration) -> ToolError {
    if safe_to_redo(plan) {
        ToolError::Timeout {
            after_secs: limit.as_secs(),
        }
    } else {
        ToolError::Uncertain {
            message: format!(
                "超过活动执行时限 {}s，副作用是否已经发生未知",
                limit.as_secs()
            ),
        }
    }
}

pub(crate) fn result_body(outcome: &Result<ToolOutput, ToolError>) -> ToolResultBody {
    match outcome {
        Ok(output) => ToolResultBody {
            status: output.status,
            result: output.result.clone(),
            error: None,
            exit_code: output.exit_code,
            artifacts: output.artifacts.clone(),
            // 图片要先写成文件才有引用：发布前由 [`store_images`] 填。
            images: vec![],
            // 工具写给模型看的那段正文跟着结果一起落进 `output.json`：模型上下文的投影读
            // 这一份，而 JSONL 里的事件只留它的前 1 KiB。
            preview: output.preview.clone(),
        },
        Err(error) => ToolResultBody {
            status: if error.is_uncertain() {
                ToolResultStatus::Uncertain
            } else {
                ToolResultStatus::Failed
            },
            result: serde_json::Value::Null,
            error: Some(error.to_string()),
            exit_code: None,
            artifacts: vec![],
            images: vec![],
            // 工具失败时模型必须看见那句话（"版本冲突：a.txt"）——投影读的就是这一份。
            // 结果不明的再附上判断方法：它不停下来等人，由模型去核对。
            preview: Some(if error.is_uncertain() {
                format!("{error}。{UNCERTAIN_ADVICE}")
            } else {
                error.to_string()
            }),
        },
    }
}

/// 结果不明（执行当中超时 / 断开，或续跑时核对不出）交给模型的那段判断方法：先核对，
/// 再决定重做还是接着往下。
const UNCERTAIN_ADVICE: &str = "执行器不会自动重做。\
先核对目标的实际状态（读文件、查进程或服务状态、看 git、查远端），再决定：已经生效就接着往下；\
没生效或只做了一半就继续或重新做；只读的命令直接重跑即可。\
发消息、扣款、切换一次这类重复做会出问题、又核对不了的，先问用户";
