"""Tests of the control plane packaging (WP-CTL-01.T8). Standard library only.

Run: python3 -m unittest discover -s packaging/linux/control-plane/tests -v
(``verify-control-plane.sh`` runs this.) No compiled binary is needed.
"""

import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

CP = Path(__file__).resolve().parents[1]
REPO = CP.parents[2]
sys.path.insert(0, str(CP))
sys.path.insert(0, str(REPO / "tools" / "packaging"))

import build_control_plane as bcp  # noqa: E402

UNIT = CP / "systemd" / "spot-control-plane.service"
HOST_UNIT = REPO / "packaging" / "linux" / "systemd" / "spot.service"
CONFIG_RS = REPO / "crates" / "framework" / "spot-control-plane" / "src" / "config.rs"
SPEC = CP / "rpm" / "spot-control-plane.spec"
DEBIAN = CP / "debian"


def directives(path):
    out = {}
    section = None
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("["):
            section = line
        elif section == "[Service]" and "=" in line:
            key, value = line.split("=", 1)
            out.setdefault(key, []).append(value)
    return out


def struct_fields(source, name):
    match = re.search(rf"pub struct {name} \{{(.*?)\n\}}", source, re.S)
    return set(re.findall(r"^\s*pub (\w+):", match.group(1), re.M)) if match else None


class UnitTest(unittest.TestCase):
    def setUp(self):
        self.d = directives(UNIT)
        self.host = directives(HOST_UNIT)

    def test_same_hardening_as_the_host_unit(self):
        # Every sandbox directive of the host unit is present with the same value; the only
        # differences are the ones named in the unit's comments.
        sandbox = ["NoNewPrivileges", "CapabilityBoundingSet", "AmbientCapabilities", "ProtectSystem", "ProtectHome",
                   "PrivateTmp", "PrivateDevices", "ProtectKernelTunables", "ProtectKernelModules",
                   "ProtectKernelLogs", "ProtectControlGroups", "ProtectClock", "ProtectHostname", "ProtectProc",
                   "ProcSubset", "RestrictAddressFamilies", "RestrictNamespaces", "RestrictRealtime",
                   "RestrictSUIDSGID", "LockPersonality", "SystemCallArchitectures", "SystemCallFilter",
                   "SystemCallErrorNumber", "User", "Restart"]
        for key in sandbox:
            if key == "User":
                self.assertEqual(self.d[key], ["spot-control-plane"])
            else:
                self.assertEqual(self.d.get(key), self.host.get(key), key)
        self.assertEqual(self.d["MemoryDenyWriteExecute"], ["true"])
        self.assertNotIn("MemoryDenyWriteExecute", self.host)

    def test_empty_capability_set_and_private_state(self):
        self.assertEqual(self.d["CapabilityBoundingSet"], [""])
        self.assertEqual(self.d["NoNewPrivileges"], ["true"])
        self.assertEqual(self.d["StateDirectoryMode"], ["0700"])
        self.assertEqual(self.d["UMask"], ["0077"])
        self.assertEqual(self.d["ReadWritePaths"], ["/var/lib/spot-control-plane"])

    def test_exec_start_and_condition(self):
        self.assertEqual(self.d["ExecStart"], ["/usr/bin/spot-control-plane run --config /etc/spot-control-plane/control-plane.toml"])
        self.assertIn("ConditionPathExists=/etc/spot-control-plane/control-plane.toml", UNIT.read_text())
        self.assertNotIn("ExecStartPre", self.d)

    def test_environment_file_is_optional_and_in_the_config_dir(self):
        self.assertEqual(self.d["EnvironmentFile"], ["-/etc/spot-control-plane/environment"])

    def test_graceful_stop_budget_exceeds_server_grace(self):
        self.assertEqual(self.d["TimeoutStopSec"], ["40s"])

    @unittest.skipUnless(shutil.which("systemd-analyze"), "systemd-analyze not found")
    def test_score_does_not_exceed_the_host_unit_score(self):
        def score(path):
            with tempfile.TemporaryDirectory() as tmp:
                unit = Path(tmp) / "x.service"
                unit.write_text(re.sub(r"(?m)^(ExecStart|ExecStartPre)=.*$", lambda m: "ExecStart=/usr/bin/true" if m.group(1) == "ExecStart" else "", path.read_text()))
                run = subprocess.run(["systemd-analyze", "security", "--offline=true", str(unit)], capture_output=True, text=True)
            match = re.search(r"exposure level for [^:]*: ([0-9.]+)", run.stdout + run.stderr)
            return float(match.group(1)) if match else None
        mine, theirs = score(UNIT), score(HOST_UNIT)
        if mine is None or theirs is None:
            self.skipTest("no score")
        self.assertLessEqual(mine, theirs)


class DataFilesTest(unittest.TestCase):
    def test_sysusers_and_tmpfiles(self):
        users = [l for l in (CP / "systemd" / "spot-control-plane.sysusers").read_text().splitlines() if l.startswith("u ")]
        self.assertEqual(len(users), 1)
        self.assertTrue(users[0].endswith("/sbin/nologin"))
        rows = [l.split() for l in (CP / "systemd" / "spot-control-plane.tmpfiles").read_text().splitlines() if l.startswith("d ")]
        modes = {r[1]: (r[2], r[3], r[4]) for r in rows}
        self.assertEqual(modes["/var/lib/spot-control-plane"], ("0700", "spot-control-plane", "spot-control-plane"))
        for path, (mode, owner, group) in modes.items():
            self.assertFalse(int(mode, 8) & 0o007, f"{path} is accessible to others")
        for sub in ("operator-ca", "host-ca", "tls", "licence"):
            self.assertIn(f"/etc/spot-control-plane/{sub}", modes)

    def test_no_key_certificate_or_secret_in_any_packaged_file(self):
        for path in CP.rglob("*"):
            if not path.is_file() or path.suffix in (".pyc",) or path.name == "test_control_plane_packaging.py":
                continue
            text = path.read_text(errors="ignore")
            self.assertNotRegex(text, r"-----BEGIN [A-Z ]*(PRIVATE KEY|CERTIFICATE)-----\n[A-Za-z0-9+/=]{20}", path)
            self.assertNotRegex(text.lower(), r"\bpassword\s*=\s*[^\s.\"]", path)

    def test_environment_template_has_no_assignment(self):
        lines = [l for l in (CP / "etc" / "environment").read_text().splitlines() if l.strip() and not l.startswith("#")]
        self.assertEqual(lines, [])


class ConfigExampleTest(unittest.TestCase):
    def setUp(self):
        self.data = tomllib.loads((CP / "etc" / "control-plane.toml.example").read_text())
        self.dev = tomllib.loads((CP / "compose" / "config.dev.toml").read_text())

    @unittest.skipUnless(CONFIG_RS.is_file(), "WP-CTL-01.T6 source not in this tree")
    def test_keys_are_the_ones_the_service_accepts(self):
        source = CONFIG_RS.read_text()
        for data in (self.data, self.dev):
            self.assertEqual(set(data) - struct_fields(source, "Config"), set())
            self.assertGreaterEqual(set(data), struct_fields(source, "Config") - {"database"} | {"database"})
            self.assertEqual(set(data["database"]) - struct_fields(source, "DatabaseConfig"), set())
            for listener in ("operator_listener", "host_listener"):
                self.assertEqual(set(data[listener]), struct_fields(source, "ListenerConfig"))
            self.assertEqual(set(data["host_ca"]) - struct_fields(source, "HostCaConfig"), set())
            self.assertEqual(set(data["operators"][0]), struct_fields(source, "OperatorConfig"))

    def test_the_two_listeners_trust_different_cas_and_addresses(self):
        for data in (self.data, self.dev):
            self.assertNotEqual(data["operator_listener"]["client_ca"], data["host_listener"]["client_ca"])
            self.assertNotEqual(data["operator_listener"]["listen"], data["host_listener"]["listen"])
            self.assertEqual(data["host_listener"]["client_ca"], data["host_ca"]["cert"])

    def test_template_cannot_start_as_is(self):
        # The administrator key is a placeholder (not 64 hex characters): the service refuses.
        for data in (self.data, self.dev):
            self.assertIsNone(re.fullmatch(r"[0-9a-fA-F]{64}", data["administrator_keys"][0]))

    def test_defaults_are_safe(self):
        self.assertTrue(self.data["database"]["tls"])
        self.assertTrue(self.data["operator_listener"]["listen"].startswith("127.0.0.1:"))
        self.assertTrue(self.data["host_listener"]["listen"].startswith("127.0.0.1:"))
        self.assertLessEqual(self.data["host_ca"]["validity_days"], 30)
        self.assertEqual(self.data["database"]["password_env"], "SPOT_CP_DB_PASSWORD")
        self.assertTrue(self.data["host_ca"]["revocation_list"].startswith("/var/lib/spot-control-plane/"))

    def test_certificate_paths_are_under_the_config_directory(self):
        for listener in ("operator_listener", "host_listener"):
            for key in ("server_cert", "server_key", "client_ca"):
                self.assertTrue(self.data[listener][key].startswith("/etc/spot-control-plane/"))


class SpecAndDebTest(unittest.TestCase):
    def test_spec_never_starts_or_writes_secrets(self):
        spec = SPEC.read_text()
        for name in ("pre", "post", "preun", "postun"):
            m = re.search(rf"(?ms)^%{name}\n(.*?)(?=^%[a-z]+\b|\Z)", spec)
            body = "\n".join(l for l in m.group(1).splitlines() if not l.lstrip().startswith("#"))
            self.assertNotRegex(body, r"systemctl\s+(start|enable|restart)")
            self.assertNotRegex(body, r"(?i)openssl|password|token")
        self.assertIn("%systemd_postun_with_restart spot-control-plane.service", spec)

    def test_spec_modes(self):
        spec = SPEC.read_text()
        self.assertIn("%attr(0600,spot-control-plane,spot-control-plane) %{_sysconfdir}/spot-control-plane/environment", spec)
        self.assertIn("%dir %attr(0700,spot-control-plane,spot-control-plane) %{_localstatedir}/lib/spot-control-plane", spec)
        for line in spec.splitlines():
            m = re.search(r"install .*-m (0?[0-7]{3,4})\b", line)
            if m:
                self.assertFalse(int(m.group(1), 8) & 0o002, line)

    def test_spec_comments_have_no_macro_references(self):
        for line in SPEC.read_text().splitlines():
            if line.startswith("#"):
                self.assertNotIn("%", line)

    def test_render_spec_and_control(self):
        text, version, release = bcp.render_spec("2.0.0-rc.1", 1767225600)
        self.assertEqual((version, release), ("2.0.0", "0.rc.1"))
        self.assertNotRegex(text, r"@[A-Z]+@")
        control = bcp.deb_control("2.0.0-rc.1", "x86_64", 10)
        self.assertIn("Version: 2.0.0~rc.1", control)
        self.assertIn("Package: spot-control-plane", control)

    def test_deb_scripts_do_not_start_enable_or_write_secrets(self):
        for name in ("postinst", "prerm", "postrm"):
            body = "\n".join(l for l in (DEBIAN / name).read_text().splitlines() if not l.lstrip().startswith("#"))
            self.assertNotRegex(body, r"systemctl\s+(start|enable|restart)\b")
            self.assertNotRegex(body, r"(?i)openssl|password|token|rm -rf")
            self.assertTrue((DEBIAN / name).read_text().startswith("#!/bin/sh\nset -e\n"))
        self.assertIn("systemctl try-restart spot-control-plane.service", (DEBIAN / "postinst").read_text())

    def test_conffiles_exist_in_the_staged_layout(self):
        for line in (DEBIAN / "conffiles").read_text().split():
            self.assertTrue(line.startswith("/etc/spot-control-plane/"))

    def test_build_without_a_binary_fails_clearly(self):
        with self.assertRaises(SystemExit):
            bcp._inputs(Path("/nonexistent/spot-control-plane"))

    def test_staged_modes(self):
        with tempfile.TemporaryDirectory() as tmp:
            binary = Path(tmp) / "bin"
            binary.write_bytes(b"x")
            staged = bcp._inputs(binary)
        self.assertEqual(staged["environment"][1], 0o600)
        self.assertEqual(staged["control-plane.toml.example"][1], 0o640)
        for name, (_, mode) in staged.items():
            self.assertFalse(mode & 0o002, name)


class ContainerTest(unittest.TestCase):
    def setUp(self):
        self.text = (CP / "container" / "Containerfile").read_text()
        self.code = "\n".join(l for l in self.text.splitlines() if not l.lstrip().startswith("#"))

    def test_final_stage_is_empty_and_non_root(self):
        stages = re.findall(r"(?m)^FROM (\S+)", self.code)
        self.assertEqual(stages[-1], "scratch")
        self.assertRegex(self.code, r"(?m)^USER 10001:10001$")
        self.assertNotRegex(self.code, r"(?m)^USER (root|0)\b")

    def test_no_shell_or_package_manager_in_the_final_image(self):
        final = self.code.split("FROM scratch", 1)[1]
        self.assertNotRegex(final, r"(?m)^RUN\b")
        self.assertNotRegex(final, r"(bash|/bin/sh|microdnf|dnf|apt)")
        installed = re.search(r"\binstall (glibc libgcc curl-minimal)\b", self.code)
        self.assertIsNotNone(installed)
        self.assertEqual(installed.group(1).split(), ["glibc", "libgcc", "curl-minimal"])

    def test_healthcheck_is_exec_form_on_the_live_route(self):
        m = re.search(r"(?s)HEALTHCHECK .*?CMD (\[.*?\])\n", self.code)
        self.assertIsNotNone(m)
        self.assertIn("/cp/v1/live", m.group(1))
        self.assertIn("--cert", m.group(1))
        self.assertTrue(m.group(1).lstrip().startswith('["/usr/bin/curl"'))

    def test_nothing_secret_is_copied_in(self):
        for line in self.code.splitlines():
            if line.startswith(("COPY", "ADD")):
                self.assertNotRegex(line, r"\.(pem|key|crt|toml|p12)\b|environment|secret", line)

    def test_ports_entrypoint_and_state_volume(self):
        self.assertIn("EXPOSE 8443 8444", self.code)
        self.assertIn('ENTRYPOINT ["/usr/bin/spot-control-plane"]', self.code)
        self.assertIn('VOLUME ["/var/lib/spot-control-plane"]', self.code)

    @unittest.skipUnless(shutil.which("podman") and os.environ.get("SPOT_CP_BUILD_IMAGE"), "set SPOT_CP_BUILD_IMAGE=1 with podman for the build smoke test")
    def test_image_builds_and_runs_non_root_read_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            ctx = Path(tmp)
            (ctx / "spot-control-plane").write_bytes(b"#!/bin/true\n")
            (ctx / "LICENSE").write_text("x")
            (ctx / "NOTICE").write_text("x")
            tag = "spot-control-plane-smoke:test"
            run = subprocess.run(["podman", "build", "-f", str(CP / "container" / "Containerfile"), "-t", tag, str(ctx)],
                                 capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stderr)
            user = subprocess.run(["podman", "image", "inspect", "-f", "{{.Config.User}}", tag], capture_output=True, text=True)
            self.assertEqual(user.stdout.strip(), "10001:10001")
            sh = subprocess.run(["podman", "run", "--rm", "--read-only", "--entrypoint", "/bin/sh", tag, "-c", "true"],
                                capture_output=True, text=True)
            self.assertNotEqual(sh.returncode, 0, "the image must contain no shell")
            subprocess.run(["podman", "rmi", "-f", tag], capture_output=True)


class ComposeTest(unittest.TestCase):
    def setUp(self):
        self.text = (CP / "compose" / "compose.yaml").read_text()

    def test_marked_development_only(self):
        self.assertIn("DEVELOPMENT ONLY", self.text)

    def test_no_password_value_and_required_variables(self):
        self.assertIn("${SPOT_CP_DB_PASSWORD:?", self.text)
        for line in self.text.splitlines():
            if "PASSWORD" in line and not line.lstrip().startswith("#"):
                self.assertRegex(line, r"\$\{|export", line)

    def test_container_is_locked_down(self):
        self.assertIn("read_only: true", self.text)
        self.assertIn("cap_drop: [ALL]", self.text)
        self.assertIn("no-new-privileges:true", self.text)
        self.assertIn("- ./dev:/etc/spot-control-plane:ro", self.text)

    def test_ports_are_loopback_only(self):
        ports = re.findall(r'- "([^"]+:\d+:\d+)"', self.text)
        self.assertEqual(sorted(ports), ["127.0.0.1:8443:8443", "127.0.0.1:8444:8444"])


class DevCertsTest(unittest.TestCase):
    def test_script_syntax(self):
        run = subprocess.run(["bash", "-n", str(CP / "compose" / "gen-dev-certs.sh")], capture_output=True, text=True)
        self.assertEqual(run.returncode, 0, run.stderr)

    @unittest.skipUnless(shutil.which("openssl"), "openssl not found")
    def test_keys_are_0600_and_the_operator_ca_key_is_not_left(self):
        with tempfile.TemporaryDirectory() as tmp:
            run = subprocess.run(["bash", str(CP / "compose" / "gen-dev-certs.sh"), tmp], capture_output=True, text=True)
            self.assertEqual(run.returncode, 0, run.stderr)
            keys = list(Path(tmp).rglob("*.key"))
            self.assertGreaterEqual(len(keys), 4)
            for key in keys:
                self.assertEqual(stat.S_IMODE(key.stat().st_mode), 0o600, key)
            self.assertFalse((Path(tmp) / "operator-ca" / "ca.key").exists())
            self.assertTrue((Path(tmp) / "host-ca" / "ca.key").exists())
            verify = subprocess.run(["openssl", "verify", "-CAfile", str(Path(tmp) / "operator-ca" / "ca.pem"),
                                     str(Path(tmp) / "probe" / "client.pem")], capture_output=True, text=True)
            self.assertEqual(verify.returncode, 0, verify.stderr)


if __name__ == "__main__":
    unittest.main()
