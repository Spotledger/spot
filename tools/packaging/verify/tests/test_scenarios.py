"""The scenario table: complete for every distro, ordered, and deterministic to print.

The table is the contract between the driver and the distributions: every distro gets every
scenario, each scenario is its own function in its own module, a prerequisite always comes
earlier in the order, and the printed plan does not depend on the clock, the environment or
the working tree.
"""

import unittest

import support  # noqa: F401 - puts the driver modules on sys.path

import distros
import packages
import scenarios

EXPECTED = (
    "clean_install",
    "start_and_health",
    "upgrade",
    "downgrade",
    "removal",
    "container_image",
)


class TableCompletenessTests(unittest.TestCase):
    def test_every_distro_gets_every_scenario(self):
        for distro in distros.DISTROS:
            with self.subTest(distro=distro.name):
                names = tuple(scenario.name for scenario in scenarios.for_distro(distro))
                self.assertEqual(names, EXPECTED)

    def test_the_order_runs_an_install_first_and_the_image_last(self):
        self.assertEqual(scenarios.NAMES[0], "clean_install")
        self.assertEqual(scenarios.NAMES[-1], "container_image")

    def test_each_scenario_is_its_own_function_in_its_own_module(self):
        self.assertEqual(len(scenarios.SCENARIOS), len(EXPECTED))
        self.assertEqual(len({scenario.run for scenario in scenarios.SCENARIOS}), len(EXPECTED))
        modules = {scenario.run.__module__ for scenario in scenarios.SCENARIOS}
        self.assertEqual(len(modules), len(EXPECTED))
        for scenario in scenarios.SCENARIOS:
            self.assertTrue(scenario.proves, f"{scenario.name} says nothing about what it proves")

    def test_a_prerequisite_always_runs_earlier(self):
        seen: list[str] = []
        for scenario in scenarios.SCENARIOS:
            for requirement in scenario.requires:
                with self.subTest(scenario=scenario.name, requires=requirement):
                    self.assertIn(requirement, EX_PLAIN)
                    self.assertIn(requirement, seen)
            seen.append(scenario.name)

    def test_only_the_image_scenario_is_shared(self):
        shared = [scenario.name for scenario in scenarios.shared_scenarios()]
        self.assertEqual(shared, ["container_image"])
        for scenario in scenarios.distro_scenarios(distros.FEDORA_44):
            self.assertFalse(scenario.shared)

    def test_select_keeps_table_order_and_rejects_unknown_names(self):
        chosen = scenarios.select(["removal", "clean_install"])
        self.assertEqual([scenario.name for scenario in chosen], ["clean_install", "removal"])
        with self.assertRaises(KeyError):
            scenarios.get("no_such_scenario")


#: The scenario names independent of the table's order, for the prerequisite check.
EX_PLAIN = frozenset(EXPECTED)


class PlanTests(unittest.TestCase):
    def setUp(self):
        self.artefacts = (
            support.rpm("1.2.3"),
            support.rpm("1.2.4"),
            support.deb("1.2.3"),
            support.deb("1.2.4"),
        )

    def test_every_plan_is_a_list_of_argument_lists(self):
        for distro in distros.DISTROS:
            for scenario in scenarios.for_distro(distro):
                steps = scenarios.steps_for(scenario, distro, self.artefacts)
                self.assertGreater(len(steps), 2, f"{scenario.name} plans almost nothing")
                for step in steps:
                    with self.subTest(scenario=scenario.name, step=step.label):
                        self.assertIsInstance(step.argv, tuple)
                        self.assertTrue(step.argv)
                        self.assertTrue(all(isinstance(item, str) for item in step.argv))
                        self.assertTrue(step.expect)

    def test_plans_use_the_artefacts_that_were_found(self):
        steps = scenarios.steps_for(scenarios.get("clean_install"), distros.FEDORA_44, self.artefacts)
        install = steps[0]
        self.assertEqual(install.argv[-1], "/run/spot-pkgs/spot-1.2.3-1.fc44.x86_64.rpm")
        deb_steps = scenarios.steps_for(
            scenarios.get("clean_install"), distros.UBUNTU_2404, self.artefacts
        )
        self.assertEqual(deb_steps[0].argv[-1], "/run/spot-pkgs/spot_1.2.3_amd64.deb")

    def test_plans_without_artefacts_use_placeholders(self):
        steps = scenarios.steps_for(scenarios.get("clean_install"), distros.FEDORA_44, ())
        self.assertEqual(steps[0].argv[-1], "<package>")

    def test_plans_are_deterministic(self):
        for distro in distros.DISTROS:
            for scenario in scenarios.for_distro(distro):
                first = scenarios.steps_for(scenario, distro, self.artefacts)
                second = scenarios.steps_for(scenario, distro, self.artefacts)
                self.assertEqual(first, second)


class DryRunTests(unittest.TestCase):
    def setUp(self):
        self.artefacts = (support.rpm("1.2.3"), support.rpm("1.2.4"))

    def test_the_plan_is_byte_identical_between_runs(self):
        first = scenarios.dry_run(distros.DISTROS, self.artefacts)
        second = scenarios.dry_run(distros.DISTROS, self.artefacts)
        self.assertEqual(first, second)

    def test_the_plan_names_every_distro_and_scenario(self):
        output = scenarios.dry_run(distros.DISTROS, self.artefacts)
        for distro in distros.DISTROS:
            self.assertIn(distro.name, output)
            self.assertIn(distro.container_name, output)
        for name in EXPECTED:
            self.assertIn(f"scenario {name}", output)

    def test_the_plan_prints_argument_lists_not_shell_strings(self):
        output = scenarios.dry_run(distros.DISTROS, self.artefacts)
        self.assertIn('["dnf", "-y", "install"', output)
        self.assertIn('"http://127.0.0.1:8000/api/health/live"', output)
        self.assertNotIn("sh -c", output)

    def test_the_plan_does_not_dump_an_absolute_package_path(self):
        output = scenarios.dry_run(distros.DISTROS, self.artefacts)
        self.assertNotIn(str(support.REPO_ROOT), output)
        self.assertIn("spot-1.2.4-1.fc44.x86_64.rpm", output)


class DescribeTests(unittest.TestCase):
    def test_describe_lists_the_containerfiles_and_the_expectations(self):
        output = scenarios.describe(distros.DISTROS)
        for distro in distros.DISTROS:
            self.assertIn(str(distro.containerfile.relative_to(support.REPO_ROOT)), output)
        lines = [line for line in output.splitlines() if "upgrade restart:" in line]
        self.assertEqual(len(lines), len(distros.DISTROS))
        for line in lines:
            self.assertIn("upgrade restart: yes", line)
        self.assertIn("separate purge step: yes", output)


class ArtefactHelperTests(unittest.TestCase):
    def test_container_path_is_one_element(self):
        context, _, _ = support.context_for(distros.FEDORA_44)
        artefact = support.rpm("1.2.3")
        self.assertEqual(
            context.container_path(artefact), "/run/spot-pkgs/spot-1.2.3-1.fc44.x86_64.rpm"
        )
        self.assertIsInstance(context.container_path(artefact), str)

    def test_a_hostile_filename_is_one_element(self):
        context, _, _ = support.context_for(distros.FEDORA_44)
        artefact = support.rpm("1.2.3", filename="spot 1.2.3; id-1.fc44.x86_64.rpm")
        self.assertEqual(
            context.container_path(artefact), "/run/spot-pkgs/spot 1.2.3; id-1.fc44.x86_64.rpm"
        )

    def test_first_package_is_the_oldest_so_the_upgrade_has_somewhere_to_go(self):
        artefacts = (support.rpm("1.2.3"), support.rpm("1.2.4"))
        context, _, _ = support.context_for(distros.FEDORA_44, artefacts)
        self.assertEqual(context.first_package().version, "1.2.3")
        self.assertEqual(context.package().version, "1.2.4")

    def test_packages_missing_produces_a_skip(self):
        from context import ScenarioSkip

        context, _, _ = support.context_for(distros.FEDORA_44)
        with self.assertRaises(ScenarioSkip):
            context.package()
        context.artefacts = (support.deb("1.2.3"),)
        with self.assertRaises(ScenarioSkip):
            context.pair()

    def test_latest_prefers_the_newest_version(self):
        artefacts = (support.rpm("1.2.3"), support.rpm("1.2.4"))
        self.assertEqual(packages.latest(artefacts, "rpm").version, "1.2.4")


if __name__ == "__main__":
    unittest.main()
