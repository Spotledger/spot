"""Deterministic syllable-based names.

Every name is composed from the syllable table below, written for this
generator. No real people or companies are named and no list is copied from
anywhere (ADR-32). The same rng state always yields the same name.
"""

_SYLLABLES = (
    "ba", "be", "cha", "da", "den", "fen", "gil", "ha", "hul", "jis",
    "ka", "kel", "lo", "ma", "mav", "na", "nor", "pa", "pel", "qui",
    "ra", "ri", "sa", "so", "ta", "tol", "ve", "ven", "za", "zo",
)

_COMPANY_SUFFIXES = ("Group", "Works", "Trading", "Supply", "Logistics", "Services")
_ITEM_SUFFIXES = ("Mix", "Blend", "Base", "Prime", "Core")
_PLACE_KINDS = ("Depot", "Store", "Yard")


def _word(rng, syllables: int) -> str:
    return "".join(rng.choice(_SYLLABLES) for _ in range(syllables)).capitalize()


def unique_name(rng, used: set, make) -> str:
    """Return a fresh name from make(rng), retrying on collision. After 64
    attempts it falls back to a numbered suffix so generation terminates."""
    for _ in range(64):
        candidate = make(rng)
        if candidate not in used:
            used.add(candidate)
            return candidate
    base = make(rng)
    suffix = 1
    fallback = f"{base} {suffix}"
    while fallback in used:
        suffix += 1
        fallback = f"{base} {suffix}"
    used.add(fallback)
    return fallback


def company_name(rng) -> str:
    return f"{_word(rng, rng.randrange(2, 4))} {rng.choice(_COMPANY_SUFFIXES)}"


def party_name(rng) -> str:
    return _word(rng, rng.randrange(2, 4))


def item_name(rng) -> str:
    return f"{_word(rng, 2)} {rng.choice(_ITEM_SUFFIXES)}"


def warehouse_name(rng, sequence: int) -> str:
    return f"{_word(rng, 2)} {rng.choice(_PLACE_KINDS)} {sequence}"