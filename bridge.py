#!/usr/bin/env python3
"""A small Termux-friendly Feishu <-> Codex app-server bridge."""
from __future__ import annotations

import json
import hashlib
import mimetypes
import os
import queue
import shlex
import subprocess
import threading
import time
import uuid
from collections import deque
from pathlib import Path
from typing import Any, Callable

import httpx


ROOT = Path(os.environ.get("CODEX_BRIDGE_CWD", os.getcwd())).resolve()
APP_ID = os.environ["FEISHU_APP_ID"]
APP_SECRET = os.environ["FEISHU_APP_SECRET"]
ALLOWED = {x.strip() for x in os.environ.get("FEISHU_ALLOWED_OPEN_IDS", "").split(",") if x.strip()}
DEFAULT_MODEL = os.environ.get("CODEX_MODEL", "")
MAX_ATTACHMENT = int(os.environ.get("CODEX_MAX_ATTACHMENT_BYTES", str(20 * 1024 * 1024)))
SESSION_FILE = Path(os.environ.get("CODEX_SESSION_FILE", str(ROOT / ".feishu-codex-session")))
SETTINGS_FILE = Path(os.environ.get("CODEX_SETTINGS_FILE", str(ROOT / ".feishu-codex-settings")))
SEEN_MESSAGES_FILE = Path(os.environ.get("CODEX_SEEN_MESSAGES_FILE", str(ROOT / ".feishu-codex-seen-messages")))
APPROVAL_TIMEOUT = int(os.environ.get("CODEX_APPROVAL_TIMEOUT_SECONDS", "600"))
STREAM_CHUNK = int(os.environ.get("CODEX_STREAM_CHUNK_CHARS", "1200"))
GENERATED_IMAGES = Path(os.environ.get("CODEX_GENERATED_IMAGES", str(Path.home() / ".codex" / "generated_images")))


def log_event(event: str, **fields: Any) -> None:
    """Emit parseable diagnostics without chat or command content."""
    print(json.dumps({"event": event, **fields}, ensure_ascii=False, separators=(",", ":")), flush=True)


class Feishu:
    def __init__(self) -> None:
        # Termux may export a SOCKS proxy without socksio installed. The Feishu
        # client should use the normal network path unless explicitly extended.
        self.http = httpx.Client(timeout=30, trust_env=False)
        self._token = ""
        self._token_expiry = 0.0

    def token(self) -> str:
        if self._token and time.time() < self._token_expiry:
            return self._token
        response = self.http.post(
            "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal",
            json={"app_id": APP_ID, "app_secret": APP_SECRET},
        )
        response.raise_for_status()
        data = response.json()
        self._token = data["tenant_access_token"]
        self._token_expiry = time.time() + int(data.get("expire", 7200)) - 120
        return self._token

    def api(self, path: str, payload: dict[str, Any]) -> dict[str, Any]:
        response = self.http.post(
            "https://open.feishu.cn/open-apis" + path,
            headers={"Authorization": f"Bearer {self.token()}"},
            json=payload,
        )
        response.raise_for_status()
        data = response.json()
        if data.get("code", 0) != 0:
            raise RuntimeError(data)
        return data

    def text(self, chat_id: str, value: str) -> None:
        value = value or "（无文本回复）"
        for offset in range(0, len(value), 3500):
            self.api("/im/v1/messages?receive_id_type=chat_id", {
                "receive_id": chat_id,
                "msg_type": "text",
                "content": json.dumps({"text": value[offset:offset + 3500]}, ensure_ascii=False),
            })

    @staticmethod
    def make_card(title: str, content: str, color: str = "blue",
                  buttons: list[dict[str, Any]] | None = None) -> dict[str, Any]:
        elements: list[dict[str, Any]] = [{"tag": "markdown", "content": content or "（无内容）"}]
        if buttons:
            # Card JSON 2.0 uses a button element with a callback behavior.
            # The legacy `action` container is rejected by the current API.
            for item in buttons:
                button = {
                "tag": "button",
                "width": "fill",
                "text": {"tag": "plain_text", "content": item["text"]},
                "type": item.get("type", "default"),
                "behaviors": [{"type": "callback", "value": item["value"]}],
                }
                if item.get("description"):
                    # Vertical layout is deliberately used for mobile: a narrow
                    # side button truncates labels such as a model identifier.
                    if len(elements) > 1:
                        elements.append({"tag": "hr"})
                    elements.append({"tag": "markdown", "content": item["description"]})
                    elements.append(button)
                elif item.get("group"):
                    if not elements or elements[-1].get("_group") != item["group"]:
                        elements.append({"tag": "markdown", "content": f"<font color='grey'>{item['group']}</font>"})
                        elements.append({"tag": "column_set", "horizontal_spacing": "8px", "columns": [], "_group": item["group"]})
                    elements[-1]["columns"].append({"tag": "column", "width": "weighted", "weight": 1, "elements": [button]})
                else:
                    elements.append(button)
            for element in elements:
                element.pop("_group", None)
        return {"schema": "2.0", "header": {
            "template": color,
            "title": {"tag": "plain_text", "content": title},
        }, "body": {"elements": elements}}

    def card(self, chat_id: str, title: str, content: str, color: str = "blue",
             buttons: list[dict[str, Any]] | None = None) -> str:
        result = self.api("/im/v1/messages?receive_id_type=chat_id", {
            "receive_id": chat_id, "msg_type": "interactive",
            "content": json.dumps(self.make_card(title, content, color, buttons), ensure_ascii=False),
        })
        return str(result.get("data", {}).get("message_id", ""))

    def update_card(self, message_id: str, title: str, content: str, color: str = "blue",
                    buttons: list[dict[str, Any]] | None = None) -> None:
        if not message_id:
            return
        response = self.http.patch(
            f"https://open.feishu.cn/open-apis/im/v1/messages/{message_id}",
            headers={"Authorization": f"Bearer {self.token()}"},
            json={"msg_type": "interactive",
                  "content": json.dumps(self.make_card(title, content, color, buttons), ensure_ascii=False)},
        )
        response.raise_for_status()
        data = response.json()
        if data.get("code", 0) != 0:
            raise RuntimeError(data)

    def card_or_text(self, chat_id: str, title: str, content: str, color: str = "blue",
                     buttons: list[dict[str, Any]] | None = None) -> str:
        try:
            return self.card(chat_id, title, content, color, buttons)
        except Exception as exc:
            log_event("card_send_failed", title=title, error_type=type(exc).__name__)
            self.text(chat_id, f"{title}\n{content}")
            return ""

    def upload_file(self, chat_id: str, path: Path) -> None:
        if path.stat().st_size > MAX_ATTACHMENT:
            self.text(chat_id, f"交付物过大，未上传：{path.name}（{path.stat().st_size} bytes）")
            return
        mime, _ = mimetypes.guess_type(path.name)
        if mime and mime.startswith("image/"):
            with path.open("rb") as stream:
                response = self.http.post(
                    "https://open.feishu.cn/open-apis/im/v1/images",
                    headers={"Authorization": f"Bearer {self.token()}"},
                    data={"image_type": "message"},
                    files={"image": (path.name, stream, mime)},
                )
            response.raise_for_status()
            image_key = response.json()["data"]["image_key"]
            self.api("/im/v1/messages?receive_id_type=chat_id", {
                "receive_id": chat_id, "msg_type": "image",
                "content": json.dumps({"image_key": image_key}),
            })
            return
        with path.open("rb") as stream:
            response = self.http.post(
                "https://open.feishu.cn/open-apis/im/v1/files",
                headers={"Authorization": f"Bearer {self.token()}"},
                data={"file_type": "stream", "file_name": path.name},
                files={"file": (path.name, stream)},
            )
        response.raise_for_status()
        file_key = response.json()["data"]["file_key"]
        self.api("/im/v1/messages?receive_id_type=chat_id", {
            "receive_id": chat_id,
            "msg_type": "file",
            "content": json.dumps({"file_key": file_key}),
        })

    def download_resource(self, message_id: str, resource_key: str, resource_type: str,
                          filename: str = "") -> Path:
        response = self.http.get(
            f"https://open.feishu.cn/open-apis/im/v1/messages/{message_id}/resources/{resource_key}",
            params={"type": resource_type},
            headers={"Authorization": f"Bearer {self.token()}"},
        )
        response.raise_for_status()
        inbox = ROOT / "feishu-inbox"
        inbox.mkdir(mode=0o700, exist_ok=True)
        safe_name = Path(filename).name if filename else resource_key
        if "." not in safe_name:
            mime = response.headers.get("content-type", "").split(";", 1)[0].lower()
            extensions = {"image/jpeg": ".jpg", "image/png": ".png", "image/gif": ".gif",
                          "image/webp": ".webp", "application/pdf": ".pdf"}
            safe_name += extensions.get(mime, "")
        destination = inbox / f"{message_id}-{safe_name}"
        destination.write_bytes(response.content)
        return destination


class CodexServer:
    def __init__(self, event: Callable[[str, Any], None]) -> None:
        self.command = shlex.split(os.environ.get("CODEX_APP_SERVER", "codex app-server"))
        self.event = event
        self.write_lock = threading.Lock()
        self.rpc_id = 0
        self.threads: dict[str, str] = {}
        self.approvals: dict[int, str] = {}
        self.approval_created: dict[int, float] = {}
        self.turn_text = ""
        self.last_turn_status = ""
        self.active_turn: dict[str, str] = {}
        self.starting_turns: set[str] = set()
        self.pending_interrupt: set[str] = set()
        self._spawn()

    def _spawn(self) -> None:
        self.pending_lock = threading.Lock()
        self.pending: dict[int, queue.Queue] = {}
        self.notifications = queue.Queue()
        self.completions = queue.Queue()
        self.process = subprocess.Popen(
            self.command, cwd=ROOT, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            # stdout is a JSON-RPC stream; diagnostics must never be merged into it.
            stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        self.reader = threading.Thread(target=self._receive_rpc, args=(self.process, self.notifications, self.completions), daemon=True)
        self.reader.start()
        threading.Thread(target=self._dispatch_events, args=(self.notifications, self.completions), daemon=True).start()
        self._initialize()

    def _receive_rpc(self, process, notifications, completions) -> None:
        try:
            for line in process.stdout:
                message = json.loads(line)
                if "method" in message:
                    notifications.put(message)
                else:
                    with self.pending_lock:
                        waiter = self.pending.get(message.get("id"))
                    if waiter:
                        waiter.put(message)
        finally:
            error = {"error": "codex app-server 连接已关闭"}
            with self.pending_lock:
                for waiter in self.pending.values():
                    waiter.put(error)
            notifications.put(None)

    def _dispatch_events(self, notifications, completions) -> None:
        while True:
            message = notifications.get()
            if message is None:
                completions.put({"error": "codex app-server 连接已关闭"})
                return
            try:
                self.handle_event(message)
            except Exception as exc:
                log_event("codex_event_delivery_failed", error_type=type(exc).__name__)
            finally:
                if message.get("method") == "turn/completed":
                    completions.put(message)

    def restart(self) -> None:
        old = self.process
        if old.poll() is None:
            old.kill()
            old.wait(timeout=5)
        self.reader.join(timeout=5)
        self.rpc_id = 0
        self.approvals.clear()
        self.approval_created.clear()
        self.active_turn.clear()
        self.threads.clear()
        self._spawn()

    def send(self, message: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        with self.write_lock:
            self.process.stdin.write(json.dumps(message, ensure_ascii=False) + "\n")
            self.process.stdin.flush()

    def _initialize(self) -> None:
        self.request("initialize", {"clientInfo": {"name": "feishu-codex-bridge", "version": "0.1.0"}})
        self.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

    def request(self, method: str, params: dict[str, Any], timeout: int = 30) -> dict[str, Any]:
        with self.pending_lock:
            self.rpc_id += 1
            ident = self.rpc_id
            waiter = queue.Queue()
            self.pending[ident] = waiter
        try:
            self.send({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
            message = waiter.get(timeout=timeout)
            if "error" in message:
                raise RuntimeError(message["error"])
            return message.get("result", {})
        except queue.Empty:
            raise TimeoutError(f"Codex 请求超时：{method}") from None
        finally:
            with self.pending_lock:
                self.pending.pop(ident, None)

    def handle_event(self, message: dict[str, Any]) -> None:
        method = message.get("method", "")
        params = message.get("params", {})
        if message.get("id") is not None and method.endswith("requestApproval"):
            self.event("approval", message)
        elif method.endswith("agentMessage/delta"):
            delta = params.get("delta", params.get("text", ""))
            self.turn_text += delta
            self.event("delta", delta)
        elif method == "turn/completed":
            turn = params.get("turn", {})
            if isinstance(turn, dict):
                self.last_turn_status = str(turn.get("status", ""))
            self.event("completed", params)
        elif method.startswith("item/"):
            self.event("item", params)

    def turn(self, key: str, prompt: str, model: str,
             extra_inputs: list[dict[str, Any]] | None = None) -> None:
        self.turn_text = ""
        self.last_turn_status = ""
        thread_id = self.threads.get(key)
        if not thread_id:
            result = self.request("thread/start", {"cwd": str(ROOT)})
            thread = result.get("thread", result)
            thread_id = thread["id"]
            self.threads[key] = thread_id
        params: dict[str, Any] = {
            "threadId": thread_id,
            "input": [{"type": "text", "text": prompt}] + (extra_inputs or []),
        }
        if model:
            params["model"] = model
        self.starting_turns.add(key)
        try:
            result = self.request("turn/start", params)
            turn = result.get("turn", result)
            if turn.get("id"):
                self.active_turn[key] = turn["id"]
                if key in self.pending_interrupt:
                    self._send_interrupt(key)
        finally:
            self.starting_turns.discard(key)
        while True:
            message = self.completions.get()
            if "error" in message:
                self.active_turn.pop(key, None)
                raise RuntimeError(message["error"])
            completed = message.get("params", {})
            if completed.get("threadId") == thread_id and completed.get("turn", {}).get("id") == turn.get("id"):
                self.active_turn.pop(key, None)
                return

    def resume(self, key: str, thread_id: str) -> None:
        self.request("thread/resume", {"threadId": thread_id, "cwd": str(ROOT)})
        self.threads[key] = thread_id

    def _send_interrupt(self, key: str) -> None:
        thread_id = self.threads.get(key)
        turn_id = self.active_turn.get(key)
        if not thread_id or not turn_id:
            return
        # Do not synchronously read the RPC response here: the worker is already
        # consuming the app-server stream for the active turn.
        self.request("turn/interrupt", {"threadId": thread_id, "turnId": turn_id})
        self.pending_interrupt.discard(key)

    def interrupt(self, key: str) -> bool:
        if not self.active_turn.get(key) and key not in self.starting_turns:
            self.pending_interrupt.discard(key)
            return False
        if not self.active_turn.get(key):
            self.pending_interrupt.add(key)
            return False
        self._send_interrupt(key)
        return True

    def compact(self, key: str) -> None:
        thread_id = self.threads.get(key)
        if not thread_id:
            raise RuntimeError("当前还没有 Codex 会话")
        self.request("thread/compact/start", {"threadId": thread_id})

    def list_threads(self) -> list[dict[str, Any]]:
        result = self.request("thread/list", {"cwd": [str(ROOT)], "limit": 20,
                                                "sortKey": "updated_at", "sortDirection": "desc"})
        return result.get("data", [])

    def models(self) -> list[str]:
        result = self.request("model/list", {})
        values = result.get("data", result.get("models", []))
        return [item.get("id", str(item)) for item in values]

    def approve(self, request_id: int, yes: bool) -> None:
        if request_id not in self.approvals:
            raise KeyError(request_id)
        self.send({"jsonrpc": "2.0", "id": request_id, "result": {
            "decision": "accept" if yes else "decline"
        }})
        del self.approvals[request_id]
        self.approval_created.pop(request_id, None)


class Bridge:
    def __init__(self) -> None:
        self.started_at = time.monotonic()
        self.feishu = Feishu()
        self.server = CodexServer(self.codex_event)
        self.models = self.load_model_settings()
        self.current_chat: dict[str, str] = {}
        self.stream_buffers: dict[str, str] = {}
        self.active_cards: dict[str, str] = {}
        self.active_task_tokens: dict[str, str] = {}
        self.stopping_tasks: set[str] = set()
        self.task_lock = threading.Lock()
        self.help_cards: dict[str, tuple[str, str]] = {}
        self.approval_cards: dict[int, str] = {}
        self.approval_summaries: dict[int, str] = {}
        self.approval_items: dict[str, dict[str, Any]] = {}
        self.approval_lock = threading.Lock()
        self.card_updated_at: dict[str, float] = {}
        self.progress_lock = threading.Lock()
        self.progress: dict[str, Any] = {}
        self.jobs: queue.Queue[tuple[str, str, str, dict[str, str] | None, int]] = queue.Queue()
        self.generations: dict[str, int] = {}
        self.seen_messages: deque[str] = deque(maxlen=1000)
        self.seen_message_set: set[str] = set()
        self.seen_lock = threading.Lock()
        self.load_seen_messages()
        threading.Thread(target=self.worker, daemon=True).start()
        threading.Thread(target=self.approval_reaper, daemon=True).start()
        threading.Thread(target=self.progress_worker, daemon=True).start()

    def progress_worker(self) -> None:
        while True:
            time.sleep(2)
            with self.progress_lock:
                state = self.progress
                if not state or not state.get("card"):
                    continue
                elapsed = int(time.monotonic() - state["started"])
                preview = state.get("text") or "任务仍在运行，暂未收到新的输出。"
                content = f"已耗时 {elapsed} 秒\n\n{preview[-5000:]}"
                try:
                    self.feishu.update_card(state["card"], "Codex 处理中", content, "blue", [
                        {"text": "停止任务", "type": "danger", "value": {"command": "/stop", "task_id": state["task_id"]}}
                    ])
                except Exception as exc:
                    log_event("progress_update_failed", error_type=type(exc).__name__)

    def session_key(self, user_id: str) -> str:
        return f"{user_id}:{ROOT}"

    def load_model_settings(self) -> dict[str, str]:
        try:
            parsed = json.loads(SETTINGS_FILE.read_text(encoding="utf-8"))
            return {str(key): str(value) for key, value in parsed.get("models", {}).items()}
        except (FileNotFoundError, OSError, json.JSONDecodeError, AttributeError):
            return {}

    def save_model_settings(self) -> None:
        temporary = SETTINGS_FILE.with_name(SETTINGS_FILE.name + ".tmp")
        temporary.write_text(json.dumps({"models": self.models}, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        temporary.chmod(0o600)
        os.replace(temporary, SETTINGS_FILE)

    def load_session(self, key: str = "") -> str:
        try:
            raw = SESSION_FILE.read_text(encoding="utf-8").strip()
            if not raw:
                return ""
            try:
                sessions = json.loads(raw)
            except json.JSONDecodeError:
                return raw
            return str(sessions.get(key, "")) if isinstance(sessions, dict) else ""
        except FileNotFoundError:
            return ""

    def save_session(self, thread_id: str, key: str = "") -> None:
        sessions: dict[str, str] = {}
        if SESSION_FILE.exists():
            try:
                raw = SESSION_FILE.read_text(encoding="utf-8").strip()
                parsed = json.loads(raw) if raw else {}
                if isinstance(parsed, dict):
                    sessions = {str(k): str(v) for k, v in parsed.items()}
                elif raw and key:
                    sessions[key] = raw
            except (OSError, json.JSONDecodeError):
                pass
        sessions[key] = thread_id
        temporary = SESSION_FILE.with_name(SESSION_FILE.name + ".tmp")
        temporary.write_text(json.dumps(sessions, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        temporary.chmod(0o600)
        os.replace(temporary, SESSION_FILE)

    def clear_session(self, key: str = "") -> None:
        try:
            raw = SESSION_FILE.read_text(encoding="utf-8").strip()
            parsed = json.loads(raw) if raw else {}
            if not isinstance(parsed, dict) or not key:
                SESSION_FILE.unlink()
                return
            parsed.pop(key, None)
            if parsed:
                temporary = SESSION_FILE.with_name(SESSION_FILE.name + ".tmp")
                temporary.write_text(json.dumps(parsed, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
                temporary.chmod(0o600)
                os.replace(temporary, SESSION_FILE)
            else:
                SESSION_FILE.unlink()
        except FileNotFoundError:
            pass
        except json.JSONDecodeError:
            SESSION_FILE.unlink(missing_ok=True)

    def load_seen_messages(self) -> None:
        """Restore a bounded event-id journal so reconnects cannot replay work."""
        try:
            parsed = json.loads(SEEN_MESSAGES_FILE.read_text(encoding="utf-8"))
            if not isinstance(parsed, list):
                return
            for message_id in parsed[-self.seen_messages.maxlen:]:
                message_id = str(message_id)
                if message_id and message_id not in self.seen_message_set:
                    self.seen_messages.append(message_id)
                    self.seen_message_set.add(message_id)
        except (FileNotFoundError, OSError, json.JSONDecodeError):
            return

    def save_seen_messages(self) -> None:
        temporary = SEEN_MESSAGES_FILE.with_name(SEEN_MESSAGES_FILE.name + ".tmp")
        try:
            temporary.write_text(json.dumps(list(self.seen_messages), ensure_ascii=False) + "\n", encoding="utf-8")
            temporary.chmod(0o600)
            os.replace(temporary, SEEN_MESSAGES_FILE)
        except OSError as exc:
            temporary.unlink(missing_ok=True)
            log_event("message_dedup_save_failed", error_type=type(exc).__name__)

    def remember_message(self, message_id: str) -> bool:
        if not message_id:
            return False
        with self.seen_lock:
            if message_id in self.seen_message_set:
                return False
            if len(self.seen_messages) == self.seen_messages.maxlen:
                self.seen_message_set.discard(self.seen_messages[0])
            self.seen_messages.append(message_id)
            self.seen_message_set.add(message_id)
            self.save_seen_messages()
            return True

    def approval_summary(self, request: dict[str, Any]) -> str:
        params = request.get("params", {})
        item = self.approval_items.get(params.get("itemId", ""), {})
        method = request.get("method", "")
        category = "文件修改" if "fileChange" in method else "命令执行" if "commandExecution" in method else "权限请求"
        parts = [f"**操作类型：{category}**"]
        fields = [("申请原因", params.get("reason")),
                  ("执行命令", params.get("command") or item.get("command")),
                  ("工作目录", params.get("cwd") or item.get("cwd")),
                  ("文件改动", item.get("changes") or params.get("changes")),
                  ("申请写入目录", params.get("grantRoot")),
                  ("额外权限", params.get("additionalPermissions") or params.get("permissions")),
                  ("网络访问", params.get("networkApprovalContext"))]
        found = False
        for label, value in fields:
            if value is None or value == "":
                continue
            found = True
            if label == "文件改动" and isinstance(value, list):
                for change in value:
                    if not isinstance(change, dict):
                        parts.append("文件改动格式无法识别，请确认完整操作后再审批。")
                        continue
                    kind = change.get("kind", {})
                    operation = kind.get("type", "") if isinstance(kind, dict) else kind
                    operation = {"add": "新增", "delete": "删除", "update": "修改"}.get(operation, operation or "未提供")
                    path = str(change.get("path") or "未提供").replace("```", "` ` `")
                    parts.append(f"**文件路径**\n```\n{path}\n```\n**改动类型：{operation}**")
                    if isinstance(kind, dict) and kind.get("movePath"):
                        target = str(kind["movePath"]).replace("```", "` ` `")
                        parts.append(f"**移动目标**\n```\n{target}\n```")
                    detail = change.get("diff")
                    if detail is None:
                        parts.append("具体内容未提供。")
                    else:
                        detail = str(detail)
                        if len(detail) > 2400:
                            detail = detail[:2400] + "\n…（内容已截断，请确认完整操作后再审批）"
                        detail = detail.replace("```", "` ` `")
                        parts.append(f"**具体内容**\n```\n{detail}\n```")
                continue
            detail = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False, indent=2)
            if len(detail) > 2400:
                detail = detail[:2400] + "\n…（内容已截断，请确认完整操作后再审批）"
            detail = detail.replace("```", "` ` `")
            parts.append(f"**{label}**\n```\n{detail}\n```")
        if not found:
            parts.append("请求未提供操作详情，暂时无法判断具体改动；建议拒绝并让 Codex 补充说明。")
        parts.append(f"审批编号：`{request['id']}`")
        return "\n\n".join(parts)

    def codex_event(self, kind: str, value: Any) -> None:
        if kind == "item":
            item = value.get("item", {})
            if item.get("id"):
                self.approval_items[item["id"]] = item
        elif kind == "delta":
            chat_id = self.current_chat.get("active", "")
            if not chat_id:
                return
            buffer = self.stream_buffers.get(chat_id, "") + str(value)
            while len(buffer) >= STREAM_CHUNK:
                buffer = buffer[STREAM_CHUNK:]
            self.stream_buffers[chat_id] = buffer
            with self.progress_lock:
                if self.progress:
                    self.progress["text"] = self.server.turn_text
        elif kind == "approval":
            request_id = int(value["id"])
            self.server.approvals[request_id] = self.current_chat.get("active", "")
            self.server.approval_created[request_id] = time.time()
            chat_id = self.server.approvals[request_id]
            if chat_id:
                summary = self.approval_summary(value)
                self.approval_summaries[request_id] = summary
                self.approval_cards[request_id] = self.feishu.card_or_text(chat_id, "需要审批", summary, "yellow", [
                    {"text": "允许", "type": "primary", "value": {"command": "/approve", "id": request_id}},
                    {"text": "拒绝", "type": "danger", "value": {"command": "/deny", "id": request_id}},
                ])

    def approval_reaper(self) -> None:
        while True:
            time.sleep(5)
            now = time.time()
            for request_id, created in list(self.server.approval_created.items()):
                if now - created < APPROVAL_TIMEOUT:
                    continue
                chat_id = self.server.approvals.get(request_id, "")
                try:
                    self.resolve_approval(request_id, chat_id, False, expired=True)
                except Exception:
                    self.server.approval_created.pop(request_id, None)

    def worker(self) -> None:
        while True:
            key, chat_id, prompt, resource, generation = self.jobs.get()
            if generation != self.generations.get(key, 0):
                self.feishu.card_or_text(chat_id, "任务已取消", "任务仍在等待队列中，已取消执行。", "red")
                self.jobs.task_done()
                continue
            task_id = uuid.uuid4().hex
            with self.task_lock:
                self.active_task_tokens[key] = task_id
                self.stopping_tasks.discard(key)
            self.current_chat["active"] = chat_id
            self.approval_items.clear()
            self.current_chat["key"] = key
            try:
                before = self.snapshot()
                started_at = time.time()
                card_id = self.feishu.card_or_text(chat_id, "Codex 开始处理", f"目录：`{ROOT}`\n\n正在准备执行…", "blue", [
                    {"text": "停止任务", "type": "danger", "value": {"command": "/stop", "task_id": task_id}}
                ])
                self.active_cards[chat_id] = card_id
                with self.progress_lock:
                    self.progress = {"card": card_id, "task_id": task_id, "started": time.monotonic(), "text": "正在准备执行…"}
                self.card_updated_at[chat_id] = time.time()
                extra_inputs: list[dict[str, Any]] = []
                if resource:
                    local_path = self.feishu.download_resource(
                        resource["message_id"], resource["resource_key"],
                        resource["resource_type"], resource.get("filename", ""),
                    )
                    if resource["resource_type"] == "image":
                        extra_inputs.append({"type": "localImage", "path": str(local_path), "detail": "auto"})
                        prompt += "\n\n用户附加了一张图片，请直接分析图片内容。"
                    else:
                        prompt += f"\n\n用户附加了一个文件，请读取它：{local_path}。"
                stored = self.load_session(key)
                if stored and key not in self.server.threads:
                    self.server.resume(key, stored)
                self.server.turn(key, prompt, self.models.get(key, DEFAULT_MODEL), extra_inputs)
                self.save_session(self.server.threads[key], key)
                with self.progress_lock:
                    self.progress = {}
                self.stream_buffers.pop(chat_id, "")
                elapsed = int(time.time() - started_at)
                if self.server.last_turn_status == "interrupted":
                    self.finish_card(chat_id, "任务已停止", "Codex turn 已停止。", "red")
                else:
                    status = self.server.last_turn_status
                    self.finish_card(chat_id, "Codex 执行失败" if status == "failed" else "Codex 已完成",
                                     f"耗时 {elapsed} 秒\n\n" + (self.server.turn_text or "没有返回文字。"),
                                     "red" if status == "failed" else "green")
                paths = list(dict.fromkeys(self.changed_files(before) + self.generated_files(key, started_at)))
                deliveries = []
                for path in paths:
                    try:
                        size = path.stat().st_size
                        if size > MAX_ATTACHMENT:
                            deliveries.append(f"- {path.name}：超过上传大小限制，未上传")
                            continue
                        self.feishu.upload_file(chat_id, path)
                        deliveries.append(f"- {path.name} · {size:,} bytes · 已发送")
                    except Exception as exc:
                        deliveries.append(f"- {path.name} · 上传失败（{type(exc).__name__}）")
                if deliveries:
                    self.feishu.card_or_text(chat_id, "交付物", "\n".join(deliveries))
            except Exception as exc:
                log_event("worker_error", error_type=type(exc).__name__)
                if self.server.process.poll() is not None:
                    try:
                        self.server.restart()
                        self.feishu.card_or_text(chat_id, "Codex 进程已重启", "下一条消息会自动恢复会话。", "yellow")
                    except Exception as restart_error:
                        self.feishu.card_or_text(chat_id, "Codex 自动重启失败", str(restart_error), "red")
                self.finish_card(chat_id, "Codex 执行失败", str(exc), "red")
            finally:
                with self.progress_lock:
                    self.progress = {}
                self.active_cards.pop(chat_id, None)
                self.card_updated_at.pop(chat_id, None)
                self.current_chat.pop("active", None)
                self.current_chat.pop("key", None)
                with self.task_lock:
                    if self.active_task_tokens.get(key) == task_id:
                        self.active_task_tokens.pop(key, None)
                        self.stopping_tasks.discard(key)
                self.jobs.task_done()

    @staticmethod
    def split_card_content(content: str, max_chars: int = 5600) -> list[str]:
        """Split long Markdown without leaving a code fence open in a card."""
        chunks: list[str] = []
        current = ""
        fence = ""

        def update_fence(line: str) -> None:
            nonlocal fence
            marker = line.lstrip()
            if marker.startswith("```"):
                fence = "" if fence else marker[3:].strip()

        def flush() -> None:
            nonlocal current
            if not current:
                return
            if fence:
                current = current.rstrip() + "\n```\n"
            chunks.append(current)
            current = f"```{fence}\n" if fence else ""

        for line in content.splitlines(keepends=True) or [content]:
            remaining = line
            while remaining:
                capacity = max_chars - len(current)
                if capacity <= 0:
                    flush()
                    continue
                if len(remaining) <= capacity:
                    current += remaining
                    update_fence(remaining)
                    break
                if current:
                    flush()
                    continue
                # A single unbroken line is longer than the card limit.
                current = remaining[:capacity]
                remaining = remaining[capacity:]
                flush()
        if current:
            if fence:
                current = current.rstrip() + "\n```\n"
            chunks.append(current)
        return chunks or ["（无内容）"]

    def finish_card(self, chat_id: str, title: str, content: str, color: str) -> None:
        with self.progress_lock:
            self.progress = {}
        card_id = self.active_cards.get(chat_id, "")
        chunks = self.split_card_content(content)
        if card_id:
            try:
                # Keep the primary card within Feishu's practical card size;
                # continuation cards preserve the complete long response.
                self.feishu.update_card(card_id, title, chunks[0], color)
                chunks = chunks[1:]
            except Exception as exc:
                log_event("card_update_failed", title=title, error_type=type(exc).__name__)
        for index, chunk in enumerate(chunks, 2):
            self.feishu.card_or_text(chat_id, f"{title}（续 {index}）", chunk, color)

    def snapshot(self) -> dict[str, tuple[int, int]]:
        result: dict[str, tuple[int, int]] = {}
        for path in ROOT.rglob("*"):
            if (not path.is_file() or ".git" in path.parts or ".runtime" in path.parts or
                    "feishu-inbox" in path.parts or path.name.startswith(".feishu-codex")):
                continue
            try:
                stat = path.stat()
                result[str(path)] = (stat.st_mtime_ns, stat.st_size)
            except OSError:
                pass
        return result

    def changed_files(self, before: dict[str, tuple[int, int]]) -> list[Path]:
        after = self.snapshot()
        changed: list[Path] = []
        for name, metadata in after.items():
            if before.get(name) != metadata:
                path = Path(name)
                if path.is_relative_to(ROOT) and path.stat().st_size <= MAX_ATTACHMENT:
                    changed.append(path)
        return changed[:10]

    def generated_files(self, key: str, started_at: float) -> list[Path]:
        thread_id = self.server.threads.get(key)
        if not thread_id:
            return []
        directory = GENERATED_IMAGES / thread_id
        if not directory.is_dir():
            return []
        result: list[Path] = []
        for path in directory.iterdir():
            if path.is_file() and path.suffix.lower() in {".png", ".jpg", ".jpeg", ".webp", ".gif"}:
                try:
                    if path.stat().st_mtime >= started_at - 2:
                        result.append(path)
                except OSError:
                    pass
        return sorted(result, key=lambda p: p.stat().st_mtime)[:10]

    def receive(self, data: lark.im.v1.P2ImMessageReceiveV1) -> None:
        event = data.event
        message = event.message
        message_id = getattr(message, "message_id", "")
        message_tag = hashlib.sha256(message_id.encode()).hexdigest()[:12] if message_id else "missing"
        if not self.remember_message(message_id):
            log_event("message_ignored", reason="duplicate", message=message_tag)
            return
        sender = getattr(getattr(event, "sender", None), "sender_id", None)
        user_id = getattr(sender, "open_id", "")
        user_tag = hashlib.sha256(user_id.encode()).hexdigest()[:12] if user_id else "missing"
        log_event("message_received", user=user_tag, message=message_tag)
        if ALLOWED and user_id not in ALLOWED:
            log_event("message_ignored", reason="unauthorized", user=user_tag)
            return
        content = json.loads(message.content or "{}")
        message_type = getattr(message, "message_type", "text")
        resource: dict[str, str] | None = None
        if message_type in ("image", "file", "media", "audio", "video"):
            resource_key = content.get("image_key") or content.get("file_key") or content.get("media_key")
            if resource_key:
                resource = {"message_id": message.message_id, "resource_key": resource_key,
                            "resource_type": "image" if message_type == "image" else "file",
                            "filename": content.get("file_name", "")}
                text = f"用户发送了一个{message_type}，请查看附件内容。"
            else:
                text = "用户发送了一个无法读取的附件。"
        else:
            text = content.get("text", "").strip()
        key = self.session_key(user_id)
        if text.startswith("/"):
            # Commands such as /resume or /models may take long enough to
            # starve the Feishu WebSocket heartbeat. Run them off the callback.
            threading.Thread(target=self.command, args=(user_id, message.chat_id, key, text), daemon=True).start()
        else:
            generation = self.generations.get(key, 0)
            self.jobs.put((key, message.chat_id, text, resource, generation))

    def resolve_approval(self, request_id: int, chat_id: str, yes: bool,
                         expired: bool = False, source: str = "") -> None:
        with self.approval_lock:
            if self.server.approvals.get(request_id) != chat_id:
                raise ValueError("审批已处理、已过期或不属于当前聊天")
            if source and self.approval_cards.get(request_id) != source:
                raise ValueError("旧审批卡片已失效")
            self.server.approve(request_id, yes)
            card_id = self.approval_cards.pop(request_id, "")
            summary = self.approval_summaries.pop(request_id, "")
        title = "审批已超时" if expired else ("已允许" if yes else "已拒绝")
        content = f"审批编号：`{request_id}`\n\n" + ("已自动拒绝。" if expired else "审批决定已提交。")
        if summary:
            content = summary + "\n\n" + ("已超时，自动拒绝。" if expired else "审批决定已提交。")
        if card_id:
            try:
                self.feishu.update_card(card_id, title, content, "green" if yes else "grey")
                return
            except Exception as exc:
                log_event("approval_card_update_failed", error_type=type(exc).__name__)
        self.feishu.card_or_text(chat_id, title, content)

    def command(self, user_id: str, chat_id: str, key: str, text: str, source: str = "") -> None:
        parts = text.split(maxsplit=1)
        command, argument = parts[0].lower(), parts[1].strip() if len(parts) == 2 else ""
        log_event("command_received", command=command, via="card" if source else "text")
        if self.current_chat.get("active") and (command in ("/new", "/compact") or (command == "/resume" and argument)):
            self.feishu.card_or_text(chat_id, "任务执行中", "请等待当前任务结束或先停止任务，再切换会话或压缩上下文。", "yellow")
            return
        if command == "/help":
            card_id = self.feishu.card_or_text(chat_id, "Codex 控制面板", "点击执行操作，也支持输入 /命令。", buttons=[
                {"text": f"{label} {cmd}", "group": group, "type": "danger" if cmd == "/stop" else "default", "value": {"command": cmd}}
                for group, label, cmd in [("会话", "恢复", "/resume"), ("会话", "新建", "/new"),
                                         ("模型", "当前", "/model"), ("模型", "列表", "/models"),
                                         ("任务", "状态", "/status"), ("任务", "压缩", "/compact"),
                                         ("", "停止", "/stop")]
            ])
            if card_id:
                self.help_cards[card_id] = (chat_id, key)
                if len(self.help_cards) > 100:
                    self.help_cards.pop(next(iter(self.help_cards)))
        elif command == "/new":
            self.server.threads.pop(key, None)
            self.clear_session(key)
            self.feishu.card_or_text(chat_id, "新会话", "已切换到新会话，下次提问时自动创建。", "green")
        elif command == "/resume" and argument:
            try:
                self.server.resume(key, argument)
                self.save_session(argument, key)
                self.feishu.card_or_text(chat_id, "会话已恢复", f"会话 ID：`{argument}`", "green")
            except Exception as exc:
                self.feishu.card_or_text(chat_id, "恢复会话失败", str(exc), "red")
        elif command == "/resume":
            try:
                threads = self.server.list_threads()
                shown = threads[:8]
                content = "选择下方会话继续对话。" if shown else "没有找到会话"
                buttons = [{"text": "恢复", "description": f"**{index}. {(item.get('title') or '未命名')[:60]}**\n`{str(item.get('id'))[:8]}…`", "type": "primary", "value": {"command": "/resume", "thread_id": item.get("id")}}
                           for index, item in enumerate(shown, 1) if item.get("id")]
                # A resume button carries the same command semantics as text.
                for button in buttons:
                    button["value"]["command"] = "/resume"
                self.feishu.card_or_text(chat_id, "可恢复会话", content, "blue", buttons)
            except Exception as exc:
                self.feishu.card_or_text(chat_id, "读取会话列表失败", str(exc), "red")
        elif command == "/model":
            if argument:
                self.models[key] = argument
                self.save_model_settings()
                self.feishu.card_or_text(chat_id, "模型已切换", f"后续请求使用：`{argument}`", "green")
            else:
                self.feishu.card_or_text(chat_id, "当前模型", f"`{self.models.get(key, DEFAULT_MODEL) or 'Codex 默认'}`")
        elif command == "/models":
            try:
                available = self.server.models()
                selected = self.models.get(key, DEFAULT_MODEL)
                buttons = [{
                    "text": "当前模型" if model == selected else "使用此模型",
                    "description": f"**{model}**" + ("\n\n当前使用" if model == selected else ""),
                    "type": "primary" if model == selected else "default",
                    "value": {"command": "/model", "model": model},
                } for model in available[:12]]
                self.feishu.card_or_text(chat_id, "选择模型", "点击模型后，后续请求会使用该模型。" if buttons else "暂无模型", "blue", buttons)
            except Exception as exc:
                self.feishu.card_or_text(chat_id, "读取模型失败", str(exc), "red")
        elif command == "/status":
            thread_id = self.server.threads.get(key) or self.load_session(key)
            with self.task_lock:
                active_task = self.active_task_tokens.get(key)
                stopping = key in self.stopping_tasks
            if stopping:
                task_state = "正在停止"
            elif active_task:
                task_state = "执行中"
            elif self.jobs.qsize():
                task_state = f"等待队列中（共 {self.jobs.qsize()} 项）"
            else:
                task_state = "空闲"
            uptime = int(time.monotonic() - self.started_at)
            uptime_text = f"{uptime // 3600} 小时 {(uptime % 3600) // 60} 分 {uptime % 60} 秒"
            self.feishu.card_or_text(chat_id, "Codex 状态", f"**任务**\n{task_state}\n\n**目录**\n`{ROOT}`\n\n**会话**\n`{thread_id or '尚未创建'}`\n\n**模型**\n`{self.models.get(key, DEFAULT_MODEL) or '默认'}`\n\n**桥接运行时长**\n{uptime_text}")
        elif command == "/stop":
            try:
                with self.task_lock:
                    active_task = self.active_task_tokens.get(key, "")
                    if argument and active_task != argument:
                        raise ValueError("这张任务卡已失效，不能停止其他任务")
                    if active_task and key in self.stopping_tasks:
                        self.feishu.card_or_text(chat_id, "停止请求已提交", "任务正在停止，请等待结果。", "yellow")
                        return
                    if active_task:
                        self.stopping_tasks.add(key)
                self.generations[key] = self.generations.get(key, 0) + 1
                interrupted = self.server.interrupt(key)
                self.feishu.card_or_text(chat_id, "停止请求已提交", "正在停止当前 Codex turn。" if interrupted else "已取消排队任务，并登记停止正在启动的任务。", "yellow")
            except Exception as exc:
                with self.task_lock:
                    self.stopping_tasks.discard(key)
                self.feishu.card_or_text(chat_id, "停止失败", str(exc), "red")
        elif command == "/compact":
            try:
                self.server.compact(key)
                self.feishu.card_or_text(chat_id, "上下文压缩", "已请求压缩当前会话上下文。", "green")
            except Exception as exc:
                self.feishu.card_or_text(chat_id, "压缩失败", str(exc), "red")
        elif command in ("/approve", "/deny") and argument.isdigit():
            try:
                self.resolve_approval(int(argument), chat_id, command == "/approve", source=source)
            except Exception as exc:
                self.feishu.card_or_text(chat_id, f"审批失败：{argument}", str(exc), "red")
        else:
            self.feishu.card_or_text(chat_id, "未知命令", "请发送 `/help` 查看可用命令。", "yellow")


bridge: Bridge | None = None


def on_message(data: lark.im.v1.P2ImMessageReceiveV1) -> None:
    assert bridge is not None
    bridge.receive(data)


def on_card_action(data: Any) -> Any:
    """Handle buttons from interactive cards over the same WebSocket."""
    from lark_oapi.event.callback.model.p2_card_action_trigger import P2CardActionTriggerResponse

    try:
        event = data.event
        operator = event.operator
        action = event.action
        context = event.context
        user_id = getattr(operator, "open_id", "")
        value = getattr(action, "value", {}) or {}
        command = str(value.get("command", ""))
        if command in ("/approve", "/deny"):
            command += f" {int(value.get('id'))}"
        elif command == "/resume" and value.get("thread_id"):
            command += f" {value['thread_id']}"
        elif command == "/stop" and value.get("task_id"):
            command += f" {value['task_id']}"
        elif command == "/model" and value.get("model"):
            command += f" {value['model']}"
        chat_id = getattr(context, "open_chat_id", "")
        if bridge and chat_id and command.startswith("/"):
            if ALLOWED and user_id not in ALLOWED:
                return P2CardActionTriggerResponse({})
            log_event("card_action_received", command=command.split(maxsplit=1)[0])
            threading.Thread(
                target=bridge.command,
                args=(user_id, chat_id, bridge.session_key(user_id), command,
                      getattr(context, "open_message_id", "")),
                daemon=True,
            ).start()
    except Exception as exc:
        log_event("card_action_failed", error_type=type(exc).__name__)
    return P2CardActionTriggerResponse({})


def patch_lark_card_callback(lark: Any) -> None:
    """Work around lark-oapi 1.7.x dropping CARD frames in WebSocket mode."""
    import inspect
    from lark_oapi.core.json import JSON
    from lark_oapi.ws.client import _get_by_key
    from lark_oapi.ws.const import HEADER_BIZ_RT, HEADER_MESSAGE_ID, HEADER_SEQ, HEADER_SUM, HEADER_TRACE_ID, HEADER_TYPE
    from lark_oapi.ws.enum import MessageType
    from lark_oapi.ws.model import Response
    import http
    import time as _time

    client_class = lark.ws.Client
    source = inspect.getsource(client_class._handle_data_frame)
    if "message_type == MessageType.CARD" not in source:
        return

    async def handle_data_frame(self: Any, frame: Any) -> None:
        hs = frame.headers
        msg_id = _get_by_key(hs, HEADER_MESSAGE_ID)
        trace_id = _get_by_key(hs, HEADER_TRACE_ID)
        sum_ = _get_by_key(hs, HEADER_SUM)
        seq = _get_by_key(hs, HEADER_SEQ)
        type_ = _get_by_key(hs, HEADER_TYPE)
        payload = frame.payload
        if int(sum_) > 1:
            payload = self._combine(msg_id, int(sum_), int(seq), payload)
            if payload is None:
                return
        message_type = MessageType(type_)
        if message_type not in (MessageType.EVENT, MessageType.CARD):
            return
        response = Response(code=http.HTTPStatus.OK)
        try:
            started = int(round(_time.time() * 1000))
            result = self._event_handler._do_without_validation(payload)
            header = hs.add()
            header.key = HEADER_BIZ_RT
            header.value = str(int(round(_time.time() * 1000)) - started)
            if result is not None:
                response.data = base64.b64encode(JSON.marshal(result).encode("utf-8"))
        except Exception:
            response = Response(code=http.HTTPStatus.INTERNAL_SERVER_ERROR)
        frame.payload = JSON.marshal(response).encode("utf-8")
        await self._write_message(frame.SerializeToString())

    import base64
    client_class._handle_data_frame = handle_data_frame


def main() -> None:
    global bridge
    log_event("bridge_starting", cwd=str(ROOT))
    bridge = Bridge()
    log_event("bridge_ready", cwd=str(ROOT))
    import lark_oapi as lark
    patch_lark_card_callback(lark)
    handler = (lark.EventDispatcherHandler.builder("", "")
               .register_p2_im_message_receive_v1(on_message)
               .register_p2_card_action_trigger(on_card_action)
               .build())
    lark.ws.Client(APP_ID, APP_SECRET, event_handler=handler, log_level=lark.LogLevel.INFO).start()


if __name__ == "__main__":
    main()
