import os
import queue
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace


os.environ.setdefault("FEISHU_APP_ID", "test-app")
os.environ.setdefault("FEISHU_APP_SECRET", "test-secret")

import bridge


class Outbox:
    def __init__(self):
        self.calls = []

    def card_or_text(self, *args, **kwargs):
        self.calls.append((args, kwargs))
        return "card-id"


class BridgeTests(unittest.TestCase):
    def test_card_uses_mobile_safe_button_layouts(self):
        card = bridge.Feishu.make_card("标题", "内容", buttons=[
            {"text": "状态 /status", "group": "任务", "value": {"command": "/status"}},
            {"text": "模型", "description": "**gpt-test**", "value": {"command": "/model"}},
        ])
        elements = card["body"]["elements"]
        columns = next(item for item in elements if item["tag"] == "column_set")
        self.assertEqual(columns["columns"][0]["elements"][0]["width"], "fill")
        model_button = elements[-1]
        self.assertEqual(model_button["tag"], "button")
        self.assertEqual(model_button["width"], "fill")

    def test_split_card_content_reopens_fenced_code_block(self):
        content = "```python\n" + "x = 1\n" * 20 + "```\n结束"
        chunks = bridge.Bridge.split_card_content(content, max_chars=45)
        self.assertGreater(len(chunks), 1)
        self.assertTrue(chunks[0].rstrip().endswith("```"))
        self.assertTrue(chunks[1].startswith("```python\n"))
        self.assertTrue(all(chunk.count("```") % 2 == 0 for chunk in chunks))

    def test_session_and_model_preferences_are_isolated_by_key(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        with tempfile.TemporaryDirectory() as directory:
            original_session, original_settings = bridge.SESSION_FILE, bridge.SETTINGS_FILE
            try:
                bridge.SESSION_FILE = Path(directory) / "sessions.json"
                bridge.SETTINGS_FILE = Path(directory) / "settings.json"
                item.models = {"user-a:cwd-a": "model-a", "user-b:cwd-b": "model-b"}
                item.save_session("thread-a", "user-a:cwd-a")
                item.save_session("thread-b", "user-b:cwd-b")
                item.save_model_settings()
                self.assertEqual(item.load_session("user-a:cwd-a"), "thread-a")
                self.assertEqual(item.load_session("user-b:cwd-b"), "thread-b")
                self.assertEqual(item.load_model_settings()["user-b:cwd-b"], "model-b")
            finally:
                bridge.SESSION_FILE, bridge.SETTINGS_FILE = original_session, original_settings

    def test_message_deduplication_survives_a_bridge_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            original = bridge.SEEN_MESSAGES_FILE
            try:
                bridge.SEEN_MESSAGES_FILE = Path(directory) / "seen.json"
                first = bridge.Bridge.__new__(bridge.Bridge)
                first.seen_messages = bridge.deque(maxlen=1000)
                first.seen_message_set = set()
                first.seen_lock = threading.Lock()
                first.load_seen_messages()
                self.assertTrue(first.remember_message("message-1"))
                self.assertFalse(first.remember_message("message-1"))

                restarted = bridge.Bridge.__new__(bridge.Bridge)
                restarted.seen_messages = bridge.deque(maxlen=1000)
                restarted.seen_message_set = set()
                restarted.seen_lock = threading.Lock()
                restarted.load_seen_messages()
                self.assertFalse(restarted.remember_message("message-1"))
                self.assertTrue(restarted.remember_message("message-2"))
            finally:
                bridge.SEEN_MESSAGES_FILE = original

    def test_pairing_persists_multiple_users_without_storing_bad_attempts(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        with tempfile.TemporaryDirectory() as directory:
            original_file, original_code = bridge.ALLOWED_OPEN_IDS_FILE, bridge.PAIRING_CODE
            try:
                bridge.ALLOWED_OPEN_IDS_FILE = Path(directory) / "allowed.json"
                bridge.PAIRING_CODE = "correct-secret"
                item.allowed_lock = threading.Lock()
                item.allowed_open_ids = set()
                self.assertFalse(item.pair_user("ou-bad", "wrong-secret"))
                self.assertFalse(bridge.ALLOWED_OPEN_IDS_FILE.exists())
                self.assertTrue(item.pair_user("ou-first", "correct-secret"))
                self.assertTrue(item.pair_user("ou-second", "correct-secret"))
                self.assertTrue(item.pair_user("ou-first", "correct-secret"))
                self.assertEqual(item.load_allowed_open_ids(), {"ou-first", "ou-second"})
                self.assertEqual(bridge.ALLOWED_OPEN_IDS_FILE.stat().st_mode & 0o777, 0o600)
            finally:
                bridge.ALLOWED_OPEN_IDS_FILE, bridge.PAIRING_CODE = original_file, original_code

    def test_status_reports_idle_and_active_task(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        item.server = SimpleNamespace(threads={})
        item.models = {}
        item.load_session = lambda key: "saved-thread"
        item.feishu = Outbox()
        item.task_lock = threading.Lock()
        item.active_task_tokens = {}
        item.stopping_tasks = set()
        item.jobs = queue.Queue()
        item.started_at = time.monotonic() - 65
        item.current_chat = {}
        original_log_event = bridge.log_event
        bridge.log_event = lambda *_args, **_kwargs: None
        try:
            item.command("user", "chat", "user:cwd", "/status")
            idle = item.feishu.calls[-1][0][2]
            self.assertIn("空闲", idle)
            self.assertIn("saved-thread", idle)
            self.assertIn("1 分 5 秒", idle)
            item.active_task_tokens["user:cwd"] = "task-1"
            item.command("user", "chat", "user:cwd", "/status")
            self.assertIn("执行中", item.feishu.calls[-1][0][2])
        finally:
            bridge.log_event = original_log_event


if __name__ == "__main__":
    unittest.main()
