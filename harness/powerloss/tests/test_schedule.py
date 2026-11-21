"""Seed handling: the RNG, the schedule and the campaign's per-cycle plan."""

from __future__ import annotations

import unittest

import support  # noqa: F401  (path bootstrap)

from campaign import cycle_seeds
from workload import Rng, Weights, generate_schedule, plan_one


class RngTests(unittest.TestCase):
    def test_same_seed_same_stream(self) -> None:
        a = Rng(1234)
        b = Rng(1234)
        self.assertEqual([a.next_u64() for _ in range(5)], [b.next_u64() for _ in range(5)])

    def test_different_seeds_differ(self) -> None:
        a = Rng(1)
        b = Rng(2)
        self.assertNotEqual(a.next_u64(), b.next_u64())

    def test_seed_zero_is_not_special(self) -> None:
        self.assertNotEqual(Rng(0).next_u64(), 0)

    def test_below_is_in_range(self) -> None:
        rng = Rng(9)
        for _ in range(1000):
            self.assertLess(rng.below(7), 7)

    def test_below_zero_is_zero(self) -> None:
        self.assertEqual(Rng(9).below(0), 0)


class ScheduleTests(unittest.TestCase):
    def test_total_matches_operations(self) -> None:
        plan = generate_schedule(5, 4, 101, Weights())
        self.assertEqual(sum(len(w) for w in plan), 101)
        self.assertEqual(len(plan), 4)

    def test_deterministic(self) -> None:
        self.assertEqual(generate_schedule(11, 3, 50, Weights()), generate_schedule(11, 3, 50, Weights()))

    def test_at_least_one_worker(self) -> None:
        self.assertEqual(len(generate_schedule(1, 0, 10, Weights())), 1)

    def test_plan_one_returns_a_known_kind(self) -> None:
        rng = Rng(3)
        kinds = {plan_one(rng, Weights()).kind["kind"] for _ in range(200)}
        self.assertTrue(kinds <= {
            "create_party",
            "create_order",
            "create_linked_order",
            "update_order",
            "delete_order",
            "delete_party",
            "increment_counter",
            "allocate_sequence",
            "emit",
            "read",
        })
        self.assertTrue(kinds)  # at least one


class CycleSeedTests(unittest.TestCase):
    def test_deterministic(self) -> None:
        self.assertEqual(cycle_seeds(7, 5, 100, 1), cycle_seeds(7, 5, 100, 1))

    def test_cut_offset_in_range(self) -> None:
        for seed, cut in cycle_seeds(7, 50, 40, 3):
            self.assertGreaterEqual(cut, 3)
            self.assertLessEqual(cut, 40)

    def test_low_is_clamped_to_operations(self) -> None:
        for _seed, cut in cycle_seeds(1, 10, 5, 100):
            self.assertEqual(cut, 5)

    def test_seeds_are_unrelated(self) -> None:
        seeds = [seed for seed, _ in cycle_seeds(7, 20, 10, 1)]
        self.assertEqual(len(set(seeds)), 20)


if __name__ == "__main__":
    unittest.main()
