#!/usr/bin/env python3
"""Render the perf history as a standalone HTML page of line charts.

One chart per metric and corpus, commits on the x axis in commit-time order,
the latest row's values in a table below. No external assets, so the page can
be opened from disk or published anywhere.

Usage: perf/report.py [--history PATH] [--out perf/report.html] [--last N]
"""
import argparse
import html
import json
import os
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from run import default_history  # noqa: E402

# (title, unit, path into a corpus row). Grouped so related charts sit together.
GROUPS = [
    ("Cold build", [
        ("rock compile", "ms", ("cold_compile", "mean_ms")),
        ("rock check", "ms", ("cold_check", "mean_ms")),
        ("bone semantic (in-process)", "ms", ("backend_stats", "semantic_ms")),
        ("skull emit (in-process)", "ms", ("backend_stats", "emit_ms")),
        ("skull emit setup", "ms", ("backend_stats", "emit_setup_ms")),
        ("parse", "ms", ("backend_stats", "parse_ms")),
    ]),
    ("Time budget (fastest of several cold compiles; every level sums to its parent)", [
        ("whole compile", "ms", ("budget", "flat", "compile")),
        ("discovery", "ms", ("budget", "flat", "compile/discovery")),
        ("parse+stdlib", "ms", ("budget", "flat", "compile/parse+stdlib")),
        ("semantic", "ms", ("budget", "flat", "compile/semantic")),
        ("codegen setup (serial)", "ms", ("budget", "flat", "compile/codegen/setup")),
        ("setup: graph", "ms", ("budget", "flat", "compile/codegen/setup/graph")),
        ("setup: plans+lanes", "ms", ("budget", "flat", "compile/codegen/setup/concrete plans+lanes")),
        ("setup: inline candidate table", "ms", ("budget", "flat", "compile/codegen/setup/inline candidate table")),
        ("codegen fan-out (wall)", "ms", ("budget", "flat", "compile/codegen/fan-out")),
        ("unit cpu, all passes (summed)", "ms", ("budget", "flat", "compile/codegen#cpu")),
        ("unit cpu: lower hir->mir", "ms", ("budget", "flat", "compile/codegen#cpu/lower hir->mir")),
        ("unit cpu: emit: stringify", "ms", ("budget", "flat", "compile/codegen#cpu/emit: stringify")),
        ("codegen assembly (serial)", "ms", ("budget", "flat", "compile/codegen/assembly")),
        ("write", "ms", ("budget", "flat", "compile/write")),
    ]),
    ("Incremental build (hot reload)", [
        ("body edit -> overlay", "ms", ("incremental", "body", "mean_ms")),
        ("body edit overlay emit", "ms", ("incremental", "body", "emit_mean_ms")),
        ("signature edit -> full rebuild", "ms", ("incremental", "signature", "mean_ms")),
        ("initial session build", "ms", ("incremental", "init_ms")),
    ]),
    ("Language server", [
        ("body edit update", "ms", ("lsp", "body", "update_mean_ms")),
        ("signature edit update", "ms", ("lsp", "signature", "update_mean_ms")),
        ("broken edit update", "ms", ("lsp", "broken", "update_mean_ms")),
        ("worst edit refresh", "ms", ("lsp", "signature", "worst_edit_ms")),
        ("hover (after body edit)", "ms", ("lsp", "body", "hover_mean_ms")),
        ("completion (after body edit)", "ms", ("lsp", "body", "completion_mean_ms")),
    ]),
]

CSS = """
:root {
  color-scheme: light;
  --surface: #fcfcfb; --card: #ffffff; --line: #e6e5e1;
  --text: #0b0b0b; --text-2: #52514e; --muted: #8a8984;
  --series: #2a78d6; --series-soft: #2a78d61f; --good: #008300; --bad: #e34948;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    color-scheme: dark;
    --surface: #1a1a19; --card: #222221; --line: #343432;
    --text: #f0efec; --text-2: #c3c2b7; --muted: #8a8984;
    --series: #3987e5; --series-soft: #3987e52e; --good: #4caf50; --bad: #e66767;
  }
}
:root[data-theme="dark"] {
  color-scheme: dark;
  --surface: #1a1a19; --card: #222221; --line: #343432;
  --text: #f0efec; --text-2: #c3c2b7; --muted: #8a8984;
  --series: #3987e5; --series-soft: #3987e52e; --good: #4caf50; --bad: #e66767;
}
* { box-sizing: border-box; }
body { margin: 0; padding: 24px 16px 48px; background: var(--surface); color: var(--text);
  font: 14px/1.45 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif; }
main { max-width: 1240px; margin: 0 auto; }
h1 { font-size: 22px; margin: 0 0 4px; }
h2 { font-size: 16px; margin: 32px 0 12px; color: var(--text); }
.sub { color: var(--text-2); margin: 0 0 16px; }
.controls { display: flex; gap: 12px; flex-wrap: wrap; align-items: center; margin: 12px 0 8px; }
.controls label { color: var(--text-2); }
select { font: inherit; color: var(--text); background: var(--card); border: 1px solid var(--line);
  border-radius: 6px; padding: 4px 8px; }
.grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(360px, 1fr)); gap: 12px; }
.card { background: var(--card); border: 1px solid var(--line); border-radius: 8px; padding: 12px 12px 8px; }
.card h3 { margin: 0; font-size: 13px; font-weight: 600; color: var(--text-2); }
.card .latest { font-size: 22px; font-weight: 600; margin: 2px 0 0; font-variant-numeric: tabular-nums; }
.card .latest small { font-size: 12px; font-weight: 500; color: var(--muted); margin-left: 6px; }
.delta-good { color: var(--good); } .delta-bad { color: var(--bad); }
.chart { position: relative; }
svg { display: block; width: 100%; height: auto; overflow: visible; }
.grid-line { stroke: var(--line); stroke-width: 1; }
.axis-text { fill: var(--muted); font-size: 10px; font-variant-numeric: tabular-nums; }
.series { fill: none; stroke: var(--series); stroke-width: 2; stroke-linejoin: round; }
.area { fill: var(--series-soft); }
.marker { fill: var(--series); stroke: var(--card); stroke-width: 2; }
.hair { stroke: var(--muted); stroke-width: 1; stroke-dasharray: 2 3; display: none; }
.tip { position: absolute; pointer-events: none; display: none; background: var(--card);
  color: var(--text); border: 1px solid var(--line); border-radius: 6px; padding: 6px 8px;
  font-size: 12px; box-shadow: 0 2px 8px rgba(0,0,0,.12); white-space: nowrap; z-index: 2; }
.tip b { font-size: 14px; font-variant-numeric: tabular-nums; }
.tip .sha { color: var(--text-2); font-family: ui-monospace, Menlo, monospace; }
.empty { color: var(--muted); font-size: 12px; padding: 24px 0; text-align: center; }
table { border-collapse: collapse; width: 100%; font-variant-numeric: tabular-nums; }
th, td { text-align: left; padding: 6px 8px; border-bottom: 1px solid var(--line); }
th { color: var(--text-2); font-weight: 600; font-size: 12px; }
td.num, th.num { text-align: right; }
code { font-family: ui-monospace, Menlo, monospace; font-size: 12px; }
footer { color: var(--muted); font-size: 12px; margin-top: 32px; }
"""

JS = """
const rows = DATA.rows;
function fmt(v) { return v == null ? "n/a" : (v >= 100 ? v.toFixed(0) : v.toFixed(1)); }
function get(obj, path) { for (const k of path) { if (obj == null) return null; obj = obj[k]; } return obj ?? null; }
function tipFor(card) {
  const tip = document.createElement("div"); tip.className = "tip";
  card.querySelector(".chart").appendChild(tip); return tip;
}
document.querySelectorAll(".card[data-path]").forEach(card => {
  const path = JSON.parse(card.dataset.path), corpus = card.dataset.corpus, unit = card.dataset.unit;
  const svg = card.querySelector("svg"); if (!svg) return;
  const hair = svg.querySelector(".hair"), tip = tipFor(card);
  const markers = [...svg.querySelectorAll(".marker")];
  const pts = markers.map(m => ({x: +m.getAttribute("cx"), i: +m.dataset.i}));
  const W = svg.viewBox.baseVal.width;
  svg.addEventListener("pointermove", ev => {
    if (!pts.length) return;
    const rect = svg.getBoundingClientRect(), x = (ev.clientX - rect.left) / rect.width * W;
    let best = pts[0]; for (const p of pts) if (Math.abs(p.x - x) < Math.abs(best.x - x)) best = p;
    const row = rows[best.i], v = get(row.corpora[corpus], path);
    hair.setAttribute("x1", best.x); hair.setAttribute("x2", best.x); hair.style.display = "block";
    markers.forEach(m => m.setAttribute("r", +m.dataset.i === best.i ? 5 : 3.5));
    tip.replaceChildren();
    const b = document.createElement("b"); b.textContent = fmt(v) + " " + unit; tip.appendChild(b);
    const s = document.createElement("div"); s.className = "sha";
    s.textContent = row.short + (row.dirty ? " (dirty)" : "") + " · " + new Date(row.commit_time * 1000).toISOString().slice(0, 16).replace("T", " ");
    tip.appendChild(s);
    const t = document.createElement("div"); t.textContent = row.subject; tip.appendChild(t);
    tip.style.display = "block";
    const left = best.x / W * rect.width; tip.style.left = Math.min(left + 12, rect.width - tip.offsetWidth - 4) + "px"; tip.style.top = "8px";
  });
  svg.addEventListener("pointerleave", () => { hair.style.display = "none"; tip.style.display = "none"; markers.forEach(m => m.setAttribute("r", 3.5)); });
});
const sel = document.getElementById("corpus");
function showCorpus() { document.querySelectorAll("[data-corpus]").forEach(el => { el.hidden = el.dataset.corpus !== sel.value; }); }
sel.addEventListener("change", showCorpus); showCorpus();
"""


def get(obj, path):
    for key in path:
        if not isinstance(obj, dict) or obj.get(key) is None:
            return None
        obj = obj[key]
    return obj


def nice_ticks(vmax):
    if vmax <= 0:
        return [0, 1]
    raw = vmax / 4
    scale = 10 ** (len(str(int(raw))) - 1) if raw >= 1 else 0.1
    for step in (1, 2, 2.5, 5, 10):
        if raw <= step * scale:
            step *= scale
            break
    ticks, v = [], 0.0
    while v < vmax + step * 0.999:
        ticks.append(round(v, 6))
        v += step
    return ticks


def chart_svg(rows, corpus, path, unit):
    values = [get(r["corpora"].get(corpus, {}), path) for r in rows]
    points = [(i, v) for i, v in enumerate(values) if v is not None]
    if not points:
        return '<div class="empty">no data for this metric yet</div>'
    W, H, L, R, T, B = 360, 150, 44, 8, 8, 20
    n = len(rows)
    vmax = max(v for _, v in points)
    ticks = nice_ticks(vmax)
    top = ticks[-1]
    sx = lambda i: L + (i / max(n - 1, 1)) * (W - L - R)  # noqa: E731
    sy = lambda v: T + (1 - v / top) * (H - T - B) if top else H - B  # noqa: E731
    out = [f'<svg viewBox="0 0 {W} {H}" role="img" aria-label="{html.escape(unit)} over commits">']
    for t in ticks:
        y = sy(t)
        out.append(f'<line class="grid-line" x1="{L}" x2="{W - R}" y1="{y:.1f}" y2="{y:.1f}"/>')
        label = f"{t:g}"
        out.append(f'<text class="axis-text" x="{L - 6}" y="{y + 3:.1f}" text-anchor="end">{label}</text>')
    first, last = rows[0], rows[-1]
    out.append(f'<text class="axis-text" x="{L}" y="{H - 4}">{html.escape(first["short"])}</text>')
    out.append(f'<text class="axis-text" x="{W - R}" y="{H - 4}" text-anchor="end">{html.escape(last["short"])}</text>')
    coords = [(sx(i), sy(v)) for i, v in points]
    if len(coords) > 1:
        poly = " ".join(f"{x:.1f},{y:.1f}" for x, y in coords)
        base = H - B
        out.append(f'<polygon class="area" points="{coords[0][0]:.1f},{base} {poly} {coords[-1][0]:.1f},{base}"/>')
        out.append(f'<polyline class="series" points="{poly}"/>')
    for (i, _), (x, y) in zip(points, coords):
        out.append(f'<circle class="marker" data-i="{i}" cx="{x:.1f}" cy="{y:.1f}" r="3.5"/>')
    out.append(f'<line class="hair" x1="0" x2="0" y1="{T}" y2="{H - B}"/>')
    out.append('<rect x="0" y="0" width="100%" height="100%" fill="transparent"/>')
    out.append("</svg>")
    return "".join(out)


def delta_html(values):
    present = [v for v in values if v is not None]
    if len(present) < 2:
        return ""
    now, before = present[-1], present[-2]
    if not before:
        return ""
    pct = (now - before) / before * 100
    cls = "delta-good" if pct < -2 else "delta-bad" if pct > 2 else ""
    return f'<small class="{cls}">{pct:+.0f}% vs previous</small>'


def render(rows, history_path):
    corpora = sorted({name for r in rows for name in r["corpora"]})
    latest = rows[-1]
    parts = [f"<!doctype html><html><head><meta charset='utf-8'>"
             f"<meta name='viewport' content='width=device-width, initial-scale=1'>"
             f"<title>Rock compiler performance</title><style>{CSS}</style></head><body><main>"]
    parts.append("<h1>Rock compiler performance</h1>")
    parts.append(f"<p class='sub'>{len(rows)} measured commits, latest "
                 f"<code>{html.escape(latest['short'])}</code> {html.escape(latest['subject'])}. "
                 f"Means of repeated runs; cold builds from hyperfine, the rest in-process.</p>")
    parts.append("<div class='controls'><label for='corpus'>Corpus</label><select id='corpus'>")
    for c in corpora:
        parts.append(f"<option value='{html.escape(c)}'>{html.escape(c)}</option>")
    parts.append("</select></div>")
    for group, metrics in GROUPS:
        parts.append(f"<h2>{html.escape(group)}</h2>")
        for corpus in corpora:
            parts.append(f"<div class='grid' data-corpus='{html.escape(corpus)}'>")
            for title, unit, path in metrics:
                values = [get(r["corpora"].get(corpus, {}), path) for r in rows]
                last = next((v for v in reversed(values) if v is not None), None)
                shown = "n/a" if last is None else (f"{last:.0f}" if last >= 100 else f"{last:.1f}")
                parts.append(
                    f"<div class='card' data-corpus='{html.escape(corpus)}' data-path='{html.escape(json.dumps(path))}' "
                    f"data-unit='{unit}'><h3>{html.escape(title)}</h3>"
                    f"<p class='latest'>{shown} <small>{unit}</small>{delta_html(values)}</p>"
                    f"<div class='chart'>{chart_svg(rows, corpus, path, unit)}</div></div>")
            parts.append("</div>")
    parts.append("<h2>Latest values</h2>")
    for corpus in corpora:
        parts.append(f"<div data-corpus='{html.escape(corpus)}'><table><thead><tr><th>Metric</th>"
                     "<th class='num'>Latest</th><th class='num'>Previous</th><th class='num'>Best</th></tr></thead><tbody>")
        for group, metrics in GROUPS:
            for title, unit, path in metrics:
                values = [get(r["corpora"].get(corpus, {}), path) for r in rows]
                present = [v for v in values if v is not None]
                cells = [present[-1] if present else None,
                         present[-2] if len(present) > 1 else None,
                         min(present) if present else None]
                tds = "".join(f"<td class='num'>{'n/a' if v is None else f'{v:.1f}'}</td>" for v in cells)
                parts.append(f"<tr><td>{html.escape(group)}: {html.escape(title)} ({unit})</td>{tds}</tr>")
        parts.append("</tbody></table></div>")
    parts.append(f"<footer>Generated {time.strftime('%Y-%m-%d %H:%M')} from "
                 f"{html.escape(history_path.name)} by perf/report.py.</footer>")
    data = dict(rows=[dict(short=r["short"], subject=r["subject"], commit_time=r["commit_time"],
                           dirty=r.get("dirty", False), corpora=r["corpora"]) for r in rows])
    parts.append(f"<script>const DATA = {json.dumps(data).replace('</', '<\\/')};{JS}</script>")
    parts.append("</main></body></html>")
    return "".join(parts)


def load_rows(path, last):
    if not path.exists():
        sys.exit(f"no history at {path}; run perf/run.py first")
    rows = [json.loads(line) for line in path.read_text().splitlines() if line.strip()]
    # One point per commit: the latest measurement of each sha, in commit order.
    by_sha = {}
    for row in rows:
        by_sha[row["sha"]] = row
    rows = sorted(by_sha.values(), key=lambda r: (r["commit_time"], r["measured_at"]))
    return rows[-last:] if last else rows


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--history", type=Path, default=default_history())
    parser.add_argument("--out", type=Path, default=Path(__file__).resolve().parent / "report.html")
    parser.add_argument("--last", type=int, default=0, help="only the last N commits")
    args = parser.parse_args()
    rows = load_rows(args.history, args.last)
    args.out.write_text(render(rows, args.history))
    print(f"wrote {args.out} ({len(rows)} commits)")


if __name__ == "__main__":
    main()
