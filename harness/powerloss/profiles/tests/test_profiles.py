"""The shipped-host profiles: settings assertion, extras, adapters, profile data, the run loop."""

from __future__ import annotations

import contextlib
import io
import os
import shutil
import tempfile
import unittest

import profiles_support  # noqa: F401  (path bootstrap)

import campaign as campaign_mod
import run as run_mod
import shipped_adapters as adapters
import shipped_extras as extras
import shipped_profile as sp
import shipped_runner
import shipped_settings as ss
from targets import FakeTarget, TargetError
from workload import WorkloadConfig

GOOD = {"fsync": "on", "full_page_writes": "on", "synchronous_commit": "on"}


class SettingsTests(unittest.TestCase):
    def test_shipped_values_pass(self) -> None:
        ss.assert_shipped(GOOD)
        self.assertEqual(ss.problems(GOOD), [])

    def test_every_changed_value_fails(self) -> None:
        for name, bad in (
            ("fsync", "off"),
            ("full_page_writes", "off"),
            ("synchronous_commit", "off"),
            ("synchronous_commit", "local"),
            ("synchronous_commit", "remote_write"),
        ):
            with self.subTest(name=name, bad=bad):
                with self.assertRaises(ss.ShippedSettingsError):
                    ss.assert_shipped({**GOOD, name: bad})

    def test_missing_value_fails_and_is_named(self) -> None:
        found = ss.problems({"fsync": "on"})
        self.assertEqual(len(found), 2)
        self.assertTrue(any("full_page_writes=missing" in line for line in found))

    def test_case_and_whitespace_are_normalised(self) -> None:
        ss.assert_shipped({"fsync": " ON ", "full_page_writes": "On", "synchronous_commit": "on\n"})

    def test_show_argv_is_a_list_and_refuses_unlisted_names(self) -> None:
        argv = ss.show_argv("psql", "fsync")
        self.assertIsInstance(argv, list)
        self.assertEqual(argv[-1], "SHOW fsync")
        with self.assertRaises(ss.ShippedSettingsError):
            ss.show_argv("psql", "fsync; DROP TABLE x")

    def test_read_settings_runs_one_show_per_setting(self) -> None:
        seen = []

        def run(argv):
            seen.append(argv[-1])
            return "on\n"

        values = ss.read_settings(run)
        self.assertEqual(values, GOOD)
        self.assertEqual(sorted(seen), sorted(f"SHOW {n}" for n in ss.REQUIRED_NAMES))

    def test_url_travels_in_the_environment_only(self) -> None:
        env = ss.psql_env({ss.PG_URL_ENV: "postgres://u:secret@h/db", "PATH": "/bin"})
        self.assertEqual(env["PGDATABASE"], "postgres://u:secret@h/db")
        self.assertNotIn(ss.PG_URL_ENV, env)
        for argv in (ss.show_argv("psql", n) for n in ss.REQUIRED_NAMES):
            self.assertFalse(any("secret" in token for token in argv))
        with self.assertRaises(ss.ShippedSettingsError):
            ss.psql_env({"PATH": "/bin"})

    def test_json_contract_for_the_harness(self) -> None:
        self.assertEqual(ss.to_json(GOOD), '{"fsync": "on", "full_page_writes": "on", "synchronous_commit": "on"}')


class ExtrasTests(unittest.TestCase):
    def test_clean_snapshot_has_no_violations(self) -> None:
        snap = extras.ExtraSnapshot(
            numbers={"S": ("A-1", "A-2")}, ledger={"PKR": (5, 5)}, job_runs={"j": 1}, job_states={"j": ("completed",)}
        )
        self.assertEqual(extras.numbering_violations(snap) + extras.balance_violations(snap) + extras.job_violations(snap), [])

    def test_duplicate_number_unbalanced_and_double_run(self) -> None:
        snap = extras.ExtraSnapshot(
            numbers={"S": ("A-1", "A-1", "A-1")},
            ledger={"PKR": (10, 9), "USD": (1, 1)},
            job_runs={"j": 2},
            job_states={"k": ("pending", "completed")},
        )
        self.assertEqual([v.detail for v in extras.numbering_violations(snap)], ["duplicate_number A-1"])
        self.assertEqual([v.id for v in extras.balance_violations(snap)], ["PKR"])
        self.assertEqual(sorted(v.id for v in extras.job_violations(snap)), ["j", "k"])

    def test_missing_parts_are_violations_not_a_pass(self) -> None:
        snap = extras.ExtraSnapshot.unavailable()
        self.assertEqual(len(extras.completeness_violations(snap)), len(extras.PARTS))

    def test_round_trip_and_float_totals_are_refused(self) -> None:
        snap = extras.ExtraSnapshot(numbers={"S": ("A",)}, ledger={"PKR": (3, 3)}, job_runs={"j": 1})
        self.assertEqual(extras.ExtraSnapshot.from_json_text(snap.to_json_text()), snap)
        with self.assertRaises(extras.ExtrasError):
            extras.ExtraSnapshot.from_json_text('{"ledger": {"PKR": [1.5, 1.5]}}')
        with self.assertRaises(extras.ExtrasError):
            extras.ExtraSnapshot.from_json_text("not json")
        with self.assertRaises(extras.ExtrasError):
            extras.ExtraSnapshot.from_json_text("[]")

    def test_invariants_bind_to_the_snapshot(self) -> None:
        snap = extras.ExtraSnapshot(ledger={"PKR": (1, 2)})
        found = {inv.id: inv.run(None, None) for inv in extras.invariants(snap)}  # type: ignore[arg-type]
        self.assertEqual(len(found[extras.BALANCES]), 1)
        self.assertEqual(found[extras.NUMBERING], [])


class AdapterTests(unittest.TestCase):
    def test_parse_postmaster_pid(self) -> None:
        self.assertEqual(adapters.parse_postmaster_pid("4242\n/data\n1700000000\n"), 4242)
        self.assertIsNone(adapters.parse_postmaster_pid(""))
        self.assertIsNone(adapters.parse_postmaster_pid("abc\n"))
        self.assertIsNone(adapters.parse_postmaster_pid("0\n"))

    def test_parse_sc_queryex(self) -> None:
        text = "SERVICE_NAME: SpotledgerServer\n        STATE              : 4  RUNNING\n        PID                : 1234\n"
        self.assertEqual(adapters.parse_sc_queryex(text), 1234)
        self.assertIsNone(adapters.parse_sc_queryex(text.replace("1234", "0")))
        self.assertIsNone(adapters.parse_sc_queryex("nothing"))

    def test_match_serve_cmdline(self) -> None:
        self.assertTrue(adapters.match_serve_cmdline(["spot", "--root", "/srv/spot", "serve"], "/srv/spot/"))
        self.assertFalse(adapters.match_serve_cmdline(["spot", "--root", "/other", "serve"], "/srv/spot"))
        self.assertFalse(adapters.match_serve_cmdline(["spot", "check"], "/srv/spot"))

    def test_linux_serve_pids_reads_a_proc_tree(self) -> None:
        proc = tempfile.mkdtemp(prefix="proc-")
        try:
            for pid, parts in (("11", [b"/usr/bin/spot", b"--root", b"/srv/spot", b"serve"]), ("12", [b"bash"]), ("x", [b"spot"])):
                os.makedirs(os.path.join(proc, pid))
                with open(os.path.join(proc, pid, "cmdline"), "wb") as handle:
                    handle.write(b"\0".join(parts) + b"\0")
            self.assertEqual(adapters.linux_serve_pids("/srv/spot", proc), [11])
        finally:
            shutil.rmtree(proc, ignore_errors=True)

    def test_ready_url(self) -> None:
        self.assertEqual(adapters.ready_url("127.0.0.1:8080"), "http://127.0.0.1:8080/api/health/ready")
        with self.assertRaises(ValueError):
            adapters.ready_url("nope")

    def test_snapshot_without_a_reader_reports_every_part_missing(self) -> None:
        saved = os.environ.pop(adapters.SNAPSHOT_READER_ENV, None)
        try:
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(adapters.main(["snapshot"]), 0)
            from storesnapshot import StoreSnapshot

            snap = StoreSnapshot.from_json_text(out.getvalue())
            self.assertEqual(set(snap.missing), set(adapters.SNAPSHOT_PARTS))
        finally:
            if saved is not None:
                os.environ[adapters.SNAPSHOT_READER_ENV] = saved

    def test_extras_without_a_reader_reports_every_part_missing(self) -> None:
        saved = os.environ.pop(adapters.EXTRAS_READER_ENV, None)
        try:
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(adapters.main(["extras"]), 0)
            self.assertEqual(set(extras.ExtraSnapshot.from_json_text(out.getvalue()).missing), set(extras.PARTS))
        finally:
            if saved is not None:
                os.environ[adapters.EXTRAS_READER_ENV] = saved


class ProfileTests(unittest.TestCase):
    def test_three_profiles_with_known_drivers(self) -> None:
        self.assertEqual(sorted(sp.PROFILES), ["linux-process-kill", "linux-vm-power-cut", "windows-vm"])
        import cutdrivers

        for profile in sp.PROFILES.values():
            self.assertIn(profile.driver, cutdrivers.DRIVERS)
            self.assertEqual(profile.cycles, 200)

    def test_validated_names_what_is_missing(self) -> None:
        profile = sp.PROFILES["linux-vm-power-cut"]
        with self.assertRaises(sp.ProfileError) as ctx:
            sp.validated(profile, sp.HostSettings())
        message = str(ctx.exception)
        for needle in ("--pg-data", "--root", "--vm", "--guest-host"):
            self.assertIn(needle, message)

    def test_unsafe_vm_name_is_refused(self) -> None:
        profile = sp.PROFILES["windows-vm"]
        host = sp.HostSettings(pg_data="D", vm="x'; Remove-Item *", guest_host="h", guest_user="u")
        with self.assertRaises(Exception):
            sp.validated(profile, host)

    def test_windows_pids_use_the_service_and_linux_the_root(self) -> None:
        win = sp.adapter_argv(sp.PROFILES["windows-vm"], sp.HostSettings(pg_data="D"), "pids")
        self.assertIn(sp.SERVICE_NAME, win)
        lin = sp.adapter_argv(sp.PROFILES["linux-process-kill"], sp.HostSettings(pg_data="D", root="/r"), "pids")
        self.assertIn("/r", lin)
        self.assertNotIn(sp.SERVICE_NAME, lin)

    def test_workload_needs_a_streaming_client(self) -> None:
        profile = sp.PROFILES["linux-process-kill"]
        with self.assertRaises(sp.ProfileError):
            sp.workload_argv(profile, sp.HostSettings(), {})
        argv = sp.workload_argv(profile, sp.HostSettings(), {sp.CLIENT_CMD_ENV: "client run"})
        self.assertEqual(argv[-2:], ("--client-cmd", "client run"))

    def test_process_kill_needs_a_start_command_vm_does_not(self) -> None:
        with self.assertRaises(sp.ProfileError):
            sp.boot_argv(sp.PROFILES["linux-process-kill"], sp.HostSettings(), {})
        self.assertEqual(
            sp.boot_argv(sp.PROFILES["linux-process-kill"], sp.HostSettings(), {sp.START_CMD_ENV: "systemctl start spot"}),
            ("systemctl", "start", "spot"),
        )
        self.assertEqual(sp.boot_argv(sp.PROFILES["linux-vm-power-cut"], sp.HostSettings(vm="vm1"), {}), ("virsh", "start", "vm1"))
        self.assertEqual(sp.boot_argv(sp.PROFILES["windows-vm"], sp.HostSettings(vm="vm1"), {})[-1], "Start-VM -Name 'vm1'")

    def test_windows_start_is_the_service(self) -> None:
        self.assertEqual(sp.start_host_argv(sp.PROFILES["windows-vm"], sp.HostSettings()), ("sc.exe", "start", "SpotledgerServer"))


def _model(tmp: str, *, fsync: bool = True, name: str = "h") -> FakeTarget:
    return FakeTarget(
        durability={"fsync": fsync, "synchronous_commit": True, "full_page_writes": True},
        config=WorkloadConfig(seed=0, workers=1, operations=30, profile="smoke"),
        history_path=os.path.join(tmp, f"{name}.jsonl"),
        clock=lambda: 0,
    )


def _config(tmp: str, cycles: int, out: str) -> shipped_runner.ProfileConfig:
    return shipped_runner.ProfileConfig(
        campaign=campaign_mod.CampaignConfig(
            campaign_seed=3, cycles=cycles, operations=30, workers=1, driver="process-kill", out_path=os.path.join(tmp, out)
        ),
        os_name="linux",
    )


CLEAN = extras.ExtraSnapshot(numbers={"S": ("A",)}, ledger={"PKR": (1, 1)}, job_runs={"j": 1})


class RunLoopTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.mkdtemp(prefix="profile-test-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)

    def test_clean_model_campaign_passes(self) -> None:
        summary = shipped_runner.run_profile(_model(self.tmp), lambda: CLEAN, "process-kill", _config(self.tmp, 4, "a.jsonl"))
        self.assertEqual((summary.cycles_run, summary.violations), (4, 0))

    def test_unbalanced_ledger_stops_the_run(self) -> None:
        bad = extras.ExtraSnapshot(ledger={"PKR": (2, 1)})
        summary = shipped_runner.run_profile(_model(self.tmp), lambda: bad, "process-kill", _config(self.tmp, 4, "b.jsonl"))
        self.assertTrue(summary.stopped_on_violation)
        self.assertEqual(summary.cycles_run, 1)
        self.assertIn(extras.BALANCES, summary.outcomes[0].failed_ids)

    def test_unreadable_extras_fail_the_cycle(self) -> None:
        summary = shipped_runner.run_profile(
            _model(self.tmp), extras.ExtraSnapshot.unavailable, "process-kill", _config(self.tmp, 2, "c.jsonl")
        )
        self.assertIn(extras.COMPLETE, summary.outcomes[0].failed_ids)

    def test_changed_setting_is_refused_before_the_cut(self) -> None:
        with self.assertRaises(TargetError):
            shipped_runner.run_profile(
                _model(self.tmp, fsync=False, name="off"), lambda: CLEAN, "process-kill", _config(self.tmp, 1, "d.jsonl")
            )

    def test_setting_changed_by_recovery_is_refused(self) -> None:
        class Drifting(FakeTarget):
            calls = 0

            def settings(self):  # the server comes back weaker than it went down
                Drifting.calls += 1
                values = super().settings()
                if Drifting.calls > 1:
                    values = {**values, "synchronous_commit": "off"}
                return values

        target = Drifting(
            config=WorkloadConfig(seed=0, workers=1, operations=30, profile="smoke"),
            history_path=os.path.join(self.tmp, "drift.jsonl"),
            clock=lambda: 0,
        )
        with self.assertRaises(TargetError) as ctx:
            shipped_runner.run_profile(target, lambda: CLEAN, "process-kill", _config(self.tmp, 1, "e.jsonl"))
        self.assertIn("after recovery", str(ctx.exception))

    def test_snapshot_gap_fails_the_cycle(self) -> None:
        class Gappy(FakeTarget):
            def snapshot(self):
                snap = super().snapshot()
                from dataclasses import replace

                return replace(snap, missing=("outbox",))

        target = Gappy(
            config=WorkloadConfig(seed=0, workers=1, operations=30, profile="smoke"),
            history_path=os.path.join(self.tmp, "gap.jsonl"),
            clock=lambda: 0,
        )
        summary = shipped_runner.run_profile(target, lambda: CLEAN, "process-kill", _config(self.tmp, 1, "f.jsonl"))
        self.assertIn("snapshot-complete", summary.outcomes[0].failed_ids)


class RunScriptTests(unittest.TestCase):
    def test_dry_run_exits_zero_and_touches_nothing(self) -> None:
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = run_mod.main(["--dry-run"])
        self.assertEqual(code, 0)
        text = out.getvalue()
        for name in sp.PROFILES:
            self.assertIn(f"profile {name}", text)
        self.assertIn("DRY RUN OK", text)

    def test_real_run_is_refused_without_confirm_disposable(self) -> None:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = run_mod.main(["run", "--profile", "linux-process-kill"])
        self.assertEqual(code, 2)
        self.assertIn("--confirm-disposable", err.getvalue())

    def test_a_run_names_what_is_missing(self) -> None:
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = run_mod.main(["run", "--profile", "linux-process-kill", "--confirm-disposable"])
        self.assertEqual(code, 2)
        self.assertIn("--pg-data", err.getvalue())


if __name__ == "__main__":
    unittest.main()
