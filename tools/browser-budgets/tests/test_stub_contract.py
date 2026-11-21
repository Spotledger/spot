"""The stub backend's data and responses against contracts/native-api/openapi.yaml (WP-UI-09.T5)."""

from __future__ import annotations

import json
import sys
import unittest

import _support
import _stub

sys.path.insert(0, str(_support.ROOT / "stub-backend"))
import generate_data  # noqa: E402

import _schema  # noqa: E402

SCHEMAS = "#/components/schemas/"


def no_float(text):
    """json.loads that refuses any float: amounts and quantities are decimal strings (ADR-12)."""
    def refuse(token):
        raise AssertionError(f"a JSON float ({token}) in the stub data")
    return json.loads(text, parse_float=refuse)


@unittest.skipIf(_schema.yaml is None, "PyYAML is not installed: the contract cannot be read")
class ResponsesMatchTheContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.validator = _schema.Validator(_schema.load_spec())
        cls.stub = _stub.StubProcess()
        cls.stub.start()
        cls.token = cls.stub.login()["data"]["token"]

    @classmethod
    def tearDownClass(cls):
        cls.stub.stop()

    def check(self, ref, body):
        errors = self.validator.errors({"$ref": SCHEMAS + ref}, body)
        self.assertEqual(errors, [], errors[:5])

    def get(self, path, token=True):
        return self.stub.request("GET", path, token=self.token if token else None)

    def test_health(self):
        status, _, body = self.get("/api/health/live", token=False)
        self.assertEqual(status, 200)
        self.check("Live", body)
        status, _, body = self.get("/api/health/ready", token=False)
        self.assertEqual(status, 200)
        self.check("Ready", body)

    def test_login_and_session(self):
        body = self.stub.login()
        self.check("Session", body["data"])
        status, _, session = self.get("/api/v1/auth/session")
        self.assertEqual(status, 200)
        self.check("SessionInfo", session["data"])

    def test_errors(self):
        status, _, body = self.stub.request("POST", "/api/v1/auth/login",
                                            {"user": _stub.USER, "password": "wrong"})
        self.assertEqual(status, 401)
        self.check("ErrorEnvelope", body)
        self.assertEqual(body["error"]["code"], "auth.invalid_credentials")
        status, _, body = self.get("/api/v1/auth/session", token=False)
        self.assertEqual((status, body["error"]["code"]), (401, "auth.required"))
        self.check("ErrorEnvelope", body)
        status, _, body = self.get("/api/v1/meta/no_such_doctype")
        self.assertEqual((status, body["error"]["code"]), (404, "meta.unknown_doctype"))
        self.check("ErrorEnvelope", body)
        status, _, body = self.get("/api/v1/resource/sales_invoice/0192f3c4-0001-7000-8000-ffffffffffff")
        self.assertEqual((status, body["error"]["code"]), (404, "document.not_found"))
        self.check("ErrorEnvelope", body)

    def test_error_codes_are_registered(self):
        registry = self.validator.spec["x-spot-error-codes"]
        for code in ("auth.invalid_credentials", "auth.required", "auth.session_expired",
                     "meta.unknown_doctype", "document.not_found", "api.unknown_route",
                     "api.method_not_allowed", "api.invalid_request", "api.invalid_json", "http.internal"):
            self.assertIn(code, registry)

    def test_meta(self):
        status, headers, body = self.get("/api/v1/meta/sales_invoice")
        self.assertEqual(status, 200)
        self.check("DocTypeMeta", body["data"])
        self.assertEqual(headers["etag"], '"1"')
        status, _, _ = self.stub.request("GET", "/api/v1/meta/sales_invoice", token=self.token,
                                         headers={"If-None-Match": '"1"'})
        self.assertEqual(status, 304)

    def test_list_and_paging(self):
        status, _, page = self.get("/api/v1/resource/sales_invoice?limit=500")
        self.assertEqual(status, 200)
        self.check("DocumentPage", page)
        self.assertEqual(len(page["data"]), 500)
        seen = {row["id"] for row in page["data"]}
        cursor = page["next_cursor"]
        while cursor:
            status, _, page = self.get(f"/api/v1/resource/sales_invoice?limit=500&cursor={cursor}")
            self.check("DocumentPage", page)
            seen |= {row["id"] for row in page["data"]}
            cursor = page["next_cursor"]
        self.assertEqual(len(seen), 2000)

    def test_query(self):
        status, _, page = self.stub.request("POST", "/api/v1/resource/sales_invoice/query",
                                            {"fields": ["id", "name"], "limit": 50, "with_total": True},
                                            token=self.token)
        self.assertEqual(status, 200)
        self.check("DocumentPage", page)
        self.assertEqual((len(page["data"]), page["total"]), (50, 2000))

    def test_document_with_twenty_child_rows(self):
        _, _, page = self.get("/api/v1/resource/sales_invoice?limit=1")
        status, headers, body = self.get(f"/api/v1/resource/sales_invoice/{page['data'][0]['id']}")
        self.assertEqual(status, 200)
        self.check("DocumentResponse", body)
        self.assertEqual(headers["etag"], '"1"')
        self.assertEqual(len(body["data"]["items"]), 20)
        for row in body["data"]["items"]:
            self.check("ChildRow", row)


class DataFiles(unittest.TestCase):
    def test_files_equal_the_generator_output(self):
        for name, text in generate_data.files().items():
            self.assertEqual((generate_data.DATA / name).read_text(encoding="utf-8"), text, name)

    def test_shape(self):
        rows = no_float((generate_data.DATA / "list-rows.json").read_text(encoding="utf-8"))
        self.assertEqual(len(rows), 2000)
        self.assertEqual(len({row["id"] for row in rows}), 2000)
        document = no_float((generate_data.DATA / "document.json").read_text(encoding="utf-8"))
        self.assertEqual(len(document["items"]), 20)
        self.assertIsInstance(document["grand_total"], str)
        meta = no_float((generate_data.DATA / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta["sales_invoice"]["label"], "Sales Invoice")
        for field in meta["sales_invoice"]["fields"] + meta["sales_invoice_item"]["fields"]:
            if field["type"] == "decimal":
                self.assertIsInstance(field.get("default", "0"), str)


if __name__ == "__main__":
    unittest.main()
