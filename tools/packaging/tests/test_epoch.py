"""The build epoch: one fixed default that rpm, dpkg-deb and tar all accept.

rpm 6 takes the newest ``%changelog`` entry as the package build time and refuses
a date outside the window its parser accepts, with ``bad date in %changelog``.
The default epoch used to be 1980-01-01, the earliest portable tar mtime, and
that alone made ``build.py rpm`` fail. These tests pin the replacement: a fixed
modern date inside rpm's window, a changelog date whose weekday label agrees with
the date it is attached to, byte-identical text for a repeated build, and a
refusal that names ``SOURCE_DATE_EPOCH`` before rpmbuild is started.
"""

import calendar
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build  # noqa: E402

DAYS = ("Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun")
BINARY_BYTES = b"#!/bin/sh\necho spot\n"
OLD_DEFAULT_EPOCH = 315532800  # 1980-01-01, what rpm 6 refuses


def changelog_stamp(text):
    """The date part of the single ``%changelog`` entry in a rendered spec."""
    entries = [line for line in text.splitlines() if line.startswith("* ")]
    if len(entries) != 1:
        raise AssertionError(f"expected one changelog entry, found {len(entries)}")
    stamp = entries[0][2:].split(" Spotledger")[0]
    return stamp


def stamp_as_day(stamp):
    """The calendar day a ``%a %b %d %Y`` stamp names, in seconds, ignoring the label."""
    parsed = time.strptime(stamp, "%a %b %d %Y")
    return calendar.timegm((parsed.tm_year, parsed.tm_mon, parsed.tm_mday, 0, 0, 0))


def stamped_weekday(stamp):
    """The weekday *label* of a stamp, as parsed from the label itself."""
    return time.strptime(stamp, "%a %b %d %Y").tm_wday


class DefaultEpochTest(unittest.TestCase):
    def test_the_default_epoch_renders_a_changelog_date_with_its_own_weekday(self):
        text, _, _ = build.render_spec("1.2.3", build.DEFAULT_EPOCH)
        stamp = changelog_stamp(text)
        # strptime takes the weekday from the label and the day from the rest, so a
        # label that disagrees with its date is caught instead of accepted.
        labelled = stamped_weekday(stamp)
        actual = time.gmtime(stamp_as_day(stamp)).tm_wday
        self.assertEqual(
            labelled, actual,
            f"{stamp!r} says {DAYS[labelled]} but {time.strftime('%Y-%m-%d', time.gmtime(stamp_as_day(stamp)))} "
            f"is a {DAYS[actual]}",
        )

    def test_the_default_epoch_names_the_day_it_was_chosen_for(self):
        text, _, _ = build.render_spec("1.2.3", build.DEFAULT_EPOCH)
        self.assertEqual(stamp_as_day(changelog_stamp(text)),
                         build.DEFAULT_EPOCH - build.DEFAULT_EPOCH % 86400)

    def test_the_default_epoch_is_inside_the_window_every_tool_accepts(self):
        self.assertGreaterEqual(build.DEFAULT_EPOCH, build.RPM_CHANGELOG_MIN_EPOCH)
        self.assertLessEqual(build.DEFAULT_EPOCH, build.RPM_CHANGELOG_MAX_EPOCH)
        self.assertLessEqual(build.DEFAULT_EPOCH, build.MAX_EPOCH)
        self.assertNotEqual(build.DEFAULT_EPOCH, OLD_DEFAULT_EPOCH)

    def test_the_default_epoch_does_not_read_the_clock(self):
        # The defect this replaces was a constant, so the guard is about the new
        # value: rendering must not consult the wall clock at all.
        with mock.patch.object(build.time, "time", side_effect=AssertionError("clock read")):
            text, _, _ = build.render_spec("1.2.3", build.DEFAULT_EPOCH)
        self.assertEqual(stamp_as_day(changelog_stamp(text)),
                         build.DEFAULT_EPOCH - build.DEFAULT_EPOCH % 86400)

    def test_two_builds_with_the_same_epoch_render_identical_changelog_text(self):
        first = build.render_spec("1.2.3", build.DEFAULT_EPOCH)[0]
        second = build.render_spec("1.2.3", build.DEFAULT_EPOCH)[0]
        self.assertEqual(first, second)
        self.assertEqual(changelog_stamp(first), changelog_stamp(second))

    def test_a_caller_epoch_is_honoured(self):
        epoch = 1700000000
        with mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": str(epoch)}):
            self.assertEqual(build.source_date_epoch(), epoch)
        text, _, _ = build.render_spec("1.2.3", epoch)
        self.assertEqual(stamp_as_day(changelog_stamp(text)), epoch - epoch % 86400)

    def test_an_unset_or_empty_variable_falls_back_to_the_default(self):
        for raw in (None, "", "   "):
            with self.subTest(value=raw):
                environment = {key: value for key, value in os.environ.items()
                               if key != "SOURCE_DATE_EPOCH"}
                if raw is not None:
                    environment["SOURCE_DATE_EPOCH"] = raw
                with mock.patch.dict(os.environ, environment, clear=True):
                    self.assertEqual(build.source_date_epoch(), build.DEFAULT_EPOCH)


class RefusedEpochTest(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="spot-epoch-")
        self.tmp = Path(self._tmp.name)
        self.binary = self.tmp / "spot"
        self.binary.write_bytes(BINARY_BYTES)
        self.binary.chmod(0o755)

    def tearDown(self):
        self._tmp.cleanup()

    def test_a_negative_or_non_integer_value_is_refused_by_name(self):
        for raw in ("-1", "-1767225600", "1.5", "abc", "0x10"):
            with self.subTest(value=raw), mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": raw}):
                with self.assertRaises(SystemExit) as raised:
                    build.source_date_epoch()
                self.assertIn("SOURCE_DATE_EPOCH", str(raised.exception))

    def test_a_value_too_large_for_tar_is_refused_by_name(self):
        for raw in (str(build.MAX_EPOCH + 1), "99999999999999"):
            with self.subTest(value=raw), mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": raw}):
                with self.assertRaises(SystemExit) as raised:
                    build.source_date_epoch()
                self.assertIn("SOURCE_DATE_EPOCH", str(raised.exception))

    def test_an_epoch_outside_the_rpm_window_is_refused_by_name(self):
        for epoch in (OLD_DEFAULT_EPOCH, build.RPM_CHANGELOG_MIN_EPOCH - 1,
                      build.RPM_CHANGELOG_MAX_EPOCH + 1):
            with self.subTest(epoch=epoch):
                with self.assertRaises(SystemExit) as raised:
                    build.render_spec("1.2.3", epoch)
                message = str(raised.exception)
                self.assertIn("SOURCE_DATE_EPOCH", message)
                self.assertIn(str(epoch), message)
                self.assertIn(build.changelog_date(epoch), message)

    def test_the_edges_of_the_window_are_accepted(self):
        for epoch in (build.RPM_CHANGELOG_MIN_EPOCH, build.RPM_CHANGELOG_MAX_EPOCH):
            with self.subTest(epoch=epoch):
                text, _, _ = build.render_spec("1.2.3", epoch)
                self.assertIn("Spotledger Packaging", text)

    def test_the_old_1980_default_is_refused_before_rpmbuild_runs(self):
        out = self.tmp / "out"
        with mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": str(OLD_DEFAULT_EPOCH)}), \
                mock.patch.object(build.shutil, "which", return_value="/usr/bin/rpmbuild"), \
                mock.patch.object(build.subprocess, "run") as run:
            with self.assertRaises(SystemExit) as raised:
                build.build("rpm", self.binary, "1.2.3", out, "x86_64")
        run.assert_not_called()
        message = str(raised.exception)
        self.assertIn("SOURCE_DATE_EPOCH", message)
        self.assertIn("1980", message)
        self.assertNotIn("bad date in", message)


if __name__ == "__main__":
    unittest.main()
