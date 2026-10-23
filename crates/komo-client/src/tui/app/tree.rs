//! Tree paths are conversation authority; the event cursor remains session-global.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use komo_kernel::fold::SurfaceMessage;
use komo_kernel::protocol::http::SessionTreeResponse;
use komo_kernel::session::{EntryKind, SessionTree};
use komo_kernel::types::ids::{EventId, Seq, ToolCallId};
use komo_kernel::types::model::Effort;
use komo_kernel::types::status::ToolCallState;
use komo_kernel::types::turn::Role;

use super::{App, Effect, blank_tool};
use crate::tui::tree_picker::{TreePicker, TreePurpose, content_text};

impl App {
    pub(super) fn refresh_tree(&mut self) -> Vec<Effect> {
        if self.session.is_none() {
            return Vec::new();
        }
        if self.tree_mutating {
            self.tree_dirty = true;
            return Vec::new();
        }
        if self.tree_fetching {
            self.tree_dirty = true;
            return Vec::new();
        }
        self.tree_fetching = true;
        vec![Effect::FetchTree {
            generation: self.tree_generation,
        }]
    }

    pub(super) fn open_tree(&mut self, purpose: TreePurpose) -> Vec<Effect> {
        if self.session.is_none() {
            self.fail("还没有会话");
            return Vec::new();
        }
        self.opening_tree = Some(purpose);
        self.refresh_tree()
    }

    pub(super) fn tree_mutation(&mut self, effect: Effect) -> Vec<Effect> {
        if self.session.is_none() {
            self.fail("还没有会话");
            return Vec::new();
        }
        if self.tree_busy
            || self.phase.is_backfilling()
            || self
                .surface
                .runs
                .values()
                .any(|run| !run.status.is_terminal())
            || self
                .pending_submissions
                .iter()
                .any(|p| !matches!(p.state, super::SubmissionState::Failed { .. }))
        {
            self.fail("有任务尚未结束或会话正在更新，稍后再操作会话树");
            return Vec::new();
        }
        self.restore_tree_controls = matches!(&effect, Effect::Navigate { .. });
        self.tree_generation += 1;
        self.tree_fetching = false;
        self.tree_dirty = false;
        self.tree_busy = true;
        self.tree_mutating = true;
        self.opening_tree = None;
        self.tree_picker = None;
        vec![effect]
    }

    pub(super) fn tree_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let picker = self.tree_picker.as_mut().expect("tree picker is open");
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.tree_picker = None;
                self.opening_tree = None;
            }
            KeyCode::Up => picker.move_selection(-1),
            KeyCode::Down => picker.move_selection(1),
            KeyCode::PageUp => picker.move_selection(-8),
            KeyCode::PageDown => picker.move_selection(8),
            KeyCode::Home => picker.selected = 0,
            KeyCode::End => picker.selected = picker.visible().len().saturating_sub(1),
            KeyCode::Left => {
                let parent = picker.selected_row().and_then(|r| r.parent.clone());
                if let Some(at) = picker
                    .visible()
                    .iter()
                    .position(|r| Some(&r.id) == parent.as_ref())
                {
                    picker.selected = at;
                }
            }
            KeyCode::Right => {
                let id = picker.selected_row().map(|r| r.id.clone());
                if let Some(at) = picker
                    .visible()
                    .iter()
                    .position(|r| r.parent.as_ref() == id.as_ref())
                {
                    picker.selected = at;
                }
            }
            KeyCode::Enter | KeyCode::Char('s' | 'f') if key.code == KeyCode::Enter || ctrl => {
                let Some(row) = picker.selected_row() else {
                    return Vec::new();
                };
                let target = row.id.clone();
                let fork = picker.purpose == TreePurpose::Fork || key.code == KeyCode::Char('f');
                if fork && !row.user {
                    self.fail("fork 请选择用户消息条目");
                    return Vec::new();
                }
                return self.tree_mutation(if fork {
                    Effect::Fork {
                        target: Some(target),
                        clone: false,
                    }
                } else {
                    Effect::Navigate {
                        target,
                        summarize: key.code == KeyCode::Char('s'),
                    }
                });
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
            }
            KeyCode::Char(ch) if !ctrl => {
                picker.query.push(ch);
                picker.selected = 0;
            }
            _ => {}
        }
        Vec::new()
    }

    pub(super) fn receive_tree(
        &mut self,
        response: SessionTreeResponse,
        generation: u64,
    ) -> Vec<Effect> {
        if generation != self.tree_generation {
            return Vec::new();
        }
        self.tree_fetching = false;
        let changed = self.tree.as_ref() != Some(&response);
        if changed || self.tree_busy || self.tree_projection_dirty {
            if let Err(error) = self.rebuild_branch(&response) {
                self.fail(format!("会话树无效：{error}"));
                self.tree_busy = false;
                return Vec::new();
            }
            self.tree = Some(response.clone());
            self.tree_projection_dirty = false;
            self.restore_tree_controls = false;
            self.repaint = true;
        }
        self.tree_busy = false;
        // Automatic refreshes never open a picker or take the draft's focus.
        if let Some(purpose) = self.opening_tree.take() {
            self.tree_picker = Some(TreePicker::new(
                &response.entries,
                response.leaf_id.clone(),
                purpose,
            ));
        } else if let Some(old) = self.tree_picker.take() {
            let selected = old.selected_row().map(|r| r.id.clone());
            let mut picker =
                TreePicker::new(&response.entries, response.leaf_id.clone(), old.purpose);
            picker.query = old.query;
            picker.selected = picker
                .visible()
                .iter()
                .position(|r| Some(&r.id) == selected.as_ref())
                .unwrap_or(0);
            self.tree_picker = Some(picker);
        }
        if std::mem::take(&mut self.tree_dirty) {
            self.refresh_tree()
        } else {
            Vec::new()
        }
    }

    pub(super) fn rebuild_branch(&mut self, response: &SessionTreeResponse) -> Result<(), String> {
        let mut tree = SessionTree::from_entries(response.entries.clone())?;
        tree.branch(response.leaf_id.as_deref())?;
        // None is an explicit empty path, rather than the last entry in the file.
        let path = tree.path(tree.leaf_id())?;
        let mut messages = Vec::new();
        let mut tools = std::collections::BTreeMap::new();
        let mut model = None;
        let mut effort = None;
        let name = tree.name().map(str::to_string);
        for (index, entry) in path.iter().enumerate() {
            // These are display positions only, never SSE cursors.
            let seq = Seq(index as u64 + 1);
            match &entry.kind {
                EntryKind::Message { message } => {
                    let role = match message["role"].as_str() {
                        Some("user") => Role::User,
                        Some("assistant") => Role::Assistant,
                        Some("toolResult") => {
                            if let Some(id) = message["toolCallId"].as_str() {
                                let call = self
                                    .provider_calls
                                    .get(id)
                                    .cloned()
                                    .unwrap_or_else(|| ToolCallId::from_raw(id));
                                let tool = tools
                                    .entry(call.clone())
                                    .or_insert_with(|| blank_tool(call, None, seq));
                                tool.state = self
                                    .surface
                                    .calls
                                    .get(&tool.call)
                                    .map(|call| call.state)
                                    .filter(|state| state.is_terminal())
                                    .unwrap_or_else(|| {
                                        if message["isError"].as_bool().unwrap_or(false) {
                                            ToolCallState::Failed
                                        } else {
                                            ToolCallState::Completed
                                        }
                                    });
                                tool.preview = Some(content_text(&message["content"]));
                                if let Some(tool_name) = message["toolName"].as_str() {
                                    tool.tool = tool_name.into();
                                }
                            }
                            continue;
                        }
                        _ => continue, // system checkpoints are context, not spoken history.
                    };
                    let text = content_text(&message["content"]);
                    let mut displayed = display_message(&entry.id, seq, role, text);
                    displayed.run = self.entry_runs.get(&entry.id).cloned();
                    messages.push(displayed);
                    if role == Role::Assistant {
                        for block in message["content"].as_array().into_iter().flatten() {
                            if block["type"] != "toolCall" {
                                continue;
                            }
                            let Some(id) = block["id"].as_str() else {
                                continue;
                            };
                            let call = self
                                .provider_calls
                                .get(id)
                                .cloned()
                                .unwrap_or_else(|| ToolCallId::from_raw(id));
                            let mut tool = self.tools.get(&call).cloned().unwrap_or_else(|| {
                                blank_tool(
                                    call.clone(),
                                    self.entry_runs.get(&entry.id).cloned(),
                                    seq,
                                )
                            });
                            tool.seq = seq;
                            tool.tool = block["name"].as_str().unwrap_or("?").into();
                            tool.args = block["arguments"].clone();
                            tools.insert(call, tool);
                        }
                    }
                }
                EntryKind::CustomMessage {
                    custom_type,
                    content,
                    display,
                    ..
                } if *display => messages.push(display_message(
                    &entry.id,
                    seq,
                    Role::Assistant,
                    format!("[{custom_type}] {}", content_text(content)),
                )),
                EntryKind::BranchSummary { summary, .. } => messages.push(display_message(
                    &entry.id,
                    seq,
                    Role::Assistant,
                    format!("[分支摘要] {summary}"),
                )),
                EntryKind::Compaction { summary, .. } => messages.push(display_message(
                    &entry.id,
                    seq,
                    Role::Assistant,
                    format!("[压缩摘要] {summary}"),
                )),
                EntryKind::ModelChange { provider, model_id } => {
                    model = Some((provider.clone(), model_id.clone()))
                }
                EntryKind::ThinkingLevelChange { thinking_level } => {
                    effort = Some(Effort::new(thinking_level))
                }
                _ => {}
            }
        }
        // Restore a server alias only if the menu recognizes it. Provider-native IDs
        // need not be accepted as aliases by SubmitRunRequest.
        if self.restore_tree_controls || self.tree.is_none() {
            if let Some((provider, model)) = model {
                let matched = self.model_menu.iter().find(|m| m.id == model).or_else(|| {
                    self.model_menu
                        .iter()
                        .find(|m| m.model == model && m.provider == provider)
                });
                if let Some(alias) = matched {
                    self.model = Some(alias.id.clone());
                    self.effort = effort;
                }
            } else {
                self.model = None;
                self.effort = effort;
            }
        }
        for tool in tools.values_mut() {
            if tool.state == ToolCallState::Planned
                && !tool
                    .run
                    .as_ref()
                    .and_then(|run| self.surface.runs.get(run))
                    .is_some_and(|run| !run.status.is_terminal())
            {
                tool.state = ToolCallState::Uncertain;
            }
        }
        self.session_name = name;
        self.branch_messages = Some(messages);
        self.tools = tools;
        Ok(())
    }
}

fn display_message(id: &str, seq: Seq, role: Role, text: String) -> SurfaceMessage {
    SurfaceMessage {
        seq,
        event_id: EventId::from_raw(id),
        role,
        run: None,
        text: Some(text),
        text_ref: None,
        tool_calls: Vec::new(),
        tool_results: Vec::new(),
        provider_blocks: None,
        absorbed_from: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{ServerEvent, TuiMode};
    use crate::tui::paste::InputEvent;
    use crate::tui::test_support as fixture;
    use komo_kernel::protocol::http::NavigateSessionResponse;
    use komo_kernel::protocol::sse::{SseEvent, SseFrame};
    use komo_kernel::session::SessionEntry;
    use serde_json::json;

    fn entry(id: &str, parent: Option<&str>, kind: EntryKind) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            timestamp: "2026-10-23T00:00:00Z".into(),
            kind,
        }
    }
    fn message(id: &str, parent: Option<&str>, role: &str, text: &str) -> SessionEntry {
        entry(
            id,
            parent,
            EntryKind::Message {
                message: json!({"role":role,"content":text}),
            },
        )
    }
    fn tree(leaf: Option<&str>) -> SessionTreeResponse {
        SessionTreeResponse {
            entries: vec![
                message("u1", None, "user", "root question"),
                message("a1", Some("u1"), "assistant", "first answer"),
                message("u2", Some("a1"), "user", "old branch question"),
                message("a2", Some("u2"), "assistant", "old branch answer"),
                message("u3", Some("a1"), "user", "new branch question"),
                message("a3", Some("u3"), "assistant", "new branch answer"),
            ],
            leaf_id: leaf.map(str::to_string),
        }
    }
    fn app() -> App {
        App::new(Some(fixture::session()), TuiMode::New, "tree-tests")
    }
    fn receive(app: &mut App, tree: SessionTreeResponse) -> Vec<Effect> {
        app.apply(ServerEvent::Tree {
            response: Box::new(tree),
            generation: app.tree_generation,
        })
    }
    fn command(app: &mut App, text: &str) -> Vec<Effect> {
        app.input.set(text.to_string());
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    #[test]
    fn switching_paths_replaces_transcript_and_draft_without_rewinding_stream_cursor() {
        let mut app = app();
        app.cursor = Seq(420);
        receive(&mut app, tree(Some("a2")));
        let mut emitted = crate::tui::transcript::Emitted::new();
        let old = format!("{:?}", emitted.take(&app, 100));
        assert!(old.contains("old branch answer"));
        assert!(!old.contains("new branch answer"));
        app.repaint = false;
        assert_eq!(
            command(&mut app, "/tree u3"),
            vec![Effect::Navigate {
                target: "u3".into(),
                summarize: false
            }]
        );
        app.input.set("discard this draft");
        assert!(
            app.apply(ServerEvent::TreeChanged(Box::new(
                NavigateSessionResponse {
                    leaf_id: Some("a1".into()),
                    editor_text: Some("new branch question".into()),
                    cancelled: false
                }
            )))
            .iter()
            .any(|e| matches!(e, Effect::FetchTree { .. }))
        );
        receive(&mut app, tree(Some("a1")));
        assert_eq!(app.cursor, Seq(420));
        assert_eq!(app.input.text(), "new branch question");
        assert_eq!(app.messages().len(), 2);
        assert!(
            app.repaint,
            "driver must erase siblings already in terminal scrollback"
        );
        let redrawn = format!(
            "{:?}",
            crate::tui::transcript::Emitted::new().take(&app, 100)
        );
        assert!(redrawn.contains("first answer"));
        assert!(!redrawn.contains("old branch"));
        assert!(!redrawn.contains("new branch"));
        receive(&mut app, tree(None));
        assert!(
            app.messages().is_empty(),
            "null leaf means before all roots"
        );
    }

    #[test]
    fn context_edits_preserve_original_history_and_tool_results_follow_the_path() {
        let mut app = app();
        app.provider_calls
            .insert("tool-1".into(), ToolCallId::from_raw("internal-1"));
        let mut response = tree(Some("edit"));
        response.entries.extend([
            entry("tools", Some("a3"), EntryKind::Message { message: json!({"role":"assistant","content":[{"type":"text","text":"checking"},{"type":"toolCall","id":"tool-1","name":"read","arguments":{"path":"a.rs"}}]}) }),
            entry("result", Some("tools"), EntryKind::Message { message: json!({"role":"toolResult","toolCallId":"tool-1","toolName":"read","content":[{"type":"text","text":"file contents"}],"isError":false}) }),
            entry("edit", Some("result"), EntryKind::ContextEdit { target_id: "u3".into(), replacement: None }),
        ]);
        receive(&mut app, response.clone());
        assert!(
            app.messages()
                .iter()
                .any(|m| m.text.as_deref() == Some("new branch question"))
        );
        let tool = app.tool(&ToolCallId::from_raw("internal-1")).unwrap();
        assert_eq!(tool.tool, "read");
        assert_eq!(tool.state, ToolCallState::Completed);
        assert_eq!(tool.preview.as_deref(), Some("file contents"));
        response.leaf_id = Some("a2".into());
        receive(&mut app, response);
        assert!(app.tool_lines().is_empty());
    }

    #[test]
    fn navigation_restores_model_alias_and_effort_but_unknown_model_preserves_controls() {
        let mut app = app();
        app.model_menu = vec![serde_json::from_value(json!({"id":"alias-a","model":"native-a","provider":"provider-a","efforts":["high"]})).unwrap()];
        let mut response = tree(Some("thinking"));
        response.entries.extend([
            entry(
                "model",
                Some("a1"),
                EntryKind::ModelChange {
                    provider: "provider-a".into(),
                    model_id: "native-a".into(),
                },
            ),
            entry(
                "thinking",
                Some("model"),
                EntryKind::ThinkingLevelChange {
                    thinking_level: "high".into(),
                },
            ),
        ]);
        receive(&mut app, response.clone());
        assert_eq!(app.model.as_deref(), Some("alias-a"));
        assert_eq!(app.effort.as_ref().map(Effort::as_str), Some("high"));
        app.model = Some("manual-alias".into());
        response.entries.push(entry(
            "unknown",
            Some("thinking"),
            EntryKind::ModelChange {
                provider: "other".into(),
                model_id: "unknown".into(),
            },
        ));
        response.leaf_id = Some("unknown".into());
        app.tree_busy = true;
        app.restore_tree_controls = true;
        receive(&mut app, response);
        assert_eq!(app.model.as_deref(), Some("manual-alias"));
        assert_eq!(app.effort.as_ref().map(Effort::as_str), Some("high"));
    }

    #[test]
    fn finalized_events_coalesce_tree_fetches_tokens_do_not_fetch_or_steal_focus() {
        let mut app = app();
        receive(&mut app, tree(Some("a1")));
        app.input.set("draft stays here");
        let delta = ServerEvent::Frame(Box::new(SseFrame {
            id: Seq(10),
            session: fixture::session(),
            event: SseEvent::AssistantDelta {
                run: fixture::run(),
                round: 0,
                text: "stream".into(),
            },
        }));
        assert!(app.apply(delta).is_empty());
        let event = fixture::conversation().into_iter().last().unwrap();
        let frame = || {
            ServerEvent::Frame(Box::new(SseFrame {
                id: Seq(20),
                session: fixture::session(),
                event: SseEvent::Event(Box::new(event.clone())),
            }))
        };
        assert!(matches!(
            app.apply(frame()).as_slice(),
            [Effect::FetchTree { .. }]
        ));
        assert!(app.apply(frame()).is_empty());
        assert!(matches!(
            receive(&mut app, tree(Some("a1"))).as_slice(),
            [Effect::FetchTree { .. }]
        ));
        assert!(receive(&mut app, tree(Some("a1"))).is_empty());
        assert!(app.tree_picker.is_none());
        assert_eq!(app.input.text(), "draft stays here");
    }

    #[test]
    fn closing_picker_does_not_cancel_run_and_stale_tree_cannot_undo_navigation() {
        let mut app = app();
        receive(&mut app, tree(Some("a2")));
        app.input.set("saved draft");
        app.tree_picker = Some(TreePicker::new(
            &tree(Some("a2")).entries,
            Some("a2".into()),
            TreePurpose::Navigate,
        ));
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .is_empty()
        );
        assert!(app.tree_picker.is_none());
        assert_eq!(app.input.text(), "saved draft");
        let old_generation = app.tree_generation;
        command(&mut app, "/tree a3 --summarize");
        app.apply(ServerEvent::Tree {
            response: Box::new(tree(Some("a2"))),
            generation: old_generation,
        });
        assert!(app.tree_busy);
        app.apply(ServerEvent::TreeChanged(Box::new(
            NavigateSessionResponse {
                leaf_id: Some("a3".into()),
                editor_text: None,
                cancelled: false,
            },
        )));
        receive(&mut app, tree(Some("a3")));
        assert!(
            app.messages()
                .last()
                .unwrap()
                .text
                .as_deref()
                .unwrap()
                .contains("new branch answer")
        );
        app.handle_input(InputEvent::Paste("more".into()));
        assert_eq!(app.input.text(), "more");
    }

    #[test]
    fn metadata_refresh_keeps_manual_model_and_name_is_session_wide() {
        let mut app = app();
        let mut response = tree(Some("a1"));
        response.entries.push(entry(
            "session-name",
            Some("a3"),
            EntryKind::SessionInfo {
                name: "investigation".into(),
            },
        ));
        receive(&mut app, response.clone());
        assert_eq!(app.session_name.as_deref(), Some("investigation"));
        app.model = Some("manual-alias".into());
        assert_eq!(
            command(&mut app, "/label a1 decision"),
            vec![Effect::Label {
                target: "a1".into(),
                label: Some("decision".into())
            }]
        );
        app.apply(ServerEvent::TreeUpdated);
        response.entries.push(entry(
            "label",
            Some("a1"),
            EntryKind::Label {
                target_id: "a1".into(),
                label: Some("decision".into()),
            },
        ));
        response.leaf_id = Some("label".into());
        receive(&mut app, response);
        assert_eq!(app.model.as_deref(), Some("manual-alias"));
        assert_eq!(app.messages().len(), 2);
    }

    #[test]
    fn escape_while_loading_tree_prevents_late_focus_theft_and_busy_run_refuses_navigation() {
        let mut app = app();
        let effects = command(&mut app, "/tree");
        assert!(matches!(effects.as_slice(), [Effect::FetchTree { .. }]));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.input.set("keep typing");
        receive(&mut app, tree(Some("a1")));
        assert!(app.tree_picker.is_none());
        assert_eq!(app.input.text(), "keep typing");
        let accepted = fixture::conversation().remove(0);
        app.apply(ServerEvent::Frame(Box::new(SseFrame {
            id: accepted.seq,
            session: fixture::session(),
            event: SseEvent::Event(Box::new(accepted)),
        })));
        assert!(command(&mut app, "/tree a3").is_empty());
        assert!(app.notices.last().unwrap().is_error);
    }

    #[test]
    fn mutation_failure_preserves_draft_and_allows_retry_and_reconnect_refreshes_tree() {
        let mut app = app();
        command(&mut app, "/clone");
        app.input.set("preserve");
        app.apply(ServerEvent::TreeMutationFailed(
            "conflict: unfinished run".into(),
        ));
        assert!(app.input_enabled());
        assert_eq!(app.input.text(), "preserve");
        assert!(app.notices.last().unwrap().is_error);
        app.connection = crate::sse::ConnectionState::Reconnecting {
            attempt: 1,
            reason: "offline".into(),
        };
        let effects = app.apply(ServerEvent::Connection(
            crate::sse::ConnectionState::Connected,
        ));
        assert!(effects.contains(&Effect::FetchPending));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::FetchTree { .. }))
        );
    }
}
