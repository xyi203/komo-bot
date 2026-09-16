//! 进程内唯一的 `Arc<ConfigSnapshot>` 与它的热重载（§3、§13.2）。
//!
//! 四步流程（§3）在这里各占一段：重新解析成**完整的**新快照并跑与 `komo config check`
//! **相同**的校验 → 任何错误就原样保留旧快照 → 通过则 arc-swap 原子替换并报告差异
//! （只出键名）→ 只在启动时生效的那几个键单列出来。
//!
//! 触发方式（mtime 轮询、`SIGHUP`、`komo config reload`）不在这里：它们是 Gateway 的
//! 事，这里只提供两个入口——[`ConfigHolder::changed_since`] 和
//! [`ConfigHolder::reload`]。

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use komo_kernel::protocol::config::{ConfigIssue, ConfigSnapshot, KeyPath};
use time::OffsetDateTime;

use super::effort::EffortCapabilities;
use super::env::Secrets;
use super::error::ConfigError;
use super::file::Sources;
use super::{LoadOptions, Loaded, load_config};

/// 一次重载的结果。**只有键名**——值不进来，凭证更不进来（§3 第 3 步）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadReport {
    /// 变了的键。
    pub changed: Vec<KeyPath>,
    /// 其中**只在启动时生效**的那些：重载照常完成其余部分，然后明确报告它们需要
    /// `komo gateway restart`（§3 第 4 步）。
    pub start_only: Vec<KeyPath>,
    /// 校验通过但值得说的（例如某渠道 enabled 而 allow_from 为空）。
    pub warnings: Vec<ConfigIssue>,
    pub loaded_at: OffsetDateTime,
}

impl ReloadReport {
    /// 什么都没变——重载照样发生了，但没有需要重建的东西。
    pub fn is_noop(&self) -> bool {
        self.changed.is_empty()
    }

    /// 给操作者看的一行话。
    pub fn summary(&self) -> String {
        if self.changed.is_empty() {
            return "配置已重载：没有键发生变化".to_string();
        }
        let mut text = format!(
            "配置已重载：{} 个键变化（{}）",
            self.changed.len(),
            self.changed
                .iter()
                .map(KeyPath::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        );
        if !self.start_only.is_empty() {
            text.push_str(&format!(
                "；以下键需要 `komo gateway restart` 才生效：{}",
                self.start_only
                    .iter()
                    .map(KeyPath::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        text
    }
}

/// 持有当前快照的那一个东西。
pub struct ConfigHolder {
    current: ArcSwap<Loaded>,
    sources: Sources,
    home: PathBuf,
    caps: EffortCapabilities,
}

impl std::fmt::Debug for ConfigHolder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigHolder")
            .field("sources", &self.sources)
            .field("loaded_at", &self.current.load().snapshot.loaded_at)
            .finish()
    }
}

impl ConfigHolder {
    /// 解析、校验、装上。校验不过就一个快照都不装——**进程里没有"半装上"的配置**。
    pub fn load(options: &LoadOptions) -> Result<Self, ConfigError> {
        let loaded = load_config(options)?;
        let home = loaded.home.clone();
        let sources = loaded.sources.clone();
        Ok(ConfigHolder {
            current: ArcSwap::from(Arc::new(loaded)),
            sources,
            home,
            caps: options.caps.clone(),
        })
    }

    /// 已经有一份加载结果时（测试、或者启动时已经加载过一次）。
    pub fn adopt(loaded: Loaded, caps: EffortCapabilities) -> Self {
        let home = loaded.home.clone();
        let sources = loaded.sources.clone();
        ConfigHolder {
            current: ArcSwap::from(Arc::new(loaded)),
            sources,
            home,
            caps,
        }
    }

    /// 当前快照。**按用途读当前快照、不缓存**（§3 第 2 步）。
    pub fn current(&self) -> Arc<ConfigSnapshot> {
        Arc::clone(&self.current.load().snapshot)
    }

    /// 当前凭证。值只在这里，快照里只有指纹。
    pub fn secrets(&self) -> Arc<Secrets> {
        Arc::clone(&self.current.load().secrets)
    }

    /// 当前这份配置装上时留下的警告。
    pub fn warnings(&self) -> Vec<ConfigIssue> {
        self.current.load().issues.clone()
    }

    pub fn sources(&self) -> &Sources {
        &self.sources
    }

    pub fn home(&self) -> &std::path::Path {
        &self.home
    }

    /// 三个来源文件的 mtime 和装上这份快照时记下的不一样吗。
    ///
    /// Gateway 每秒调一次它（§3：mtime 轮询，不引入 inotify）。文件**消失**也算变化：
    /// 删掉 policy.toml 是一次真实的配置改动。
    pub fn changed_since(&self) -> bool {
        let recorded = &self.current.load().snapshot.sources;
        let now = self.sources.stamps();
        if recorded.len() != now.len() {
            return true;
        }
        now.iter().any(|fresh| {
            !recorded
                .iter()
                .any(|old| old.path == fresh.path && old.mtime == fresh.mtime)
        })
    }

    /// §3 的四步。**任何错误 → 旧快照原样保留**。
    pub fn reload(&self) -> Result<ReloadReport, ConfigError> {
        let options = LoadOptions {
            home: Some(self.home.clone()),
            caps: self.caps.clone(),
        };
        // 解析 + 校验。失败就直接返回——`current` 一个字节都没动。
        let next = load_config(&options)?;

        let previous = self.current.load_full();
        let changed = previous.snapshot.diff(&next.snapshot);
        let start_only = previous.snapshot.start_only_changes(&next.snapshot);
        let report = ReloadReport {
            changed,
            start_only,
            warnings: next.issues.clone(),
            loaded_at: next.snapshot.loaded_at,
        };

        // 原子替换。
        self.current.store(Arc::new(next));
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::testing::{Fixture, write};

    #[test]
    fn a_broken_edit_leaves_the_old_snapshot_in_place() {
        let fixture = Fixture::valid();
        let holder = fixture.holder();
        let before = holder.current();
        assert_eq!(before.model.model, "chat-a");

        write(
            &fixture.sources().config,
            &Fixture::config_text("chat-b", "ultra"),
        );
        let error = holder.reload().unwrap_err();
        assert!(!error.issues().is_empty(), "{error}");

        let after = holder.current();
        assert_eq!(after.model.model, "chat-a", "旧快照原样保留");
        assert_eq!(after.loaded_at, before.loaded_at);
    }

    #[test]
    fn a_good_edit_swaps_the_snapshot_and_reports_key_names_only() {
        let fixture = Fixture::valid();
        let holder = fixture.holder();
        write(
            &fixture.sources().config,
            &Fixture::config_text("chat-b", "high"),
        );

        let report = holder.reload().unwrap();
        assert_eq!(holder.current().model.model, "chat-b");
        assert!(
            report
                .changed
                .iter()
                .any(|k| k.as_str() == "model.model" || k.as_str() == "model.effort"),
            "{report:?}"
        );
        for key in &report.changed {
            assert!(!key.as_str().contains("chat-b"), "diff 只出键名：{key}");
            assert!(!key.as_str().contains("high"), "diff 只出键名：{key}");
        }
    }

    #[test]
    fn a_start_only_key_is_reported_on_its_own() {
        let fixture = Fixture::valid();
        let holder = fixture.holder();
        let mut text = Fixture::config_text("chat-a", "medium");
        text.push_str("\n[gateway]\nlisten = \"127.0.0.1:7788\"\n");
        write(&fixture.sources().config, &text);

        let report = holder.reload().unwrap();
        assert_eq!(
            report.start_only,
            vec![KeyPath::new("start_only.listen")],
            "{report:?}"
        );
        assert!(
            report.summary().contains("komo gateway restart"),
            "{}",
            report.summary()
        );
        // 其余部分照常完成：新的监听地址确实装进了快照，只是要重启才生效。
        assert_eq!(holder.current().start_only.listen, "127.0.0.1:7788");
    }

    #[test]
    fn mtimes_are_what_changed_since_compares() {
        let fixture = Fixture::valid();
        let holder = fixture.holder();
        assert!(!holder.changed_since());

        // 换一份 mtime 明显不同的内容。
        let path = fixture.sources().config.clone();
        let text = Fixture::config_text("chat-a", "medium");
        write(&path, &format!("{text}\n# touched\n"));
        filetime_bump(&path);
        assert!(holder.changed_since());

        holder.reload().unwrap();
        assert!(!holder.changed_since(), "装上之后就不再是「改了但没装上」");
    }

    #[test]
    fn a_reload_that_changes_nothing_is_a_noop_report() {
        let fixture = Fixture::valid();
        let holder = fixture.holder();
        let report = holder.reload().unwrap();
        assert!(report.is_noop(), "{report:?}");
        assert!(report.summary().contains("没有键发生变化"));
    }

    /// 文件系统的 mtime 分辨率可能粗到一秒；把 mtime 明确往前推，测的是比对逻辑而不
    /// 是时钟精度。
    fn filetime_bump(path: &std::path::Path) {
        let metadata = std::fs::metadata(path).unwrap();
        let later = metadata.modified().unwrap() + std::time::Duration::from_secs(5);
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(later).unwrap();
    }
}
