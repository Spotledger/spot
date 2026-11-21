import contextlib
import io
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import tag_gate  # noqa: E402

GIT_ENV = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@x", "GIT_COMMITTER_NAME": "t",
           "GIT_COMMITTER_EMAIL": "t@x", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull}


def g(cwd, *a, env=None):
    return subprocess.run(["git", "-C", str(cwd), *a], check=True, capture_output=True, text=True,
                          env=env or GIT_ENV).stdout.strip()


def ok_http(sha):
    def f(url, token):
        assert sha in url
        return {"workflow_runs": [{"id": 1, "head_sha": sha, "status": "completed", "conclusion": "success",
                                   "created_at": "2026-01-01", "html_url": "u"}]}
    return f


def run(argv, http=None):
    out = io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
        code = tag_gate.main(argv, http=http) if http else tag_gate.main(argv)
    return code, out.getvalue()


class Base(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp, True)
        t = Path(self.tmp)
        self.origin = t / "origin.git"
        g(t, "init", "--bare", "-b", "main", str(self.origin))
        self.work = t / "work"
        g(t, "clone", str(self.origin), str(self.work))
        g(self.work, "checkout", "-b", "main")
        (self.work / "a").write_text("1")
        g(self.work, "add", "a")
        g(self.work, "commit", "-m", "one")
        g(self.work, "push", "origin", "main")
        self.sha = g(self.work, "rev-parse", "HEAD")

    def args(self, tag, *extra):
        return ["--tag", tag, "--repo-dir", str(self.work), "--repo", "o/r", "--workflow", "ci.yml", *extra]


class TestShape(unittest.TestCase):
    def test_shapes(self):
        for t in ("v0.1.0", "v1.2.3-rc.1", "v10.0.0-beta"):
            self.assertEqual(tag_gate.check_shape(t), [], t)
        for t in ("0.1.0", "v1.2", "v1.2.3-", "v1.2.3+x", "vx.1.2", "", "v1.2.3\n", "-v1.2.3"):
            self.assertTrue(tag_gate.check_shape(t), t)

    def test_help(self):
        self.assertEqual(run(["--help"])[0], 0)

    def test_usage_error(self):
        self.assertEqual(run([])[0], 2)


class TestGit(Base):
    def test_annotated_on_main_passes_and_unsigned_notice(self):
        g(self.work, "tag", "-a", "v0.1.0", "-m", "r")
        code, out = run(self.args("v0.1.0"), ok_http(self.sha))
        self.assertEqual(code, 0, out)
        self.assertIn("NOTICE tag signature not required", out)

    def test_lightweight_refused(self):
        g(self.work, "tag", "v0.1.0")
        code, out = run(self.args("v0.1.0"), ok_http(self.sha))
        self.assertEqual(code, 1)
        self.assertIn("git tag -a", out)

    def test_missing_tag_refused(self):
        code, _ = run(self.args("v0.9.9"), ok_http(self.sha))
        self.assertEqual(code, 1)

    def test_side_branch_refused(self):
        g(self.work, "checkout", "-b", "side")
        (self.work / "b").write_text("2")
        g(self.work, "add", "b")
        g(self.work, "commit", "-m", "side")
        side = g(self.work, "rev-parse", "HEAD")
        g(self.work, "tag", "-a", "v0.1.0", "-m", "r")
        code, out = run(self.args("v0.1.0"), ok_http(side))
        self.assertEqual(code, 1)
        self.assertIn("not contained", out)

    def test_shallow_refused(self):
        (self.work / "b").write_text("2")
        g(self.work, "add", "b")
        g(self.work, "commit", "-m", "two")
        g(self.work, "push", "origin", "main")
        g(self.work, "tag", "-a", "v0.1.0", "-m", "r")
        sh = Path(self.tmp) / "shallow"
        g(self.tmp, "clone", "--depth", "1", "file://" + str(self.origin), str(sh))
        sha = g(sh, "rev-parse", "HEAD")
        g(sh, "tag", "-a", "v0.1.0", "-m", "r")
        code, out = run(["--tag", "v0.1.0", "--repo-dir", str(sh), "--skip-ci"])
        self.assertEqual(code, 1, out)
        self.assertIn("shallow", out)
        self.assertTrue(sha)

    def test_all_reasons_listed(self):
        g(self.work, "tag", "v0.1.0")
        code, out = run(self.args("v0.1.0"), lambda u, t: {"workflow_runs": []})
        self.assertEqual(code, 1)
        self.assertIn("lightweight", out)

    def test_bad_repo_dir_is_usage_error(self):
        self.assertEqual(run(["--tag", "v0.1.0", "--repo-dir", "/nonexistent-xyz"])[0], 2)


class TestCI(unittest.TestCase):
    sha = "a" * 40

    def check(self, runs):
        return tag_gate.check_ci("o/r", "ci.yml", self.sha, lambda u, t: {"workflow_runs": runs})

    def run_(self, status, conclusion, sha=None, **kw):
        return {"id": kw.get("id", 1), "head_sha": sha or self.sha, "status": status,
                "conclusion": conclusion, "created_at": kw.get("at", "2026-01-01"), "html_url": "http://run/1"}

    def test_success(self):
        self.assertEqual(self.check([self.run_("completed", "success")]), [])

    def test_failure_has_url(self):
        r = self.check([self.run_("completed", "failure")])
        self.assertTrue(r and "http://run/1" in r[0])

    def test_none(self):
        self.assertTrue(self.check([]))

    def test_other_sha_ignored(self):
        self.assertTrue(self.check([self.run_("completed", "success", sha="b" * 40)]))

    def test_in_progress(self):
        self.assertTrue(self.check([self.run_("in_progress", None)]))

    def test_newest_completed_wins(self):
        old_ok = self.run_("completed", "success", id=1, at="2026-01-01")
        new_bad = self.run_("completed", "failure", id=2, at="2026-01-02")
        self.assertTrue(self.check([old_ok, new_bad]))
        self.assertEqual(self.check([self.run_("completed", "failure", id=1, at="2026-01-01"),
                                     self.run_("completed", "success", id=2, at="2026-01-02")]), [])

    def test_api_error_fails_closed(self):
        def boom(u, t):
            raise OSError("down")
        self.assertTrue(tag_gate.check_ci("o/r", "ci.yml", self.sha, boom))

    def test_malformed_fails_closed(self):
        self.assertTrue(tag_gate.check_ci("o/r", "ci.yml", self.sha, lambda u, t: {"x": 1}))

    def test_token_not_printed(self):
        seen = {}

        def http(u, t):
            seen["t"] = t
            raise OSError("secret-token-123 leaked?")
        os.environ["GITHUB_TOKEN"] = "secret-token-123"
        self.addCleanup(os.environ.pop, "GITHUB_TOKEN", None)
        r = tag_gate.check_ci("o/r", "ci.yml", self.sha, http, "secret-token-123")
        self.assertNotIn("secret-token-123", " ".join(r))


@unittest.skipUnless(shutil.which("gpg"), "gpg not installed: signature tests skipped")
class TestSigned(Base):
    def keygen(self, name):
        home = Path(self.tmp) / f"gh-{name}"
        home.mkdir(mode=0o700)
        env = {**GIT_ENV, "GNUPGHOME": str(home)}
        subprocess.run(["gpg", "--batch", "--passphrase", "", "--quick-generate-key", f"{name} <{name}@x>",
                        "default", "default", "never"], check=True, capture_output=True, env=env)
        pub = Path(self.tmp) / f"{name}.asc"
        pub.write_bytes(subprocess.run(["gpg", "--armor", "--export", name], check=True, capture_output=True,
                                       env=env).stdout)
        return env, pub

    def sign(self, env, tag):
        fpr = subprocess.run(["gpg", "--list-keys", "--with-colons"], check=True, capture_output=True, text=True,
                             env=env).stdout
        key = [l.split(":")[9] for l in fpr.splitlines() if l.startswith("fpr")][0]
        g(self.work, "-c", "gpg.program=gpg", "tag", "-u", key, "-m", "r", tag, env=env)

    def test_signed_passes_and_prints_fingerprint(self):
        env, pub = self.keygen("alice")
        self.sign(env, "v0.1.0")
        code, out = run(self.args("v0.1.0", "--require-signed", "--keys", str(pub)), ok_http(self.sha))
        self.assertEqual(code, 0, out)
        self.assertIn("fingerprint", out)

    def test_wrong_key_refused(self):
        env, _ = self.keygen("alice")
        _, other_pub = self.keygen("bob")
        self.sign(env, "v0.1.0")
        code, _ = run(self.args("v0.1.0", "--require-signed", "--keys", str(other_pub)), ok_http(self.sha))
        self.assertEqual(code, 1)

    def test_unsigned_refused_when_required(self):
        _, pub = self.keygen("alice")
        g(self.work, "tag", "-a", "v0.1.0", "-m", "r")
        code, _ = run(self.args("v0.1.0", "--require-signed", "--keys", str(pub)), ok_http(self.sha))
        self.assertEqual(code, 1)

    def test_missing_keys_refused(self):
        g(self.work, "tag", "-a", "v0.1.0", "-m", "r")
        code, _ = run(self.args("v0.1.0", "--require-signed", "--keys", "/nonexistent"), ok_http(self.sha))
        self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
