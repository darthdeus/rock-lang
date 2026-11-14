"""Compare emitted guards with a valid-input-only Lua ablation.

Usage: python3 benchmarks/guard_proofs.py emitted.lua
The exact replacements deliberately fail closed if code generation changes.
"""
import json
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile

source = Path(sys.argv[1]).read_text()
replacements = {
    '__vec_checked_set(r0, c0, c0)': 'r0._data[c0 + 1] = c0',
    'c0._data[c1 + 1] or __vec_checked_get(c0, c1)': 'c0._data[c1 + 1]',
    '__nb_checked_get(c0, ((c3 * (4)) + c4))': 'c0.ptr[c3 * 4 + c4]',
    '__nb_checked_set(r1, (0), (2))': 'r1.ptr[0] = 2',
    '__nb_checked_set(r1, (1), (3))': 'r1.ptr[1] = 3',
}
unchecked = source
for before, after in replacements.items():
    if unchecked.count(before) != 1:
        raise SystemExit(f'Expected exactly one occurrence: {before}')
    unchecked = unchecked.replace(before, after)

rows = []
runner = Path(__file__).with_suffix('.lua').resolve()
with tempfile.TemporaryDirectory(prefix='rock-guard-audit-') as directory:
    paths = {side: Path(directory) / f'{side}.lua' for side in ('checked', 'unchecked')}
    paths['checked'].write_text(source)
    paths['unchecked'].write_text(unchecked)
    for round_number in range(6):
        order = ('checked', 'unchecked') if round_number % 2 == 0 else ('unchecked', 'checked')
        for side in order:
            output = subprocess.check_output(['luajit', str(runner), str(paths[side])], text=True)
            for line in output.splitlines():
                name, ns, sink, aborts = line.split()
                rows.append(dict(round=round_number, side=side, name=name,
                                 ns=float(ns), sink=sink, aborts=int(aborts)))
for name in sorted({row['name'] for row in rows}):
    matching = [row for row in rows if row['name'] == name]
    if len({row['sink'] for row in matching}) != 1:
        raise SystemExit(f'Sink mismatch: {name}')
    for side in ('checked', 'unchecked'):
        selected = [row for row in matching if row['side'] == side]
        values = [row['ns'] for row in selected]
        print(name, side, 'median', statistics.median(values), 'range', min(values), max(values),
              'aborts', [row['aborts'] for row in selected], file=sys.stderr)
print(json.dumps(rows, indent=2))
