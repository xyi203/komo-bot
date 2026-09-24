//! 系统提示正文：工具名、skills 目录的位置、主 / 子代理共用的 `RULES`。

use std::path::Path;

use komo_kernel::types::delegate::DelegateSpec;
use komo_kernel::types::ids::{RunId, ToolCallId};

use super::*;
use crate::skills::{OfferContext, SkillRegistry};

#[test]
fn the_system_prompt_names_the_tools_that_are_actually_mounted() {
    let prompt = build(
        Path::new("/tmp/w"),
        &["read".to_string()],
        &InvocationContext::Main,
        None,
        None,
    );
    assert!(prompt.contains("read"), "{prompt}");
    assert!(prompt.contains("/tmp/w"), "{prompt}");
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
        Some(&catalog),
        None,
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
        None,
        None,
    );
    assert!(!bare.contains("Skills"), "{bare}");
    assert_eq!(bare.lines().count(), 6, "{bare}");
}

/// 「搜代码用 rg，不要用 shell 里的 grep / find / ls」是**两条提示共用**的一句
/// （[`RULES`]）：子代理也要照它做，所以它不能只写在主对话那一份里。
#[test]
fn both_prompts_tell_the_model_to_search_with_rg() {
    let tools = vec!["rg".to_string()];
    let main = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Main,
        None,
        None,
    );
    assert!(main.contains("可用工具：rg"), "{main}");
    assert!(main.contains("不要用 shell 里的 grep"), "{main}");

    let spec = DelegateSpec::new(
        RunId::from_raw("run-1"),
        ToolCallId::from_raw("call-1"),
        "查一下调用方",
    );
    let sub = build(
        Path::new("/tmp/w"),
        &tools,
        &InvocationContext::Delegated(spec),
        None,
        None,
    );
    assert!(sub.contains("不要用 shell 里的 grep"), "{sub}");
}

/// 主 Agent 挂了 `shell` 时，提示里说它跑在 komo 里、komo 的状态用 komo CLI 查，并给出
/// Gateway 传进来的那个**绝对路径**（PATH 里没有 komo 也跑得通）；排在 skills 目录前面。
#[test]
fn the_main_prompt_tells_the_model_to_query_komo_with_the_given_exe() {
    let tools = vec!["read".to_string(), "shell".to_string()];
    let exe = Path::new("/opt/komo/bin/komo");
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
        Some(&catalog),
        Some(exe),
    );
    assert!(prompt.contains("你运行在 komo 里"), "{prompt}");
    assert!(
        prompt.contains("komo 可执行文件：/opt/komo/bin/komo"),
        "{prompt}"
    );
    assert!(prompt.contains("cron list"), "{prompt}");
    let own = prompt.find("你运行在 komo 里").unwrap();
    let skills = prompt.find("cron-scheduler").unwrap();
    assert!(own < skills, "{prompt}");
    assert_eq!(
        prompt,
        build(
            Path::new("/tmp/w"),
            &tools,
            &InvocationContext::Main,
            Some(&catalog),
            Some(exe),
        ),
        "同一份输入逐字相同（提示前缀缓存）"
    );
}

/// 没有 `shell`（分发器只有 `dispatch` / `follow`）或是子代理时，这一段不出现：前者说了
/// 也跑不了，后者只拿任务里写的东西（§4）。
#[test]
fn the_komo_section_needs_shell_and_the_main_agent() {
    let exe = Path::new("/opt/komo/bin/komo");
    let dispatcher = build(
        Path::new("/tmp/w"),
        &["dispatch".to_string(), "follow".to_string()],
        &InvocationContext::Main,
        None,
        Some(exe),
    );
    assert!(!dispatcher.contains("你运行在 komo 里"), "{dispatcher}");

    let spec = DelegateSpec::new(
        RunId::from_raw("run-1"),
        ToolCallId::from_raw("call-1"),
        "查一下调用方",
    );
    let sub = build(
        Path::new("/tmp/w"),
        &["shell".to_string()],
        &InvocationContext::Delegated(spec),
        None,
        Some(exe),
    );
    assert!(!sub.contains("你运行在 komo 里"), "{sub}");
}
