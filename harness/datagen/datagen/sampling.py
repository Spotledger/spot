"""Integer-only random sampling.

random() returns floats and pow/log can differ between machines, so sampling
never touches floats: every distribution is a table of integer weights, and a
draw is randrange (an integer Random method) plus a binary search over the
cumulative weights. The same seed therefore yields identical choices on every
machine. RNG streams are derived per entity type with
hashlib.sha256(f"{seed}:{stream}"), so changing one entity's count never
changes the bytes another entity's stream produces.
"""

import hashlib
from bisect import bisect_right
from random import Random

ZIPF_SCALE = 10**12


class SamplingError(ValueError):
    """Raised when a distribution cannot be built from its parameters."""


def stream_rng(seed: int, stream: str):
    """One independent Random per entity type, derived from seed and stream."""
    digest = hashlib.sha256(f"{seed}:{stream}".encode("utf-8")).digest()
    return Random(int.from_bytes(digest, "big"))


def zipf_cum_weights(count: int, skew: int) -> list:
    """Cumulative integer weights for ranks 1..count with
    weight(rank) = ZIPF_SCALE // rank**skew."""
    if count < 1:
        raise SamplingError("count must be at least 1")
    if skew < 1:
        raise SamplingError("skew must be at least 1")
    cum_weights = []
    total = 0
    for rank in range(1, count + 1):
        total += ZIPF_SCALE // rank**skew
        cum_weights.append(total)
    return cum_weights


def triangular_cum_weights(minimum: int, peak: int, maximum: int) -> list:
    """Cumulative integer weights over the values minimum..maximum with a
    soft triangular peak at `peak` (used for lines per invoice)."""
    if not minimum <= peak <= maximum:
        raise SamplingError("need minimum <= peak <= maximum")
    span = maximum - minimum
    cum_weights = []
    total = 0
    for value in range(minimum, maximum + 1):
        total += 1 + max(0, span - abs(value - peak))
        cum_weights.append(total)
    return cum_weights


def weighted_index(rng, cum_weights) -> int:
    """Draw an index proportionally to its integer weight band, without floats."""
    return bisect_right(cum_weights, rng.randrange(cum_weights[-1]))


def band_share_permille(cum_weights, index: int) -> int:
    """Integer per-mille share of the weight band at `index` (0-based)."""
    total = cum_weights[-1]
    low = cum_weights[index - 1] if index else 0
    return (cum_weights[index] - low) * 1000 // total