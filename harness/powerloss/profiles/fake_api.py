"""An in-memory stand-in for the native API's sign-in, create and list calls (WP-PRD-05.T3).

For the dry run and the tests only: it stores committed documents, replays an ``Idempotency-Key``
and can lose an answer after committing (the cut between commit and reply).
"""

from __future__ import annotations

import json
from typing import Mapping

TOKEN = "fake-token"


class FakeApi:
    def __init__(self, page: int = 500) -> None:
        self.docs: dict[str, dict] = {}
        self.keys: dict[str, str] = {}
        self.lose_reply_for: set[str] = set()  # idempotency keys whose first reply is lost
        self.page = page
        self.counter = 0

    def transport(self, method: str, url: str, headers: Mapping[str, str], body: "bytes | None") -> "tuple[int, str]":
        from operator_http import HttpError

        path = url.split("://", 1)[1].split("/", 1)[1]
        if path == "api/v1/auth/login":
            ok = body is not None and json.loads(body).get("password") == "pw"
            return (200, json.dumps({"data": {"token": TOKEN}})) if ok else (401, "{}")
        if headers.get("Authorization") != f"Bearer {TOKEN}":
            return 401, "{}"
        route = path.split("?", 1)[0]
        if method == "POST" and route.startswith("api/v1/resource/"):
            key = headers.get("Idempotency-Key", "")
            if key in self.keys:
                return 201, self.keys[key]
            doc = json.loads(body or b"{}")
            if "id" not in doc:
                return 422, "{}"
            self.counter += 1
            stored = {"id": doc["id"], "version": 1, "name": f"POST-{self.counter:05d}"}
            self.docs[doc["id"]] = stored
            reply = json.dumps({"data": stored})
            self.keys[key] = reply
            if key in self.lose_reply_for:
                self.lose_reply_for.discard(key)
                raise HttpError(0, "reply lost")
            return 201, reply
        if method == "GET" and route.startswith("api/v1/resource/"):
            rows = [self.docs[k] for k in sorted(self.docs)]
            start = 0
            if "cursor=" in path:
                start = int(path.split("cursor=", 1)[1].split("&", 1)[0])
            chunk = rows[start : start + self.page]
            nxt = str(start + self.page) if start + self.page < len(rows) else None
            return 200, json.dumps({"data": chunk, "next_cursor": nxt})
        return 404, "{}"
