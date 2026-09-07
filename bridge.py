#!/usr/bin/env python3
"""A small Termux-friendly Feishu <-> Codex app-server bridge."""
from __future__ import annotations

import json
import html
import hashlib
import hmac
import inspect
import difflib
import mimetypes
import os
import queue
import shlex
import stat
import subprocess
import threading
import time
import uuid
from collections import deque
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Callable

import httpx


ROOT = Path(os.environ.get("CODEX_BRIDGE_CWD", os.getcwd())).resolve()
WORKSPACE_ROOT_RAW = os.environ.get("CODEX_WORKSPACE_ROOT", "")
WORKSPACE_ROOT = Path(WORKSPACE_ROOT_RAW).expanduser().resolve() if WORKSPACE_ROOT_RAW else None
APP_ID = os.environ["FEISHU_APP_ID"]
APP_SECRET = os.environ["FEISHU_APP_SECRET"]
FEISHU_PROXY_URL = os.environ.get("FEISHU_PROXY_URL", "").strip()
CONFIGURED_ALLOWED = {x.strip() for x in os.environ.get("FEISHU_ALLOWED_OPEN_IDS", "").split(",") if x.strip()}
PAIRING_CODE = os.environ.get("FEISHU_PAIRING_CODE", "")
DEFAULT_MODEL = os.environ.get("CODEX_MODEL", "")
MAX_ATTACHMENT = int(os.environ.get("CODEX_MAX_ATTACHMENT_BYTES", str(20 * 1024 * 1024)))
SESSION_FILE = Path(os.environ.get("CODEX_SESSION_FILE", str(ROOT / ".feishu-codex-session")))
SETTINGS_FILE = Path(os.environ.get("CODEX_SETTINGS_FILE", str(ROOT / ".feishu-codex-settings")))
SEEN_MESSAGES_FILE = Path(os.environ.get("CODEX_SEEN_MESSAGES_FILE", str(ROOT / ".feishu-codex-seen-messages")))
ALLOWED_OPEN_IDS_FILE = Path(os.environ.get("CODEX_ALLOWED_OPEN_IDS_FILE", str(ROOT / ".feishu-codex-allowed-open-ids")))
APPROVAL_TIMEOUT = int(os.environ.get("CODEX_APPROVAL_TIMEOUT_SECONDS", "600"))
QUESTION_TIMEOUT = int(os.environ.get("CODEX_QUESTION_TIMEOUT_SECONDS", "600"))
PLAN_ACTION_TIMEOUT = int(os.environ.get("CODEX_PLAN_ACTION_TIMEOUT_SECONDS", "600"))
STREAM_CHUNK = int(os.environ.get("CODEX_STREAM_CHUNK_CHARS", "1200"))
GENERATED_IMAGES = Path(os.environ.get("CODEX_GENERATED_IMAGES", str(Path.home() / ".codex" / "generated_images")))
SANDBOX_MODE = os.environ.get("CODEX_SANDBOX_MODE", "workspaceWrite")
if SANDBOX_MODE not in {"workspaceWrite", "dangerFullAccess"}:
    raise RuntimeError("CODEX_SANDBOX_MODE 必须是 workspaceWrite 或 dangerFullAccess")
SNAPSHOT_MAX_FILES = int(os.environ.get("CODEX_SNAPSHOT_MAX_FILES", "200"))
SNAPSHOT_MAX_TEXT_BYTES = int(os.environ.get("CODEX_SNAPSHOT_MAX_TEXT_BYTES", str(256 * 1024)))
IGNORED_DIRECTORIES = {".git", ".runtime", "feishu-inbox", "__pycache__", ".venv", "venv",
                       "node_modules", ".pytest_cache", ".mypy_cache", ".ruff_cache", ".cache",
                       "target", "build", "dist"}
IGNORED_SUFFIXES = {".pyc", ".pyo", ".o", ".obj", ".class", ".so", ".dll", ".a", ".lib"}
ARTIFACT_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".bmp", ".ico",
                     ".pdf", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".odt", ".ods",
                     ".mp3", ".wav", ".ogg", ".m4a", ".mp4", ".mov", ".webm",
                     ".zip", ".tar", ".gz", ".bz2", ".xz", ".7z"}
TEXT_SUFFIXES = {".py", ".pyi", ".js", ".jsx", ".ts", ".tsx", ".c", ".h", ".cpp", ".hpp",
                 ".rs", ".go", ".java", ".kt", ".swift", ".sh", ".bash", ".css", ".html",
                 ".vue", ".svelte", ".sql", ".rb", ".php", ".cs", ".md", ".txt", ".json",
                 ".jsonl", ".yaml", ".yml", ".toml", ".ini", ".cfg", ".conf", ".xml", ".lock"}
DIFF_MAX_CHARS = int(os.environ.get("CODEX_DIFF_MAX_CHARS", "20000"))


def log_event(event: str, **fields: Any) -> None:
    """Emit parseable diagnostics without chat or command content."""
    print(json.dumps({"event": event, **fields}, ensure_ascii=False, separators=(",", ":")), flush=True)


class Feishu:
    def __init__(self) -> None:
        # Follow the launching environment unless a Feishu override is supplied.
        self.http = httpx.Client(timeout=30, trust_env=not bool(FEISHU_PROXY_URL),
                                 proxy=FEISHU_PROXY_URL or None)
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
                new_group = item.get("group") and elements[-1].get("_group") != item["group"]
                # Keep navigation, independent choices, and control groups visibly
                # separate; adjacent actions for the same entry stay together.
                if (item.get("section") or item.get("separate") or item.get("description")
                        or new_group or len(elements) == 1):
                    elements.append({"tag": "hr"})
                if item.get("section"):
                    elements.append({"tag": "markdown", "content": f"**{item['section']}**"})
                if item.get("description"):
                    # Vertical layout preserves long labels on mobile.
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
                          directory: Path, filename: str = "") -> Path:
        response = self.http.get(
            f"https://open.feishu.cn/open-apis/im/v1/messages/{message_id}/resources/{resource_key}",
            params={"type": resource_type},
            headers={"Authorization": f"Bearer {self.token()}"},
        )
        response.raise_for_status()
        inbox = directory / "feishu-inbox"
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
        self.command = shlex.split(os.environ.get("CODEX_APP_SERVER", "codex app-server --enable collaboration_modes"))
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
        self.last_plan_text = ""
        self.last_turn_error = ""
        self.default_model = ""
        self.model_ids: list[str] = []
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
        try:
            self.refresh_models()
        except Exception as exc:
            log_event("model_list_on_start_failed", error_type=type(exc).__name__)

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
        self.default_model = ""
        self.model_ids = []
        self._spawn()

    def send(self, message: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        with self.write_lock:
            self.process.stdin.write(json.dumps(message, ensure_ascii=False) + "\n")
            self.process.stdin.flush()

    def _initialize(self) -> None:
        self.request("initialize", {"clientInfo": {"name": "feishu-codex-bridge", "version": "0.1.0"},
                                    "capabilities": {"experimentalApi": True}})
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
        if message.get("id") is not None and method == "item/tool/requestUserInput":
            self.event("user_input", message)
        elif message.get("id") is not None and method.endswith("requestApproval"):
            self.event("approval", message)
        elif method == "thread/archived":
            self.event("archived", params)
        elif method.endswith("agentMessage/delta"):
            delta = params.get("delta", params.get("text", ""))
            self.turn_text += delta
            self.event("delta", delta)
        elif method == "turn/completed":
            turn = params.get("turn", {})
            if isinstance(turn, dict):
                self.last_turn_status = str(turn.get("status", ""))
                error = turn.get("error") or {}
                if isinstance(error, dict):
                    self.last_turn_error = str(error.get("message", ""))
                    detail = str(error.get("additionalDetails") or "")
                    if detail:
                        self.last_turn_error += f"\n\n{detail}"
            self.event("completed", params)
        elif method.startswith("item/"):
            item = params.get("item", {})
            if isinstance(item, dict) and item.get("type") == "plan" and item.get("text"):
                self.last_plan_text = str(item["text"])
            self.event("item", params)

    def turn(self, key: str, directory: Path, prompt: str, model: str,
             extra_inputs: list[dict[str, Any]] | None = None, plan_mode: bool | None = None) -> None:
        self.turn_text = ""
        self.last_turn_status = ""
        self.last_plan_text = ""
        self.last_turn_error = ""
        thread_id = self.threads.get(key)
        if not thread_id:
            result = self.request("thread/start", {"cwd": str(directory)})
            thread = result.get("thread", result)
            thread_id = thread["id"]
            self.threads[key] = thread_id
        sandbox_policy = {"type": "dangerFullAccess"} if SANDBOX_MODE == "dangerFullAccess" else {
            "type": "workspaceWrite", "writableRoots": [str(directory)], "networkAccess": False,
        }
        params: dict[str, Any] = {
            "threadId": thread_id,
            "input": [{"type": "text", "text": prompt}] + (extra_inputs or []),
            # Keep ordinary work inside the selected directory.  Codex may
            # request approval to leave this sandbox, which is rendered by
            # Bridge as the existing Feishu approval card.
            "approvalPolicy": "on-request",
            "sandboxPolicy": sandbox_policy,
        }
        if model:
            params["model"] = model
        if plan_mode is not None:
            collaboration_model = model or self.default_model
            if not collaboration_model:
                self.refresh_models()
                collaboration_model = self.default_model
            if not collaboration_model:
                raise RuntimeError("无法从 Codex 获取可用默认模型，不能切换 Plan 模式")
            params["collaborationMode"] = {
                "mode": "plan" if plan_mode else "default",
                "settings": {"model": collaboration_model},
            }
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

    def resume(self, key: str, thread_id: str, directory: Path) -> None:
        self.request("thread/resume", {"threadId": thread_id, "cwd": str(directory)})
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

    def list_threads(self, directory: Path, archived: bool = False) -> list[dict[str, Any]]:
        result = self.request("thread/list", {"cwd": [str(directory)], "limit": 20,
                                                "sortKey": "updated_at", "sortDirection": "desc", "archived": archived})
        return result.get("data", [])

    def archive_thread(self, thread_id: str, archived: bool = True) -> None:
        self.request("thread/archive" if archived else "thread/unarchive", {"threadId": thread_id})

    def read_thread(self, thread_id: str) -> dict[str, Any]:
        return self.request("thread/read", {"threadId": thread_id, "includeTurns": False})["thread"]

    def refresh_models(self) -> list[str]:
        result = self.request("model/list", {})
        values = result.get("data", result.get("models", []))
        self.model_ids = [item.get("id", str(item)) for item in values]
        self.default_model = next((str(item.get("id", "")) for item in values
                                   if isinstance(item, dict) and item.get("isDefault")), "")
        return self.model_ids

    def models(self) -> list[str]:
        return self.refresh_models()

    def approve(self, request_id: int, yes: bool) -> None:
        if request_id not in self.approvals:
            raise KeyError(request_id)
        self.send({"jsonrpc": "2.0", "id": request_id, "result": {
            "decision": "accept" if yes else "decline"
        }})
        del self.approvals[request_id]
        self.approval_created.pop(request_id, None)

    def answer_user_input(self, request_id: int | str, answers: dict[str, list[str]]) -> None:
        self.send({"jsonrpc": "2.0", "id": request_id, "result": {
            "answers": {question_id: {"answers": values}
                        for question_id, values in answers.items()}
        }})


class Bridge:
    def __init__(self) -> None:
        self.started_at = time.monotonic()
        self.feishu = Feishu()
        self.server = CodexServer(self.codex_event)
        self.models, self.directories, self.plan_modes = self.load_settings()
        self.allowed_lock = threading.Lock()
        self.allowed_open_ids = CONFIGURED_ALLOWED | self.load_allowed_open_ids()
        self.current_chat: dict[str, str] = {}
        self.stream_buffers: dict[str, str] = {}
        self.active_cards: dict[str, str] = {}
        self.active_task_tokens: dict[str, str] = {}
        self.stopping_tasks: set[str] = set()
        self.task_lock = threading.Lock()
        self.thread_cards: dict[str, dict[str, Any]] = {}
        self.thread_lock = threading.RLock()
        self.thread_mutation = False
        self.help_cards: dict[str, tuple[str, str]] = {}
        self.approval_cards: dict[int, str] = {}
        self.approval_summaries: dict[int, str] = {}
        self.approval_items: dict[str, dict[str, Any]] = {}
        self.approval_lock = threading.Lock()
        self.card_updated_at: dict[str, float] = {}
        self.progress_lock = threading.Lock()
        self.progress: dict[str, Any] = {}
        self.jobs: queue.Queue[tuple[str, str, Path, str, str, dict[str, str] | None, int]] = queue.Queue()
        self.user_job_counts: dict[str, int] = {}
        self.pending_directories: dict[str, tuple[str, str, Path, float]] = {}
        self.pending_questions: dict[int | str, dict[str, Any]] = {}
        self.question_lock = threading.RLock()
        self.plan_actions: dict[str, dict[str, Any]] = {}
        self.pending_default_modes: set[str] = set()
        self.generations: dict[str, int] = {}
        self.seen_messages: deque[str] = deque(maxlen=1000)
        self.seen_message_set: set[str] = set()
        self.seen_lock = threading.Lock()
        self.load_seen_messages()
        threading.Thread(target=self.worker, daemon=True).start()
        threading.Thread(target=self.approval_reaper, daemon=True).start()
        threading.Thread(target=self.question_reaper, daemon=True).start()
        threading.Thread(target=self.plan_action_reaper, daemon=True).start()
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
                content = self.progress_content(elapsed, preview)
                try:
                    self.feishu.update_card(state["card"], "Codex 处理中", content, "blue", [
                        {"text": "停止任务", "type": "danger", "value": {"command": "/stop", "task_id": state["task_id"]}}
                    ])
                except Exception as exc:
                    log_event("progress_update_failed", error_type=type(exc).__name__)

    def session_key(self, user_id: str, directory: Path | None = None) -> str:
        return f"{user_id}:{directory or self.current_directory(user_id)}"

    @staticmethod
    def directory_is_allowed(directory: Path) -> bool:
        return WORKSPACE_ROOT is not None and directory.is_relative_to(WORKSPACE_ROOT)

    def current_directory(self, user_id: str) -> Path:
        stored = getattr(self, "directories", {}).get(user_id, str(ROOT))
        try:
            directory = Path(stored).expanduser().resolve()
        except OSError:
            directory = ROOT
        if not directory.is_dir() or not self.directory_is_allowed(directory):
            getattr(self, "directories", {}).pop(user_id, None)
            return ROOT
        return directory

    def resolve_directory(self, current: Path, value: str) -> Path:
        candidate = Path(value).expanduser()
        if not candidate.is_absolute():
            candidate = current / candidate
        candidate = candidate.resolve()
        if not self.directory_is_allowed(candidate):
            raise ValueError("目录必须位于已配置的工作区根目录内")
        return candidate

    def set_directory(self, user_id: str, directory: Path) -> None:
        self.directories[user_id] = str(directory)
        self.save_model_settings()

    def directory_buttons(self, directory: Path) -> list[dict[str, Any]]:
        buttons: list[dict[str, Any]] = []
        if WORKSPACE_ROOT is not None and directory != WORKSPACE_ROOT:
            buttons.append({"text": "进入上级目录", "description": f"`{directory.parent}`",
                            "value": {"command": "/cd", "path": str(directory.parent)}})
        try:
            children = sorted((path for path in directory.iterdir()
                               if path.is_dir() and self.directory_is_allowed(path.resolve())),
                              key=lambda path: path.name.lower())
        except OSError:
            children = []
        for child in children[:12]:
            buttons.append({"text": "进入目录", "description": f"**{child.name}**\n`{child}`",
                            "value": {"command": "/cd", "path": str(child)}})
        return buttons

    def load_allowed_open_ids(self) -> set[str]:
        """Load self-paired users without ever logging their identifiers."""
        try:
            parsed = json.loads(ALLOWED_OPEN_IDS_FILE.read_text(encoding="utf-8"))
            if isinstance(parsed, list):
                return {str(value) for value in parsed if str(value)}
        except (FileNotFoundError, OSError, json.JSONDecodeError):
            pass
        return set()

    def is_allowed(self, user_id: str) -> bool:
        # Preserve the previous open-by-default behavior only when neither
        # static IDs nor the opt-in pairing mechanism has been configured.
        if not CONFIGURED_ALLOWED and not PAIRING_CODE:
            return True
        with self.allowed_lock:
            return user_id in self.allowed_open_ids

    def pair_user(self, user_id: str, code: str) -> bool:
        """Persist a user admitted with the administrator's pairing secret."""
        if not PAIRING_CODE or not hmac.compare_digest(code, PAIRING_CODE):
            return False
        with self.allowed_lock:
            if user_id in self.allowed_open_ids:
                return True
            updated = sorted(self.allowed_open_ids | {user_id})
            temporary = ALLOWED_OPEN_IDS_FILE.with_name(ALLOWED_OPEN_IDS_FILE.name + ".tmp")
            temporary.write_text(json.dumps(updated, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            temporary.chmod(0o600)
            os.replace(temporary, ALLOWED_OPEN_IDS_FILE)
            self.allowed_open_ids = set(updated)
        return True

    def load_settings(self) -> tuple[dict[str, str], dict[str, str], dict[str, bool]]:
        try:
            parsed = json.loads(SETTINGS_FILE.read_text(encoding="utf-8"))
            models = {str(key): str(value) for key, value in parsed.get("models", {}).items()}
            directories = {str(key): str(value) for key, value in parsed.get("directories", {}).items()}
            plan_modes = {str(key): bool(value) for key, value in parsed.get("plan_modes", {}).items() if value}
            return models, directories, plan_modes
        except (FileNotFoundError, OSError, json.JSONDecodeError, AttributeError):
            return {}, {}, {}

    def load_model_settings(self) -> dict[str, str]:
        """Compatibility helper retained for tests and callers."""
        return self.load_settings()[0]

    def save_model_settings(self) -> None:
        temporary = SETTINGS_FILE.with_name(SETTINGS_FILE.name + ".tmp")
        temporary.write_text(json.dumps({"models": self.models,
                                         "directories": getattr(self, "directories", {}),
                                         "plan_modes": getattr(self, "plan_modes", {})},
                                        ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
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
        parts = [f"**操作类型：{category}**", self.timeout_notice(APPROVAL_TIMEOUT, "超时后将自动拒绝")]
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
        if kind == "archived":
            with self.thread_lock, self.task_lock:
                self.clear_thread_bindings(str(value.get("threadId", "")))
        elif kind == "item":
            item = value.get("item", {})
            if item.get("id"):
                self.approval_items[item["id"]] = item
        elif kind == "user_input":
            self.begin_user_input(value)
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

    def question_reaper(self) -> None:
        while True:
            time.sleep(5)
            now = time.time()
            expired: list[tuple[int | str, dict[str, Any]]] = []
            with self.question_lock:
                for request_id, state in list(self.pending_questions.items()):
                    if now - state["created"] >= QUESTION_TIMEOUT:
                        expired.append((request_id, self.pending_questions.pop(request_id)))
            for request_id, state in expired:
                try:
                    self.server.answer_user_input(request_id, state["answers"])
                    if state.get("card"):
                        self.feishu.update_card(state["card"], "问题已超时", "未在规定时间内回答，已将空答案返回给 Codex。", "grey")
                    else:
                        self.feishu.card_or_text(state["chat_id"], "问题已超时", "未在规定时间内回答，已将空答案返回给 Codex。", "grey")
                except Exception as exc:
                    log_event("question_timeout_failed", error_type=type(exc).__name__)

    def plan_action_reaper(self) -> None:
        while True:
            time.sleep(5)
            now = time.time()
            expired = []
            for action_id, action in list(self.plan_actions.items()):
                if now - action["created"] >= PLAN_ACTION_TIMEOUT:
                    removed = self.plan_actions.pop(action_id, None)
                    if removed:
                        expired.append((action_id, removed))
            for _action_id, action in expired:
                try:
                    card = action.get("card", "")
                    if card:
                        self.feishu.update_card(card, "计划操作已超时",
                                                "未在规定时间内选择后续操作；计划仍可继续讨论。", "grey")
                    else:
                        self.feishu.card_or_text(action["chat_id"], "计划操作已超时",
                                                 "未在规定时间内选择后续操作；计划仍可继续讨论。", "grey")
                except Exception as exc:
                    log_event("plan_action_timeout_failed", error_type=type(exc).__name__)

    def show_next_question(self, request_id: int | str) -> None:
        with self.question_lock:
            state = self.pending_questions.get(request_id)
            if not state:
                return
            index = state["index"]
            question = state["questions"][index]
            options = question.get("options") or []
            buttons = [{
                "text": str(option.get("label", "选择")),
                "description": str(option.get("description", "")),
                "separate": True,
                "type": "primary" if position == 0 else "default",
                "value": {"command": "/question-answer", "request_id": request_id,
                          "question_id": question["id"], "answer": str(option.get("label", ""))},
            } for position, option in enumerate(options)]
            if question.get("isOther"):
                buttons.append({"text": "其他（文字输入）", "section": "自行回答",
                                "description": "以上选项都不合适时，点击后发送你的回答。", "value": {
                    "command": "/question-other", "request_id": request_id,
                    "question_id": question["id"]}})
            content = (f"**{question.get('header', '需要你的选择')}**\n\n{question.get('question', '')}\n\n"
                       f"第 {index + 1}/{len(state['questions'])} 题\n\n"
                       + self.timeout_notice(max(0, QUESTION_TIMEOUT - int(time.time() - state["created"])),
                                             "超时后将返回空答案"))
            state["card"] = self.feishu.card_or_text(state["chat_id"], "Codex 需要你的选择", content, "yellow", buttons)

    def begin_user_input(self, request: dict[str, Any]) -> None:
        request_id = request["id"]
        params = request.get("params", {})
        questions = [question for question in params.get("questions", []) if question.get("id")]
        chat_id = self.current_chat.get("active", "")
        key = self.current_chat.get("key", "")
        if not chat_id or not key or not questions:
            self.server.answer_user_input(request_id, {str(question.get("id", "")): [] for question in questions})
            return
        with self.question_lock:
            self.pending_questions[request_id] = {
                "chat_id": chat_id, "key": key, "questions": questions, "index": 0,
                "answers": {str(question["id"]): [] for question in questions},
                "created": time.time(), "card": "", "other": False,
            }
        self.show_next_question(request_id)

    def question_action(self, user_id: str, chat_id: str, key: str, value: dict[str, Any], source: str) -> None:
        request_id = value.get("request_id")
        question_id = str(value.get("question_id", ""))
        with self.question_lock:
            state = self.pending_questions.get(request_id)
            if not state or state["chat_id"] != chat_id or state["key"] != key:
                self.feishu.card_or_text(chat_id, "问题已失效", "该问题已回答、超时或不属于当前会话。", "grey")
                return
            if state.get("card") and source and state["card"] != source:
                self.feishu.card_or_text(chat_id, "问题已失效", "这张问题卡已被新的问题替代。", "grey")
                return
            question = state["questions"][state["index"]]
            if question["id"] != question_id:
                self.feishu.card_or_text(chat_id, "问题已失效", "该选项不属于当前问题。", "grey")
                return
            if value.get("command") == "/question-other":
                state["other"] = True
                self.feishu.card_or_text(chat_id, "请输入其他回答", "请直接发送你的自定义回答；它只会用于当前问题。", "yellow")
                return
            self._record_question_answer(request_id, state, [str(value.get("answer", ""))])

    def answer_question_text(self, user_id: str, chat_id: str, key: str, text: str) -> bool:
        with self.question_lock:
            for request_id, state in self.pending_questions.items():
                if state["chat_id"] == chat_id and state["key"] == key and state.get("other"):
                    state["other"] = False
                    self._record_question_answer(request_id, state, [text])
                    return True
        return False

    def cancel_questions(self, key: str) -> None:
        with self.question_lock:
            cancelled = [(request_id, self.pending_questions.pop(request_id))
                         for request_id, state in self.pending_questions.items() if state["key"] == key]
        for request_id, state in cancelled:
            try:
                self.server.answer_user_input(request_id, state["answers"])
            except Exception as exc:
                log_event("question_cancel_failed", error_type=type(exc).__name__)

    def _record_question_answer(self, request_id: int | str, state: dict[str, Any], answers: list[str]) -> None:
        question = state["questions"][state["index"]]
        state["answers"][str(question["id"])] = answers
        card = state.get("card", "")
        if card:
            try:
                self.feishu.update_card(card, "已选择", f"**{question.get('header', '问题')}**\n\n已回答：{', '.join(answers) or '（空）'}", "green")
            except Exception as exc:
                log_event("question_card_update_failed", error_type=type(exc).__name__)
        state["index"] += 1
        if state["index"] < len(state["questions"]):
            self.show_next_question(request_id)
            return
        self.pending_questions.pop(request_id, None)
        self.server.answer_user_input(request_id, state["answers"])

    def plan_buttons(self, user_id: str, key: str, chat_id: str, plan_text: str,
                     card_id: str = "") -> list[dict[str, Any]]:
        action_id = uuid.uuid4().hex
        self.plan_actions[action_id] = {"user_id": user_id, "key": key, "chat_id": chat_id,
                                        "plan": plan_text, "card": card_id,
                                        "created": time.time()}
        return [
            {"text": "是，实现此计划", "description": "切换到默认模式并开始编码。",
             "type": "primary", "value": {"command": "/plan-implement", "action_id": action_id}},
            {"text": "是，清空上下文后实现", "description": "新建会话，仅带上这份计划后开始编码。",
             "value": {"command": "/plan-clear-implement", "action_id": action_id}},
            {"text": "否，留在 Plan 模式", "description": "保持当前上下文，继续和 Codex 讨论或修改计划。",
             "value": {"command": "/plan-stay", "action_id": action_id}},
        ]

    def help_buttons(self, key: str) -> list[dict[str, Any]]:
        plan_enabled = self.plan_modes.get(key, False)
        plan_button = {
            "text": "关闭 Plan" if plan_enabled else "开启 Plan", "group": "模式",
            "type": "danger" if plan_enabled else "primary",
            "value": {"command": "/plan-toggle", "enabled": not plan_enabled},
        }
        return [
            {"text": f"{label} {cmd}", "group": group,
             "type": "danger" if cmd == "/stop" else "default", "value": {"command": cmd}}
            for group, label, cmd in [("会话", "恢复", "/resume"), ("会话", "新建", "/new"),
                                      ("模型", "当前", "/model"), ("模型", "列表", "/models")]
        ] + [plan_button] + [
            {"text": f"{label} {cmd}", "group": group,
             "section": "停止任务" if cmd == "/stop" else "",
             "type": "danger" if cmd == "/stop" else "default", "value": {"command": cmd}}
            for group, label, cmd in [("目录", "切换", "/cd"),
                                      ("任务", "状态", "/status"), ("任务", "压缩", "/compact"),
                                      ("", "停止", "/stop")]
        ]

    def update_help_card(self, card_id: str, key: str) -> None:
        if not card_id:
            return
        self.feishu.update_card(card_id, "Codex 控制面板", "点击执行操作，也支持输入 /命令。", "blue",
                                self.help_buttons(key))

    def resolve_plan_action(self, user_id: str, chat_id: str, key: str, command: str,
                            action_id: str, source: str = "") -> None:
        action = self.plan_actions.pop(action_id, None)
        if not action or action["user_id"] != user_id or action["chat_id"] != chat_id or action["key"] != key:
            raise ValueError("该计划操作已失效或不属于当前会话")
        if time.time() - action["created"] >= PLAN_ACTION_TIMEOUT:
            raise ValueError("该计划操作已超时，请继续讨论后重新生成计划")
        if action.get("card") and source and action["card"] != source:
            raise ValueError("这张计划卡已失效")
        if command == "/plan-stay":
            self.plan_modes[key] = True
            self.save_model_settings()
            self.feishu.card_or_text(chat_id, "继续 Plan 模式", "可继续发送消息讨论或修改计划。", "blue")
            return
        self.plan_modes.pop(key, None)
        self.pending_default_modes.add(key)
        self.save_model_settings()
        prompt = "请按照刚才已确认的计划开始实施。"
        if command == "/plan-clear-implement":
            self.server.threads.pop(key, None)
            self.clear_session(key)
            prompt = "请实施以下已确认的计划：\n\n" + action["plan"]
        directory = self.current_directory(user_id)
        with self.task_lock:
            self.user_job_counts[user_id] = self.user_job_counts.get(user_id, 0) + 1
        self.jobs.put((user_id, key, directory, chat_id, prompt, None, self.generations.get(key, 0)))
        self.feishu.card_or_text(chat_id, "开始实施计划", "已切换到默认模式并加入执行队列。", "green")

    def worker(self) -> None:
        while True:
            user_id, key, directory, chat_id, prompt, resource, generation = self.jobs.get()
            if generation != self.generations.get(key, 0):
                self.feishu.card_or_text(chat_id, "任务已取消", "任务仍在等待队列中，已取消执行。", "red")
                with self.task_lock:
                    self.user_job_counts[user_id] = max(0, self.user_job_counts.get(user_id, 1) - 1)
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
                plan_enabled = self.plan_modes.get(key, False)
                plan_mode: bool | None = True if plan_enabled else (False if key in self.pending_default_modes else None)
                before = self.snapshot(directory)
                started_at = time.time()
                card_id = self.feishu.card_or_text(chat_id, "Codex 开始处理", f"目录：`{directory}`\n\n正在准备执行…", "blue", [
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
                        resource["resource_type"], directory, resource.get("filename", ""),
                    )
                    if resource["resource_type"] == "image":
                        extra_inputs.append({"type": "localImage", "path": str(local_path), "detail": "auto"})
                        prompt += "\n\n用户附加了一张图片，请直接分析图片内容。"
                    else:
                        prompt += f"\n\n用户附加了一个文件，请读取它：{local_path}。"
                stored = self.load_session(key)
                if stored and key not in self.server.threads:
                    self.server.resume(key, stored, directory)
                self.server.turn(key, directory, prompt, self.models.get(key, DEFAULT_MODEL), extra_inputs, plan_mode)
                self.pending_default_modes.discard(key)
                self.save_session(self.server.threads[key], key)
                with self.progress_lock:
                    self.progress = {}
                self.stream_buffers.pop(chat_id, "")
                elapsed = int(time.time() - started_at)
                if self.server.last_turn_status == "interrupted":
                    self.finish_card(chat_id, "任务已停止", "Codex turn 已停止。", "red")
                else:
                    status = self.server.last_turn_status
                    title = "Codex 执行失败" if status == "failed" else ("计划已生成" if plan_enabled else "Codex 已完成")
                    result_text = self.server.turn_text or self.server.last_plan_text
                    if status == "failed":
                        result_text = self.server.last_turn_error or result_text or "Codex 未提供失败详情。"
                    if plan_enabled and status != "failed":
                        self.finish_plan_turn(user_id, key, chat_id, elapsed)
                    else:
                        content = f"耗时 {elapsed} 秒\n\n" + (result_text or "没有返回文字。")
                        self.finish_card(chat_id, title, content, "red" if status == "failed" else "green")
                after = self.snapshot(directory)
                self.send_file_diffs(chat_id, directory, before, after)
                paths = list(dict.fromkeys(self.changed_files(directory, before, after)
                                           + self.generated_files(key, started_at)))[:10]
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
                    self.user_job_counts[user_id] = max(0, self.user_job_counts.get(user_id, 1) - 1)
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
                if current and current != (f"```{fence}\n" if fence else ""):
                    flush()
                    continue
                # Split an oversized line even when a reopened fence is present.
                current += remaining[:capacity]
                remaining = remaining[capacity:]
                flush()
        if current:
            if fence:
                current = current.rstrip() + "\n```\n"
            chunks.append(current)
        return chunks or ["（无内容）"]

    @staticmethod
    def progress_content(elapsed: int, text: str) -> str:
        """Render only the most recent process preview in the mutable card."""
        return f"已耗时 {elapsed} 秒\n\n{text[-5000:]}"

    @staticmethod
    def timeout_notice(seconds: int, consequence: str) -> str:
        minutes, remainder = divmod(max(0, seconds), 60)
        remaining = f"{minutes} 分钟" if remainder == 0 else f"{minutes} 分 {remainder} 秒"
        return f"<font color='orange'>请在 {remaining} 内操作；{consequence}。</font>"

    def finish_plan_turn(self, user_id: str, key: str, chat_id: str, elapsed: int) -> None:
        """Send the complete plan before its time-sensitive action card."""
        plan_text = self.server.last_plan_text or self.server.turn_text
        self.finish_card(chat_id, "计划已生成", f"耗时 {elapsed} 秒\n\n计划详情和下一步操作将分别发送。", "green")
        detail = plan_text or "（未收到结构化计划 item，且没有可展示的最终文本。）"
        if not self.server.last_plan_text:
            detail = "**未收到结构化计划 item；以下为最终文本降级展示。**\n\n" + detail
        self.send_split_cards(chat_id, "计划详情", detail, "blue")
        buttons = self.plan_buttons(user_id, key, chat_id, plan_text)
        action_card = self.feishu.card_or_text(
            chat_id, "计划下一步", "计划已就绪。请选择后续操作。\n\n"
            + self.timeout_notice(PLAN_ACTION_TIMEOUT, "超时后此操作卡将失效"), "green", buttons)
        action_id = buttons[0]["value"]["action_id"]
        self.plan_actions[action_id]["card"] = action_card

    def finish_card(self, chat_id: str, title: str, content: str, color: str,
                    buttons: list[dict[str, Any]] | None = None) -> None:
        with self.progress_lock:
            self.progress = {}
        card_id = self.active_cards.get(chat_id, "")
        chunks = self.split_card_content(content)
        if card_id:
            try:
                # Keep the primary card within Feishu's practical card size;
                # continuation cards preserve the complete long response.
                self.feishu.update_card(card_id, title, chunks[0], color, buttons)
                chunks = chunks[1:]
            except Exception as exc:
                log_event("card_update_failed", title=title, error_type=type(exc).__name__)
        for index, chunk in enumerate(chunks, 2):
            self.feishu.card_or_text(chat_id, f"{title}（续 {index}）", chunk, color, buttons if index == 2 and not card_id else None)

    def send_split_cards(self, chat_id: str, title: str, content: str, color: str = "blue") -> None:
        for index, chunk in enumerate(self.split_card_content(content), 1):
            suffix = "" if index == 1 else f"（续 {index}）"
            self.feishu.card_or_text(chat_id, title + suffix, chunk, color)

    @staticmethod
    def file_kind(path: Path) -> str:
        if (any(part in IGNORED_DIRECTORIES for part in path.parts)
                or path.suffix.lower() in IGNORED_SUFFIXES
                or path.name.startswith(".feishu-codex") or path.name == ".env"
                or (path.name.startswith(".env.") and path.name != ".env.example")):
            return "ignore"
        return "artifact" if path.suffix.lower() in ARTIFACT_SUFFIXES else "text"

    @staticmethod
    def _snapshot_text(path: Path) -> tuple[str | None, str]:
        try:
            with path.open("rb") as stream:
                data = stream.read(SNAPSHOT_MAX_TEXT_BYTES + 1)
        except OSError:
            return None, "无法读取"
        if len(data) > SNAPSHOT_MAX_TEXT_BYTES:
            return None, "文件过大"
        if b"\0" in data:
            return None, "二进制文件"
        try:
            return data.decode("utf-8"), ""
        except UnicodeDecodeError:
            return None, "非 UTF-8 文本"

    def snapshot(self, directory: Path) -> dict[str, dict[str, Any]]:
        result: dict[str, dict[str, Any]] = {}
        for root, directories, files in os.walk(directory, followlinks=False):
            directories[:] = sorted(name for name in directories
                                    if name not in IGNORED_DIRECTORIES
                                    and not (Path(root) / name).is_symlink())
            for name in sorted(files):
                path = Path(root) / name
                kind = self.file_kind(path.relative_to(directory))
                if kind == "ignore" or path.is_symlink():
                    continue
                if len(result) >= SNAPSHOT_MAX_FILES:
                    return result
                try:
                    file_stat = path.stat()
                    if not stat.S_ISREG(file_stat.st_mode):
                        continue
                    text, skipped = self._snapshot_text(path) if kind == "text" else (None, "")
                    if kind == "text" and text is None and path.suffix.lower() not in TEXT_SUFFIXES:
                        kind = "unknown"
                    result[str(path)] = {"metadata": (file_stat.st_mtime_ns, file_stat.st_size),
                                         "text": text, "skipped": skipped, "kind": kind}
                except OSError:
                    pass
        return result

    @staticmethod
    def file_changed(old: dict[str, Any] | None, new: dict[str, Any] | None) -> bool:
        if old is None or new is None:
            return True
        if old["text"] is not None and new["text"] is not None:
            return old["text"] != new["text"]
        return old["metadata"] != new["metadata"]

    def changed_files(self, directory: Path, before: dict[str, dict[str, Any]],
                      after: dict[str, dict[str, Any]] | None = None) -> list[Path]:
        after = self.snapshot(directory) if after is None else after
        return [Path(name) for name, state in after.items()
                if state.get("kind") == "artifact" and self.file_changed(before.get(name), state)][:10]

    def file_diffs(self, directory: Path, before: dict[str, dict[str, Any]],
                   after: dict[str, dict[str, Any]] | None = None) -> list[tuple[Path, str]]:
        after = self.snapshot(directory) if after is None else after
        changes: list[tuple[Path, str]] = []
        for name in sorted(set(before) | set(after)):
            old, new = before.get(name), after.get(name)
            if not self.file_changed(old, new):
                continue
            path = Path(name)
            if new is None and path.exists():
                continue
            relative = path.relative_to(directory)
            if self.file_kind(relative) in {"ignore", "artifact"}:
                continue
            operation = "新增" if old is None else ("删除" if new is None else "修改")
            if (old and old["text"] is None) or (new and new["text"] is None):
                skipped = next(state.get("skipped") or "无法读取" for state in (old, new)
                               if state and state["text"] is None)
                changes.append((path, f"{operation} · 无法生成文本差异：{skipped}。未上传原文件。"))
                continue
            old_text = old["text"] if old else ""
            new_text = new["text"] if new else ""
            # Normalize missing final newlines for display so +/- lines never run together.
            old_lines, new_lines = old_text.splitlines(), new_text.splitlines()
            diff_lines = list(difflib.unified_diff(old_lines, new_lines,
                             fromfile=f"a/{relative}", tofile=f"b/{relative}", lineterm=""))
            added = sum(line.startswith("+") for line in diff_lines[2:])
            removed = sum(line.startswith("-") for line in diff_lines[2:])
            diff = "\n".join(diff_lines)
            if len(diff) > DIFF_MAX_CHARS:
                diff = diff[:DIFF_MAX_CHARS] + "\n…（差异已截断）"
            note = ""
            if old_text.endswith("\n") != new_text.endswith("\n"):
                note = "\n文件末尾换行状态发生变化。"
            changes.append((path, f"{operation} · +{added} / -{removed}{note}\n\n```diff\n"
                            + (diff or "（空文件或文件末尾换行变化）") + "\n```"))
        return changes

    def send_file_diffs(self, chat_id: str, directory: Path, before: dict[str, dict[str, Any]],
                        after: dict[str, dict[str, Any]] | None = None) -> None:
        for path, content in self.file_diffs(directory, before, after):
            relative = path.relative_to(directory)
            self.send_split_cards(chat_id, f"文件差异：{relative}", content, "blue")

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
        content = json.loads(message.content or "{}")
        text = content.get("text", "").strip()
        if not self.is_allowed(user_id):
            parts = text.split(maxsplit=1)
            if parts and parts[0].lower() == "/pair" and self.pair_user(user_id, parts[1].strip() if len(parts) == 2 else ""):
                log_event("user_paired", user=user_tag)
                self.feishu.card_or_text(message.chat_id, "配对成功", "此账号已加入本机白名单。现在可以发送 `/help`。", "green")
                return
            log_event("message_ignored", reason="unauthorized", user=user_tag)
            return
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
        directory = self.current_directory(user_id)
        key = self.session_key(user_id, directory)
        if text.startswith("/"):
            # Commands such as /resume or /models may take long enough to
            # starve the Feishu WebSocket heartbeat. Run them off the callback.
            threading.Thread(target=self.command, args=(user_id, message.chat_id, key, text), daemon=True).start()
        else:
            if self.answer_question_text(user_id, message.chat_id, key, text):
                return
            generation = self.generations.get(key, 0)
            with self.task_lock:
                changing = getattr(self, "thread_mutation", False)
                if not changing:
                    self.user_job_counts[user_id] = self.user_job_counts.get(user_id, 0) + 1
            if changing:
                self.feishu.card_or_text(message.chat_id, "对话操作中", "请等待归档或恢复完成后重新发送消息。", "yellow")
                return
            self.jobs.put((user_id, key, directory, message.chat_id, text, resource, generation))

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

    @staticmethod
    def thread_title(thread: dict[str, Any]) -> str:
        title = next((" ".join(str(thread.get(field) or "").split())
                      for field in ("name", "title", "preview") if str(thread.get(field) or "").strip()), "未命名")[:60]
        title = html.escape(title)
        for char in "\\`*_[]":
            title = title.replace(char, "\\" + char)
        return title

    def show_threads(self, user_id: str, chat_id: str, key: str, directory: Path,
                     archived: bool = False, source: str = "") -> None:
        with self.thread_lock:
            threads = self.server.list_threads(directory, archived=archived)[:8]
            current = self.server.threads.get(key) or self.load_session(key)
            buttons = []
            token = uuid.uuid4().hex
            allowed = {}
            for thread in threads:
                thread_id = thread.get("id")
                if not thread_id:
                    continue
                allowed[thread_id] = thread
                description = f"**{self.thread_title(thread)}**" + (" · 当前对话" if thread_id == current else "")
                description += f"\nID：`{thread_id}`"
                operations = [("取消归档", "unarchive")] if archived else [("恢复", "resume"), ("归档", "archive")]
                for index, (label, operation) in enumerate(operations):
                    buttons.append({"text": label, "type": "default" if operation == "archive" else "primary",
                                    "description": description if index == 0 else "",
                                    "value": {"command": "/thread-action", "token": token,
                                              "thread_id": thread_id, "operation": operation}})
            buttons.append({"text": "返回普通对话列表" if archived else "查看已归档对话",
                            "section": "列表导航", "value": {
                "command": "/thread-action", "token": token,
                "operation": "list", "thread_id": ""}})
            title = "已归档对话" if archived else "对话列表"
            content = ("归档会隐藏对话并保留历史记录。" if threads else "没有找到对话。")
            content += "\n\n操作卡 10 分钟后失效，届时请重新发送 /resume 或 /archived。"
            if source:
                try:
                    self.feishu.update_card(source, title, content, "blue", buttons)
                    card_id = source
                except Exception:
                    card_id = self.feishu.card_or_text(chat_id, title, content, "blue", buttons)
            else:
                card_id = self.feishu.card_or_text(chat_id, title, content, "blue", buttons)
            for old_token, state in list(self.thread_cards.items()):
                if state["card"] == card_id or time.time() - state["created"] > 600:
                    self.thread_cards.pop(old_token, None)
            self.thread_cards[token] = {"user": user_id, "chat": chat_id, "key": key,
                                        "directory": directory, "archived": archived, "threads": allowed,
                                        "card": card_id, "created": time.time()}
            while len(self.thread_cards) > 100:
                self.thread_cards.pop(next(iter(self.thread_cards)))

    def clear_thread_bindings(self, thread_id: str) -> None:
        keys = {key for key, value in self.server.threads.items() if value == thread_id}
        try:
            raw = SESSION_FILE.read_text(encoding="utf-8").strip()
            try:
                saved = json.loads(raw)
            except json.JSONDecodeError:
                saved = raw
            if isinstance(saved, dict):
                keys.update(key for key, value in saved.items() if value == thread_id)
            elif saved == thread_id:
                self.clear_session()
        except FileNotFoundError:
            pass
        for key in keys:
            self.server.threads.pop(key, None)
            self.clear_session(key)
        for action_id, action in list(self.plan_actions.items()):
            if action.get("key") in keys:
                self.plan_actions.pop(action_id, None)
        for token, state in list(self.thread_cards.items()):
            if thread_id in state["threads"]:
                self.thread_cards.pop(token, None)

    def change_thread(self, user_id: str, chat_id: str, key: str, directory: Path,
                      thread_id: str, operation: str) -> None:
        # Reserve admission under the short-lived lock; RPC must not block the
        # Feishu receive callback on this lock while waiting for app-server.
        with self.task_lock:
            if (getattr(self, "thread_mutation", False) or any(self.user_job_counts.values())
                    or self.active_task_tokens):
                raise ValueError("有任务执行中、正在排队或正在切换对话，请等待完成或先停止任务。")
            self.thread_mutation = True
        try:
            thread = self.server.read_thread(thread_id)
            if (thread.get("status") or {}).get("type") == "active":
                raise ValueError("该对话正在执行任务，请先停止或等待完成。")
            if not thread.get("cwd") or Path(thread["cwd"]).resolve() != directory.resolve():
                raise ValueError("该对话不属于当前目录")
            if operation == "resume":
                self.server.resume(key, thread_id, directory)
                self.save_session(thread_id, key)
                title = "会话已恢复"
            else:
                self.server.archive_thread(thread_id, archived=operation == "archive")
                if operation == "archive":
                    self.clear_thread_bindings(thread_id)
                title = "对话已归档" if operation == "archive" else "已取消归档"
        finally:
            with self.task_lock:
                self.thread_mutation = False
        self.feishu.card_or_text(chat_id, title,
                                 f"**{self.thread_title(thread)}**\nID：`{thread_id}`", "green")

    def thread_action(self, user_id: str, chat_id: str, key: str,
                      value: dict[str, Any], source: str) -> None:
        try:
            with self.thread_lock:
                state = self.thread_cards.get(str(value.get("token", "")))
                if (not state or (state["user"], state["chat"], state["key"], state["card"])
                        != (user_id, chat_id, key, source) or time.time() - state["created"] > 600
                        or state["directory"] != self.current_directory(user_id)):
                    raise ValueError("对话卡片已失效或不属于当前用户、聊天及目录，请重新发送 /resume。")
                operation = value.get("operation")
                if operation == "list":
                    self.show_threads(user_id, chat_id, key, state["directory"], not state["archived"], source)
                    return
                thread_id = str(value.get("thread_id", ""))
                expected = {"unarchive"} if state["archived"] else {"resume", "archive"}
                if thread_id not in state["threads"] or operation not in expected:
                    raise ValueError("无效的对话操作")
                self.change_thread(user_id, chat_id, key, state["directory"], thread_id, operation)
                self.thread_cards.pop(str(value.get("token", "")), None)
                self.show_threads(user_id, chat_id, key, state["directory"], state["archived"], source)
        except Exception as exc:
            self.feishu.card_or_text(chat_id, "对话操作失败", str(exc), "red")

    def command(self, user_id: str, chat_id: str, key: str, text: str, source: str = "") -> None:
        parts = text.split(maxsplit=1)
        command, argument = parts[0].lower(), parts[1].strip() if len(parts) == 2 else ""
        log_event("command_received", command=command, via="card" if source else "text")
        directory = self.current_directory(user_id)
        if command == "/cd-confirm":
            pending = self.pending_directories.pop(argument, None)
            if not pending or pending[0] != user_id or pending[1] != chat_id or time.time() - pending[3] > 600:
                self.feishu.card_or_text(chat_id, "创建目录失败", "确认已失效，请重新发送 `/cd <路径>`。", "yellow")
                return
            target = pending[2]
            try:
                if not self.directory_is_allowed(target):
                    raise ValueError("目录必须位于已配置的工作区根目录内")
                target.mkdir(parents=True, exist_ok=True)
                self.set_directory(user_id, target.resolve())
                self.feishu.card_or_text(chat_id, "目录已创建并切换", f"当前目录：`{target.resolve()}`", "green")
            except Exception as exc:
                self.feishu.card_or_text(chat_id, "创建目录失败", str(exc), "red")
            return
        if command == "/cd-cancel":
            self.pending_directories.pop(argument, None)
            self.feishu.card_or_text(chat_id, "已取消创建目录", f"当前目录保持为：`{directory}`", "grey")
            return
        if command == "/cd":
            with self.task_lock:
                busy = getattr(self, "user_job_counts", {}).get(user_id, 0) > 0
            if busy:
                self.feishu.card_or_text(chat_id, "任务执行中", "请等待你的任务完成，或先用 `/stop` 停止后再切换目录。", "yellow")
                return
            if not argument:
                buttons = self.directory_buttons(directory)
                content = f"当前目录：`{directory}`\n\n选择子目录，或直接发送 `/cd <路径>`。"
                self.feishu.card_or_text(chat_id, "切换工作目录", content, "blue", buttons)
                return
            try:
                target = self.resolve_directory(directory, argument)
                if target.is_dir():
                    self.set_directory(user_id, target)
                    self.feishu.card_or_text(chat_id, "工作目录已切换", f"当前目录：`{target}`", "green")
                elif target.exists():
                    self.feishu.card_or_text(chat_id, "切换目录失败", f"目标不是目录：`{target}`\n\n当前目录保持为：`{directory}`", "red")
                else:
                    request_id = uuid.uuid4().hex
                    self.pending_directories[request_id] = (user_id, chat_id, target, time.time())
                    self.feishu.card_or_text(chat_id, "目录不存在", f"目标：`{target}`\n\n是否创建并进入该目录？", "yellow", [
                        {"text": "创建并进入", "type": "primary", "value": {"command": "/cd-confirm", "directory_id": request_id}},
                        {"text": "取消", "type": "default", "value": {"command": "/cd-cancel", "directory_id": request_id}},
                    ])
            except Exception as exc:
                self.feishu.card_or_text(chat_id, "切换目录失败", f"{exc}\n\n当前目录保持为：`{directory}`", "red")
            return
        if self.current_chat.get("active") and (command in ("/new", "/compact") or (command == "/resume" and argument)):
            self.feishu.card_or_text(chat_id, "任务执行中", "请等待当前任务结束或先停止任务，再切换会话或压缩上下文。", "yellow")
            return
        if command == "/help":
            card_id = self.feishu.card_or_text(chat_id, "Codex 控制面板", "点击执行操作，也支持输入 /命令。",
                                                buttons=self.help_buttons(key))
            if card_id:
                self.help_cards[card_id] = (chat_id, key)
                if len(self.help_cards) > 100:
                    self.help_cards.pop(next(iter(self.help_cards)))
        elif command == "/pair":
            self.feishu.card_or_text(chat_id, "无需配对", "此账号已在白名单中。", "green")
        elif command == "/new":
            try:
                with self.thread_lock, self.task_lock:
                    if any(self.user_job_counts.values()) or self.active_task_tokens:
                        raise ValueError("有任务执行中或正在排队，请等待完成或先停止任务。")
                    self.server.threads.pop(key, None)
                    self.clear_session(key)
                self.feishu.card_or_text(chat_id, "新会话", "已切换到新会话，下次提问时自动创建。", "green")
            except Exception as exc:
                self.feishu.card_or_text(chat_id, "新建会话失败", str(exc), "red")
        elif command == "/plan":
            if argument.lower() in ("on", "开启", "打开"):
                self.plan_modes[key] = True
                self.save_model_settings()
                self.feishu.card_or_text(chat_id, "Plan 模式已开启", "后续消息将先分析和产出计划，不会直接实施。", "blue")
            elif argument.lower() in ("off", "关闭", "退出"):
                self.plan_modes.pop(key, None)
                self.save_model_settings()
                self.feishu.card_or_text(chat_id, "Plan 模式已关闭", "后续消息会在默认模式执行。", "green")
            else:
                enabled = self.plan_modes.get(key, False)
                self.feishu.card_or_text(chat_id, "Plan 模式", "当前：**已开启**。发送 `/plan off` 退出。" if enabled else "当前：**未开启**。发送 `/plan on` 开启。")
        elif command == "/plan-toggle":
            enabled = argument.lower() in ("on", "true", "1", "开启")
            if enabled:
                self.plan_modes[key] = True
            else:
                self.plan_modes.pop(key, None)
            self.save_model_settings()
            try:
                self.update_help_card(source, key)
            except Exception as exc:
                log_event("help_card_update_failed", error_type=type(exc).__name__)
                self.feishu.card_or_text(chat_id, "Plan 模式已" + ("开启" if enabled else "关闭"),
                                         "控制面板更新失败，请发送 `/help` 刷新。", "yellow")
        elif command in ("/plan-implement", "/plan-clear-implement", "/plan-stay"):
            try:
                with self.thread_lock:
                    self.resolve_plan_action(user_id, chat_id, key, command, argument, source)
            except Exception as exc:
                self.feishu.card_or_text(chat_id, "计划操作失败", str(exc), "red")
        elif command in ("/archive", "/unarchive") or (command == "/resume" and argument):
            try:
                if not argument:
                    raise ValueError(f"请输入 {command} <thread_id>")
                with self.thread_lock:
                    self.change_thread(user_id, chat_id, key, directory, argument, command[1:])
            except Exception as exc:
                self.feishu.card_or_text(chat_id, "对话操作失败", str(exc), "red")
        elif command in ("/resume", "/archived"):
            try:
                self.show_threads(user_id, chat_id, key, directory, command == "/archived")
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
            mode = "Plan" if getattr(self, "plan_modes", {}).get(key, False) else "默认执行"
            self.feishu.card_or_text(chat_id, "Codex 状态", f"**任务**\n{task_state}\n\n**目录**\n`{directory}`\n\n**会话**\n`{thread_id or '尚未创建'}`\n\n**模型**\n`{self.models.get(key, DEFAULT_MODEL) or '默认'}`\n\n**模式**\n{mode}\n\n**桥接运行时长**\n{uptime_text}")
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
                self.cancel_questions(key)
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
        chat_id = getattr(context, "open_chat_id", "")
        source = getattr(context, "open_message_id", "")
        if command == "/thread-action":
            if bridge and chat_id and bridge.is_allowed(user_id):
                log_event("card_action_received", command=command)
                threading.Thread(target=bridge.thread_action,
                                 args=(user_id, chat_id, bridge.session_key(user_id), value, source),
                                 daemon=True).start()
            return P2CardActionTriggerResponse({})
        if command in ("/question-answer", "/question-other"):
            if bridge and chat_id and bridge.is_allowed(user_id):
                log_event("card_action_received", command=command)
                threading.Thread(target=bridge.question_action,
                                 args=(user_id, chat_id, bridge.session_key(user_id), value, source),
                                 daemon=True).start()
            return P2CardActionTriggerResponse({})
        if command in ("/approve", "/deny"):
            command += f" {int(value.get('id'))}"
        elif command == "/resume" and value.get("thread_id"):
            command += f" {value['thread_id']}"
        elif command == "/stop" and value.get("task_id"):
            command += f" {value['task_id']}"
        elif command == "/model" and value.get("model"):
            command += f" {value['model']}"
        elif command == "/cd" and value.get("path"):
            command += f" {value['path']}"
        elif command == "/plan-toggle":
            command += " on" if value.get("enabled") else " off"
        elif command in ("/cd-confirm", "/cd-cancel") and value.get("directory_id"):
            command += f" {value['directory_id']}"
        elif command in ("/plan-implement", "/plan-clear-implement", "/plan-stay") and value.get("action_id"):
            command += f" {value['action_id']}"
        if bridge and chat_id and command.startswith("/"):
            if not bridge.is_allowed(user_id):
                return P2CardActionTriggerResponse({})
            log_event("card_action_received", command=command.split(maxsplit=1)[0])
            threading.Thread(
                target=bridge.command,
                args=(user_id, chat_id, bridge.session_key(user_id), command,
                      source),
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


def configure_lark_proxy(client_module, proxy_url: str) -> None:
    """Restore SDK WebSocket environment discovery, with an optional override."""
    if (not hasattr(client_module, "_ws_connect_kwargs") or
            "proxy" not in inspect.signature(client_module.websockets.connect).parameters):
        raise RuntimeError("飞书代理需要支持 _ws_connect_kwargs 的 lark-oapi 和 websockets>=15")
    client_module._ws_connect_kwargs = lambda: {"proxy": proxy_url or True}
    if not proxy_url:
        return
    post = client_module.requests.post

    def proxy_post(*args, **kwargs):
        kwargs["proxies"] = {"http": proxy_url, "https": proxy_url}
        return post(*args, **kwargs)

    client_module.requests = SimpleNamespace(post=proxy_post)


def main() -> None:
    global bridge
    if WORKSPACE_ROOT is None:
        raise RuntimeError("缺少 CODEX_WORKSPACE_ROOT；请在 .env 中设置允许切换的工作区根目录")
    if not WORKSPACE_ROOT.is_dir():
        raise RuntimeError("CODEX_WORKSPACE_ROOT 不存在或不是目录")
    if not ROOT.is_dir() or not ROOT.is_relative_to(WORKSPACE_ROOT):
        raise RuntimeError("CODEX_BRIDGE_CWD 必须位于 CODEX_WORKSPACE_ROOT 内")
    log_event("bridge_starting", cwd=str(ROOT))
    bridge = Bridge()
    log_event("bridge_ready", cwd=str(ROOT))
    import lark_oapi as lark
    import lark_oapi.ws.client as lark_client
    configure_lark_proxy(lark_client, FEISHU_PROXY_URL)
    patch_lark_card_callback(lark)
    handler = (lark.EventDispatcherHandler.builder("", "")
               .register_p2_im_message_receive_v1(on_message)
               .register_p2_card_action_trigger(on_card_action)
               .build())
    lark.ws.Client(APP_ID, APP_SECRET, event_handler=handler, log_level=lark.LogLevel.INFO).start()


if __name__ == "__main__":
    main()
