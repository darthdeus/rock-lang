"""Check the operator rejection family, including two deliberate test mutations."""
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[2]
source = (root / "tests/errors/non_overloadable_operator_def.rock.err").read_text()
cases = re.split(r"(?m)^(//=== TEST CASE: .* ===)\n", source)
count = len(cases) // 2
if count != 18:
    raise SystemExit(f"Review the operator case inventory: expected 18, found {count}")
valid = []
for index in range(1, len(cases), 2):
    expectations = re.findall(r"(?m)^//! expect: .+", cases[index + 1])
    if len(expectations) != 1:
        raise SystemExit(f"Expected one diagnostic assertion in {cases[index]}")
    valid.append("\n".join([cases[index], *expectations, "fn main() {}", ""]))

variants = [
    ("original", source, 0, "18 passed; 0 failed; 0 todo"),
    ("wrong-diagnostic", re.sub(r"(?m)^//! expect: .*", "//! expect: DELIBERATELY_WRONG_DIAGNOSTIC", source),
     1, "0 passed; 18 failed; 0 todo"),
    ("valid-programs", "\n".join(valid), 1, "0 passed; 18 failed; 0 todo"),
]
with tempfile.TemporaryDirectory(prefix="rock-operator-evidence-") as scratch:
    for name, contents, exit_code, summary in variants:
        # The error runner scans the parent directory, so isolate each variant.
        folder = Path(scratch) / name
        folder.mkdir()
        fixture = folder / "operators.rock.err"
        fixture.write_text(contents)
        result = subprocess.run(
            ["cargo", "run", "--quiet", "--locked", "-p", "rock-cli", "--bin", "rock",
             "--", "test", str(fixture)],
            cwd=root, text=True, capture_output=True,
        )
        output = result.stdout + result.stderr
        if result.returncode != exit_code or summary not in output:
            raise SystemExit(f"{name}: unexpected result ({result.returncode})\n{output}")
        print(f"{name}: {summary}")
