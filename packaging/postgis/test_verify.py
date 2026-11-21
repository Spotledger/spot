#!/usr/bin/env python3
"""Unit tests of `verify.py` (WP-PRD-02.T7).

Run from the repository root:

    python3 -m unittest discover -s packaging/postgis -p 'test_*.py'

The cases are the ones the task brief names — the verifier accepts a good manifest and
rejects a wrong hash and a missing platform — plus the shapes around them (a file that is
not on disk, an unpinned hash, a malformed role). Every hash here is computed from bytes the
test itself writes, so nothing in this file asserts a real download's value.

CI does not run this yet (the workflow files are outside this task's owned paths); the
verifier and its test are plain standard-library Python 3.11+.
"""

from __future__ import annotations

import hashlib
import tempfile
import unittest
from pathlib import Path

import verify


def _file_entry(path: str, data: bytes, component: str = "postgis", role: str = "library") -> dict:
    return {
        "component": component,
        "path": path,
        "role": role,
        "size_bytes": len(data),
        "sha256": hashlib.sha256(data).hexdigest(),
    }


def _manifest(entries: list[dict], *, platform_id: str = "windows-x86_64") -> dict:
    return {
        "schema": 1,
        "min_version": "3.0",
        "bundle": {"required_platforms": [platform_id], "prune": ["gdal-data/**"]},
        "platform": [
            {
                "id": platform_id,
                "os": "windows",
                "arch": "x86_64",
                "install_prefix": "pgsql",
                "component": [
                    {
                        "name": "postgis",
                        "version": "3.6.2",
                        "upstream": "https://postgis.net/",
                        "url": "https://download.osgeo.org/postgis/windows/pg18/postgis-bundle-pg18-3.6.2x64.zip",
                        "archive": "postgis-bundle-pg18-3.6.2x64.zip",
                        "extract_prefix": "postgis-bundle-pg18-3.6.2x64",
                        "size_bytes": 124037332,
                        "sha256": hashlib.sha256(b"archive stand-in").hexdigest(),
                    }
                ],
                "file": entries,
            }
        ],
    }


class Workspace:
    """A temp dir holding `MANIFEST.toml` and an unpacked prefix."""

    def __init__(self, data: bytes = b"staged PostGIS module") -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.prefix = self.root / "pgsql"
        self.target = self.prefix / "lib" / "postgis-3.dll"
        self.target.parent.mkdir(parents=True)
        self.target.write_bytes(data)
        self.data = data
        self.manifest = self.root / "MANIFEST.toml"

    def write(self, text: str) -> Path:
        self.manifest.write_text(text, encoding="utf-8")
        return self.manifest

    def close(self) -> None:
        self._tmp.cleanup()


def _toml(manifest: dict) -> str:
    """A minimal TOML rendering of the test manifest (no external writer in the stdlib)."""
    lines = [
        f"schema = {manifest['schema']}",
        f'min_version = "{manifest["min_version"]}"',
        "[bundle]",
        "required_platforms = ["
        + ", ".join(f'"{item}"' for item in manifest["bundle"]["required_platforms"])
        + "]",
        "prune = [" + ", ".join(f'"{item}"' for item in manifest["bundle"]["prune"]) + "]",
    ]
    for platform in manifest["platform"]:
        lines += [
            "[[platform]]",
            f'id = "{platform["id"]}"',
            f'os = "{platform["os"]}"',
            f'arch = "{platform["arch"]}"',
            f'install_prefix = "{platform["install_prefix"]}"',
        ]
        for component in platform["component"]:
            lines += ["[[platform.component]]"]
            for key in (
                "name",
                "version",
                "upstream",
                "url",
                "archive",
                "extract_prefix",
            ):
                lines.append(f'{key} = "{component[key]}"')
            lines.append(f"size_bytes = {component['size_bytes']}")
            lines.append(f'sha256 = "{component["sha256"]}"')
        for entry in platform["file"]:
            lines += ["[[platform.file]]"]
            for key in ("component", "path", "role", "sha256"):
                if key in entry:
                    lines.append(f'{key} = "{entry[key]}"')
            lines.append(f"size_bytes = {entry['size_bytes']}")
    return "\n".join(lines) + "\n"


class VerifyTest(unittest.TestCase):
    def setUp(self) -> None:
        self.workspace = Workspace()
        self.addCleanup(self.workspace.close)

    def build(self, entries: list[dict] | None = None, **kwargs) -> Path:
        entries = entries if entries is not None else [_file_entry("lib/postgis-3.dll", self.workspace.data)]
        return self.workspace.write(_toml(_manifest(entries, **kwargs)))

    # -- the good case ---------------------------------------------------------------

    def test_a_good_manifest_is_valid(self) -> None:
        manifest = self.build()
        self.assertEqual(verify.main(["--manifest", str(manifest)]), 0)

    def test_a_good_manifest_with_a_staged_prefix_matches(self) -> None:
        manifest = self.build()
        code = verify.main(
            ["--manifest", str(manifest), "--files", str(self.workspace.prefix)]
        )
        self.assertEqual(code, 0)

    # -- the rejections the brief names ------------------------------------------------

    def test_a_wrong_hash_is_rejected(self) -> None:
        entry = _file_entry("lib/postgis-3.dll", self.workspace.data)
        entry["sha256"] = hashlib.sha256(b"some other build").hexdigest()
        manifest = self.build([entry])
        report = verify.verify(manifest, self.workspace.prefix)
        self.assertEqual(report.exit_code, 1)
        self.assertTrue(
            any("mismatch" in error for error in report.errors), report.errors
        )

    def test_a_missing_platform_is_rejected(self) -> None:
        text = _toml(_manifest([_file_entry("lib/postgis-3.dll", self.workspace.data)]))
        # The bundle requires two platforms but only one is declared: a silently missing
        # install, which is exactly what the check exists for.
        text = text.replace(
            'required_platforms = ["windows-x86_64"]',
            'required_platforms = ["windows-x86_64", "linux-x86_64"]',
        )
        manifest = self.workspace.write(text)
        report = verify.verify(manifest)
        self.assertEqual(report.exit_code, 1)
        self.assertTrue(
            any("required platform" in error and "linux-x86_64" in error for error in report.errors),
            report.errors,
        )

    # -- the shapes around them -------------------------------------------------------

    def test_a_file_missing_from_the_prefix_is_rejected(self) -> None:
        self.build()
        (self.workspace.prefix / "lib" / "postgis-3.dll").unlink()
        report = verify.verify(self.workspace.manifest, self.workspace.prefix)
        self.assertEqual(report.exit_code, 1)
        self.assertTrue(any("missing" in error for error in report.errors), report.errors)

    def test_an_unpinned_hash_is_not_shippable(self) -> None:
        entry = _file_entry("lib/postgis-3.dll", self.workspace.data)
        entry["sha256"] = verify.TODO
        manifest = self.build([entry])
        report = verify.verify(manifest, self.workspace.prefix)
        self.assertEqual(report.exit_code, 2, report.errors)
        self.assertTrue(any("not pinned" in item for item in report.unpinned), report.unpinned)

    def test_a_bad_role_is_rejected(self) -> None:
        entry = _file_entry("lib/postgis-3.dll", self.workspace.data, role="binary")
        manifest = self.build([entry])
        self.assertEqual(verify.verify(manifest).exit_code, 1)

    def test_a_path_outside_the_prefix_is_rejected(self) -> None:
        entry = _file_entry("../elsewhere/postgis-3.dll", self.workspace.data)
        manifest = self.build([entry])
        report = verify.verify(manifest)
        self.assertEqual(report.exit_code, 1)
        self.assertTrue(any("relative path" in error for error in report.errors), report.errors)

    def test_a_required_file_of_the_platform_must_exist(self) -> None:
        # An empty [[platform.file]] list is a platform that ships nothing.
        manifest = self.workspace.write(
            _toml(_manifest([])).replace("prune = [", "prune = [")
        )
        report = verify.verify(manifest)
        self.assertEqual(report.exit_code, 1)

    def test_a_malformed_manifest_is_reported_not_raised(self) -> None:
        manifest = self.workspace.write("schema = 1\nplatform = []\n")
        report = verify.verify(manifest)
        self.assertEqual(report.exit_code, 1)
        self.assertTrue(any("platform" in error for error in report.errors), report.errors)


class CommittedManifestTest(unittest.TestCase):
    """The manifest committed beside this test must pass its own shape check."""

    def test_the_committed_manifest_is_valid(self) -> None:
        manifest = Path(__file__).with_name("MANIFEST.toml")
        report = verify.verify(manifest)
        self.assertEqual(report.lines(), [], "the committed MANIFEST.toml has findings")
        self.assertEqual(report.exit_code, 0)


if __name__ == "__main__":
    unittest.main()
