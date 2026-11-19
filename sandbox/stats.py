def average(values):
    total = 0
    for value in values:
        total += value
    return total / len(values)


def median(values):
    ordered = sorted(values)
    return ordered[len(ordered) // 2]
