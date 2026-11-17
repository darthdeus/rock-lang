# JSON Lines reader benchmark

This probe compares validation-only `JsonReader` decoding with typed record
construction over the same deterministic 1,826-line input. Each row has 15
root fields, two nested objects with eight fields total, escaped strings,
integers, fractional and exponent-form numbers, and optional arrays of 8-64
floats. `generate.py` writes `target/json137/input.jsonl` by default and prints
its line count, byte size, and SHA-256. Pass an output path to write elsewhere.

Run the commands from the repository root. Set the two compiler variables to
the emitted compiler binaries under comparison:

```sh
mkdir -p target/json137/baseline-stdlib
cp -R stdlib/. target/json137/baseline-stdlib/
git show origin/master:stdlib/json.rock > target/json137/baseline-stdlib/json.rock
python3 benchmarks/json_lines/generate.py

BASELINE_ROCK=target/baseline/rock
CANDIDATE_ROCK=target/candidate/rock
ROCK_STDLIB_DIR=target/json137/baseline-stdlib "$BASELINE_ROCK" compile benchmarks/json_lines/fixture.rock --output target/json137/baseline.lua --quiet
"$CANDIDATE_ROCK" compile benchmarks/json_lines/fixture.rock --output target/json137/candidate.lua --quiet

python3 benchmarks/json_lines/measure.py target/json137/baseline.lua target/json137/candidate.lua target/json137/input.jsonl > target/json137/measurement.json
```

The compiler reads the working standard library when compiling. For a baseline
compiler, the first compile command therefore points `ROCK_STDLIB_DIR` at a
copy of the current stdlib with the baseline `json.rock` restored. The second
compile uses the candidate compiler and working-copy stdlib. The emitted Lua
files and generated input stay under `target/json137/`.

The driver passes `{_data = rows, len = #rows}` to the emitted Rock function.
`validate` validates each row and consumes its `id`. `records` reads the
typed fields, nested objects, strings, and arrays, then returns a checksum.
Both functions must return matching checksums for each mode across the two
emitted files.

`measure.py` starts a fresh LuaJIT process for every sample. It runs at least
10 alternating pairs for time and heap, then the same number of separate trace
pairs. Timing warms three full input passes, measures 20 passes with the JIT on
and GC running, and reports ns/document and decimal MB/s. Heap warms the same
way, collects, stops GC, processes the input once, and reports uncollected
growth in bytes per decimal input MB. Trace runs report starts, stops, aborts,
and abort counts grouped by reason and source basename. The optional `profile`
driver mode samples 200 full passes and groups samples by function and line.

The JSON output includes hashes for the emitted Lua, input and driver, LuaJIT
version, VM OS and architecture, every sample, and medians. It contains no
artifact paths. The measurement JSON is written under `target/` and should not
be committed.

To run one diagnostic directly:

```sh
luajit benchmarks/json_lines/driver.lua target/json137/candidate.lua records trace target/json137/input.jsonl
luajit benchmarks/json_lines/driver.lua target/json137/candidate.lua records profile target/json137/input.jsonl
```
