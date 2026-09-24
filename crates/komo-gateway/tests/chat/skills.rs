//! §5.6：skills **进系统提示**——目录行（名字 + 一句描述，总量有上限）是启动快照。
//!
//! 这一段曾经整个缺席：`SkillRegistry::catalog` 那套门控写好了、`komo skills list` 也读
//! 得到，但 Gateway 里没有一处把它拼进系统提示，于是模型**不知道有 skills 这回事**，
//! 更不会去 `read` 那个 `SKILL.md`。这里证的就是那条线：
//!
//! 1. 适用本平台 / 这套工具的 skill 在提示里，不适用的不在；
//! 2. 被 `disable` 的不在（一个都不在时提示里**一个字都不多**）；
//! 3. 配置重载会按新快照重算（目录行是启动快照，重载是唯一会动它的时刻）。

use std::path::Path;
use std::sync::Arc;

use komo_kernel::traits::LlmClient;
use komo_kernel::types::ids::SessionId;

use crate::harness::{FakeLlm, Gw, Home, config_toml, telegram_block};

/// 一个 `<root>/<name>/SKILL.md`。
fn write_skill(root: &Path, name: &str, front: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).expect("建 skill 目录");
    std::fs::write(dir.join("SKILL.md"), format!("---\n{front}---\n{body}")).expect("写 SKILL.md");
}

/// 配置里带上 skills 目录（config 里的相对路径按数据目录解析，这里给绝对的）。
fn config_with_skills(skill_dirs: &[&Path]) -> String {
    let dirs: Vec<String> = skill_dirs
        .iter()
        .map(|dir| format!("\"{}\"", dir.display()))
        .collect();
    config_toml(&format!(
        "[paths]\nskill_dirs = [{}]\n{}",
        dirs.join(", "),
        telegram_block("111")
    ))
}

/// 跑一句到终态，返回这一轮交给模型的系统提示。
async fn prompt_of(gw: &Gw, llm: &FakeLlm, session: &SessionId, text: &str, key: &str) -> String {
    let before = llm.turns();
    let run = gw.submit(session, key, text).await.run;
    gw.wait_terminal(&run).await;
    let requests = llm.requests.lock().expect("脚本模型");
    assert!(requests.len() > before, "这一轮没到模型：{requests:?}");
    requests[before].system_prompt.clone()
}

#[tokio::test]
async fn the_system_prompt_lists_the_skills_the_model_can_read() {
    let skills = tempfile::tempdir().expect("临时 skills 目录");
    write_skill(
        skills.path(),
        "pr-review",
        "name: pr-review\ndescription: 怎么审一个 PR\n",
        "第一步……\n",
    );
    // 只给别的平台的：这台机器上**不该**进提示（§5.6：`platforms:` 只门控目录行）。
    let elsewhere = if std::env::consts::OS == "macos" {
        "linux"
    } else {
        "macos"
    };
    write_skill(
        skills.path(),
        "finder-tricks",
        &format!("name: finder-tricks\ndescription: Finder 脚本\nplatforms: [{elsewhere}]\n"),
        "……\n",
    );

    let home = Home::with_config(&config_with_skills(&[skills.path()]));
    let llm = FakeLlm::finisher("看过了。");
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;

    let prompt = prompt_of(&gw, &llm, &session, "审一下这个 PR", "skills-1").await;
    assert!(
        prompt.contains("- pr-review：怎么审一个 PR"),
        "目录行要在系统提示里：{prompt}"
    );
    assert!(
        prompt.contains(&skills.path().display().to_string()),
        "根也要在，模型才找得到那个 SKILL.md：{prompt}"
    );
    assert!(
        !prompt.contains("finder-tricks"),
        "平台不满足的不进提示（但 `komo skills inspect` 照样看得到）：{prompt}"
    );
    // 拼的是**追加**，原来那段正文还在。
    assert!(prompt.contains("你是 komo"), "{prompt}");
}

#[tokio::test]
async fn a_disabled_skill_leaves_no_trace_in_the_prompt() {
    let skills = tempfile::tempdir().expect("临时 skills 目录");
    // 两个：disable 掉的那个不在，"一个都没有"这件事要另说（见 doc：标题不该留下）。
    write_skill(
        skills.path(),
        "pr-review",
        "name: pr-review\ndescription: 怎么审一个 PR\n",
        "第一步……\n",
    );

    let home = Home::with_config(&config_with_skills(&[skills.path()]));
    let llm = FakeLlm::finisher("看过了。");
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;

    let before = prompt_of(&gw, &llm, &session, "在吗", "skills-1").await;
    assert!(before.contains("pr-review"), "{before}");

    // `komo skills disable`：名单落在 runtime 下，重载之后系统提示里就没有它了。
    std::fs::write(
        home.path().join("runtime").join("skills-disabled.json"),
        "[\"pr-review\"]",
    )
    .expect("写 disable 名单");
    komo_gateway::reload::reload(gw.state())
        .await
        .expect("新配置装得上");

    let after = prompt_of(&gw, &llm, &session, "现在呢", "skills-2").await;
    assert!(!after.contains("pr-review"), "{after}");
    assert!(
        !after.contains("Skills"),
        "一条可露面的都没有时，提示里不该留下一个空标题：{after}"
    );
}

#[tokio::test]
async fn a_reload_picks_up_a_skill_written_after_startup() {
    let skills = tempfile::tempdir().expect("临时 skills 目录");
    let home = Home::with_config(&config_with_skills(&[skills.path()]));
    let llm = FakeLlm::finisher("看过了。");
    let gw = home.start(Arc::clone(&llm) as Arc<dyn LlmClient>).await;
    let session = gw.open_session().await;

    // 启动时目录还是空的。
    let before = prompt_of(&gw, &llm, &session, "在吗", "skills-1").await;
    assert!(!before.contains("Skills"), "{before}");

    // 人放了一个 skill 进去，然后重载：目录行是启动快照，重载是唯一会动它的时刻。
    write_skill(
        skills.path(),
        "pr-review",
        "name: pr-review\ndescription: 怎么审一个 PR\n",
        "第一步……\n",
    );
    komo_gateway::reload::reload(gw.state())
        .await
        .expect("新配置装得上");

    let after = prompt_of(&gw, &llm, &session, "现在呢", "skills-2").await;
    assert!(after.contains("- pr-review：怎么审一个 PR"), "{after}");
}
