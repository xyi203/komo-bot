//! Router 的提示（`docs/tasks.md` §4.11）：只有分发规则与这一次的输入。
//!
//! 没有记忆、没有目标、没有技能、没有项目规范——这些只注入 Worker。纯函数：相同的
//! [`RouterInput`] 产出相同的文本，没有 I/O，也不读时钟，所以判断重做（重启后从检查点续）
//! 时给模型的是同一份提示。

use crate::router::{CandidateState, RouteScope, RouterInput, Speaker};

/// 一次判断要发给模型的两段文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterPrompt {
    pub system: String,
    pub user: String,
}

/// 候选任务里"最后一次对外说的话"最多留这么多字符：够认出是哪件事，又不撑大每条消息的输入。
pub const LAST_SAID_CHARS: usize = 80;
/// 最近的消息每条最多留这么多字符。当前这条消息不截（`new` 的任务说明要靠它写完整）。
pub const RECENT_MESSAGE_CHARS: usize = 200;

const PRIVATE_RULES: &str = "\
你是消息分发器。每条进来的消息交给你判断一次，你只负责填写去向：不回答问题，不动手做事，也不调用任何工具。

输入包括：这条消息的原文、候选任务（正在做的、等用户回话的、最近结束的）、最近的几条消息。

去向只有四种，输出一个 JSON 对象，不加解释，不加代码围栏：
- new：这是一件新的事。{\"route\":\"new\",\"description\":\"…\",\"title\":\"…\"}
  description 是交给执行者的任务说明：把用户的要求写完整，不改变原意，不添加用户没说的要求；
  需要结合最近的消息才看得懂的指代，在说明里写明白。title 是不超过 20 个字的简短标题。
- continue：用户在接着说候选里的某个任务（补充、追问、改要求，或者“还是按上次那样”“刚才那个再改一下”）。
  {\"route\":\"continue\",\"task_id\":\"候选里的 id\"}
- stop：用户要停掉候选里的某个任务。{\"route\":\"stop\",\"task_id\":\"候选里的 id\"}
- list：用户在问手上有哪些事、现在在做什么。{\"route\":\"list\"}

task_id 必须原样取自候选任务；没有对得上的任务就不要选 continue 或 stop。
拿不准时选 new。";

const GROUP_RULES: &str = "\
你是群消息分发器。每条进来的消息交给你判断一次，你只负责填写去向：不回答问题，不动手做事，也不调用任何工具。

这条消息来自一个群。候选任务与最近的消息都只来自这个群。

输入包括：这条消息的原文、候选任务（这个群里正在做的、等用户回话的、最近结束的）、这个话题或群里最近的几条消息。

去向只有四种，输出一个 JSON 对象，不加解释，不加代码围栏：
- new：有人在请 Agent 做一件新的事。{\"route\":\"new\",\"description\":\"…\",\"title\":\"…\"}
  description 是交给执行者的任务说明：把对方的要求写完整，不改变原意，不添加对方没说的要求；
  需要结合最近的消息才看得懂的指代，在说明里写明白。title 是不超过 20 个字的简短标题。
- continue：对方在接着说候选里的某个任务。{\"route\":\"continue\",\"task_id\":\"候选里的 id\"}
- stop：对方要停掉候选里的某个任务。{\"route\":\"stop\",\"task_id\":\"候选里的 id\"}
- ignore：不该由 Agent 接的消息，例如明显是在问操作者本人而不是问 Agent，或者只是群友之间的交谈。
  {\"route\":\"ignore\"}

task_id 必须原样取自候选任务；没有对得上的任务就不要选 continue 或 stop。
没有“列出任务”这个去向。拿不准时选 ignore。";

/// 分发规则（系统提示）。
pub fn system_prompt(scope: RouteScope) -> &'static str {
    match scope {
        RouteScope::Private => PRIVATE_RULES,
        RouteScope::Group => GROUP_RULES,
    }
}

/// 把一次输入渲染成提示。
pub fn render(input: &RouterInput) -> RouterPrompt {
    let mut user = String::new();
    user.push_str("## 这条消息\n");
    user.push_str(&input.message);
    user.push_str("\n\n## 候选任务\n");
    if input.tasks.is_empty() {
        user.push_str("（没有）\n");
    }
    for task in &input.tasks {
        let state = match task.state {
            CandidateState::Running => "进行中",
            CandidateState::Waiting => "等用户回话",
            CandidateState::Finished => "已结束",
        };
        user.push_str(&format!(
            "- id：{}｜状态：{state}｜标题：{}｜最后对外说的：{}\n",
            task.id,
            one_line(&task.title),
            clip(&task.last_said, LAST_SAID_CHARS),
        ));
    }
    user.push_str("\n## 最近的消息\n");
    if input.recent.is_empty() {
        user.push_str("（没有）\n");
    }
    for message in &input.recent {
        let who = match message.speaker {
            Speaker::Operator => "操作者",
            Speaker::Agent => "助手",
            Speaker::Member => "群成员",
        };
        user.push_str(&format!(
            "- {who}：{}\n",
            clip(&message.text, RECENT_MESSAGE_CHARS)
        ));
    }
    RouterPrompt {
        system: system_prompt(input.scope).to_string(),
        user,
    }
}

/// 压成一行再按**字符**截断（中文按字节切会切出半个字），截了加 `…`。
fn clip(text: &str, max: usize) -> String {
    let line = one_line(text);
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max).collect();
    cut.push('…');
    cut
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{SECTION_BASE, SECTION_INSTRUCTIONS, SECTION_PROJECT, SECTION_SKILLS};
    use crate::router::{CandidateTask, RecentMessage, RouterInput, RouterLimits};

    fn input(scope: RouteScope) -> RouterInput {
        RouterInput::new(
            scope,
            "把周报发我",
            vec![
                CandidateTask {
                    id: "t1".into(),
                    title: "整理周报".into(),
                    state: CandidateState::Running,
                    last_said: "我先把这份报告整理出来".repeat(20),
                },
                CandidateTask {
                    id: "t2".into(),
                    title: "查告警".into(),
                    state: CandidateState::Finished,
                    last_said: "查完了\n一共三条".into(),
                },
            ],
            vec![RecentMessage {
                speaker: Speaker::Operator,
                text: "帮我看下周报".into(),
            }],
            RouterLimits::default(),
        )
    }

    #[test]
    fn the_same_input_renders_byte_identical_prompts() {
        for scope in [RouteScope::Private, RouteScope::Group] {
            assert_eq!(render(&input(scope)), render(&input(scope)));
        }
    }

    #[test]
    fn the_prompt_has_no_memory_goal_skill_or_project_sections() {
        for scope in [RouteScope::Private, RouteScope::Group] {
            let prompt = render(&input(scope));
            let text = format!("{}\n{}", prompt.system, prompt.user);
            for banned in [
                "记忆",
                "目标",
                "技能",
                "项目规范",
                "AGENTS.md",
                "skill",
                SECTION_SKILLS,
                SECTION_PROJECT,
                SECTION_INSTRUCTIONS,
                SECTION_BASE,
            ] {
                assert!(!text.contains(banned), "{scope:?} 的提示里不该有 {banned}");
            }
        }
    }

    #[test]
    fn the_prompt_lists_the_message_tasks_and_recent_messages() {
        let prompt = render(&input(RouteScope::Private));
        assert!(prompt.user.contains("把周报发我"));
        assert!(prompt.user.contains("id：t1｜状态：进行中｜标题：整理周报"));
        assert!(
            prompt
                .user
                .contains("id：t2｜状态：已结束｜标题：查告警｜最后对外说的：查完了 一共三条")
        );
        assert!(prompt.user.contains("- 操作者：帮我看下周报"));
        let said = prompt
            .user
            .lines()
            .find(|line| line.contains("t1"))
            .unwrap()
            .split("最后对外说的：")
            .nth(1)
            .unwrap();
        assert_eq!(
            said.chars().count(),
            LAST_SAID_CHARS + 1,
            "截到固定长度并加省略号"
        );
        assert!(said.ends_with('…'));
    }

    #[test]
    fn only_the_private_prompt_offers_list_and_only_the_group_prompt_offers_ignore() {
        let private = system_prompt(RouteScope::Private);
        assert!(
            private.contains("\"route\":\"list\"") && !private.contains("\"route\":\"ignore\"")
        );
        let group = system_prompt(RouteScope::Group);
        assert!(group.contains("\"route\":\"ignore\"") && !group.contains("\"route\":\"list\""));
        assert!(private.contains("拿不准时选 new"));
        assert!(group.contains("拿不准时选 ignore"));
    }

    #[test]
    fn empty_candidates_and_recent_messages_are_said_so() {
        let empty = RouterInput::new(
            RouteScope::Private,
            "hi",
            vec![],
            vec![],
            RouterLimits::default(),
        );
        let prompt = render(&empty);
        assert_eq!(prompt.user.matches("（没有）").count(), 2);
    }
}
