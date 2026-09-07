"""SDK-only transition process. stdout is private IPC, never a log stream.

No session storage, authorization, Codex calls, REST delivery or business commands.
The Rust parent must own the App-ID service lock before launching this process.
"""
import json
import os
import queue
import sys
import threading
from importlib.metadata import version

PROTOCOL_VERSION = 1
MAX_FRAME_BYTES = 2 * 1024 * 1024


def message_event(data):
    event = data.event
    message = event.message
    sender = getattr(getattr(event, "sender", None), "sender_id", None)
    return {
        "kind": "message",
        "message_id": getattr(message, "message_id", ""),
        "user_id": getattr(sender, "open_id", ""),
        "chat_id": getattr(message, "chat_id", ""),
        "chat_type": getattr(message, "chat_type", ""),
        "message_type": getattr(message, "message_type", "text"),
        "content": json.loads(message.content or "{}"),
    }


def card_event(data):
    event = data.event
    return {
        "kind": "card",
        "user_id": getattr(event.operator, "open_id", ""),
        "chat_id": getattr(event.context, "open_chat_id", ""),
        "message_id": getattr(event.context, "open_message_id", ""),
        "action": getattr(event.action, "value", {}) or {},
    }


class Emitter:
    """Bounded queue and bounded wait for the Rust owner's acceptance."""
    def __init__(self, epoch, capacity=128):
        self.epoch = epoch
        self.sequence = 0
        self.pending = {}
        self.capacity = capacity
        self.lock = threading.Lock()
        self.outgoing = queue.Queue(maxsize=capacity)

    def emit(self, event, confirmation=None):
        with self.lock:
            envelope = {"version": PROTOCOL_VERSION, "epoch": self.epoch,
                        "sequence": self.sequence, "event": event}
            encoded = json.dumps(envelope, ensure_ascii=False, separators=(",", ":")) + "\n"
            if len(encoded.encode("utf-8")) > MAX_FRAME_BYTES:
                raise ValueError("SDK event exceeds IPC frame limit")
            # queue.Full propagates to frame handling, which sends a failed ACK.
            if confirmation is not None and len(self.pending) >= self.capacity:
                raise queue.Full
            self.outgoing.put_nowait(encoded)
            sequence = self.sequence
            if confirmation is not None:
                self.pending[sequence] = confirmation
            self.sequence += 1
            return sequence

    def emit_confirmed(self, event, timeout=2.0):
        confirmation = queue.Queue(maxsize=1)
        sequence = self.emit(event, confirmation)
        try:
            try:
                accepted = confirmation.get(timeout=timeout)
            except queue.Empty:
                raise TimeoutError("Rust acceptance timed out") from None
            if not accepted:
                raise RuntimeError("Rust did not accept event")
        finally:
            with self.lock:
                self.pending.pop(sequence, None)

    def confirm(self, envelope):
        if not isinstance(envelope, dict) or envelope.get("version") != PROTOCOL_VERSION:
            raise ValueError("Invalid acceptance schema")
        if envelope.get("epoch") != self.epoch:
            raise ValueError("Invalid acceptance epoch")
        sequence = envelope.get("sequence")
        accepted = envelope.get("accepted")
        if type(sequence) is not int or sequence < 0 or type(accepted) is not bool:
            raise ValueError("Invalid acceptance fields")
        with self.lock:
            pending = self.pending.get(sequence)
            if pending is not None:
                try:
                    pending.put_nowait(accepted)
                except queue.Full:
                    pass  # A duplicate confirmation cannot replace the first.

    def read_confirmations(self, stream):
        try:
            while True:
                line = stream.readline(4097)
                if not line or len(line) > 4096 or not line.endswith("\n"):
                    raise ValueError("Invalid acceptance frame")
                self.confirm(json.loads(line))
        except (OSError, ValueError):
            os._exit(70)

    def write_forever(self, stream):
        try:
            while True:
                stream.write(self.outgoing.get())
                stream.flush()
        except (BrokenPipeError, OSError):
            # The owner disappeared; don't leave a second bot connection alive.
            os._exit(70)


def main():
    # Refuse unreviewed private-SDK changes rather than silently dropping cards.
    if version("lark-oapi") != "1.7.3" or version("websockets") != "15.0.1":
        raise RuntimeError("SDK versions differ from reviewed compatibility baseline")
    import lark_oapi as lark
    import lark_oapi.ws.client as client_module
    from lark_oapi.event.callback.model.p2_card_action_trigger import P2CardActionTriggerResponse
    from sdk_compat import configure_proxy, patch_cards, observed_client

    emitter = Emitter(os.environ["BRIDGE_CONNECTION_EPOCH"])
    threading.Thread(target=emitter.write_forever, args=(sys.stdout,), daemon=True).start()
    threading.Thread(target=emitter.read_confirmations, args=(sys.stdin,), daemon=True).start()

    def receive(data):
        emitter.emit_confirmed(message_event(data))

    def card(data):
        emitter.emit_confirmed(card_event(data))
        return P2CardActionTriggerResponse({})

    configure_proxy(client_module, os.environ.get("FEISHU_PROXY_URL", ""))
    patch_cards(lark)
    handler = (lark.EventDispatcherHandler.builder("", "")
               .register_p2_im_message_receive_v1(receive)
               .register_p2_card_action_trigger(card).build())
    # The SDK can include payloads/URLs in logs. Disable its handlers; lifecycle
    # comes through explicit IPC hooks, never through parsing SDK log strings.
    from lark_oapi.core.log import logger
    logger.disabled = True
    client_type = observed_client(lark.ws.Client, emitter.emit)
    client = client_type(os.environ["FEISHU_APP_ID"], os.environ["FEISHU_APP_SECRET"],
                            event_handler=handler, log_level=lark.LogLevel.ERROR)
    client.on_reconnecting = lambda: emitter.emit({"kind": "connection", "state": "reconnecting"})
    emitter.emit({"kind": "connection", "state": "starting"})
    client.start()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        # Never print exception payloads, which can contain authenticated URLs.
        print(json.dumps({"event": "adapter_failed", "error_type": type(error).__name__}), file=sys.stderr)
        raise SystemExit(1) from None
