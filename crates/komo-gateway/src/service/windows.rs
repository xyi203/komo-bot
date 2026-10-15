//! provider `/models` 报的上下文窗口缓存。
//!
//! 解析顺序：配置里的 `context_window` > 这里探到的 > 未知。探测只在后台跑，受理 Run 时
//! 只读缓存，没有就当未知（Run 快照冻结后不再变）。

use std::collections::HashMap;
use std::sync::Mutex;

use komo_kernel::protocol::http::{ModelWindow, WindowSource};
use komo_kernel::types::model::ModelConfig;

/// 同一个端点上的同一个上游模型 id 只有一个窗口，与 alias 无关。
pub type WindowKey = (String, String);

pub fn key_of(config: &ModelConfig) -> WindowKey {
    (config.base_url.clone(), config.model.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowState {
    Probing,
    Found(u64),
    /// provider 答了，但没说这个模型的窗口。
    Missing,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct WindowCache {
    entries: Mutex<HashMap<WindowKey, WindowState>>,
}

impl WindowCache {
    /// 要不要由调用方起一次探测：没有记录或上次失败才要，并当场记成 `Probing`，
    /// 同一个键并发调用只有一个拿到 `true`。
    pub fn begin(&self, key: &WindowKey) -> bool {
        let mut entries = self.entries.lock().expect("锁没中毒");
        match entries.get(key) {
            None | Some(WindowState::Failed(_)) => {
                entries.insert(key.clone(), WindowState::Probing);
                true
            }
            Some(_) => false,
        }
    }

    pub fn finish(&self, key: WindowKey, state: WindowState) {
        self.entries.lock().expect("锁没中毒").insert(key, state);
    }

    pub fn get(&self, key: &WindowKey) -> Option<WindowState> {
        self.entries.lock().expect("锁没中毒").get(key).cloned()
    }

    /// 配置优先，其次探到的。
    pub fn effective(&self, config: &ModelConfig) -> Option<u64> {
        config
            .context_window
            .or_else(|| match self.get(&key_of(config)) {
                Some(WindowState::Found(n)) => Some(n),
                _ => None,
            })
    }

    /// `komo config check` 那一行。
    pub fn describe(&self, alias: &str, config: &ModelConfig) -> ModelWindow {
        let unknown = |detail: &str| ModelWindow {
            alias: alias.to_string(),
            window: None,
            source: WindowSource::Unknown,
            detail: Some(detail.to_string()),
        };
        if let Some(n) = config.context_window {
            return ModelWindow {
                alias: alias.to_string(),
                window: Some(n),
                source: WindowSource::Config,
                detail: None,
            };
        }
        match self.get(&key_of(config)) {
            Some(WindowState::Found(n)) => ModelWindow {
                alias: alias.to_string(),
                window: Some(n),
                source: WindowSource::Provider,
                detail: None,
            },
            Some(WindowState::Missing) => unknown("provider 没有报告"),
            Some(WindowState::Failed(message)) => unknown(&message),
            Some(WindowState::Probing) | None => unknown("探测中"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(window: Option<u64>) -> ModelConfig {
        ModelConfig {
            provider: "responses".into(),
            base_url: "https://x.example/v1".into(),
            model: "m".into(),
            api_key_env: "K".into(),
            auth: None,
            effort: None,
            efforts: None,
            timeout_secs: 30,
            context_window: window,
        }
    }

    #[test]
    fn config_value_beats_probed_value() {
        let cache = WindowCache::default();
        cache.finish(key_of(&model(None)), WindowState::Found(500_000));
        assert_eq!(cache.effective(&model(Some(100_000))), Some(100_000));
        assert_eq!(cache.effective(&model(None)), Some(500_000));
        assert_eq!(
            cache.describe("a", &model(Some(100_000))).source,
            WindowSource::Config
        );
        assert_eq!(
            cache.describe("a", &model(None)).source,
            WindowSource::Provider
        );
    }

    #[test]
    fn missing_failed_and_absent_leave_the_window_unknown() {
        let cache = WindowCache::default();
        assert_eq!(cache.effective(&model(None)), None);
        cache.finish(key_of(&model(None)), WindowState::Missing);
        assert_eq!(cache.effective(&model(None)), None);
        assert_eq!(
            cache.describe("a", &model(None)).detail.as_deref(),
            Some("provider 没有报告")
        );
        cache.finish(key_of(&model(None)), WindowState::Failed("超时".into()));
        assert_eq!(cache.effective(&model(None)), None);
        assert_eq!(
            cache.describe("a", &model(None)).detail.as_deref(),
            Some("超时")
        );
    }

    #[test]
    fn failed_key_is_probed_again_but_probing_and_settled_keys_are_not() {
        let cache = WindowCache::default();
        let key = key_of(&model(None));
        assert!(cache.begin(&key), "没有记录要探");
        assert!(!cache.begin(&key), "正在探不重复起");
        cache.finish(key.clone(), WindowState::Failed("x".into()));
        assert!(cache.begin(&key), "失败的重探");
        cache.finish(key.clone(), WindowState::Found(1));
        assert!(!cache.begin(&key));
        cache.finish(key.clone(), WindowState::Missing);
        assert!(!cache.begin(&key));
    }
}
