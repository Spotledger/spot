"""The stub backend's own safety rules (WP-UI-09.T5)."""

from __future__ import annotations

import subprocess
import unittest

import _stub
from _stub import StubProcess, needs_node, stub_env


@needs_node
class Refusals(unittest.TestCase):
    def run_stub(self, env):
        return subprocess.run([_stub.NODE, str(_stub.SERVER)], env=env, capture_output=True,
                              text=True, timeout=20)

    def test_refuses_without_ci_or_the_variable(self):
        result = self.run_stub(stub_env())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("refuses to run outside CI", result.stderr)
        self.assertEqual(result.stdout, "")

    def test_refuses_when_ci_is_not_true(self):
        result = self.run_stub(stub_env(CI="false", SPOT_BUDGETS_STUB="0"))
        self.assertNotEqual(result.returncode, 0)

    def test_starts_under_ci_true(self):
        stub = StubProcess(env=stub_env(CI="true"))
        stub.start()
        try:
            status, _, body = stub.request("GET", "/api/health/live")
            self.assertEqual((status, body["status"]), (200, "live"))
        finally:
            stub.stop()


@needs_node
class Behaviour(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.origin = "http://127.0.0.1:9"
        cls.stub = StubProcess(origins=[cls.origin])
        cls.stub.start()

    @classmethod
    def tearDownClass(cls):
        cls.stub.stop()

    def test_signs_in_and_lists(self):
        token = self.stub.login()["data"]["token"]
        status, _, page = self.stub.request("GET", "/api/v1/resource/sales_invoice?limit=3", token=token)
        self.assertEqual(status, 200)
        self.assertEqual(len(page["data"]), 3)
        self.assertIsNotNone(page["next_cursor"])

    def test_refuses_a_wrong_credential(self):
        for user, password in ((_stub.USER, "nope"), ("someone@else.test", _stub.PASSWORD), ("", "")):
            status, _, body = self.stub.request("POST", "/api/v1/auth/login",
                                                {"user": user, "password": password})
            self.assertIn(status, (400, 401))
            self.assertNotIn("token", str(body))

    def test_refuses_a_request_without_or_with_a_wrong_token(self):
        self.assertEqual(self.stub.request("GET", "/api/v1/resource/sales_invoice")[0], 401)
        self.assertEqual(self.stub.request("GET", "/api/v1/resource/sales_invoice", token="x")[0], 401)

    def test_listens_on_loopback_only(self):
        _, _, state = self.stub.request("GET", "/__stub/state")
        self.assertEqual(state["bind"], "127.0.0.1")

    def test_never_sets_a_cookie(self):
        token = self.stub.login()["data"]["token"]
        for method, path, body, use in (
            ("GET", "/api/health/live", None, None),
            ("POST", "/api/v1/auth/login", {"user": _stub.USER, "password": _stub.PASSWORD}, None),
            ("POST", "/api/v1/auth/login", {"user": _stub.USER, "password": "bad"}, None),
            ("GET", "/api/v1/auth/session", None, token),
            ("GET", "/api/v1/resource/sales_invoice", None, token),
        ):
            _, headers, _ = self.stub.request(method, path, body, token=use, headers={"Origin": self.origin})
            self.assertNotIn("set-cookie", headers, path)

    def test_cors_allows_only_the_configured_origin(self):
        _, headers, _ = self.stub.request("GET", "/api/health/live", headers={"Origin": self.origin})
        self.assertEqual(headers["access-control-allow-origin"], self.origin)
        _, headers, _ = self.stub.request("GET", "/api/health/live", headers={"Origin": "http://evil.test"})
        self.assertNotIn("access-control-allow-origin", headers)
        status, headers, _ = self.stub.request("OPTIONS", "/api/v1/auth/login", headers={
            "Origin": self.origin, "Access-Control-Request-Method": "POST",
            "Access-Control-Request-Headers": "content-type,authorization"})
        self.assertEqual(status, 204)
        self.assertEqual(headers["access-control-allow-origin"], self.origin)
        self.assertNotEqual(headers["access-control-allow-origin"], "*")
        status, _, _ = self.stub.request("OPTIONS", "/api/v1/auth/login", headers={"Origin": "http://evil.test"})
        self.assertEqual(status, 403)

    def test_unknown_route_and_method(self):
        token = self.stub.login()["data"]["token"]
        self.assertEqual(self.stub.request("GET", "/api/v1/nothing", token=token)[0], 404)
        self.assertEqual(self.stub.request("POST", "/api/health/live")[0], 405)


if __name__ == "__main__":
    unittest.main()
