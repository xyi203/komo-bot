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
    );
    assert!(
        prompt.ends_with(&catalog.prompt_block().expect("有目录行")),
        "{prompt}"
    );
    assert!(prompt.contains("你是 komo"), "{prompt}");

    let bare = build(Path::new("/tmp/w"), &tools, &InvocationContext::Main, None);
    assert!(!bare.contains("Skills"), "{bare}");
    assert_eq!(bare.lines().count(), 6, "{bare}");
}

/// 「搜代码用 rg，不要用 shell 里的 grep / find / ls」是**两条提示共用**的一句
/// （[`RULES`]）：子代理也要照它做，所以它不能只写在主对话那一份里。
#[test]
fn both_prompts_tell_the_model_to_search_with_rg() {
    let tools = vec!["rg".to_string()];
    let main = build(Path::new("/tmp/w"), &tools, &InvocationContext::Main, None);
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
    );
    assert!(sub.contains("不要用 shell 里的 grep"), "{sub}");
}
