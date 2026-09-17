"""toolbox.memos 的候选测试：对一个**本地假 Memos**跑，不碰真实例。

它同时是"模块自带测试长什么样"的样板：一个普通的 unittest 模块，跑起来不需要网络、
不需要凭证、不留下痕迹。
"""

import json
import os
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer

from toolbox import memos

STATE = {"memos": {}, "next": 1}


class FakeMemos(BaseHTTPRequestHandler):
    def log_message(self, *_args):  # 测试输出里不要一堆访问日志
        pass

    def _authorized(self):
        if self.headers.get("Authorization") != "Bearer test-token":
            self._json(401, {"message": "unauthorized"})
            return False
        return True

    def _json(self, code, body):
        raw = json.dumps(body).encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(length) or b"{}")

    def _identifier(self):
        return self.path.split("?")[0].rsplit("/", 1)[-1]

    def do_POST(self):
        if not self._authorized():
            return
        body = self._body()
        identifier = str(STATE["next"])
        STATE["next"] += 1
        memo = {"name": "memos/%s" % identifier, "content": body.get("content", "")}
        STATE["memos"][identifier] = memo
        self._json(200, memo)

    def do_GET(self):
        if not self._authorized():
            return
        if self.path.split("?")[0] == "/api/v1/memos":
            self._json(200, {"memos": list(STATE["memos"].values())})
            return
        memo = STATE["memos"].get(self._identifier())
        if memo is None:
            self._json(404, {"message": "not found"})
            return
        self._json(200, memo)

    def do_PATCH(self):
        if not self._authorized():
            return
        identifier = self._identifier()
        memo = STATE["memos"].get(identifier)
        if memo is None:
            self._json(404, {"message": "not found"})
            return
        memo["content"] = self._body().get("content", memo["content"])
        self._json(200, memo)

    def do_DELETE(self):
        if not self._authorized():
            return
        if STATE["memos"].pop(self._identifier(), None) is None:
            self._json(404, {"message": "not found"})
            return
        self._json(200, {})


class MemosTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = HTTPServer(("127.0.0.1", 0), FakeMemos)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        os.environ["MEMOS_BASE_URL"] = "http://127.0.0.1:%d" % cls.server.server_address[1]
        os.environ["MEMOS_TOKEN"] = "test-token"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def setUp(self):
        STATE["memos"] = {}
        STATE["next"] = 1

    def test_create_returns_an_id_and_a_link(self):
        written = memos.create("买牛奶")
        self.assertTrue(written["id"])
        self.assertTrue(written["url"].endswith("/m/" + written["id"]))
        self.assertEqual(written["content"], "买牛奶")

    def test_get_comes_back_to_the_original_text(self):
        written = memos.create("会议纪要：下周一发布")
        read = memos.get(written["id"])
        self.assertEqual(read["content"], "会议纪要：下周一发布")
        self.assertEqual(read["id"], written["id"])

    def test_search_finds_it_by_content(self):
        memos.create("牙医 周三 10:00")
        memos.create("另一件事")
        found = memos.search("牙医")["memos"]
        self.assertEqual([memo["content"] for memo in found], ["牙医 周三 10:00"])

    def test_update_returns_the_new_content_and_the_same_id(self):
        written = memos.create("旧的")
        changed = memos.update(written["id"], "新的")
        self.assertEqual(changed["id"], written["id"])
        self.assertEqual(changed["content"], "新的")
        self.assertEqual(memos.get(written["id"])["content"], "新的")

    def test_delete_really_removes_it(self):
        written = memos.create("临时")
        memos.delete(written["id"])
        with self.assertRaises(memos.MemosError):
            memos.get(written["id"])

    def test_verify_tells_a_written_memo_from_one_that_never_landed(self):
        self.assertEqual(
            memos.verify("create", {"text": "没写进去的"})["kind"], "not_performed"
        )
        memos.create("写进去了")
        self.assertEqual(memos.verify("create", {"text": "写进去了"})["kind"], "already_satisfied")

    def test_two_identical_memos_cannot_be_told_apart(self):
        memos.create("一样的")
        memos.create("一样的")
        # "内容相似、时间接近"不能证明是原调用创建的那一条（§8.6）。
        self.assertEqual(memos.verify("create", {"text": "一样的"})["kind"], "unknown")

    def test_a_missing_token_is_reported_not_swallowed(self):
        token = os.environ.pop("MEMOS_TOKEN")
        try:
            with self.assertRaises(memos.MemosError):
                memos.create("没有令牌")
        finally:
            os.environ["MEMOS_TOKEN"] = token

    def test_an_unreachable_memos_never_becomes_a_local_note(self):
        base = os.environ["MEMOS_BASE_URL"]
        os.environ["MEMOS_BASE_URL"] = "http://127.0.0.1:1"
        try:
            with self.assertRaises(memos.MemosError):
                memos.create("连不上的时候")
        finally:
            os.environ["MEMOS_BASE_URL"] = base
