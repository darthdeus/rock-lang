#!/usr/bin/env python3
"""Extract emitted Stone writers into a line-mapped LuaJIT fixture."""

from __future__ import annotations

import argparse
import json
import re
import statistics
import subprocess
import sys
from pathlib import Path

WRITERS = (
    {"name": "__rect_batch_push_textured_quad4", "prefix": "__rect_batch_push_textured_quad4", "mode": "function", "batch": False, "vertices": 6, "stride": 6},
    {"name": "__rect_batch_push_triangle_verts", "prefix": "__rect_batch_push_triangle_verts", "mode": "function", "batch": False, "vertices": 3, "stride": 6},
    {"name": "__sprite_mesh_push_verts", "prefix": "__sprite_mesh_push_verts", "mode": "function", "batch": False, "vertices": 6, "stride": 6},
    {"name": "__sprite_mesh_push_quad4", "prefix": "__sprite_mesh_push_quad4", "mode": "function", "batch": False, "vertices": 6, "stride": 6},
    {"name": "__sprite_mesh_push_segment", "prefix": "__sprite_mesh_push_segment", "mode": "function", "batch": False, "vertices": 6, "stride": 6},
    {"name": "__queue_text_quad", "prefix": "__queue_text_quad", "mode": "function", "batch": False, "vertices": 6, "stride": 6},
    {"name": "__ui_sprite_push", "prefix": "__ui_sprite_push", "mode": "function", "batch": True, "batch_kind": "verts", "vertices": 6, "stride": 6, "exclude": ("__ui_sprite_push_rotated",)},
    {"name": "__ui_sprite_push_rotated", "prefix": "__ui_sprite_push_rotated", "mode": "function", "batch": True, "batch_kind": "verts", "vertices": 6, "stride": 6},
    {"name": "emit_textured_quad", "prefix": "__rock_mk_emit_textured_quad", "mode": "factory", "batch": True, "batch_kind": "verts", "vertices": 6, "stride": 6, "state_field": "texel_bleed_prevention"},
    {"name": "ui_push_sprite_quad", "prefix": "__rock_mk_ui_push_sprite_quad", "mode": "factory", "batch": True, "batch_kind": "verts", "vertices": 6, "stride": 6},
    {"name": "rect_batch_push_circle", "prefix": "rect_batch_push_circle", "mode": "function", "batch": True, "batch_kind": "rect", "vertices": 96, "stride": 96},
    {"name": "append_vertex", "prefix": "append_vertex", "mode": "function", "batch": True, "batch_kind": "verts", "vertices": 1, "stride": 1},
)

# The emitter writes function declarations and factory assignments at column 0.
HEADER_RE = re.compile(
    r"^(?:local\s+function\s+[^\s(]+\s*\(|"
    r"function\s+[^\s(]+\s*\(|"
    r"[A-Za-z_][\w.]*\s*=\s*function\s*\()",
    re.MULTILINE,
)
PRELUDE_RE = re.compile(r"^if jit and jit\.opt then .*jit\.opt\.start\(.*\) end$", re.MULTILINE)
CALL_RE = re.compile(r"\b(__[A-Za-z_][A-Za-z_0-9]*)\s*\(")


def matches_header(header_line: str, target: dict) -> bool:
    prefix = re.escape(target["prefix"])
    excluded = target.get("exclude", ())
    if target["mode"] == "factory":
        match = re.match(prefix + r"_[^\s=]+\s*=\s*function\s*\(", header_line)
        symbol_match = re.match(r"([^\s=]+)\s*=\s*function", header_line)
    else:
        match = re.match(r"function\s+" + prefix + r"_[^\s(]+\s*\(", header_line)
        symbol_match = re.match(r"function\s+([^\s(]+)", header_line)
    if not match or not symbol_match:
        return False
    symbol = symbol_match.group(1)
    return not any(symbol.startswith(name + "_") for name in excluded)


def find_chunks(source: str, targets: tuple[dict, ...], source_name: str):
    headers = list(HEADER_RE.finditer(source))
    chunks = {}
    for target in targets:
        found = []
        for index, header in enumerate(headers):
            line_end = source.find("\n", header.start())
            if line_end < 0:
                line_end = len(source)
            header_line = source[header.start():line_end]
            if matches_header(header_line, target):
                end = headers[index + 1].start() if index + 1 < len(headers) else len(source)
                found.append((header, end, header_line))
        if len(found) != 1:
            raise ValueError(f"{source_name}: expected one top-level {target['prefix']}_ function, found {len(found)}")
        header, end, header_line = found[0]
        chunk = source[header.start():end].rstrip() + "\n"
        symbol_pattern = r"function\s+([^\s(]+)" if target["mode"] == "function" else r"([^\s=]+)\s*=\s*function"
        symbol = re.match(symbol_pattern, header_line).group(1)
        line = source.count("\n", 0, header.start()) + 1
        chunks[target["name"]] = {
            "source": chunk,
            "line": line,
            "symbol": symbol,
            "source_name": source_name,
            "mode": target["mode"],
            "local_prefix": target.get("local_prefix"),
        }
    return headers, chunks


def lua_long_string(value: str) -> str:
    equals = ""
    while "]" + equals + "]" in value:
        equals += "="
    return "[" + equals + "[" + value + "]" + equals + "]"


def parse_params(symbol: str, source: str, mode: str) -> int:
    if mode == "factory":
        match = re.search(r"return\s+function\s*\(([^)]*)\)", source)
    else:
        match = re.search(r"^function\s+" + re.escape(symbol) + r"\s*\(([^)]*)\)", source, re.MULTILINE)
    if not match:
        raise ValueError(f"could not parse parameter list for {symbol}")
    params = match.group(1).strip()
    return 0 if not params else len(params.split(","))


def emit_fixture(input_path: Path, output_path: Path):
    text = input_path.read_text(encoding="utf-8")
    source_name = input_path.name
    prelude = PRELUDE_RE.search(text)
    if not prelude:
        raise ValueError(f"{input_path}: emitted jit.opt prelude not found")

    headers, chunks = find_chunks(text, WRITERS, source_name)

    for target in WRITERS:
        state_field = target.get("state_field")
        if state_field:
            body = chunks[target["name"]]["source"]
            keys = set(re.findall(r"\b__rock_state_cell\s*\[\s*(\d+)\s*\]", body))
            if len(keys) != 1 or not re.search(r"\." + re.escape(state_field) + r"\b", body):
                raise ValueError(f"{source_name}: expected one state cell for {state_field}")
            key = next(iter(keys))
            chunks[target["name"]]["local_prefix"] = (
                f"local __rock_state_cell = {{ [{key}] = {{ {state_field} = 0.5 }} }}"
            )

    # Candidate writers may call newly emitted __ub_* helpers. Include only
    # helper definitions actually referenced by one of the target bodies.
    dependencies = set()
    for item in chunks.values():
        dependencies.update(CALL_RE.findall(item["source"]))
    helper_candidates = {
        "__nb_checked_index",
        "reserve_vertices",
        "__ensure_vertex_capacity",
        "__grow_rect_batch_buf",
        "__grow_vertex_buf",
        *(name for name in dependencies if name.startswith("__ub_")),
    }
    referenced_source = "\n".join(item["source"] for item in chunks.values())
    helper_names = sorted(
        helper
        for helper in helper_candidates
        if re.search(r"\b" + re.escape(helper) + r"\s*\(", referenced_source)
    )
    if "reserve_vertices" in helper_names and "__ensure_vertex_capacity" not in helper_names:
        helper_names.append("__ensure_vertex_capacity")
    helpers = {}
    for helper in helper_names:
        found = []
        for index, header in enumerate(headers):
            line_end = text.find("\n", header.start())
            if line_end < 0:
                line_end = len(text)
            header_line = text[header.start():line_end]
            if re.match(r"(?:local\s+)?function\s+" + re.escape(helper) + r"\s*\(", header_line):
                end = headers[index + 1].start() if index + 1 < len(headers) else len(text)
                found.append((header, end, header_line))
        if len(found) != 1:
            raise ValueError(f"{input_path}: expected one helper function {helper}, found {len(found)}")
        header, end, _ = found[0]
        helpers[helper] = {
            "source": text[header.start():end].rstrip() + "\n",
            "line": text.count("\n", 0, header.start()) + 1,
            "symbol": helper,
            "source_name": source_name,
        }

    all_chunks = {**helpers, **chunks}
    lines = [
        "-- Generated by micro.py. Source functions are kept byte-for-byte.",
        prelude.group(0),
        'local ffi = require("ffi")',
        "_G.ffi = ffi",
        "local loadstring = loadstring or load",
        "writer_source_map = {}",
    ]
    for key, item in all_chunks.items():
        literal = lua_long_string(item["source"])
        local_prefix = item.get("local_prefix")
        pad = item["line"] - (2 if local_prefix else 1)
        if pad < 0:
            raise ValueError(f"cannot preserve line mapping for {key}")
        prefix_source = (local_prefix + "\n") if local_prefix else ""
        lines.extend(
            [
                "do",
                f"  local chunk = string.rep(\"\\n\", {pad}) .. {json.dumps(prefix_source)} .. {literal}",
                f"  local loader, err = loadstring(chunk, \"@{source_name}\")",
                "  if not loader then error(err) end",
                "  loader()",
                f"  writer_source_map[{json.dumps(key)}] = {{ source = {json.dumps(source_name)}, line = {item['line']}, symbol = {json.dumps(item['symbol'])} }}",
                "end",
            ]
        )

    capacity = 65536
    lines.extend(
        [
            f"local writer_capacity = {capacity}",
            "local function writer_buffer()",
            '  local values = ffi.new("float[?]", writer_capacity * 13)',
            "  return { __vf = values, len = writer_capacity, ptr = values }",
            "end",
            "writer_cases = {}",
        ]
    )
    for target in WRITERS:
        name = target["name"]
        item = chunks[name]
        param_count = parse_params(item["symbol"], item["source"], item["mode"])
        leading_params = 1 if target["batch"] else 2
        if param_count < leading_params:
            raise ValueError(f"{item['symbol']} has too few parameters")
        suffix = param_count - leading_params
        args = ["x"]
        args.extend(f"(x + {index * 0.125!r})" for index in range(1, suffix))
        # These emitted slots retain their Rock ABI types. Numeric truthy
        # values would add float constants and misrepresent optional lanes.
        typed_args = {
            "__sprite_mesh_push_quad4": (40, {20: "true", 25: "true", 30: "true", 35: "true"}),
            "emit_textured_quad": (24, {
                1: "1", 2: "1", 3: "32.0", 4: "16.0",
                13: "(i % 17)", 14: "2", 15: "8", 16: "6", 22: "i",
            }),
            "ui_push_sprite_quad": (21, {
                1: "1", 2: "1", 3: "32.0", 4: "16.0",
                13: "(i % 17)", 14: "2", 15: "8", 16: "6",
            }),
            "append_vertex": (11, {10: "i"}),
        }
        if name in typed_args:
            expected, overrides = typed_args[name]
            if param_count != expected:
                raise ValueError(f"{name}: scalar ABI changed; update typed benchmark arguments")
            for slot, value in overrides.items():
                args[slot - leading_params] = value
        extra_args = ", " + ", ".join(args) if args else ""
        writer_expr = item["symbol"]
        if item["mode"] == "factory":
            writer_expr += "(function(...) return 32, 16 end)"
        buffer_field = "vbo" if target.get("batch_kind") == "rect" else "verts"
        buffer_expr = f"batch.{buffer_field}" if target["batch"] else "buffer"
        if not target["batch"]:
            setup = f"local buffer = writer_buffer()\n  local writer = {writer_expr}"
        else:
            capacity_expr = "writer_capacity / 6" if target.get("batch_kind") == "rect" else "writer_capacity"
            batch_fields = f"count = 0, capacity = {capacity_expr}, {buffer_field} = buffer"
            setup = (
                "local buffer = writer_buffer()\n"
                f"  local batch = {{ {batch_fields} }}\n"
                f"  local writer = {writer_expr}"
            )
        call_expr = f"writer(batch{extra_args})" if target["batch"] else f"writer(buffer, buffer.count{extra_args})"
        batch_reset = "    batch.count = 0\n" if target["batch"] else ""
        direct_count_init = "    buffer.count = 0\n" if not target["batch"] else ""
        direct_count_step = f"      buffer.count = buffer.count + {target['stride']}\n" if not target["batch"] else ""
        lines.extend(
            [
                "do",
                "  " + setup.replace("\n", "\n  "),
                f"  writer_cases[#writer_cases + 1] = {{ name = {json.dumps(name)}, vertices_per_call = {target['vertices']}, max_calls = math.floor(writer_capacity / {target['stride']}), run = function(n)",
                f"    if n < 0 or n * {target['stride']} > writer_capacity then error('writer case exceeds buffer capacity') end",
                direct_count_init.rstrip("\n"),
                batch_reset.rstrip("\n"),
                "    for i = 0, n - 1 do",
                "      local x = i * 0.125",
                f"      {call_expr}",
                direct_count_step.rstrip("\n"),
                "    end",
                f"    return {buffer_expr}.__vf[(n > 0 and ((n - 1) * {target['stride']} * 13) or 0)]",
                "  end }",
                "end",
            ]
        )
    output_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return {
        "input": source_name,
        "output": str(output_path),
        "writers": [
            {"name": target["name"], "symbol": chunks[target["name"]]["symbol"], "line": chunks[target["name"]]["line"],
             "params": parse_params(chunks[target["name"]]["symbol"], chunks[target["name"]]["source"], chunks[target["name"]]["mode"])}
            for target in WRITERS
        ],
        "helpers": [{"name": name, "line": helpers[name]["line"]} for name in helper_names],
    }


def run_comparison(args) -> int:
    base_path = args.input
    candidate_path = args.candidate
    output_dir = args.outdir or base_path.parent / "micro-bench"
    output_dir.mkdir(parents=True, exist_ok=True)
    fixtures = {
        "base": output_dir / "base.micro.lua",
        "candidate": output_dir / "candidate.micro.lua",
    }
    try:
        emit_fixture(base_path, fixtures["base"])
        emit_fixture(candidate_path, fixtures["candidate"])
    except (OSError, ValueError) as exc:
        print(f"micro.py: {exc}", file=sys.stderr)
        return 2

    driver = args.driver or Path(__file__).with_name("micro-driver.lua")
    runs = []
    failed = False
    for round_index in range(args.rounds):
        order = ("base", "candidate") if round_index % 2 == 0 else ("candidate", "base")
        for position, label in enumerate(order, 1):
            log_name = f"round-{round_index + 1:02d}-{label}.log"
            log_path = output_dir / log_name
            command = [
                args.luajit,
                str(driver),
                str(fixtures[label]),
                str(args.calls),
                str(args.samples),
                str(args.repetitions),
                str(args.warmup_runs),
            ]
            try:
                process = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, check=False)
                log_path.write_text(process.stdout, encoding="utf-8")
                json_line = next((line for line in reversed(process.stdout.splitlines()) if line.startswith("{")), None)
                result = json.loads(json_line) if json_line else None
                if process.returncode != 0 or result is None:
                    failed = True
                runs.append({
                    "round": round_index + 1,
                    "position": position,
                    "label": label,
                    "log": log_name,
                    "returncode": process.returncode,
                    "result": result,
                })
            except OSError as exc:
                failed = True
                log_path.write_text(str(exc) + "\n", encoding="utf-8")
                runs.append({
                    "round": round_index + 1,
                    "position": position,
                    "label": label,
                    "log": log_name,
                    "error": str(exc),
                })

    workloads = {}
    for label in ("base", "candidate"):
        by_name = {}
        for run in runs:
            if run["label"] != label or not run.get("result"):
                continue
            for workload in run["result"].get("workloads", []):
                by_name.setdefault(workload["name"], []).append(workload["median_ns_per_quad"])
        workloads[label] = {
            name: {
                "median_of_run_medians_ns_per_quad": statistics.median(values),
                "min_run_median_ns_per_quad": min(values),
                "max_run_median_ns_per_quad": max(values),
                "runs": len(values),
            }
            for name, values in by_name.items()
        }
    comparisons = {}
    for name in sorted(set(workloads["base"]) & set(workloads["candidate"])):
        base_median = workloads["base"][name]["median_of_run_medians_ns_per_quad"]
        candidate_median = workloads["candidate"][name]["median_of_run_medians_ns_per_quad"]
        comparisons[name] = {
            "candidate_over_base": candidate_median / base_median,
            "speedup": base_median / candidate_median,
        }

    summary = {
        "inputs": {"base": base_path.name, "candidate": candidate_path.name},
        "settings": {
            "rounds": args.rounds,
            "calls": args.calls,
            "samples": args.samples,
            "repetitions": args.repetitions,
            "warmup_runs": args.warmup_runs,
            "interpreter": Path(args.luajit).name,
        },
        "fixtures": {label: path.name for label, path in fixtures.items()},
        "runs": runs,
        "workloads": workloads,
        "comparison": comparisons,
        "success": not failed,
    }
    summary_path = args.summary or output_dir / "summary.json"
    summary_path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"summary": str(summary_path), "outdir": str(output_dir), "success": not failed}, indent=2))
    return 1 if failed else 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=Path, help="emitted Lua input, e.g. stone-base")
    parser.add_argument("-o", "--output", type=Path, help="fixture path (default: INPUT.micro.lua)")
    parser.add_argument("--candidate", type=Path, help="second emitted Lua input; run alternating fresh-process comparison")
    parser.add_argument("--rounds", type=int, default=15, help="comparison rounds (default: 15)")
    parser.add_argument("--calls", type=int, default=2048, help="writer calls per workload pass")
    parser.add_argument("--samples", type=int, default=7, help="timed samples per workload per process")
    parser.add_argument("--repetitions", type=int, default=20, help="passes per timed sample")
    parser.add_argument("--warmup-runs", type=int, default=8, help="untimed warmup passes per workload")
    parser.add_argument("--luajit", default="luajit", help="LuaJIT executable")
    parser.add_argument("--driver", type=Path, help="Lua benchmark driver (default: beside micro.py)")
    parser.add_argument("--outdir", type=Path, help="comparison artifacts directory")
    parser.add_argument("--summary", type=Path, help="summary JSON path")
    args = parser.parse_args(argv)
    if args.rounds < 1 or args.calls < 1 or args.samples < 1 or args.repetitions < 1 or args.warmup_runs < 0:
        parser.error("rounds, calls, samples, and repetitions must be positive; warmup-runs must be nonnegative")
    if args.candidate:
        if args.output:
            parser.error("--output is only for single-input fixture extraction; use --outdir in comparison mode")
        return run_comparison(args)
    output_path = args.output or args.input.with_name(args.input.name + ".micro.lua")
    try:
        result = emit_fixture(args.input, output_path)
    except (OSError, ValueError) as exc:
        print(f"micro.py: {exc}", file=sys.stderr)
        return 2
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
