#!/usr/bin/env python3
"""Tests for the ``$ref`` handling in ``convert.py`` (WP-CON-03.T8).

Run directly (``python3 tools/contract-clients/test_convert.py``) or with
``python3 -m unittest``. The fixtures are YAML text, so the tests exercise the
same load path ``convert.py`` uses on the real contract.

The bug these cover: ``resolve_ref`` used to split ``ref[1:]``, which leaves the
leading ``/`` of ``#/components/schemas/X`` and raises ``KeyError: ''`` on the
first resolution. It only fired once the contract carried a ``$ref`` with
sibling keywords, because plain ``$ref`` objects are passed through untouched.
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from typing import Any

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))

import convert  # noqa: E402  (after the path fix-up above)


def load(text: str) -> dict[str, Any]:
    return yaml.safe_load(text)


class ResolveRef(unittest.TestCase):
    """``resolve_ref`` walks a local JSON Pointer over the document."""

    def test_plain_ref(self) -> None:
        root = load(
            """
            components:
              schemas:
                Identifier:
                  type: string
                  pattern: "^[a-z]+$"
            """
        )
        self.assertEqual(
            convert.resolve_ref(root, "#/components/schemas/Identifier"),
            {"type": "string", "pattern": "^[a-z]+$"},
        )

    def test_pointer_escapes(self) -> None:
        # RFC 6901: '~1' is '/', '~0' is '~', and '~1' must be undone first so
        # a literal '~1' key survives.
        root = {"components": {"schemas": {"a/b": {"t": 1}, "a~b": {"t": 2}, "a~1b": {"t": 3}}}}
        self.assertEqual(convert.resolve_ref(root, "#/components/schemas/a~1b"), {"t": 1})
        self.assertEqual(convert.resolve_ref(root, "#/components/schemas/a~0b"), {"t": 2})
        self.assertEqual(convert.resolve_ref(root, "#/components/schemas/a~01b"), {"t": 3})

    def test_index_into_a_list(self) -> None:
        root = {"paths": {"/x": {"get": {"parameters": [{"name": "a"}, {"name": "b"}]}}}}
        self.assertEqual(
            convert.resolve_ref(root, "#/paths/~1x/get/parameters/1"), {"name": "b"}
        )

    def test_non_local_ref_refused(self) -> None:
        root: dict[str, Any] = {"components": {"schemas": {}}}
        for ref in (
            "other.yaml#/components/schemas/X",  # another file
            "https://example.test/openapi.yaml#/components/schemas/X",  # remote
            "components/schemas/X",  # no '#' fragment marker
            "#",  # the whole document, not a '#/...' pointer
        ):
            with self.subTest(ref=ref):
                with self.assertRaises(SystemExit) as caught:
                    convert.resolve_ref(root, ref)
                self.assertIn(ref, str(caught.exception.code))

    def test_missing_target_names_the_ref(self) -> None:
        with self.assertRaises(SystemExit) as caught:
            convert.resolve_ref({"components": {"schemas": {}}}, "#/components/schemas/Absent")
        self.assertIn("#/components/schemas/Absent", str(caught.exception.code))


class FixSchemaWithRef(unittest.TestCase):
    """``fix_schema`` inlines a ``$ref`` that carries sibling keywords."""

    def setUp(self) -> None:
        del convert.DROPPED[:]
        del convert.TRANSLATED[:]
        del convert.ALL_SCHEMAS[:]

    def test_sibling_ref_is_inlined(self) -> None:
        root = load(
            """
            components:
              schemas:
                FieldValue:
                  description: the referenced text
                  type: [string, "null"]
                RevealRequest:
                  type: object
                  properties:
                    field:
                      $ref: "#/components/schemas/FieldValue"
                      description: the sibling wins
                      minLength: 1
            """
        )
        fixed = convert.fix_schema(
            root["components"]["schemas"]["RevealRequest"],
            "#/components/schemas/RevealRequest",
            root,
        )
        field = fixed["properties"]["field"]
        # The pointer resolved (no KeyError), the target's keywords are copied,
        # and the sibling overlays it.
        self.assertEqual(field["description"], "the sibling wins")
        self.assertEqual(field["minLength"], 1)
        self.assertEqual(field["type"], "string")
        self.assertTrue(field["nullable"])
        self.assertNotIn("$ref", field)
        # Inlining is logged, so a reviewer sees the contract being rewritten.
        self.assertIn(
            "$ref sibling inlined at #/components/schemas/RevealRequest.field",
            convert.DROPPED,
        )

    def test_plain_ref_is_not_resolved_or_rewritten(self) -> None:
        # 3.0 keeps a lone $ref as-is; resolving it here would inline the whole
        # schema tree and change what the generators see.
        root = load(
            """
            components:
              schemas:
                Identifier:
                  type: string
                LoginRequest:
                  type: object
                  properties:
                    doctype:
                      $ref: "#/components/schemas/Identifier"
            """
        )
        fixed = convert.fix_schema(
            root["components"]["schemas"]["LoginRequest"],
            "#/components/schemas/LoginRequest",
            root,
        )
        self.assertEqual(
            fixed["properties"]["doctype"], {"$ref": "#/components/schemas/Identifier"}
        )
        self.assertEqual(convert.DROPPED, [])

    def test_escaped_pointer_in_a_sibling_ref(self) -> None:
        # A '~1' escape resolves to a literal '/' key, which is how the real
        # contract's path items are addressed from a pointer.
        root = load(
            """
            components:
              schemas:
                "/api/v1/x":
                  type: string
                  pattern: "^x$"
                Wrapper:
                  type: object
                  properties:
                    path:
                      $ref: "#/components/schemas/~1api~1v1~1x"
                      description: escaped
            """
        )
        fixed = convert.fix_schema(
            root["components"]["schemas"]["Wrapper"], "#/components/schemas/Wrapper", root
        )
        self.assertEqual(fixed["properties"]["path"]["pattern"], "^x$")
        self.assertEqual(fixed["properties"]["path"]["description"], "escaped")


class FoldRequestBody(unittest.TestCase):
    """progenitor types exactly one request-body media type per operation."""

    def setUp(self) -> None:
        del convert.DROPPED[:]
        del convert.TRANSLATED[:]

    def test_two_media_types_keep_the_one_progenitor_can_type(self) -> None:
        body = load(
            """
            required: true
            content:
              multipart/form-data:
                schema:
                  type: object
                  properties:
                    file: {type: string, format: binary}
              application/octet-stream:
                schema: {type: string, format: binary}
            """
        )
        convert.fold_request_body(body, "paths:/api/v1/files:post")
        self.assertEqual(list(body["content"]), ["application/octet-stream"])
        self.assertEqual(len(convert.DROPPED), 1)
        self.assertIn("multipart/form-data", convert.DROPPED[0])
        self.assertIn("paths:/api/v1/files:post", convert.DROPPED[0])

    def test_single_media_type_is_untouched(self) -> None:
        body = {"content": {"application/json": {"schema": {"type": "object"}}}}
        convert.fold_request_body(body, "paths:/x:post")
        self.assertEqual(list(body["content"]), ["application/json"])
        self.assertEqual(convert.DROPPED, [])

    def test_untypable_only_media_type_is_left_for_progenitor_to_report(self) -> None:
        # Dropping the only media type would silently remove the parameter, so
        # the body is left alone and progenitor's own error is the loud one.
        body = {"content": {"multipart/form-data": {"schema": {"type": "object"}}}}
        convert.fold_request_body(body, "paths:/x:post")
        self.assertEqual(list(body["content"]), ["multipart/form-data"])
        self.assertEqual(convert.DROPPED, [])

    def test_missing_or_malformed_body_is_ignored(self) -> None:
        for body in (None, {}, {"content": None}, "not a body"):
            with self.subTest(body=body):
                convert.fold_request_body(body, "paths:/x:post")
        self.assertEqual(convert.DROPPED, [])


if __name__ == "__main__":
    unittest.main()
