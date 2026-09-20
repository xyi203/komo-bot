//! 目录真的在磁盘上，扫描真的在读它。

use super::*;

fn write_skill(dir: &Path, name: &str, front: &str, body: &str) -> PathBuf {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    let path = skill_dir.join("SKILL.md");
    std::fs::write(&path, format!("---\n{front}---\n{body}")).unwrap();
    path
}

fn context() -> OfferContext {
    OfferContext::here(["read", "write", "edit", "shell", "python"]).on("linux")
}

#[test]
fn a_new_skill_file_shows_up_on_the_very_next_query() {
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);
    assert!(registry.list().is_empty());

    write_skill(
        dir.path(),
        "pr-review",
        "name: pr-review\ndescription: 怎么审一个 PR\n",
        "第一步……\n",
    );

    // **没有重启，没有重新构造 registry。**
    let listed = registry.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "pr-review");
    assert_eq!(listed[0].description, "怎么审一个 PR");
    assert_eq!(
        registry.catalog_text(&context()),
        "- pr-review：怎么审一个 PR"
    );
}

#[test]
fn the_search_path_is_ordered_and_the_first_name_wins() {
    let project = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let first = write_skill(
        project.path(),
        "deploy",
        "name: deploy\ndescription: 项目自带的那份\n",
        "",
    );
    write_skill(
        shared.path(),
        "deploy",
        "name: deploy\ndescription: 共享的那份\n",
        "",
    );

    let registry = SkillRegistry::new(vec![
        project.path().to_path_buf(),
        shared.path().to_path_buf(),
    ]);
    let resolved = registry.find("deploy").unwrap();
    assert_eq!(resolved.path, first);
    assert_eq!(resolved.description, "项目自带的那份");

    // 被盖住的那一份仍然列得出来——要说得清"你改的是哪个文件"。
    let shadowed: Vec<_> = registry
        .list()
        .into_iter()
        .filter(|s| s.shadowed_by.is_some())
        .collect();
    assert_eq!(shadowed.len(), 1);
    assert_eq!(shadowed[0].shadowed_by.as_ref(), Some(&first));

    // 目录行只出一条。
    assert_eq!(registry.catalog(&context()).len(), 1);
}

/// §5.6：`requires_tools` 不满足时**不出现在提示目录里，但仍可 inspect**。
#[test]
fn a_skill_whose_tools_are_missing_is_hidden_from_the_catalog_but_still_inspectable() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(
        dir.path(),
        "nas-backup",
        "name: nas-backup\ndescription: 备份到 NAS\nrequires_tools: [shell, py__rsync]\n",
        "先挂载……\n",
    );
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);

    let without = OfferContext::here(["shell"]).on("linux");
    assert!(registry.catalog(&without).is_empty(), "工具不齐就不进目录");

    let document = registry
        .inspect("nas-backup")
        .expect("inspect 不受门控影响");
    assert_eq!(document.body, "先挂载……\n");
    assert_eq!(document.skill.requires_tools, vec!["shell", "py__rsync"]);

    let with = OfferContext::here(["shell", "py__rsync"]).on("linux");
    assert_eq!(registry.catalog(&with).len(), 1);
}

#[test]
fn a_platform_declaration_also_only_gates_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(
        dir.path(),
        "mac-only",
        "name: mac-only\ndescription: 只在 Mac 上有意义\nplatforms: [macos]\n",
        "",
    );
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);

    assert!(registry.catalog(&context()).is_empty());
    assert!(registry.find("mac-only").is_some());
    assert_eq!(
        registry
            .catalog(&OfferContext::here(["read"]).on("darwin"))
            .len(),
        1,
        "darwin / macos 是同一个平台"
    );
}

/// `disable` 只从目录行隐藏，**不删文件**。
#[test]
fn disable_hides_the_catalog_line_and_leaves_the_file_alone() {
    let dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let path = write_skill(
        dir.path(),
        "pr-review",
        "name: pr-review\ndescription: 怎么审一个 PR\n",
        "正文\n",
    );
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()])
        .with_disabled_file(state.path().join("skills-disabled.json"));

    registry.disable("pr-review").unwrap();
    assert!(registry.is_disabled("pr-review"));
    assert!(registry.catalog(&context()).is_empty());
    assert!(path.exists(), "文件还在");
    assert!(registry.find("pr-review").is_some(), "仍然解析得到");
    assert!(registry.inspect("pr-review").is_some());

    registry.enable("pr-review").unwrap();
    assert_eq!(registry.catalog(&context()).len(), 1);
}

#[test]
fn a_file_without_a_description_is_listed_with_its_problem_but_kept_out_of_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("bare")).unwrap();
    std::fs::write(dir.path().join("bare").join("SKILL.md"), "就是一段正文\n").unwrap();

    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);
    let listed = registry.list();
    assert_eq!(listed[0].name, "bare", "没有 name 就用目录名");
    assert_eq!(listed[0].issues.len(), 2);
    assert!(registry.catalog(&context()).is_empty());
    assert_eq!(registry.inspect("bare").unwrap().body, "就是一段正文\n");
}

#[test]
fn the_catalog_stops_at_its_total_budget() {
    let dir = tempfile::tempdir().unwrap();
    for n in 0..10 {
        write_skill(
            dir.path(),
            &format!("skill-{n}"),
            &format!("name: skill-{n}\ndescription: 一句很长很长很长很长的描述\n"),
            "",
        );
    }
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);
    let tight = context().with_max_chars(60);
    let catalog = registry.catalog(&tight);
    assert!(catalog.len() < 10, "上限是上限：{}", catalog.len());
    assert!(registry.catalog_text(&tight).chars().count() <= 60);
}

#[test]
fn a_directory_that_does_not_exist_is_not_an_error() {
    let registry = SkillRegistry::new(vec![PathBuf::from("/definitely/not/here")]);
    assert!(registry.list().is_empty());
    assert!(registry.catalog(&context()).is_empty());
}

#[test]
fn a_loose_file_in_the_skills_directory_is_not_a_skill() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "不是 skill").unwrap();
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);
    assert!(registry.list().is_empty());
}

/// §5.6 的搜索路径顺序。
#[test]
fn the_default_search_path_follows_the_documented_order() {
    let mut snapshot = crate::config::testing::snapshot_fixture();
    snapshot.paths.skill_dirs = vec![PathBuf::from("/configured/skills")];
    snapshot.start_only.data_dir = PathBuf::from("/home/u/.komo");

    let dirs = runtime_skill_dirs(
        &snapshot,
        Some(Path::new("/work/project")),
        Some(Path::new("/home/u")),
    );
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/configured/skills"),
            PathBuf::from("/work/project/skills"),
            PathBuf::from("/work/project/.claude/skills"),
            PathBuf::from("/home/u/.komo/skills"),
            PathBuf::from("/home/u/.agents/skills"),
            PathBuf::from("/home/u/.claude/skills"),
        ]
    );
}

#[test]
fn the_same_directory_listed_twice_is_only_searched_once() {
    let mut snapshot = crate::config::testing::snapshot_fixture();
    snapshot.start_only.data_dir = PathBuf::from("/home/u/.komo");
    snapshot.paths.skill_dirs = vec![PathBuf::from("/home/u/.komo/skills")];
    let dirs = runtime_skill_dirs(&snapshot, None, None);
    assert_eq!(dirs, vec![PathBuf::from("/home/u/.komo/skills")]);
}

/// §5.6：拼进系统提示的那一块——根在前、目录行在后。
#[test]
fn the_prompt_block_names_the_roots_in_search_order_then_the_lines() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    // 两个目录各一个同名 skill：先到先得，只有一份进目录行，两个根里也只该出现
    // **真的出了条目的那一份**。
    write_skill(
        first.path(),
        "pr-review",
        "name: pr-review\ndescription: 生效的那一份\n",
        "第一步……\n",
    );
    write_skill(
        second.path(),
        "pr-review",
        "name: pr-review\ndescription: 被盖住的那一份\n",
        "……\n",
    );
    write_skill(
        second.path(),
        "release-notes",
        "name: release-notes\ndescription: 怎么写发布说明\n",
        "……\n",
    );
    let registry = SkillRegistry::new(vec![
        first.path().to_path_buf(),
        second.path().to_path_buf(),
    ]);

    let block = registry.prompt_block(&context()).expect("有目录行");
    assert!(block.contains("- pr-review：生效的那一份"), "{block}");
    assert!(!block.contains("被盖住的那一份"), "同名只出一条：{block}");
    assert!(block.contains("- release-notes：怎么写发布说明"), "{block}");
    // 根的先后就是搜索顺序：模型顺着找，先撞上的那份正是生效的那一份。
    let first_root = block.find(&first.path().display().to_string());
    let second_root = block.find(&second.path().display().to_string());
    assert!(first_root.is_some() && second_root.is_some(), "{block}");
    assert!(first_root < second_root, "{block}");
}

#[test]
fn an_empty_catalog_is_not_a_block() {
    let dir = tempfile::tempdir().unwrap();
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);
    assert_eq!(registry.prompt_block(&context()), None);
}

#[test]
fn a_gated_or_disabled_skill_leaves_the_block_empty() {
    let dir = tempfile::tempdir().unwrap();
    // 只给 macos 的，而这里问的是 linux。
    write_skill(
        dir.path(),
        "finder-tricks",
        "name: finder-tricks\ndescription: Finder 脚本\nplatforms: [macos]\n",
        "……\n",
    );
    let registry = SkillRegistry::new(vec![dir.path().to_path_buf()]);
    assert_eq!(
        registry.prompt_block(&context()),
        None,
        "平台不满足就不该露面"
    );
}
