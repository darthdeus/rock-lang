#!/usr/bin/env python3
"""Compile the same fixture twice and run alternating fresh-process pairs."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import statistics
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
SOURCE = HERE / 'fixture.rock'
DRIVER = HERE / 'driver.lua'
SIDES = ('baseline', 'candidate')
MODES = ('local', 'match', 'direct')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def prepare(directory, compilers):
    directory.mkdir(parents=True, exist_ok=False)
    shutil.copyfile(SOURCE, directory / 'fixture.rock')
    shutil.copyfile(DRIVER, directory / 'driver.lua')
    manifest = {'source_sha256': digest(SOURCE), 'driver_sha256': digest(DRIVER),
                'compilers': {}}
    for name, compiler in zip(SIDES, compilers):
        output = directory / (name + '.lua')
        subprocess.run([str(compiler.resolve()), 'compile', str(SOURCE), '--output', str(output), '--quiet'], check=True)
        manifest['compilers'][name] = {'name': compiler.name, 'sha256': digest(compiler),
                                       'output_sha256': digest(output)}
    (directory / 'build.json').write_text(json.dumps(manifest, indent=2) + '\n')


def measure(directory, luajit, pairs):
    manifest = json.loads((directory / 'build.json').read_text())
    for file, expected in [('fixture.rock', manifest['source_sha256']),
                           ('driver.lua', manifest['driver_sha256'])]:
        if digest(directory / file) != expected:
            raise ValueError(f'hash mismatch: {file}')
    for side in SIDES:
        if digest(directory / (side + '.lua')) != manifest['compilers'][side]['output_sha256']:
            raise ValueError(f'hash mismatch: {side}.lua')
    report = dict(manifest)
    report['luajit_version'] = subprocess.check_output([luajit, '-v'], stderr=subprocess.STDOUT, text=True).strip()
    report['samples'] = []
    for mode in MODES:
        for metric in ('time', 'heap', 'trace'):
            # Trace observations are also fresh processes, never attached to timing runs.
            for pair in range(pairs if metric != 'trace' else 1):
                order = SIDES if pair % 2 == 0 else tuple(reversed(SIDES))
                for side in order:
                    out = subprocess.check_output([luajit, str(directory / 'driver.lua'), str(directory / (side + '.lua')), mode, metric], text=True)
                    fields = out.strip().split()
                    if len(fields) != 8 or fields[:2] != [mode, metric]:
                        raise ValueError(f'unexpected driver output: {out!r}')
                    report['samples'].append(dict(mode=mode, metric=metric, pair=pair, side=side,
                        ns_per_call=float(fields[2]), heap_bytes_per_call=float(fields[3]),
                        sink=int(fields[4]), trace_starts=int(fields[5]),
                        trace_stops=int(fields[6]), trace_aborts=int(fields[7])))
    if len({r['sink'] for r in report['samples']}) != 1:
        raise ValueError('sink mismatch')
    report['medians'] = {}
    for mode in MODES:
        for metric, field in [('time', 'ns_per_call'), ('heap', 'heap_bytes_per_call')]:
            for side in SIDES:
                values = [r[field] for r in report['samples'] if r['mode'] == mode and r['metric'] == metric and r['side'] == side]
                report['medians'][f'{mode}.{metric}.{side}'] = statistics.median(values)
    return report


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('compilers', type=Path, nargs='*', metavar='COMPILER', help='baseline then candidate compiler')
    group = parser.add_mutually_exclusive_group()
    group.add_argument('--prepare-only', type=Path, metavar='DIR')
    group.add_argument('--artifacts', type=Path, metavar='DIR')
    parser.add_argument('--luajit', default='luajit')
    parser.add_argument('--pairs', type=int, default=6)
    args = parser.parse_args()
    if args.pairs < 1:
        parser.error('--pairs must be positive')
    if args.artifacts:
        if args.compilers:
            parser.error('--artifacts does not take compilers')
        report = measure(args.artifacts.resolve(), args.luajit, args.pairs)
    else:
        if len(args.compilers) != 2:
            parser.error('supply baseline and candidate compilers')
        if args.prepare_only:
            prepare(args.prepare_only, args.compilers)
            return
        with tempfile.TemporaryDirectory(prefix='rock-call-boundaries-') as tmp:
            directory = Path(tmp) / 'artifacts'
            prepare(directory, args.compilers)
            report = measure(directory, args.luajit, args.pairs)
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
