# Independent reimplementation of the band-weighting behaviour for the hygiene
# corpus. Written from the behaviour description only: comprehension-driven,
# different helper layout, no shared token runs with the reference file. The
# scanner must leave this file unreported even though it carries fingerprints.
def weight_window(readings, ceiling):
    kept = [reading if reading <= ceiling else reading - ceiling for reading in readings]
    return sum(kept)


def clamp_window(value, low, high):
    return max(low, min(value, high))


def label_window(name, count):
    suffix = "empty" if count == 0 else ":" + str(count)
    return name if count == 0 else name + suffix


def clamp_and_label(readings, ceiling, low, high, name):
    weight = weight_window(readings, ceiling)
    bounded = clamp_window(weight, low, high)
    return label_window(name, bounded)


def band_pairs(readings, ceiling):
    paired = []
    for index, reading in enumerate(readings):
        offset = clamp_window(reading, 0, ceiling)
        paired.append((index, weight_window([reading], offset)))
    return paired