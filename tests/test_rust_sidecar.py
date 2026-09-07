"""SDK sidecar conversion tests; importing the module does not load the SDK."""
import asyncio
import importlib.util
import json
from pathlib import Path
import queue
import threading
from types import SimpleNamespace
import unittest

spec = importlib.util.spec_from_file_location(
    "rust_sidecar", Path(__file__).resolve().parents[1] / "compat/feishu-sdk/adapter.py")
adapter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(adapter)


class SidecarTests(unittest.TestCase):
    def test_message_keeps_sender_context_and_does_not_execute_commands(self):
        data = SimpleNamespace(event=SimpleNamespace(
            message=SimpleNamespace(message_id="m", chat_id="c", chat_type="p2p",
                                    content='{"text":"/stop"}', message_type="text"),
            sender=SimpleNamespace(sender_id=SimpleNamespace(open_id="u"))))
        event = adapter.message_event(data)
        self.assertEqual(event["content"], {"text": "/stop"})
        self.assertEqual(event["user_id"], "u")
        self.assertEqual(event["chat_type"], "p2p")

    def test_card_preserves_source_and_opaque_token(self):
        data = SimpleNamespace(event=SimpleNamespace(
            operator=SimpleNamespace(open_id="u"),
            context=SimpleNamespace(open_chat_id="c", open_message_id="source"),
            action=SimpleNamespace(value={"command": "/interaction", "token": "opaque", "choice": "allow"})))
        event = adapter.card_event(data)
        self.assertEqual(event["message_id"], "source")
        self.assertEqual(event["action"]["token"], "opaque")

    def test_backpressure_is_bounded_and_failed_enqueue_does_not_advance_sequence(self):
        emitter = adapter.Emitter("generation", capacity=1)
        emitter.emit({"kind": "message"})
        with self.assertRaises(queue.Full):
            emitter.emit({"kind": "message"})
        self.assertEqual(emitter.sequence, 1)
        frame = json.loads(emitter.outgoing.get_nowait())
        self.assertEqual(frame["epoch"], "generation")
        self.assertEqual(frame["version"], 1)
        self.assertEqual(frame["sequence"], 0)

    def test_oversize_event_is_not_queued(self):
        emitter = adapter.Emitter("generation")
        with self.assertRaises(ValueError):
            emitter.emit({"text": "中" * adapter.MAX_FRAME_BYTES})
        self.assertTrue(emitter.outgoing.empty())

    def test_sdk_callback_waits_for_owner_acceptance(self):
        emitter = adapter.Emitter("g")
        done = threading.Event()
        errors = []
        def callback():
            try:
                emitter.emit_confirmed({"kind": "message"}, timeout=1)
            except Exception as error:
                errors.append(type(error).__name__)
            finally:
                done.set()
        thread = threading.Thread(target=callback)
        thread.start()
        try:
            frame = json.loads(emitter.outgoing.get(timeout=1))
            self.assertFalse(done.is_set())
            emitter.confirm({"version": 1, "epoch": "g", "sequence": frame["sequence"], "accepted": True})
        finally:
            thread.join(timeout=2)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(emitter.pending, {})

    def test_timeout_and_invalid_ack_never_become_acceptance(self):
        emitter = adapter.Emitter("g")
        with self.assertRaises(TimeoutError):
            emitter.emit_confirmed({"kind": "message"}, timeout=0)
        self.assertEqual(emitter.pending, {})
        with self.assertRaises(ValueError):
            emitter.confirm({"version": 1, "epoch": "old", "sequence": 0, "accepted": True})
        with self.assertRaises(ValueError):
            emitter.confirm({"version": 1, "epoch": "g", "sequence": 0, "accepted": "true"})

    def test_initial_connect_and_reconnect_emit_without_log_parsing(self):
        spec = importlib.util.spec_from_file_location(
            "sdk_compat_test", Path(__file__).resolve().parents[1] / "compat/feishu-sdk/sdk_compat.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        class FakeClient:
            def __init__(self):
                self._conn = None
            async def _connect(self):
                if self._conn is None:
                    self._conn = object()
        events = []
        client = module.observed_client(FakeClient, events.append)()
        async def connect():
            await client._connect()
            await client._connect()
            client._conn = None
            await client._connect()
        asyncio.run(connect())
        self.assertEqual(events, [{"kind": "connection", "state": "connected"}] * 2)
