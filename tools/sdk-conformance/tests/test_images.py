"""Unit tests for image-configuration parsing (WP-STR-07.T1). No containers needed."""

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import images  # noqa: E402

HERE = Path(__file__).resolve().parent.parent
REAL_IMAGES = HERE / "images.toml"

VALID = """
[harness]
runner = "runner/target/release/spot-sdk-conformance"
cases = "cases.json"
component = "out/component.wasm"
wasm_tools_version = "1.262.0"

[languages.rust]
image = "docker.io/library/rust:1.98-slim"
toolchain = "rustc 1.98"
provision = ""
build = "./build.sh"

[languages.go]
image = "docker.io/library/golang@sha256:{digest}"
toolchain = "Go 1.27"
provision = ""
build = "./build.sh"

[languages.csharp]
image = "mcr.microsoft.com/dotnet/sdk:10.0"
toolchain = ".NET 10"
provision = ""
build = "./build.sh"

[languages.javascript]
image = "docker.io/library/node:26-bookworm"
toolchain = "Node 26"
provision = ""
build = "./build.sh"

[languages.python]
image = "docker.io/library/python:3.14-slim"
toolchain = "CPython 3.14"
provision = "pip install componentize-py"
build = "./build.sh"
""".format(digest="a" * 64)


def write(text: str) -> Path:
    """Writes TOML to a temporary file and returns its path."""
    handle = tempfile.NamedTemporaryFile("w", suffix=".toml", delete=False)
    handle.write(text)
    handle.close()
    return Path(handle.name)


class PinningTests(unittest.TestCase):
    def test_a_version_tag_is_pinned(self):
        self.assertTrue(images.is_pinned("docker.io/library/rust:1.98-slim"))

    def test_a_digest_is_pinned(self):
        self.assertTrue(images.is_pinned("rust@sha256:" + "0" * 64))

    def test_latest_is_not_pinned(self):
        self.assertFalse(images.is_pinned("rust:latest"))

    def test_no_tag_is_not_pinned(self):
        self.assertFalse(images.is_pinned("rust"))

    def test_a_short_digest_is_not_pinned(self):
        self.assertFalse(images.is_pinned("rust@sha256:abc"))

    def test_an_uppercase_digest_is_not_pinned(self):
        self.assertFalse(images.is_pinned("rust@sha256:" + "A" * 64))

    def test_an_entry_reports_its_parts(self):
        entry = images.LanguageImage(
            language="go",
            image="golang@sha256:" + "b" * 64,
            toolchain="",
            provision="",
            build="x",
        )
        self.assertEqual(entry.digest, "b" * 64)
        self.assertEqual(entry.tag, "")
        self.assertTrue(entry.pinned)


class LoadingTests(unittest.TestCase):
    def test_reads_the_real_configuration(self):
        config, entries = images.load_images(REAL_IMAGES)
        self.assertEqual(config.cases, "cases.json")
        names = [entry.language for entry in entries]
        for language in images.KNOWN_LANGUAGES:
            self.assertIn(language, names)
        for entry in entries:
            self.assertTrue(entry.pinned, entry.image)

    def test_reads_a_valid_configuration(self):
        config, entries = images.load_images(write(VALID))
        self.assertEqual(config.runner, "runner/target/release/spot-sdk-conformance")
        self.assertEqual([entry.language for entry in entries][:2], ["rust", "go"])

    def test_rejects_an_unpinned_image(self):
        text = VALID.replace("docker.io/library/rust:1.98-slim", "docker.io/library/rust:latest")
        with self.assertRaises(images.ImagesError):
            images.load_images(write(text))

    def test_rejects_a_missing_field(self):
        text = VALID.replace('toolchain = "Go 1.27"\n', "")
        with self.assertRaises(images.ImagesError):
            images.load_images(write(text))

    def test_rejects_a_missing_harness_table(self):
        text = VALID.split("[languages.rust]", 1)[1]
        with self.assertRaises(images.ImagesError):
            images.load_images(write("[languages.rust]" + text))

    def test_rejects_a_missing_shipped_language(self):
        text = VALID.split("[languages.python]", 1)[0]
        with self.assertRaises(images.ImagesError):
            images.load_images(write(text))

    def test_rejects_missing_file(self):
        with self.assertRaises(images.ImagesError):
            images.load_images(Path("/nonexistent/images.toml"))

    def test_by_language_keys_the_entries(self):
        _, entries = images.load_images(write(VALID))
        keyed = images.by_language(entries)
        self.assertEqual(keyed["rust"].language, "rust")


if __name__ == "__main__":
    unittest.main()
