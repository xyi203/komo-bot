//! `komo channel wechat login`：终端二维码登录与凭证落盘（§3、§12）。
//!
//! 这是微信渠道唯一一段**人在终端前**才跑的代码，也是唯一一段会写凭证的代码。它不在
//! Gateway 里：`komo channel wechat login` 与 `komo channel list | probe` 一样不经
//! Gateway（§3），因为它要一个能显示二维码、能读一行配对码的终端。
//!
//! 七态状态机被拆成两半，这是这个模块的全部设计：
//!
//! - [`advance`] 是**纯函数**：一份 `QrStatusResponse` 加"手上有没有一个待提交的配对
//!   码"，答一个 [`LoginStep`]。它不联网、不睡觉、不打印，所以那七种状态的转移可以用
//!   一串假响应逐条断言。
//! - [`login_with`] 是**驱动**：轮询、退避、换二维码、问配对码、落盘。它自己不判断
//!   状态。
//!
//! 状态取自 spike §2.7（读 wechatbot 0.4.0 的 `bot.rs:115-255`），逐条对应：
//! `scaned` / `need_verifycode` / `verify_code_blocked` / `scaned_but_redirect` /
//! `binded_redirect` / `expired` / `confirmed`。**认不出的状态按"继续轮询"处理**，
//! 不按失败：iLink 没有公开接口文档，服务端多一个中间态时，停下来比多等两秒糟得多。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use komo_kernel::traits::ChannelError;
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use wechatbot::protocol::{DEFAULT_BASE_URL, ILinkClient, QrStatusResponse};
use wechatbot::types::Credentials;

/// 凭证文件在数据目录里的位置（§12：`~/.komo/wechat/`）。
pub fn credentials_path(data_dir: &Path) -> PathBuf {
    data_dir.join("wechat").join("credentials.json")
}

/// 登录这件事能出的岔子。
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// 与 iLink 的往返失败。
    #[error("微信登录：{0}")]
    Protocol(String),
    /// 凭证文件读写失败。
    #[error("微信凭证 {path}：{message}")]
    Credentials { path: String, message: String },
    /// 二维码渲染失败（几乎只可能是负载太长）。
    #[error("微信二维码渲染失败：{0}")]
    Qr(String),
    /// 流程走不下去了——二维码刷新次数用完、`binded_redirect` 但本地没凭证……
    #[error("微信登录中止：{0}")]
    Aborted(String),
}

impl From<LoginError> for ChannelError {
    fn from(error: LoginError) -> Self {
        ChannelError {
            channel: "wechat".into(),
            message: error.to_string(),
        }
    }
}

/// 状态机的一步。
///
/// 没有 `PartialEq`：`Confirmed` 里装的 [`Credentials`] 是 SDK 的类型，它只有 `Debug`
/// 与 `Clone`。测试按变体匹配，这也正是 §11.4 对错误的要求在这里的同一条纪律。
#[derive(Debug)]
pub enum LoginStep {
    /// 已扫码，等手机上确认。手上那个待提交的配对码已经被接受了，清掉它。
    Scanned,
    /// 还没有新消息，接着轮询。
    Poll,
    /// 服务端要配对码（手机上显示的那串数字）。`retry` = 上一个被拒了。
    AskVerifyCode { retry: bool },
    /// 换 IDC：之后的轮询打这个 host。
    Redirect { host: String },
    /// 这张二维码不能用了，换一张。
    NewQr { reason: &'static str },
    /// 服务端说这个机器人已经绑定过本客户端，复用本地凭证。
    ReuseStored,
    /// 成功。
    Confirmed(Box<Credentials>),
    /// 服务端答了 `confirmed` 却没给 token 这类说不通的情形。
    Failed(String),
}

/// 一次状态轮询的解读。**纯函数**。
///
/// `pending_code` = 手上有没有一个已经提交、还没得到答复的配对码；
/// `fallback_base_url` = `confirmed` 没带 `baseurl` 时用哪个 IDC。
pub fn advance(
    status: &QrStatusResponse,
    pending_code: bool,
    fallback_base_url: &str,
) -> LoginStep {
    match status.status.as_str() {
        "scaned" => LoginStep::Scanned,
        "need_verifycode" => LoginStep::AskVerifyCode {
            retry: pending_code,
        },
        // 配对码错太多次：这张二维码被服务端封了，只能换一张（计入刷新上限）。
        "verify_code_blocked" => LoginStep::NewQr {
            reason: "配对码错误次数过多",
        },
        "scaned_but_redirect" => match status.redirect_host.as_deref() {
            Some(host) if !host.is_empty() => LoginStep::Redirect {
                host: host.to_string(),
            },
            // 说要重定向却没说去哪：按"接着轮询"处理，不编一个 host 出来。
            _ => LoginStep::Poll,
        },
        "binded_redirect" => LoginStep::ReuseStored,
        "expired" => LoginStep::NewQr {
            reason: "二维码已过期",
        },
        "confirmed" => match status.bot_token.as_deref() {
            Some(token) if !token.is_empty() => LoginStep::Confirmed(Box::new(Credentials {
                token: token.to_string(),
                base_url: status
                    .baseurl
                    .clone()
                    .filter(|url| !url.is_empty())
                    .unwrap_or_else(|| fallback_base_url.to_string()),
                account_id: status.ilink_bot_id.clone().unwrap_or_default(),
                user_id: status.ilink_user_id.clone().unwrap_or_default(),
                saved_at: Some(now_stamp()),
            })),
            _ => LoginStep::Failed("服务端说已确认，却没有给 bot_token".into()),
        },
        // 认不出的状态：接着轮询。iLink 没有公开接口文档，多一个中间态时停下来比多等
        // 两秒糟得多——而真的走不下去时，二维码会自己 `expired`。
        _ => LoginStep::Poll,
    }
}

/// 手机上那串配对码从哪来。
///
/// 是一个接缝而不是直接读 stdin：登录流程要能在测试里整条跑一遍，而测试进程没有终端。
pub trait VerifyCodePrompt: Send + Sync {
    /// `retry` = 上一个码被拒了。
    fn ask(&self, retry: bool) -> Result<String, LoginError>;
}

/// 生产实现：从 stdin 读一行。
#[derive(Debug, Default)]
pub struct StdinPrompt;

impl VerifyCodePrompt for StdinPrompt {
    fn ask(&self, retry: bool) -> Result<String, LoginError> {
        use std::io::BufRead;
        let prompt = if retry {
            "配对码不对，请再输入一次微信里显示的配对码："
        } else {
            "请输入手机微信里显示的配对码："
        };
        eprint!("{prompt}");
        std::io::stderr().flush().ok();
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|error| LoginError::Aborted(format!("读配对码失败：{error}")))?;
        Ok(line.trim().to_string())
    }
}

/// 登录流程的可调项。生产用 [`LoginOptions::default`]，测试把间隔调短。
#[derive(Debug, Clone)]
pub struct LoginOptions {
    /// 取二维码与轮询状态的起始 host。
    pub qr_base_url: String,
    /// 两次状态轮询之间隔多久（SDK 用 2 秒）。
    pub poll_interval: Duration,
    /// 最多换几张二维码（SDK 的 `MAX_QR_REFRESH`）。
    pub max_qr_refresh: u32,
}

impl Default for LoginOptions {
    fn default() -> Self {
        LoginOptions {
            qr_base_url: DEFAULT_BASE_URL.to_string(),
            poll_interval: Duration::from_secs(2),
            max_qr_refresh: 3,
        }
    }
}

/// `komo channel wechat login` 的那一次性流程。
///
/// `out` 是二维码与进度打到哪里——bin 传 stderr。**凭证本身一个字都不打印**。
pub async fn login(
    credentials_path: &Path,
    out: &mut (dyn Write + Send),
) -> Result<Credentials, LoginError> {
    login_with(credentials_path, out, &StdinPrompt, LoginOptions::default()).await
}

/// [`login`] 的可注入版本。
pub async fn login_with(
    credentials_path: &Path,
    out: &mut (dyn Write + Send),
    prompt: &dyn VerifyCodePrompt,
    options: LoginOptions,
) -> Result<Credentials, LoginError> {
    let client = ILinkClient::new();
    // 已有的凭证有两个用处：把旧 token 报给服务端（它才能答 `binded_redirect` 而不是
    // 再开一份会话），以及 `binded_redirect` 时拿来复用。读不出来不是错误——第一次
    // 登录本来就没有。
    let stored = load_credentials(credentials_path).ok();
    let local_tokens: Vec<String> = stored
        .as_ref()
        .filter(|creds| !creds.token.is_empty())
        .map(|creds| vec![creds.token.clone()])
        .unwrap_or_default();

    for attempt in 1..=options.max_qr_refresh {
        let qr = client
            .get_qr_code(&options.qr_base_url, &local_tokens)
            .await
            .map_err(|error| LoginError::Protocol(format!("取二维码失败：{error}")))?;

        writeln!(
            out,
            "用微信扫这张码（第 {attempt} 张，共 {} 张）：",
            options.max_qr_refresh
        )
        .ok();
        writeln!(out, "{}", render_qr(&qr.qrcode_img_content)?).ok();
        out.flush().ok();

        let mut poll_base = options.qr_base_url.clone();
        let mut pending_code: Option<String> = None;

        loop {
            let status = client
                .poll_qr_status(&poll_base, &qr.qrcode, pending_code.as_deref())
                .await
                .map_err(|error| LoginError::Protocol(format!("轮询扫码状态失败：{error}")))?;

            match advance(&status, pending_code.is_some(), &options.qr_base_url) {
                LoginStep::Scanned => {
                    // 走到这里说明待提交的配对码被接受了。
                    pending_code = None;
                    writeln!(out, "已扫码，请在手机上确认。").ok();
                }
                LoginStep::Poll => {}
                LoginStep::AskVerifyCode { retry } => {
                    pending_code = Some(prompt.ask(retry)?);
                    // 带着码立刻重询，不睡这一轮。
                    continue;
                }
                LoginStep::Redirect { host } => {
                    poll_base = format!("https://{host}");
                    writeln!(out, "服务端要求换一个接入点，继续等待确认。").ok();
                }
                LoginStep::NewQr { reason } => {
                    writeln!(out, "{reason}，换一张二维码。").ok();
                    break;
                }
                LoginStep::ReuseStored => {
                    let Some(stored) = stored else {
                        return Err(LoginError::Aborted(
                            "服务端说这个机器人已经绑定过本机，但本机没有凭证文件；\
                             请先在微信里解绑再重新登录"
                                .into(),
                        ));
                    };
                    writeln!(out, "这个机器人已经绑定过本机，沿用现有凭证。").ok();
                    return Ok(stored);
                }
                LoginStep::Confirmed(credentials) => {
                    save_credentials(credentials_path, &credentials)?;
                    writeln!(
                        out,
                        "登录成功，凭证已写入 {}（权限 0600）。",
                        credentials_path.display()
                    )
                    .ok();
                    return Ok(*credentials);
                }
                LoginStep::Failed(message) => return Err(LoginError::Aborted(message)),
            }

            out.flush().ok();
            tokio::time::sleep(options.poll_interval).await;
        }
    }

    Err(LoginError::Aborted(format!(
        "二维码连续过期 {} 次",
        options.max_qr_refresh
    )))
}

/// 把二维码负载画成终端字符。
///
/// `Dense1x2` 用 `▀` / `▄` / `█` 一个字符装两行，所以一张 QR 在 80 列的终端里放得下。
pub fn render_qr(payload: &str) -> Result<String, LoginError> {
    let code = QrCode::new(payload.as_bytes())
        .map_err(|error| LoginError::Qr(format!("{error}（负载 {} 字节）", payload.len())))?;
    Ok(code
        .render::<Dense1x2>()
        // 静默区是规范要求的：贴着终端边缘的码有些手机扫不出来。
        .quiet_zone(true)
        .build())
}

// ---------------------------------------------------------------- 凭证文件

/// 读一份凭证（§12：`~/.komo/wechat/credentials.json`）。
pub fn load_credentials(path: &Path) -> Result<Credentials, LoginError> {
    let data = std::fs::read_to_string(path).map_err(|error| LoginError::Credentials {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    serde_json::from_str(&data).map_err(|error| LoginError::Credentials {
        path: path.display().to_string(),
        message: format!("解析失败：{error}"),
    })
}

/// 写一份凭证，权限 **0600**。
///
/// 文件里是一个可以直接冒充这台机器的 bearer token，所以模式在**创建时**就给死，不是
/// 写完再 chmod：后者中间有一个别人读得到的瞬间。
pub fn save_credentials(path: &Path, credentials: &Credentials) -> Result<(), LoginError> {
    let fail = |message: String| LoginError::Credentials {
        path: path.display().to_string(),
        message,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| fail(error.to_string()))?;
    }
    let body = serde_json::to_string_pretty(credentials)
        .map_err(|error| fail(format!("序列化失败：{error}")))?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| fail(error.to_string()))?;
    // 与 SDK 一样以换行收尾，好让两边写出来的文件逐字节一致。
    writeln!(file, "{body}").map_err(|error| fail(error.to_string()))?;

    // 文件已经存在时 `mode(0o600)` 不生效（那是创建模式），所以再收一次。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| fail(error.to_string()))?;
    }
    Ok(())
}

/// `saved_at` 的格式：`"{unix 秒}Z"`。
///
/// **不是 ISO 8601**，尽管长得像——SDK 的 `chrono_now`（`bot.rs:745-748`）写的就是这个
/// 串。komo 照抄它而不是"顺手修正"，因为这个文件两边都要读得懂。
pub fn now_stamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    format!("{seconds}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(kind: &str) -> QrStatusResponse {
        serde_json::from_value(serde_json::json!({ "status": kind })).expect("状态")
    }

    // ⑩ QR 状态机的转移（假响应序列，不联网）。
    #[test]
    fn the_state_machine_walks_all_seven_states() {
        assert!(matches!(
            advance(&status("scaned"), true, DEFAULT_BASE_URL),
            LoginStep::Scanned
        ));

        // 第一次要码不是重试；手上已经有一个待答复的码时才是。
        assert!(matches!(
            advance(&status("need_verifycode"), false, DEFAULT_BASE_URL),
            LoginStep::AskVerifyCode { retry: false }
        ));
        assert!(matches!(
            advance(&status("need_verifycode"), true, DEFAULT_BASE_URL),
            LoginStep::AskVerifyCode { retry: true }
        ));

        assert!(matches!(
            advance(&status("verify_code_blocked"), true, DEFAULT_BASE_URL),
            LoginStep::NewQr { .. }
        ));
        assert!(matches!(
            advance(&status("expired"), false, DEFAULT_BASE_URL),
            LoginStep::NewQr { .. }
        ));
        assert!(matches!(
            advance(&status("binded_redirect"), false, DEFAULT_BASE_URL),
            LoginStep::ReuseStored
        ));

        let redirect: QrStatusResponse = serde_json::from_value(serde_json::json!({
            "status": "scaned_but_redirect",
            "redirect_host": "idc2.example",
        }))
        .expect("状态");
        match advance(&redirect, false, DEFAULT_BASE_URL) {
            LoginStep::Redirect { host } => assert_eq!(host, "idc2.example"),
            other => panic!("要换 IDC：{other:?}"),
        }

        let confirmed: QrStatusResponse = serde_json::from_value(serde_json::json!({
            "status": "confirmed",
            "bot_token": "tok",
            "baseurl": "https://idc2.example",
            "ilink_bot_id": "acct",
            "ilink_user_id": "wxid_op",
        }))
        .expect("状态");
        match advance(&confirmed, false, DEFAULT_BASE_URL) {
            LoginStep::Confirmed(credentials) => {
                assert_eq!(credentials.token, "tok");
                assert_eq!(credentials.base_url, "https://idc2.example");
                assert_eq!(credentials.account_id, "acct");
                assert_eq!(credentials.user_id, "wxid_op");
                assert!(
                    credentials.saved_at.is_some_and(|at| at.ends_with('Z')),
                    "saved_at 是 \"{{unix 秒}}Z\""
                );
            }
            other => panic!("该成功了：{other:?}"),
        }
    }

    #[test]
    fn a_confirmation_without_a_token_fails_instead_of_writing_a_blank_credential() {
        let confirmed: QrStatusResponse =
            serde_json::from_value(serde_json::json!({ "status": "confirmed" })).expect("状态");
        assert!(matches!(
            advance(&confirmed, false, DEFAULT_BASE_URL),
            LoginStep::Failed(_)
        ));
    }

    #[test]
    fn a_confirmation_without_a_baseurl_falls_back_to_the_qr_host() {
        let confirmed: QrStatusResponse = serde_json::from_value(
            serde_json::json!({ "status": "confirmed", "bot_token": "tok" }),
        )
        .expect("状态");
        match advance(&confirmed, false, "https://fallback.example") {
            LoginStep::Confirmed(credentials) => {
                assert_eq!(credentials.base_url, "https://fallback.example")
            }
            other => panic!("该成功了：{other:?}"),
        }
    }

    #[test]
    fn a_redirect_without_a_host_just_keeps_polling() {
        assert!(matches!(
            advance(&status("scaned_but_redirect"), false, DEFAULT_BASE_URL),
            LoginStep::Poll
        ));
    }

    #[test]
    fn an_unknown_status_keeps_polling_rather_than_giving_up() {
        assert!(matches!(
            advance(&status("some_future_state"), false, DEFAULT_BASE_URL),
            LoginStep::Poll
        ));
    }

    // ⑨ 凭证文件读写往返 + 0600。
    #[test]
    fn credentials_round_trip_and_stay_private() {
        let home = tempfile::tempdir().expect("临时目录");
        let path = credentials_path(home.path());
        assert!(path.ends_with("wechat/credentials.json"), "§12 的位置");

        let credentials = Credentials {
            token: "tok".into(),
            base_url: "https://idc2.example".into(),
            account_id: "acct".into(),
            user_id: "wxid_op".into(),
            saved_at: Some(now_stamp()),
        };
        save_credentials(&path, &credentials).expect("写凭证");

        let raw = std::fs::read_to_string(&path).expect("读回来");
        let json: serde_json::Value = serde_json::from_str(&raw).expect("是 JSON");
        // 字段名照 wechatbot 的 serde rename——两边要读得懂同一个文件。
        assert_eq!(json["token"], "tok");
        assert_eq!(json["baseUrl"], "https://idc2.example");
        assert_eq!(json["accountId"], "acct");
        assert_eq!(json["userId"], "wxid_op");
        let saved_at = json["saved_at"].as_str().expect("saved_at");
        assert!(saved_at.ends_with('Z'), "{saved_at}");
        assert!(
            saved_at.trim_end_matches('Z').parse::<u64>().is_ok(),
            "是 \"{{unix 秒}}Z\" 而不是 ISO 8601：{saved_at}"
        );
        assert!(raw.ends_with('\n'), "与 SDK 一样以换行收尾");

        let read_back = load_credentials(&path).expect("读凭证");
        assert_eq!(read_back.token, credentials.token);
        assert_eq!(read_back.base_url, credentials.base_url);
        assert_eq!(read_back.account_id, credentials.account_id);
        assert_eq!(read_back.user_id, credentials.user_id);
        assert_eq!(read_back.saved_at, credentials.saved_at);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("元数据")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "凭证只给自己读：{:o}", mode & 0o777);
        }
    }

    #[cfg(unix)]
    #[test]
    fn rewriting_a_loose_credentials_file_tightens_it() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().expect("临时目录");
        let path = credentials_path(home.path());
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
        std::fs::write(&path, "{}").expect("先写一个松的");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("放松");

        save_credentials(
            &path,
            &Credentials {
                token: "tok".into(),
                base_url: "https://idc2.example".into(),
                account_id: String::new(),
                user_id: String::new(),
                saved_at: Some(now_stamp()),
            },
        )
        .expect("写凭证");

        let mode = std::fs::metadata(&path)
            .expect("元数据")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "已存在的文件也要收紧：{:o}",
            mode & 0o777
        );
    }

    #[test]
    fn a_missing_credentials_file_is_an_error_that_names_the_path() {
        let home = tempfile::tempdir().expect("临时目录");
        let path = credentials_path(home.path());
        let error = load_credentials(&path).expect_err("没有这个文件");
        assert!(error.to_string().contains("credentials.json"), "{error}");
    }

    /// 记下每次被问配对码，答一个固定的码。
    #[derive(Debug, Default)]
    struct RecordingPrompt {
        asked: std::sync::Mutex<Vec<bool>>,
    }

    impl VerifyCodePrompt for RecordingPrompt {
        fn ask(&self, retry: bool) -> Result<String, LoginError> {
            self.asked.lock().expect("配对码记录").push(retry);
            Ok("123456".into())
        }
    }

    fn fast(endpoint: String) -> LoginOptions {
        LoginOptions {
            qr_base_url: endpoint,
            poll_interval: Duration::from_millis(1),
            max_qr_refresh: 3,
        }
    }

    // ⑩ 整条登录流程用一串假响应驱动，不联网。
    #[tokio::test]
    async fn a_login_walks_scan_pairing_code_and_confirmation() {
        use super::super::fake::{Behavior, FakeILink};

        let fake = FakeILink::start_with_qr(
            Behavior::default(),
            vec![
                serde_json::json!({ "status": "scaned" }),
                serde_json::json!({ "status": "need_verifycode" }),
                serde_json::json!({ "status": "scaned" }),
                serde_json::json!({
                    "status": "confirmed",
                    "bot_token": "tok",
                    "baseurl": "https://idc2.example",
                    "ilink_bot_id": "acct",
                    "ilink_user_id": "wxid_op",
                }),
            ],
        )
        .await;
        let home = tempfile::tempdir().expect("临时目录");
        let path = credentials_path(home.path());
        let prompt = RecordingPrompt::default();
        let mut out: Vec<u8> = Vec::new();

        let credentials = login_with(&path, &mut out, &prompt, fast(fake.endpoint()))
            .await
            .expect("登录成功");

        assert_eq!(credentials.token, "tok");
        assert_eq!(credentials.base_url, "https://idc2.example");
        assert_eq!(
            *prompt.asked.lock().expect("配对码记录"),
            vec![false],
            "只问一次，而且不是重试"
        );
        // 带着码立刻重询：那一次的查询串里有 verify_code。
        let polls = fake.calls_to("/ilink/bot/get_qrcode_status");
        assert_eq!(polls.len(), 4, "{polls:?}");
        assert!(
            polls[2]["query"]
                .as_str()
                .is_some_and(|query| query.contains("verify_code=123456")),
            "{polls:?}"
        );
        assert_eq!(fake.call_count("/ilink/bot/get_bot_qrcode"), 1);

        // 凭证落盘了，而屏幕上一个字的 token 都没有。
        assert_eq!(load_credentials(&path).expect("读回来").token, "tok");
        let printed = String::from_utf8(out).expect("UTF-8");
        assert!(!printed.contains("tok"), "{printed}");
        assert!(printed.contains("登录成功"), "{printed}");
    }

    #[tokio::test]
    async fn an_expired_qr_is_replaced_rather_than_retried() {
        use super::super::fake::{Behavior, FakeILink};

        let fake = FakeILink::start_with_qr(
            Behavior::default(),
            vec![
                serde_json::json!({ "status": "expired" }),
                serde_json::json!({ "status": "confirmed", "bot_token": "tok" }),
            ],
        )
        .await;
        let home = tempfile::tempdir().expect("临时目录");
        let path = credentials_path(home.path());
        let mut out: Vec<u8> = Vec::new();

        login_with(
            &path,
            &mut out,
            &RecordingPrompt::default(),
            fast(fake.endpoint()),
        )
        .await
        .expect("第二张码成了");
        assert_eq!(
            fake.call_count("/ilink/bot/get_bot_qrcode"),
            2,
            "过期就换一张，不是用同一张重试"
        );
    }

    #[tokio::test]
    async fn a_login_gives_up_after_the_refresh_limit() {
        use super::super::fake::{Behavior, FakeILink};

        let fake = FakeILink::start_with_qr(
            Behavior::default(),
            vec![serde_json::json!({ "status": "expired" })],
        )
        .await;
        let home = tempfile::tempdir().expect("临时目录");
        let mut out: Vec<u8> = Vec::new();

        let error = login_with(
            &credentials_path(home.path()),
            &mut out,
            &RecordingPrompt::default(),
            fast(fake.endpoint()),
        )
        .await
        .expect_err("三张码都过期了");
        assert!(matches!(error, LoginError::Aborted(_)), "{error:?}");
        assert_eq!(fake.call_count("/ilink/bot/get_bot_qrcode"), 3);
    }

    #[tokio::test]
    async fn a_bound_bot_without_local_credentials_says_what_to_do() {
        use super::super::fake::{Behavior, FakeILink};

        let fake = FakeILink::start_with_qr(
            Behavior::default(),
            vec![serde_json::json!({ "status": "binded_redirect" })],
        )
        .await;
        let home = tempfile::tempdir().expect("临时目录");
        let mut out: Vec<u8> = Vec::new();

        let error = login_with(
            &credentials_path(home.path()),
            &mut out,
            &RecordingPrompt::default(),
            fast(fake.endpoint()),
        )
        .await
        .expect_err("服务端说绑过，本机却没有凭证");
        assert!(error.to_string().contains("解绑"), "{error}");
    }

    #[tokio::test]
    async fn a_bound_bot_with_local_credentials_reuses_them() {
        use super::super::fake::{Behavior, FakeILink};

        let fake = FakeILink::start_with_qr(
            Behavior::default(),
            vec![serde_json::json!({ "status": "binded_redirect" })],
        )
        .await;
        let home = tempfile::tempdir().expect("临时目录");
        let path = credentials_path(home.path());
        save_credentials(
            &path,
            &Credentials {
                token: "old-tok".into(),
                base_url: "https://idc1.example".into(),
                account_id: "acct".into(),
                user_id: "wxid_op".into(),
                saved_at: Some(now_stamp()),
            },
        )
        .expect("写凭证");
        let mut out: Vec<u8> = Vec::new();

        let credentials = login_with(
            &path,
            &mut out,
            &RecordingPrompt::default(),
            fast(fake.endpoint()),
        )
        .await
        .expect("沿用现有凭证");
        assert_eq!(credentials.token, "old-tok");
        // 旧 token 报给了服务端，它才答得出 `binded_redirect`。
        let qr = fake.calls_to("/ilink/bot/get_bot_qrcode");
        assert_eq!(qr[0]["local_token_list"][0], "old-tok");
    }

    #[test]
    fn a_qr_payload_renders_into_terminal_characters() {
        let rendered = render_qr("https://ilinkai.weixin.qq.com/qr/abc").expect("画得出来");
        assert!(rendered.lines().count() > 8, "一张码不止几行");
        assert!(
            rendered
                .chars()
                .any(|ch| ch == '█' || ch == '▀' || ch == '▄'),
            "用的是半格字符"
        );
    }
}
