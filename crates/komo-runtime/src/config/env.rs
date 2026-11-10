//! 环境：`KOMO_HOME`、`.env` 里的凭证，以及 `KOMO_*` 覆盖（§3、§11.2、§12）。
//!
//! **这是整个 crate 里唯一读环境变量的地方。**别处一律读当前 [`ConfigSnapshot`]
//! （§3 第 2 步），凭证则只以变量名出现在快照里，值由这里装进 [`Secrets`]。

use std::path::{Path, PathBuf};

use crate::llm::Secrets;

/// 数据目录的环境变量。**唯一直接读的那个**（§12：「数据目录可通过配置或 KOMO_HOME
/// 改变」）。
pub const KOMO_HOME: &str = "KOMO_HOME";

/// `KOMO_*` 覆盖：文档只定义了 `KOMO_HOME`，这里再认一个监听地址，因为它和数据目录
/// 一样属于「启动时才定得下来、部署环境说了算」的那一小组键（§3 第 4 步）。
///
// TODO(decide: §3 只点名 KOMO_HOME。模型 / 渠道键要不要也接受 KOMO_* 覆盖没有定论；
// 在定下来之前只认这两个，其余一律以文件为准——多认一个键就是多一处"配置文件里写的
// 不算数"的地方。优先级按任务书：默认 < config.toml < 环境。
pub const KOMO_LISTEN: &str = "KOMO_LISTEN";

/// 读 `.env`，再让**进程环境**覆盖它（默认 < 文件 < 环境）。
///
/// 用 `from_path_iter` 而不是 `dotenvy::from_path`：后者会往进程环境里写，于是"谁读了
/// 环境"这件事就散到了整个进程里，而热重载还会让它越写越多。
pub fn load_secrets(env_file: &Path) -> Result<Secrets, std::io::Error> {
    let text = if env_file.exists() {
        Some(std::fs::read_to_string(env_file)?)
    } else {
        None
    };
    secrets_from_text(text.as_deref(), env_file)
}

/// 同 [`load_secrets`]，但 `.env` 的内容由调用方给：Web 改配置时先拿**改后的**文本校验，
/// 校验过了才落盘（§3「从 Web 改模型配置」）。`env_file` 只用来写错误信息。
pub fn secrets_from_text(text: Option<&str>, env_file: &Path) -> Result<Secrets, std::io::Error> {
    let mut secrets = Secrets::new();
    if let Some(text) = text {
        for entry in dotenvy::from_read_iter(text.as_bytes()) {
            let (key, value) =
                entry.map_err(|e| std::io::Error::other(format!("{}：{e}", env_file.display())))?;
            secrets.insert(key, value);
        }
    }
    for (key, value) in std::env::vars() {
        // 进程环境只补 `.env` 里出现过的凭证名与我们认识的 KOMO_* 键，别的不搬——
        // 整个进程环境搬进来等于把无关变量也变成"配置的一部分"。
        if secrets.get(&key).is_some() || is_credential_name(&key) {
            secrets.insert(key, value);
        }
    }
    Ok(secrets)
}

/// 看起来像凭证名的变量：`.env` 里没有、但操作者从环境里喂进来的那种。
fn is_credential_name(name: &str) -> bool {
    name.ends_with("_API_KEY")
        || name.ends_with("_TOKEN")
        || name.ends_with("_SECRET")
        || name.ends_with("_APP_ID")
}

/// 数据目录：`override_home` > `KOMO_HOME` > `~/.komo`。
pub fn resolve_home(override_home: Option<&Path>) -> Result<PathBuf, HomeError> {
    if let Some(home) = override_home {
        return Ok(home.to_path_buf());
    }
    if let Ok(home) = std::env::var(KOMO_HOME)
        && !home.trim().is_empty()
    {
        return Ok(PathBuf::from(home));
    }
    Ok(user_home()?.join(".komo"))
}

/// 当前用户的家目录。skills 的共享搜索路径（`~/.agents/skills`）也要它（§5.6）。
pub fn user_home() -> Result<PathBuf, HomeError> {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE"));
    match home {
        Ok(home) if !home.trim().is_empty() => Ok(PathBuf::from(home)),
        _ => Err(HomeError),
    }
}

/// `KOMO_*` 覆盖：只有我们认识的那几个键。
pub fn env_override(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("找不到家目录（HOME / USERPROFILE 都没有设置），请显式设置 KOMO_HOME")]
pub struct HomeError;
