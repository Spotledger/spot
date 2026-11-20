"""The HTTP side of the operator readers and the post client (WP-PRD-05.T3).

Everything goes through one injected ``transport(method, url, headers, body) -> (status, text)``
so the tests and the dry run use a fake API. The real transport is ``urllib``. The password is
read from the environment only (``SPOT_POWERLOSS_PASSWORD``), never an argument, never printed,
never part of an error text.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Callable, Mapping

BASE_URL_ENV = "SPOT_POWERLOSS_BASE_URL"
USER_ENV = "SPOT_POWERLOSS_USER"
PASSWORD_ENV = "SPOT_POWERLOSS_PASSWORD"
DOCTYPE_ENV = "SPOT_POWERLOSS_DOCTYPE"
DEFAULT_DOCTYPE = "posting"
MAX_PAGES = 10_000

Transport = Callable[[str, str, Mapping[str, str], "bytes | None"], "tuple[int, str]"]


class HttpError(Exception):
    """A call failed in a way the caller must classify; carries a status (0: no answer)."""

    def __init__(self, status: int, what: str) -> None:
        super().__init__(f"{what} (status {status})")
        self.status = status


def urllib_transport(timeout_s: float = 30.0) -> Transport:
    def send(method: str, url: str, headers: Mapping[str, str], body: "bytes | None") -> "tuple[int, str]":
        request = urllib.request.Request(url, data=body, method=method, headers=dict(headers))
        try:
            with urllib.request.urlopen(request, timeout=timeout_s) as response:  # noqa: S310 (operator-given http(s) URL)
                return response.status, response.read().decode("utf-8", "replace")
        except urllib.error.HTTPError as exc:
            return exc.code, exc.read().decode("utf-8", "replace")
        except (urllib.error.URLError, OSError) as exc:
            raise HttpError(0, f"no answer from the server ({type(exc).__name__})") from exc

    return send


class Config:
    def __init__(self, base_url: str, user: str, password: str, doctype: str = DEFAULT_DOCTYPE) -> None:
        if not base_url.startswith(("http://", "https://")):
            raise ValueError("base URL must start with http:// or https://")
        if not (user and password):
            raise ValueError(f"{USER_ENV} and {PASSWORD_ENV} must be set")
        if not doctype.replace("_", "").isalnum():
            raise ValueError("doctype is not a plain storage name")
        self.base_url = base_url.rstrip("/")
        self.user = user
        self.password = password
        self.doctype = doctype

    @staticmethod
    def from_env(environ: Mapping[str, str]) -> "Config":
        return Config(
            environ.get(BASE_URL_ENV, ""),
            environ.get(USER_ENV, ""),
            environ.get(PASSWORD_ENV, ""),
            environ.get(DOCTYPE_ENV, DEFAULT_DOCTYPE),
        )

    def __repr__(self) -> str:  # never shows the password
        return f"Config({self.base_url!r}, user={self.user!r}, doctype={self.doctype!r})"


def _json(status: int, text: str, what: str) -> dict:
    try:
        value = json.loads(text)
    except json.JSONDecodeError as exc:
        raise HttpError(status, f"{what}: the answer is not JSON") from exc
    if not isinstance(value, dict):
        raise HttpError(status, f"{what}: the answer is not an object")
    return value


class Session:
    """A signed-in session over the native API."""

    def __init__(self, config: Config, transport: Transport) -> None:
        self.config = config
        self.transport = transport
        self.token = ""

    def login(self) -> None:
        body = json.dumps({"user": self.config.user, "password": self.config.password}).encode()
        status, text = self.transport(
            "POST", f"{self.config.base_url}/api/v1/auth/login", {"Content-Type": "application/json"}, body
        )
        if status != 200:
            raise HttpError(status, "sign-in refused")
        token = _json(status, text, "sign-in").get("data", {}).get("token")
        if not isinstance(token, str) or not token:
            raise HttpError(status, "sign-in returned no token")
        self.token = token

    def headers(self, extra: "Mapping[str, str] | None" = None) -> dict[str, str]:
        out = {"Authorization": f"Bearer {self.token}", "Content-Type": "application/json"}
        out.update(extra or {})
        return out

    def create(self, document: dict, idempotency_key: str) -> "tuple[int, dict]":
        """POST one document; returns ``(status, parsed body or {})``."""
        status, text = self.transport(
            "POST",
            f"{self.config.base_url}/api/v1/resource/{self.config.doctype}",
            self.headers({"Idempotency-Key": idempotency_key}),
            json.dumps(document).encode(),
        )
        try:
            body = json.loads(text) if text.strip() else {}
        except json.JSONDecodeError:
            body = {}
        return status, body if isinstance(body, dict) else {}

    def list_all(self) -> list[dict]:
        """Every document of the doctype through the HTTP list port, sorted by id."""
        rows: list[dict] = []
        cursor = ""
        for _ in range(MAX_PAGES):
            url = f"{self.config.base_url}/api/v1/resource/{self.config.doctype}?limit=500&fields=id,version,name"
            if cursor:
                url += f"&cursor={cursor}"
            status, text = self.transport("GET", url, self.headers(), None)
            if status != 200:
                raise HttpError(status, "list refused")
            page = _json(status, text, "list")
            rows.extend(r for r in page.get("data", []) if isinstance(r, dict))
            cursor = page.get("next_cursor") or ""
            if not cursor:
                return sorted(rows, key=lambda r: str(r.get("id", "")))
        raise HttpError(0, "list did not end")
