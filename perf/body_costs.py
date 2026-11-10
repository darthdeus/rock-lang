#!/usr/bin/env python3
"""Compile cost by source definition: which code the compiler spends its time
and output on, and which source patterns produce poor Lua.

    ROCK_BODY_COSTS=costs.jsonl ROCK_COMPILE_STATS=stats.jsonl \\
        rock compile packages/stone --output stone.lua
    perf/body_costs.py costs.jsonl --stats stats.jsonl --lua stone.lua \\
        --html report.html --md summary.md
    perf/body_costs.py game-costs.jsonl --lua game.lua --root path/to/game --mask

`ROCK_BODY_COSTS=<file>` makes a project compile write one JSON line per
emitted body (`"record": "body"`) and one per module and front-end pass
(`"record": "module"`); see `crates/rock-cli/src/body_costs.rs`. This script
groups the bodies by the source definition they came from (a generic's
specializations under the generic), by file and by package, attributes
inlining to the callee, and, given the emitted Lua, finds output patterns
(wide signatures and calls, functions near LuaJIT limits, label/goto bodies,
table constructors in loops, per-site failure blocks) and the definitions
that produce them.

Units. "CPU" is the worker thread's CPU time for a body where the fan-out
measured it (`thread_cpu_ns`) and otherwise the body's summed stage times;
both are summed over threads, so compare them with each other and with the
compile budget's `unit_cpu_ms`, never with a wall window. Stage columns are
elapsed time on the worker. Front-end module times are elapsed time on the
worker, summed over threads. Lua sizes are bytes and lines of the body's own
text; glue between definitions (prelude, binding lines, registration) is not
attributed and is reported as the remainder. Inlining costs per callee are
estimates: the caller's post-splice work and text, apportioned by the share
of its MIR the callee's copies make up.

`--mask` prints every name and file outside the standard library and the
engine packages (`--public`, default `^stone`) as a hash, so a report can be
shared without private identifiers.
"""
import argparse
import hashlib
import html
import json
import os
import re
import sys
from collections import Counter, defaultdict

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import output_census as census  # noqa: E402

STAGES = ["lower", "inline", "cleanup", "closure", "counted_loop", "hoisted_inline",
          "validation", "emit"]
STAGE_LABEL = {"lower": "lower", "inline": "inline", "cleanup": "cleanup",
               "closure": "closure conv.", "counted_loop": "loop recovery",
               "hoisted_inline": "hoisted inline", "validation": "validation",
               "emit": "print (incl. regalloc)"}
# Ranking weight of one byte of Lua against compile CPU: about LuaJIT's parse
# time per byte, so a saving in output counts as load time saved.
BYTE_NS = 10
POST_SPLICE = ["cleanup", "closure", "counted_loop", "hoisted_inline", "validation", "emit"]


# ---------------------------------------------------------------- loading

def load_costs(path):
    bodies, modules = [], []
    with open(path) as f:
        for line in f:
            if not line.strip():
                continue
            r = json.loads(line)
            (modules if r.get("record") == "module" else bodies).append(r)
    for b in bodies:
        stage = sum(b.get(s + "_ns", 0) for s in STAGES)
        b["stage_ns"] = stage
        b["cpu_ns"] = b["thread_cpu_ns"] if b.get("thread_cpu_ns") is not None else stage
        b.setdefault("file", "?")
        b.setdefault("name", "?")
        b.setdefault("line", 0)
        b.setdefault("end_line", b["line"])
        b["src_lines"] = max(1, b["end_line"] - b["line"] + 1)
        if "key" not in b:
            b["key"] = "%s %s@%s:%s" % (b.get("kind"), b["name"], b["file"], b["line"])
    return bodies, modules


def load_stats(path):
    if not path:
        return None
    lines = [l for l in open(path) if l.strip()]
    if not lines:
        return None
    rec = json.loads(lines[-1])

    def find(node, name):
        if node.get("name") == name:
            return node
        for c in node.get("children", []):
            r = find(c, name)
            if r:
                return r
        return None

    budget = rec["budget"]
    out = {"wall_ms": rec.get("wall_ms"), "threads": rec.get("threads"),
           "lua_bytes": rec.get("totals", {}).get("emitted_lua_bytes")}
    for name in ["parse+stdlib", "semantic", "codegen", "fan-out", "setup", "assembly",
                 "global accessors", "write", "discovery"]:
        n = find(budget, name)
        out[name] = n["wall_ms"] if n else None
    cg = find(budget, "codegen") or {}
    out["unit_cpu_ms"] = cg.get("unit_cpu_ms")
    sem = find(budget, "semantic") or {}
    out["semantic_children"] = {c["name"]: c["wall_ms"] for c in sem.get("children", [])}
    return out


# ---------------------------------------------------------------- names

class Namer:
    def __init__(self, root, public, mask):
        self.root = os.path.abspath(root) + os.sep if root else None
        self.public = re.compile(public)
        self.mask = mask

    def group(self, rec):
        f = rec.get("file", "")
        pkg = rec.get("package")
        if f.startswith("<stdlib>") or (pkg is None and "/stdlib/" in f):
            return "stdlib"
        if pkg and self.public.search(pkg):
            return "engine"
        return "program"

    def is_public(self, rec):
        return self.group(rec) != "program"

    def path(self, rec):
        f = rec.get("file", "?")
        if self.mask and not self.is_public(rec):
            return "file:" + hashlib.sha1(f.encode()).hexdigest()[:8]
        if "/packages/" in f:
            return "packages/" + f.split("/packages/", 1)[1]
        if self.root and f.startswith(self.root):
            return f[len(self.root):]
        return f

    def name(self, rec, name=None):
        name = name if name is not None else rec.get("name", "?")
        if self.mask and not self.is_public(rec):
            return "private:" + hashlib.sha1(name.encode()).hexdigest()[:8]
        return name

    def site(self, rec):
        return "%s:%s" % (self.path(rec), rec.get("line", 0))

    def symbol(self, rec, sym):
        if self.mask and not self.is_public(rec):
            return "private:" + hashlib.sha1(sym.encode()).hexdigest()[:8]
        return sym


def parse_key(key):
    """`<name>@<file>:<line>` -> a record-like dict."""
    name, _, rest = key.rpartition("@")
    f, _, line = rest.rpartition(":")
    return {"name": name, "file": f, "line": int(line) if line.isdigit() else 0}


# ---------------------------------------------------------------- Lua analysis

LOCAL_LINE = re.compile(r"^\s*local ([A-Za-z_][A-Za-z0-9_]*(?:, [A-Za-z_][A-Za-z0-9_]*)*)")
FACTORY = re.compile(r"^__rock_mk_[A-Za-z0-9_]+ = function\(([^)]*)\) return function\(([^)]*)\)")
PLAIN = re.compile(r"^function [A-Za-z0-9_]+\(([^)]*)\)")
MULTI_ASSIGN = re.compile(r"^\s*((?:[rc]\d+, ){29,}[rc]\d+) = ")
CALL_START = re.compile(r"\b(?!function\()[A-Za-z_][A-Za-z0-9_.:]*\(")
BOUND = re.compile(r"^b(\d+)$")
LOOP_OPEN = re.compile(r"^\s*(?:while .* do|for .* do|repeat)\s*$")
BLOCK_OPEN = re.compile(r"^\s*(?:if .* then|do|while .* do|for .* do|repeat)\s*$|\bfunction\s*\([^)]*\)\s*$")
TABLE_CTOR = re.compile(r"[=(,{]\s*\{(?!\s*\})")
WLABEL = re.compile(r"^\s*::(w\d+)::\s*$")
WGOTO = re.compile(r"goto (w\d+)\b")
LUA_WRAP = re.compile(r"^\s*local [A-Za-z_][A-Za-z0-9_]* = [rc]\d+\s*$")
DEAD_PARAM = re.compile(r"^\s*([rc]\d+) = nil\s*$")


def count_args(line, start):
    """Top-level arguments of the call whose `(` is at `start`."""
    depth, args, seen = 0, 0, False
    i = start
    n = len(line)
    in_str = None
    while i < n:
        ch = line[i]
        if in_str:
            if ch == "\\":
                i += 2
                continue
            if ch == in_str:
                in_str = None
        elif ch in "\"'":
            in_str = ch
        elif ch in "({[":
            depth += 1
            if depth == 1 and ch == "(":
                i += 1
                continue
        elif ch in ")}]":
            depth -= 1
            if depth == 0:
                return args + (1 if seen else 0)
        elif ch == "," and depth == 1:
            args += 1
        elif depth >= 1 and not ch.isspace():
            seen = True
        i += 1
    return args + (1 if seen else 0)


def analyze_def(lines, d, binds):
    body = lines[d["start"]:d["end"]]
    bound = binds.get(d["name"], [])
    wide_callees = []
    head = body[0] if body else ""
    m = FACTORY.match(head)
    if m:
        upvalues = [p for p in m.group(1).split(", ") if p]
        params = [p for p in m.group(2).split(", ") if p]
    else:
        m = PLAIN.match(head)
        upvalues = []
        params = [p for p in (m.group(1).split(", ") if m else []) if p and p != "..."]
    text_after = "\n".join(body[1:])
    unused = 0
    for p in params:
        uses = re.findall(r"\b%s\b" % re.escape(p), text_after)
        # a parameter whose only mention is its own kill is unread
        if not uses or (len(uses) == 1 and re.search(r"^\s*%s = nil\s*$" % re.escape(p), text_after, re.M)):
            unused += 1
    locals_ = set()
    wide_calls = 0
    max_args = 0
    multi = 0
    max_results = 0
    tracebacks = 0
    labels = 0
    gotos = 0
    wraps = 0
    guards = 0
    for i, l in enumerate(body):
        lm = LOCAL_LINE.match(l)
        if lm:
            locals_.update(lm.group(1).split(", "))
        for cm in CALL_START.finditer(l):
            if l.count(",") < 29:
                break
            a = count_args(l, cm.end() - 1)
            if a >= 30:
                wide_calls += 1
                max_args = max(max_args, a)
                callee = cm.group(0)[:-1]
                bm = BOUND.match(callee)
                if bm and int(bm.group(1)) < len(bound):
                    callee = bound[int(bm.group(1))]
                wide_callees.append((callee, a))
        mm = MULTI_ASSIGN.match(l)
        if mm:
            multi += 1
            max_results = max(max_results, mm.group(1).count(",") + 1)
        if "debug.traceback(" in l:
            tracebacks += 1
        if census.LABEL.match(l):
            labels += 1
        if census.GOTO.match(l):
            gotos += 1
        if l.strip() == "do" and i + 1 < len(body) and LUA_WRAP.match(body[i + 1]):
            wraps += 1
        if "getmetatable(" in l:
            guards += 1
    # table constructors inside loops: structured loops by block nesting,
    # goto loops between a `::wN::` header and its last `goto wN`
    in_goto_loop = [False] * len(body)
    first = {}
    last = {}
    for i, l in enumerate(body):
        wm = WLABEL.match(l)
        if wm:
            first.setdefault(wm.group(1), i)
        for g in WGOTO.findall(l):
            last[g] = i
    for lab, a in first.items():
        b = last.get(lab)
        if b is not None and b > a:
            for k in range(a, b + 1):
                in_goto_loop[k] = True
    stack = []
    ctors_in_loops = 0
    for i, l in enumerate(body[1:], start=1):
        s = l.strip()
        if s in ("end", "end end", "end)", "until") or s.startswith("until ") or s.startswith("end,") or s.startswith("end)"):
            if stack:
                stack.pop()
            continue
        in_loop = in_goto_loop[i] or any(stack)
        if in_loop:
            ctors_in_loops += len(TABLE_CTOR.findall(census.strip_strings(l)))
        if LOOP_OPEN.match(l):
            stack.append(True)
        elif BLOCK_OPEN.search(l):
            stack.append(False)
    return {
        "params": len(params), "unused_params": unused, "upvalues": len(upvalues),
        "locals": len(locals_), "wide_calls": wide_calls, "max_args": max_args,
        "multi_results": multi, "max_results": max_results, "tracebacks": tracebacks,
        "labels": labels, "gotos": gotos, "lua_wraps": wraps, "guards": guards,
        "ctors_in_loops": ctors_in_loops, "wide_callees": wide_callees,
    }


def lua_index(path):
    lines = open(path, encoding="utf-8", errors="replace").read().split("\n")
    defs, _ = census.parse(lines)
    by_name = {d["name"]: d for d in defs}
    binds = {}
    for l in lines:
        m = census.BIND.match(l)
        if m:
            inner = l[m.end():l.rindex(")")] if ")" in l else ""
            binds[m.group(1)] = [a.strip() for a in inner.split(",") if a.strip()]
    return lines, defs, by_name, binds


def attach_lua(bodies, lines, defs, by_name, binds):
    """Per body: the analysis of its own definition and of the lambdas and
    nested functions its chunk carries (`<symbol>__lambda<k>`)."""
    lambdas = defaultdict(list)
    for d in defs:
        if "__lambda" in d["name"]:
            lambdas[d["name"].split("__lambda", 1)[0]].append(d)
    for b in bodies:
        sym = b.get("symbol")
        d = by_name.get(sym) if sym else None
        if not d:
            continue
        b["lua"] = analyze_def(lines, d, binds)
        b["lua"]["lambdas"] = len(lambdas.get(sym, []))
        b["norm"] = hashlib.sha1(census.normalized_body(lines, d, True).encode()).hexdigest()
        b["exact"] = hashlib.sha1(census.normalized_body(lines, d, False).encode()).hexdigest()


def split_args(line, start):
    """Top-level argument texts of the call whose `(` is at `start`."""
    depth, out, s, i, n, in_str = 0, [], start + 1, start, len(line), None
    while i < n:
        ch = line[i]
        if in_str:
            if ch == "\\":
                i += 2
                continue
            if ch == in_str:
                in_str = None
        elif ch in "\"'":
            in_str = ch
        elif ch in "({[":
            depth += 1
        elif ch in ")}]":
            depth -= 1
            if depth == 0:
                if line[s:i].strip():
                    out.append(line[s:i].strip())
                return out
        elif ch == "," and depth == 1:
            out.append(line[s:i].strip())
            s = i + 1
        i += 1
    return None


RESULT_LIST = re.compile(r"^\s*(?:local )?((?:[A-Za-z_][A-Za-z0-9_]*, )+[A-Za-z_][A-Za-z0-9_]*) = $")


# A flattened position wider than this, on a signature with WIDE_CALLS or more
# residual calls, is worth changing in source. nobox types are kept unboxed on
# purpose (register pressure), so they get more room.
WIDE_ORDINARY, WIDE_NOBOX, WIDE_CALLS = 16, 24, 2
WIDE_RULE = ("A row is worth changing when a signature with %d or more residual calls carries it in more "
             "than %d lanes (%d for nobox)." % (WIDE_CALLS, WIDE_ORDINARY, WIDE_NOBOX))


def wide_advice(t):
    if t["nobox"]:
        return "pass the fields the callee reads, or make it a struct if it need not stay unboxed"
    return "make it a struct, or pass the fields the callee reads"


def worth_changing(t):
    n = t["worth_changing"]
    if not n:
        return ""
    return "yes (%d signature%s): %s" % (n, "" if n == 1 else "s", t["advice"])


def value_widening(bodies, lines, defs, binds, min_lanes=1):
    """Per value type, what flattening it costs at call boundaries that were
    not inlined: the signatures that carry its leaves, the residual calls into
    them, and the text of the leaf arguments, parameters and result lists.
    Needs the body records' `wire` field and the emitted Lua."""
    sigs = {}
    for b in bodies:
        wire, sym = b.get("wire"), b.get("symbol")
        if not wire or not sym:
            continue
        positions, at = [], 0
        for w in wire:
            if w["result"]:
                positions.append((w, None))
            else:
                positions.append((w, at))
                at += w["lanes"]
        positions = [(w, at) if w["lanes"] >= min_lanes else (dict(w, type=None), at) for w, at in positions]
        if any(w["type"] for w, _ in positions):
            sigs[sym] = (b, positions)
    types = {}

    def entry(w):
        t = types.get(w["type"])
        if t is None:
            t = types[w["type"]] = {"type": w["type"], "declared": w.get("declared"), "nobox": w["nobox"],
                                    "leaves": 0, "min_leaves": w["lanes"], "signatures": set(), "results": set(), "calls": 0,
                                    "arg_bytes": 0, "param_bytes": 0, "result_bytes": 0}
        t["leaves"] = max(t["leaves"], w["lanes"])
        t["min_leaves"] = min(t["min_leaves"], w["lanes"])
        return t

    sig_calls = Counter()
    by_name = {d["name"]: d for d in defs}
    for sym, (b, positions) in sigs.items():
        d = by_name.get(sym)
        head = lines[d["start"]] if d else ""
        m = FACTORY.match(head) or PLAIN.match(head)
        params = [p for p in (m.groups()[-1].split(", ") if m else []) if p]
        for w, at in positions:
            if not w["type"]:
                continue
            t = entry(w)
            if at is None:
                t["results"].add(sym)
            else:
                t["signatures"].add(sym)
                t["param_bytes"] += sum(len(p) + 2 for p in params[at:at + w["lanes"]])
    for d in defs:
        bound = binds.get(d["name"], [])
        for l in lines[d["start"] + 1:d["end"]]:
            if "(" not in l:
                continue
            for cm in CALL_START.finditer(l):
                callee = cm.group(0)[:-1]
                bm = BOUND.match(callee)
                if bm and int(bm.group(1)) < len(bound):
                    callee = bound[int(bm.group(1))]
                sig = sigs.get(callee)
                if sig is None:
                    continue
                args = split_args(l, cm.end() - 1)
                if args is None:
                    continue
                sig_calls[callee] += 1
                lhs = RESULT_LIST.match(l[:cm.start()])
                for w, at in sig[1]:
                    if not w["type"]:
                        continue
                    t = entry(w)
                    if at is None:
                        if lhs:
                            t["result_bytes"] += len(lhs.group(1)) + 1
                        continue
                    t["arg_bytes"] += sum(len(a) + 2 for a in args[at:at + w["lanes"]])
    for sym, (_, positions) in sigs.items():
        for w, _ in positions:
            limit = WIDE_NOBOX if w["nobox"] else WIDE_ORDINARY
            if w["type"] and w["lanes"] > limit and sig_calls[sym] >= WIDE_CALLS:
                types[w["type"]].setdefault("flagged", set()).add(sym)
    rows = []
    for t in types.values():
        t["calls"] = sum(sig_calls[sym] for sym in t["signatures"] | t["results"])
        t["worth_changing"] = len(t.pop("flagged", ()))
        t["advice"] = wide_advice(t) if t["worth_changing"] else ""
        f, _, line = (t["declared"] or "?:0").rpartition(":")
        t["file"], t["line"], t["package"] = f, int(line) if line.isdigit() else 0, t["type"].split("::", 1)[0]
        t["signatures"], t["results"] = len(t["signatures"]), len(t["results"])
        t["bytes"] = t["arg_bytes"] + t["param_bytes"] + t["result_bytes"]
        t["lanes"] = ("%d" % t["leaves"] if t["min_leaves"] == t["leaves"]
                      else "%d-%d" % (t["min_leaves"], t["leaves"]))
        rows.append(t)
    rows.sort(key=lambda t: -t["bytes"])
    return rows


# ---------------------------------------------------------------- aggregation

def ms(ns):
    return ns / 1e6


def aggregate(bodies, namer):
    defs = {}
    for b in bodies:
        g = defs.get(b["key"])
        if g is None:
            g = defs[b["key"]] = {
                "key": b["key"], "rec": b, "kind": b.get("kind"), "instances": 0,
                "cpu_ns": 0, "lua_bytes": 0, "lua_lines": 0, "mir_lowered": 0,
                "mir_inlined": 0, "mir_final": 0, "src_lines": b["src_lines"],
                "group": namer.group(b), "synthetic": b.get("synthetic", False),
                "annotations": b.get("annotations", []), "norm": Counter(),
                "spliced_insts": 0,
                **{s + "_ns": 0 for s in STAGES},
            }
        g["instances"] += 1
        for k in ["cpu_ns", "lua_bytes", "lua_lines", "mir_lowered", "mir_inlined", "mir_final"] + [s + "_ns" for s in STAGES]:
            g[k] += b.get(k, 0)
        g["spliced_insts"] += sum(s["instructions"] for s in b.get("splices", []))
        if b.get("kind") == "specialization" and "norm" in b:
            g["norm"][b["norm"]] += 1
    for g in defs.values():
        n = g["instances"]
        g["ratio"] = g["lua_lines"] / n / g["src_lines"]
        distinct = len(g["norm"]) if g["norm"] else n
        g["distinct"] = distinct
        g["redundant_cpu_ns"] = g["cpu_ns"] * (n - distinct) / n if n else 0
        g["redundant_bytes"] = g["lua_bytes"] * (n - distinct) / n if n else 0
    return defs


def aggregate_callees(bodies):
    callees = {}
    for b in bodies:
        total_spliced = sum(s["instructions"] for s in b.get("splices", []))
        base = max(b.get("mir_inlined", 0), b.get("mir_lowered", 0), 1)
        # Stage times are elapsed on the worker and inflate when the machine is
        # loaded; scale them to the body's thread CPU where it was measured.
        post = sum(b.get(s + "_ns", 0) for s in POST_SPLICE) * b["cpu_ns"] / max(b["stage_ns"], 1)
        for s in b.get("splices", []):
            c = callees.setdefault(s["callee"], {"key": s["callee"], "splices": 0,
                                                 "callers": 0, "instructions": 0,
                                                 "est_cpu_ns": 0.0,
                                                 "est_bytes": 0.0})
            share = min(1.0, s["instructions"] / base)
            c["splices"] += s["count"]
            c["callers"] += 1
            c["instructions"] += s["instructions"]
            c["est_cpu_ns"] += share * post
            c["est_bytes"] += share * b.get("lua_bytes", 0)
        b["inlined_share"] = min(1.0, total_spliced / base)
    return callees


def concentration(values):
    vals = sorted(values, reverse=True)
    total = sum(vals) or 1
    out = {}
    for p in (1, 5, 20, 50):
        k = max(1, round(len(vals) * p / 100))
        out[p] = sum(vals[:k]) / total
    return out


# ---------------------------------------------------------------- patterns

PATTERNS = [
    ("wide_signature", "Functions with 20+ parameters",
     lambda b: b["lua"]["params"] >= 20, lambda b: b["lua"]["params"],
     "parameters",
     "A value type (`nobox` struct or a struct the compiler scalarizes) passed "
     "by value is flattened into one parameter per leaf field. Pass the "
     "aggregate by reference (a boxed struct or a handle/index) or split the "
     "function so each part takes only the fields it reads."),
    ("unused_params", "Functions with 5+ unread parameters",
     lambda b: b["lua"]["unused_params"] >= 5, lambda b: b["lua"]["unused_params"],
     "unread parameters",
     "Every caller computes and passes arguments the body never reads, usually "
     "the unread fields of a flattened value type. Take only the fields used, "
     "or pass a reference. (Compiler side: dead-parameter elimination.)"),
    ("wide_calls", "Calls with 30+ arguments",
     lambda b: b["lua"]["wide_calls"] > 0, lambda b: b["lua"]["wide_calls"],
     "wide calls",
     "The callee takes a flattened value type; see wide signatures. Calls "
     "this wide also stop LuaJIT from keeping values in registers."),
    ("wide_results", "Assignments of 30+ call results",
     lambda b: b["lua"]["multi_results"] > 0, lambda b: b["lua"]["multi_results"],
     "wide result lists",
     "A function returns a value type by value and the compiler returns each "
     "leaf. Return a reference, write into an out-parameter struct, or return "
     "only what callers use."),
    ("many_locals", "Functions with 150+ locals (LuaJIT limit 200)",
     lambda b: b["lua"]["locals"] >= 150, lambda b: b["lua"]["locals"],
     "locals",
     "The function is close to LuaJIT's 200-local limit; inlining and value-"
     "type flattening add locals. Split the function."),
    ("many_upvalues", "Functions with 50+ bound callees (LuaJIT limit 60 upvalues)",
     lambda b: b["lua"]["upvalues"] >= 50, lambda b: b["lua"]["upvalues"],
     "upvalues",
     "Callees past the 60-upvalue cap become global lookups. Split the "
     "function; it calls too many distinct functions."),
    ("label_goto", "Bodies printed as labels and gotos (20+ labels)",
     lambda b: b["lua"]["labels"] >= 20, lambda b: b["lua"]["labels"],
     "labels",
     "Coroutines are printed as a state machine of labels and gotos, and "
     "control flow the structurer cannot express falls back to gotos. Keep "
     "coroutine bodies thin: move the work between yields into plain "
     "functions."),
    ("ctors_in_loops", "Table constructors inside loops",
     lambda b: b["lua"]["ctors_in_loops"] > 0, lambda b: b["lua"]["ctors_in_loops"],
     "constructors in loops",
     "A table is built every iteration: a struct literal, a boxed value, a "
     "closure environment or an Option/enum value in a loop. Hoist it out of "
     "the loop, reuse a buffer, or use a value type."),
    ("failure_blocks", "Per-site failure blocks (traceback + error)",
     lambda b: b["lua"]["tracebacks"] >= 3, lambda b: b["lua"]["tracebacks"],
     "failure blocks",
     "Each `unwrap`/`expect`/assertion site expands to its own traceback and "
     "error call. Unwrap once and bind the value, or use `if let`/`match` "
     "where absence is expected. (Compiler side: a shared failure helper.)"),
    ("lua_wraps", "inline(lua) wrapper blocks",
     lambda b: b["lua"]["lua_wraps"] >= 10, lambda b: b["lua"]["lua_wraps"],
     "wrapper blocks",
     "Each use of an `inline(lua)` function rebinds its arguments in a "
     "`do local ... end` block. Compiler side: substitute plain-register "
     "arguments; nothing to change in source."),
    ("guarded_stores", "Guarded field stores (getmetatable checks)",
     lambda b: b["lua"]["guards"] >= 10, lambda b: b["lua"]["guards"],
     "guards",
     "Field stores of possibly-nil values are guarded. Compiler side: skip the "
     "guard for non-nullable values; nothing to change in source."),
]


def find_patterns(bodies, defs):
    found = []
    with_lua = [b for b in bodies if "lua" in b]
    for pid, title, pred, metric, unit, advice in PATTERNS:
        hits = [b for b in with_lua if pred(b)]
        by_def = defaultdict(lambda: [0, 0, 0, 0, None])
        for b in hits:
            e = by_def[b["key"]]
            e[0] += 1
            e[1] = max(e[1], metric(b))
            e[2] += b["cpu_ns"]
            e[3] += b["lua_bytes"]
            e[4] = b
        found.append({"id": pid, "title": title, "unit": unit, "advice": advice,
                      "bodies": len(hits), "definitions": len(by_def),
                      "total": sum(metric(b) for b in hits),
                      "cpu_ns": sum(b["cpu_ns"] for b in hits),
                      "lua_bytes": sum(b["lua_bytes"] for b in hits),
                      "top": sorted(by_def.items(), key=lambda kv: (-kv[1][1], -kv[1][3]))[:12]})
    # source-level patterns that need no Lua text
    extreme = [g for g in defs.values() if g["ratio"] >= 40 and g["lua_lines"] >= 400]
    found.append({"id": "expansion", "title": "Extreme expansion (40+ Lua lines per source line, 400+ Lua lines)",
                  "unit": "Lua lines per source line",
                  "advice": "Usually inlining of large callees or a generic whose every instance "
                            "repeats the body. Check the inlined-callee table for what is spliced; "
                            "mark large helpers `@noinline` or make the generic part a thin wrapper "
                            "around a non-generic core.",
                  "bodies": sum(g["instances"] for g in extreme), "definitions": len(extreme),
                  "total": None, "cpu_ns": sum(g["cpu_ns"] for g in extreme),
                  "lua_bytes": sum(g["lua_bytes"] for g in extreme),
                  "top": [(g["key"], [g["instances"], round(g["ratio"], 1), g["cpu_ns"], g["lua_bytes"], g["rec"]])
                          for g in sorted(extreme, key=lambda g: -g["lua_bytes"])[:12]]})
    many = [g for g in defs.values() if g["instances"] >= 50]
    found.append({"id": "many_instances", "title": "Generics instantiated 50+ times",
                  "unit": "instances",
                  "advice": "Every instance is lowered, optimized and printed separately. Move the "
                            "type-independent part of the body into a non-generic function, take a "
                            "trait object/handle where the type parameter is only passed through, or "
                            "reduce the set of types it is used with.",
                  "bodies": sum(g["instances"] for g in many), "definitions": len(many),
                  "total": None, "cpu_ns": sum(g["cpu_ns"] for g in many),
                  "lua_bytes": sum(g["lua_bytes"] for g in many),
                  "top": [(g["key"], [g["instances"], g["distinct"], g["cpu_ns"], g["lua_bytes"], g["rec"]])
                          for g in sorted(many, key=lambda g: -g["cpu_ns"])[:12]]})
    by_symbol = {b["symbol"]: b for b in bodies if b.get("symbol")}
    targets = {}
    for b in with_lua:
        for callee, n in b["lua"]["wide_callees"]:
            t = targets.setdefault(callee, {"calls": 0, "callers": set(), "max_args": 0,
                                            "rec": by_symbol.get(callee)})
            t["calls"] += 1
            t["callers"].add(b["key"])
            t["max_args"] = max(t["max_args"], n)
    wide_targets = sorted(targets.items(), key=lambda kv: -kv[1]["calls"])
    synth = defaultdict(lambda: [0, 0, 0])
    for b in bodies:
        if b.get("synthetic"):
            k = "synthetic"
        elif b.get("annotations"):
            k = "@" + ",".join(sorted(set(b["annotations"])))
        else:
            continue
        synth[k][0] += 1
        synth[k][1] += b["cpu_ns"]
        synth[k][2] += b["lua_bytes"]
    return found, synth, wide_targets


# ---------------------------------------------------------------- recommendations

def recommendations(defs, callees, namer, limit=10):
    recs = []
    for g in defs.values():
        if g["kind"] == "specialization" and g["instances"] >= 4 and g["redundant_cpu_ns"] > 0:
            recs.append({"what": "Generic `%s` (%s): %d instances, only %d distinct after erasing type names"
                         % (namer.name(g["rec"]), namer.site(g["rec"]), g["instances"], g["distinct"]),
                         "change": "Hoist the type-independent body into a non-generic function so the "
                                   "identical instances collapse into one.",
                         "cpu_ns": g["redundant_cpu_ns"], "bytes": g["redundant_bytes"],
                         "basis": "measured cost of the duplicate instances"})
        elif g["kind"] == "specialization" and g["instances"] >= 50:
            recs.append({"what": "Generic `%s` (%s): %d instances"
                         % (namer.name(g["rec"]), namer.site(g["rec"]), g["instances"]),
                         "change": "Reduce the instantiations: make the type-dependent part a small "
                                   "generic shim around a shared core.",
                         "cpu_ns": g["cpu_ns"] * 0.5, "bytes": g["lua_bytes"] * 0.5,
                         "basis": "half the measured cost of all instances (estimate)"})
    for c in callees.values():
        if c["splices"] >= 20 and c["est_bytes"] > 0:
            rec = parse_key(c["key"])
            rec_full = dict(rec)
            recs.append({"what": "Inlined callee `%s` (%s): spliced %d times into %d bodies"
                         % (namer.name(rec_full), namer.site(rec_full), c["splices"], c["callers"]),
                         "change": "Keep it out of line (`@noinline`), or shrink it so the copies are small.",
                         "cpu_ns": c["est_cpu_ns"] * 0.8, "bytes": c["est_bytes"] * 0.8,
                         "basis": "80% of the estimated cost of its copies (a call stays behind)"})
    for g in defs.values():
        if g["kind"] != "specialization" and g["cpu_ns"] > 0 and g["ratio"] >= 40 and g["lua_lines"] >= 400:
            recs.append({"what": "`%s` (%s): %d source lines print as %d Lua lines (x%.0f)"
                         % (namer.name(g["rec"]), namer.site(g["rec"]), g["src_lines"], g["lua_lines"], g["ratio"]),
                         "change": "Find what is inlined into it (inlined-callee table) and keep the "
                                   "large callees out of line.",
                         "cpu_ns": g["cpu_ns"] * 0.5, "bytes": g["lua_bytes"] * 0.5,
                         "basis": "half its measured cost (estimate)"})
    recs.sort(key=lambda r: -(r["cpu_ns"] + r["bytes"] * BYTE_NS))
    return recs[:limit]


# ---------------------------------------------------------------- output

def fmt_ms(ns):
    return "%.1f" % ms(ns)


def fmt_kb(b):
    return "%.1f" % (b / 1024)


def pct(x):
    return "%.1f%%" % (100 * x)


CSS = """
:root { --bg:#fff; --fg:#1d1d1f; --muted:#666; --line:#ddd; --head:#f3f3f5; --accent:#2a62c9; --bar:#9bb5e8; }
@media (prefers-color-scheme: dark) { :root:not([data-theme="light"]) {
  --bg:#17181a; --fg:#e6e6e6; --muted:#9a9a9a; --line:#33363a; --head:#222428; --accent:#7ea6f0; --bar:#3c5a94; } }
:root[data-theme="dark"] { --bg:#17181a; --fg:#e6e6e6; --muted:#9a9a9a; --line:#33363a; --head:#222428; --accent:#7ea6f0; --bar:#3c5a94; }
body { background:var(--bg); color:var(--fg); font:14px/1.45 -apple-system, system-ui, sans-serif; margin:0 auto; max-width:1200px; padding:16px; }
h1 { font-size:22px; } h2 { font-size:18px; margin-top:32px; border-bottom:1px solid var(--line); padding-bottom:4px; }
h3 { font-size:15px; margin-top:20px; }
.note { color:var(--muted); font-size:13px; }
table { border-collapse:collapse; width:100%; margin:8px 0 16px; font-size:13px; }
th, td { border-bottom:1px solid var(--line); padding:3px 6px; text-align:right; white-space:nowrap; }
th { background:var(--head); cursor:pointer; position:sticky; top:0; }
td.l, th.l { text-align:left; white-space:normal; word-break:break-all; }
.scroll { max-height:520px; overflow:auto; border:1px solid var(--line); }
.bar { display:inline-block; height:9px; background:var(--bar); vertical-align:middle; }
code { font-size:12px; }
ol li { margin-bottom:6px; }
"""

JS = """
document.querySelectorAll('table.sort').forEach(function (t) {
  t.querySelectorAll('th').forEach(function (th, i) {
    th.addEventListener('click', function () {
      var body = t.tBodies[0], rows = Array.prototype.slice.call(body.rows);
      var dir = th.dataset.dir === 'desc' ? 1 : -1; th.dataset.dir = dir === 1 ? 'asc' : 'desc';
      rows.sort(function (a, b) {
        var x = a.cells[i].dataset.v || a.cells[i].textContent, y = b.cells[i].dataset.v || b.cells[i].textContent;
        var nx = parseFloat(x), ny = parseFloat(y);
        if (!isNaN(nx) && !isNaN(ny)) return dir * (nx - ny);
        return dir * x.localeCompare(y);
      });
      rows.forEach(function (r) { body.appendChild(r); });
    });
  });
});
"""


def table(headers, rows, cls="sort"):
    out = ['<div class="scroll"><table class="%s"><thead><tr>' % cls]
    for h in headers:
        left = h.startswith("<")
        out.append('<th class="l">%s</th>' % h[1:] if left else "<th>%s</th>" % h)
    out.append("</tr></thead><tbody>")
    for r in rows:
        out.append("<tr>")
        for h, v in zip(headers, r):
            if h.startswith("<"):
                out.append('<td class="l">%s</td>' % html.escape(str(v)))
            else:
                out.append("<td>%s</td>" % html.escape(str(v)))
        out.append("</tr>")
    out.append("</tbody></table></div>")
    return "".join(out)


def build(args):
    namer = Namer(args.root, args.public, args.mask)
    bodies, modules = load_costs(args.costs)
    stats = load_stats(args.stats)
    lua_total = None
    if args.lua:
        lines, ldefs, by_name, binds = lua_index(args.lua)
        attach_lua(bodies, lines, ldefs, by_name, binds)
        lua_total = sum(len(l) + 1 for l in lines)
    defs = aggregate(bodies, namer)
    callees = aggregate_callees(bodies)
    patterns, synth, wide_targets = find_patterns(bodies, defs) if args.lua else ([], {}, [])
    widening = value_widening(bodies, lines, ldefs, binds, args.wide_lanes) if args.lua else []
    recs = recommendations(defs, callees, namer)

    total_cpu = sum(b["cpu_ns"] for b in bodies)
    total_stage = sum(b["stage_ns"] for b in bodies)
    measured = [b for b in bodies if b.get("thread_cpu_ns") is not None]
    measured_cpu = sum(b["thread_cpu_ns"] for b in measured)
    measured_stage = sum(b["stage_ns"] for b in measured)
    total_bytes = sum(b["lua_bytes"] for b in bodies if b.get("emitted", True))
    total_lines = sum(b["lua_lines"] for b in bodies if b.get("emitted", True))

    groups = defaultdict(lambda: Counter())
    for b in bodies:
        gname = namer.group(b)
        groups[gname]["bodies"] += 1
        groups[gname]["cpu_ns"] += b["cpu_ns"]
        groups[gname]["lua_bytes"] += b["lua_bytes"]
        groups[gname]["lua_lines"] += b["lua_lines"]
    files = defaultdict(lambda: Counter())
    file_rec = {}
    for b in bodies:
        f = b["file"]
        file_rec.setdefault(f, b)
        files[f]["bodies"] += 1
        files[f]["cpu_ns"] += b["cpu_ns"]
        files[f]["lua_bytes"] += b["lua_bytes"]
        files[f]["lua_lines"] += b["lua_lines"]
        files[f]["spec"] += b.get("kind") == "specialization"
    front = Counter()
    for m in modules:
        files[m["file"]][m["pass"]] += m["elapsed_ns"]
        file_rec.setdefault(m["file"], m)
        front[m["pass"]] += m["elapsed_ns"]
        groups[namer.group(m)][m["pass"]] += m["elapsed_ns"]
    stage_tot = {s: sum(b.get(s + "_ns", 0) for b in bodies) for s in STAGES}
    regalloc = sum(b.get("regalloc_ns", 0) for b in bodies)

    conc_defs_cpu = concentration([g["cpu_ns"] for g in defs.values()])
    conc_defs_bytes = concentration([g["lua_bytes"] for g in defs.values()])
    conc_files = concentration([c["cpu_ns"] for c in files.values()])

    H = []
    title = args.title or "Compile cost by source"
    H.append("<!doctype html><html><head><meta charset='utf-8'><meta name='viewport' content='width=device-width, initial-scale=1'><title>%s</title><style>%s</style></head><body>" % (html.escape(title), CSS))
    H.append("<h1>%s</h1>" % html.escape(title))
    H.append("<p class='note'>%d emitted bodies from %d source definitions in %d files. "
             "CPU = worker thread CPU where measured, else summed stage time; all CPU numbers are summed over threads, not wall. "
             "Lua = bytes/lines of each body's own text.</p>" % (len(bodies), len(defs), len(files)))

    # totals and sanity check
    H.append("<h2>Totals and sanity check</h2><ul>")
    H.append("<li>Body CPU, all bodies: <b>%s ms</b> (thread CPU for %d bodies: %s ms; their summed stage times: %s ms; the other %d bodies use stage time).</li>"
             % (fmt_ms(total_cpu), len(measured), fmt_ms(measured_cpu), fmt_ms(measured_stage), len(bodies) - len(measured)))
    if stats:
        H.append("<li>Compile budget: wall %.0f ms at %s threads; codegen wall %.0f ms (fan-out window %.0f ms); "
                 "summed per-pass unit time <code>unit_cpu_ms</code> = %.1f ms. Summed stage time of the dump = %.1f ms, "
                 "unattributed remainder %.1f ms (%.1f%%).</li>"
                 % (stats["wall_ms"] or 0, stats["threads"], stats["codegen"] or 0, stats["fan-out"] or 0,
                    stats["unit_cpu_ms"] or 0, ms(total_stage), (stats["unit_cpu_ms"] or 0) - ms(total_stage),
                    100 * ((stats["unit_cpu_ms"] or 0) - ms(total_stage)) / max(stats["unit_cpu_ms"] or 1, 1e-9)))
        H.append("<li>Front end (wall): parse+stdlib %.0f ms, semantic %.0f ms. Per-module front-end time below covers "
                 "<i>lower functions</i> (%.0f ms summed over threads) and <i>infer functions</i> (%.0f ms summed); the rest of "
                 "semantic analysis is whole-program and not attributed.</li>"
                 % (stats["parse+stdlib"] or 0, stats["semantic"] or 0, ms(front["lower functions"]), ms(front["infer functions"])))
    if lua_total is not None:
        H.append("<li>Lua: bodies own %s KB in %d lines of the %s KB output (%s); the rest is prelude, type tables, "
                 "binding lines, registration and global initialization glue.</li>"
                 % (fmt_kb(total_bytes), total_lines, fmt_kb(lua_total), pct(total_bytes / max(lua_total, 1))))
    H.append("</ul>")
    H.append(table(["<stage", "elapsed on worker, ms (summed)", "share"],
                   [[STAGE_LABEL[s], fmt_ms(stage_tot[s]), pct(stage_tot[s] / max(total_stage, 1))] for s in STAGES]
                   + [["of which register allocation", fmt_ms(regalloc), pct(regalloc / max(total_stage, 1))]], cls="plain"))
    H.append(table(["<package group", "bodies", "CPU ms", "CPU share", "Lua KB", "Lua lines", "lower fns ms", "infer ms"],
                   [[k, v["bodies"], fmt_ms(v["cpu_ns"]), pct(v["cpu_ns"] / max(total_cpu, 1)), fmt_kb(v["lua_bytes"]),
                     v["lua_lines"], fmt_ms(v["lower functions"]), fmt_ms(v["infer functions"])]
                    for k, v in sorted(groups.items(), key=lambda kv: -kv[1]["cpu_ns"])], cls="plain"))

    # recommendations
    H.append("<h2>Most valuable source changes (automatic ranking)</h2>")
    H.append("<p class='note'>Ranked by estimated saving of body CPU (ms, summed over threads) plus Lua bytes, "
             "one KB counted as %d us (about LuaJIT's parse time per KB). " % (BYTE_NS * 1024 // 1000))
    H.append("<span class='note'>"
             "Each item says how its saving was estimated.</span><ol>")
    for r in recs:
        H.append("<li><b>%s</b><br>%s<br><span class='note'>saves about %s ms CPU, %s KB Lua; %s</span></li>"
                 % (html.escape(r["what"]), html.escape(r["change"]), fmt_ms(r["cpu_ns"]), fmt_kb(r["bytes"]), html.escape(r["basis"])))
    H.append("</ol>")

    # concentration
    H.append("<h2>How concentrated the cost is</h2>")
    H.append(table(["<population", "top 1%", "top 5%", "top 20%", "top 50%"],
                   [["source definitions by CPU (%d)" % len(defs)] + [pct(conc_defs_cpu[p]) for p in (1, 5, 20, 50)],
                    ["source definitions by Lua bytes"] + [pct(conc_defs_bytes[p]) for p in (1, 5, 20, 50)],
                    ["files by CPU (%d)" % len(files)] + [pct(conc_files[p]) for p in (1, 5, 20, 50)]], cls="plain"))
    kinds = defaultdict(Counter)
    for b in bodies:
        kinds[b.get("kind")]["n"] += 1
        kinds[b.get("kind")]["cpu"] += b["cpu_ns"]
        kinds[b.get("kind")]["bytes"] += b["lua_bytes"]
    H.append(table(["<body kind", "bodies", "CPU ms", "share", "Lua KB"],
                   [[k, v["n"], fmt_ms(v["cpu"]), pct(v["cpu"] / max(total_cpu, 1)), fmt_kb(v["bytes"])]
                    for k, v in sorted(kinds.items(), key=lambda kv: -kv[1]["cpu"])], cls="plain"))

    # per definition
    top_defs = sorted(defs.values(), key=lambda g: -g["cpu_ns"])[:args.rows]
    H.append("<h2>Source definitions</h2><p class='note'>One row per source function, method, global or decorator; a generic's "
             "specializations are summed into it. CPU ms summed over threads; stage columns are elapsed on the worker. "
             "MIR = instructions: lowered, after inlining, after cleanup (summed over instances). x src = Lua lines of one "
             "instance per source line. "
             "Click a header to sort. Top %d by CPU.</p>" % len(top_defs))
    H.append(table(["<definition", "<site", "<kind", "inst.", "CPU ms", "ms/inst", "lower", "inline", "cleanup", "valid.", "print",
                    "MIR low", "MIR inl", "MIR final", "inlined %", "Lua KB", "Lua lines", "src lines", "x src"],
                   [[namer.name(g["rec"]), namer.site(g["rec"]), g["kind"], g["instances"], fmt_ms(g["cpu_ns"]),
                     "%.3f" % (ms(g["cpu_ns"]) / g["instances"]), fmt_ms(g["lower_ns"]), fmt_ms(g["inline_ns"]),
                     fmt_ms(g["cleanup_ns"]), fmt_ms(g["validation_ns"]), fmt_ms(g["emit_ns"]), g["mir_lowered"],
                     g["mir_inlined"], g["mir_final"],
                     "%.0f" % (100 * min(1.0, g["spliced_insts"] / max(g["mir_inlined"], 1))),
                     fmt_kb(g["lua_bytes"]), g["lua_lines"], g["src_lines"], "%.1f" % g["ratio"]] for g in top_defs]))

    # generics
    gens = sorted((g for g in defs.values() if g["kind"] == "specialization"), key=lambda g: -g["cpu_ns"])[:args.rows]
    H.append("<h2>Generics</h2><p class='note'>Instances = specializations emitted. Distinct = different bodies after "
             "erasing type names from the Lua (needs <code>--lua</code>); the rest are near-duplicates. "
             "Redundant = the cost of the near-duplicate instances.</p>")
    gen_rows = []
    for g in gens:
        args_sample = sorted({b["symbol"].split("_spec_", 1)[1] if "_spec_" in b.get("symbol", "") else b.get("symbol", "")
                              for b in bodies if b["key"] == g["key"]})[:3]
        gen_rows.append([namer.name(g["rec"]), namer.site(g["rec"]), g["instances"], g["distinct"], fmt_ms(g["cpu_ns"]),
                         fmt_ms(g["redundant_cpu_ns"]), fmt_kb(g["lua_bytes"]), fmt_kb(g["redundant_bytes"]),
                         "; ".join(namer.symbol(g["rec"], a) for a in args_sample)])
    H.append(table(["<generic", "<site", "instances", "distinct", "CPU ms", "redundant ms", "Lua KB", "redundant KB", "<type arguments (sample)"], gen_rows))

    # files
    frows = []
    for f, c in sorted(files.items(), key=lambda kv: -(kv[1]["cpu_ns"] + kv[1]["infer functions"] + kv[1]["lower functions"]))[:args.rows]:
        rec = file_rec[f]
        frows.append([namer.path(rec), namer.group(rec), c["bodies"], c["spec"], fmt_ms(c["cpu_ns"]),
                      pct(c["cpu_ns"] / max(total_cpu, 1)), fmt_ms(c["lower functions"]), fmt_ms(c["infer functions"]),
                      fmt_kb(c["lua_bytes"]), c["lua_lines"]])
    H.append("<h2>Files</h2><p class='note'>Backend CPU of the bodies defined in the file (a generic's instances count "
             "where the generic is defined), and the front-end passes timed per module (elapsed on worker, summed).</p>")
    H.append(table(["<file", "<group", "bodies", "spec.", "backend CPU ms", "share", "lower fns ms", "infer ms", "Lua KB", "Lua lines"], frows))

    # inlining
    crow = []
    for c in sorted(callees.values(), key=lambda c: -c["est_cpu_ns"])[:args.rows]:
        rec = parse_key(c["key"])
        crow.append([namer.name(rec), namer.site(rec), c["splices"], c["callers"], c["instructions"],
                     fmt_ms(c["est_cpu_ns"]), fmt_kb(c["est_bytes"])])
    H.append("<h2>Inlined callees</h2><p class='note'>Splices = copies spliced into callers; MIR copied = callee "
             "instructions copied in. Est. CPU is the callers' post-splice work (cleanup, "
             "validation, printing) apportioned by the copies' share of each caller's MIR; est. Lua KB apportions the "
             "callers' text the same way. Both are estimates; the splicing itself is in each caller's inline stage.</p>")
    H.append(table(["<callee", "<site", "splices", "callers", "MIR copied", "est. CPU ms", "est. Lua KB"], crow))
    heavy = sorted((b for b in bodies if b.get("inlined_share", 0) >= 0.5 and b["lua_bytes"] > 2000),
                   key=lambda b: -b["lua_bytes"])[:40]
    H.append("<h3>Callers made mostly of inlined code</h3>")
    H.append(table(["<body", "<site", "inlined %", "MIR inl", "Lua KB", "CPU ms"],
                   [[namer.symbol(b, b.get("symbol") or b["name"]), namer.site(b), "%.0f" % (100 * b["inlined_share"]),
                     b.get("mir_inlined", 0), fmt_kb(b["lua_bytes"]), fmt_ms(b["cpu_ns"])] for b in heavy]))

    # patterns
    if patterns:
        H.append("<h2>Output patterns traced to source</h2><p class='note'>Found in each body's own Lua text. "
                 "CPU and Lua columns are the totals of the bodies showing the pattern.</p>")
        H.append(table(["<pattern", "bodies", "definitions", "total", "CPU ms", "Lua KB"],
                       [[p["title"], p["bodies"], p["definitions"], p["total"] if p["total"] is not None else "",
                         fmt_ms(p["cpu_ns"]), fmt_kb(p["lua_bytes"])] for p in patterns], cls="plain"))
        for p in patterns:
            if not p["top"]:
                continue
            H.append("<h3>%s</h3><p>%s</p>" % (html.escape(p["title"]), html.escape(p["advice"])))
            if p["id"] == "expansion":
                hdr = ["<definition", "<site", "instances", "x src", "CPU ms", "Lua KB"]
            elif p["id"] == "many_instances":
                hdr = ["<definition", "<site", "instances", "distinct", "CPU ms", "Lua KB"]
            else:
                hdr = ["<definition", "<site", "bodies", "max " + p["unit"], "CPU ms", "Lua KB"]
            H.append(table(hdr, [[namer.name(e[4]), namer.site(e[4]), e[0], e[1], fmt_ms(e[2]), fmt_kb(e[3])] for _, e in p["top"]]))
            if p["id"] == "wide_calls" and wide_targets:
                H.append("<p>The callees these calls go to, which is where the signature can change. A name ending in "
                         "<code>_vt</code> or <code>_lct</code> is a variant or struct constructor of that type.</p>")
                H.append(table(["<callee", "<site", "calls", "calling definitions", "max args"],
                               [[namer.name(t["rec"]) if t["rec"] else
                                 ("private:" + hashlib.sha1(c.encode()).hexdigest()[:8] if args.mask else c),
                                 namer.site(t["rec"]) if t["rec"] else "", t["calls"], len(t["callers"]), t["max_args"]]
                                for c, t in wide_targets[:40]]))
        if widening:
            H.append("<h3>Value types widening call boundaries</h3><p>Each value type whose leaves a residual "
                     "(not inlined) call passes one by one, counting positions of %d or more lanes "
                     "(<code>--wide-lanes</code>; a lane is one Lua value, after unread leaves are dropped): "
                     "the signatures that take or return it flattened, the "
                     "calls into them, and the text of its leaf arguments, parameters and result lists. Bodies also "
                     "move, store and kill every leaf, often several times this text. Boxing a type (a plain "
                     "<code>struct</code>), passing fewer fields, or inlining the callee removes it; "
                     "<code>nobox</code> types never box. %s</p>" % (args.wide_lanes, html.escape(WIDE_RULE)))
            H.append(table(["<type", "<declared", "nobox", "lanes", "signatures", "returning", "calls",
                            "arg KB", "param KB", "result KB", "total KB", "<worth changing"],
                           [[namer.symbol(t, t["type"]), namer.site(t), "yes" if t["nobox"] else "", t["lanes"], t["signatures"], t["results"], t["calls"],
                             fmt_kb(t["arg_bytes"]), fmt_kb(t["param_bytes"]), fmt_kb(t["result_bytes"]),
                             fmt_kb(t["bytes"]), worth_changing(t)] for t in widening[:args.rows]]))
        if synth:
            H.append("<h3>Generated and annotated definitions</h3><p class='note'>Bodies the compiler synthesized "
                     "(derives, generated accessors) and bodies grouped by their annotations.</p>")
            H.append(table(["<origin", "bodies", "CPU ms", "Lua KB"],
                           [[k, v[0], fmt_ms(v[1]), fmt_kb(v[2])] for k, v in sorted(synth.items(), key=lambda kv: -kv[1][1])[:30]]))
    H.append("<script>%s</script></body></html>" % JS)
    if args.html:
        with open(args.html, "w") as f:
            f.write("".join(H))

    # markdown summary
    M = ["# %s" % title, ""]
    M.append("- %d bodies, %d source definitions, %d files." % (len(bodies), len(defs), len(files)))
    M.append("- Body CPU (summed over threads): %s ms; thread CPU measured for %d bodies (%s ms) vs their stage time %s ms."
             % (fmt_ms(total_cpu), len(measured), fmt_ms(measured_cpu), fmt_ms(measured_stage)))
    if stats:
        M.append("- Compile wall %.0f ms at %s threads; codegen unit_cpu_ms %.1f vs dump stage sum %.1f (remainder %.1f ms)."
                 % (stats["wall_ms"] or 0, stats["threads"], stats["unit_cpu_ms"] or 0, ms(total_stage),
                    (stats["unit_cpu_ms"] or 0) - ms(total_stage)))
        M.append("- Front end wall: parse+stdlib %.0f ms, semantic %.0f ms; per-module lower functions %.0f ms + infer %.0f ms (summed over threads)."
                 % (stats["parse+stdlib"] or 0, stats["semantic"] or 0, ms(front["lower functions"]), ms(front["infer functions"])))
    if lua_total is not None:
        M.append("- Lua: bodies %s KB of %s KB output (%s)." % (fmt_kb(total_bytes), fmt_kb(lua_total), pct(total_bytes / max(lua_total, 1))))
    M.append("- Stages (elapsed on worker, summed): " + ", ".join("%s %s ms" % (STAGE_LABEL[s], fmt_ms(stage_tot[s])) for s in STAGES))
    M.append("- Groups: " + "; ".join("%s %s ms CPU / %s KB" % (k, fmt_ms(v["cpu_ns"]), fmt_kb(v["lua_bytes"]))
                                     for k, v in sorted(groups.items(), key=lambda kv: -kv[1]["cpu_ns"])))
    M.append("- Concentration (definitions by CPU): top 1%% %s, 5%% %s, 20%% %s, 50%% %s."
             % tuple(pct(conc_defs_cpu[p]) for p in (1, 5, 20, 50)))
    M.append("")
    M.append("## Top source changes (automatic)")
    M.append("")
    for i, r in enumerate(recs, 1):
        M.append("%d. %s. %s Saves about %s ms CPU, %s KB Lua (%s)." % (i, r["what"], r["change"], fmt_ms(r["cpu_ns"]), fmt_kb(r["bytes"]), r["basis"]))
    M.append("")
    M.append("## Top definitions by CPU")
    M.append("")
    M.append("| definition | site | kind | inst. | CPU ms | Lua KB | x src |")
    M.append("|---|---|---|---:|---:|---:|---:|")
    for g in top_defs[:15]:
        M.append("| %s | %s | %s | %d | %s | %s | %.1f |" % (namer.name(g["rec"]), namer.site(g["rec"]), g["kind"], g["instances"],
                                                         fmt_ms(g["cpu_ns"]), fmt_kb(g["lua_bytes"]), g["ratio"]))
    if patterns:
        M.append("")
        M.append("## Output patterns")
        M.append("")
        for p in patterns:
            M.append("- %s: %d bodies in %d definitions, %s ms CPU, %s KB." % (p["title"], p["bodies"], p["definitions"], fmt_ms(p["cpu_ns"]), fmt_kb(p["lua_bytes"])))
    if widening:
        M.append("")
        M.append("## Value types widening call boundaries")
        M.append("")
        M.append("Flattened value positions of %d or more lanes in residual calls; bytes are leaf arguments, "
                 "parameters and result lists. %s" % (args.wide_lanes, WIDE_RULE))
        M.append("")
        M.append("| type | lanes | nobox | signatures | returning | calls | total KB | worth changing |")
        M.append("|---|---:|---|---:|---:|---:|---:|---|")
        for t in widening[:15]:
            M.append("| %s | %s | %s | %d | %d | %d | %s | %s |" % (
                namer.symbol(t, t["type"]), t["lanes"], "yes" if t["nobox"] else "",
                t["signatures"], t["results"], t["calls"], fmt_kb(t["bytes"]), worth_changing(t)))
    text = "\n".join(M) + "\n"
    if args.md:
        with open(args.md, "w") as f:
            f.write(text)
    if not args.html and not args.md:
        sys.stdout.write(text)
    return {"bodies": bodies, "defs": defs, "callees": callees, "patterns": patterns,
            "wide_targets": wide_targets, "widening": widening}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("costs", help="the ROCK_BODY_COSTS file")
    p.add_argument("--stats", help="the ROCK_COMPILE_STATS file of the same compile (last record is used)")
    p.add_argument("--lua", help="the Lua the same compile wrote, for the output patterns")
    p.add_argument("--root", help="directory program paths are shown relative to")
    p.add_argument("--public", default=r"^stone", help="regex of package names that are public (default ^stone)")
    p.add_argument("--mask", action="store_true", help="hash names and files outside stdlib and public packages")
    p.add_argument("--html", help="write the HTML report here")
    p.add_argument("--md", help="write the Markdown summary here")
    p.add_argument("--rows", type=int, default=300, help="rows per table (default 300)")
    p.add_argument("--title", help="report title")
    p.add_argument("--wide-lanes", type=int, default=8,
                   help="count a flattened value position from this many lanes (default 8)")
    build(p.parse_args())


if __name__ == "__main__":
    main()
