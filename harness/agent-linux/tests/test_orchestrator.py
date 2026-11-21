"""Tests for the orchestration logic, driven by a stubbed runner.

These prove the decisions (what runs where, and why not) and the exact container argv,
without a container runtime or a real host.
"""

import json
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from agent_linux_harness.junit import ERROR, FAILED, PASSED, SKIPPED  # noqa: E402
from agent_linux_harness.matrix import Matrix, Target, TestCase  # noqa: E402
from agent_linux_harness.orchestrator import (  # noqa: E402
    build_test_binary,
    container_create_argv,
    container_exec_argv,
    container_name,
    container_remove_argv,
    execute,
    has_failures,
    image_argv,
    parse_cargo_artifacts,
    plan,
    plan_target,
    render_table,
    select_targets,
    wait_for_systemd,
)
from agent_linux_harness.runner import CommandResult, StubRunner

CONTAINER = Target(
    "fedora-44",
    "fedora",
    "registry.fedoraproject.org/fedora:44",
    capabilities=frozenset({"systemd", "privileged"}),
)
VM = Target(
    "fedora-44-vm",
    "fedora",
    "fedora.qcow2",
    image_source="qemu",
    mode="vm",
    capabilities=frozenset({"systemd", "privileged", "selinux", "firewalld"}),
)
TESTS = (
    TestCase("a", frozenset(), False),
    TestCase("b", frozenset({"systemd"}), False),
    TestCase("c", frozenset({"selinux"}), True),
    TestCase("d", frozenset({"lvm_loop"}), False),
)
MATRIX = Matrix(1, "junit", (CONTAINER, VM), TESTS)


def libtest_ok(name: str) -> str:
    return (
        "running 1 test\n"
        f"test {name} ... ok\n\n"
        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; "
        "finished in 0.01s\n"
    )


def libtest_failed(name: str) -> str:
    return (
        "running 1 test\n"
        f"test {name} ... FAILED\n\n"
        "failures:\n\n"
        f"---- {name} stdout ----\n"
        "thread panicked: the daemon is not running\n\n"
        "failures:\n"
        f"    {name}\n\n"
        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; "
        "finished in 0.02s\n"
    )


class PlanTests(unittest.TestCase):
    def test_skips_vm_only_on_a_container(self):
        target_plan = plan_target(CONTAINER, TESTS, "container")
        by_name = {decision.test.name: decision for decision in target_plan.decisions}
        self.assertTrue(by_name["a"].run)
        self.assertTrue(by_name["b"].run)
        self.assertFalse(by_name["c"].run)
        self.assertEqual(by_name["c"].reason, "needs vm")
        self.assertFalse(by_name["d"].run)
        self.assertEqual(by_name["d"].reason, "needs lvm_loop")

    def test_vm_only_runs_on_a_vm(self):
        target_plan = plan_target(VM, TESTS, "vm")
        by_name = {decision.test.name: decision for decision in target_plan.decisions}
        self.assertTrue(by_name["c"].run)
        self.assertEqual(by_name["d"].reason, "needs lvm_loop")

    def test_missing_capabilities_are_all_named(self):
        test = TestCase("e", frozenset({"systemd", "chrony"}), False)
        target = Target("t", "fedora", "img", capabilities=frozenset({"systemd"}))
        decision = plan_target(target, (test,), "container").decisions[0]
        self.assertFalse(decision.run)
        self.assertEqual(decision.reason, "needs chrony")

    def test_select_targets_filters_by_mode(self):
        self.assertEqual(select_targets(MATRIX, None, "container"), (CONTAINER,))
        self.assertEqual(select_targets(MATRIX, None, "vm"), (VM,))

    def test_select_targets_by_name(self):
        self.assertEqual(select_targets(MATRIX, ["fedora-44"], "container"), (CONTAINER,))

    def test_select_targets_unknown_name(self):
        with self.assertRaises(KeyError):
            select_targets(MATRIX, ["nope"], "container")

    def test_plan_counts(self):
        run_plan = plan(MATRIX, None, "container")
        self.assertEqual(run_plan.runnable_count, 2)


class ArgvTests(unittest.TestCase):
    def test_create_podman_systemd_privileged(self):
        argv = container_create_argv("podman", CONTAINER, "/bin/real", "spot-c")
        self.assertEqual(argv[:4], ["podman", "run", "-d", "--name"])
        self.assertIn("--systemd=always", argv)
        self.assertIn("--privileged", argv)
        self.assertIn("--security-opt", argv)
        self.assertIn("/bin/real:/opt/spot/real-tests:ro,z", argv)
        self.assertIn("SPOT_TARGET_FAMILY=fedora", argv)
        self.assertIn("SPOT_TARGET_NAME=fedora-44", argv)
        # A systemd container's entry point is systemd, not the test binary.
        self.assertNotIn("--entrypoint", argv)
        self.assertEqual(argv[-1], CONTAINER.image)

    def test_create_docker_systemd_uses_cgroup_plumbing(self):
        argv = container_create_argv("docker", CONTAINER, "/bin/real", "spot-c")
        self.assertNotIn("--systemd=always", argv)
        self.assertIn("--cgroupns=host", argv)
        self.assertIn("/sys/fs/cgroup:/sys/fs/cgroup:rw", argv)
        self.assertIn("/bin/real:/opt/spot/real-tests:ro", argv)
        self.assertEqual(argv[argv.index("--entrypoint") + 1], "/sbin/init")

    def test_create_non_systemd_target_runs_a_keepalive(self):
        plain = Target("plain", "fedora", "img", capabilities=frozenset())
        argv = container_create_argv("podman", plain, "/bin/real", "spot-c")
        self.assertNotIn("--systemd=always", argv)
        self.assertNotIn("--privileged", argv)
        self.assertEqual(
            argv[-5:], ["--entrypoint", "/bin/sh", "img", "-c", "sleep infinity"]
        )

    def test_exec_runs_one_test(self):
        argv = container_exec_argv("podman", "spot-c", "b")
        self.assertEqual(
            argv,
            [
                "podman",
                "exec",
                "spot-c",
                "/opt/spot/real-tests",
                "--ignored",
                "--exact",
                "--test-threads=1",
                "b",
            ],
        )

    def test_remove_force_removes(self):
        self.assertEqual(
            container_remove_argv("podman", "spot-c"), ["podman", "rm", "-f", "spot-c"]
        )

    def test_container_name_is_deterministic(self):
        self.assertEqual(container_name("fedora-44"), "spot-agent-linux-fedora-44")

    def test_image_argv_pull(self):
        self.assertEqual(
            image_argv("podman", CONTAINER, "."),
            ["podman", "pull", CONTAINER.image],
        )

    def test_image_argv_build(self):
        built = Target(
            "custom", "fedora", "spot/custom", image_source="build", dockerfile="docker/Dockerfile"
        )
        argv = image_argv("docker", built, "/matrix")
        self.assertEqual(argv[:4], ["docker", "build", "-t", "custom"])
        self.assertIn("-f", argv)
        self.assertIn("/matrix/docker/Dockerfile", argv)


class WaitTests(unittest.TestCase):
    def test_ready_when_systemd_is_running(self):
        runner = StubRunner(
            responses={
                "podman": CommandResult(("podman", "exec"), 0, "running\n", ""),
            }
        )
        self.assertTrue(wait_for_systemd(runner, "podman", "spot-c", timeout=0, sleep=lambda _: None))

    def test_not_ready_on_timeout(self):
        runner = StubRunner(
            responses={"podman": CommandResult(("podman", "exec"), 1, "starting\n", "")}
        )
        self.assertFalse(
            wait_for_systemd(runner, "podman", "spot-c", timeout=0, sleep=lambda _: None)
        )


class CargoArtifactTests(unittest.TestCase):
    def test_parses_the_test_executable(self):
        lines = [
            "Compiling spot-agent-linux v0.1.0",
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "target": {"name": "real", "kind": ["test"]},
                    "executable": "/tmp/target/debug/deps/real-abc123",
                }
            ),
            json.dumps(
                {
                    "reason": "compiler-artifact",
                    "target": {"name": "spot_agent_linux_real", "kind": ["lib"]},
                    "executable": None,
                }
            ),
            json.dumps({"reason": "build-finished", "success": True}),
        ]
        self.assertEqual(parse_cargo_artifacts(lines), ["/tmp/target/debug/deps/real-abc123"])

    def test_ignores_non_json_lines(self):
        self.assertEqual(parse_cargo_artifacts(["Compiling x", "warning: y"]), [])

    def test_build_test_binary(self):
        artifact = json.dumps(
            {
                "reason": "compiler-artifact",
                "target": {"name": "real", "kind": ["test"]},
                "executable": "/tmp/real-abc",
            }
        )
        runner = StubRunner(
            responses={
                "cargo": CommandResult(("cargo", "test"), 0, artifact + "\n", ""),
            }
        )
        binary = build_test_binary(runner, "/crate")
        self.assertEqual(binary, "/tmp/real-abc")
        self.assertEqual(runner.cwds, ["/crate"])

    def test_build_test_binary_without_an_executable_raises(self):
        runner = StubRunner(responses={"cargo": CommandResult(("cargo",), 1, "", "error: boom")})
        with self.assertRaises(RuntimeError):
            build_test_binary(runner, "/crate")


class ExecuteTests(unittest.TestCase):
    def _runner_for(self, run_plan, outputs):
        runner = StubRunner()
        runner.programs["podman"] = "/usr/bin/podman"
        for target_plan in run_plan.targets:
            target = target_plan.target
            image = tuple(image_argv("podman", target, "."))
            runner.responses[image] = CommandResult(image, 0, "pulled", "")
            name = container_name(target.name)
            create = tuple(container_create_argv("podman", target, "/bin/real", name))
            runner.responses[create] = CommandResult(create, 0, "container-id", "")
            ready = ("podman", "exec", name, "systemctl", "is-system-running")
            runner.responses[ready] = CommandResult(ready, 0, "running\n", "")
            for decision in target_plan.runnable:
                argv = tuple(container_exec_argv("podman", name, decision.test.name))
                runner.responses[argv] = CommandResult(
                    argv, 0, outputs.get(decision.test.name, libtest_ok(decision.test.name)), ""
                )
        return runner

    def test_successful_run(self):
        run_plan = plan(MATRIX, ["fedora-44"], "container")
        runner = self._runner_for(run_plan, {})
        reports = execute(
            run_plan, runner, runtime="podman", binary="/bin/real", matrix_dir=".", ready_timeout=0
        )
        report = reports[0]
        statuses = {result.name: result.status for result in report.results}
        self.assertEqual(statuses["a"], PASSED)
        self.assertEqual(statuses["b"], PASSED)
        self.assertEqual(statuses["c"], SKIPPED)
        self.assertEqual(statuses["d"], SKIPPED)
        self.assertFalse(has_failures(reports))
        # The container is removed.
        self.assertIn(("podman", "rm", "-f", "spot-agent-linux-fedora-44"), runner.calls)

    def test_a_failing_test_is_reported(self):
        run_plan = plan(MATRIX, ["fedora-44"], "container")
        runner = self._runner_for(run_plan, {"b": libtest_failed("b")})
        reports = execute(
            run_plan, runner, runtime="podman", binary="/bin/real", matrix_dir=".", ready_timeout=0
        )
        statuses = {result.name: result.status for result in reports[0].results}
        self.assertEqual(statuses["b"], FAILED)
        self.assertTrue(has_failures(reports))

    def test_a_test_name_that_matched_nothing_is_an_error_not_a_pass(self):
        run_plan = plan(MATRIX, ["fedora-44"], "container")
        silent = "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored\n"
        runner = self._runner_for(run_plan, {"b": silent})
        reports = execute(
            run_plan, runner, runtime="podman", binary="/bin/real", matrix_dir=".", ready_timeout=0
        )
        statuses = {result.name: result.status for result in reports[0].results}
        self.assertEqual(statuses["a"], PASSED)
        self.assertEqual(statuses["b"], ERROR)
        self.assertTrue(has_failures(reports))

    def test_an_image_failure_errors_the_runnable_tests(self):
        run_plan = plan(MATRIX, ["fedora-44"], "container")
        runner = StubRunner()
        image = tuple(image_argv("podman", CONTAINER, "."))
        runner.responses[image] = CommandResult(image, 125, "", "no such image")
        reports = execute(
            run_plan, runner, runtime="podman", binary="/bin/real", matrix_dir=".", ready_timeout=0
        )
        self.assertIn("no such image", reports[0].error)
        statuses = {result.name: result.status for result in reports[0].results}
        self.assertEqual(statuses["a"], ERROR)
        self.assertEqual(statuses["c"], SKIPPED)
        self.assertTrue(has_failures(reports))
        # No container was started, so none is removed.
        self.assertNotIn(("podman", "rm", "-f", "spot-agent-linux-fedora-44"), runner.calls)

    def test_render_table_lists_every_target(self):
        run_plan = plan(MATRIX, None, "container")
        runner = self._runner_for(run_plan, {})
        reports = execute(
            run_plan, runner, runtime="podman", binary="/bin/real", matrix_dir=".", ready_timeout=0
        )
        table = render_table(run_plan, reports)
        self.assertIn("fedora-44", table)
        self.assertIn("SKIP-vm", table)
        self.assertIn("SKIP-cap", table)
        self.assertIn("PASS", table)


if __name__ == "__main__":
    unittest.main()
