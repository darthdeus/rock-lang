"""Common reporting for alternating, checksum-verified benchmark pairs."""

from statistics import median


def pairs(rounds, modes, run):
    samples = []
    for pair in range(rounds):
        order = modes if pair % 2 == 0 else reversed(modes)
        for mode in order:
            samples.append(dict(pair=pair, mode=mode, **run(mode)))
    if len({sample["sink"] for sample in samples}) != 1:
        raise RuntimeError(f"Benchmark sinks differ: {[sample['sink'] for sample in samples]}")
    return samples


def spread(values):
    return median(values), min(values), max(values)
