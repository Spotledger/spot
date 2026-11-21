"""Integer-only sampling: no floats, per-entity streams, the brief's formula."""

import sys
import unittest
from collections import Counter
from pathlib import Path
from random import Random

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from datagen.sampling import (
    ZIPF_SCALE,
    SamplingError,
    band_share_permille,
    stream_rng,
    triangular_cum_weights,
    weighted_index,
    zipf_cum_weights,
)


class ZipfTests(unittest.TestCase):
    def test_weight_formula_from_the_task_brief(self):
        cum = zipf_cum_weights(6, 2)
        steps = [cum[0]] + [cum[i] - cum[i - 1] for i in range(1, len(cum))]
        self.assertEqual(steps, [ZIPF_SCALE // rank**2 for rank in range(1, 7)])

    def test_cumulative_weights_are_increasing_integers(self):
        for skew in (1, 3, 8):
            cum = zipf_cum_weights(50, skew)
            self.assertTrue(all(type(weight) is int for weight in cum))
            self.assertEqual(cum, sorted(cum))

    def test_rejects_bad_parameters(self):
        with self.assertRaises(SamplingError):
            zipf_cum_weights(0, 1)
        with self.assertRaises(SamplingError):
            zipf_cum_weights(5, 0)


class TriangularTests(unittest.TestCase):
    def test_weights_rise_to_the_peak_and_fall(self):
        cum = triangular_cum_weights(1, 2, 6)
        self.assertEqual(len(cum), 6)
        steps = [cum[0]] + [cum[i] - cum[i - 1] for i in range(1, len(cum))]
        self.assertEqual(steps, [5, 6, 5, 4, 3, 2])

    def test_rejects_bad_parameters(self):
        with self.assertRaises(SamplingError):
            triangular_cum_weights(3, 2, 6)


class WeightedIndexTests(unittest.TestCase):
    def test_draws_follow_the_weights(self):
        cum = zipf_cum_weights(10, 3)
        rng = Random(42)
        counts = Counter(weighted_index(rng, cum) for _ in range(20000))
        self.assertTrue(all(0 <= index < len(cum) for index in counts))
        # w(1):w(2):w(3) = 10**12 : 1.25*10**11 : ~3.7*10**10
        self.assertGreater(counts[0], 4 * counts[1])
        self.assertGreater(counts[1], 2 * counts[2])

    def test_every_index_is_reachable(self):
        cum = zipf_cum_weights(10, 3)
        rng = Random(7)
        seen = {weighted_index(rng, cum) for _ in range(5000)}
        self.assertEqual(len(seen), 10)


class BandShareTests(unittest.TestCase):
    def test_permille_shares_are_exact_integers(self):
        cum = zipf_cum_weights(2, 1)  # weights 10**12 and 5*10**11
        self.assertEqual(band_share_permille(cum, 0), 666)
        self.assertEqual(band_share_permille(cum, 1), 333)

    def test_shares_sum_close_to_1000(self):
        cum = zipf_cum_weights(9, 2)
        self.assertTrue(990 <= sum(band_share_permille(cum, i) for i in range(9)) <= 1000)


class StreamTests(unittest.TestCase):
    def test_same_seed_and_stream_gives_same_draws(self):
        first = [stream_rng(42, "sales_invoice").randrange(10**9) for _ in range(5)]
        second = [stream_rng(42, "sales_invoice").randrange(10**9) for _ in range(5)]
        self.assertEqual(first, second)

    def test_stream_name_changes_the_sequence(self):
        self.assertNotEqual(
            stream_rng(42, "sales_invoice").randrange(10**9),
            stream_rng(42, "purchase_invoice").randrange(10**9),
        )

    def test_seed_changes_the_sequence(self):
        self.assertNotEqual(
            stream_rng(42, "item").randrange(10**9),
            stream_rng(43, "item").randrange(10**9),
        )


if __name__ == "__main__":
    unittest.main()