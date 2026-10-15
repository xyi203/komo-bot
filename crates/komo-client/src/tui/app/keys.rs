//! 按键与命令：状态机的输入一侧（[`super::App`] 的方法，拆出来只是为了文件不过长）。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use komo_kernel::protocol::http::InterventionVerdict;

use super::{
    App, Effect, PendingSubmission, SubmissionState, intervention_kind_text, verdicts_text,
};
use crate::tui::command::{self, Command};
use crate::tui::paste::InputEvent;
use crate::tui::shell;

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
        // **`Ctrl-C` 是暂停，先于一切状态处理**：模型菜单开着、补读历史——哪里都要走得掉。
        // 一块菜单不该把唯一一个"先走开"的键吞掉。Run 留在 Gateway 里照跑，回来用
        // `komo resume <会话>`——驱动在退出时把那行命令印出来。
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return vec![Effect::Quit];
        }
        if self.model_picker.is_some() {
            return self.model_picker_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // 按词走：Alt-← / Alt-→ 是 macOS 与 emacs 的习惯，Ctrl-← / Ctrl-→ 是别处的。
        let word = alt || ctrl;
        match key.code {
            KeyCode::Char('d') if ctrl && self.input.is_empty() => {
                self.quit = true;
                return vec![Effect::Quit];
            }
            // 工具调用全部展开 / 折叠，已经印出去的一起重画。
            KeyCode::Char('t') if ctrl => {
                self.toggle_tool_detail();
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
            // Alt-Backspace 往前删一个词；光秃秃的 Backspace 删一个字符。
            KeyCode::Backspace if alt => {
                self.edit(|input| input.delete_word_before());
                return Vec::new();
            }
            KeyCode::Backspace => {
                if self.input_enabled() {
                    self.input.backspace();
                    self.reset_history_browse();
                }
                return Vec::new();
            }
            // 行内编辑（readline 的那几个），都只在输入框能用时才动。
            KeyCode::Char('w') if ctrl => self.edit(|input| input.delete_word_before()),
            KeyCode::Char('u') if ctrl => self.edit(|input| input.delete_to_line_start()),
            KeyCode::Char('k') if ctrl => self.edit(|input| input.delete_to_line_end()),
            KeyCode::Char('a') if ctrl => self.input.move_home(),
            KeyCode::Char('e') if ctrl => self.input.move_end(),
            KeyCode::Delete => self.edit(|input| input.delete_forward()),
            KeyCode::Left if word => self.input.move_word_left(),
            KeyCode::Right if word => self.input.move_word_right(),
            KeyCode::Left => self.input.move_left(),
            KeyCode::Right => self.input.move_right(),
            KeyCode::Home if ctrl => self.input.move_start_of_text(),
            KeyCode::End if ctrl => self.input.move_end_of_text(),
            KeyCode::Home => self.input.move_home(),
            KeyCode::End => self.input.move_end(),
            // **多行草稿里，↑ / ↓ 先是走行**：一条三行的草稿按 ↑ 就跳走翻历史，等于
            // 这三行白写了。走不动了（已经在首行 / 末行）才轮到历史。
            KeyCode::Up => {
                if !self.input.move_line_up() {
                    self.history_back();
                }
            }
            KeyCode::Down => {
                if !self.input.move_line_down() {
                    self.history_forward();
                }
            }
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

    /// 改草稿的那一类按键共用的一道门：弹窗开着或正在补读历史时输入框是禁用的，改了
    /// 也没人看得见（[`App::input_enabled`]），而翻历史的浏览位置一改就要复位。
    fn edit(&mut self, change: impl FnOnce(&mut crate::tui::paste::Input)) {
        if !self.input_enabled() {
            return;
        }
        change(&mut self.input);
        self.reset_history_browse();
    }

    /// **Esc 的两个意思**：Run 在跑时取消它；空闲时什么都不做——一个有时会清掉草稿的
    /// 停止键，比多按一下还糟。
    fn escape(&mut self) -> Vec<Effect> {
        match (self.current_run.clone(), self.run_state()) {
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
        if let Some(command) = shell::command_of(&text) {
            return self.submit_shell(&text, command.to_string());
        }

        let body = self.input.take();
        let request_key = self.next_key("run");
        self.pending_submissions.push(PendingSubmission {
            request_key: request_key.clone(),
            text: body.clone(),
            state: SubmissionState::Sending,
        });
        vec![Effect::Submit {
            request_key,
            text: body,
            model: self.model.clone(),
            effort: self.effort.clone(),
            shell: false,
            interject: true,
        }]
    }

    /// `!命令`：不经模型，交给 Gateway 直接跑。跑不好的（要 TTY、要提权）先说清楚，草稿
    /// 留在框里改。
    fn submit_shell(&mut self, text: &str, command: String) -> Vec<Effect> {
        if let Some(reason) = shell::refuse(&command) {
            self.fail(reason);
            return Vec::new();
        }
        self.input.clear();
        let request_key = self.next_key("shell");
        self.pending_submissions.push(PendingSubmission {
            request_key: request_key.clone(),
            text: text.to_string(),
            state: SubmissionState::Sending,
        });
        vec![Effect::Submit {
            request_key,
            text: command,
            model: None,
            effort: None,
            shell: true,
            interject: false,
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
                // 会话还没铸出来（`komo` 裸命令、还没发过消息）：没有可划的边界。
                if self.session.is_none() {
                    self.fail("还没有会话：/new 先要有第一条消息");
                    return Vec::new();
                }
                self.note("已划一条回放边界");
                vec![Effect::Boundary]
            }
            Command::Cancel => match (self.current_run.clone(), self.run_state()) {
                (Some(run), Some(status)) if !status.is_terminal() => {
                    let request_key = self.next_key("cancel");
                    vec![Effect::Cancel { run, request_key }]
                }
                _ => {
                    self.fail("现在没有在跑的 Run");
                    Vec::new()
                }
            },
            Command::Status => {
                if self.session.is_none() {
                    self.fail("还没有会话：/status 先要有第一条消息");
                    return Vec::new();
                }
                vec![Effect::FetchStatus]
            }
            Command::Pending => {
                // 命令行问的，空清单也要印一句"没有"（自动那一问不印，见 `Pending`）。
                self.asking_pending = true;
                vec![Effect::FetchPending]
            }
            // `/answer <句柄> <结论>`：结果不明与阻塞共用的一条路，一次答一条（§11.3）。
            Command::Answer { handle, verdict } => self.answer(handle, verdict),
            Command::Model { id } => match id {
                Some(id) => {
                    self.choose_model(id);
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
                // 输入框的提示行只挂得下最常用的那几个键（窄终端上标题会被截），剩下的
                // 在这里说一次——一个没人说过的快捷键等于不存在。
                for line in command::KEYS {
                    self.note(*line);
                }
                Vec::new()
            }
            Command::Quit => {
                self.quit = true;
                vec![Effect::Quit]
            }
        }
    }

    /// 设定下一个 Run 的模型（`/model <id>` 与选择菜单共用）。
    /// 选中了什么看状态行（`model_label`），这里不另印；只有 effort 被清掉时说一句。
    fn choose_model(&mut self, id: String) {
        self.model = Some(id.clone());
        // 换了模型，可选的 effort 档位也就换了：手上那一档新模型不收，就别留着让下一次
        // 提交被网关拒掉——清掉，跟随 Gateway，并且说出来。
        if let Some(effort) = self.effort.take() {
            if self
                .effort_options()
                .iter()
                .any(|option| option == effort.as_str())
            {
                self.effort = Some(effort);
            } else {
                self.note(format!("{id} 不收 effort {effort}，已改回跟随 Gateway"));
            }
        }
    }

    /// 选择菜单开着时的按键。
    fn model_picker_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(picker) = self.model_picker.as_mut() else {
            return Vec::new();
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let chosen = match key.code {
            KeyCode::Up => {
                picker.move_selection(-1);
                return Vec::new();
            }
            KeyCode::Down => {
                picker.move_selection(1);
                return Vec::new();
            }
            KeyCode::Char('p') if ctrl => {
                picker.move_selection(-1);
                return Vec::new();
            }
            KeyCode::Char('n') if ctrl => {
                picker.move_selection(1);
                return Vec::new();
            }
            KeyCode::Esc => {
                self.model_picker = None;
                return Vec::new();
            }
            KeyCode::Enter => picker.selected,
            KeyCode::Char(ch) => match picker.index_of_shortcut(ch) {
                Some(index) => index,
                None => return Vec::new(),
            },
            _ => return Vec::new(),
        };
        let id = picker.entries.get(chosen).map(|entry| entry.id.clone());
        self.model_picker = None;
        if let Some(id) = id {
            self.choose_model(id);
        }
        Vec::new()
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

    /// 答一条：先按种类核对这个结论能不能答。
    ///
    /// 结论按种类分派（§7.5）：给 `blocked` 一条 `satisfied` 是操作者打错了字，或者界面推了
    /// 一个不存在的答案——两种都要**说出来**，而不是发一个服务端只能拒绝的请求。
    fn answer(&mut self, handle: String, verdict: InterventionVerdict) -> Vec<Effect> {
        let Some(item) = self.pending.get(&handle) else {
            self.fail(format!("{handle} 不在待处理清单里"));
            return Vec::new();
        };
        if !verdict.allowed_for(item.kind) {
            self.fail(format!(
                "{handle} 是{}，答不了 {}；它现在能答：{}",
                intervention_kind_text(item.kind),
                verdict.as_str(),
                verdicts_text(&item.verdicts)
            ));
            return Vec::new();
        }
        let request_key = self.next_key("answer");
        vec![Effect::Answer {
            handle,
            verdict,
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
