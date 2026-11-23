#!/usr/bin/env python3
"""Explain trace and allocation changes between two Rock perf captures."""
import argparse
from collections import Counter, defaultdict
from functools import cache
from pathlib import Path
import re

from join_allocation_sites import allocation_rows, capture_inputs, check_totals, read_manifest
from site_diff import ALLOCATION_EVENTS, EVENTS, read_capture, source_owners


SPECIALIZATION = re.compile(r"_[0-9a-f]{8}_|_spec_")
LAMBDA = re.compile(r"__lambda\d+$")
EMITTED_NAME = re.compile(r"\b(?:__rock_mk_)?\w+(?:_[0-9a-f]{8}_|_spec_)\w+")


def short_function(name):
    base = name.removeprefix("__rock_mk_")
    parts = SPECIALIZATION.split(base, maxsplit=1)
    if len(parts) == 1:
        return base
    closure = LAMBDA.search(parts[1])
    return parts[0] + (closure[0] if closure else "")


def load(root, vm):
    lua, manifest, capture, meta = capture_inputs(root, vm)
    attempts, costs = {}, []
    sites, traces, reasons, counts = read_capture(capture, attempts=attempts, costs=costs)
    owners, lines = source_owners(lua)
    census, sources = read_manifest(manifest)
    prefixes = sorted(meta.get("source_roots", {}).items(), key=lambda pair: -len(pair[0]))

    def source_path(source):
        for prefix, label in prefixes:
            if source.startswith(prefix + "/"):
                suffix = source[len(prefix) + 1:]
                return f"{label}/{suffix}" if label else suffix
        return source

    @cache
    def function(ident):
        if not ident:
            return "(VM)"
        chunk, first, *_ = sites[ident]
        return owners.get(first, f"{chunk}:{first}") if chunk == "main.lua" else f"{chunk}:{first}"

    @cache
    def location(ident):
        owner = function(ident)
        if not ident:
            return owner
        chunk, first, line, pc, _, _ = sites[ident]
        if chunk == "main.lua":
            for mapping in (sources, census):
                candidates = {source for source, *_ in mapping.get((owner, line - first + 1), ())}
                if len(candidates) == 1:
                    return f"{owner} [{source_path(next(iter(candidates)))}]"
        return f"{owner}@{pc}"

    lineage = {0: ""}
    compiled, aborts = defaultdict(Counter), defaultdict(Counter)
    exits = defaultdict(set)
    phases = Counter()
    for ident, attempt in attempts.items():
        parent = lineage[attempt.parent]
        start = location(attempt.start)
        lineage[ident] = f"{parent} > {start}" if parent else start
        if parent:
            exits[lineage[ident]].add(attempt.exit)
        path = tuple(map(function, attempt.path))
        if ident in traces:
            phases[attempt.end_phase] += 1
            compiled[lineage[ident]][(location(attempt.end), attempt.ir, attempt.constants,
                                       attempt.slots, path)] += 1
        elif attempt.reason >= 0 and attempt.end_phase == 1:
            aborts[lineage[ident]][(reasons[attempt.reason], location(attempt.end),
                                    attempt.ir, attempt.constants, attempt.slots, path)] += 1

    allocations = defaultdict(Counter)
    compiler, compile_sites = Counter(), Counter()
    origins = Counter()
    totals = [0, 0]
    for site, event, (source, kind, *_), trace, phase, count, size in allocation_rows(
            vm, lua, (owners, lines), sites, counts, census, sources, sunk=True):
        if event in ALLOCATION_EVENTS:
            totals[0] += count
            totals[1] += size
            origin = EVENTS[event] if event in (6, 7, 9) else "game"
            origins[origin] += size
        if event == 6:
            attempt = attempts.get(trace)
            outcome = "compiled" if trace in traces else (
                reasons[attempt.reason] if attempt and attempt.reason >= 0 else "other")
            route = ("side" if attempt and attempt.parent and attempt.exit >= 0 else
                     "stitched" if attempt and attempt.parent else "root")
            compiler[(route, outcome, ("record", "optimize", "assemble")[phase])] += size
            if trace:
                compile_sites[(lineage[trace], outcome)] += size
        else:
            where = f"{source_path(source)} ({kind})" if source else location(site)
            if event in (7, 9):
                where = kind
            key = EVENTS[event], lineage[trace]
            n, b = allocations[where].get(key, (0, 0))
            allocations[where][key] = n + count, b + size
    check_totals(meta, tuple(totals))

    work = defaultdict(lambda: (0,) * 6)
    for site, *values in costs:
        name = function(site)
        work[name] = tuple(a + b for a, b in zip(work[name], values))
    return {"traces": compiled, "aborts": aborts, "allocations": allocations, "costs": work,
            "compiler": compiler, "compile_sites": compile_sites, "exits": exits, "phases": phases,
            "origins": origins,
            "frames": meta["frames"], "window_start": meta.get("window_start"),
            "late_window": meta.get("late_window")}


def shown(rows, limit, focus):
    for row in rows:
        display = EMITTED_NAME.sub(lambda match: short_function(match[0]), row)
        if not focus or focus.search(row) or focus.search(display):
            yield display
            limit -= 1
            if limit == 0:
                break


def compare(base, candidate, limit, focus=None):
    if any(base.get(key) != candidate.get(key) for key in ("frames", "window_start", "late_window")):
        raise ValueError("captures use different allocation windows")
    if frames := base.get("frames"):
        start = base.get("window_start")
        label = "Late allocation window" if base.get("late_window") else "Allocation window"
        when = f" from GC frame {start}" if start is not None else ""
        yield f"{label} ({frames} frames{when})"

    def with_exits(key):
        a, b = (arm.get("exits", {}).get(key, set()) for arm in (base, candidate))
        ancestry = key.split(" > ")
        if len(ancestry) > 4:
            key = " > ".join([ancestry[0], f"... ({len(ancestry) - 3})", *ancestry[-2:]])
        if not a and not b:
            return key

        def fmt(exits):
            return "/".join("stitch" if n == -1 else str(n) for n in sorted(exits)) or "none"

        exits = fmt(a) if a == b else f"{fmt(a)} -> {fmt(b)}"
        return f"{key} [parent exit {exits}]"

    before, after = base["traces"], candidate["traces"]
    added, removed = after.keys() - before.keys(), before.keys() - after.keys()
    changed = {key for key in before.keys() & after.keys() if before[key] != after[key]}
    yield f"Trace lineages: {len(added)} added, {len(removed)} removed, {len(changed)} changed"
    for index, label in enumerate(("before window", "in window", "after window")):
        a, b = (arm.get("phases", {}).get(index, 0) for arm in (base, candidate))
        if a or b:
            yield f"  Compiles {label}: {a:,} -> {b:,}"
    for keys, kind, values in ((added, "added", after), (removed, "removed", before),
                               (changed, "changed", after)):
        rows = []
        for key in sorted(keys, key=lambda k: (-sum(values[k].values()), k)):
            signature = max(values[key], key=lambda item: (values[key][item], item))
            end, ir, constants, slots, path = signature
            shape = f"{ir} IR, {constants} constants, {slots} slots"
            if kind == "changed":
                old = max(before[key], key=lambda item: (before[key][item], item))
                shape = (f"{old[1]} -> {ir} IR, {old[2]} -> {constants} constants, "
                         f"{old[3]} -> {slots} slots")
            functions = list(dict.fromkeys(path))
            suffix = f" +{len(functions) - 6}" if len(functions) > 6 else ""
            crossing = f", crosses {', '.join(functions[:6])}{suffix}" if functions else ""
            summary = f"ends at {end}, {shape}{crossing}"
            rows.append(f"  {kind}: {with_exits(key)} ({summary})")
        yield from shown(rows, limit, focus)

    before, after = base["aborts"], candidate["aborts"]
    yield "Abort reasons in window"
    for reason in sorted({row[0] for arm in (before, after) for rows in arm.values() for row in rows}):
        a, b = (sum(n for rows in arm.values() for row, n in rows.items() if row[0] == reason)
                for arm in (before, after))
        if a != b:
            yield f"  {reason}: {a:,} -> {b:,}"
    shifts = []
    for key in before.keys() | after.keys():
        a, b = Counter(before.get(key, {})), Counter(after.get(key, {}))
        endpoints = []
        for rows in (a, b):
            endpoint = Counter()
            for row, count in rows.items():
                endpoint[row[:2]] += count
            endpoints.append(endpoint)
        if endpoints[0] != endpoints[1]:
            lost, gained = endpoints[0] - endpoints[1], endpoints[1] - endpoints[0]
            moved = sum(lost.values()) + sum(gained.values())
            old = Counter({row: count for row, count in (a - b).items() if row[:2] in lost})
            new = Counter({row: count for row, count in (b - a).items() if row[:2] in gained})
            shifts.append((bool(old and new), moved, key, old, new))

    def outcome(rows):
        if not rows:
            return "none"
        representative = max(rows, key=lambda item: (rows[item], item))
        reason, end, ir, constants, slots, path = representative
        count = rows[representative]
        more = f", +{len(rows) - 1} outcomes" if len(rows) > 1 else ""
        tail = f", last crossed {' > '.join(path[-3:])}" if path else ""
        return f"{count}x {reason} at {end} ({ir} IR, {constants} constants, {slots} slots{tail}{more})"

    yield from shown([f"  {with_exits(key)}: {outcome(a)} -> {outcome(b)}"
                      for _, _, key, a, b in sorted(shifts, key=lambda r: (-r[0], -r[1], r[2]))],
                     limit, focus)

    before, after = base["compiler"], candidate["compiler"]
    a, b = sum(before.values()), sum(after.values())
    yield f"JIT compiler allocation bytes: {a:,} -> {b:,} ({b - a:+,})"
    for origin in ("game", "runtime", "collector"):
        a, b = (arm.get("origins", {}).get(origin, 0) for arm in (base, candidate))
        if a or b:
            yield f"  {origin} allocation bytes: {a:,} -> {b:,} ({b - a:+,})"
    for key in sorted(before.keys() | after.keys(),
                      key=lambda k: (-abs(after.get(k, 0) - before.get(k, 0)), k))[:limit]:
        a, b = before.get(key, 0), after.get(key, 0)
        if a != b:
            yield f"  {' / '.join(key)}: {a:,} -> {b:,} ({b - a:+,})"
    before, after = (arm.get("compile_sites", {}) for arm in (base, candidate))
    changed_bytes = []
    for key in before.keys() | after.keys():
        a, b = before.get(key, 0), after.get(key, 0)
        if a != b:
            changed_bytes.append((abs(b - a), key, a, b))
    yield from shown([f"  {with_exits(site)} ({outcome}): {a:,} -> {b:,} B"
                      for _, (site, outcome), a, b in sorted(changed_bytes, key=lambda row: (-row[0], row[1]))],
                     limit, focus)

    before, after = base["allocations"], candidate["allocations"]
    yield "Allocations by source and execution mode (requests)"
    shifts = []
    for site in before.keys() | after.keys():
        a, b = before.get(site, {}), after.get(site, {})
        keys = a.keys() | b.keys()
        modes = sorted({mode for mode, _ in keys})
        changes = []
        modes_changed = 0
        for mode in modes:
            counts = [sum(n for (event, _), (n, _) in rows.items() if event == mode)
                      for rows in (a, b)]
            if counts[0] != counts[1]:
                changes.append(f"{mode} {counts[0]:,} -> {counts[1]:,}")
                if mode != "sunk-ir":
                    modes_changed += abs(counts[1] - counts[0])
        delta_bytes = sum(size for _, size in b.values()) - sum(size for _, size in a.values())
        score = sum(abs(a.get(key, (0, 0))[0] - b.get(key, (0, 0))[0]) for key in keys
                    if key[0] != "sunk-ir")
        if score or delta_bytes or changes:
            traces = sorted((-abs(b.get(key, (0, 0))[0] - a.get(key, (0, 0))[0]), key)
                            for key in keys if key[1])
            for _, (mode, trace) in traces[:2]:
                n, m = a.get((mode, trace), (0, 0))[0], b.get((mode, trace), (0, 0))[0]
                if n != m:
                    changes.append(f"{mode} {n:,} -> {m:,} on {with_exits(trace)}")
            detail = ", " + ", ".join(changes) if changes else ""
            shifts.append((-modes_changed, -abs(delta_bytes), -score, site, f"  {site}: {delta_bytes:+,} B{detail}"))
    yield from shown([row for *_, row in sorted(shifts)],
                     limit, focus)

    before, after = base["costs"], candidate["costs"]
    if not before and not after:
        return
    if not before or not after:
        raise ValueError("bytecode work requires both captures to have cost data")
    yield "Bytecode work (interpreted / compiled / recorded by JIT)"
    for start, label in ((0, "before window"), (3, "in window")):
        totals = [tuple(sum(row[start + i] for row in arm.values()) for i in range(3))
                  for arm in (before, after)]
        yield f"  {label}: " + " / ".join(f"{a:,} -> {b:,}" for a, b in zip(*totals))
    shifts = []
    for name in before.keys() | after.keys():
        a, b = (rows.get(name, (0,) * 6)[3:] for rows in (before, after))
        score = sum(abs(x - y) for x, y in zip(a, b))
        if score:
            shifts.append((score, name, f"  {name}: " + ", ".join(
                f"{x:,} -> {y:,} {mode}" for x, y, mode in zip(a, b, ("interpreted", "compiled", "recorded")))))
    yield from shown([row for _, _, row in sorted(shifts, key=lambda r: (-r[0], r[1]))],
                     limit, focus)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("candidate", type=Path)
    parser.add_argument("--luajit", required=True, type=lambda p: Path(p).resolve())
    parser.add_argument("--limit", default=3, type=int)
    parser.add_argument("--focus", type=re.compile, help="show changed rows matching this regex")
    args = parser.parse_args()
    if args.limit < 1:
        parser.error("--limit must be positive")
    try:
        for row in compare(load(args.base, args.luajit), load(args.candidate, args.luajit),
                           args.limit, args.focus):
            print(row)
    except (ValueError, OSError, KeyError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
