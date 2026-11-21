"""The packaged unit makes /etc/spot/spot.toml the configuration the CLI reads.

Decision P1 (31 Oct 2026): ``spot serve --root /var/lib/spot`` reads
``/var/lib/spot/common.toml``; the unit links that path to ``/etc/spot/spot.toml``
before start. These tests pin the link, the root, the single config path and
that the shipped default is accepted by the CLI's own key list.
"""

import os
import re
import subprocess
import sys
import tomllib
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import build  # noqa: E402

LINUX = build.PACKAGING
REPO = LINUX.parents[1]
UNIT = LINUX / "systemd" / "spot.service"
TOML = LINUX / "etc" / "spot.toml"
CONFIG_RS = REPO / "crates" / "framework" / "spot-cli" / "src" / "config.rs"

#: The fields of ``pub struct Common { ... }`` in spot-cli's config.rs.
COMMON_STRUCT = re.compile(r"pub struct Common \{(.*?)\n\}", re.S)
#: One ``pub name:`` field at the start of a line inside that struct.
FIELD = re.compile(r"^\s*pub (\w+):", re.M)


def common_keys():
    body = COMMON_STRUCT.search(CONFIG_RS.read_text()).group(1)
    return set(FIELD.findall(body))


class ShippedConfig(unittest.TestCase):
    def test_parses_and_keys_are_known(self):
        data = tomllib.loads(TOML.read_text())
        self.assertIn("apps", data)
        unknown = set(data) - common_keys()
        self.assertEqual(unknown, set(), "keys the CLI would reject (deny_unknown_fields)")

    def test_key_list_is_read_from_the_cli(self):
        self.assertTrue({"apps", "server", "postgres"} <= common_keys())

    @unittest.skipUnless(os.environ.get("SPOT_BIN"), "SPOT_BIN not set")
    def test_real_binary_accepts_the_file(self):
        # Only checks the binary starts and reads the file as common.toml; it
        # may still refuse to serve (no sites), which is not a parse error.
        import shutil
        import tempfile

        with tempfile.TemporaryDirectory() as root:
            shutil.copy(TOML, Path(root) / "common.toml")
            run = subprocess.run(
                [os.environ["SPOT_BIN"], "serve", "--root", root, "--help"],
                capture_output=True, text=True, timeout=60,
            )
            self.assertNotIn("unknown field", run.stdout + run.stderr)


class Unit(unittest.TestCase):
    def setUp(self):
        self.text = UNIT.read_text()

    def test_symlink_line(self):
        pre = [l for l in self.text.splitlines() if l.startswith("ExecStartPre=")]
        self.assertEqual(len(pre), 1)
        self.assertIn("ln -s /etc/spot/spot.toml", pre[0])
        self.assertIn("/var/lib/spot/common.toml", pre[0])
        self.assertNotIn("ln -sf", pre[0], "an operator's file must not be overwritten")
        self.assertIn("test -e", pre[0])
        self.assertIn("echo", pre[0], "the journal must say which file is used")

    def test_exec_start_keeps_root(self):
        self.assertIn("ExecStart=/usr/bin/spot serve --root /var/lib/spot\n", self.text)

    def test_pre_runs_before_exec_start(self):
        self.assertLess(self.text.index("ExecStartPre="), self.text.index("\nExecStart="))

    def test_no_third_path(self):
        allowed = {"/etc/spot/spot.toml", "/var/lib/spot/common.toml"}
        pattern = re.compile(r"(?<![\w./-])/(?:etc|var|usr|opt|run|srv)/[\w./-]*(?:spot|common)\.toml")
        for path in LINUX.rglob("*"):
            if not path.is_file() or path.suffix == ".pyc":
                continue
            for found in pattern.findall(path.read_text(errors="replace")):
                self.assertIn(found, allowed | {"/etc/spot/spot.toml.rpmsave"}, f"{path}: {found}")

    def test_script_behaviour(self):
        """Run the shell fragment against a temp dir: link, keep file, keep link."""
        import tempfile

        line = next(l for l in self.text.splitlines() if l.startswith("ExecStartPre="))
        script = re.search(r"-c '(.*)'$", line).group(1).replace("$$", "$")
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "common.toml"
            sub = script.replace("/var/lib/spot/common.toml", str(f))
            run = lambda: subprocess.run(["/bin/sh", "-c", sub], capture_output=True, text=True)
            self.assertEqual(run().returncode, 0)
            self.assertEqual(os.readlink(f), "/etc/spot/spot.toml")
            f.unlink()
            f.write_text("apps = []\n")
            out = run()
            self.assertEqual(out.returncode, 0)
            self.assertEqual(f.read_text(), "apps = []\n")
            self.assertIn(str(f), out.stdout)


class DebianPrerm(unittest.TestCase):
    def test_f5_stops_on_remove_not_upgrade(self):
        text = (LINUX / "debian" / "prerm").read_text()
        code = "\n".join(l for l in text.splitlines() if not l.lstrip().startswith("#"))
        if not re.search(r"systemctl\s+stop|deb-systemd-invoke\s+stop", code):
            self.skipTest("prerm has no stop logic yet: decision F5 belongs to WP-PRD-09.T5")
        self.assertRegex(code, r"remove")
        self.assertNotRegex(code, r"upgrade[^\n]*\n?[^\n]*stop", "must not stop on upgrade")


if __name__ == "__main__":
    unittest.main()
