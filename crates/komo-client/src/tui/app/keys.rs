//! 按键与命令：状态机的输入一侧（[`super::App`] 的方法，拆出来只是为了文件不过长）。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use komo_kernel::types::chat::ApprovalScope;

use super::{App, Effect};
use crate::tui::approval::ApprovalAnswer;
use crate::tui::command::{self, Command};
use crate::tui::paste::InputEvent;

impl App {
    /// 一个输入事件（按键或一次粘贴）。
    pub fn handle_input(&mut self, event: InputEvent) -> Vec<Effect> {
        match event {
            InputEvent::Key(key) => self.handle_key(key),
            InputEvent::Paste(text) => {
                if self.input_enabled() {
                    self.input.paste(&text);
                }
                Vec::new()
            }
        }
    }

    /// 一次按键。
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        // 终端会同时报 Press 与 Release；只认按下，否则每个字符进两次。
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if self.approval.is_some() {
            return self.approval_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.quit = true;
                return vec![Effect::Quit];
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => {
                self.quit = true;
                return vec![Effect::Quit];
            }
            // 展开 / 收起工具调用的完整参数与结果预览。
            KeyCode::Char('t') if ctrl => {
                self.toggle_all_tools();
                return Vec::new();
            }
            // Ctrl-J、Shift-Enter、Alt-Enter 都是换行（后两者要终端开着 kitty 协议）。
            KeyCode::Char('j') if ctrl => {
                if self.input_enabled() {
                    self.input.insert_char('\n');
                }
                return Vec::new();
            }
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                if self.input_enabled() {
                    self.input.insert_char('\n');
                }
                return Vec::new();
            }
            KeyCode::Enter => return self.submit(),
            KeyCode::Esc => return self.escape(),
            KeyCode::Backspace => {
                if self.input_enabled() {
                    self.input.backspace();
                    self.reset_history_browse();
                }
                return Vec::new();
            }
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            KeyCode::Up => self.history_back(),
            KeyCode::Down => self.history_forward(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(5),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(5),
            KeyCode::Tab => {
                if let Some(completed) = command::complete(self.input.text()) {
                    self.input.set(completed);
                }
            }
            KeyCode::Char(ch) if self.input_enabled() => {
                self.input.insert_char(ch);
                self.reset_history_browse();
            }
            _ => {}
        }
        Vec::new()
    }

    /// **Esc 的两个意思**：Run 在跑时取消它；空闲时什么都不做——一个有时会清掉草稿的
    /// 停止键，比多按一下还糟。
    fn escape(&mut self) -> Vec<Effect> {
        match (self.current_run.clone(), self.run_status()) {
            (Some(run), Some(status)) if !status.is_terminal() => {
                let request_key = self.next_key("cancel");
                self.note(format!("已请求取消 {run}"));
                vec![Effect::Cancel { run, request_key }]
            }
            _ => Vec::new(),
        }
    }

    fn submit(&mut self) -> Vec<Effect> {
        if self.phase.is_backfilling() {
            self.fail("正在补读历史，读完再发");
            return Vec::new();
        }
        let text = self.input.text().trim().to_string();
        if text.is_empty() {
            return Vec::new();
        }
        self.history.push(text.clone());
        self.reset_history_browse();

        if command::is_command(&text) {
            self.input.clear();
            return self.run_command(&text);
        }

        let body = self.input.take();
        let request_key = self.next_key("run");
        self.scroll = 0;
        vec![Effect::Submit {
            request_key,
            text: body,
            model: self.model.clone(),
            effort: self.effort.clone(),
        }]
    }

    fn run_command(&mut self, text: &str) -> Vec<Effect> {
        let command = match command::parse(text, &self.model_menu) {
            Ok(command) => command,
            Err(error) => {
                self.fail(error.to_string());
                return Vec::new();
            }
        };
        match command {
            Command::New => {
                self.note("已划一条回放边界");
                vec![Effect::Boundary]
            }
            Command::Cancel => match (self.current_run.clone(), self.run_status()) {
                (Some(run), Some(status)) if !status.is_terminal() => {
                    let request_key = self.next_key("cancel");
                    vec![Effect::Cancel { run, request_key }]
                }
                _ => {
                    self.fail("现在没有在跑的 Run");
                    Vec::new()
                }
            },
            Command::Status => vec![Effect::FetchStatus],
            Command::Pending => vec![Effect::FetchPending],
            Command::Approve { short_id, scope } => self.decide_by_short_id(short_id, true, scope),
            Command::Reject { short_id } => {
                self.decide_by_short_id(short_id, false, ApprovalScope::Once)
            }
            Command::Model { id } => {
                match id {
                    Some(id) => {
                        self.note(format!("下一个 Run 用模型 {id}"));
                        self.model = Some(id);
                    }
                    None => self.note(self.model_options()),
                }
                Vec::new()
            }
            Command::Effort { level } => {
                match level {
                    Some(level) => {
                        self.note(format!("下一个 Run 的 effort = {level}"));
                        self.effort = Some(level);
                    }
                    None => self.note(format!(
                        "可选 effort：{}（当前 {}）",
                        command::EFFORT_LEVELS.join(" · "),
                        self.effort
                            .as_ref()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "跟随 Gateway".into())
                    )),
                }
                Vec::new()
            }
            Command::Help => {
                for (name, blurb) in command::COMMANDS {
                    self.note(format!("{name}  {blurb}"));
                }
                Vec::new()
            }
            Command::Quit => {
                self.quit = true;
                vec![Effect::Quit]
            }
        }
    }

    fn model_options(&self) -> String {
        let current = self
            .model
            .clone()
            .unwrap_or_else(|| "跟随 Gateway".to_string());
        if self.model_menu.is_empty() {
            // TODO(decide: §13.1 没有「列出可选模型」的接口，所以清单可能是空的。空清单
            // 时 `/model x` 不拦——拦一个自己也不知道对不对的值只会挡住人。)
            format!("当前模型：{current}。Gateway 没有提供模型清单，`/model <id>` 直接设定")
        } else {
            format!(
                "可选模型：{}（当前 {current}）",
                self.model_menu.join(" · ")
            )
        }
    }

    /// `/approve` / `/reject`：无 ID 时只有**恰好一个**待处理请求才生效（§11.3）。
    fn decide_by_short_id(
        &mut self,
        short_id: Option<komo_kernel::types::ids::ShortId>,
        approved: bool,
        scope: ApprovalScope,
    ) -> Vec<Effect> {
        let target = match short_id {
            Some(short_id) => self
                .surface
                .pending_approvals
                .values()
                .find(|view| view.short_id == short_id)
                .map(|view| view.approval.clone()),
            None => {
                let mut pending = self.surface.pending_approvals.values();
                match (pending.next(), pending.next()) {
                    (Some(only), None) => Some(only.approval.clone()),
                    (None, _) => {
                        self.fail("没有待处理的审批");
                        return Vec::new();
                    }
                    (Some(_), Some(_)) => {
                        let ids: Vec<String> = self
                            .surface
                            .pending_approvals
                            .values()
                            .map(|view| view.short_id.to_string())
                            .collect();
                        self.fail(format!("有多条待处理：{}。请指明短 ID", ids.join(" · ")));
                        return Vec::new();
                    }
                }
            }
        };
        match target {
            Some(approval) => {
                let request_key = self.next_key("decision");
                vec![Effect::Decide {
                    approval,
                    approved,
                    scope,
                    request_key,
                }]
            }
            None => {
                self.fail("没有这个短 ID 的待处理审批");
                Vec::new()
            }
        }
    }

    fn approval_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(modal) = self.approval.as_mut() else {
            return Vec::new();
        };
        let answer = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ApprovalAnswer::ONCE),
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if modal.allows_run_scope() {
                    Some(ApprovalAnswer::RUN)
                } else {
                    self.fail("这条请求不可范围化，只能批本次（y）或拒绝（n）");
                    return Vec::new();
                }
            }
            // **Esc 在弹窗里就是拒绝**，和 n 一样：一个没答案的审批会一直占着 Run。
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(ApprovalAnswer::REJECT),
            KeyCode::PageUp | KeyCode::Up => {
                modal.scroll_up();
                None
            }
            KeyCode::PageDown | KeyCode::Down => {
                modal.scroll_down();
                None
            }
            _ => None,
        };
        let Some(answer) = answer else {
            return Vec::new();
        };
        if modal.answering {
            return Vec::new();
        }
        modal.answering = true;
        let approval = modal.record.approval.clone();
        let request_key = self.next_key("decision");
        vec![Effect::Decide {
            approval,
            approved: answer.approved,
            scope: answer.scope,
            request_key,
        }]
    }

    fn history_back(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_pos {
            None => {
                self.history_draft = Some(self.input.text().to_string());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(position) => position - 1,
        };
        self.history_pos = Some(next);
        self.input.set(self.history[next].clone());
    }

    fn history_forward(&mut self) {
        let Some(position) = self.history_pos else {
            return;
        };
        if position + 1 < self.history.len() {
            self.history_pos = Some(position + 1);
            self.input.set(self.history[position + 1].clone());
        } else {
            self.history_pos = None;
            let draft = self.history_draft.take().unwrap_or_default();
            self.input.set(draft);
        }
    }

    fn reset_history_browse(&mut self) {
        self.history_pos = None;
        self.history_draft = None;
    }
}
