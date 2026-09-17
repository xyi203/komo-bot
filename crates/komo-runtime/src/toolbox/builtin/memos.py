"""Memos 客户端：用户**主动保存**的记录的原文来源（§5.5）。

自动 Memory 保存在 state.db，用户说"记一下"的那种记录保存在 Memos——这个模块是那条
路上的唯一实现。它提供创建、读取、查询、修改、删除五个导出函数，成功后**返回记录 ID
和原文链接**，查询时回到原文。

写入失败要明确报告；请求已发出而响应丢失时由执行器进入 uncertain，先用 `verify` 核对
远端结果，不直接重试——那会产生第二条记录（§5.5、§8.6）。**不在 Memos 不可用时改存
本地并声称已完成。**

地址与令牌从环境变量读，由配置点名传进来（§5.3「HA、Memos、搜索服务地址和凭证引用
通过配置传给已授权模块」）：

    MEMOS_BASE_URL   例如 https://memos.example.com
    MEMOS_TOKEN      用户访问令牌

令牌只进 Authorization 头，**不进返回值、不 print、不写进异常文本**。

接口按 Memos API v1（`/api/v1/memos`）写。**接入时要核对用户部署的那个版本**——
latest 文档不是该实例的接口保证（§5.5）；`_memo()` 因此对 `name` / `uid` / `id` 三种
标识形态都能读出 ID，读不出就明确报错而不是猜一个。
"""

import json
import os
import urllib.error
import urllib.parse
import urllib.request

__all__ = ["create", "get", "search", "update", "delete", "verify"]
__komo_verify__ = "verify"
__komo_env__ = ["MEMOS_BASE_URL", "MEMOS_TOKEN"]

TIMEOUT = 20


class MemosError(RuntimeError):
    """Memos 那一侧的问题。消息里不含令牌。"""


def _config():
    base = (os.environ.get("MEMOS_BASE_URL") or "").strip().rstrip("/")
    token = (os.environ.get("MEMOS_TOKEN") or "").strip()
    if not base:
        raise MemosError("MEMOS_BASE_URL 没有配置：不知道该写到哪个 Memos 实例")
    if not token:
        raise MemosError("MEMOS_TOKEN 没有配置：Memos 需要用户访问令牌")
    return base, token


def _request(method, path, body=None, query=None):
    base, token = _config()
    url = base + path
    if query:
        url = url + "?" + urllib.parse.urlencode(query)
    data = None
    if body is not None:
        data = json.dumps(body, ensure_ascii=False).encode("utf-8")
    request = urllib.request.Request(url, data=data, method=method)
    request.add_header("Authorization", "Bearer " + token)
    request.add_header("Accept", "application/json")
    if data is not None:
        request.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT) as response:
            raw = response.read().decode("utf-8") or "{}"
    except urllib.error.HTTPError as error:
        detail = ""
        try:
            detail = error.read().decode("utf-8", "replace")[:500]
        except Exception:  # noqa: BLE001 —— 读不出正文不该盖掉状态码
            detail = ""
        raise MemosError("Memos 返回 %s：%s" % (error.code, detail)) from None
    except urllib.error.URLError as error:
        # 连不上就是连不上。**不改存本地**（§5.5）。
        raise MemosError("连不上 Memos：%s" % (error.reason,)) from None
    try:
        return json.loads(raw) if raw.strip() else {}
    except ValueError:
        raise MemosError("Memos 的回复不是 JSON") from None


def _identifier(memo):
    """从一条记录里读出它的 ID。三种形态都收，读不出就报错。"""
    name = memo.get("name")
    if isinstance(name, str) and "/" in name:
        return name.rsplit("/", 1)[-1]
    for key in ("uid", "id", "memoId"):
        value = memo.get(key)
        if isinstance(value, (str, int)) and str(value):
            return str(value)
    raise MemosError("这条记录没有可用的 ID：%s" % (sorted(memo.keys()),))


def _memo(memo):
    """统一的返回形状：ID、原文链接、正文。"""
    base, _ = _config()
    identifier = _identifier(memo)
    return {
        "id": identifier,
        "url": "%s/m/%s" % (base, identifier),
        "content": memo.get("content", ""),
        "created_at": memo.get("createTime") or memo.get("createdTs"),
        "updated_at": memo.get("updateTime") or memo.get("updatedTs"),
    }


def create(text, visibility="PRIVATE"):
    """写一条记录，返回它的 ID 与原文链接（§5.5）。"""
    if not isinstance(text, str) or not text.strip():
        raise MemosError("要保存的内容不能为空")
    memo = _request("POST", "/api/v1/memos", {"content": text, "visibility": visibility})
    return _memo(memo)


def get(identifier):
    """按 ID 读回原文。"""
    if not identifier:
        raise MemosError("要读的记录 ID 不能为空")
    memo = _request("GET", "/api/v1/memos/%s" % (urllib.parse.quote(str(identifier)),))
    return _memo(memo)


def search(query, limit=10):
    """按内容查询，回到原文（§5.5「查询用户主动保存的记录……读取原文」）。

    服务端的 `filter` 语法在各版本之间变过，所以先照 v1 的 CEL 发一次；被拒就退回
    "取一页再在本地筛"——**明确降级**，不假装服务端筛过了。
    """
    limit = max(1, min(int(limit or 10), 100))
    needle = str(query) if query else None
    where = "none"
    page = None
    if needle:
        try:
            page = _request(
                "GET",
                "/api/v1/memos",
                query={
                    "pageSize": limit,
                    "filter": 'content.contains("%s")' % (needle.replace('"', '\\"'),),
                },
            )
            where = "server"
        except MemosError:
            page = None
    if page is None:
        page = _request("GET", "/api/v1/memos", query={"pageSize": 200})
        where = "client" if needle else "none"
    memos = [_memo(memo) for memo in page.get("memos", [])]
    if needle:
        # **本地再筛一遍**，哪怕服务端说它筛过了：这个部署的 filter 语义是什么，
        # latest 文档不作保证（§5.5）。返回的每一条都真的含有这个词，才对得起
        # "查询回到原文"这句话。
        memos = [memo for memo in memos if needle in memo["content"]]
    return {"filtered_by": where, "memos": memos[:limit]}


def update(identifier, text):
    """改一条记录的正文，返回新内容与 ID——关联摘要靠这两样更新（§5.5）。"""
    if not identifier:
        raise MemosError("要改的记录 ID 不能为空")
    if not isinstance(text, str) or not text.strip():
        raise MemosError("新的内容不能为空")
    memo = _request(
        "PATCH",
        "/api/v1/memos/%s" % (urllib.parse.quote(str(identifier)),),
        {"content": text},
        query={"updateMask": "content"},
    )
    return _memo(memo)


def delete(identifier):
    """删一条记录。删完再 `get` 一次确认它真的没了。"""
    if not identifier:
        raise MemosError("要删的记录 ID 不能为空")
    _request("DELETE", "/api/v1/memos/%s" % (urllib.parse.quote(str(identifier)),))
    return {"id": str(identifier), "deleted": True}


def verify(function=None, args=None):
    """与版本绑定的核对函数（§8.6）。

    执行器在"started 了却没有结果"时通过同一套 python 机制调它，**核对本身仍经过
    Policy**。它回答四种结论之一，绝不回答"大概吧"：

        already_satisfied  目标已达到，附证据（ID / 链接）
        not_performed      确定未执行，且前提仍成立，可以重做同一次调用
        conflict           出现了第三种状态
        unknown            核对不出结论 → 交给人

    `create` 是这里唯一真正难的一个：Memos 没有幂等键，所以只能按**内容逐字相同**去
    找。找到恰好一条才算"已达到"；找到多条就是 unknown——"内容相似、时间接近"不能证明
    它就是原调用创建的那一条（§8.6）。
    """
    args = args or {}
    try:
        if function in ("get", "search"):
            # 只读，重做一次就是（§8.6 第一行）。
            return {"kind": "not_performed", "evidence": "%s 是只读调用，重做安全" % (function,)}
        if function == "create":
            text = args.get("text")
            if not isinstance(text, str) or not text.strip():
                return {"kind": "unknown", "reason": "原调用没有可核对的正文"}
            found = [
                memo for memo in search(text, limit=100)["memos"] if memo["content"] == text
            ]
            if not found:
                return {"kind": "not_performed", "evidence": "Memos 里没有这条内容"}
            if len(found) == 1:
                return {
                    "kind": "already_satisfied",
                    "evidence": "已存在：%s（%s）" % (found[0]["id"], found[0]["url"]),
                }
            return {
                "kind": "unknown",
                "reason": "内容相同的记录有 %d 条，无法确认哪一条是原调用创建的"
                % (len(found),),
            }
        if function == "update":
            identifier = args.get("identifier") or args.get("id")
            text = args.get("text")
            memo = get(identifier)
            if memo["content"] == text:
                return {
                    "kind": "already_satisfied",
                    "evidence": "%s 的正文已经是这一份（%s）" % (memo["id"], memo["url"]),
                }
            return {
                "kind": "not_performed",
                "evidence": "%s 的正文仍是改动之前的那一份" % (memo["id"],),
            }
        if function == "delete":
            identifier = args.get("identifier") or args.get("id")
            try:
                get(identifier)
            except MemosError as error:
                if "404" in str(error):
                    return {"kind": "already_satisfied", "evidence": "%s 已经不在了" % (identifier,)}
                raise
            return {"kind": "not_performed", "evidence": "%s 还在" % (identifier,)}
        return {"kind": "unknown", "reason": "没有为 %r 写核对逻辑" % (function,)}
    except MemosError as error:
        return {"kind": "unknown", "reason": str(error)}
