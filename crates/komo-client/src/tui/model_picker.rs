//! `/model` 的选择菜单：`↑` / `↓` 移动高亮、`Enter` 选定、`Esc` 不改。
//!
//! 清单是 `/model` 那一下**去问来的**那一份（`GET /v1/models`），打开时抄一份进来：
//! 人正在挑的时候清单在底下换了，高亮就会悄悄指到另一个模型上。行首的数字是直通键，
//! 和审批菜单行首的字母一个意思——知道自己要第几个的人不必先移动高亮。

use komo_kernel::protocol::http::ModelMenuEntry;

/// 菜单自己的状态：打开那一刻的清单、一个高亮位置。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelPicker {
    pub entries: Vec<ModelMenuEntry>,
    /// 高亮那一行，`entries` 的下标。
    pub selected: usize,
}

impl ModelPicker {
    /// 高亮先落在 `current` 上（`/model` 设过的那个，没设过就是网关标了 default 的那个）：
    /// 打开菜单再按 `Enter` 应该是"什么都没变"，而不是换到第一行。
    pub fn new(entries: Vec<ModelMenuEntry>, current: Option<&str>) -> Self {
        let selected = current
            .and_then(|id| entries.iter().position(|entry| entry.id == id))
            .or_else(|| entries.iter().position(|entry| entry.default))
            .unwrap_or(0);
        ModelPicker { entries, selected }
    }

    /// 移动高亮。到两头就停住，**不绕回去**（与审批菜单同一条规矩）。
    pub fn move_selection(&mut self, delta: i16) {
        let moved = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected.saturating_add(delta as usize)
        };
        self.selected = moved.min(self.entries.len().saturating_sub(1));
    }

    pub fn selected_entry(&self) -> Option<&ModelMenuEntry> {
        self.entries.get(self.selected)
    }

    /// 第 `index` 行（0 起）的直通键：前九行是 `1`–`9`，再往后没有。
    pub fn shortcut(index: usize) -> Option<char> {
        (index < 9).then(|| char::from(b'1' + index as u8))
    }

    /// 直通键对应哪一行。
    pub fn index_of_shortcut(&self, key: char) -> Option<usize> {
        let index = key.to_digit(10)?.checked_sub(1)? as usize;
        (index < self.entries.len().min(9)).then_some(index)
    }

    pub fn keys_hint(&self) -> &'static str {
        "↑/↓ 选择 · Enter 确认 · Esc 不改"
    }
}

/// 一行的说明：显示名（与 alias 不同时）、上游 model、provider、effort 档位。
///
/// alias 是提交时用的那个名字，排在行首另印；这里是"它到底是什么"——同一个上游模型
/// 挂在两个 provider 底下时，只看 alias 分不出来。
pub fn entry_detail(entry: &ModelMenuEntry) -> String {
    let mut parts = Vec::new();
    if !entry.name.is_empty() && entry.name != entry.id {
        parts.push(entry.name.clone());
    }
    if !entry.model.is_empty() && entry.model != entry.id {
        parts.push(entry.model.clone());
    }
    if !entry.provider.is_empty() {
        parts.push(entry.provider.clone());
    }
    if entry.efforts.is_empty() {
        parts.push("无 effort".to_string());
    } else {
        let efforts: Vec<String> = entry.efforts.iter().map(|e| e.to_string()).collect();
        parts.push(format!("effort {}", efforts.join("/")));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, default: bool) -> ModelMenuEntry {
        ModelMenuEntry {
            id: id.into(),
            default,
            ..ModelMenuEntry::default()
        }
    }

    #[test]
    fn the_highlight_starts_on_the_current_model_then_the_default() {
        let entries = vec![entry("a", false), entry("b", true), entry("c", false)];
        assert_eq!(ModelPicker::new(entries.clone(), Some("c")).selected, 2);
        assert_eq!(ModelPicker::new(entries.clone(), None).selected, 1);
        // 设过的模型不在清单里了：退到 default，不是停在一个不存在的下标上。
        assert_eq!(ModelPicker::new(entries, Some("gone")).selected, 1);
    }

    #[test]
    fn moving_stops_at_both_ends() {
        let mut picker = ModelPicker::new(vec![entry("a", false), entry("b", false)], None);
        picker.move_selection(-1);
        assert_eq!(picker.selected, 0);
        picker.move_selection(5);
        assert_eq!(picker.selected, 1);
    }

    #[test]
    fn digit_shortcuts_cover_only_rows_that_exist() {
        let picker = ModelPicker::new(vec![entry("a", false), entry("b", false)], None);
        assert_eq!(picker.index_of_shortcut('2'), Some(1));
        assert_eq!(picker.index_of_shortcut('3'), None);
        assert_eq!(picker.index_of_shortcut('0'), None);
        assert_eq!(ModelPicker::shortcut(8), Some('9'));
        assert_eq!(ModelPicker::shortcut(9), None);
    }
}
