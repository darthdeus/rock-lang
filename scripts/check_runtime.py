#!/usr/bin/env python3
"""Exercise an extracted Linux runtime without source, build tools or networking."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tarfile
import tempfile


def timeout_evidence(group, temporary, output):
    output.mkdir()
    processes = []
    for directory in Path('/proc').glob('[0-9]*'):
        pid = int(directory.name)
        try:
            if os.getpgid(pid) != group:
                continue
        except ProcessLookupError:
            continue
        row = dict(pid=pid, threads={})
        for field in ('cmdline', 'status'):
            try:
                row[field] = (directory / field).read_text().replace('\0', ' ')
            except OSError as error:
                row[field] = str(error)
        for thread in (directory / 'task').glob('[0-9]*'):
            try:
                row['threads'][thread.name] = (thread / 'wchan').read_text()
            except OSError as error:
                row['threads'][thread.name] = str(error)
        processes.append(row)
    (output / 'processes.json').write_text(json.dumps(processes, indent=2) + '\n')
    for source in temporary.rglob('*.log'):
        target = output / source.relative_to(temporary)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)


def run_logged(command, *, cwd, env, output, name, timeout=180):
    log = output / f'{name}.log'
    # Keep output live: a timeout must not discard the only progress evidence.
    with log.open('w') as stream:
        process = subprocess.Popen(command, cwd=cwd, env=env, stdout=stream,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            try:
                timeout_evidence(process.pid, Path(env['TMPDIR']), output / f'{name}-timeout')
            finally:
                # The shell, test VM and profiler child all belong to this invocation.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
            raise
    return subprocess.CompletedProcess(command, code, stdout=log.read_text())


def check(archive, output):
    output.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(archive, output / 'runtime.tar.gz')
    with archive.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    (output / 'sha256.txt').write_text(f'{digest}  runtime.tar.gz\n')
    assert set(p.name for p in Path('/sys/class/net').iterdir()) == {'lo'}, 'run with --network none'
    for tool in ('cargo', 'rustc', 'git', 'luajit', 'rock'):
        assert shutil.which(tool) is None, f'ambient tool: {tool}'
    for path in ('/src', '/usr/local/cargo', Path.home() / '.cargo'):
        assert not Path(path).exists(), f'build input present: {path}'
    with tempfile.TemporaryDirectory(prefix='installed-', dir='/work') as temporary:
        root = Path(temporary)
        with tarfile.open(archive) as bundle:
            for member in bundle:
                assert (root / member.name).resolve().is_relative_to(root), member.name
                assert member.isfile() or member.isdir(), f'unsupported archive entry: {member.name}'
                bundle.extract(member, root)
        installs = list(root.iterdir())
        assert len(installs) == 1 and installs[0].is_dir(), 'archive must have one root'
        install = installs[0]
        rock = install / 'bin/rock'
        # The container owns resource containment; it has no systemd user manager.
        env = dict(os.environ, PATH=f'{install}/bin:' + os.environ['PATH'],
                   HOME=str(root / 'home'), TMPDIR=str(root / 'tmp'), LIBGL_ALWAYS_SOFTWARE='1',
                   ROCK_INVOCATION_LOG='off', ROCK_HEADLESS_SCOPE='0')
        Path(env['HOME']).mkdir()
        Path(env['TMPDIR']).mkdir()
        work = root / 'project'
        work.mkdir()
        results = []

        def run(name, args, *, expected=None, display=False, extra_env=None):
            command = [str(rock), *map(str, args)]
            if display:
                command = ['xvfb-run', '-a', '-s', '-screen 0 1280x720x24', *command]
            result = run_logged(command, cwd=work, env=env | (extra_env or {}),
                                output=output, name=name)
            assert result.returncode == 0, f'{name}: exit {result.returncode}\n{result.stdout}'
            assert 'Error in frame:' not in result.stdout, result.stdout
            if expected is not None:
                assert expected in result.stdout, f'{name}: missing {expected!r}\n{result.stdout}'
            if args[0] == 'test' and '--backend' not in args:
                assert re.search(r'rock test: ([1-9][0-9]*)/\1 @tests passed \(0 failed\)', result.stdout), result.stdout
            results.append(name)
            print(f'PASS {name}', flush=True)

        source = work / 'hello.rock'
        source.write_text('fn main() { print("installed-runtime-42") }\n')
        run('help', ['--help'], expected='stone')
        run('compile', ['compile', source, work / 'hello.lua'])
        assert (work / 'hello.lua').stat().st_size > 0
        run('run', ['run', source], expected='installed-runtime-42')
        run('fmt', ['fmt', source])
        run('fmt-check', ['fmt', '--check', source])
        test = work / 'value.rock'
        test.write_text('@test(returns = 42)\nfn installed_value() -> int { return 6 * 7 }\n')
        run('test', ['test', test], expected='1/1 @tests passed (0 failed)')

        messages = [
            dict(jsonrpc='2.0', id=1, method='initialize',
                 params=dict(processId=None, capabilities={})),
            dict(jsonrpc='2.0', method='initialized', params={}),
            dict(jsonrpc='2.0', id=2, method='shutdown'),
            dict(jsonrpc='2.0', method='exit'),
        ]
        request = b''
        for message in messages:
            body = json.dumps(message).encode()
            request += f'Content-Length: {len(body)}\r\n\r\n'.encode() + body
        lsp = subprocess.run([str(rock), 'lsp', 'serve'], input=request,
                             cwd=work, env=env, capture_output=True, timeout=30)
        (output / 'lsp.log').write_bytes(lsp.stdout + lsp.stderr)
        assert lsp.returncode == 0, lsp.stderr.decode()
        responses = []
        remaining = lsp.stdout
        while remaining:
            headers, remaining = remaining.split(b'\r\n\r\n', 1)
            length = int(next(line.split(b':', 1)[1] for line in headers.split(b'\r\n')
                              if line.lower().startswith(b'content-length:')))
            responses.append(json.loads(remaining[:length]))
            remaining = remaining[length:]
        initialized = next(r for r in responses if r.get('id') == 1)
        assert 'capabilities' in initialized['result'], initialized
        assert next(r for r in responses if r.get('id') == 2)['result'] is None
        results.append('lsp')
        print('PASS lsp', flush=True)

        run('glsl', ['test', '--backend', 'glsl', test], expected='1/1 value tests passed (0 failed)', display=True)
        run('stone-core', ['test', install / 'packages/stone-core'])
        run('stone', ['test', install / 'packages/stone'], display=True)
        screenshot = output / 'testbed.png'
        # Keep a longer diagnostic capture to distinguish startup from persistent rendering defects.
        run('frame', ['frame', install / 'demos/testbed', '--frames', '60',
                      '--output', work / 'frames'], display=True,
            extra_env={'STONE_HEADLESS': '1', 'STONE_SCREENSHOT_PATH': str(screenshot)})
        # Emitted Lua is private to its compiling user; evidence must be readable by the host uploader.
        shutil.copyfile(work / 'frames/main.lua', output / 'testbed.lua')
        assert screenshot.read_bytes().startswith(b'\x89PNG\r\n\x1a\n'), 'missing render'
        (output / 'result.json').write_text(json.dumps(dict(passed=results,
            renderer='Mesa software rendering', network='none', source_available=False), indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    parser.add_argument('--output', type=Path, default=Path('/work/evidence'))
    args = parser.parse_args()
    check(args.archive.resolve(), args.output.resolve())
