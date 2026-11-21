"""The RPM spec may not name a macro inside a comment.

rpm expands macro references everywhere it reads a line, comments included. A
comment that spells out ``%systemd_post`` is therefore expanded with no
arguments and aborts the build

    error: The %systemd_post macro requires some arguments

which is exactly what happened on Fedora 44 (rpm 6) and AlmaLinux 10 before this
test existed. The rule is: a comment line may mention a macro only as the
escaped ``%%name`` form, which rpm renders as a literal percent sign.
"""

import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build  # noqa: E402

SPEC = build.PACKAGING / "rpm" / "spot.spec"

#: A macro reference starts with ``%`` and continues with a name or ``{...}``.
MACRO_BODY = re.compile(r"[A-Za-z_][A-Za-z0-9_]*|\{[^}]*\}")


def comment_lines(text):
    """Yield ``(line number, line)`` for every line rpm reads as a comment."""
    for number, line in enumerate(text.splitlines(), start=1):
        if line.lstrip().startswith("#"):
            yield number, line


def macro_references(line):
    """Yield each unescaped ``%name`` / ``%{name}`` reference in one line.

    ``%%`` is rpm's escape and is skipped as a pair, so ``%%systemd_post`` is
    not a reference. ``%`` followed by anything else (a percent sign in prose,
    ``%{?dist}`` is a reference, ``%post`` is a reference) is reported.
    """
    index = 0
    while index < len(line):
        if line[index] != "%":
            index += 1
            continue
        if line[index + 1:index + 2] == "%":
            index += 2
            continue
        match = MACRO_BODY.match(line, index + 1)
        if match:
            yield "%" + match.group(0)
            index = match.end()
        else:
            index += 1


def violations(text):
    """Every unescaped macro reference in a comment, as ``line N: %name``."""
    found = []
    for number, line in comment_lines(text):
        for reference in macro_references(line):
            found.append(f"line {number}: {reference}")
    return found


class SpecCommentTest(unittest.TestCase):
    def setUp(self):
        self.text = SPEC.read_text(encoding="utf-8")

    def test_template_has_no_macro_reference_in_a_comment(self):
        found = violations(self.text)
        self.assertEqual(
            found,
            [],
            "packaging/linux/rpm/spot.spec names a macro inside a comment; rpm "
            "expands it with no arguments and fails the build. Escape it as "
            "%%name or say it in words:\n  " + "\n  ".join(found),
        )

    def test_rendered_spec_has_no_macro_reference_in_a_comment(self):
        rendered, _, _ = build.render_spec("1.2.3", build.DEFAULT_EPOCH)
        self.assertEqual(violations(rendered), [])

    def test_the_historical_scriptlet_macros_are_not_named_in_a_comment(self):
        for name in ("%systemd_post", "%systemd_preun",
                     "%systemd_postun_with_restart", "%sysusers_create_compat"):
            with self.subTest(macro=name):
                hits = [item for item in violations(self.text) if item.endswith(" " + name)]
                self.assertEqual(hits, [], f"{name} is named in a comment: {hits}")

    def test_the_scriptlets_still_call_them(self):
        # The fix is about comments only: the sections themselves must keep working.
        for call in ("%sysusers_create_compat %{SOURCE1}",
                     "%systemd_post spot.service",
                     "%systemd_preun spot.service",
                     "%systemd_postun_with_restart spot.service"):
            with self.subTest(call=call):
                self.assertIn(call, self.text)

    def test_the_scanner_catches_a_planted_reference(self):
        # A test that can never fail proves nothing: plant the defect the test exists
        # for and check that the scanner reports its line number.
        planted = "# An old note\n# calls %systemd_post with nothing\nName: spot\n"
        self.assertEqual(violations(planted),
                         ["line 2: %systemd_post"])
        self.assertEqual(violations("# use %{name} here\n"), ["line 1: %{name}"])
        self.assertEqual(violations("# a section is a macro too: %post\n"),
                         ["line 1: %post"])

    def test_the_scanner_accepts_escapes_and_plain_percent_signs(self):
        self.assertEqual(violations("# write %%systemd_post to mention it\n"), [])
        self.assertEqual(violations("\t# 100% of the time\n"), [])
        self.assertEqual(violations("# 50%% done\n"), [])
        # A macro outside a comment is what the spec is for.
        self.assertEqual(violations("%global debug_package %{nil}\n"), [])
        self.assertEqual(violations("Release: 1%{?dist}\n"), [])


if __name__ == "__main__":
    unittest.main()
