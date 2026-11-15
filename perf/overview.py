"""Render a reviewed subsystem map over the measured source inventory."""
from collections import Counter
from html import escape as esc
import json
import os
from pathlib import Path
from urllib.parse import quote

import inventory


def owner(path, units):
    matches = [unit["id"] for unit in units
               if any(path == p or path.startswith(p + "/") for p in unit["paths"])]
    if len(matches) > 1:
        raise ValueError(f"multiple owners for {path}: {matches}")
    return matches[0] if matches else "unmapped"


def validate(root, units):
    if len({u["id"] for u in units}) != len(units):
        raise ValueError("duplicate subsystem id")
    for unit in units:
        for path in unit["paths"] + unit["tests"] + unit["docs"]:
            if not (root / path).exists():
                raise ValueError(f"missing map path: {path}")
        for ref in unit["sources"]:
            path = root / ref["path"]
            if not path.is_file() or ref["anchor"] not in path.read_text():
                raise ValueError(f"stale anchor: {ref}")


def check_status(claim, results, revision, dirty):
    row = next((r for r in results.get("results", []) if r["id"] == claim), None)
    if row is None:
        return "not run"
    if dirty or results.get("dirty") or results.get("revision") != revision:
        return "stale result"
    return "passed" if row["passed"] else "FAILED"


def build(root, destination):
    catalog = json.loads((root / "docs/subsystems.json").read_text())
    units = catalog["subsystems"]
    validate(root, units)
    files, missing = inventory.collect(root)
    for row in files:
        row["subsystem"] = owner(row["path"], units)
    revision = inventory.git(root, "rev-parse", "HEAD").strip()
    dirty = bool(inventory.git(root, "status", "--porcelain").strip())
    workspace = inventory.workspace(root)
    ledger = json.loads((root / "docs/evidence.json").read_text())
    result_path = destination / "evidence/results.json"
    results = json.loads(result_path.read_text()) if result_path.exists() else {}
    project = [r for r in files if r["origin"] == "project"]
    measured = inventory.grouped(project, "subsystem")
    # Dependencies are declared normal Cargo edges, not a runtime call graph.
    edges = Counter()
    for package, deps in workspace["local_dependencies"].items():
        source = owner("crates/" + package + "/Cargo.toml", units)
        if source == "unmapped":
            source = owner(package + "/Cargo.toml", units)
        for dep in deps:
            target = owner("crates/" + dep + "/Cargo.toml", units)
            if target == "unmapped":
                target = owner(dep + "/Cargo.toml", units)
            if source != target:
                edges[source, target] += 1

    def link(path, label=None):
        relative = os.path.relpath(root / path, destination)
        return f'<a href="{esc(quote(relative))}">{esc(label or path)}</a>'

    def listing(paths):
        return "<br>".join(link(p) for p in paths) or "None mapped"

    def table(headers, rows):
        return '<div class="scroll"><table><thead><tr>' + ''.join(f'<th>{esc(h)}</th>' for h in headers) + '</tr></thead><tbody>' + ''.join(
            '<tr>' + ''.join(f'<td>{cell}</td>' for cell in row) + '</tr>' for row in rows) + '</tbody></table></div>'

    parts = ['''<!doctype html><html lang="en"><meta charset="utf-8">
<meta name="viewport" content="width=device-width"><title>Rock subsystem map</title>
<style>
:root{color-scheme:dark}body{font:16px/1.5 system-ui;max-width:1200px;margin:36px auto;padding:0 20px;background:#15181d;color:#dce1e8}a{color:#8ac5ff}h1,h2,h3{color:#f0f3f7}p{max-width:90ch}table{border-collapse:collapse;width:100%;margin:20px 0}td,th{text-align:left;vertical-align:top;padding:9px;border-bottom:1px solid #343b46}th{background:#20252d}code{overflow-wrap:anywhere}summary{cursor:pointer;font-weight:650}details{padding:16px 0;border-bottom:1px solid #343b46}.scroll{overflow-x:auto}input{font:inherit;padding:8px;width:min(90%,500px);background:#20252d;color:inherit;border:1px solid #596473}meter{width:110px;margin-right:10px}small{color:#abb6c6}nav{display:flex;gap:20px;flex-wrap:wrap}
</style><h1>Rock subsystem map</h1>
<nav><a href="#size">Where the code is</a><a href="#map">Responsibilities and checks</a><a href="#queue">Cleanup queue</a><a href="#edges">Dependencies</a><a href="index.html">Detailed inventory</a></nav>''',
             f'<p>Revision <code>{revision}</code>; working tree {"dirty" if dirty else "clean"}. Review date {esc(catalog["reviewed"])}. <a href="overview.json">Measured map JSON</a>.</p>',
             '<p><b>Measured:</b> tracked source size and declared Cargo edges. <b>Reviewed:</b> responsibility and entrypoint summaries from a bounded source read. <b>Checked:</b> only the linked executable claims, at their recorded revision. Everything else remains unverified; this map is not a coverage score.</p>',
             '<p>Core flow: source &rarr; syntax &rarr; lowered modules &rarr; inference &rarr; semantic queries &rarr; instance graph &rarr; MIR &rarr; backend output. Incremental updates reuse stages; CLI, editor and browser choose different entrypoints. The groups below are work boundaries, not independent layers.</p>',
             '<h2 id="size">Where the code is</h2><p>Project source only; upstream-derived code and prebuilt bindings are excluded. Physical lines include blanks and comments. Test-file lines do not include inline Rust tests. Markers are textual occurrences, not executed tests or feature coverage. Cross-cutting test corpora have their own row, so totals are not counted twice.</p>']
    names = {u["id"]: u["name"] for u in units}
    total = sum(v["lines"] for v in measured.values())
    parts.append(table(["Subsystem", "Physical lines", "Test-file lines", "Test markers", "Unpinned / rejection cases"], [
        [f'<a href="#unit-{esc(key)}">{esc(names.get(key, key))}</a>',
         f'<meter min="0" max="{total}" value="{v["lines"]}"></meter>{v["lines"]:,}',
         f'{v["test_file_lines"]:,}', str(v["test_markers"]), f'{v["unpinned"]} / {v["error_cases"]}']
        for key, v in measured.items()]))
    corpus = inventory.grouped([r for r in project if r["subsystem"] == "corpus"], "group")
    parts.append('<details><summary>Shared test corpus by topic directory</summary><p>Location counts only. One case can exercise several subsystems; these are not coverage percentages.</p>')
    parts.append(table(["Directory", "Files", "Runtime markers", "Rejection cases", "Unpinned"], [
        [link(key), str(v["files"]), str(v["test_markers"]), str(v["error_cases"]), str(v["unpinned"])]
        for key, v in corpus.items()]))
    parts.append('</details>')
    parts.append('<h2 id="map">Responsibilities and checks</h2><p>Commands below are entrypoints for future work; listing a command does not mean it was run in this survey. Expand a subsystem for source links, current bounded checks and unknowns.</p><input id="filter" aria-label="Filter subsystems" placeholder="Filter: inference, Lua, editor...">')
    for unit in units:
        claims = [c for c in ledger if any(owner(r["path"], units) == unit["id"] for r in c["sources"])]
        own = [r for r in project if r["subsystem"] == unit["id"]]
        largest = sorted(own, key=lambda r: (-r["lines"], r["path"]))[:3]
        parts.extend([f'<details id="unit-{esc(unit["id"])}"><summary>{esc(unit["name"])} &mdash; {esc(unit["responsibility"])}</summary>',
                      '<p><b>Owned paths:</b> ' + listing(unit["paths"]) + '</p>',
                      '<p><b>Reviewed entrypoints:</b><br>' + '<br>'.join(link(r["path"]) + ' &middot; <code>' + esc(r["anchor"]) + '</code>' for r in unit["sources"]) + '</p>',
                      '<p><b>Test locations:</b><br>' + listing(unit["tests"]) + '</p>',
                      '<pre>' + esc('\n'.join(unit["commands"])) + '</pre>',
                      '<p><b>Documentation:</b> ' + listing(unit["docs"]) + '</p>',
                      '<p><b>Unknown / unverified:</b> ' + esc(unit["unknown"]) + '</p>',
                      '<p><b>Largest files:</b><br>' + '<br>'.join(link(r["path"]) + f' ({r["lines"]:,} lines; {esc(r["role"])})' for r in largest) + '</p>'])
        parts.append('<p><b>Executable claims touching this subsystem:</b><br>' + ('<br>'.join(
            esc(c["id"]) + ': <b>' + esc(check_status(c["id"], results, revision, dirty)) + '</b> &mdash; ' + esc(c["limit"])
            for c in claims) if claims else 'None in the current small ledger. This does not mean there are no tests.') + '</p></details>')
    parts.append('<h2 id="queue">Cleanup queue</h2><p>Ordered by reproduced impact and clarity of the acceptance check. Candidates are proposals, not established defects. Parallel lanes describe file ownership; shared test builds and final integration still need coordination.</p>')
    parts.append(table(["Priority / status", "Task and reason", "Owned scope / conflicts", "Acceptance check"], [
        [esc(q["status"]), esc(q["title"]) + '<br><small>' + esc(q["reason"]) + '</small>',
         listing(q["paths"]) + '<br>' + esc(q["conflicts"]), esc(q["acceptance"])] for q in catalog["queue"]]))
    parts.append('<h2 id="edges">Declared dependencies between groups</h2><p>Counts are package-to-package normal dependency declarations, including target-specific entries, excluding dev/build dependencies. They are navigation hints, not measured coupling. Internal group edges are omitted.</p>')
    parts.append(table(["From", "To", "Declared edges"], [[esc(names.get(a, a)), esc(names.get(b, b)), str(n)] for (a, b), n in sorted(edges.items())]))
    unmapped = [r["path"] for r in files if r["subsystem"] == "unmapped"]
    parts.append('<h2 id="unit-unmapped">Unmapped source</h2><p>New or intentionally unassigned paths stay visible here.</p>' + (listing(unmapped) if unmapped else '<p>None.</p>'))
    parts.append('''<script>document.querySelector('#filter').addEventListener('input',e=>{const q=e.target.value.toLowerCase();document.querySelectorAll('details[id]').forEach(d=>{d.hidden=!d.textContent.toLowerCase().includes(q);d.open=!!q&&!d.hidden})});</script></html>''')
    return {"revision": revision, "dirty": dirty, "reviewed": catalog["reviewed"],
            "measurements": measured, "unmapped": unmapped, "missing_tracked_files": missing,
            "subsystems": units, "queue": catalog["queue"]}, '\n'.join(parts)


def main():
    root = Path(__file__).resolve().parent.parent
    destination = root / "target/inventory"
    destination.mkdir(parents=True, exist_ok=True)
    data, html = build(root, destination)
    (destination / "overview.json").write_text(json.dumps(data, indent=2) + '\n')
    (destination / "overview.html").write_text(html)
    print(f'{len(data["subsystems"])} subsystems; {len(data["unmapped"])} unmapped source files')
    print("target/inventory/overview.html")


if __name__ == "__main__":
    main()
