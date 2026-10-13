//! 系统提示正文：工作目录、skills 目录的位置、主 / 子代理共用的 [`rules`]。

use std::path::Path;

use komo_kernel::types::delegate::DelegateSpec;
use komo_kernel::types::ids::{RunId, ToolCallId};

use super::*;
use crate::skills::{OfferContext, SkillRegistry};

/// 工具名不在正文里再列一遍：API 的 tool schema 里已经有一份（学 pi）。
#[test]
fn the_system_prompt_names_the_workspace_and_leaves_tools_to_the_schema() {
    let prompt = build(
        Path::new("/tmp/w"),
        &["read".to_string()],
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(prompt.contains("/tmp/w"), "{prompt}");
    assert!(!prompt.contains("read"), "{prompt}");
}

/// §5.6 的目录行拼在**后面**（原来那段正文一句不少），空的时候一个字都不多。
#[test]
fn the_system_prompt_carries_the_skills_catalog_after_the_base_text() {
    let tools = vec!["read".to_string()];
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("pr-review")).unwrap();
    std::fs::write(
        dir.path().join("pr-review").join("SKILL.md"),
        "---\nname: pr-review\ndescription: 怎么审一个 PR\n---\n正文",
    )
    .unwrap();
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);
    let catalog = registry.offer(&OfferContext::here(tools.clone()));

    let prompt = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Main,
        &[],
        Some(&catalog),
    );
    assert!(
        prompt.ends_with(&catalog.prompt_block().expect("有目录行")),
        "{prompt}"
    );
    assert!(prompt.contains("你是 komo"), "{prompt}");

    let bare = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(!bare.contains("Skills"), "{bare}");
    assert_eq!(bare.lines().count(), 4, "{bare}");
}

/// 「搜代码用 rg，别用 shell 拼 grep / find / ls」是**两条提示共用**的一句
/// （[`rules`]）：子代理也要照它做，所以它不能只写在主对话那一份里；没挂 `rg` 时两边都不说。
#[test]
fn both_prompts_tell_the_model_to_search_with_rg() {
    let tools = vec!["rg".to_string()];
    let main = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(main.contains("别用 shell 拼 grep"), "{main}");
    let without_rg = build(
        Path::new("/tmp/w"),
        &["read".to_string()],
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(!without_rg.contains("rg"), "{without_rg}");

    let spec = DelegateSpec::new(
        RunId::from_raw("run-1"),
        ToolCallId::from_raw("call-1"),
        "查一下调用方",
    );
    let sub = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Delegated(spec),
        &[],
        None,
    );
    assert!(sub.contains("别用 shell 拼 grep"), "{sub}");
}

/// 挂了 `update_plan` 才提计划，主对话与子代理一样。
#[test]
fn both_prompts_mention_the_plan_only_when_update_plan_is_mounted() {
    let tools = vec!["read".to_string(), UPDATE_PLAN_TOOL.to_string()];
    let main = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(main.contains("update_plan 登记计划"), "{main}");
    let sub = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Delegated(DelegateSpec::new(
            RunId::from_raw("run-1"),
            ToolCallId::from_raw("call-1"),
            "查一下调用方",
        )),
        &[],
        None,
    );
    assert!(sub.contains("update_plan 登记计划"), "{sub}");
    let without = build(
        Path::new("/tmp/w"),
        &["read".to_string()],
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(!without.contains("update_plan"), "{without}");
}

/// 主 Agent 挂了 `shell` 时，提示里说它跑在 komo 里、komo 的状态用 komo CLI 查；排在
/// skills 目录前面。
#[test]
fn the_main_prompt_tells_the_model_to_query_komo_with_its_cli() {
    let tools = vec!["read".to_string(), "shell".to_string()];
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("cron-scheduler")).unwrap();
    std::fs::write(
        dir.path().join("cron-scheduler").join("SKILL.md"),
        "---\nname: cron-scheduler\ndescription: 系统 cron\n---\n正文",
    )
    .unwrap();
    let catalog = SkillRegistry::new(vec![dir.path().to_path_buf()])
        .offer(&OfferContext::here(tools.clone()));

    let prompt = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Main,
        &[],
        Some(&catalog),
    );
    assert!(prompt.contains("komo 自己的状态"), "{prompt}");
    assert!(prompt.contains("`komo <子命令>`"), "{prompt}");
    assert!(prompt.contains("cron list"), "{prompt}");
    let own = prompt.find("komo 自己的状态").unwrap();
    let skills = prompt.find("cron-scheduler").unwrap();
    assert!(own < skills, "{prompt}");
    assert_eq!(
        prompt,
        build(
            Path::new("/tmp/w"),
            &tools,
            &InvocationContext::Main,
            &[],
            Some(&catalog),
        ),
        "同一份输入逐字相同（提示前缀缓存）"
    );
}

/// 没有 `shell`（例如只给了 `dispatch` / `follow` 的 Profile）或是子代理时，这一段不出现：前者说了
/// 也跑不了，后者只拿任务里写的东西（§4）。
#[test]
fn the_komo_section_needs_shell_and_the_main_agent() {
    let dispatcher = build(
        Path::new("/tmp/w"),
        &["dispatch".to_string(), "follow".to_string()],
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(!dispatcher.contains("komo 自己的状态"), "{dispatcher}");

    let spec = DelegateSpec::new(
        RunId::from_raw("run-1"),
        ToolCallId::from_raw("call-1"),
        "查一下调用方",
    );
    let sub = build(
        Path::new("/tmp/w"),
        &["shell".to_string()],
        &InvocationContext::Delegated(spec),
        &[],
        None,
    );
    assert!(!sub.contains("komo 自己的状态"), "{sub}");
}

/// 挂了 `codemode` 才说"多处读、搜用 codemode 一次做完"；主 / 子代理都说（共用 [`rules`]）。
#[test]
fn both_prompts_steer_batched_reads_to_codemode_only_when_it_is_mounted() {
    let tools = vec!["read".to_string(), "codemode".to_string()];
    let main = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(main.contains("用 codemode 写一段脚本一次做完"), "{main}");
    let sub = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Delegated(DelegateSpec::new(
            RunId::from_raw("run-1"),
            ToolCallId::from_raw("call-1"),
            "查一下调用方",
        )),
        &[],
        None,
    );
    assert!(sub.contains("用 codemode 写一段脚本一次做完"), "{sub}");
    let without = build(
        Path::new("/tmp/w"),
        &["read".to_string(), "rg".to_string()],
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(!without.contains("codemode"), "{without}");
}

/// "看文件用 read 别用 cat"只在 `read` 与 `shell` 都挂着时出现：缺了哪一个这句都是空话。
#[test]
fn the_read_over_cat_rule_needs_both_read_and_shell() {
    let both = build(
        Path::new("/tmp/w"),
        &["read".to_string(), "shell".to_string()],
        &InvocationContext::Main,
        &[],
        None,
    );
    assert!(both.contains("看文件用 read"), "{both}");
    for tools in [vec!["read".to_string()], vec!["shell".to_string()]] {
        let prompt = build(
            Path::new("/tmp/w"),
            &tools,
            &InvocationContext::Main,
            &[],
            None,
        );
        assert!(!prompt.contains("看文件用 read"), "{prompt}");
    }
}

fn project_file(path: &str, content: &str) -> ProjectFile {
    ProjectFile {
        path: path.into(),
        content: content.into(),
    }
}

/// 项目规范排在 komo 自查之后、skills 目录之前，按给的顺序（外层在前）原样包进
/// `<project_instructions>`；文件末尾的换行不多留一个空行。
#[test]
fn the_main_prompt_puts_project_files_between_the_komo_section_and_skills() {
    let tools = vec!["read".to_string(), "shell".to_string()];
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("pr-review")).unwrap();
    std::fs::write(
        dir.path().join("pr-review").join("SKILL.md"),
        "---\nname: pr-review\ndescription: 怎么审一个 PR\n---\n正文",
    )
    .unwrap();
    let catalog = SkillRegistry::new(vec![dir.path().to_path_buf()])
        .offer(&OfferContext::here(tools.clone()));
    let files = [
        project_file("/code/AGENTS.md", "外层规范\n"),
        project_file("/code/repo/AGENTS.md", "仓库规范"),
    ];

    let prompt = build(
        Path::new("/code/repo"),
        &tools,
        &InvocationContext::Main,
        &files,
        Some(&catalog),
    );
    let own = prompt.find("komo 自己的状态").unwrap();
    let outer = prompt
        .find("<project_instructions path=\"/code/AGENTS.md\">\n外层规范\n</project_instructions>")
        .expect("外层那一份原样在提示里");
    let inner = prompt
        .find("<project_instructions path=\"/code/repo/AGENTS.md\">\n仓库规范\n</project_instructions>")
        .expect("工作目录那一份原样在提示里");
    let skills = prompt.find("pr-review").unwrap();
    assert!(own < outer && outer < inner && inner < skills, "{prompt}");
}

/// 子代理只看得见任务（§4）：给了也不渲染。没有、或者正文全是空白时，主 Agent 的提示里
/// 也一个字都不多。
#[test]
fn project_files_reach_only_the_main_prompt_and_only_when_non_blank() {
    let tools = vec!["read".to_string()];
    let files = [project_file("/w/AGENTS.md", "先读再改。")];
    let spec = DelegateSpec::new(
        RunId::from_raw("run-1"),
        ToolCallId::from_raw("call-1"),
        "查一下调用方",
    );
    let sub = build(
        Path::new("/w"),
        &tools,
        &InvocationContext::Delegated(spec),
        &files,
        None,
    );
    assert!(!sub.contains("先读再改"), "{sub}");
    assert!(!sub.contains("project_instructions"), "{sub}");

    let bare = build(Path::new("/w"), &tools, &InvocationContext::Main, &[], None);
    let blank = build(
        Path::new("/w"),
        &tools,
        &InvocationContext::Main,
        &[project_file("/w/AGENTS.md", " \n\n")],
        None,
    );
    assert_eq!(blank, bare, "空白的规范文件不留空标题");
    assert!(!bare.contains("项目规范"), "{bare}");
}
