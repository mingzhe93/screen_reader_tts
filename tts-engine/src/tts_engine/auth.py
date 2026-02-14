from __future__ import annotations

from fastapi import Request, WebSocket

from .constants import WS_AUTH_SUBPROTOCOL
from .errors import EngineError


def _parse_bearer_token(raw_header: str | None) -> str | None:
    if not raw_header:
        return None
    parts = raw_header.strip().split(" ", 1)
    if len(parts) != 2:
        return None
    scheme, token = parts
    if scheme.lower() != "bearer":
        return None
    token = token.strip()
    return token or None


def _split_subprotocol_header(raw_header: str | None) -> list[str]:
    if not raw_header:
        return []
    return [segment.strip() for segment in raw_header.split(",") if segment.strip()]


def verify_http_request(request: Request, expected_token: str) -> None:
    provided = _parse_bearer_token(request.headers.get("authorization"))
    if provided != expected_token:
        raise EngineError(
            code="UNAUTHORIZED",
            message="Missing or invalid bearer token",
            status_code=401,
        )


async def verify_websocket(websocket: WebSocket, expected_token: str) -> tuple[bool, str | None]:
    provided = _parse_bearer_token(websocket.headers.get("authorization"))
    if provided == expected_token:
        return True, None

    offered = _split_subprotocol_header(websocket.headers.get("sec-websocket-protocol"))
    for index, protocol in enumerate(offered[:-1]):
        if protocol == WS_AUTH_SUBPROTOCOL and offered[index + 1] == expected_token:
            return True, WS_AUTH_SUBPROTOCOL

    return False, None
