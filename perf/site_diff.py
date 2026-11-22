#!/usr/bin/env python3
"""Diff LuaJIT measurement sites across matched, interleaved runs."""
import argparse
from collections import defaultdict
import csv
from pathlib import Path
import re
from statistics import median
import sys
from typing import NamedTuple


EVENTS = ("interpreter", "trace", "restored", "sunk-ir", "abort", "exit",
          "compiler", "collector", "wide-trace", "runtime")
ALLOCATION_EVENTS = frozenset((0, 1, 2, 6, 7, 9))
FUNCTION = re.compile(r'^\s*(?:(?:local )?function ([\w.]+)\s*\(|([\w.]+)\s*=\s*function\b)')


def source_owners(path):
    owners = {}
    lines = path.read_text().split("\n")
    for number, line in enumerate(lines, 1):
        match = FUNCTION.match(line)
        if match:
            owners[number] = match.group(1) or match.group(2)
    return owners, lines


class Attempt(NamedTuple):
    start: int
    parent: int
    exit: int
    end: int
    reason: int
    ir: int
    constants: int
    slots: int
    start_phase: int
    end_phase: int
    path: tuple[int, ...]


def read_capture(path, *, attempts=None, costs=None):
    sites, traces, reasons, counts = {}, {}, {}, []
    with path.open() as file:
        header = file.readline().rstrip("\n").split("\t")
        if (header not in (["rock-sites-v1", "0"], ["rock-sites-v2", "0"])
                or attempts is not None and header[0] != "rock-sites-v2"):
            raise ValueError(f"invalid or overflowed site capture: {path}")
        for line in file:
            tag, *values = line.rstrip("\n").split("\t")
            if tag == "reason":
                code, reason = values
                reasons[int(code)] = reason
            elif tag == "site":
                ident, first, line_number, pc, params, slots, chunk = values
                sites[int(ident)] = (chunk, *map(int, (first, line_number, pc, params, slots)))
            elif tag == "trace":
                ident, *fields = map(int, values)
                traces[ident] = tuple(fields)
            elif tag == "attempt" and header[0] == "rock-sites-v2":
                if attempts is not None:
                    ident, *fields = map(int, values)
                    attempts[ident] = Attempt(*fields[:10], tuple(fields[10:]))
            elif tag == "cost" and header[0] == "rock-sites-v2":
                if costs is not None:
                    costs.append(tuple(map(int, values)))
            elif tag == "count":
                counts.append(tuple(map(int, values)))
            else:
                raise ValueError(f"unknown site record {tag!r} in {path}")
    return sites, traces, reasons, counts


def joined_sites(path, owners, by_prototype):
    by_first, lines = owners
    sites, traces, reasons, counts = read_capture(path)

    def label(ident):
        if ident == 0:
            return "(unattributed)", 0, 0, ""
        chunk, first, line, pc, params, _ = sites[ident]
        if chunk == "main.lua" and first in by_first:
            prototype = f"{by_first[first]}/{params}"
            text = lines[line - 1].strip() if 1 <= line <= len(lines) else ""
            return (prototype, 0, 0, "") if by_prototype else (prototype, line - first, pc, text)
        prototype = f"{chunk}:{first}/{params}"
        return (prototype, 0, 0, "") if by_prototype else (prototype, line - first, pc, "")

    def location(ident):
        prototype, line, pc, _ = label(ident)
        return prototype if by_prototype else f"{prototype}:{line}@{pc}"

    result = defaultdict(lambda: (0, 0))
    for ident, raw_kind, other, detail, count, size in counts:
        kind = EVENTS[raw_kind]
        if kind == "abort":
            context = location(other)
            detail = reasons.get(detail, str(detail))
        elif kind in ("exit", "wide-trace"):
            # Trace IDs are reused and differ between arms. A compiled trace's
            # start and exit target survive ASLR and process-local numbering.
            start, parent, exitno, *_ = traces.get(other, (0, 0, 0))
            context = location(start)
            if parent:
                context += f" from {location(traces[parent][0])} exit {exitno}"
            detail = str(detail)
        else:
            context, detail = "", ""
        owner, line, pc, text = label(ident)
        key = kind, owner, line, pc, context, detail, text
        old_count, old_size = result[key]
        result[key] = old_count + count, old_size + size
    return dict(result)


def differences(base, candidate):
    keys = set().union(*base, *candidate)
    for key in keys:
        a = [run.get(key, (0, 0)) for run in base]
        b = [run.get(key, (0, 0)) for run in candidate]
        an, ab = map(median, zip(*a))
        bn, bb = map(median, zip(*b))
        dn = median(y[0] - x[0] for x, y in zip(a, b))
        db = median(y[1] - x[1] for x, y in zip(a, b))
        if an != bn or ab != bb or dn or db:
            yield (*key, an, bn, dn, ab, bb, db)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base_lua", type=Path)
    parser.add_argument("candidate_lua", type=Path)
    parser.add_argument("--base", type=Path, nargs="+", required=True, help="base captures in pair order")
    parser.add_argument("--candidate", type=Path, nargs="+", required=True, help="candidate captures in pair order")
    parser.add_argument("--by-prototype", action="store_true", help="aggregate PCs within each emitted function")
    args = parser.parse_args()
    if len(args.base) != len(args.candidate):
        parser.error("supply the same number of captures in both arms")
    arms = []
    for lua, paths in ((args.base_lua, args.base), (args.candidate_lua, args.candidate)):
        owners = source_owners(lua)
        arms.append([joined_sites(path, owners, args.by_prototype) for path in paths])
    writer = csv.writer(sys.stdout, delimiter="\t", lineterminator="\n")
    writer.writerow(("event", "prototype", "relative_line", "pc", "trace_origin", "detail",
                     "lua", "base_count", "candidate_count", "paired_count_delta", "base_bytes",
                     "candidate_bytes", "paired_bytes_delta"))
    writer.writerows(sorted(differences(*arms),
                            key=lambda row: (-abs(row[-1]), -abs(row[9]), row[:7])))


if __name__ == "__main__":
    main()
