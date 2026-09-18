//! 按键与命令：状态机的输入一侧（[`super::App`] 的方法，拆出来只是为了文件不过长）。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use komo_kernel::protocol::ApprovalTarget;
use komo_kernel::types::chat::ApprovalScope;
use komo_kernel::types::ids::ApprovalId;

use super::{App, Effect, PendingSubmission, SubmissionState};
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
        self.pending_submissions.push(PendingSubmission {
            request_key: request_key.clone(),
            text: body.clone(),
            state: SubmissionState::Sending,
        });
        self.scroll = 0;
        vec![Effect::Submit {
            request_key,
            text: body,
            model: self.model.clone(),
            effort: self.effort.clone(),
        }]
    }

    fn run_command(&mut self, text: &str) -> Vec<Effect> {
        let command = match command::parse(text, &self.command_menu()) {
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
            Command::Pending => {
                // 命令行问的，空清单也要印一句"没有"（自动那一问不印，见 `Pending`）。
                self.asking_pending = true;
                vec![Effect::FetchPending]
            }
            Command::Approve { target, scope } => self.decide_by_target(target, true, scope),
            Command::Reject { target } => self.decide_by_target(target, false, ApprovalScope::Once),
            Command::Model { id } => match id {
                Some(id) => {
                    self.note(format!("下一个 Run 用模型 {id}"));
                    self.model = Some(id);
                    // 换了模型，可选的 effort 档位也就换了。
                    self.note(self.effort_blurb());
                    Vec::new()
                }
                // 清单是**去问来的**，不是启动时抄下来就再不更新的一份（§3：模型改完，
                // 下一个 Run 用新模型）。取回来再印，见 `ServerEvent::ModelMenu`。
                None => {
                    self.listing_models = true;
                    vec![Effect::FetchModels]
                }
            },
            Command::Effort { level } => {
                match level {
                    Some(level) => {
                        self.note(format!("下一个 Run 的 effort = {level}"));
                        self.effort = Some(level);
                    }
                    None => self.note(self.effort_blurb()),
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

    /// 解析命令时用的菜单：模型 id 表 + **当前模型**支持的 effort 档位。
    pub(super) fn command_menu(&self) -> command::CommandMenu {
        command::CommandMenu {
            models: self.model_options(),
            efforts: self.effort_options(),
        }
    }

    /// `/model` 无参时印的那一段。
    pub(super) fn model_blurb(&self) -> String {
        let current = self
            .model
            .clone()
            .unwrap_or_else(|| "跟随 Gateway".to_string());
        let options = self.model_options();
        if options.is_empty() {
            format!("当前模型：{current}。Gateway 没有报出模型清单，`/model <id>` 直接设定")
        } else {
            format!("可选模型：{}（当前 {current}）", options.join(" · "))
        }
    }

    /// `/effort` 无参时印的那一段。
    pub(super) fn effort_blurb(&self) -> String {
        let current = self
            .effort
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "跟随 Gateway".into());
        let options = self.effort_options();
        if options.is_empty() {
            format!("这个模型不接受显式 effort（当前 {current}）")
        } else {
            format!("可选 effort：{}（当前 {current}）", options.join(" · "))
        }
    }

    /// `/approve` / `/reject`：`Only` 只在**恰好一个**待处理时生效（§11.3），`All` 是
    /// 待处理的**全部**（一次答一批，各按本次调用）。
    fn decide_by_target(
        &mut self,
        target: ApprovalTarget,
        approved: bool,
        scope: ApprovalScope,
    ) -> Vec<Effect> {
        if target == ApprovalTarget::All {
            return self.decide_all(approved, scope);
        }
        let target = match target {
            ApprovalTarget::One(short_id) => self
                .pending
                .values()
                .find(|record| record.short_id == short_id)
                .map(|record| record.approval.clone()),
            ApprovalTarget::Only => {
                let mut pending = self.pending.values();
                match (pending.next(), pending.next()) {
                    (Some(only), None) => Some(only.approval.clone()),
                    (None, _) => {
                        self.fail("没有待处理的审批");
                        return Vec::new();
                    }
                    (Some(_), Some(_)) => {
                        self.fail(format!(
                            "有多条待处理：{}。请指明短 ID，或 `/approve all` 全批",
                            self.pending_short_list()
                        ));
                        return Vec::new();
                    }
                }
            }
            ApprovalTarget::All => unreachable!("`all` 在上面就分流了"),
        };
        match target {
            Some(approval) => {
                let request_key = self.next_key("decision");
                self.answering.insert(approval.clone());
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

    /// 待处理的**全部**，一次答一批（`a` 键与 `/approve all` 共用）。
    ///
    /// 名单是**这一刻清单上那几条**（`GET /v1/approvals`，`App::pending`）：操作者按下键
    /// 的那一刻看到的就是那一份，而不是服务端在答复到达时才决定的一份。
    ///
    /// 范围只按**本次调用**：一批互不相干的计划共用一个范围（本次 Run / Cron Job）只能
    /// 是替操作者猜——`scope` 参数只用来在他说了范围却拿到本次时**告诉他**。
    fn decide_all(&mut self, approved: bool, scope: ApprovalScope) -> Vec<Effect> {
        let approvals: Vec<ApprovalId> = self.pending.keys().cloned().collect();
        if approvals.is_empty() {
            self.fail("没有待处理的审批");
            return Vec::new();
        }
        if scope != ApprovalScope::Once {
            self.fail("批量答复按本次调用——范围绑的是单份计划，要范围请逐条 /approve <短ID> run");
        }
        let request_key = self.next_key("decisions");
        self.answering.extend(approvals.iter().cloned());
        vec![Effect::DecideMany {
            approvals,
            approved,
            request_key,
        }]
    }

    /// 待处理的那几个短 ID，一行。
    fn pending_short_list(&self) -> String {
        self.pending
            .values()
            .map(|record| record.short_id.to_string())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    fn approval_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(modal) = self.approval.as_ref() else {
            return Vec::new();
        };
        // 已经答过了，正在等服务端回执——再按一次不该发第二个请求。
        if modal.answering {
            return Vec::new();
        }
        let run_scope_ok = modal.allows_run_scope();
        let approval = modal.record.approval.clone();

        // 滚动键只动弹窗。
        let scroll_by: Option<i16> = match key.code {
            KeyCode::PageUp | KeyCode::Up => Some(-1),
            KeyCode::PageDown | KeyCode::Down => Some(1),
            _ => None,
        };
        if let Some(direction) = scroll_by {
            if let Some(modal) = self.approval.as_mut() {
                if direction < 0 {
                    modal.scroll_up();
                } else {
                    modal.scroll_down();
                }
            }
            return Vec::new();
        }

        // `a` = **全部批准**（§11.3 的 `/approve all`）：一批互不相干的审批逐条按本次
        // 调用答。它答的是所有待处理，**含眼前这条**——眼前这条排在名单第一个。
        if matches!(key.code, KeyCode::Char('a') | KeyCode::Char('A')) {
            let mut approvals: Vec<ApprovalId> = self
                .pending
                .keys()
                .filter(|candidate| **candidate != approval)
                .cloned()
                .collect();
            approvals.insert(0, approval.clone());
            self.mark_answering(&approvals);
            let request_key = self.next_key("decisions");
            return vec![Effect::DecideMany {
                approvals,
                approved: true,
                request_key,
            }];
        }

        let answer = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => ApprovalAnswer::ONCE,
            KeyCode::Char('r') | KeyCode::Char('R') => {
                if !run_scope_ok {
                    self.fail("这条请求不可范围化，只能批本次（y）或拒绝（n）");
                    return Vec::new();
                }
                ApprovalAnswer::RUN
            }
            // **Esc 在弹窗里就是拒绝**，和 n 一样：一个没答案的审批会一直占着 Run。
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => ApprovalAnswer::REJECT,
            _ => return Vec::new(),
        };
        self.mark_answering(std::slice::from_ref(&approval));
        let request_key = self.next_key("decision");
        vec![Effect::Decide {
            approval,
            approved: answer.approved,
            scope: answer.scope,
            request_key,
        }]
    }

    /// 记下"这几条是**我们自己**在答"。
    ///
    /// 决定会以 `approval_decided` 从 SSE 回来（网关每条决定推一帧），它和"别人在别处
    /// 答的"长得一模一样。没有这张表，自己按下的那一下会在下一秒被说成「这条审批在别处
    /// 批准了」——一句假话。
    fn mark_answering(&mut self, approvals: &[ApprovalId]) {
        if let Some(modal) = self.approval.as_mut() {
            modal.answering = true;
        }
        self.answering.extend(approvals.iter().cloned());
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
