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
// TODO(decide: §5.6 只说"总量有上限"，没有给数。2000 字符大约是几十条"名字 + 一句描述"，
// 在一个每轮都要发的前缀里是可以接受的量级；真要定，应当按实测的前缀大小改这里。
pub const DEFAULT_CATALOG_CHARS: usize = 2_000;

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
        format!("- {}：{}", self.name, self.description)
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

    /// 系统提示里的那份目录：过掉被盖住的、被 `disable` 的、门控不过的，再按总量截断。
    pub fn catalog(&self, context: &OfferContext) -> Vec<Skill> {
        let disabled = self.disabled();
        let mut lines = Vec::new();
        let mut used = 0usize;
        for skill in self.list() {
            if skill.shadowed_by.is_some() || disabled.contains(&skill.name) {
                continue;
            }
            if !skill.offered(context) {
                continue;
            }
            let cost = skill.catalog_line().chars().count() + 1;
            if used + cost > context.max_chars {
                tracing::debug!(
                    skill = %skill.name,
                    "目录行已到总量上限，这一条不进系统提示"
                );
                continue;
            }
            used += cost;
            lines.push(skill);
        }
        lines
    }

    /// 渲染好的目录正文。
    pub fn catalog_text(&self, context: &OfferContext) -> String {
        self.catalog(context)
            .iter()
            .map(Skill::catalog_line)
            .collect::<Vec<_>>()
            .join("\n")
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
