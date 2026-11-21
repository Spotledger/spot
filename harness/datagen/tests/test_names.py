"""Names are invented from our own syllable table (ADR-32)."""

import re
import sys
import unittest
from pathlib import Path
from random import Random

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from datagen import names

_WORD = re.compile(r"[A-Za-z]+")


class NameShapeTests(unittest.TestCase):
    def test_company_names_use_our_suffixes(self):
        rng = Random(1)
        for _ in range(50):
            name = names.company_name(rng)
            word, suffix = name.rsplit(" ", 1)
            self.assertIn(suffix, names._COMPANY_SUFFIXES)
            self.assertRegex(word, _WORD)

    def test_words_are_syllable_concatenations(self):
        rng = Random(2)
        for _ in range(50):
            company = names.company_name(rng)
            self.assertTrue(_made_of_syllables(company.split()[0].lower()), company)
            party = names.party_name(rng)
            self.assertTrue(_made_of_syllables(party.lower()), party)
            item = names.item_name(rng)
            self.assertTrue(_made_of_syllables(item.split()[0].lower()), item)

    def test_item_and_warehouse_names(self):
        rng = Random(3)
        name = names.item_name(rng)
        self.assertIn(name.rsplit(" ", 1)[1], names._ITEM_SUFFIXES)
        place = names.warehouse_name(rng, 7)
        self.assertTrue(place.endswith(" 7"))
        self.assertIn(place.rsplit(" ", 2)[1], names._PLACE_KINDS)

    def test_no_digits_in_entity_names(self):
        rng = Random(4)
        for _ in range(50):
            for name in (names.company_name(rng), names.party_name(rng), names.item_name(rng)):
                self.assertNotRegex(name, r"\d")


def _made_of_syllables(word: str) -> bool:
    """True when every prefix of `word` is built from our syllable table,
    so no outside word lists can have leaked in (ADR-32)."""
    for cut in range(1, len(word)):
        if not any(
            syllable.startswith(word[:cut]) or word[:cut].startswith(syllable)
            for syllable in names._SYLLABLES
        ):
            return False
    return True


class DeterminismTests(unittest.TestCase):
    def test_same_rng_state_gives_same_names(self):
        for maker in (names.company_name, names.party_name, names.item_name):
            first = [maker(Random(42)) for _ in range(20)]
            second = [maker(Random(42)) for _ in range(20)]
            self.assertEqual(first, second)


class UniqueNameTests(unittest.TestCase):
    def test_returns_distinct_names(self):
        rng = Random(5)
        used = set()
        made = {names.unique_name(rng, used, names.party_name) for _ in range(100)}
        self.assertEqual(len(made), 100)

    def test_falls_back_to_numbered_suffix(self):
        rng = Random(6)
        made = names.unique_name(rng, {"X"}, lambda rng_: "X")
        self.assertEqual(made, "X 1")


if __name__ == "__main__":
    unittest.main()