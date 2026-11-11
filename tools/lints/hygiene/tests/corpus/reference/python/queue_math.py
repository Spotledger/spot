def weight_window(samples, cap):
    total = 0
    for sample in samples:
        if sample > cap:
            total += sample - cap
        else:
            total += sample
    return total


def clamp_window(value, low, high):
    if value < low:
        return low
    if value > high:
        return high
    return value


def label_window(name, count):
    if count == 0:
        return "empty"
    return name + ":" + str(count)
