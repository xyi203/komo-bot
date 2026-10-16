# 测试用的极简 HTTP MCP 服务器，要 OAuth：发现端点 + 一个只认 bearer token 的 /mcp。
# argv[1]：服务器此刻认的 token 放在这个文件里（测试改它来模拟"token 作废"）。
# argv[2]：每次真的执行 whoami 往这里追加一行。
# 起来后往 stdout 打一行端口号。
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VALID = sys.argv[1]
CALLS = sys.argv[2]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def base(self):
        return "http://127.0.0.1:%d" % self.server.server_port

    def send_json(self, status, body, headers=()):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(data)

    def empty(self, status):
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        base = self.base()
        if self.path.startswith("/.well-known/oauth-protected-resource"):
            self.send_json(200, {"resource": base + "/mcp", "authorization_servers": [base]})
        elif self.path.startswith("/.well-known/oauth-authorization-server"):
            self.send_json(200, {
                "issuer": base,
                "authorization_endpoint": base + "/authorize",
                "token_endpoint": base + "/token",
                "response_types_supported": ["code"],
            })
        else:
            # /mcp 的 SSE 流不支持。
            self.empty(405 if self.path == "/mcp" else 404)

    def do_DELETE(self):
        self.empty(200)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        with open(VALID) as f:
            valid = f.read().strip()
        if self.headers.get("Authorization") != "Bearer " + valid:
            self.send_json(401, {"error": "invalid_token"}, [(
                "WWW-Authenticate",
                'Bearer resource_metadata="%s/.well-known/oauth-protected-resource"' % self.base(),
            )])
            return
        msg = json.loads(body)
        if "id" not in msg:
            self.empty(202)
            return
        method = msg.get("method")
        if method == "initialize":
            result = {
                "protocolVersion": msg["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "secure", "version": "0.1"},
                "instructions": "要登录的演示服务器。",
            }
        elif method == "tools/list":
            result = {"tools": [{
                "name": "whoami",
                "description": "报出调用者",
                "inputSchema": {"type": "object", "properties": {}},
            }]}
        elif method == "tools/call":
            with open(CALLS, "a") as f:
                f.write("whoami\n")
            result = {"content": [{"type": "text", "text": "you=" + valid}]}
        else:
            self.send_json(200, {"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "no"}})
            return
        self.send_json(200, {"jsonrpc": "2.0", "id": msg["id"], "result": result})


server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print(server.server_port, flush=True)
server.serve_forever()
