//! `komo goal` / `komo area` 的输出（`docs/goals.md` §4）。
//!
//! 清单一条一行：`id@版本 状态 标题`，目标另带下一步（等待中带在等什么）、截止日期与
//! 「已超期」，领域另带标准。Agent 经 bash 读它，所以一行就是一条，没有表头和对齐空格。
//! 超期按响应里 Gateway 的今天读（`Matter::is_overdue`），不按本机时区再算。

use komo_kernel::protocol::http::{MatterDetail, MatterListResponse};
use komo_kernel::types::matter::{Matter, MatterKind, MatterState};
use time::Date;

use super::{evidence_ref, provenance, stamp};

/// `komo goal list` / `komo area list`。
pub fn matter_list(response: &MatterListResponse) -> String {
    if response.matters.is_empty() {
        return "没有事项".into();
    }
    response
        .matters
        .iter()
        .map(|matter| matter_line(matter, response.today))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 一条一行。写命令（add / next / done …）的回执也是这一行。
pub fn matter_line(matter: &Matter, today: Date) -> String {
    let mut parts = vec![format!(
        "{}@{} {} {}",
        matter.id,
        matter.revision,
        matter.state,
        flat(&matter.title)
    )];
    match matter.kind {
        MatterKind::Goal => {
            match (&matter.waiting_for, &matter.next) {
                (Some(waiting), _) if matter.state == MatterState::Waiting => {
                    parts.push(format!("在等：{}", flat(waiting)))
                }
                (_, Some(next)) => parts.push(format!("下一步：{}", flat(next))),
                _ => {}
            }
            if let Some(due) = matter.due {
                parts.push(format!("截止 {due}"));
            }
            if matter.is_overdue(today) {
                parts.push("已超期".into());
            }
        }
        MatterKind::Area => {
            if let Some(standard) = &matter.standard {
                parts.push(format!("标准：{}", flat(standard)));
            }
        }
    }
    parts.join(" · ")
}

/// 写命令的回执。
pub fn matter_changed(detail: &MatterDetail) -> String {
    matter_line(&detail.matter, detail.today)
}

/// `komo goal show` / `komo area show`。
pub fn matter_show(detail: &MatterDetail) -> String {
    let matter = &detail.matter;
    let state = if matter.is_overdue(detail.today) {
        format!("{}（已超期）", matter.state)
    } else {
        matter.state.to_string()
    };
    let mut out = vec![
        format!(
            "{}  {}  (revision {})",
            matter.kind.label(),
            matter.id,
            matter.revision
        ),
        format!("标题    {}", matter.title),
        format!("状态    {state}"),
    ];
    match matter.kind {
        MatterKind::Goal => {
            out.push(format!(
                "下一步  {}",
                matter.next.as_deref().unwrap_or("（无）")
            ));
            if let Some(waiting) = &matter.waiting_for {
                out.push(format!("在等    {waiting}"));
            }
            out.push(format!(
                "截止    {}",
                matter
                    .due
                    .map(|due| due.to_string())
                    .unwrap_or_else(|| "（无）".into())
            ));
        }
        MatterKind::Area => out.push(format!(
            "标准    {}",
            matter.standard.as_deref().unwrap_or("（无）")
        )),
    }
    let mut times = format!(
        "创建    {}   更新 {}",
        stamp(matter.created_at),
        stamp(matter.updated_at)
    );
    if let Some(closed) = matter.closed_at {
        times.push_str(&format!("   结案 {}", stamp(closed)));
    }
    out.push(times);
    if matter.evidence.is_empty() {
        out.push("证据    （无）".into());
    } else {
        out.push("证据".into());
        for evidence in &matter.evidence {
            out.push(format!(
                "  {} · {} · {}",
                evidence_ref(&evidence.reference),
                provenance(evidence.provenance),
                stamp(evidence.observed_at)
            ));
        }
    }
    out.join("\n")
}

/// 一行里不留换行与制表符：一行一条始终成立（入库时已拒换行，这里只兜旧数据）。
fn flat(text: &str) -> String {
    text.replace(['\n', '\r', '\t'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use komo_kernel::types::ids::MatterId;
    use komo_kernel::types::matter::NewMatter;
    use time::OffsetDateTime;
    use time::macros::{date, datetime};

    const T0: OffsetDateTime = datetime!(2026-10-29 08:00 UTC);

    fn goal(id: &str, state: MatterState) -> Matter {
        let mut matter = NewMatter {
            kind: MatterKind::Goal,
            title: "这周把 owl 的发布观察做完".into(),
            next: Some("看 AB 组的 5xx".into()),
            due: Some(date!(2026 - 11 - 01)),
            standard: None,
        }
        .build(MatterId::from_raw(id), T0)
        .unwrap();
        matter.state = state;
        if state == MatterState::Waiting {
            matter.waiting_for = Some("SRE 审批".into());
            matter.revision = 2;
        }
        matter
    }

    fn area() -> Matter {
        NewMatter {
            kind: MatterKind::Area,
            title: "告警值班轮换".into(),
            next: None,
            due: None,
            standard: Some("每周一前完成".into()),
        }
        .build(MatterId::from_raw("a-1"), T0)
        .unwrap()
    }

    /// 一条一行：目标带下一步 / 在等什么、截止与已超期，领域带标准。
    #[test]
    fn a_matter_list_is_one_line_per_matter() {
        let response = MatterListResponse {
            matters: vec![
                goal("g-1", MatterState::Active),
                goal("g-2", MatterState::Waiting),
                area(),
            ],
            today: date!(2026 - 11 - 02),
        };
        assert_eq!(
            matter_list(&response),
            "g-1@1 active 这周把 owl 的发布观察做完 · 下一步：看 AB 组的 5xx · 截止 2026-11-01 · 已超期\n\
             g-2@2 waiting 这周把 owl 的发布观察做完 · 在等：SRE 审批 · 截止 2026-11-01 · 已超期\n\
             a-1@1 active 告警值班轮换 · 标准：每周一前完成"
        );
        // 截止当天不算超期；结案的不算。
        let on_time = MatterListResponse {
            matters: vec![
                goal("g-1", MatterState::Active),
                goal("g-3", MatterState::Done),
            ],
            today: date!(2026 - 11 - 01),
        };
        assert!(!matter_list(&on_time).contains("已超期"));
        assert_eq!(
            matter_list(&MatterListResponse {
                matters: vec![],
                today: date!(2026 - 11 - 01)
            }),
            "没有事项"
        );
    }

    #[test]
    fn a_matter_detail_shows_every_field_and_the_overdue_mark() {
        let detail = MatterDetail {
            matter: goal("g-2", MatterState::Waiting),
            today: date!(2026 - 11 - 02),
        };
        let shown = matter_show(&detail);
        assert!(shown.starts_with("目标  g-2  (revision 2)"), "{shown}");
        assert!(shown.contains("状态    waiting（已超期）"), "{shown}");
        assert!(shown.contains("在等    SRE 审批"), "{shown}");
        assert!(shown.contains("截止    2026-11-01"), "{shown}");
        assert!(shown.contains("证据    （无）"), "{shown}");
        let area = matter_show(&MatterDetail {
            matter: area(),
            today: date!(2026 - 11 - 02),
        });
        assert!(area.contains("标准    每周一前完成"), "{area}");
        assert!(!area.contains("截止"), "{area}");
    }
}
