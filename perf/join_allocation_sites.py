#!/usr/bin/env python3
"""Rank Rock source locations or implicitly boxed value types by measured allocator traffic."""
import argparse
from bisect import bisect_right
from collections import defaultdict
import csv
import json
from pathlib import Path
import re
import subprocess
import sys
from tempfile import TemporaryDirectory

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
HELPER = re.compile(r"_[0-9a-f]{8}_(?:vt|lct|ct)$")
BOXING = {"value-construction", "tuple-construction"}


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
            boxed = "", ""
            if tag == "pc-line" and pc_body:
                body = pc_body
                line, kind, index, source_line = fields
                source = f"{files[index]}:{source_line}"
                target = sources
            elif tag == "pc-source":
                body, line, kind, source = fields
                target = sources
                if kind == "generated-value-bridge":
                    boxed = HELPER.sub("", body), "helper"
            elif len(fields) >= 4 and re.fullmatch(r"[0-9a-f]{16}", tag):
                kind, source, body, line = fields[:4]
                target = census
                if kind in BOXING and len(fields) >= 9:
                    boxed = fields[7].rsplit("::", 1)[-1], fields[5]
            else:
                continue
            if SOURCE.fullmatch(source):
                target[body, int(line)].add((source, kind, *boxed))
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
        matches = [entry for entry in entries if compatible(entry[1], op)]
        if matches:
            locations, kinds, *boxed = map(set, zip(*matches))
            if len(locations) == 1:
                kind = kinds.pop() if len(kinds) == 1 else "multiple-kinds"
                if kind == "generated-value-bridge":
                    kind = ("generated-table-bridge" if op in ("TNEW", "TDUP")
                            else "generated-cdata-bridge")
                return locations.pop(), kind, *(
                    field.pop() if len(field) == 1 else "mixed" for field in boxed)
            break
    return "", "unknown", "", ""


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
        source = ("", {6: "vm-compiler", 7: "vm-collector", 9: "vm-runtime"}.get(event, "unknown"),
                  "", "")
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


def bridge_callers(run, lua_sha256, census):
    """Calls per (source, use) of each type's generated helpers in a --bridge-callers run."""
    lua = run / "main.lua"
    if sha256_file(lua) != lua_sha256:
        raise ValueError("the bridge caller run compiled different Lua")
    owners, lines = source_owners(lua)
    starts = sorted(owners)
    callers = defaultdict(lambda: defaultdict(int))
    for row in (run / "callers.tsv").read_text().splitlines():
        helper, line, calls = row.split("\t")
        line = int(line)
        first = starts[bisect_right(starts, line) - 1]
        body = owners[first]
        if "return function(" in lines[first - 1]:
            body = body.removeprefix("__rock_mk_")
        boxed = HELPER.sub("", helper)
        sites = {(source, use) for source, _, value_type, use in census.get((body, line - first + 1), ())
                 if value_type == boxed}
        site = sites.pop() if len(sites) == 1 else (body, "mixed" if sites else "unmapped")
        callers[boxed][site] += int(calls)
    return callers


def by_type(rows, callers):
    types = defaultdict(lambda: [0, 0, defaultdict(lambda: [0, 0])])
    for (source, kind, boxed, use), (count, size) in rows.items():
        if not boxed:
            continue
        entry = types[boxed]
        entry[0] += count
        entry[1] += size
        split = callers.get(boxed) if use == "helper" else None
        total = sum(split.values()) if split else 0
        for (site, site_use), calls in split.items() if total else [((source, use), 1)]:
            share = calls / total if total else 1
            entry[2][site, kind, site_use][0] += count * share
            entry[2][site, kind, site_use][1] += size * share
    return sorted(types.items(), key=lambda item: (-item[1][1], -item[1][0], item[0]))


def nobox_verdict(rock, project, declaration):
    path, line = declaration.rsplit(":", 1)
    path = Path(path)
    if not path.is_file():
        return "declaration not editable"
    original = path.read_text()
    lines = original.splitlines(keepends=True)
    index = int(line) - 1
    if "nobox" in lines[index]:
        return "declared nobox"
    lines[index], found = re.subn(r"\bvalue\b", "nobox value", lines[index], count=1)
    if not found:
        return "not a value declaration"
    try:
        path.write_text("".join(lines))
        with TemporaryDirectory() as output:
            result = subprocess.run([rock, "compile", project, "--output", f"{output}/main.lua"],
                                    capture_output=True, text=True)
    finally:
        path.write_text(original)
    if result.returncode == 0:
        return "accepted"
    log = result.stdout + result.stderr
    errors = re.findall(r"Error: (.*)", log)
    where = re.search(r"\[([^\[\]]+:\d+):\d+\]", log)
    first = f"{where.group(1)}: {errors[0]}" if where and errors else log.strip()[-200:]
    return f"rejected at {len(errors)} sites, first {first}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture_dir", type=Path)
    parser.add_argument("--luajit", type=Path, required=True)
    parser.add_argument("--by-type", action="store_true",
                        help="rank implicitly boxed value types instead of source locations")
    parser.add_argument("--callers", type=Path,
                        help="charge generated helpers to their callers from a --bridge-callers run")
    parser.add_argument("--nobox-check", nargs=2, metavar=("ROCK", "PROJECT"),
                        help="compile PROJECT once per ranked type declared nobox")
    parser.add_argument("--min-bytes", type=float, default=256,
                        help="bytes per frame below which a type is not nobox-checked")
    args = parser.parse_args()
    frames, rows = join(args.capture_dir, args.luajit)
    writer = csv.writer(sys.stdout, delimiter="\t", lineterminator="\n")
    if not args.by_type:
        writer.writerow(("source", "kind", "boxed type", "use", "requests/frame", "bytes/frame"))
        for (source, kind, boxed, use), (count, size) in sorted(
                rows.items(), key=lambda row: (-row[1][1], -row[1][0], row[0])):
            writer.writerow((source, kind, boxed, use, f"{count / frames:.2f}",
                             f"{size / frames:.2f}"))
        return
    census, sources = read_manifest(args.capture_dir / "manifest.txt")
    callers = {}
    if args.callers:
        meta = json.loads((args.capture_dir / "allocation-capture.json").read_text())
        callers = bridge_callers(args.callers, meta["lua_sha256"], census)
    declarations = defaultdict(set)
    for entries in sources.values():
        for source, kind, boxed, _ in entries:
            if kind == "generated-value-bridge":
                declarations[boxed].add(source)

    scanned = defaultdict(set)

    def declared(boxed):
        if boxed in declarations:
            return declarations[boxed]
        if not scanned:
            pattern = re.compile(r"^\s*(?:pub\s+)?(?:@\w+\s+)*value\s+(\w+)")
            for path in {source.rsplit(":", 1)[0] for entries in census.values()
                         for source, *_ in entries}:
                if Path(path).is_file():
                    for number, line in enumerate(Path(path).open(), 1):
                        if match := pattern.match(line):
                            scanned[match.group(1)].add(f"{path}:{number}")
        return scanned.get(boxed, set())
    writer.writerow(("type", "requests/frame", "bytes/frame", "nobox", "site", "kind", "use",
                     "site requests/frame", "site bytes/frame", "fix"))
    for boxed, (count, size, sites) in by_type(rows, callers):
        verdict = ""
        if args.nobox_check and size / frames >= args.min_bytes:
            found = declared(boxed)
            verdict = (nobox_verdict(*args.nobox_check, next(iter(found))) if len(found) == 1
                       else f"{len(found)} declarations")
        for (source, kind, use), (site_count, site_size) in sorted(
                sites.items(), key=lambda item: (-item[1][1], item[0])):
            fix = "store the fields in place" if use in ("global-store", "field-store") else ""
            writer.writerow((boxed, f"{count / frames:.2f}", f"{size / frames:.2f}", verdict,
                             source, kind, use, f"{site_count / frames:.2f}",
                             f"{site_size / frames:.2f}", fix))


if __name__ == "__main__":
    main()
