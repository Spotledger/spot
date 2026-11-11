def rotate_left(values, amount):
    if not values:
        return []
    offset = amount % len(values)
    return values[offset:] + values[:offset]


def pair_totals(values):
    result = []
    for index in range(0, len(values), 2):
        second = values[index + 1] if index + 1 < len(values) else 0
        result.append(values[index] + second)
    return result


def has_repeat(values):
    return len(values) != len(set(values))


def partition_values(values, marker):
    left = []
    right = []
    for value in values:
        if value < marker:
            left.append(value)
        else:
            right.append(value)
    return left, right
