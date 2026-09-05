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
os.environ.setdefault("CODEX_WORKSPACE_ROOT", "/tmp")

import bridge


class Outbox:
    def __init__(self):
        self.calls = []

    def card_or_text(self, *args, **kwargs):
        self.calls.append((args, kwargs))
        return "card-id"

    def update_card(self, *args, **kwargs):
        self.calls.append((args, kwargs))


class InputServer:
    def __init__(self):
        self.answers = []
        self.threads = {}

    def answer_user_input(self, request_id, answers):
        self.answers.append((request_id, answers))


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
                item.plan_modes = {"user-a:cwd-a": True}
                item.directories = {}
                item.save_session("thread-a", "user-a:cwd-a")
                item.save_session("thread-b", "user-b:cwd-b")
                item.save_model_settings()
                self.assertEqual(item.load_session("user-a:cwd-a"), "thread-a")
                self.assertEqual(item.load_session("user-b:cwd-b"), "thread-b")
                self.assertEqual(item.load_model_settings()["user-b:cwd-b"], "model-b")
                self.assertTrue(item.load_settings()[2]["user-a:cwd-a"])
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

    def test_plan_mode_is_sent_with_turn_start(self):
        server = bridge.CodexServer.__new__(bridge.CodexServer)
        server.turn_text = ""
        server.last_turn_status = ""
        server.last_plan_text = ""
        server.threads = {"key": "thread-1"}
        server.starting_turns = set()
        server.pending_interrupt = set()
        server.active_turn = {}
        server.completions = queue.Queue()
        server.completions.put({"params": {"threadId": "thread-1", "turn": {"id": "turn-1"}}})
        requests = []
        server.request = lambda method, params: requests.append((method, params)) or {"turn": {"id": "turn-1"}}
        server._send_interrupt = lambda _key: None
        server.turn("key", Path("/tmp"), "plan this", "model", plan_mode=True)
        self.assertEqual(requests[0][0], "turn/start")
        self.assertEqual(requests[0][1]["collaborationMode"]["mode"], "plan")
        self.assertEqual(requests[0][1]["approvalPolicy"], "on-request")
        self.assertEqual(requests[0][1]["sandboxPolicy"], {
            "type": "workspaceWrite", "writableRoots": ["/tmp"], "networkAccess": False,
        })

    def test_danger_full_access_requires_explicit_sandbox_setting(self):
        server = bridge.CodexServer.__new__(bridge.CodexServer)
        server.turn_text = ""
        server.last_turn_status = ""
        server.last_plan_text = ""
        server.last_turn_error = ""
        server.threads = {"key": "thread-1"}
        server.starting_turns = set()
        server.pending_interrupt = set()
        server.active_turn = {}
        server.completions = queue.Queue()
        server.completions.put({"params": {"threadId": "thread-1", "turn": {"id": "turn-1"}}})
        requests = []
        server.request = lambda method, params: requests.append((method, params)) or {"turn": {"id": "turn-1"}}
        server._send_interrupt = lambda _key: None
        original = bridge.SANDBOX_MODE
        try:
            bridge.SANDBOX_MODE = "dangerFullAccess"
            server.turn("key", Path("/tmp"), "read this", "model")
            self.assertEqual(requests[0][1]["sandboxPolicy"], {"type": "dangerFullAccess"})
        finally:
            bridge.SANDBOX_MODE = original

    def test_collaboration_mode_uses_app_server_default_model(self):
        server = bridge.CodexServer.__new__(bridge.CodexServer)
        server.turn_text = ""
        server.last_turn_status = ""
        server.last_turn_error = ""
        server.last_plan_text = ""
        server.default_model = ""
        server.model_ids = []
        server.threads = {"key": "thread-1"}
        server.starting_turns = set()
        server.pending_interrupt = set()
        server.active_turn = {}
        server.completions = queue.Queue()
        server.completions.put({"params": {"threadId": "thread-1", "turn": {"id": "turn-1"}}})
        requests = []
        def request(method, params):
            requests.append((method, params))
            if method == "model/list":
                return {"data": [{"id": "gpt-current", "isDefault": True}]}
            return {"turn": {"id": "turn-1"}}
        server.request = request
        server._send_interrupt = lambda _key: None
        server.turn("key", Path("/tmp"), "plan this", "", plan_mode=True)
        self.assertEqual(requests[-1][1]["collaborationMode"]["settings"]["model"], "gpt-current")

    def test_completed_turn_failure_keeps_server_error(self):
        server = bridge.CodexServer.__new__(bridge.CodexServer)
        server.event = lambda *_args: None
        server.last_turn_status = ""
        server.last_turn_error = ""
        server.handle_event({"method": "turn/completed", "params": {"turn": {
            "status": "failed", "error": {"message": "模型不可用", "additionalDetails": "选择其他模型"}}}})
        self.assertEqual(server.last_turn_status, "failed")
        self.assertIn("模型不可用", server.last_turn_error)
        self.assertIn("选择其他模型", server.last_turn_error)

    def test_initialize_enables_experimental_app_server_protocol(self):
        server = bridge.CodexServer.__new__(bridge.CodexServer)
        calls = []
        server.request = lambda method, params: calls.append((method, params)) or {}
        server.send = lambda message: calls.append(("send", message))
        server._initialize()
        self.assertTrue(calls[0][1]["capabilities"]["experimentalApi"])

    def test_question_cards_reply_with_selected_and_other_text(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        item.server = InputServer()
        item.feishu = Outbox()
        item.current_chat = {"active": "chat", "key": "user:cwd"}
        item.pending_questions = {}
        item.question_lock = threading.RLock()
        request = {"id": 42, "params": {"questions": [
            {"id": "style", "header": "样式", "question": "选择样式", "options": [{"label": "简洁", "description": "短"}]},
            {"id": "detail", "header": "细节", "question": "补充细节", "isOther": True},
        ]}}
        item.begin_user_input(request)
        item.question_action("user", "chat", "user:cwd", {"command": "/question-answer", "request_id": 42,
                             "question_id": "style", "answer": "简洁"}, "card-id")
        item.question_action("user", "chat", "user:cwd", {"command": "/question-other", "request_id": 42,
                             "question_id": "detail"}, "card-id")
        self.assertTrue(item.answer_question_text("user", "chat", "user:cwd", "自定义内容"))
        self.assertEqual(item.server.answers, [(42, {"style": ["简洁"], "detail": ["自定义内容"]})])

    def test_plan_stay_action_keeps_mode(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        with tempfile.TemporaryDirectory() as directory:
            original_settings = bridge.SETTINGS_FILE
            try:
                bridge.SETTINGS_FILE = Path(directory) / "settings.json"
                item.models = {}
                item.directories = {}
                item.plan_modes = {"user:cwd": True}
                item.plan_actions = {"action": {"user_id": "user", "chat_id": "chat", "key": "user:cwd", "plan": "计划", "created": time.time()}}
                item.feishu = Outbox()
                item.resolve_plan_action("user", "chat", "user:cwd", "/plan-stay", "action")
                self.assertTrue(item.plan_modes["user:cwd"])
                self.assertIn("继续 Plan 模式", item.feishu.calls[-1][0][1])
            finally:
                bridge.SETTINGS_FILE = original_settings

    def test_help_plan_button_reflects_mode_and_updates_the_original_card(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        with tempfile.TemporaryDirectory() as directory:
            original_settings = bridge.SETTINGS_FILE
            try:
                bridge.SETTINGS_FILE = Path(directory) / "settings.json"
                item.models = {}
                item.directories = {}
                item.plan_modes = {}
                item.feishu = Outbox()
                item.help_cards = {}
                item.current_chat = {}
                item.task_lock = threading.Lock()
                item.user_job_counts = {}
                item.pending_directories = {}
                item.server = SimpleNamespace()
                item.command("user", "chat", "user:cwd", "/help")
                button = next(button for button in item.feishu.calls[-1][1]["buttons"] if button["value"]["command"] == "/plan-toggle")
                self.assertEqual(button["text"], "开启 Plan")
                self.assertTrue(button["value"]["enabled"])

                item.command("user", "chat", "user:cwd", "/plan-toggle on", "card-id")
                self.assertTrue(item.plan_modes["user:cwd"])
                update = item.feishu.calls[-1][0]
                self.assertEqual(update[0], "card-id")
                updated_button = next(button for button in item.feishu.calls[-1][0][4] if button["value"]["command"] == "/plan-toggle")
                self.assertEqual(updated_button["text"], "关闭 Plan")
                self.assertFalse(updated_button["value"]["enabled"])
            finally:
                bridge.SETTINGS_FILE = original_settings

    def test_plan_completion_sends_separate_detail_and_fallback(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        item.server = SimpleNamespace(last_plan_text="# 正式计划\n\n- 第一步", turn_text="过程文本")
        item.active_cards = {"chat": "progress-card"}
        item.plan_actions = {}
        item.feishu = Outbox()
        events = []
        item.finish_card = lambda *args: events.append(("finished", args))
        item.send_split_cards = lambda *args: events.append(("details", args))
        item.finish_plan_turn("user", "user:cwd", "chat", 12)
        self.assertEqual(events[0][0], "finished")
        self.assertIn("耗时 12 秒", events[0][1][2])
        self.assertEqual(events[1], ("details", ("chat", "计划详情", "# 正式计划\n\n- 第一步", "blue")))
        action = item.feishu.calls[-1][0]
        self.assertEqual(action[1], "计划下一步")
        self.assertEqual(len(action[4]), 3)
        self.assertIn("超时后此操作卡将失效", action[2])
        self.assertEqual(next(iter(item.plan_actions.values()))["card"], "card-id")

        item.server.last_plan_text = ""
        item.server.turn_text = "降级文本"
        item.finish_plan_turn("user", "user:cwd", "chat", 1)
        self.assertIn("未收到结构化计划 item", events[-1][1][2])

    def test_file_diffs_cover_add_modify_delete_and_skip_binary(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            modified = root / "modified.txt"
            deleted = root / "deleted.txt"
            binary = root / "binary.bin"
            modified.write_text("before\n", encoding="utf-8")
            deleted.write_text("remove me\n", encoding="utf-8")
            binary.write_bytes(b"\0before")
            before = item.snapshot(root)
            modified.write_text("after\n", encoding="utf-8")
            deleted.unlink()
            binary.write_bytes(b"\0after")
            (root / "added.txt").write_text("new\n", encoding="utf-8")
            diffs = {path.name: content for path, content in item.file_diffs(root, before)}
            self.assertIn("-before", diffs["modified.txt"])
            self.assertIn("+after", diffs["modified.txt"])
            self.assertIn("--- a/deleted.txt", diffs["deleted.txt"])
            self.assertIn("+++ b/added.txt", diffs["added.txt"])
            self.assertIn("无法生成文本差异：二进制文件", diffs["binary.bin"])

    def test_progress_content_keeps_only_recent_process_preview(self):
        content = bridge.Bridge.progress_content(4, "x" * 6000)
        self.assertTrue(content.startswith("已耗时 4 秒\n\n"))
        self.assertEqual(len(content), len("已耗时 4 秒\n\n") + 5000)

    def test_no_file_diff_card_is_sent_when_snapshot_is_unchanged(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        item.feishu = Outbox()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "unchanged.txt").write_text("same\n", encoding="utf-8")
            before = item.snapshot(root)
            item.send_file_diffs("chat", root, before)
        self.assertEqual(item.feishu.calls, [])

    def test_interaction_cards_include_timeout_notices(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        item.approval_items = {}
        self.assertIn("超时后将自动拒绝", item.approval_summary({"id": 7, "method": "item/fileChange/requestApproval", "params": {}}))
        self.assertIn("10 分钟", item.timeout_notice(600, "超时"))

        item.server = InputServer()
        item.feishu = Outbox()
        item.current_chat = {"active": "chat", "key": "user:cwd"}
        item.pending_questions = {}
        item.question_lock = threading.RLock()
        item.begin_user_input({"id": 9, "params": {"questions": [{"id": "q", "question": "继续吗？", "options": []}]}})
        self.assertIn("超时后将返回空答案", item.feishu.calls[-1][0][2])

    def test_cd_is_scoped_persisted_and_offers_creation_confirmation(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            current = root / "project"
            child = current / "child"
            current.mkdir()
            child.mkdir()
            original_root, original_settings = bridge.WORKSPACE_ROOT, bridge.SETTINGS_FILE
            try:
                bridge.WORKSPACE_ROOT = root
                bridge.SETTINGS_FILE = root / "settings.json"
                item.models = {}
                item.directories = {"user": str(root)}
                item.feishu = Outbox()
                item.task_lock = threading.Lock()
                item.user_job_counts = {}
                item.pending_directories = {}
                item.current_chat = {}

                item.command("user", "chat", "unused", "/cd project")
                self.assertEqual(item.current_directory("user"), current)
                self.assertIn("工作目录已切换", item.feishu.calls[-1][0][1])
                self.assertEqual(item.load_settings()[1]["user"], str(current))

                item.command("user", "chat", "unused", "/cd ../../outside")
                self.assertEqual(item.current_directory("user"), current)
                self.assertIn("切换目录失败", item.feishu.calls[-1][0][1])

                (current / "outside-link").symlink_to(root.parent, target_is_directory=True)
                item.command("user", "chat", "unused", "/cd outside-link")
                self.assertEqual(item.current_directory("user"), current)
                self.assertIn("切换目录失败", item.feishu.calls[-1][0][1])

                item.command("user", "chat", "unused", "/cd new-project")
                buttons = item.feishu.calls[-1][0][4]
                confirmation = buttons[0]["value"]["directory_id"]
                item.command("user", "chat", "unused", f"/cd-confirm {confirmation}")
                self.assertTrue((current / "new-project").is_dir())
                self.assertEqual(item.current_directory("user"), current / "new-project")
            finally:
                bridge.WORKSPACE_ROOT, bridge.SETTINGS_FILE = original_root, original_settings

    def test_cd_rejects_switching_while_user_has_work(self):
        item = bridge.Bridge.__new__(bridge.Bridge)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "project").mkdir()
            original_root = bridge.WORKSPACE_ROOT
            try:
                bridge.WORKSPACE_ROOT = root
                item.directories = {"user": str(root)}
                item.feishu = Outbox()
                item.task_lock = threading.Lock()
                item.user_job_counts = {"user": 1}
                item.command("user", "chat", "unused", "/cd project")
                self.assertIn("任务执行中", item.feishu.calls[-1][0][1])
                self.assertEqual(item.current_directory("user"), root)
            finally:
                bridge.WORKSPACE_ROOT = original_root


if __name__ == "__main__":
    unittest.main()
