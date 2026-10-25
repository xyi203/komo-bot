//! §5.6：skills **进系统提示**（Durable 存储版，D5 W3a：与 `../skills.rs` 同名同断言，
//! Gateway 跑在新存储上，只有起 Gateway 那一行不同）。
//!
//! 原文：skills **进系统提示**——目录行（名字 + 一句描述，总量有上限）是启动快照。
//!
//! 这一段曾经整个缺席：`SkillRegistry::catalog` 那套门控写好了、`komo skills list` 也读
//! 得到，但 Gateway 里没有一处把它拼进系统提示，于是模型**不知道有 skills 这回事**，
//! 更不会去 `read` 那个 `SKILL.md`。这里证的就是那条线：
//!
//! 1. 适用本平台 / 这套工具的 skill 在提示里，不适用的不在；
//! 2. 被 `disable` 的不在（一个都不在时提示里**一个字都不多**）；
//! 3. 配置重载之后**新会话**按新快照重算；
//! 4. 已展示的目录**钉住到下一次 head**（`/new`、压缩；W4b，同 `project` / `memory` 段）：续跑那一段与
//!    第一段逐字相同，同一会话里之后的 Run 也不变，注册表的变化到下一次 head 才生效。
//!
//! 与 Legacy 不同：Legacy 的目录冻结在每条 Run 的受理快照里，"下一条 Run 起生效"；Durable 没有受理快照，
//! 目录是系统段，钉住的粒度是 head（同一会话里的下一条 Run 也看不到新 skill，开新会话或 `/new` 才看得到）。
//! 第 3 条因此改成开新会话再看。

use std::path::Path;
use std::sync::Arc;

use komo_kernel::types::ids::SessionId;
use komo_kernel::types::turn::LlmError;

use crate::harness::{FakeLlm, Gw, Home, config_toml, telegram_block, text_round};

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
    let gw = home.start(Arc::clone(&llm)).await;
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
    let gw = home.start(Arc::clone(&llm)).await;
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

    // 已展示的目录钉住：同一会话里的下一条 Run 仍是那份（改了断言：Legacy 这里就没有了）。
    let pinned = prompt_of(&gw, &llm, &session, "现在呢", "skills-2").await;
    assert!(pinned.contains("pr-review"), "{pinned}");
    // 新会话按新的注册表。
    let fresh = gw.open_session().await;
    let after = prompt_of(&gw, &llm, &fresh, "现在呢", "skills-3").await;
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
    let gw = home.start(Arc::clone(&llm)).await;
    let session = gw.open_session().await;

    // 启动时目录还是空的。
    let before = prompt_of(&gw, &llm, &session, "在吗", "skills-1").await;
    assert!(!before.contains("Skills"), "{before}");

    // 人放了一个 skill 进去，然后重载：之后新开的会话按新的注册表出目录。
    write_skill(
        skills.path(),
        "pr-review",
        "name: pr-review\ndescription: 怎么审一个 PR\n",
        "第一步……\n",
    );
    komo_gateway::reload::reload(gw.state())
        .await
        .expect("新配置装得上");

    // 已展示的"没有"也钉住：同一会话里的下一条 Run 仍然没有（改了断言）。
    let pinned = prompt_of(&gw, &llm, &session, "现在呢", "skills-2").await;
    assert!(!pinned.contains("Skills"), "{pinned}");
    // 新会话按新的注册表。
    let fresh = gw.open_session().await;
    let after = prompt_of(&gw, &llm, &fresh, "现在呢", "skills-3").await;
    assert!(after.contains("- pr-review：怎么审一个 PR"), "{after}");
}

/// 目录钉在已展示的那份里：Run 停在退避上时有人加了 skill 并重载，续跑那一段的系统提示仍与第一段逐字
/// 相同——否则 provider 的前缀缓存从系统提示起整段作废，一条跑了几十轮的 Run 一次就丢掉十几万 token 的
/// 缓存。同一会话里之后的 Run 也一样，新 skill 到下一次 head（`/new`）才出现。
#[tokio::test]
async fn a_resumed_segment_keeps_the_skills_catalog_it_was_accepted_with() {
    let skills = tempfile::tempdir().expect("临时 skills 目录");
    write_skill(
        skills.path(),
        "pr-review",
        "name: pr-review\ndescription: 怎么审一个 PR\n",
        "第一步……\n",
    );

    let home = Home::with_config(&config_with_skills(&[skills.path()]));
    // 第一段的回复没收齐（让出名额等退避），第二段才答完。
    let llm = FakeLlm::new(vec![
        vec![Err(LlmError::Incomplete)],
        vec![text_round(1, "看过了。")],
    ]);
    let gw = home.start(Arc::clone(&llm)).await;
    let session = gw.open_session().await;

    let run = gw.submit(&session, "skills-1", "审一下这个 PR").await.run;
    gw.wait_waiting(&run).await;
    write_skill(
        skills.path(),
        "log-diagnosis",
        "name: log-diagnosis\ndescription: 查日志\n",
        "……\n",
    );
    komo_gateway::reload::reload(gw.state())
        .await
        .expect("新配置装得上");
    gw.wait_terminal(&run).await;

    let (first, resumed) = {
        let requests = llm.requests.lock().expect("脚本模型");
        assert_eq!(requests.len(), 2, "退避到期之后再请求一次：{requests:?}");
        (
            requests[0].system_prompt.clone(),
            requests[1].system_prompt.clone(),
        )
    };
    assert!(first.contains("- pr-review：怎么审一个 PR"), "{first}");
    assert!(!first.contains("log-diagnosis"), "{first}");
    // W4b 做了冻结：skills 段钉住到下一次 head，退避后重发的请求与第一次逐字相同（W3a 当时记下的是
    // "重发的请求带上了期间新增的 skill"这个现状，这里改回 Legacy 的断言）。
    assert_eq!(resumed, first);

    // 同一会话里的下一条 Run 仍是钉住的那份。
    let next = prompt_of(&gw, &llm, &session, "再看看", "skills-2").await;
    assert_eq!(next, first, "同一会话里新 skill 要等下一次 head");

    // `/new` 之后才出现。
    gw.state()
        .core()
        .expect("Durable 模式")
        .boundary(session.as_str(), None)
        .await
        .expect("/new");
    let after_new = prompt_of(&gw, &llm, &session, "重新开始", "skills-3").await;
    assert!(
        after_new.contains("- log-diagnosis：查日志") && after_new.contains("- pr-review"),
        "{after_new}"
    );
}
