#!/usr/bin/env python3
"""A small Termux-friendly Feishu <-> Codex app-server bridge."""
from __future__ import annotations

import json
import mimetypes
import os
import queue
import shlex
import subprocess
import threading
import time
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
APPROVAL_TIMEOUT = int(os.environ.get("CODEX_APPROVAL_TIMEOUT_SECONDS", "600"))
STREAM_CHUNK = int(os.environ.get("CODEX_STREAM_CHUNK_CHARS", "1200"))
GENERATED_IMAGES = Path(os.environ.get("CODEX_GENERATED_IMAGES", str(Path.home() / ".codex" / "generated_images")))


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
        self.active_turn: dict[str, str] = {}
        self.starting_turns: set[str] = set()
        self.pending_interrupt: set[str] = set()
        self._spawn()

    def _spawn(self) -> None:
        self.process = subprocess.Popen(
            self.command, cwd=ROOT, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            # stdout is a JSON-RPC stream; diagnostics must never be merged into it.
            stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        self._initialize()

    def restart(self) -> None:
        old = self.process
        if old.poll() is None:
            old.kill()
            old.wait(timeout=5)
        self.rpc_id = 0
        self.approvals.clear()
        self.approval_created.clear()
        self.active_turn.clear()
        self._spawn()

    def send(self, message: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        with self.write_lock:
            self.process.stdin.write(json.dumps(message, ensure_ascii=False) + "\n")
            self.process.stdin.flush()

    def _initialize(self) -> None:
        self.request("initialize", {"clientInfo": {"name": "feishu-codex-bridge", "version": "0.1.0"}})
        self.send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

    def _read(self) -> dict[str, Any]:
        assert self.process.stdout is not None
        line = self.process.stdout.readline()
        if not line:
            raise RuntimeError("codex app-server 已退出")
        return json.loads(line)

    def request(self, method: str, params: dict[str, Any], timeout: int = 30) -> dict[str, Any]:
        self.rpc_id += 1
        ident = self.rpc_id
        self.send({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
        deadline = time.time() + timeout
        while time.time() < deadline:
            message = self._read()
            if message.get("id") == ident:
                if "error" in message:
                    raise RuntimeError(message["error"])
                return message.get("result", {})
            self.handle_event(message)
        raise TimeoutError(f"Codex 请求超时：{method}")

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
            self.event("completed", params)
        elif method.startswith("item/"):
            self.event("item", params)

    def turn(self, key: str, prompt: str, model: str,
             extra_inputs: list[dict[str, Any]] | None = None) -> None:
        self.turn_text = ""
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
            message = self._read()
            self.handle_event(message)
            if message.get("method") == "turn/completed":
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
        self.send({"jsonrpc": "2.0", "id": self.rpc_id + 1,
                   "method": "turn/interrupt",
                   "params": {"threadId": thread_id, "turnId": turn_id}})
        self.rpc_id += 1
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
        self.feishu = Feishu()
        self.server = CodexServer(self.codex_event)
        self.models: dict[str, str] = {}
        self.current_chat: dict[str, str] = {}
        self.stream_buffers: dict[str, str] = {}
        self.jobs: queue.Queue[tuple[str, str, str, dict[str, str] | None, int]] = queue.Queue()
        self.generations: dict[str, int] = {}
        self.seen_messages: deque[str] = deque(maxlen=1000)
        self.seen_message_set: set[str] = set()
        threading.Thread(target=self.worker, daemon=True).start()
        threading.Thread(target=self.approval_reaper, daemon=True).start()

    def session_key(self, user_id: str) -> str:
        return f"{user_id}:{ROOT}"

    def load_session(self) -> str:
        try:
            value = SESSION_FILE.read_text(encoding="utf-8").strip()
            return value if value else ""
        except FileNotFoundError:
            return ""

    def save_session(self, thread_id: str) -> None:
        temporary = SESSION_FILE.with_name(SESSION_FILE.name + ".tmp")
        temporary.write_text(thread_id + "\n", encoding="utf-8")
        os.replace(temporary, SESSION_FILE)

    def clear_session(self) -> None:
        try:
            SESSION_FILE.unlink()
        except FileNotFoundError:
            pass

    def remember_message(self, message_id: str) -> bool:
        if not message_id or message_id in self.seen_message_set:
            return False
        if len(self.seen_messages) == self.seen_messages.maxlen:
            self.seen_message_set.discard(self.seen_messages[0])
        self.seen_messages.append(message_id)
        self.seen_message_set.add(message_id)
        return True

    def codex_event(self, kind: str, value: Any) -> None:
        if kind == "delta":
            chat_id = self.current_chat.get("active", "")
            if not chat_id:
                return
            buffer = self.stream_buffers.get(chat_id, "") + str(value)
            while len(buffer) >= STREAM_CHUNK:
                self.feishu.text(chat_id, "[Codex 进度]\n" + buffer[:STREAM_CHUNK])
                buffer = buffer[STREAM_CHUNK:]
            self.stream_buffers[chat_id] = buffer
        elif kind == "approval":
            request_id = int(value["id"])
            self.server.approvals[request_id] = self.current_chat.get("active", "")
            self.server.approval_created[request_id] = time.time()
            chat_id = self.server.approvals[request_id]
            if chat_id:
                self.feishu.text(chat_id, f"Codex 请求审批（{request_id}）。回复 /approve {request_id} 或 /deny {request_id}")

    def approval_reaper(self) -> None:
        while True:
            time.sleep(5)
            now = time.time()
            for request_id, created in list(self.server.approval_created.items()):
                if now - created < APPROVAL_TIMEOUT:
                    continue
                chat_id = self.server.approvals.get(request_id, "")
                try:
                    self.server.approve(request_id, False)
                    if chat_id:
                        self.feishu.text(chat_id, f"审批 {request_id} 已超时，已自动拒绝。")
                except Exception:
                    self.server.approval_created.pop(request_id, None)

    def worker(self) -> None:
        while True:
            key, chat_id, prompt, resource, generation = self.jobs.get()
            if generation != self.generations.get(key, 0):
                self.feishu.text(chat_id, "任务已取消（仍在等待队列中）。")
                self.jobs.task_done()
                continue
            self.current_chat["active"] = chat_id
            try:
                before = self.snapshot()
                started_at = time.time()
                self.feishu.text(chat_id, "Codex 开始处理…")
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
                stored = self.load_session()
                if stored and key not in self.server.threads:
                    self.server.resume(key, stored)
                self.server.turn(key, prompt, self.models.get(key, DEFAULT_MODEL), extra_inputs)
                self.save_session(self.server.threads[key])
                remainder = self.stream_buffers.pop(chat_id, "")
                if remainder:
                    self.feishu.text(chat_id, "[Codex 进度]\n" + remainder)
                self.feishu.text(chat_id, self.server.turn_text or "Codex 已完成，但没有返回文字。")
                for path in self.changed_files(before):
                    self.feishu.upload_file(chat_id, path)
                for path in self.generated_files(key, started_at):
                    self.feishu.upload_file(chat_id, path)
            except Exception as exc:
                if self.server.process.poll() is not None:
                    try:
                        self.server.restart()
                        self.feishu.text(chat_id, "Codex 进程已退出，已自动重启；下一条消息会自动恢复会话。")
                    except Exception as restart_error:
                        self.feishu.text(chat_id, f"Codex 自动重启失败：{restart_error}")
                self.feishu.text(chat_id, f"Codex 执行失败：{exc}")
            finally:
                self.current_chat.pop("active", None)
                self.jobs.task_done()

    def snapshot(self) -> dict[str, tuple[int, int]]:
        result: dict[str, tuple[int, int]] = {}
        for path in ROOT.rglob("*"):
            if (not path.is_file() or ".git" in path.parts or
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
        if not self.remember_message(getattr(message, "message_id", "")):
            return
        sender = getattr(getattr(event, "sender", None), "sender_id", None)
        user_id = getattr(sender, "open_id", "")
        print(f"Received Feishu message: open_id={user_id or '<missing>'}", flush=True)
        if ALLOWED and user_id not in ALLOWED:
            print(f"Ignored unauthorized Feishu user: {user_id}", flush=True)
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

    def command(self, user_id: str, chat_id: str, key: str, text: str) -> None:
        parts = text.split(maxsplit=1)
        command, argument = parts[0].lower(), parts[1].strip() if len(parts) == 2 else ""
        if command == "/help":
            self.feishu.text(chat_id, "/new  /resume [thread_id]  /model [model]  /models  /status  /stop  /compact  /approve <id>  /deny <id>")
        elif command == "/new":
            self.server.threads.pop(key, None)
            self.clear_session()
            self.feishu.text(chat_id, "已切换到新会话，下次提问时创建。")
        elif command == "/resume" and argument:
            self.server.resume(key, argument)
            self.save_session(argument)
            self.feishu.text(chat_id, f"已恢复会话：{argument}")
        elif command == "/resume":
            try:
                threads = self.server.list_threads()
                lines = [f"{item.get('id')}  {item.get('title') or '未命名'}" for item in threads]
                self.feishu.text(chat_id, "可恢复会话：\n" + ("\n".join(lines) or "没有找到会话"))
            except Exception as exc:
                self.feishu.text(chat_id, f"读取会话列表失败：{exc}")
        elif command == "/model":
            if argument:
                self.models[key] = argument
                self.feishu.text(chat_id, f"后续请求使用模型：{argument}")
            else:
                self.feishu.text(chat_id, f"当前模型：{self.models.get(key, DEFAULT_MODEL) or 'Codex 默认'}")
        elif command == "/models":
            self.feishu.text(chat_id, "可用模型：\n" + "\n".join(self.server.models()))
        elif command == "/status":
            thread_id = self.server.threads.get(key) or self.load_session()
            self.feishu.text(chat_id, f"目录：{ROOT}\n会话：{thread_id or '尚未创建'}\n模型：{self.models.get(key, DEFAULT_MODEL) or '默认'}")
        elif command == "/stop":
            try:
                self.generations[key] = self.generations.get(key, 0) + 1
                interrupted = self.server.interrupt(key)
                self.feishu.text(chat_id, "已请求停止当前 Codex turn。" if interrupted else "已取消排队任务，并登记停止正在启动的任务。")
            except Exception as exc:
                self.feishu.text(chat_id, f"停止失败：{exc}")
        elif command == "/compact":
            try:
                self.server.compact(key)
                self.feishu.text(chat_id, "已请求压缩当前会话上下文。")
            except Exception as exc:
                self.feishu.text(chat_id, f"压缩失败：{exc}")
        elif command in ("/approve", "/deny") and argument.isdigit():
            try:
                self.server.approve(int(argument), command == "/approve")
                self.feishu.text(chat_id, "审批结果已提交。")
            except Exception as exc:
                self.feishu.text(chat_id, f"审批失败：{exc}")
        else:
            self.feishu.text(chat_id, "未知命令，请发送 /help。")


bridge: Bridge | None = None


def on_message(data: lark.im.v1.P2ImMessageReceiveV1) -> None:
    assert bridge is not None
    bridge.receive(data)


def main() -> None:
    global bridge
    print(f"Starting Feishu Codex Bridge; cwd={ROOT}", flush=True)
    bridge = Bridge()
    print(f"Feishu Codex Bridge ready; cwd={ROOT}", flush=True)
    import lark_oapi as lark
    handler = lark.EventDispatcherHandler.builder("", "").register_p2_im_message_receive_v1(on_message).build()
    lark.ws.Client(APP_ID, APP_SECRET, event_handler=handler, log_level=lark.LogLevel.INFO).start()


if __name__ == "__main__":
    main()
