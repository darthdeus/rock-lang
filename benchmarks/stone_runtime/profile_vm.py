"""Run separate, alternating LuaJIT VM-state sampling processes on emitted workloads."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

p = argparse.ArgumentParser()
p.add_argument('baseline', type=Path, help='baseline artifact directory')
p.add_argument('candidate', type=Path, help='candidate artifact directory')
p.add_argument('--runtime', type=Path, required=True)
p.add_argument('--stone-lib', type=Path, required=True)
p.add_argument('--outdir', type=Path, required=True)
p.add_argument('--case', choices=('mesh', 'segment'), default='mesh')
p.add_argument('--frames', type=int, default=2000)
p.add_argument('--warmup', type=int, default=30)
p.add_argument('--rounds', type=int, default=4)
p.add_argument('--interval-ms', type=int, default=1)
args = p.parse_args()
if min(args.frames, args.rounds, args.interval_ms) < 1 or args.warmup < 0:
    p.error('frames, rounds and interval must be positive; warmup must be nonnegative')
runtime = args.runtime.resolve()
out = args.outdir.resolve()
out.mkdir(parents=True, exist_ok=True)
env = dict(os.environ)
env.update(ROCK_STONE_LIB=str(args.stone_lib.resolve()), STONE_HEADLESS='1',
           LUA_PATH=f'{runtime}/share/luajit-2.1/?.lua;{runtime}/share/lua/5.1/?.lua;;',
           LUA_CPATH=f'{runtime}/lib/lua/5.1/?.so;;', ROCK_TEST_SEED='42',
           AUDIT_CASE=args.case, AUDIT_CORNERS='0', AUDIT_INDEPENDENT_CORNERS='1',
           AUDIT_FRAMES=str(args.frames), AUDIT_WARMUP=str(args.warmup),
           AUDIT_METRIC='time', AUDIT_ARM='growth')
env['LD_LIBRARY_PATH'] = ':'.join(filter(None, [str(args.stone_lib.resolve().parent),
    str(runtime / 'lib'), env.get('LD_LIBRARY_PATH'), env.get('NIX_LD_LIBRARY_PATH')]))
start = '    for i = 1, frames do\n        local start = stone.stone_get_time()'
end = '    local bytes = (stone.stone_lua_alloc_gross_request_kb() - start_bytes) * 1024'
profile_start = '''    local profile = require('jit.profile')
    local states = {N=0, I=0, C=0, G=0, J=0}
    profile.start('iINTERVAL', function(_, samples, state)
        states[state] = (states[state] or 0) + samples
    end)
'''.replace('INTERVAL', str(args.interval_ms))
profile_stop = '''    profile.stop()
    io.write(string.format('VMSTATES N=%d I=%d C=%d G=%d J=%d\\n',
        states.N, states.I, states.C, states.G, states.J))
'''
artifacts = {}
for name, directory in (('baseline', args.baseline), ('candidate', args.candidate)):
    source = (directory / 'retained.lua').read_bytes()
    text = source.decode('utf-8')
    if text.count(start) != 1 or text.count(end) != 1:
        raise RuntimeError(f'{name}: expected exactly one measurement loop')
    text = text.replace(start, profile_start + start).replace(end, profile_stop + end)
    instrumented = out / f'{name}-profile.lua'
    instrumented.write_text(text)
    artifacts[name] = dict(source_sha256=hashlib.sha256(source).hexdigest(),
        instrumented_sha256=hashlib.sha256(instrumented.read_bytes()).hexdigest(),
        build=json.loads((directory / 'build.json').read_text()))
rows = []
for pair in range(args.rounds):
    for name in (('baseline', 'candidate') if pair % 2 == 0 else ('candidate', 'baseline')):
        result = subprocess.run([str(runtime / 'bin/luajit'), str(out / f'{name}-profile.lua')],
                                env=env, text=True, capture_output=True, timeout=300)
        log = out / f'{pair}-{name}.log'
        log.write_text(result.stdout + result.stderr)
        match = re.search(r'^VMSTATES N=(\d+) I=(\d+) C=(\d+) G=(\d+) J=(\d+)$', result.stdout, re.M)
        if result.returncode or f'{args.case.upper()}_OK' not in result.stdout or not match:
            raise RuntimeError(f'Failed {log}: exit {result.returncode}')
        counts = dict(zip(('compiled', 'interpreter', 'C', 'GC', 'compiler'), map(int, match.groups())))
        if not sum(counts.values()):
            raise RuntimeError(f'No samples in {log}; increase frames')
        rows.append(dict(round=pair, variant=name, states=counts, log=log.name))
        print(pair, name, counts, flush=True)
        (out / 'results.json').write_text(json.dumps(dict(case=args.case, frames=args.frames,
            warmup=args.warmup, interval_ms=args.interval_ms, native_sha256=hashlib.sha256(args.stone_lib.read_bytes()).hexdigest(),
            artifacts=artifacts, samples=rows), indent=2) + '\n')
