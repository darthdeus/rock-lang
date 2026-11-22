#!/usr/bin/env python3
"""Rank Rock source locations by measured LuaJIT allocator traffic."""
import argparse
from collections import defaultdict
import csv
import json
from pathlib import Path
import re
import subprocess
import sys

from runtime_pairs import sha256_file
from site_diff import ALLOCATION_EVENTS, read_capture, source_owners


TABLE = {"tuple-construction", "vec-construction", "vec-column-construction",
         "vec-column-header", "struct-construction", "value-construction",
         "explicit-value-materialization", "enum-construction", "dynamic-enum-construction",
         "closure-environment", "coroutine-frame", "map-construction", "generated-value-bridge"}
CALL = {"ffi-array", "ffi-return-value", "ffi-view", "string-conversion", "string-format",
        "container-growth", "value-construction", "explicit-value-materialization",
        "vec-construction", "vec-column-construction", "vec-column-header", "map-construction",
        "opaque-call", "ffi-boundary", "generated-value-bridge"}
LOOKUP = {"ffi-view", "projection"}
STORE = {"container-growth"}
SOURCE = re.compile(r"^(?!<unknown>:).+:[1-9]\d*$")


def compatible(kind, op):
    if kind == "inline-lua":
        return True
    if op in ("TNEW", "TDUP"):
        return kind in TABLE
    if op.startswith(("CALL", "ITER")):
        return kind in CALL
    if op.startswith("TGET"):
        return kind in LOOKUP
    if op.startswith("TSET"):
        return kind in STORE
    if op == "CAT":
        return kind == "string-concat"
    if op == "FNEW":
        return kind == "closure-environment"
    return kind == "ffi-view" and op.startswith(("ADD", "SUB", "MUL", "DIV", "UNM"))


def read_manifest(path):
    census, sources = defaultdict(set), defaultdict(set)
    with path.open() as stream:
        pc_body = None
        files = {}
        for row in stream:
            tag, *fields = row.rstrip("\n").lstrip().split("\t")
            if tag == "pc-body":
                pc_body, = fields
                continue
            if tag == "pc-file":
                index, path = fields
                files[index] = path
                continue
            if tag == "pc-line" and pc_body:
                body = pc_body
                line, kind, index, source_line = fields
                source = f"{files[index]}:{source_line}"
                target = sources
            elif tag == "pc-source":
                body, line, kind, source = fields
                target = sources
            elif len(fields) >= 4 and re.fullmatch(r"[0-9a-f]{16}", tag):
                kind, source, body, line = fields[:4]
                target = census
            else:
                continue
            if SOURCE.fullmatch(source):
                target[body, int(line)].add((source, kind))
    return census, sources


def bytecode(vm, lua, coordinates):
    if not coordinates:
        return {}
    rows = subprocess.check_output([vm, "-E", Path(__file__).with_name("allocation_opcodes.lua"), lua],
                                   input="".join(f"{a} {b}\n" for a, b in sorted({
                                       (first, pc) for first, _, pc, _, _ in coordinates
                                   })), text=True)
    ops = defaultdict(list)
    for row in rows.splitlines():
        *key, op = row.split()
        ops[tuple(map(int, key))].append(op)
    if coordinates - ops.keys():
        raise ValueError("capture PCs do not match the Lua bytecode")
    return {key: ops[key][0] for key in coordinates if len(ops[key]) == 1}


def match_source(body, line, op, census, sources):
    for entries in (census.get((body, line), ()), sources.get((body, line), ()),
                    sources.get((body, 0), ())):
        matches = [(source, kind) for source, kind in entries if compatible(kind, op)]
        if matches:
            locations, kinds = map(set, zip(*matches))
            if len(locations) == 1:
                kind = kinds.pop() if len(kinds) == 1 else "multiple-kinds"
                if kind == "generated-value-bridge":
                    kind = ("generated-table-bridge" if op in ("TNEW", "TDUP")
                            else "generated-cdata-bridge")
                return locations.pop(), kind
            break
    return "", "unknown"


def capture_inputs(root, vm):
    vm = vm.resolve()
    lua = root / "main.lua"
    manifest = root / "manifest.txt"
    capture = root / "perf-summary.json.sites.tsv"
    meta = json.loads((root / "allocation-capture.json").read_text())
    for name, path in (("lua", lua), ("vm", vm), ("capture", capture), ("manifest", manifest)):
        if meta[f"{name}_sha256"] != sha256_file(path):
            raise ValueError(f"{name} differs from the captured artifact")
    frames = meta["frames"]
    if frames <= 0:
        raise ValueError("empty allocation window")

    return lua, manifest, capture, meta


def allocation_rows(vm, lua, owners_and_lines, sites, counts, census, sources, *, sunk=False):
    allocations = [row for row in counts if row[1] in ALLOCATION_EVENTS or sunk and row[1] == 3]
    coordinates = {sites[site][1:] for site, event, *_ in allocations
                   if site and event in (0, 1, 2, 3) and sites[site][0] == "main.lua"}
    ops = bytecode(vm, lua, coordinates)
    owners, lines = owners_and_lines
    for site, event, *metrics in allocations:
        source = "", {6: "vm-compiler", 7: "vm-collector", 9: "vm-runtime"}.get(event, "unknown")
        if site and event in (0, 1, 2, 3):
            chunk, *position = sites[site]
            first, line, *_ = position
            if chunk == "main.lua" and (op := ops.get(tuple(position))) and first in owners:
                body = owners[first]
                if "return function(" in lines[first - 1]:
                    body = body.removeprefix("__rock_mk_")
                source = match_source(body, line - first + 1, op, census, sources)
        yield site, event, source, *metrics


def check_totals(meta, totals):
    if totals != (meta["requests"], meta["bytes"]):
        raise ValueError("site allocations do not reconcile with the allocator window")


def join(root, vm):
    lua, manifest, capture, meta = capture_inputs(root, vm)
    sites, _, _, counts = read_capture(capture)
    rows = defaultdict(lambda: (0, 0))
    for _, _, source, _, _, count, size in allocation_rows(
            vm, lua, source_owners(lua), sites, counts, *read_manifest(manifest)):
        previous_count, previous_size = rows[source]
        rows[source] = previous_count + count, previous_size + size
    check_totals(meta, tuple(sum(metrics[i] for metrics in rows.values()) for i in (0, 1)))
    return meta["frames"], dict(rows)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture_dir", type=Path)
    parser.add_argument("--luajit", type=Path, required=True)
    args = parser.parse_args()
    frames, rows = join(args.capture_dir, args.luajit)
    writer = csv.writer(sys.stdout, delimiter="\t", lineterminator="\n")
    writer.writerow(("source", "kind", "requests/frame", "bytes/frame"))
    for (source, kind), (count, size) in sorted(rows.items(),
                                                key=lambda row: (-row[1][1], -row[1][0], row[0])):
        writer.writerow((source, kind, f"{count / frames:.2f}", f"{size / frames:.2f}"))


if __name__ == "__main__":
    main()
