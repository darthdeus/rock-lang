#!/usr/bin/env python3
"""Compile the same fixture twice and run alternating fresh-process pairs."""
import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
SOURCE = HERE / 'fixture.rock'
DRIVER = HERE / 'driver.lua'


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('baseline', type=Path, help='baseline compiler executable')
    parser.add_argument('candidate', type=Path, help='candidate compiler executable')
    parser.add_argument('--luajit', default='luajit')
    parser.add_argument('--pairs', type=int, default=6)
    args = parser.parse_args()
    if args.pairs < 1:
        parser.error('--pairs must be positive')
    compilers = {name: path.resolve() for name, path in
                 [('baseline', args.baseline), ('candidate', args.candidate)]}
    report = {'source_sha256': digest(SOURCE), 'driver_sha256': digest(DRIVER),
              'luajit_version': subprocess.check_output([args.luajit, '-v'],
                                                         stderr=subprocess.STDOUT, text=True).strip(),
              'compilers': {}, 'samples': []}
    with tempfile.TemporaryDirectory(prefix='rock-call-boundaries-') as tmp:
        outputs = {}
        for name, compiler in compilers.items():
            output = Path(tmp) / (name + '.lua')
            subprocess.run([str(compiler), 'compile', str(SOURCE), '--output', str(output), '--quiet'], check=True)
            outputs[name] = output
            report['compilers'][name] = {'path': str(compiler), 'sha256': digest(compiler),
                                         'output_sha256': digest(output)}
        for mode in ('local', 'direct'):
            for metric in ('time', 'heap', 'trace'):
                # Trace observations are also fresh processes, never attached to timing runs.
                for pair in range(args.pairs if metric != 'trace' else 1):
                    order = ('baseline', 'candidate') if pair % 2 == 0 else ('candidate', 'baseline')
                    for side in order:
                        out = subprocess.check_output([args.luajit, str(DRIVER), str(outputs[side]), mode, metric], text=True)
                        fields = out.strip().split()
                        if len(fields) != 8 or fields[:2] != [mode, metric]:
                            raise ValueError(f'unexpected driver output: {out!r}')
                        report['samples'].append(dict(mode=mode, metric=metric, pair=pair, side=side,
                            ns_per_call=float(fields[2]), heap_bytes_per_call=float(fields[3]),
                            sink=int(fields[4]), trace_starts=int(fields[5]),
                            trace_stops=int(fields[6]), trace_aborts=int(fields[7])))
    assert len({r['sink'] for r in report['samples']}) == 1, 'sink mismatch'
    report['medians'] = {}
    for mode in ('local', 'direct'):
        for metric, field in [('time', 'ns_per_call'), ('heap', 'heap_bytes_per_call')]:
            for side in compilers:
                values = [r[field] for r in report['samples'] if r['mode'] == mode and r['metric'] == metric and r['side'] == side]
                report['medians'][f'{mode}.{metric}.{side}'] = statistics.median(values)
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
