"""Source-level checks of src/measure.mjs (WP-UI-09.T9): no browser is needed to run them."""

from __future__ import annotations

import re
import unittest
from pathlib import Path

SOURCE = (Path(__file__).resolve().parent.parent / "src" / "measure.mjs").read_text(encoding="utf-8")


def body(name: str) -> str:
    start = SOURCE.index(name)
    return SOURCE[start:start + 1500]


class SignInPaths(unittest.TestCase):
    def test_hook_is_tried_before_the_keyboard(self):
        ensure = body("async function ensureSignedIn")
        self.assertLess(ensure.index("signInThroughHook"), ensure.index("signInThroughShell"))
        self.assertIn("'hook'", ensure)
        self.assertIn("'keyboard'", ensure)

    def test_hook_checks_the_function_and_falls_back_on_false(self):
        hook = body("async function signInThroughHook")
        self.assertIn("typeof shell.signIn !== 'function'", hook)
        self.assertIn("=== true", hook)

    def test_credentials_come_from_the_stub_module(self):
        self.assertIn("TEST_USER, TEST_PASSWORD } from '../stub-backend/server.mjs'", SOURCE)
        self.assertIsNone(re.search(r"signIn\([^)]*['\"]", SOURCE.replace("signIn(url, user, password)", "")))

    def test_result_records_the_method(self):
        self.assertIn("result.signInMethod = await ensureSignedIn", SOURCE)

    def test_stub_pages_carry_the_test_sign_in_flag(self):
        self.assertIn("searchParams.set('testSignIn', '1')", SOURCE)
        self.assertNotIn("page.goto(baseUrl", SOURCE)


class MemoryOrder(unittest.TestCase):
    def test_short_tab_count_fails_with_the_documented_message(self):
        self.assertIn("`memory: opened ${opened} of ${wanted} tabs`", SOURCE)
        run_memory = body("async function runMemory")
        self.assertIn("memory.opened < TABS", run_memory)
        self.assertLess(run_memory.index("shortTabsMessage"), run_memory.index("memory.total"))

    def test_memory_runs_after_the_signed_in_operations(self):
        run_once = SOURCE[SOURCE.index("async function runOnce"):]
        self.assertGreater(run_once.index("await runMemory"), run_once.index("browser.child-edit.key'] = latencies"))
        self.assertEqual(run_once.count("await runMemory"), 1)

    def test_signed_out_real_build_skips_memory(self):
        self.assertIn("} else if (stub || fixture) {", SOURCE)


if __name__ == "__main__":
    unittest.main()
