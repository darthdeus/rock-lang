"""Tracked working-tree source inventory; static counts, not coverage or a score."""
import argparse
from collections import defaultdict
from html import escape
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib
from urllib.parse import quote

EXTENSIONS = {".rs", ".rock", ".py", ".js", ".ts", ".tsx", ".c", ".h", ".lua", ".glsl", ".svelte"}
UPSTREAM = {"la-arena", "lsp-server-manual", "lsp-types-manual", "test-utils"}


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args]).decode()


def measure(path, source):
    parts = Path(path).parts
    crate = parts[1] if parts[0] == "crates" and len(parts) > 2 else None
    group = "/".join(parts[:2]) if parts[0] in {"crates", "tests", "packages", "demos", "examples"} and len(parts) > 2 else parts[0]
    is_fixture = "tests" in parts or path.endswith((".rock.err", ".rock.lsp"))
    role = "test/fixture files" if is_fixture else "source (may contain inline tests)"
    if parts[0] in {"examples", "demos", "benchmarks"}:
        role = "examples/demos/benchmarks"
    if parts[0] in {"scripts", "perf"}:
        role = "tooling"
    markers = len(re.findall(r"^\s*#\[test\]", source, re.M)) if path.endswith(".rs") else 0
    if path.endswith(".rock"):
        markers = len(re.findall(r"^\s*@test\b", source, re.M))
    cases = []
    if path.endswith(".rock.err"):
        current = None
        for line in source.splitlines():
            line = line.strip()
            if line.startswith("//==="):
                current = None
                if line.startswith("//=== TEST CASE:") and line.endswith(" ==="):
                    current = {"name": line[len("//=== TEST CASE:"):-4].strip(), "pinned": False}
                    cases.append(current)
            elif current is not None and line.startswith("//! expect:"):
                current["pinned"] |= bool(line[len("//! expect:"):].strip())
    unpinned = [case["name"] for case in cases if not case["pinned"]]
    origin = "upstream-derived" if crate in UPSTREAM else "project"
    if path.startswith("packages/stone/prebuilt/"):
        origin = "prebuilt bindings"
    return {"path": path, "group": group, "role": role,
            "origin": origin,
            "language": ".rock.err" if path.endswith(".rock.err") else Path(path).suffix,
            "lines": len(source.splitlines()),
            "nonblank_lines": sum(bool(line.strip()) for line in source.splitlines()),
            "test_markers": markers, "error_cases": len(cases),
            "unpinned_errors": unpinned}


def collect(root):
    rows, missing = [], []
    for path in sorted(git(root, "ls-files", "-z").split("\0")):
        if not path or not (Path(path).suffix in EXTENSIONS or path.endswith((".rock.err", ".rock.lsp"))):
            continue
        file = root / path
        if not file.exists():
            missing.append(path)
        elif not file.is_symlink():
            rows.append(measure(path, file.read_text()))
    return rows, missing


def workspace(root):
    config = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]
    # Expand member globs so adding a crate does not require editing this script.
    members = sorted({p.relative_to(root).as_posix() for pattern in config["members"]
                      for p in root.glob(pattern)} - set(config.get("exclude", [])))
    defaults = sorted({p.relative_to(root).as_posix() for pattern in config.get("default-members", members)
                       for p in root.glob(pattern)})
    manifests = {member: tomllib.loads((root / member / "Cargo.toml").read_text()) for member in members}
    names = {value["package"]["name"] for value in manifests.values()}
    edges = {}
    for member, manifest in manifests.items():
        dependencies = dict(manifest.get("dependencies", {}))
        for target in manifest.get("target", {}).values():
            dependencies.update(target.get("dependencies", {}))
        local = []
        for alias, definition in dependencies.items():
            if isinstance(definition, dict) and definition.get("workspace"):
                definition = config.get("dependencies", {}).get(alias, definition)
            package = definition.get("package", alias) if isinstance(definition, dict) else alias
            if package in names:
                local.append(package)
        edges[manifest["package"]["name"]] = sorted(set(local))
    return {"members": members, "default_members": defaults,
            "excluded_from_default": sorted(set(members) - set(defaults)),
            "local_dependencies": edges}


def grouped(rows, key):
    groups = defaultdict(lambda: {"files": 0, "lines": 0, "test_file_lines": 0,
                                  "test_markers": 0, "error_cases": 0, "unpinned": 0})
    for row in rows:
        group = groups[row[key]]
        group["files"] += 1
        group["lines"] += row["lines"]
        group["test_file_lines"] += row["lines"] if row["role"] == "test/fixture files" else 0
        group["test_markers"] += row["test_markers"]
        group["error_cases"] += row["error_cases"]
        group["unpinned"] += len(row["unpinned_errors"])
    return dict(sorted(groups.items(), key=lambda item: (-item[1]["lines"], item[0])))


def report(data, root, destination):
    def link(path):
        relative = os.path.relpath(root / path, destination.parent)
        return f'<a href="{escape(quote(relative))}">{escape(path)}</a>'

    def table(headers, rows):
        return '<table><thead><tr>' + ''.join(f'<th>{escape(h)}</th>' for h in headers) + '</tr></thead><tbody>' + ''.join(
            '<tr>' + ''.join(f'<td>{cell}</td>' for cell in row) + '</tr>' for row in rows) + '</tbody></table>'

    parts = ['<!doctype html><html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width"><title>Rock source inventory</title>',
             '<style>:root{color-scheme:dark}body{font:16px/1.5 system-ui;max-width:1200px;margin:40px auto;padding:0 20px;color:#dce1e8;background:#15181d}h1,h2,th{color:#f0f3f7}table{border-collapse:collapse;width:100%;margin-bottom:28px}th,td{text-align:left;padding:6px 10px;border-bottom:1px solid #343b46}th{vertical-align:bottom;background:#20252d}tbody tr:hover{background:#20252d}a{color:#8ac5ff}a:visited{color:#c5b0f3}code{overflow-wrap:anywhere}td:first-child{overflow-wrap:anywhere}p{max-width:85ch}</style>',
             '<h1>Rock source inventory</h1>',
             f'<p>Revision <code>{escape(data["revision"])}</code>; working tree {"dirty" if data["dirty"] else "clean"}. Counts read tracked files from the working tree. JSON: <a href="inventory.json">full inventory</a>.</p>',
             '<p>Physical lines include comments and blanks. Test-file lines exclude inline tests in source files. Test markers are textual <code>#[test]</code> / <code>@test</code> occurrences, not executed tests: embedded snippets can inflate them; custom harnesses and macro-generated tests can be missed. Corpus directories are locations, not feature coverage. No quality score.</p>']
    parts.append('<p>Claim definitions and interpretation: ' + link('docs/evidence.md') + '. Rerun with <code>python3 perf/check_evidence.py</code>.</p>')
    if (destination.parent / 'evidence/results.json').exists():
        parts.append('<p><a href="evidence/results.json">Last evidence-check results and log filenames</a>. These record their own revision; they are not a guarantee about later edits.</p>')
    for key, heading in [("origin", "Project and upstream-derived code"), ("role", "File roles"), ("language", "Languages"), ("group", "Subsystems and corpus directories")]:
        parts.append(f'<h2>{heading}</h2>')
        parts.append(table(["Group", "Files", "Lines", "Test-file lines", "Test markers", "Error cases", "Unpinned errors"],
                           [[escape(name), *[str(v[field]) for field in ["files", "lines", "test_file_lines", "test_markers", "error_cases", "unpinned"]]]
                            for name, v in grouped(data["files"], key).items()]))
    parts.append('<h2>Largest project files</h2><p>Investigation candidates, not automatic refactoring targets. Upstream-derived files and prebuilt bindings are excluded here; see JSON for every file.</p>')
    largest = sorted((r for r in data["files"] if r["origin"] == "project"), key=lambda r: (-r["lines"], r["path"]))[:40]
    parts.append(table(["File", "Role", "Lines", "Test markers"], [[link(r["path"]), escape(r["role"]), str(r["lines"]), str(r["test_markers"])] for r in largest]))
    parts.append('<h2>Tests that check rejection without checking the reason</h2><p>A rejection case is a deliberately invalid Rock program: the test passes when the compiler refuses it. Each <code>//=== TEST CASE:</code> section is one case; a file can contain several.</p><p>The cases below have no nonempty <code>//! expect:</code> diagnostic assertion. They check that compilation fails, but an unrelated error can also make them pass. For example, a test meant to reject an unsupported operator overload could pass because of an unrelated syntax error. A case-name suffix such as <code>TypeMismatch</code> does not assert the diagnostic.</p><p>This count is a test-strength review queue, not a count of compiler bugs or failing tests. Adding an expected diagnostic would check that each program is rejected for the intended reason.</p>')
    parts.append(table(["File", "Case"], [[link(r["path"]), escape(case)] for r in data["files"] for case in r["unpinned_errors"]]))
    parts.append('<h2>Workspace selection</h2><p>Excluded from plain Cargo build/test by the explicit default-member selection:</p><ul>' + ''.join(f'<li>{link(p + "/Cargo.toml")}</li>' for p in data["workspace"]["excluded_from_default"]) + '</ul>')
    parts.append('<h2>Declared local dependencies</h2><p>Normal dependencies including target-specific declarations; excludes dev/build dependencies. A declared edge does not establish actual coupling or architectural correctness.</p>')
    parts.append(table(["Package", "Local dependencies"], [[escape(name), escape(", ".join(deps))] for name, deps in sorted(data["workspace"]["local_dependencies"].items())]))
    if data["missing_tracked_files"]:
        parts.append('<h2>Missing tracked files (excluded from totals)</h2><pre>' + escape('\n'.join(data["missing_tracked_files"])) + '</pre>')
    parts.append('</html>')
    return '\n'.join(parts)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=Path("target/inventory"))
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    rows, missing = collect(root)
    data = {"schema_version": 1, "revision": git(root, "rev-parse", "HEAD").strip(),
            "dirty": bool(git(root, "status", "--porcelain").strip()),
            "files": rows, "missing_tracked_files": missing, "workspace": workspace(root)}
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "inventory.json").write_text(json.dumps(data, indent=2) + "\n")
    (args.out / "index.html").write_text(report(data, root, args.out.resolve() / "index.html"))
    print(f'{len(rows)} files; {sum(r["lines"] for r in rows):,} physical lines')
    print(args.out / "index.html")


if __name__ == "__main__":
    main()
