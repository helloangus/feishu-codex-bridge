"""Offline archive and delivery regressions; never load runtime credentials."""
import os
import tempfile
import threading
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

os.environ.setdefault("FEISHU_APP_ID", "test-app")
os.environ.setdefault("FEISHU_APP_SECRET", "test-secret")
import bridge


class ThreadTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.session_patch = patch.object(bridge, "SESSION_FILE", self.root / "sessions.json")
        self.session_patch.start()
        self.addCleanup(self.session_patch.stop)
        self.item = bridge.Bridge.__new__(bridge.Bridge)
        self.item.thread_lock = threading.RLock()
        self.item.task_lock = threading.Lock()
        self.item.thread_cards = {}
        self.item.plan_actions = {}
        self.item.user_job_counts = {}
        self.item.active_task_tokens = {}
        self.item.current_chat = {}
        self.item.current_directory = lambda user: self.root
        self.key = f"user:{self.root}"
        self.threads = [{"id": "12345678-first", "name": "Same *title*", "cwd": str(self.root)},
                        {"id": "12345678-second", "preview": "Same *title*", "cwd": str(self.root)}]
        self.item.server = SimpleNamespace(threads={self.key: self.threads[0]["id"]},
                                           list_threads=Mock(return_value=self.threads),
                                           read_thread=Mock(return_value=self.threads[0]),
                                           archive_thread=Mock(), resume=Mock())
        self.item.feishu = SimpleNamespace(card_or_text=Mock(return_value="card-1"), update_card=Mock())

    def card_action(self, operation="archive"):
        self.item.show_threads("user", "chat", self.key, self.root)
        return {"token": next(iter(self.item.thread_cards)), "operation": operation,
                "thread_id": self.threads[0]["id"]}

    def test_list_displays_full_ids_title_fallback_and_current_binding(self):
        self.card_action()
        buttons = self.item.feishu.card_or_text.call_args.args[4]
        self.assertIn("12345678-first", buttons[0]["description"])
        self.assertIn("12345678-second", buttons[2]["description"])
        self.assertIn("当前对话", buttons[0]["description"])
        self.assertIn(r"Same \*title\*", buttons[0]["description"])
        self.assertEqual(bridge.Bridge.thread_title({"title": "old\ntitle"}), "old title")
        self.assertEqual(bridge.Bridge.thread_title({}), "未命名")

    def test_list_navigation_is_separate_from_thread_actions_in_both_views(self):
        for archived, label in [(False, "查看已归档对话"), (True, "返回普通对话列表")]:
            self.item.show_threads("user", "chat", self.key, self.root, archived=archived)
            buttons = self.item.feishu.card_or_text.call_args.args[4]
            elements = bridge.Feishu.make_card("对话", "说明", buttons=buttons)["body"]["elements"]
            self.assertEqual(elements[-3], {"tag": "hr"})
            self.assertEqual(elements[-2]["content"], "**列表导航**")
            self.assertEqual(elements[-1]["text"]["content"], label)
            if not archived:
                index = next(i for i, element in enumerate(elements)
                             if element.get("text", {}).get("content") == "恢复")
                self.assertEqual(elements[index + 1]["text"]["content"], "归档")
            self.assertNotIn("section", elements[-1])

    def test_archive_clears_all_matching_bindings_and_rejects_replay(self):
        self.item.save_session(self.threads[0]["id"], self.key)
        self.item.save_session(self.threads[0]["id"], "other-key")
        self.item.save_session("unrelated", "keep-key")
        action = self.card_action()
        self.item.thread_action("user", "chat", self.key, action, "card-1")
        self.item.server.archive_thread.assert_called_once_with(self.threads[0]["id"], archived=True)
        self.assertNotIn(self.key, self.item.server.threads)
        self.assertEqual(self.item.load_session(self.key), "")
        self.assertEqual(self.item.load_session("other-key"), "")
        self.assertEqual(self.item.load_session("keep-key"), "unrelated")
        self.item.thread_action("user", "chat", self.key, action, "card-1")
        self.assertEqual(self.item.server.archive_thread.call_count, 1)
        self.assertEqual(self.item.feishu.card_or_text.call_args.args[1], "对话操作失败")

    def test_card_rejects_wrong_owner_chat_directory_source_and_expiration(self):
        for user, chat, key, source in [("other", "chat", self.key, "card-1"),
                                       ("user", "other", self.key, "card-1"),
                                       ("user", "chat", "other-key", "card-1"),
                                       ("user", "chat", self.key, "other-card")]:
            action = self.card_action()
            self.item.thread_action(user, chat, key, action, source)
        action = self.card_action()
        self.item.thread_cards[action["token"]]["created"] = 0
        self.item.thread_action("user", "chat", self.key, action, "card-1")
        self.item.server.archive_thread.assert_not_called()

    def test_archive_failure_preserves_bindings_and_card_for_retry(self):
        action = self.card_action()
        self.item.server.archive_thread.side_effect = RuntimeError("RPC failed")
        self.item.thread_action("user", "chat", self.key, action, "card-1")
        self.assertIn(self.key, self.item.server.threads)
        self.assertIn(action["token"], self.item.thread_cards)

    def test_archive_rejects_queued_active_and_cross_directory_threads(self):
        self.item.user_job_counts = {"user": 1}
        with self.assertRaisesRegex(ValueError, "排队"):
            self.item.change_thread("user", "chat", self.key, self.root, "id", "archive")
        self.item.user_job_counts = {}
        self.item.active_task_tokens = {self.key: "task"}
        with self.assertRaises(ValueError):
            self.item.change_thread("user", "chat", self.key, self.root, "id", "archive")
        self.item.active_task_tokens = {}
        self.item.server.read_thread.return_value = {"cwd": str(self.root / "other")}
        with self.assertRaisesRegex(ValueError, "目录"):
            self.item.change_thread("user", "chat", self.key, self.root, "id", "archive")
        self.item.server.archive_thread.assert_not_called()

    def test_archive_reserves_admission_without_blocking_receive_lock(self):
        def check_lock(*args, **kwargs):
            self.assertTrue(self.item.thread_mutation)
            self.assertTrue(self.item.task_lock.acquire(blocking=False))
            self.item.task_lock.release()
        self.item.server.archive_thread.side_effect = check_lock
        self.item.change_thread("user", "chat", self.key, self.root, "id", "archive")

    def test_archived_list_and_unarchive_do_not_switch_current_thread(self):
        self.item.show_threads("user", "chat", self.key, self.root, archived=True)
        self.item.server.list_threads.assert_called_with(self.root, archived=True)
        token = next(iter(self.item.thread_cards))
        self.item.thread_action("user", "chat", self.key,
                                {"token": token, "operation": "unarchive", "thread_id": self.threads[0]["id"]}, "card-1")
        self.item.server.archive_thread.assert_called_once_with(self.threads[0]["id"], archived=False)
        self.item.server.resume.assert_not_called()
        self.assertIn(self.key, self.item.server.threads)

    def test_new_thread_rejects_queued_work_and_clears_idle_binding(self):
        self.item.user_job_counts = {"user": 1}
        self.item.command("user", "chat", self.key, "/new")
        self.assertIn(self.key, self.item.server.threads)
        self.item.user_job_counts = {}
        self.item.command("user", "chat", self.key, "/new")
        self.assertNotIn(self.key, self.item.server.threads)

    def test_new_message_during_archive_is_not_enqueued(self):
        self.item.thread_mutation = True
        self.item.remember_message = lambda message_id: True
        self.item.is_allowed = lambda user: True
        self.item.answer_question_text = lambda *args: False
        self.item.generations = {}
        self.item.jobs = Mock()
        data = SimpleNamespace(event=SimpleNamespace(
            message=SimpleNamespace(message_id="message", content='{"text":"hello"}',
                                    message_type="text", chat_id="chat"),
            sender=SimpleNamespace(sender_id=SimpleNamespace(open_id="user"))))
        self.item.receive(data)
        self.item.jobs.put.assert_not_called()
        self.assertEqual(self.item.user_job_counts, {})
        self.assertEqual(self.item.feishu.card_or_text.call_args.args[1], "对话操作中")

    def test_archive_rpc_and_notification_use_existing_dispatcher(self):
        server = bridge.CodexServer.__new__(bridge.CodexServer)
        server.request = Mock(return_value={})
        server.event = Mock()
        server.archive_thread("id")
        server.request.assert_called_with("thread/archive", {"threadId": "id"})
        server.archive_thread("id", False)
        server.request.assert_called_with("thread/unarchive", {"threadId": "id"})
        server.handle_event({"method": "thread/archived", "params": {"threadId": "id"}})
        server.event.assert_called_with("archived", {"threadId": "id"})


class DeliveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.item = bridge.Bridge.__new__(bridge.Bridge)

    def test_engineering_text_only_diff_and_artifacts_only_upload(self):
        for name in ["main.py", "README.md", "config.json", "Makefile"]:
            (self.root / name).write_text("new text\n")
        for name in ["result.png", "report.pdf", "slides.pptx", "bundle.zip"]:
            (self.root / name).write_bytes(b"\x00binary")
        after = self.item.snapshot(self.root)
        self.assertEqual({p.name for p in self.item.changed_files(self.root, {}, after)},
                         {"result.png", "report.pdf", "slides.pptx", "bundle.zip"})
        self.assertEqual({p.name for p, _ in self.item.file_diffs(self.root, {}, after)},
                         {"main.py", "README.md", "config.json", "Makefile"})

    def test_cache_is_never_read_compared_or_uploaded(self):
        for name in ["__pycache__/main.pyc", "loose.pyc", "node_modules/pkg/index.js",
                     ".venv/lib/a.py", ".pytest_cache/state", "target/main.o", ".env"]:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"not even binary")
        with patch.object(self.item, "_snapshot_text", side_effect=AssertionError("must not read")):
            after = self.item.snapshot(self.root)
        self.assertEqual(after, {})
        self.assertEqual(self.item.file_diffs(self.root, {}, after), [])
        self.assertEqual(self.item.changed_files(self.root, {}, after), [])

    def test_metadata_only_changes_do_not_produce_diff(self):
        path = self.root / "main.py"
        path.write_text("same\n")
        before = self.item.snapshot(self.root)
        os.utime(path, (1, 1))
        self.assertEqual(self.item.file_diffs(self.root, before), [])

    def test_content_change_with_identical_metadata_is_detected(self):
        path = self.root / "main.py"
        path.write_text("old\n")
        before = self.item.snapshot(self.root)
        stat = path.stat()
        path.write_text("new\n")
        os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        self.assertIn("+new", self.item.file_diffs(self.root, before)[0][1])

    def test_large_and_unknown_files_never_fall_back_to_upload(self):
        (self.root / "main.py").write_text("x" * 100)
        (self.root / "unknown.bin").write_bytes(b"\x00binary")
        with patch.object(bridge, "SNAPSHOT_MAX_TEXT_BYTES", 20):
            after = self.item.snapshot(self.root)
        diffs = dict((p.name, diff) for p, diff in self.item.file_diffs(self.root, {}, after))
        self.assertIn("文件过大", diffs["main.py"])
        self.assertIn("二进制文件", diffs["unknown.bin"])
        self.assertEqual(self.item.changed_files(self.root, {}, after), [])

    def test_diff_counts_truncation_missing_newline_and_long_fences(self):
        path = self.root / "main.py"
        path.write_text("before")
        before = self.item.snapshot(self.root)
        path.write_text("after\n")
        diff = self.item.file_diffs(self.root, before)[0][1]
        self.assertIn("+1 / -1", diff)
        self.assertIn("-before\n+after", diff)
        self.assertIn("末尾换行", diff)
        with patch.object(bridge, "DIFF_MAX_CHARS", 10):
            self.assertIn("已截断", self.item.file_diffs(self.root, before)[0][1])
        chunks = self.item.split_card_content("```diff\n+" + "x" * 15000 + "\n```", max_chars=100)
        self.assertGreater(len(chunks), 100)
        self.assertTrue(all(chunk.count("```") % 2 == 0 for chunk in chunks))

    def test_file_limit_does_not_claim_existing_omitted_file_was_deleted(self):
        (self.root / "z.py").write_text("existing")
        with patch.object(bridge, "SNAPSHOT_MAX_FILES", 1):
            before = self.item.snapshot(self.root)
            (self.root / "a.py").write_text("new")
            diffs = self.item.file_diffs(self.root, before)
        self.assertEqual([p.name for p, _ in diffs], ["a.py"])

    def test_upload_limit_is_applied_after_filtering(self):
        for i in range(12):
            (self.root / f"{i:02}.py").write_text("code")
            (self.root / f"{i:02}.pdf").write_bytes(b"\x00pdf")
        paths = self.item.changed_files(self.root, {})
        self.assertEqual(len(paths), 10)
        self.assertTrue(all(path.suffix == ".pdf" for path in paths))
