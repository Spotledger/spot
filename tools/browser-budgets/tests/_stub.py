"""Starts the stub backend (a Node process) for tests; HTTP over http.client, no packages."""

from __future__ import annotations

import http.client
import json
import os
import shutil
import subprocess
import unittest
from pathlib import Path

SERVER = Path(__file__).resolve().parents[1] / "stub-backend" / "server.mjs"
USER = "budgets@stub.test"
PASSWORD = "stub-only-not-a-secret"
NODE = shutil.which("node")

needs_node = unittest.skipUnless(NODE, "node is not installed")


def stub_env(**extra):
    env = {key: value for key, value in os.environ.items() if key not in ("CI", "SPOT_BUDGETS_STUB")}
    env.update(extra)
    return env


class StubProcess:
    def __init__(self, origins=(), env=None):
        self.origins = list(origins)
        self.env = env if env is not None else stub_env(SPOT_BUDGETS_STUB="1")
        self.process = None
        self.port = None

    def start(self):
        if not NODE:
            raise unittest.SkipTest("node is not installed")
        args = [NODE, str(SERVER)]
        for origin in self.origins:
            args += ["--allow-origin", origin]
        self.process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                        env=self.env, text=True)
        line = self.process.stdout.readline()
        if not line:
            raise RuntimeError(f"the stub did not start: {self.process.stderr.read()}")
        self.port = json.loads(line)["port"]

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            self.process.wait(timeout=10)
        for stream in (self.process.stdout, self.process.stderr) if self.process else ():
            stream.close()

    def request(self, method, path, body=None, token=None, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        sent = dict(headers or {})
        data = None
        if body is not None:
            data = json.dumps(body)
            sent["Content-Type"] = "application/json"
        if token:
            sent["Authorization"] = f"Bearer {token}"
        try:
            connection.request(method, path, data, sent)
            response = connection.getresponse()
            raw = response.read()
            parsed = json.loads(raw) if raw else None
            return response.status, {k.lower(): v for k, v in response.getheaders()}, parsed
        finally:
            connection.close()

    def login(self):
        status, _, body = self.request("POST", "/api/v1/auth/login", {"user": USER, "password": PASSWORD})
        assert status == 200, (status, body)
        return body
