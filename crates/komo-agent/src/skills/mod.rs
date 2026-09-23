//! SkillRegistry：人写的 `SKILL.md` 与目录行门控（§5.6）。
//!
//! Skills 是**人写的程序性说明**，所以这里没有安装 / 候选 / 治理流程，也没有 `skill`
//! 工具——模型用 `read` 读 `SKILL.md`，skills 目录是 Policy 里的只读根。这个模块回答
//! 的只有三个问题：**有哪些**、**这个名字是哪一份**、**哪几条该出现在系统提示的目录里**。
//!
//! 三条性质，各有一个测试：
//!
//! - **搜索路径有序，同名先到先得**；
//! - **每次查询重扫目录**，编辑或新增无需重启；
//! - `platforms:` / `requires_tools:` **只门控目录行**，不门控加载——`inspect` 照样看得到。

mod frontmatter;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use komo_kernel::protocol::config::ConfigSnapshot;

pub use frontmatter::FrontMatter;

/// 系统提示里那份目录的总量上限。
///
/// **实测值（2026-09-21，本机）**：166 个 skill 只列名字是 2752 字符；带描述要 55 KB（描述
/// 平均 261 字符）。4000 字符能装下 ~235 条名字，留了余量——原值 2000 只够 16 条"名字 +
/// 描述"，166 个 skill 里的后 150 个**根本没进提示**，而模型不知道有它们就不会去读。
pub const DEFAULT_CATALOG_CHARS: usize = 4_000;

/// 目录行的一种形状：**整批**装得下描述就用 [`Shape::Full`]，否则只留名字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Full,
    NamesOnly,
}

/// 一次目录渲染：谁在里面、用哪种形状、因为装不下少了几个、出了条目的那些根。
///
/// 这是 `docs/agent.md` §14 的那份**值**：`SkillRegistry::offer` 现读现渲染出来
/// （discovery），`ContextInput.skills` 拿到手上之后只剩 [`Self::prompt_block`] 这一步
/// 纯渲染（presentation）——两件事不再混在一起，以后要把目录钉住（§9 缺口 1）时，钉的
/// 就是这一份值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCatalog {
    skills: Vec<Skill>,
    shape: Shape,
    dropped: usize,
    /// 按序的、真的出了条目的那些根（§5.6：目录行里的名字要能顺着这个顺序定位到文件）。
    roots: Vec<String>,
}

impl SkillCatalog {
    fn line(&self, skill: &Skill) -> String {
        match self.shape {
            Shape::Full => skill.catalog_line(),
            Shape::NamesOnly => format!("- {}", skill.name),
        }
    }

    /// 目录正文。装不下时说清少了几个——**"没有它"和"没列出来"是两件事**，模型不该因为
    /// 一条没列出来就断定它不存在。
    fn text(&self) -> String {
        let mut lines: Vec<String> = self.skills.iter().map(|skill| self.line(skill)).collect();
        if self.dropped > 0 {
            lines.push(note_text(self.dropped));
        }
        lines.join("\n")
    }

    /// 系统提示里要拼的那一块（§5.6）：先给**按序的根**，再给目录行。
    ///
    /// 根只列真的出了条目的那几个，顺序就是搜索顺序——目录行里的名字要能顺着这个顺序
    /// 定位到文件（`<根>/<名字>/SKILL.md`），"同名先到先得"这件事才落得下来：模型按这个
    /// 顺序找，先撞上的那份正是加载时生效的那一份。
    ///
    /// 一条能露面的都没有时回答 `None`：那种情况下系统提示**一个字都不多**，不因为
    /// "配置里有 skills 概念"就凭空多出一段空标题。
    pub fn prompt_block(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let mut block = String::from(
            "Skills（人写的操作说明；要用的时候按下面的顺序找 <根>/<名字>/SKILL.md，\
             用 read 读了再照做）：\n",
        );
        block.push_str("根：");
        block.push_str(&self.roots.join("、"));
        block.push('\n');
        block.push_str(&self.text());
        Some(block)
    }
}

/// 一行占多少（末尾那个换行也算：每一行都要占位）。
fn cost(line: &str) -> usize {
    line.chars().count() + 1
}

/// 装不下时末尾那句。
fn note_text(dropped: usize) -> String {
    format!("（另有 {dropped} 条没列出来：目录行到上限了）")
}

/// 一段描述压成一行：折掉的换行会把一条目录行撕成两条。
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 一份 `SKILL.md` 的元信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// frontmatter 的 `name`；没写就是目录名。
    pub name: String,
    pub description: String,
    pub platforms: Vec<String>,
    pub requires_tools: Vec<String>,
    pub path: PathBuf,
    /// 被同名的、更靠前的那一份盖住了。
    pub shadowed_by: Option<PathBuf>,
    /// 文件本身的问题（缺 frontmatter、缺描述）。**不影响加载**，只影响目录行。
    pub issues: Vec<String>,
}

impl Skill {
    /// 在这个平台、这套工具下，该不该出现在**提示目录**里。
    pub fn offered(&self, context: &OfferContext) -> bool {
        self.issues.is_empty()
            && self.platform_matches(&context.platform)
            && self.tools_available(&context.tools)
    }

    fn platform_matches(&self, platform: &str) -> bool {
        self.platforms.is_empty()
            || self
                .platforms
                .iter()
                .any(|declared| normalize_platform(declared) == normalize_platform(platform))
    }

    fn tools_available(&self, tools: &BTreeSet<String>) -> bool {
        self.requires_tools
            .iter()
            .all(|needed| tools.contains(needed.as_str()))
    }

    /// 目录行：名字 + 一句描述。
    pub fn catalog_line(&self) -> String {
        format!("- {}：{}", self.name, one_line(&self.description))
    }
}

/// 一份 `SKILL.md` 的全文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDocument {
    pub skill: Skill,
    pub body: String,
}

/// 门控目录行的上下文（**只门控目录**，§5.6）。
#[derive(Debug, Clone)]
pub struct OfferContext {
    /// `linux` / `macos`。
    pub platform: String,
    /// 这个运行时真的注册了的工具名。
    pub tools: BTreeSet<String>,
    pub max_chars: usize,
}

impl OfferContext {
    /// 当前平台 + 给定工具集。
    pub fn here<I: IntoIterator<Item = S>, S: Into<String>>(tools: I) -> Self {
        OfferContext {
            // `consts::OS` 是编译期常量，不是环境变量。
            platform: std::env::consts::OS.to_string(),
            tools: tools.into_iter().map(Into::into).collect(),
            max_chars: DEFAULT_CATALOG_CHARS,
        }
    }

    pub fn on(mut self, platform: impl Into<String>) -> Self {
        self.platform = platform.into();
        self
    }

    pub fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars;
        self
    }
}

fn normalize_platform(name: &str) -> String {
    match name.trim().to_ascii_lowercase().as_str() {
        "mac" | "macos" | "darwin" | "osx" => "macos".to_string(),
        other => other.to_string(),
    }
}

/// §5.6 的搜索路径，有序：
///
/// ```text
/// 配置里声明的目录
/// <workspace>/skills, <workspace>/.claude/skills   项目自带
/// ~/.komo/skills                                    主目录
/// ~/.agents/skills, ~/.claude/skills                与其他本地 agent 共享，只读
/// ```
///
/// `home` 是**真实家目录**（不是 `KOMO_HOME`）：`~/.agents` 与 `~/.claude` 是别的 agent
/// 也在读的目录，跟着 komo 的数据目录搬家就找不到了。
pub fn runtime_skill_dirs(
    snapshot: &ConfigSnapshot,
    workspace: Option<&Path>,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = snapshot.paths.skill_dirs.clone();
    if let Some(workspace) = workspace {
        dirs.push(workspace.join("skills"));
        dirs.push(workspace.join(".claude").join("skills"));
    }
    dirs.push(snapshot.start_only.data_dir.join("skills"));
    if let Some(home) = home {
        dirs.push(home.join(".agents").join("skills"));
        dirs.push(home.join(".claude").join("skills"));
    }
    // 同一个目录出现两次没有意义，而去重要保**第一次**出现的位置。
    let mut seen = BTreeSet::new();
    dirs.retain(|dir| seen.insert(dir.clone()));
    dirs
}

/// 目录的读者。**不缓存**：每次查询重扫（§5.6）。
#[derive(Debug, Clone)]
pub struct SkillRegistry {
    dirs: Vec<PathBuf>,
    disabled_file: Option<PathBuf>,
}

impl SkillRegistry {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        SkillRegistry {
            dirs,
            disabled_file: None,
        }
    }

    /// 从一份快照接线；`disable` 的标记落在 `runtime_dir` 下。
    pub fn from_snapshot(
        snapshot: &ConfigSnapshot,
        workspace: Option<&Path>,
        home: Option<&Path>,
    ) -> Self {
        SkillRegistry::new(runtime_skill_dirs(snapshot, workspace, home))
            .with_disabled_file(snapshot.paths.runtime_dir.join("skills-disabled.json"))
    }

    /// `disable` 把名字写进这个文件——**不删文件**（§5.6）。
    pub fn with_disabled_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.disabled_file = Some(path.into());
        self
    }

    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// 全部 skills，按搜索路径顺序，**同名先到先得**。被盖住的那一份也在列表里，带着
    /// `shadowed_by`——`komo skills list` 要说得出"你改的是哪一个文件"。
    pub fn list(&self) -> Vec<Skill> {
        let mut found: Vec<Skill> = Vec::new();
        for dir in &self.dirs {
            for skill in scan_dir(dir) {
                let winner = found
                    .iter()
                    .find(|existing| existing.name == skill.name && existing.shadowed_by.is_none())
                    .map(|existing| existing.path.clone());
                found.push(Skill {
                    shadowed_by: winner,
                    ..skill
                });
            }
        }
        found
    }

    /// 这个名字解析到哪一份（先到先得的那一份）。
    pub fn find(&self, name: &str) -> Option<Skill> {
        self.list()
            .into_iter()
            .find(|skill| skill.name == name && skill.shadowed_by.is_none())
    }

    /// 连正文一起读出来。**`disable` 与门控都不影响它**——`komo skills inspect` 要看得到。
    pub fn inspect(&self, name: &str) -> Option<SkillDocument> {
        let skill = self.find(name)?;
        let text = std::fs::read_to_string(&skill.path).ok()?;
        let (_, body) = frontmatter::split(&text);
        Some(SkillDocument {
            body: body.trim_start().to_string(),
            skill,
        })
    }

    /// 系统提示里的那份目录：过掉被盖住的、被 `disable` 的、门控不过的，再按总量定形状。
    pub fn catalog(&self, context: &OfferContext) -> Vec<Skill> {
        self.catalog_of(context).skills
    }

    /// 门控后的目录（值，`docs/agent.md` §14）：Gateway 在 I/O 阶段读一次这个活注册表，
    /// 交给 `komo-agent` 的 `ContextInput.skills`；渲染是 [`SkillCatalog::prompt_block`]
    /// 纯做的那一步。
    pub fn offer(&self, context: &OfferContext) -> SkillCatalog {
        self.catalog_of(context)
    }

    /// 候选与形状。
    ///
    /// **列全比列得详细更要紧**：一条 skill 不在目录里，模型就不知道它存在（真实会话里它为
    /// 了找 `log-diagnosis` 去 `ls` 了整个目录，然后一路找下去）。所以"名字 + 一句描述"整批
    /// 装得下就用它，装不下就退成**只有名字**，而不是按顺序砍掉后面那些人。
    fn catalog_of(&self, context: &OfferContext) -> SkillCatalog {
        let candidates = self.candidates(context);
        let full: usize = candidates
            .iter()
            .map(|skill| cost(&skill.catalog_line()))
            .sum();
        let (skills, shape, dropped) = if full <= context.max_chars {
            (candidates, Shape::Full, 0)
        } else {
            let total = candidates.len();
            let mut skills = Vec::new();
            let mut used = 0usize;
            for (index, skill) in candidates.iter().enumerate() {
                // 末尾那句"另有 N 条没列出来"也要在预算里——它是**要说的那句话**，不是装饰。
                let rest = total - index - 1;
                let note = if rest == 0 { 0 } else { cost(&note_text(rest)) };
                let line = cost(&format!("- {}", skill.name));
                if used + line + note > context.max_chars {
                    break;
                }
                used += line;
                skills.push(skill.clone());
            }
            let dropped = total - skills.len();
            if dropped > 0 {
                tracing::debug!(dropped, "名字都装不下：目录行到此为止");
            }
            (skills, Shape::NamesOnly, dropped)
        };
        // 根只列真的出了条目的那几个（§5.6），顺序即注册表的搜索顺序。
        let roots: Vec<String> = self
            .dirs
            .iter()
            .filter(|dir| skills.iter().any(|skill| skill.path.starts_with(dir)))
            .map(|dir| dir.display().to_string())
            .collect();
        SkillCatalog {
            skills,
            shape,
            dropped,
            roots,
        }
    }

    /// 能进目录的候选（顺序 = 搜索顺序）：过掉被盖住的、被 `disable` 的、门控不过的。
    fn candidates(&self, context: &OfferContext) -> Vec<Skill> {
        let disabled = self.disabled();
        self.list()
            .into_iter()
            .filter(|skill| skill.shadowed_by.is_none() && !disabled.contains(&skill.name))
            .filter(|skill| skill.offered(context))
            .collect()
    }

    /// 渲染好的目录正文。
    pub fn catalog_text(&self, context: &OfferContext) -> String {
        self.catalog_of(context).text()
    }

    /// 系统提示里要拼的那一块（§5.6）。**只是转手**：discovery（[`Self::offer`]）与
    /// presentation（[`SkillCatalog::prompt_block`]）两步现在分开了，这个方法留给别的
    /// 调用方——一步到位不想自己捏一份 `OfferContext` 之后又构造一次目录。
    pub fn prompt_block(&self, context: &OfferContext) -> Option<String> {
        self.offer(context).prompt_block()
    }

    /// 被 `disable` 隐藏的名字。
    pub fn disabled(&self) -> BTreeSet<String> {
        let Some(path) = &self.disabled_file else {
            return BTreeSet::new();
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return BTreeSet::new();
        };
        serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "disable 名单读不出来，按空名单处理");
            BTreeSet::new()
        })
    }

    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled().contains(name)
    }

    /// 从目录行里隐藏一条。**不删文件。**
    pub fn disable(&self, name: &str) -> std::io::Result<()> {
        let mut disabled = self.disabled();
        disabled.insert(name.to_string());
        self.write_disabled(&disabled)
    }

    pub fn enable(&self, name: &str) -> std::io::Result<()> {
        let mut disabled = self.disabled();
        disabled.remove(name);
        self.write_disabled(&disabled)
    }

    fn write_disabled(&self, disabled: &BTreeSet<String>) -> std::io::Result<()> {
        let Some(path) = &self.disabled_file else {
            return Err(std::io::Error::other(
                "这个 SkillRegistry 没有 disable 名单文件",
            ));
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(disabled)?)
    }
}

/// 一个目录下的 `<name>/SKILL.md`。
fn scan_dir(dir: &Path) -> Vec<Skill> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // 目录不存在是常态（共享目录多半没有），不是错误。
        return Vec::new();
    };
    let mut skills: Vec<Skill> = entries
        .flatten()
        .filter_map(|entry| read_skill(&entry.path()))
        .collect();
    // 同一个目录里的顺序要稳定，否则目录行会随文件系统的心情变。
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn read_skill(dir: &Path) -> Option<Skill> {
    if !dir.is_dir() {
        return None;
    }
    let path = dir.join("SKILL.md");
    let text = std::fs::read_to_string(&path).ok()?;
    let (front, _) = frontmatter::split(&text);

    let fallback = dir.file_name()?.to_string_lossy().to_string();
    let mut issues = Vec::new();
    if front.name.is_none() {
        issues.push("frontmatter 里没有 name，按目录名当名字".to_string());
    }
    if front.description.is_none() {
        issues.push("frontmatter 里没有 description，不进提示目录".to_string());
    }

    Some(Skill {
        name: front.name.unwrap_or(fallback),
        description: front.description.unwrap_or_default(),
        platforms: front.platforms,
        requires_tools: front.requires_tools,
        path,
        shadowed_by: None,
        issues,
    })
}

#[cfg(test)]
mod tests;
