//! 所有权树（spec §5.4、§5.5）：task 拥有 task 与 conversation，conversation 可被一个 task 拥有。
//!
//! 纯数据结构：`track` / `set_edge` 喂进已提交的记录，查询给出"某个节点下面活着的普通
//! 所有的工作"、"沿所有者链往上有没有取消意图"、"这次等待会不会成环"。history parent
//! 与所有权无关。
//!
//! 与 pi 的 `#below` 索引相比这里更直白：节点与边只增不删（体量只和见过的任务数成正比），
//! 向下遍历直接查 `children`。调用方（调度器）负责把没加载的祖先链从存储读进来
//! （[`TaskGraph::missing_above`] 告诉它缺哪一个）。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::durable::ids::{ConversationId, TaskId};
use crate::durable::records::TaskRecord;

use super::state::{cancellation_intent, is_live};

/// 所有权树的节点：一个任务，或一个 conversation。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Node {
    Task(TaskId),
    Conversation(ConversationId),
}

/// 任务记录里不可变的所有权字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaskNode {
    pub conversation: ConversationId,
    pub owner: Option<TaskId>,
    pub background: bool,
}

impl TaskNode {
    pub fn of(record: &TaskRecord) -> Self {
        Self {
            conversation: record.conversation,
            owner: record.owner,
            background: record.background,
        }
    }

    /// 它挂在哪：所有者任务，没有就是它所在的 conversation。
    pub fn parent(&self) -> Node {
        match self.owner {
            Some(owner) => Node::Task(owner),
            None => Node::Conversation(self.conversation),
        }
    }
}

/// 一次提交里还没落盘的候选：它们在所有权遍历里盖过已提交的记录。
#[derive(Debug, Clone, Default)]
pub struct Overlay {
    pub tasks: BTreeMap<TaskId, TaskRecord>,
    pub edges: BTreeMap<ConversationId, Option<TaskId>>,
}

/// 向下遍历到的一个任务。
#[derive(Debug, Clone)]
pub struct Walked {
    pub id: TaskId,
    pub node: TaskNode,
    /// 它还活着时的记录（候选优先）；已终结为 `None`。
    pub live: Option<TaskRecord>,
}

/// 向上一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Task {
        id: TaskId,
        node: TaskNode,
    },
    Conversation(ConversationId),
    /// 边还没加载：不能下结论。
    Unknown,
}

/// 普通遍历的起点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Conversation(ConversationId),
    /// 每个没有所有者的 conversation。
    Roots,
}

#[derive(Debug, Default)]
pub struct TaskGraph {
    live: BTreeMap<TaskId, TaskRecord>,
    nodes: HashMap<TaskId, TaskNode>,
    /// conversation 的所有者任务，`None` 表示没有所有者。
    edges: HashMap<ConversationId, Option<TaskId>>,
    children: HashMap<Node, BTreeSet<Node>>,
}

impl TaskGraph {
    pub fn new() -> Self {
        Self::default()
    }

    // ------------------------------------------------------------------ 喂数据

    /// 一条已提交的任务记录：活的进 `live`，终结的离开 `live`、节点留着（遍历要穿过它）。
    pub fn track(&mut self, record: &TaskRecord) {
        let node = TaskNode::of(record);
        if self.nodes.insert(record.id, node).is_none() {
            self.children
                .entry(node.parent())
                .or_default()
                .insert(Node::Task(record.id));
        }
        if is_live(record) {
            self.live.insert(record.id, record.clone());
        } else {
            self.live.remove(&record.id);
        }
    }

    /// 只知道一个任务的所有权字段（沿链往上加载时读到的已终结祖先）。
    pub fn know(&mut self, id: TaskId, node: TaskNode) {
        if self.nodes.insert(id, node).is_none() {
            self.children
                .entry(node.parent())
                .or_default()
                .insert(Node::Task(id));
        }
    }

    /// conversation 的所有者边；只会设一次（所有权不可变）。
    pub fn set_edge(&mut self, conversation: ConversationId, owner: Option<TaskId>) {
        if self.edges.insert(conversation, owner).is_none()
            && let Some(owner) = owner
        {
            self.children
                .entry(Node::Task(owner))
                .or_default()
                .insert(Node::Conversation(conversation));
        }
    }

    // ------------------------------------------------------------------ 读

    pub fn live(&self) -> impl Iterator<Item = &TaskRecord> {
        self.live.values()
    }

    pub fn live_record(&self, id: TaskId) -> Option<&TaskRecord> {
        self.live.get(&id)
    }

    pub fn is_live(&self, id: TaskId) -> bool {
        self.live.contains_key(&id)
    }

    /// 没有所有者的 conversation（已加载的）。
    pub fn root_conversations(&self) -> Vec<ConversationId> {
        let mut roots: Vec<ConversationId> = self
            .edges
            .iter()
            .filter(|(_, owner)| owner.is_none())
            .map(|(id, _)| *id)
            .collect();
        roots.sort();
        roots
    }

    pub fn node(&self, id: TaskId) -> Option<TaskNode> {
        self.nodes.get(&id).copied()
    }

    /// conversation 的所有者：`Some(None)` 没有所有者，`None` 还没加载。
    pub fn edge(&self, conversation: ConversationId) -> Option<Option<TaskId>> {
        self.edges.get(&conversation).copied()
    }

    fn node_in(&self, id: TaskId, overlay: Option<&Overlay>) -> Option<TaskNode> {
        overlay
            .and_then(|o| o.tasks.get(&id))
            .map(TaskNode::of)
            .or_else(|| self.node(id))
    }

    fn edge_in(
        &self,
        conversation: ConversationId,
        overlay: Option<&Overlay>,
    ) -> Option<Option<TaskId>> {
        overlay
            .and_then(|o| o.edges.get(&conversation).copied())
            .or_else(|| self.edge(conversation))
    }

    /// 从 `start` 往上，第一个还没加载的节点（`None` = 链是全的，直到没有所有者的根）。
    pub fn missing_above(&self, start: Node) -> Option<Node> {
        let mut at = start;
        loop {
            match at {
                Node::Task(id) => match self.node(id) {
                    None => return Some(at),
                    Some(known) => at = known.parent(),
                },
                Node::Conversation(id) => match self.edge(id) {
                    None => return Some(at),
                    Some(None) => return None,
                    Some(Some(owner)) => at = Node::Task(owner),
                },
            }
        }
    }

    fn above(&self, start: Node, overlay: Option<&Overlay>) -> Vec<Step> {
        let mut steps = Vec::new();
        let mut at = Some(start);
        while let Some(node) = at {
            match node {
                Node::Task(id) => {
                    let Some(known) = self.node_in(id, overlay) else {
                        steps.push(Step::Unknown);
                        return steps;
                    };
                    steps.push(Step::Task { id, node: known });
                    at = Some(known.parent());
                }
                Node::Conversation(id) => {
                    steps.push(Step::Conversation(id));
                    match self.edge_in(id, overlay) {
                        None => {
                            steps.push(Step::Unknown);
                            return steps;
                        }
                        Some(None) => at = None,
                        Some(Some(owner)) => at = Some(Node::Task(owner)),
                    }
                }
            }
        }
        steps
    }

    fn children_of(&self, at: Node, overlay: Option<&Overlay>) -> BTreeSet<Node> {
        let mut children = self.children.get(&at).cloned().unwrap_or_default();
        if let Some(overlay) = overlay {
            for record in overlay.tasks.values() {
                if TaskNode::of(record).parent() == at {
                    children.insert(Node::Task(record.id));
                }
            }
            if let Node::Task(owner) = at {
                for (conversation, edge) in &overlay.edges {
                    if *edge == Some(owner) {
                        children.insert(Node::Conversation(*conversation));
                    }
                }
            }
        }
        children
    }

    /// `start` 之下遍历到的任务（候选盖过已提交记录）。后台任务会被遍历到但不会进入，除非
    /// `cross_background`；`enter` 拒绝的任务同样不进入。
    pub fn walk_down(
        &self,
        start: Node,
        overlay: Option<&Overlay>,
        cross_background: bool,
        enter: &dyn Fn(TaskId) -> bool,
    ) -> Vec<Walked> {
        let mut walked = Vec::new();
        let mut seen = HashSet::new();
        let mut pending = vec![start];
        while let Some(at) = pending.pop() {
            for child in self.children_of(at, overlay) {
                if !seen.insert(child) {
                    continue;
                }
                let id = match child {
                    Node::Conversation(_) => {
                        pending.push(child);
                        continue;
                    }
                    Node::Task(id) => id,
                };
                let record = overlay
                    .and_then(|o| o.tasks.get(&id))
                    .or_else(|| self.live.get(&id));
                let Some(node) = record.map(TaskNode::of).or_else(|| self.node(id)) else {
                    continue;
                };
                let live = record.filter(|r| is_live(r)).cloned();
                walked.push(Walked { id, node, live });
                if (cross_background || !node.background) && enter(id) {
                    pending.push(child);
                }
            }
        }
        walked
    }

    /// `owner` 的活的普通所有的工作：往下走不进后台任务时碰到的活的非后台任务；
    /// `done` 里的任务不进入（它们下面已经没有活的了）。
    pub fn owned_live(
        &self,
        owner: TaskId,
        overlay: Option<&Overlay>,
        first: bool,
        done: &HashSet<TaskId>,
    ) -> Vec<TaskId> {
        let mut found = Vec::new();
        let enter = |id: TaskId| !done.contains(&id);
        for walked in self.walk_down(Node::Task(owner), overlay, false, &enter) {
            if walked.live.is_none() || walked.node.background {
                continue;
            }
            found.push(walked.id);
            if first {
                break;
            }
        }
        found.sort();
        found
    }

    pub fn has_owned_live(
        &self,
        owner: TaskId,
        overlay: Option<&Overlay>,
        done: &HashSet<TaskId>,
    ) -> bool {
        !self.owned_live(owner, overlay, true, done).is_empty()
    }

    /// 普通遍历从 `scope` 出发能不能到 `start`：往上走到该 conversation（`Roots` 则到没有
    /// 所有者的根），中途不穿过后台所有者，除非 `cross_background`。`None` = 边没加载。
    pub fn in_scope(&self, start: Node, scope: Scope, cross_background: bool) -> Option<bool> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(id) => {
                    if scope == Scope::Conversation(id) {
                        return Some(true);
                    }
                }
                Step::Task { node, .. } => {
                    if node.background && !cross_background {
                        return Some(false);
                    }
                }
            }
        }
        Some(scope == Scope::Roots)
    }

    /// 往上找最近的、取消意图够得到 `start` 的活所有者：在遇到没有自己取消意图的后台所有者
    /// 之前。终结的所有者从不级联（spec §5.4）。
    pub fn cancelling_owner(&self, start: Node) -> Option<TaskId> {
        for step in self.above(start, None) {
            match step {
                Step::Unknown => return None,
                Step::Conversation(_) => {}
                Step::Task { id, node } => {
                    if self.live.get(&id).is_some_and(cancellation_intent) {
                        return Some(id);
                    }
                    if node.background {
                        return None;
                    }
                }
            }
        }
        None
    }

    /// 已结束的普通工作 `node` 之上最近的活任务：它曾被这份工作拖住。后台任务及其下面的工作
    /// 不拖住任何人。
    pub fn nearest_live_above(
        &self,
        node: &TaskNode,
        overlay: Option<&Overlay>,
    ) -> Option<TaskRecord> {
        let mut current = *node;
        while !current.background {
            let owner = match current.owner {
                Some(owner) => owner,
                None => match self.edge_in(current.conversation, overlay) {
                    Some(Some(owner)) => owner,
                    _ => return None,
                },
            };
            if let Some(record) = overlay
                .and_then(|o| o.tasks.get(&owner))
                .or_else(|| self.live.get(&owner))
                && is_live(record)
            {
                return Some(record.clone());
            }
            current = self.node_in(owner, overlay)?;
        }
        None
    }

    /// 这次等待会不会成环：返回从 `on` 里某个成员回到 `task` 的路径。边：X→Y 表示 X 不能
    /// 先于 Y 终结——Y 在不带 abort 标记的 waiting X 的 `on` 里，或 Y 是 X 活的普通所有的
    /// 工作（它会把 X 拖在 `completing`）。每个任务只访问一次。
    pub fn wait_cycle(
        &self,
        task: TaskId,
        on: &[TaskId],
        overlay: &Overlay,
    ) -> Option<Vec<TaskId>> {
        use crate::durable::records::TaskState;
        let mut from: HashMap<TaskId, Option<TaskId>> = HashMap::new();
        let mut walked: HashSet<TaskId> = HashSet::new();
        let mut pending: Vec<TaskId> = Vec::new();
        let path = |from: &HashMap<TaskId, Option<TaskId>>, end: TaskId| {
            let mut steps = vec![];
            let mut at = Some(end);
            while let Some(id) = at {
                steps.push(id);
                at = from.get(&id).copied().flatten();
            }
            steps.reverse();
            steps
        };
        for &id in on {
            if let std::collections::hash_map::Entry::Vacant(slot) = from.entry(id) {
                slot.insert(None);
                pending.push(id);
            }
        }
        while let Some(id) = pending.pop() {
            if id == task {
                return Some(path(&from, id));
            }
            let Some(record) = overlay.tasks.get(&id).or_else(|| self.live.get(&id)) else {
                continue;
            };
            if !is_live(record) {
                continue;
            }
            // 带 abort 标记的任务只等自己的所有的工作（abort 自底向上），它的 `on` 不是边。
            if let TaskState::Waiting { on, .. } = &record.state
                && !record.abort_requested
            {
                for &member in on {
                    if let std::collections::hash_map::Entry::Vacant(slot) = from.entry(member) {
                        slot.insert(Some(id));
                        pending.push(member);
                    }
                }
            }
            if !walked.insert(id) {
                continue;
            }
            // 往下走不再重复进入已走过的子树：它自己的遍历已经覆盖。
            let enter = |below: TaskId| !walked.contains(&below);
            let mut found = Vec::new();
            for below in self.walk_down(Node::Task(id), Some(overlay), false, &enter) {
                if below.live.is_none() || below.node.background {
                    continue;
                }
                found.push(below.id);
                if let std::collections::hash_map::Entry::Vacant(slot) = from.entry(below.id) {
                    slot.insert(Some(id));
                    pending.push(below.id);
                }
            }
            walked.extend(found);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::durable::records::{JoinPolicy, TaskOutcome, TaskRecord, TaskState};

    fn task(id: u64, conversation: u64, owner: Option<u64>, background: bool) -> TaskRecord {
        let mut record = TaskRecord::pending(
            TaskId::new(id),
            ConversationId::new(conversation),
            "k",
            json!({}),
            json!({"phase": "a"}),
        );
        record.owner = owner.map(TaskId::new);
        record.background = background;
        record
    }

    fn ended(mut record: TaskRecord) -> TaskRecord {
        record.state = TaskState::Terminal {
            outcome: TaskOutcome::Completed { result: json!(0) },
        };
        record
    }

    fn graph(records: &[TaskRecord]) -> TaskGraph {
        let mut graph = TaskGraph::new();
        graph.set_edge(ConversationId::new(1), None);
        for record in records {
            graph.track(record);
        }
        graph
    }

    fn none() -> HashSet<TaskId> {
        HashSet::new()
    }

    fn ids(values: &[u64]) -> Vec<TaskId> {
        values.iter().map(|v| TaskId::new(*v)).collect()
    }

    #[test]
    fn owned_live_reaches_children_and_work_below_ended_tasks_but_not_background() {
        // 10 拥有 11（已结束），11 拥有 conversation 2，其中 12 活着；13 是后台任务，14 是它的孩子。
        let mut graph = graph(&[
            task(10, 1, None, false),
            ended(task(11, 1, Some(10), false)),
            task(12, 2, None, false),
            task(13, 1, None, true),
        ]);
        graph.set_edge(ConversationId::new(2), Some(TaskId::new(11)));
        graph.track(&task(14, 1, Some(13), false));
        // 12 在 conversation 2 里，conversation 2 由 11 拥有，11 由 10 拥有。
        assert_eq!(
            graph.owned_live(TaskId::new(10), None, false, &none()),
            ids(&[12])
        );
        // 13 是后台任务：它自己是 conversation 1 的任务，不在 10 之下，14 在 13 之下。
        assert_eq!(
            graph.owned_live(TaskId::new(13), None, false, &none()),
            ids(&[14])
        );
    }

    #[test]
    fn a_background_child_task_of_conversation_is_a_boundary_for_ordinary_traversal() {
        let mut graph = graph(&[task(10, 1, None, false), task(11, 1, None, true)]);
        graph.set_edge(ConversationId::new(2), Some(TaskId::new(11)));
        graph.track(&task(12, 2, None, false));
        let walked = graph.walk_down(
            Node::Conversation(ConversationId::new(1)),
            None,
            false,
            &|_| true,
        );
        let mut seen: Vec<_> = walked.iter().map(|w| w.id).collect();
        seen.sort();
        assert_eq!(
            seen,
            ids(&[10, 11]),
            "12 hides behind the background boundary"
        );
        let crossing = graph.walk_down(
            Node::Conversation(ConversationId::new(1)),
            None,
            true,
            &|_| true,
        );
        let mut seen: Vec<_> = crossing.iter().map(|w| w.id).collect();
        seen.sort();
        assert_eq!(seen, ids(&[10, 11, 12]));
    }

    #[test]
    fn overlay_candidates_override_committed_records_and_add_new_work() {
        let graph = graph(&[task(10, 1, None, false)]);
        assert!(!graph.has_owned_live(TaskId::new(10), None, &none()));
        let mut overlay = Overlay::default();
        overlay
            .tasks
            .insert(TaskId::new(11), task(11, 1, Some(10), false));
        assert!(graph.has_owned_live(TaskId::new(10), Some(&overlay), &none()));
        overlay
            .tasks
            .insert(TaskId::new(11), ended(task(11, 1, Some(10), false)));
        assert!(!graph.has_owned_live(TaskId::new(10), Some(&overlay), &none()));
        // 新建的 conversation 由 10 拥有，里面有新任务。
        overlay
            .edges
            .insert(ConversationId::new(5), Some(TaskId::new(10)));
        overlay
            .tasks
            .insert(TaskId::new(12), task(12, 5, None, false));
        assert_eq!(
            graph.owned_live(TaskId::new(10), Some(&overlay), false, &none()),
            ids(&[12])
        );
    }

    #[test]
    fn scope_membership_stops_at_background_owners_unless_crossing() {
        let mut graph = graph(&[task(11, 1, None, true)]);
        graph.set_edge(ConversationId::new(2), Some(TaskId::new(11)));
        let below = Node::Conversation(ConversationId::new(2));
        let scope = Scope::Conversation(ConversationId::new(1));
        assert_eq!(graph.in_scope(below, scope, false), Some(false));
        assert_eq!(graph.in_scope(below, scope, true), Some(true));
        assert_eq!(
            graph.in_scope(
                Node::Conversation(ConversationId::new(1)),
                Scope::Roots,
                false
            ),
            Some(true)
        );
        assert_eq!(
            graph.in_scope(
                Node::Conversation(ConversationId::new(9)),
                Scope::Roots,
                false
            ),
            None,
            "an edge that is not loaded decides nothing"
        );
    }

    #[test]
    fn missing_above_names_the_first_unloaded_link() {
        let mut graph = TaskGraph::new();
        graph.track(&task(12, 2, None, false));
        assert_eq!(
            graph.missing_above(Node::Conversation(ConversationId::new(2))),
            Some(Node::Conversation(ConversationId::new(2)))
        );
        graph.set_edge(ConversationId::new(2), Some(TaskId::new(11)));
        assert_eq!(
            graph.missing_above(Node::Conversation(ConversationId::new(2))),
            Some(Node::Task(TaskId::new(11)))
        );
        graph.know(
            TaskId::new(11),
            TaskNode {
                conversation: ConversationId::new(1),
                owner: None,
                background: false,
            },
        );
        graph.set_edge(ConversationId::new(1), None);
        assert_eq!(
            graph.missing_above(Node::Conversation(ConversationId::new(2))),
            None
        );
    }

    #[test]
    fn cancelling_owner_skips_terminal_owners_and_stops_at_background_without_intent() {
        let mut owner = task(10, 1, None, false);
        owner.abort_requested = true;
        let mut graph = graph(&[owner, task(11, 1, Some(10), false)]);
        assert_eq!(
            graph.cancelling_owner(Node::Task(TaskId::new(10))),
            Some(TaskId::new(10))
        );
        // 终结的所有者不级联。
        let mut done = ended(task(10, 1, None, false));
        done.abort_requested = true;
        graph.track(&done);
        assert_eq!(graph.cancelling_owner(Node::Task(TaskId::new(10))), None);
        // 没有取消意图的后台所有者把上面的意图挡住。
        let mut top = task(20, 1, None, false);
        top.abort_requested = true;
        let graph = self::graph(&[
            top,
            task(21, 1, Some(20), true),
            task(22, 1, Some(21), false),
        ]);
        assert_eq!(
            graph.cancelling_owner(Node::Task(TaskId::new(21))),
            None,
            "work owned by a background task is behind a boundary"
        );
        assert_eq!(graph.cancelling_owner(Node::Task(TaskId::new(22))), None);
    }

    #[test]
    fn nearest_live_above_passes_ended_tasks_and_stops_at_background() {
        let graph = graph(&[
            task(10, 1, None, false),
            ended(task(11, 1, Some(10), false)),
            task(12, 1, Some(11), false),
            task(13, 1, Some(10), true),
        ]);
        let above = graph.nearest_live_above(&TaskNode::of(&task(12, 1, Some(11), false)), None);
        assert_eq!(above.map(|r| r.id), Some(TaskId::new(10)));
        let none = graph.nearest_live_above(&TaskNode::of(&task(13, 1, Some(10), true)), None);
        assert!(none.is_none(), "background work holds nothing above it");
    }

    fn waiting(mut record: TaskRecord, on: &[u64]) -> TaskRecord {
        record.state = TaskState::Waiting {
            checkpoint: json!({"phase": "w"}),
            on: ids(on),
            policy: JoinPolicy::AllSettled,
        };
        record
    }

    #[test]
    fn waiting_on_itself_or_on_a_holding_owner_is_a_cycle() {
        let graph = graph(&[task(10, 1, None, false), task(11, 1, Some(10), false)]);
        let overlay = Overlay::default();
        assert_eq!(
            graph.wait_cycle(TaskId::new(10), &ids(&[10]), &overlay),
            Some(ids(&[10]))
        );
        // 11 等它的所有者 10：10 被 11 拖住（completing），互相等。
        let cycle = graph.wait_cycle(TaskId::new(11), &ids(&[10]), &overlay);
        assert_eq!(cycle, Some(ids(&[10, 11])));
    }

    #[test]
    fn a_wait_through_another_waiter_closes_a_cycle_but_a_marked_waiter_adds_no_edge() {
        let a = waiting(task(10, 1, None, false), &[11]);
        let b = task(11, 1, None, false);
        let overlay = Overlay::default();
        let graph_ab = graph(&[a.clone(), b.clone()]);
        // 11 想等 10，而 10 已经在等 11。
        assert_eq!(
            graph_ab.wait_cycle(TaskId::new(11), &ids(&[10]), &overlay),
            Some(ids(&[10, 11]))
        );
        let mut marked = a;
        marked.abort_requested = true;
        let graph_marked = graph(&[marked, b]);
        assert_eq!(
            graph_marked.wait_cycle(TaskId::new(11), &ids(&[10]), &overlay),
            None,
            "an abort-marked task waits only for its owned work"
        );
    }

    #[test]
    fn a_background_task_may_wait_on_the_owner_of_its_conversation() {
        // 后台任务 11 在 conversation 2，该会话由 10 拥有；11 不拖住 10。
        let mut graph = graph(&[task(10, 1, None, false)]);
        graph.set_edge(ConversationId::new(2), Some(TaskId::new(10)));
        graph.track(&task(11, 2, None, true));
        assert_eq!(
            graph.wait_cycle(TaskId::new(11), &ids(&[10]), &Overlay::default()),
            None
        );
    }
}
