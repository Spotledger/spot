"""The command line: ``--list``, ``--dry-run``, the exit-77 skip and bad usage.

These tests run ``run.py`` as a subprocess with a ``PATH`` that has nothing on it, which is
both the "podman is absent" case the brief asks for and the proof that neither ``--list`` nor
``--dry-run`` touches a container runtime.
"""

import unittest

import support


class ListTests(unittest.TestCase):
    def test_list_prints_the_distros_and_the_scenarios(self):
        result = support.run_cli(["--list"])
        self.assertEqual(result.returncode, 0, result.stderr)
        for distro in ("fedora-44", "almalinux-10", "ubuntu-2404"):
            self.assertIn(distro, result.stdout)
        for scenario in ("clean_install", "start_and_health", "upgrade", "downgrade",
                         "removal", "container_image"):
            self.assertIn(scenario, result.stdout)
        self.assertIn("harness/packaging-linux/fedora-44/Containerfile", result.stdout)

    def test_list_needs_no_container_runtime(self):
        result = support.run_cli(["--list"], env=support.environment_without_podman())
        self.assertEqual(result.returncode, 0, result.stderr)


class DryRunTests(unittest.TestCase):
    def test_dry_run_is_deterministic(self):
        first = support.run_cli(["--dry-run", "--distro", "all"])
        second = support.run_cli(["--dry-run", "--distro", "all"])
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(first.stdout, second.stdout)

    def test_dry_run_needs_no_container_runtime(self):
        result = support.run_cli(["--dry-run", "--distro", "all"],
                                 env=support.environment_without_podman())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("dry run", result.stdout)

    def test_dry_run_prints_argument_lists(self):
        result = support.run_cli(["--dry-run", "--distro", "fedora-44"])
        self.assertIn('["dnf", "-y", "install", "<package>"]', result.stdout)
        self.assertIn("spot-pkgverify-fedora-44", result.stdout)
        self.assertNotIn("Traceback", result.stdout)

    def test_dry_run_of_one_scenario_prints_only_that_scenario(self):
        result = support.run_cli(["--dry-run", "--distro", "ubuntu-2404",
                                  "--scenario", "removal"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("scenario removal", result.stdout)
        self.assertNotIn("scenario clean_install", result.stdout)
        self.assertNotIn("scenario upgrade", result.stdout)

    def test_a_package_directory_without_artefacts_says_so(self):
        result = support.run_cli(
            ["--dry-run", "--distro", "fedora-44", "--package-dir", "tools/packaging/verify"]
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("no artefact found", result.stdout)


class SkipTests(unittest.TestCase):
    def test_a_missing_podman_exits_77(self):
        result = support.run_cli(
            ["--distro", "fedora-44", "--scenario", "clean_install"],
            env=support.environment_without_podman(),
        )
        self.assertEqual(result.returncode, 77, result.stdout + result.stderr)
        self.assertIn("podman is not installed", result.stderr)
        self.assertIn("SKIP", result.stderr)

    def test_the_skip_message_points_at_the_readme(self):
        result = support.run_cli(["--distro", "all", "--scenario", "clean_install"],
                                 env=support.environment_without_podman())
        self.assertIn("packaging/linux/verify/README.md", result.stderr)


class UsageTests(unittest.TestCase):
    def test_an_unknown_distro_is_a_usage_error(self):
        result = support.run_cli(["--dry-run", "--distro", "gentoo"])
        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid choice", result.stderr)

    def test_an_unknown_scenario_is_a_usage_error(self):
        result = support.run_cli(["--dry-run", "--scenario", "smoke_test"])
        self.assertEqual(result.returncode, 2)

    def test_a_missing_binary_is_a_usage_error_before_any_runtime_check(self):
        result = support.run_cli(
            ["--binary", "/nonexistent/spot", "--scenario", "container_image"],
            env=support.environment_without_podman(),
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("does not exist", result.stderr)


if __name__ == "__main__":
    unittest.main()
