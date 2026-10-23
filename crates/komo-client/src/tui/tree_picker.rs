//! Session tree browsing: depth-first rows keep parents and siblings visible.
use std::collections::{BTreeMap, BTreeSet};

use komo_kernel::session::{EntryKind, SessionEntry};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreePurpose {
    Navigate,
    Fork,
}

#[derive(Debug, Clone)]
pub struct TreeRow {
    pub id: String,
    pub parent: Option<String>,
    pub depth: usize,
    pub title: String,
    pub user: bool,
}

#[derive(Debug, Clone)]
pub struct TreePicker {
    pub rows: Vec<TreeRow>,
    pub selected: usize,
    pub query: String,
    pub active: Option<String>,
    pub purpose: TreePurpose,
}

pub fn content_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(|v| v.as_str()) {
                Some("text") => block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                Some("image") => Some("[image]".into()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl TreePicker {
    pub fn new(entries: &[SessionEntry], active: Option<String>, purpose: TreePurpose) -> Self {
        let mut labels = BTreeMap::new();
        for entry in entries {
            if let EntryKind::Label { target_id, label } = &entry.kind {
                labels.insert(target_id.as_str(), label.as_deref());
            }
        }
        let mut children: BTreeMap<Option<&str>, Vec<&SessionEntry>> = BTreeMap::new();
        for entry in entries {
            children
                .entry(entry.parent_id.as_deref())
                .or_default()
                .push(entry);
        }
        let mut rows = Vec::new();
        let mut stack: Vec<_> = children
            .get(&None)
            .into_iter()
            .flatten()
            .rev()
            .map(|e| (*e, 0))
            .collect();
        let mut seen = BTreeSet::new();
        while let Some((entry, depth)) = stack.pop() {
            if !seen.insert(&entry.id) {
                continue;
            }
            let (title, user) = match &entry.kind {
                EntryKind::Message { message } => (
                    format!(
                        "{}: {}",
                        message["role"].as_str().unwrap_or("message"),
                        content_text(&message["content"])
                    ),
                    message["role"] == "user",
                ),
                EntryKind::CustomMessage {
                    custom_type,
                    content,
                    ..
                } => (format!("{custom_type}: {}", content_text(content)), false),
                EntryKind::ModelChange { model_id, .. } => (format!("model: {model_id}"), false),
                EntryKind::ThinkingLevelChange { thinking_level } => {
                    (format!("effort: {thinking_level}"), false)
                }
                EntryKind::Compaction { summary, .. } => (format!("compaction: {summary}"), false),
                EntryKind::BranchSummary { summary, .. } => {
                    (format!("branch summary: {summary}"), false)
                }
                EntryKind::SessionInfo { name } => (format!("name: {name}"), false),
                EntryKind::Label { target_id, label } => (
                    format!(
                        "label {target_id}: {}",
                        label.as_deref().unwrap_or("(cleared)")
                    ),
                    false,
                ),
                EntryKind::ContextEdit { target_id, .. } => {
                    (format!("context edit: {target_id}"), false)
                }
                EntryKind::Custom { custom_type, .. } => (custom_type.clone(), false),
                EntryKind::Usage { .. } => ("usage".into(), false),
            };
            let label = labels
                .get(entry.id.as_str())
                .copied()
                .flatten()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            rows.push(TreeRow {
                id: entry.id.clone(),
                parent: entry.parent_id.clone(),
                depth,
                title: format!(
                    "{}{label}",
                    title.split_whitespace().collect::<Vec<_>>().join(" ")
                ),
                user,
            });
            if let Some(kids) = children.get(&Some(entry.id.as_str())) {
                stack.extend(kids.iter().rev().map(|e| (*e, depth + 1)));
            }
        }
        let mut picker = Self {
            rows,
            selected: 0,
            query: String::new(),
            active,
            purpose,
        };
        picker.selected = picker
            .visible()
            .iter()
            .position(|row| Some(&row.id) == picker.active.as_ref())
            .unwrap_or(0);
        picker
    }

    pub fn visible(&self) -> Vec<&TreeRow> {
        if self.query.is_empty() && self.purpose == TreePurpose::Navigate {
            return self.rows.iter().collect();
        }
        let query = self.query.to_lowercase();
        // Keep ancestors of matches so filtering still shows the real tree.
        let mut keep: BTreeSet<&str> = BTreeSet::new();
        let by_id: BTreeMap<_, _> = self.rows.iter().map(|r| (r.id.as_str(), r)).collect();
        for row in &self.rows {
            if self.purpose == TreePurpose::Fork && !row.user {
                continue;
            }
            if query.is_empty()
                || row.title.to_lowercase().contains(&query)
                || row.id.to_lowercase().contains(&query)
            {
                let mut next = Some(row.id.as_str());
                while let Some(id) = next {
                    if !keep.insert(id) {
                        break;
                    }
                    next = by_id.get(id).and_then(|r| r.parent.as_deref());
                }
            }
        }
        self.rows
            .iter()
            .filter(|r| keep.contains(r.id.as_str()))
            .collect()
    }

    pub fn selected_row(&self) -> Option<&TreeRow> {
        self.visible().get(self.selected).copied()
    }

    pub fn move_selection(&mut self, delta: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.visible().len().saturating_sub(1));
    }

    pub fn lines(&self, height: usize) -> Vec<Line<'static>> {
        let visible = self.visible();
        if visible.is_empty() {
            return vec![Line::from("  没有匹配的条目")];
        }
        let start = (self.selected + 1).saturating_sub(height.max(1));
        visible
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(index, row)| {
                let active = self.active.as_ref() == Some(&row.id);
                let style = if index == self.selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else if active {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(
                    format!(
                        "{} {}{} {} {}",
                        if index == self.selected { "›" } else { " " },
                        "│ ".repeat(row.depth.min(20)),
                        if row.parent.is_some() {
                            "├─"
                        } else {
                            "●"
                        },
                        if active { "*" } else { " " },
                        format_args!("{} {}", row.id, row.title)
                    ),
                    style,
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn entry(id: &str, parent: Option<&str>, text: &str) -> SessionEntry {
        serde_json::from_value(json!({"id":id,"parentId":parent,"timestamp":"now","type":"message","message":{"role":"user","content":text}})).unwrap()
    }
    #[test]
    fn browsing_and_search_keep_real_parent_child_structure_and_active_leaf() {
        let entries = vec![
            entry("root", None, "root"),
            entry("left", Some("root"), "left"),
            entry("right", Some("root"), "needle"),
            entry("left-child", Some("left"), "child"),
        ];
        let mut picker = TreePicker::new(&entries, Some("right".into()), TreePurpose::Navigate);
        assert_eq!(
            picker
                .rows
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["root", "left", "left-child", "right"]
        );
        assert_eq!(picker.rows[2].depth, 2);
        assert_eq!(picker.selected_row().unwrap().id, "right");
        picker.query = "needle".into();
        picker.selected = 1;
        assert_eq!(
            picker
                .visible()
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["root", "right"]
        );
        assert!(format!("{:?}", picker.lines(8)).contains('*'));
        picker.move_selection(99);
        assert_eq!(picker.selected, 1);
        picker.move_selection(-99);
        assert_eq!(picker.selected, 0);
    }
}
