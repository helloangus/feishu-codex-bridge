"""Pinned lark-oapi compatibility; never imported by Rust business logic."""
import base64
import http
import inspect
import time
from types import SimpleNamespace


def configure_proxy(client_module, proxy_url):
    if not hasattr(client_module, "_ws_connect_kwargs") or \
            "proxy" not in inspect.signature(client_module.websockets.connect).parameters:
        raise RuntimeError("unsupported lark-oapi/websockets compatibility")
    client_module._ws_connect_kwargs = lambda: {"proxy": proxy_url or True}
    if not proxy_url:
        return
    original = client_module.requests.post

    def post(*args, **kwargs):
        kwargs["proxies"] = {"http": proxy_url, "https": proxy_url}
        return original(*args, **kwargs)
    client_module.requests = SimpleNamespace(post=post)


def patch_cards(lark):
    from lark_oapi.core.json import JSON
    from lark_oapi.ws.client import _get_by_key
    from lark_oapi.ws.const import (
        HEADER_BIZ_RT, HEADER_MESSAGE_ID, HEADER_SEQ, HEADER_SUM, HEADER_TYPE,
    )
    from lark_oapi.ws.enum import MessageType
    from lark_oapi.ws.model import Response

    if "message_type == MessageType.CARD" not in inspect.getsource(lark.ws.Client._handle_data_frame):
        raise RuntimeError("unrecognized SDK frame handler; review compatibility before upgrading")

    async def handle(self, frame):
        headers = frame.headers
        message_id = _get_by_key(headers, HEADER_MESSAGE_ID)
        count = int(_get_by_key(headers, HEADER_SUM))
        sequence = int(_get_by_key(headers, HEADER_SEQ))
        payload = frame.payload
        if count > 1:
            payload = self._combine(message_id, count, sequence, payload)
            if payload is None:
                return
        if MessageType(_get_by_key(headers, HEADER_TYPE)) not in (MessageType.EVENT, MessageType.CARD):
            return
        response = Response(code=http.HTTPStatus.OK)
        try:
            started = time.monotonic()
            result = self._event_handler._do_without_validation(payload)
            header = headers.add()
            header.key = HEADER_BIZ_RT
            header.value = str(int((time.monotonic() - started) * 1000))
            if result is not None:
                response.data = base64.b64encode(JSON.marshal(result).encode("utf-8"))
        except Exception:
            response = Response(code=http.HTTPStatus.INTERNAL_SERVER_ERROR)
        frame.payload = JSON.marshal(response).encode("utf-8")
        await self._write_message(frame.SerializeToString())

    lark.ws.Client._handle_data_frame = handle


def observed_client(base, emit):
    """Keep the reviewed private connection hook inside the SDK boundary."""
    if not inspect.iscoroutinefunction(getattr(base, "_connect", None)):
        raise RuntimeError("unsupported SDK connection lifecycle")

    class ObservedClient(base):
        async def _connect(self):
            previous = getattr(self, "_conn", None)
            await super()._connect()
            current = getattr(self, "_conn", None)
            if current is not None and current is not previous:
                emit({"kind": "connection", "state": "connected"})

    return ObservedClient
