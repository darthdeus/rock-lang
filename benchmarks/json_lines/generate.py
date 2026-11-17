#!/usr/bin/env python3
"""Generate the deterministic JSON Lines input used by this benchmark."""

import hashlib
import argparse
import json
from pathlib import Path


DEFAULT_OUTPUT = Path("target/json137/input.jsonl")
LINE_COUNT = 1826


def record(index):
    row = {
        "id": index + 1,
        "kind": ("telemetry", "snapshot", "event", "sample")[index % 4],
        "title": f'Record {index:04d}: "north" / caf\u00e9 / \u96ea',
        "timestamp": 1_700_000_000 + index * 37,
        "enabled": index % 7 != 0,
        "count": (index * 17) % 100_003,
        "score": (index % 1000) / 10.0,
        "ratio": 1e-7 if index % 19 == 0 else (index % 91) / 10.0,
        "location": {
            "x": ((index * 29) % 2001 - 1000) / 10.0,
            "y": ((index * 31) % 1601 - 800) / 10.0,
            "z": ((index * 43) % 1201 - 600) / 10.0,
            "unit": "m",
        },
        "meta": {
            "source": ("sensor-A", "gateway/2", "archive", "mobile")[index % 4],
            "attempt": index % 6,
            "verified": index % 3 != 1,
            "labels": ["north", "primary", f"batch-{index % 23}"],
        },
        "tags": ["jsonl", ("edge", "core", "relay")[index % 3]],
        "note": f'Escapes \\ and "quote"; line\nmarker; tab\tmarker; \u03a9',
        "state": ("queued", "active", "complete", "retry")[index % 4],
        "source_id": 50_000 + index % 193,
    }
    if index % 4 == 0:
        length = 8 + (index // 4) % 57
        values = []
        for offset in range(length):
            raw = ((index + 1) * (offset + 3) % 100_003 - 50_001) / 1000.0
            if offset == 0 and index % 8 == 0:
                raw = 1e-9 * (index + 1)
            elif offset == 1 and index % 12 == 0:
                raw = 1e12 + index * 0.25
            values.append(raw)
        row["samples"] = values
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", nargs="?", type=Path, default=DEFAULT_OUTPUT)
    output = parser.parse_args().output
    encoded = "".join(
        json.dumps(record(index), ensure_ascii=True, separators=(",", ":")) + "\n"
        for index in range(LINE_COUNT)
    ).encode("ascii")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(encoded)
    print(f"lines={LINE_COUNT} bytes={len(encoded)} sha256={hashlib.sha256(encoded).hexdigest()}")


if __name__ == "__main__":
    main()
