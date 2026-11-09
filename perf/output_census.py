#!/usr/bin/env python3
"""Census of one emitted Lua program: what the bytes consist of.

    perf/output_census.py out.lua                  # tables on stdout
    perf/output_census.py out.lua --json census.json
    perf/output_census.py out.lua --engine stone.lua --mask

Reads the Lua `rock compile --output` wrote and splits it into top-level
definitions (`function <name>(` or the bound-callee factory
`__rock_mk_<name> = function(...) return function(`, each closed by a
column-zero `end` or `end end`) and the glue between them: the runtime
prelude and type tables before the first program definition, the callee
binding lines, global directory entries and registration tables between
definitions, and the global initializer and shader sources after the last.

Definitions are classed by their symbol: a generic specialization carries
`_spec_`, a hoisted lambda `__lambda`, a hoisted nested fn `__local_`, a
decorator `__deco_`, runtime support `__rock_`; everything else is a declared
function. The tables answer: how many bytes each class owns, how many bodies
repeat another body byte for byte (or after erasing the type suffixes that
distinguish specializations of one generic), which generics specialize most,
how much of a body is structural boilerplate (indentation, block labels,
gotos, declarations) and which bodies are largest.

`--engine` names a second program (the Stone engine compiled alone) whose
symbols are treated as public; with `--mask`, every other symbol prints as a
hash so the report carries no private identifier.
"""
import argparse
import hashlib
import json
import re
import sys
from collections import Counter, defaultdict

HEADER = re.compile(r"^(?:function ([A-Za-z_][A-Za-z0-9_]*)\(|__rock_mk_([A-Za-z_][A-Za-z0-9_]*) = function\()")
BIND = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*) = __rock_mk_\1\(")
LABEL = re.compile(r"^\s*::bb\d+::\s*$")
GOTO = re.compile(r"^\s*goto bb\d+\s*$")
LOCAL_DECL = re.compile(r"^\s*local [rc]\d+(?:, [rc]\d+)*\s*$")
RETURN_UNIT = re.compile(r"^\s*do return end\s*$")
NIL_KILL = re.compile(r"^\s*[rc]\d+ = nil\s*$")
MODULE_HASH = re.compile(r"_[0-9a-f]{8}(?=_|$|\b)")
SPEC_SUFFIX = re.compile(r"_spec_[A-Za-z0-9_]*")
ANON_FN = re.compile(r"\bfunction\s*\(")
STRING = re.compile(r'"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'')
LONG_STRING = re.compile(r"\[(=*)\[.*?\]\1\]", re.S)
COMMENT = re.compile(r"--[^\n]*")
GLUE_KINDS = [
    ("blank", re.compile(r"^\s*$")),
    ("bind", BIND),
    ("code slot stage", re.compile(r"^__rock_code_slot_stage\(")),
    ("vtable", re.compile(r"^__rock_vt_")),
    ("global init call", re.compile(r"^[A-Za-z0-9_, ]+ = __skull_ginit_")),
    ("reflection getter", re.compile(r'^\["(global|static):')),
    ("global directory", re.compile(r'^\{name="')),
    ("registration table", re.compile(r"^_G\.[A-Za-z0-9_]+\[")),
    ("ffi assertion", re.compile(r"^if \(?ffi\.")),
    ("cdef", re.compile(r"^(typedef struct|\} [A-Za-z0-9_]+;|\s*[a-z0-9_]+ [A-Za-z0-9_]+(\[\d+\])?;)")),
    ("metatype", re.compile(r"^(pcall\(ffi\.metatype|[A-Za-z0-9_]+\.__(eq|tostring|index) = function|end, (copy|name) =|\}\)|[A-Za-z0-9_]+ = ffi\.typeof)")),
    ("shader source", re.compile(r"^(#version|#ifdef|#endif|void main|layout \(|in |out |uniform |stage |arrays |output |\]\]\)|\{[a-z]+ |\s*(vec[234]|float|int) [A-Za-z0-9_]+;)")),
    ("comment", re.compile(r"^--")),
    ("scope", re.compile(r"^(do|end\)\(\)|\(function\(\)|end)$")),
    ("indented (metatype, inspect, cdef bodies)", re.compile(r"^\s")),
]


def classify(name):
    if "__lambda" in name:
        return "lambda"
    if "__local_" in name:
        return "nested fn"
    if name.startswith("__deco_"):
        return "decorator"
    if name.startswith("__rock_"):
        return "runtime"
    if name.startswith("__skull_ginit_") or name.startswith("__nbg_"):
        return "global init"
    if "_spec_" in name:
        return "specialization"
    return "declared fn"


def strip_strings(text):
    text = LONG_STRING.sub('""', text)
    text = STRING.sub('""', text)
    return COMMENT.sub("", text)


def parse(lines):
    """Split into definitions and glue runs, each a (kind, start, end) over lines."""
    defs = []
    glue = []
    i = 0
    n = len(lines)
    run_start = 0
    while i < n:
        m = HEADER.match(lines[i])
        if not m:
            i += 1
            continue
        if i > run_start:
            glue.append((run_start, i))
        name = m.group(1) or m.group(2)
        factory = m.group(2) is not None
        j = i + 1
        while j < n and lines[j] not in ("end", "end end"):
            if HEADER.match(lines[j]) and not lines[j - 1].startswith(" "):
                break
            j += 1
        end = j + 1 if j < n and lines[j] in ("end", "end end") else j
        defs.append({"name": name, "factory": factory, "start": i, "end": end})
        i = end
        run_start = end
    if run_start < n:
        glue.append((run_start, n))
    return defs, glue


def body_bytes(lines, start, end):
    return sum(len(l) + 1 for l in lines[start:end])


def normalized_body(lines, d, erase_types=False):
    """The body with the symbol it defines erased, so two copies compare equal."""
    text = "\n".join(lines[d["start"]:d["end"]])
    text = text.replace(d["name"], "@SELF")
    if erase_types:
        text = SPEC_SUFFIX.sub("_spec_@", text)
        text = MODULE_HASH.sub("_@", text)
    return text


def spec_origin(name):
    return name.split("_spec_", 1)[0]


def spec_args(name):
    return name.split("_spec_", 1)[1] if "_spec_" in name else ""


def count_nested_functions(text):
    """Anonymous `function(` expressions inside a body and the bytes they span,
    found by block nesting over the text with strings and comments removed."""
    code = strip_strings(text)
    openers = re.compile(r"\b(function|do|if|for|while|repeat|end|until)\b")
    depth_stack = []
    anon_spans = []
    anon_count = 0
    for m in openers.finditer(code):
        w = m.group(1)
        if w in ("function", "do", "if", "for", "while", "repeat"):
            # `do` after `for`/`while` does not open another level
            if w == "do" and depth_stack and depth_stack[-1][0] in ("for", "while") and not depth_stack[-1][2]:
                depth_stack[-1] = (depth_stack[-1][0], depth_stack[-1][1], True, depth_stack[-1][3])
                continue
            is_anon = w == "function" and code[m.end():m.end() + 2].lstrip().startswith("(")
            depth_stack.append((w, m.start(), False, is_anon))
        else:
            if not depth_stack:
                continue
            kind, at, _, is_anon = depth_stack.pop()
            if is_anon:
                anon_count += 1
                anon_spans.append((at, m.end()))
    # outermost anonymous spans only, so nested lambdas are not double counted
    anon_spans.sort()
    total = 0
    last_end = -1
    for a, b in anon_spans:
        if a >= last_end:
            total += b - a
            last_end = b
    return anon_count, total


def boilerplate(lines, start, end):
    """Bytes of structural text in one body, plus the control-flow waste a
    structured emitter could drop: labels no goto targets, and a goto whose
    target label is the very next statement."""
    out = Counter()
    body = lines[start:end]
    targets = Counter()
    for l in body:
        m = GOTO.match(l)
        if m:
            targets[l.strip()[5:]] += 1
    prev_goto = None
    for l in body:
        m = LABEL.match(l)
        if m:
            label = l.strip()[2:-2]
            if targets[label] == 0:
                out["unused label"] += len(l) + 1
            if prev_goto == label:
                out["fallthrough goto"] += len(prev_goto) + 10
        prev_goto = l.strip()[5:] if GOTO.match(l) else None
    for l in body:
        stripped = l.lstrip(" ")
        out["indent"] += len(l) - len(stripped)
        if LABEL.match(l):
            out["block label"] += len(l) + 1
        elif GOTO.match(l):
            out["goto"] += len(l) + 1
        elif LOCAL_DECL.match(l):
            out["local decl"] += len(l) + 1
        elif RETURN_UNIT.match(l):
            out["do return end"] += len(l) + 1
        elif NIL_KILL.match(l):
            out["nil kill"] += len(l) + 1
        elif stripped == "":
            out["blank"] += len(l) + 1
        out["newline"] += 1
    return out


def fmt_bytes(b):
    if b >= 1 << 20:
        return f"{b / (1 << 20):.2f} MB"
    if b >= 1 << 10:
        return f"{b / 1024:.1f} KB"
    return f"{b} B"


def table(rows, headers):
    widths = [max(len(str(r[i])) for r in [headers] + rows) for i in range(len(headers))]
    line = " | ".join(str(h).ljust(w) for h, w in zip(headers, widths))
    out = [line, "-+-".join("-" * w for w in widths)]
    for r in rows:
        out.append(" | ".join(str(c).ljust(w) if i == 0 else str(c).rjust(w) for i, (c, w) in enumerate(zip(r, widths))))
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("lua")
    ap.add_argument("--json", help="also write the census as JSON")
    ap.add_argument("--engine", help="a Lua program whose symbols are public (printed unmasked)")
    ap.add_argument("--mask", action="store_true", help="print symbols not in --engine as hashes")
    ap.add_argument("--top", type=int, default=15)
    args = ap.parse_args()

    text = open(args.lua, encoding="utf-8", errors="replace").read()
    total_bytes = len(text.encode("utf-8", errors="replace"))
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    defs, glue = parse(lines)

    public = set()
    if args.engine:
        elines = open(args.engine, encoding="utf-8", errors="replace").read().split("\n")
        edefs, _ = parse(elines)
        public = {d["name"] for d in edefs}
        public |= {spec_origin(n) for n in public}

    def show(name):
        if not args.mask:
            return name
        base = spec_origin(name)
        if name in public or base in public or not MODULE_HASH.search(name) and "__" not in name:
            return name
        return "private:" + hashlib.sha1(name.encode()).hexdigest()[:8]

    # --- (1) classes --------------------------------------------------------
    for d in defs:
        d["bytes"] = body_bytes(lines, d["start"], d["end"])
        d["lines"] = d["end"] - d["start"]
        d["class"] = classify(d["name"])
        body = "\n".join(lines[d["start"] + 1:d["end"]])
        d["anon_fns"], d["anon_bytes"] = count_nested_functions(body)
        d["boiler"] = boilerplate(lines, d["start"], d["end"])
    def_bytes = sum(d["bytes"] for d in defs)
    glue_bytes = sum(body_bytes(lines, a, b) for a, b in glue)
    first_program = next((i for i, d in enumerate(defs) if d["class"] not in ("runtime",)), len(defs))
    prelude_end = defs[first_program]["start"] if first_program < len(defs) else len(lines)
    last_end = defs[-1]["end"] if defs else 0
    prelude_bytes = body_bytes(lines, 0, prelude_end) - sum(d["bytes"] for d in defs[:first_program])
    tail_bytes = body_bytes(lines, last_end, len(lines))
    between_bytes = glue_bytes - prelude_bytes - tail_bytes

    glue_kinds = Counter()
    for a, b in glue:
        for l in lines[a:b]:
            for kind, rx in GLUE_KINDS:
                if rx.match(l):
                    glue_kinds[kind] += len(l) + 1
                    break
            else:
                glue_kinds["other glue"] += len(l) + 1

    print(f"program: {fmt_bytes(total_bytes)}, {len(lines)} lines, {len(defs)} top-level definitions")
    print()
    print("(1) bytes by definition class")
    by_class = defaultdict(lambda: [0, 0, 0, 0])
    for d in defs:
        c = by_class[d["class"]]
        c[0] += 1
        c[1] += d["bytes"]
        c[2] += d["anon_fns"]
        c[3] += d["anon_bytes"]
    rows = []
    for cls, (n, b, af, ab) in sorted(by_class.items(), key=lambda kv: -kv[1][1]):
        rows.append([cls, n, fmt_bytes(b), f"{100 * b / total_bytes:.1f}%", f"{b // max(n, 1)}", af, fmt_bytes(ab)])
    rows.append(["glue: runtime prelude + type tables", "", fmt_bytes(prelude_bytes), f"{100 * prelude_bytes / total_bytes:.1f}%", "", "", ""])
    rows.append(["glue: between definitions", "", fmt_bytes(between_bytes), f"{100 * between_bytes / total_bytes:.1f}%", "", "", ""])
    rows.append(["glue: tail (global init, shaders)", "", fmt_bytes(tail_bytes), f"{100 * tail_bytes / total_bytes:.1f}%", "", "", ""])
    print(table(rows, ["class", "bodies", "bytes", "share", "avg B", "inline lambdas", "lambda bytes"]))
    factories = sum(1 for d in defs if d["factory"])
    print(f"\n{factories} definitions are bound-callee factories (__rock_mk_), {len(defs) - factories} plain functions")
    print("\nglue bytes by line kind")
    print(table([[k, fmt_bytes(v), f"{100 * v / total_bytes:.1f}%"] for k, v in glue_kinds.most_common()], ["kind", "bytes", "share"]))

    # --- (2) duplicates -----------------------------------------------------
    print("\n(2) duplicate bodies")
    dup_rows = []
    dup_json = {}
    for label, erase in (("byte-identical after renaming own symbol", False), ("identical after erasing type suffixes", True)):
        groups = defaultdict(list)
        for d in defs:
            if d["class"] == "runtime":
                continue
            groups[hashlib.sha1(normalized_body(lines, d, erase).encode()).hexdigest()].append(d)
        dup_defs = 0
        dup_bytes = 0
        per_class = Counter()
        per_class_bytes = Counter()
        for g in groups.values():
            if len(g) > 1:
                extra = g[1:]
                dup_defs += len(extra)
                dup_bytes += sum(d["bytes"] for d in extra)
                for d in extra:
                    per_class[d["class"]] += 1
                    per_class_bytes[d["class"]] += d["bytes"]
        dup_rows.append([label, dup_defs, fmt_bytes(dup_bytes), f"{100 * dup_bytes / total_bytes:.1f}%",
                         ", ".join(f"{c} {n} ({fmt_bytes(per_class_bytes[c])})" for c, n in per_class.most_common())])
        dup_json[label] = {"bodies": dup_defs, "bytes": dup_bytes, "per_class": dict(per_class)}
    print(table(dup_rows, ["dedup", "redundant bodies", "bytes saved", "share", "by class"]))

    specs = [d for d in defs if d["class"] == "specialization"]
    origins = defaultdict(list)
    for d in specs:
        origins[spec_origin(d["name"])].append(d)
    print(f"\n{len(specs)} specializations of {len(origins)} generic functions; top origins:")
    rows = []
    origin_json = []
    for origin, ds in sorted(origins.items(), key=lambda kv: -sum(d["bytes"] for d in kv[1]))[: args.top]:
        exact = len({hashlib.sha1(normalized_body(lines, d, False).encode()).hexdigest() for d in ds})
        erased = len({hashlib.sha1(normalized_body(lines, d, True).encode()).hexdigest() for d in ds})
        b = sum(d["bytes"] for d in ds)
        rows.append([show(origin), len(ds), exact, erased, fmt_bytes(b), fmt_bytes(b // len(ds))])
        origin_json.append({"origin": show(origin), "specs": len(ds), "distinct": exact, "distinct_erased": erased, "bytes": b})
    print(table(rows, ["generic", "specs", "distinct bodies", "distinct after type erasure", "bytes", "avg"]))
    hist = Counter(len(ds) for ds in origins.values())
    print("\nspecializations per generic (histogram): " + ", ".join(f"{k}x{v}" for k, v in sorted(hist.items())))

    # --- (4) boilerplate and largest bodies ---------------------------------
    print("\n(4) structural boilerplate inside bodies")
    boiler = Counter()
    for d in defs:
        boiler.update(d["boiler"])
    header_bytes = sum(len(lines[d["start"]]) + 1 for d in defs)
    end_bytes = sum(len(lines[d["end"] - 1]) + 1 for d in defs if d["end"] > d["start"])
    rows = [["definition header line", fmt_bytes(header_bytes), f"{100 * header_bytes / total_bytes:.1f}%"],
            ["closing end line", fmt_bytes(end_bytes), f"{100 * end_bytes / total_bytes:.1f}%"],
            ["bind line (name = __rock_mk_name(..))", fmt_bytes(glue_kinds["bind"]), f"{100 * glue_kinds['bind'] / total_bytes:.1f}%"]]
    for k in ("indent", "block label", "goto", "local decl", "do return end", "nil kill", "blank"):
        rows.append([k, fmt_bytes(boiler[k]), f"{100 * boiler[k] / total_bytes:.1f}%"])
    rows.append(["  of labels: never targeted", fmt_bytes(boiler["unused label"]), f"{100 * boiler['unused label'] / total_bytes:.1f}%"])
    rows.append(["  of gotos: to the next line's label", fmt_bytes(boiler["fallthrough goto"]), f"{100 * boiler['fallthrough goto'] / total_bytes:.1f}%"])
    rows.append(["newlines", fmt_bytes(boiler["newline"]), f"{100 * boiler['newline'] / total_bytes:.1f}%"])
    print(table(rows, ["kind", "bytes", "share"]))

    names = {d["name"] for d in defs}
    ident = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
    symbol_bytes = 0
    symbol_uses = 0
    spec_symbol_bytes = 0
    for m in ident.finditer(text):
        w = m.group(0)
        if w in names:
            symbol_bytes += len(w)
            symbol_uses += 1
            if "_spec_" in w:
                spec_symbol_bytes += len(w)
    print(f"\nmangled symbol occurrences: {symbol_uses}, {fmt_bytes(symbol_bytes)} ({100 * symbol_bytes / total_bytes:.1f}%), of which specialization names {fmt_bytes(spec_symbol_bytes)}; avg symbol {symbol_bytes // max(symbol_uses, 1)} B")
    paths = re.findall(r'@(/[^":]+):\d+', text)
    path_bytes = sum(len(p) + 1 for p in paths)
    print(f"embedded source paths: {len(paths)} occurrences, {fmt_bytes(path_bytes)} ({100 * path_bytes / total_bytes:.1f}%)")
    sizes = sorted(d["bytes"] for d in defs)
    def pct(p):
        return sizes[min(len(sizes) - 1, int(p * len(sizes)))] if sizes else 0
    print(f"\nbody size: p50 {pct(0.5)} B, p90 {pct(0.9)} B, p99 {pct(0.99)} B, max {sizes[-1] if sizes else 0} B")
    top10 = sorted(defs, key=lambda d: -d["bytes"])[: args.top]
    print(f"\nlargest {args.top} bodies ({fmt_bytes(sum(d['bytes'] for d in top10))} together):")
    rows = [[show(d["name"])[:70], d["class"], fmt_bytes(d["bytes"]), d["lines"], d["anon_fns"]] for d in top10]
    print(table(rows, ["symbol", "class", "bytes", "lines", "lambdas"]))
    share_top = [0.01, 0.05, 0.10]
    desc = sorted(defs, key=lambda d: -d["bytes"])
    print("\nbytes concentrated in the largest bodies: " + ", ".join(
        f"top {int(s * 100)}% of bodies hold {100 * sum(d['bytes'] for d in desc[: max(1, int(s * len(desc)))]) / max(def_bytes, 1):.0f}%" for s in share_top))

    if args.json:
        out = {
            "total_bytes": total_bytes, "lines": len(lines), "definitions": len(defs),
            "classes": {c: {"bodies": v[0], "bytes": v[1], "lambdas": v[2], "lambda_bytes": v[3]} for c, v in by_class.items()},
            "glue": {"prelude": prelude_bytes, "between": between_bytes, "tail": tail_bytes, "kinds": dict(glue_kinds)},
            "duplicates": dup_json, "origins": origin_json, "boilerplate": dict(boiler),
            "header_bytes": header_bytes, "end_bytes": end_bytes,
            "symbol_bytes": symbol_bytes, "symbol_uses": symbol_uses, "path_bytes": path_bytes,
            "largest": [{"symbol": show(d["name"]), "class": d["class"], "bytes": d["bytes"]} for d in top10],
        }
        with open(args.json, "w") as f:
            json.dump(out, f, indent=1)
    return 0


if __name__ == "__main__":
    sys.exit(main())
