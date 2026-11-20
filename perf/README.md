# Compiler performance measurement

`python3 perf/inventory.py` writes a source-size and test-location inventory to
`target/inventory/index.html`. Track cleanup work in GitHub issues.

Scripts that measure the compiler on the public corpora: how long a cold
build takes, where its time goes and what the emitted Lua consists of. The
`rock backend-stats`,
`rock incremental-stats` and `rock lsp-survey` subcommands measure the
in-process phases, incremental rebuilds after an edit and the language
server's answers.

## Comparing two compilers

Compile speed is guarded by measurement, not by counting internal work in
tests. Before landing a change that could slow compiles, build the base and
the candidate and run `perf/compare.py base/rock cand/rock`. It times cold
compiles of `packages/stone`, `demos/match3` and `demos/testbed` and a
`rock test tests/` run, interleaving the two binaries, and prints each
workload's median wall time, peak RSS and the median cand/base ratio of
paired runs with a distribution-free 95% confidence interval (`noise` when
it includes 1.00). On a loaded machine every interval is wide; measure on a
quiet one. `--runs 20` narrows the range, `--only stone,tests` picks workloads,
`--phases` adds each compile's budget top level. With one binary it prints
the timings alone; run it with the same binary twice to see the machine's
noise floor.

```sh
perf/compare.py base/rock target/release/rock --only stone --phases
```

## The time budget

`ROCK_COMPILE_STATS=<file>` makes `rock compile` append one JSON record with a
`budget` tree: compile, then process start (first Rust instant to the
compile's own start), build vars (the `[build.vars]` commands), discovery,
parse+stdlib, semantic, prepare, codegen, global accessors (the program's
accessor directory), preamble (the runtime preamble the output opens with),
write; codegen splits into the serial setup phases, the body fan-out window
and the serial assembly steps, and carries the per-pass unit
time summed over every lowered body (the fan-out and the bodies assembly
lowers serially). Every node's `wall_ms` is
the sum of its children, with an `other` child for the remainder, so a phase
nobody timed shows up as growth in `other` rather than vanishing. The tree
ends when the record is written, so teardown after it (heap release and the
kernel unmapping the address space) is the part of the process wall it does
not contain.

```sh
perf/budget.py packages/stone            # compile three times, print the fastest tree
perf/budget.py --record stats.jsonl      # print a record written elsewhere
```

Compare the summed unit times with each other and across commits, never
with a wall window.

## Reading the numbers

- Cold builds are process wall time and include startup and output writing.
  The in-process `backend-stats` numbers exclude both and split the work into
  phases; emit phase times are per-unit elapsed sums that overlap under
  parallel emit, so compare them with each other, not with the wall time.
- `incremental-stats` drives the same `IncrementalProject` as `rock stone`:
  a body edit publishes `incremental.lua`, a signature edit (a trailing
  defaulted parameter) publishes a full `main.lua`.
- `lsp-survey` times the language server's in-memory update; its signature
  edit inserts a required parameter, which the server tolerates with
  diagnostics, so it exercises the error path that the hot reload rejects.
- Measurements are noisy on a loaded machine; `perf/compare.py` prints the
  load average before it starts. Treat a number taken under load with
  suspicion and rerun it.

## The output census

`perf/output_census.py` reads the Lua one compile wrote and reports what the
bytes consist of: top-level definitions by class (declared functions, generic
specializations, hoisted lambdas, global initializers, runtime support) and
the glue around them (runtime prelude and type tables, callee binding lines,
registration tables, global initialization); how many bodies repeat another
body byte for byte or after erasing the type suffixes that tell the
specializations of one generic apart, and which generics specialize most;
the structural boilerplate inside bodies (indentation, labels, gotos, control
lines such as `if`/`else`/`end`, register kills, declarations) and the share
of bytes spent on mangled symbols; and the largest bodies.

```sh
rock compile packages/stone --output /tmp/stone.lua
perf/output_census.py /tmp/stone.lua
perf/output_census.py game.lua --engine /tmp/stone.lua --mask   # private symbols print as hashes
```


## Comparing JIT-on runtime

Run `runtime_pairs.py` from the project directory with the same scenario,
settings, assets, and display size for both arms. Supply absolute paths to
the binaries or launcher scripts. Keep workload arguments and profiling
options fixed. Pin volatile build variables with `--var NAME=VALUE`.
`--work SECTION` checks a scenario's work counter, such as its simulation
checksum, across all runs. Repeat it for additional counters.

```sh
/path/to/rock-lang/perf/runtime_pairs.py . /path/to/base/rock --out /tmp/runtime-aa
/path/to/rock-lang/perf/runtime_pairs.py . /path/to/base/rock /path/to/tip/rock \
    --noise /tmp/runtime-aa/summary.json --out /tmp/runtime-ab
```

The runner alternates pair order, checks emitted Lua within each arm, and
checks the A/B baseline against the A/A Lua. It uses Stone's fixed-step perf
mode, pins the random seed, disables post-present sync, and records the
GC-running allocation window and per-run frame and thread CPU percentiles.
Raw reports and load readings stay in
`--out`. Run A/A on the same host and under comparable load to A/B.

Each metric gets a distribution-free 95% interval for the median paired
difference. Its A/A band is the largest absolute paired difference seen
in calibration. An A/B interval fully inside that band is within observed
noise. One fully outside resolves a change. An overlapping interval remains
unresolved. Work-counter mismatches make the comparison inconclusive.

On macOS, `--instructions` samples process-wide retired instructions at frame
boundaries. It reports the mean over complete intervals between measured
frame entries, starting after the opening frame and ending at the last frame
entry. The probe adds a syscall per frame. Use it on both A/A and A/B for
that comparison. Repeat any instruction calibration recorded before the
probe moved into inline `LUA_INIT`.

`--luajit /path/to/luajit` selects the same VM for both arms through
`ROCK_LUAJIT` and records its executable digest in the calibration.

For repeatability experiments on Apple Silicon, build a separate LuaJIT
from the gamelibs tag in `gamelibs.version`. In a fresh gamelibs checkout
with its LuaJIT submodule initialized, run:

```sh
./apply-luajit-patches.sh
git -C luajit apply /path/to/rock-lang/perf/measurement-luajit.patch
MACOSX_DEPLOYMENT_TARGET=15.0 make -C luajit \
    XCFLAGS='-DLUAJIT_SECURITY_PRNG=0 -DROCK_PERF_HEAP_BASE=0x7400000000ULL'
```

Pass `--luajit /path/to/gamelibs/luajit/src/luajit` to both runs. This build
fixes the VM seed and Lua heap mapping addresses, derives string IDs from
content, and gives staged `main.lua` chunks a constant name. It keeps the JIT and collector running. Use the
patched binary for trusted benchmark inputs only. Calibrate each workload
with A/A since native work and wall-clock budgets can still change the
allocation and trace counts.

### Fixed work and site diagnostics

Add `--fixed-work` to both A/A and A/B. In this mode Stone decodes images
on the caller and schedules uploads with its existing byte quota. The
runner boots with the JIT off, then collects the Lua heap and enables the
JIT on frame three. Budget warmup frames after that reset. If the
application has its own time-budgeted jobs, configure those to use work
quotas as well. Use the scenario's work
counters and verify A/A before interpreting changes at the scale of the
observed noise.

Build a separate diagnostic VM by applying `measurement-sites-luajit.patch`
after `measurement-luajit.patch` and using the same build flags. Run it
with `--capture-sites --luajit /path/to/diagnostic/luajit`. Site diagnostics
modify trace machine code and consume CPU time. Use the ordinary measurement
VM for CPU and frame-time comparisons.

The diagnostic VM follows the first Stone allocator-counter lease. It
records request count and requested bytes at bytecode sites, with separate
lanes for interpreter, trace and restored allocations. Compiler and
collector traffic has no user bytecode site. Helpers without a known PC
appear as unattributed. Sunk IR counts describe compiled operations across
startup and the window. Restored counts reflect allocations on actual
exits during the window. Each exit to the VM identifies the trace generation,
snapshot and resume PC. Trace rows also record stack adjustment and the
number of IR values assigned spill slots.

The comparison takes the emitted Lua file from each arm and its captures
in pair order.

```sh
python3 perf/site_diff.py BASE.lua CANDIDATE.lua \
    --base base/{01,02,03}-a/perf-summary.json.sites.tsv \
    --candidate candidate/{01,02,03}-b/perf-summary.json.sites.tsv
```

Use `--by-prototype` when the bytecode changes
within a function. The raw TSV retains generated line numbers, PCs,
allocation lanes, abort reasons and trace generations.
