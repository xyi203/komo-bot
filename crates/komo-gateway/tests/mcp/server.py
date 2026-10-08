# 测试用的极简 stdio MCP 服务器：逐行 JSON-RPC，只实现 initialize / tools/list / tools/call。
import json
import os
import sys

COUNTER = sys.argv[1]

TOOLS = [
    {
        "name": "echo",
        "description": "回显 text，并报告看得见哪些环境变量",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    },
    {
        "name": "bump",
        "description": "往计数文件里追加一行",
        "inputSchema": {"type": "object", "properties": {}},
    },
]


def reply(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if "id" not in msg:
        continue
    if method == "initialize":
        reply(msg["id"], {
            "protocolVersion": msg["params"]["protocolVersion"],
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "demo", "version": "0.1"},
        })
    elif method == "tools/list":
        reply(msg["id"], {"tools": TOOLS})
    elif method == "tools/call":
        name = msg["params"]["name"]
        args = msg["params"].get("arguments") or {}
        if name == "echo":
            text = "%s token=%s inherited=%s" % (
                args["text"],
                os.environ.get("DEMO_TOKEN", "<none>"),
                "CARGO_MANIFEST_DIR" in os.environ,
            )
        else:
            with open(COUNTER, "a") as f:
                f.write("bump\n")
            text = "bumped"
        reply(msg["id"], {"content": [{"type": "text", "text": text}]})
    else:
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "no"}}) + "\n")
        sys.stdout.flush()
