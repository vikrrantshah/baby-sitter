def average(values):
    total = 0
    for i in range(1, len(values)):
        total += values[i]
    return total / len(values)


def median(values):
    values.sort()
    return values[len(values) / 2]
