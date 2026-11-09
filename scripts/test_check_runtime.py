import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

from check_runtime import run_logged


class RuntimeEvidenceTests(unittest.TestCase):
    def test_success_and_failure_keep_their_output_and_exit_status(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            for code in (0, 7):
                result = run_logged([sys.executable, '-c', f'print("output"); exit({code})'],
                                    cwd=output, env=os.environ, output=output, name=str(code))
                self.assertEqual(result.returncode, code)
                self.assertEqual(result.stdout, 'output\n')
                self.assertEqual((output / f'{code}.log').read_text(), result.stdout)

    def test_timeout_keeps_live_process_and_nested_log_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            output = root / 'evidence'
            temporary = root / 'temporary'
            output.mkdir()
            temporary.mkdir()
            child = temporary / 'child.py'
            child.write_text("import time\ntime.sleep(30)\n")
            script = '''import os, pathlib, subprocess, sys
pathlib.Path(os.environ['TMPDIR'], 'child.log').write_text('entered profiler child\\n')
print('waiting', flush=True)
subprocess.Popen([sys.executable, str(pathlib.Path(os.environ['TMPDIR'], 'child.py'))]).wait()
'''
            with self.assertRaises(subprocess.TimeoutExpired):
                run_logged([sys.executable, '-c', script], cwd=root,
                           env=dict(os.environ, TMPDIR=str(temporary)),
                           output=output, name='blocked', timeout=1)
            self.assertEqual((output / 'blocked.log').read_text(), 'waiting\n')
            evidence = output / 'blocked-timeout'
            self.assertEqual((evidence / 'child.log').read_text(), 'entered profiler child\n')
            processes = json.loads((evidence / 'processes.json').read_text())
            self.assertTrue(any('child.py' in row['cmdline'] for row in processes))
            self.assertGreaterEqual(len(processes), 2)
            self.assertTrue(any(row['threads'] for row in processes))
            for row in processes:
                status = Path(f"/proc/{row['pid']}/status")
                deadline = time.monotonic() + 1
                while True:
                    try:
                        state = status.read_text()
                    except FileNotFoundError:
                        break
                    if 'State:\tZ' in state:
                        break
                    self.assertLess(time.monotonic(), deadline, 'a descendant survived the timeout')
                    time.sleep(0.01)


if __name__ == '__main__':
    unittest.main()
