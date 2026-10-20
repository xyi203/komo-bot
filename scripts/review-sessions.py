#!/usr/bin/env python3
"""Read local session JSONL and generate a daily review snapshot; never open state.db."""
import argparse
import collections
import datetime as dt
import json
from pathlib import Path
from zoneinfo import ZoneInfo

TOKEN_KEYS = ("input_tokens", "output_tokens", "cache_read_tokens", "reasoning_tokens")
TERMINAL = {"run.completed", "run.failed", "run.cancelled", "run.absorbed"}


def timestamp(value):
    parsed = dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError("timestamp must include a timezone")
    return parsed


def percentile(values, p=0.95):
    return round(sorted(values)[int((len(values) - 1) * p)], 3) if values else None


def error_category(text):
    if not isinstance(text, str):
        return "other_or_unknown"
    if "has no attribute 'fromtimestamp'" in text or "has no attribute 'utcfromtimestamp'" in text:
        return "datetime_api"
    if "NameError:" in text and "is not defined" in text:
        return "undefined_name"
    if "不存在" in text or "FileNotFoundError:" in text:
        return "missing_path"
    if "JSONDecodeError:" in text:
        return "invalid_json"
    if "KeyError:" in text:
        return "missing_key"
    return "other_or_unknown"


def failure_text(session, data):
    """Classify in memory; neither arguments nor output/error bodies are exported."""
    ref = data.get("output_ref")
    path = ref.get("path") if isinstance(ref, dict) else ref
    if path:
        target = (session / path).resolve()
        if target.is_relative_to(session.resolve()) and target.is_file():
            try:
                body = json.loads(target.read_text())["body"]
                result = body.get("result") or {}
                return result.get("error") or body.get("error") or body.get("preview") or ""
            except (OSError, ValueError, KeyError, TypeError, AttributeError):
                pass
    body = data.get("body") or {}
    result = body.get("result") or {}
    return result.get("error") or body.get("error") or body.get("preview") or data.get("preview") or ""


def collect(home, start, end):
    events = collections.Counter()
    tokens = collections.Counter()
    token_reports = collections.Counter()
    runs, tools, memories, sources, skill_sizes = {}, [], [], [], set()
    trailing = 0
    for path in sorted((home / "sessions").glob("*/events.jsonl")):
        calls, active, last_seq = {}, False, None
        with path.open() as stream:
            for line_no, line in enumerate(stream, 1):
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    # A live writer can leave an unfinished final line. Interior corruption is fatal.
                    if not line.endswith("\n") and stream.read() == "":
                        trailing += 1
                        break
                    raise ValueError(f"invalid JSONL: {path}:{line_no}") from None
                at = timestamp(event["at"])
                if at >= end:
                    continue
                kind, data = event["type"], event["data"]
                rid = event.get("run_id") or data.get("run")
                if kind == "run.accepted":
                    runs[rid] = {"id": rid, "session": path.parent.name, "accepted_at": at.isoformat(), "seq": event["seq"], "outcome": "open"}
                    ref = data.get("snapshot", {}).get("skills_ref")
                    if ref and at >= start:
                        skill_sizes.add(ref.get("size", 0))
                run = runs.get(rid)
                if run is not None:
                    if kind == "run.started":
                        run.setdefault("started_at", at.isoformat())
                    if kind == "message.assistant":
                        run.setdefault("first_assistant_at", at.isoformat())
                    if kind in TERMINAL:
                        run.update(outcome=kind[4:], ended_at=at.isoformat())
                if kind == "message.assistant":
                    for call in data.get("tool_calls", []):
                        calls[call["call_id"]] = call["name"]
                if kind == "tool.planned":
                    calls[data["call_id"]] = data.get("plan", {}).get("tool", calls.get(data["call_id"], "unknown"))
                if at < start:
                    continue
                active, last_seq = True, event["seq"]
                events[kind] += 1
                if kind == "message.assistant":
                    for key in TOKEN_KEYS:
                        if data.get(key) is not None:
                            tokens[key] += data[key]
                            token_reports[key] += 1
                if kind == "tool.result":
                    tool = {"session": path.parent.name, "seq": event["seq"], "name": calls.get(data["call_id"], "unknown"), "status": data.get("status", "unknown"), "elapsed_ms": data.get("elapsed_ms")}
                    preview = data.get("preview") or (data.get("body") or {}).get("preview") or ""
                    tool["detached"] = tool["name"] == "bash" and "已转后台" in preview
                    if tool["name"] == "codemode" and tool["status"] == "failed":
                        tool["category"] = error_category(failure_text(path.parent, data))
                    tools.append(tool)
                if kind == "memory.injected":
                    recall = data.get("recall") or {}
                    memories.append({"session": path.parent.name, "seq": event["seq"], "uses": len(data.get("uses", [])), "mode": recall.get("mode"), "degraded": recall.get("degraded") if recall else None})
        if active:
            sources.append({"session": path.parent.name, "last_seq_in_window": last_seq})
    cohort = [r for r in runs.values() if start <= timestamp(r["accepted_at"]) < end]
    for run in cohort:
        accepted = timestamp(run["accepted_at"])
        for field, source in [("queue_s", "started_at"), ("first_assistant_s", "first_assistant_at"), ("duration_s", "ended_at")]:
            run[field] = round((timestamp(run[source]) - accepted).total_seconds(), 3) if source in run else None
    codemode = [t for t in tools if t["name"] == "codemode"]
    known_recall = [m for m in memories if m["mode"] is not None]
    queue = [r["queue_s"] for r in cohort if r["queue_s"] is not None]
    completed = [r["duration_s"] for r in cohort if r["outcome"] == "completed"]
    signals = {
        "queue_p95_s": percentile(queue),
        "completed_p50_s": percentile(completed, 0.5),
        "completed_p95_s": percentile(completed),
        "bash_over_600s": sum(t["name"] == "bash" and (t["elapsed_ms"] or 0) > 600000 for t in tools),
        "bash_detached": sum(t["detached"] for t in tools),
        "codemode_results": len(codemode),
        "codemode_failed": sum(t["status"] == "failed" for t in codemode),
        "codemode_uncertain": sum(t["status"] == "uncertain" for t in codemode),
        "codemode_failure_categories": dict(collections.Counter(t["category"] for t in codemode if "category" in t)),
        "memory_injections": len(memories),
        "memory_with_mode": len(known_recall),
        "memory_degraded": sum(m["degraded"] is True for m in known_recall),
        "memory_use_counts": dict(collections.Counter(m["uses"] for m in memories)),
        "skills_snapshot_bytes": sorted(skill_sizes),
    }
    evidence = {
        "long_tools": sorted(tools, key=lambda t: t["elapsed_ms"] or 0, reverse=True)[:10],
        "codemode_failures": [t for t in codemode if t["status"] != "completed"][:20],
        "interjections": [r for r in cohort if r["outcome"] == "absorbed"],
        "degraded_memories": [m for m in known_recall if m["degraded"]],
    }
    return {"window": {"from": start.isoformat(), "until_exclusive": end.isoformat()}, "data_home": str(home), "sessions": len(sources), "accepted_runs": len(cohort), "outcomes": dict(collections.Counter(r["outcome"] for r in cohort)), "events": dict(events), "tokens": {key: tokens[key] if token_reports[key] else None for key in TOKEN_KEYS}, "token_reporting_rounds": dict(token_reports), "signals": signals, "evidence": evidence, "sources": sources, "unfinished_jsonl_tails": trailing}


def render(snapshot, date):
    s = snapshot["signals"]
    lines = [f"# {date} session 自动统计", "", f"统计窗口：`{snapshot['window']['from']}` ≤ at < `{snapshot['window']['until_exclusive']}`。", "", "这是事件统计和待核查信号，人工结论见同日复盘；重复运行只刷新统计。", "", "| 指标 | 结果 |", "|---|---:|"]
    for label, value in [("活动 session", snapshot["sessions"]), ("新受理 Run", snapshot["accepted_runs"]), ("assistant 轮次", snapshot["events"].get("message.assistant", 0)), ("顶层 tool.result 事件", snapshot["events"].get("tool.result", 0)), ("正常领取 P95（秒）", s["queue_p95_s"]), ("完成 Run P95（秒）", s["completed_p95_s"]), ("bash 超过 600 秒", s["bash_over_600s"]), ("bash 自动转后台", s["bash_detached"]), ("codemode failed / 总数", f"{s['codemode_failed']} / {s['codemode_results']}"), ("codemode uncertain", s["codemode_uncertain"]), ("记忆降级 / 有模式诊断的注入", f"{s['memory_degraded']} / {s['memory_with_mode']}")]:
        lines.append(f"| {label} | {value if value is not None else '未知'} |")
    lines += ["", "**待核查信号**", "", f"- Run 最后状态：`{json.dumps(snapshot['outcomes'], ensure_ascii=False)}`。completed 不等于业务成功。", f"- codemode 失败分类：`{json.dumps(s['codemode_failure_categories'], ensure_ascii=False)}`；分类依赖明确错误特征，其余归 unknown。", f"- 注入条数分布：`{json.dumps(s['memory_use_counts'])}`；数量不能证明相关性。", "- 检查下面的慢调用和插话证据，区分前台等待、后台执行和用户可见回复。", "- skill 是否缺失及记忆是否相关，需要读冻结目录和原始内容，自动统计不作判断。", "", "**证据入口**", "", "seq 是 JSONL 事件序号；需要正文时读取原始 session，不把参数、回复和完整错误复制进统计。", ""]
    for group, rows in snapshot["evidence"].items():
        lines.append(f"- `{group}`：")
        for row in rows:
            detail = {k: v for k, v in row.items() if k not in {"session", "id"}}
            lines.append(f"  - `{row['session']}` seq `{row['seq']}`：`{json.dumps(detail, ensure_ascii=False)}`")
    lines += ["", f"数据完整性：忽略 {snapshot['unfinished_jsonl_tails']} 个正在追加的未完成末行；中间损坏会终止生成。", "", "口径、人工复核流程和边界见 [README](README.md)。", ""]
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--date", default="yesterday", help="YYYY-MM-DD, today or yesterday")
    parser.add_argument("--timezone", default="Asia/Hong_Kong")
    parser.add_argument("--home", type=Path, default=Path.home() / ".komo")
    parser.add_argument("--output", type=Path, help="output directory; defaults to <home>/session-reviews")
    parser.add_argument("--until", help="exclusive cutoff with timezone; defaults to now or end of selected day")
    args = parser.parse_args()
    zone = ZoneInfo(args.timezone)
    now = dt.datetime.now(zone)
    date = now.date() - dt.timedelta(days=args.date == "yesterday") if args.date in {"today", "yesterday"} else dt.date.fromisoformat(args.date)
    start = dt.datetime.combine(date, dt.time(), zone)
    end = min(start + dt.timedelta(days=1), timestamp(args.until) if args.until else now)
    if end <= start or end > now:
        parser.error("selected window must have elapsed and until must be after start")
    home = args.home.expanduser().resolve()
    if not (home / "sessions").is_dir():
        parser.error(f"sessions directory does not exist: {home / 'sessions'}")
    output = args.output.expanduser().resolve() if args.output is not None else home / "session-reviews"
    snapshot = collect(home, start, end)
    snapshot.update(schema_version=1, generated_at=now.isoformat(), timezone=args.timezone)
    output.mkdir(parents=True, exist_ok=True)
    stem = output / date.isoformat()
    stem.with_suffix(".metrics.json").write_text(json.dumps(snapshot, ensure_ascii=False, indent=2) + "\n")
    stem.with_suffix(".stats.md").write_text(render(snapshot, date))
    review = stem.with_suffix(".md")
    if not review.exists():
        review.write_text(f"# {date} session 复盘\n\n状态：自动统计已生成，尚未人工复核。\n\n[统计]({date}.stats.md) · [指标与证据]({date}.metrics.json)\n\n**结论**\n\n【待补充：本日最值得优化的 1–3 点及证据】\n\n**优化项跟踪**\n\n| 编号 | 新证据 | 当前状态 | 下一步与验收 |\n|---|---|---|---|\n| 【待补充】 | session + seq | 待核查 | 【待补充】 |\n\n**改善与限制**\n\n【待补充：哪些已改善；哪些只有代码/测试、尚无运行样本；统计窗口和缺失证据】\n")
    print(f"{review}\n{stem.with_suffix('.stats.md')}\n{stem.with_suffix('.metrics.json')}")


if __name__ == "__main__":
    main()
