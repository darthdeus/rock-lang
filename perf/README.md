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
settings, assets and display size for both arms. Supply absolute paths to
the binaries or launcher scripts. Pin volatile build variables with
`--var NAME=VALUE`. Repeat `--work SECTION` for each workload counter or
simulation checksum the runner should match across all runs.

The default run compares allocation requests, bytes, JIT aborts and flushes
using the deterministic VM described below. It enables Stone's fixed image
work and uses a fixed GC schedule. Set `ROCK_PERF_LUAJIT` or supply
`--luajit` to select that executable. Add `--capture-sites` for per-site
allocation and abort tables. Counter runs disable the optional Lua
trace observer that copies emitted machine code into Lua strings.

```sh
export ROCK_PERF_LUAJIT=/path/to/counter-vm/bin/luajit
/path/to/rock-lang/perf/runtime_pairs.py . /path/to/base/rock --out /tmp/runtime-aa
/path/to/rock-lang/perf/runtime_pairs.py . /path/to/base/rock /path/to/tip/rock \
    --noise /tmp/runtime-aa/summary.json --out /tmp/runtime-ab
```

Use `--timing` with the ordinary measurement VM for frame and thread CPU
timing. Add `--fixed-work` to choose the same image-work policy for its A/A
and A/B runs. Its Lua collector uses the ordinary incremental policy.
The counter VM uses a synthetic profiling clock and can make identity-key
probes more expensive. Site diagnostics also modify trace machine code.

```sh
/path/to/rock-lang/perf/runtime_pairs.py . /path/to/base/rock /path/to/tip/rock \
    --timing --fixed-work --instructions --luajit /path/to/timing-vm/bin/luajit \
    --out /tmp/runtime-timing
```

The runner alternates pair order, checks emitted Lua within each arm and
checks the A/B baseline against the A/A Lua. It pins the random seed,
disables post-present sync, and keeps raw reports and load readings in
`--out`. Run A/A on the same host and under comparable load to A/B.

Each metric gets a distribution-free 95% interval for the median paired
difference. Its A/A band is the largest absolute paired difference seen
in calibration. An A/B interval fully inside that band is within observed
noise. One fully outside resolves a change. An overlapping interval remains
unresolved. Work-counter mismatches make the comparison inconclusive.

On macOS, `--instructions` samples process-wide retired instructions at frame
boundaries. It reports the mean over complete intervals between measured
frame entries, starting after the opening frame and ending at the last frame
entry. The probe adds a syscall per frame. Use the same options on both
A/A and A/B.

### Building the measurement VMs

Use a fresh Apple Silicon checkout of the gamelibs tag in `gamelibs.version`
with its LuaJIT submodule initialized. Set `rock_lang`, `timing_vm` and
`counter_vm` to absolute paths.

```sh
./apply-luajit-patches.sh
git -C luajit apply "$rock_lang/perf/measurement-luajit.patch"
export MACOSX_DEPLOYMENT_TARGET=15.0
flags='-DLUAJIT_SECURITY_PRNG=0 -DROCK_PERF_HEAP_BASE=0x7400000000ULL'
make -C luajit XCFLAGS="$flags" PREFIX="$timing_vm" install

make -C luajit clean
git -C luajit apply "$rock_lang/perf/measurement-sites-luajit.patch"
git -C luajit apply "$rock_lang/perf/measurement-deterministic-luajit.patch"
make -C luajit XCFLAGS="$flags -DROCK_PERF_DETERMINISTIC" PREFIX="$counter_vm" install
```

Use these VMs for trusted benchmark inputs. Both fix the VM seed and Lua
heap mapping addresses, derive string table key hashes from content, and
give staged `main.lua` chunks a constant name. The counter VM gives each
prototype a counter and a retry sequence per bytecode instruction. Side
exits already have per-snapshot counters. The executable code area starts
at a fixed address and the VM fails if the mapping is unavailable. FFI type
names and upvalue alias hints use name content and stack slots respectively.
Table identity keys keep pointer equality and share a hash bucket per type.
The VM advertises a different build ID so Stone's stock hot-counter
offsets cannot tune its retries.

The fixed-work hook boots with the JIT off, collects the Lua heap and
enables the JIT on frame three. For counter comparisons it keeps automatic
GC paused and collects every 32 frames, so the size of the loaded program
cannot move the collector into or out of a trace on an arbitrary frame.
It also replaces the Lua FFI binding of `stone_get_time` with a virtual
monotonic clock. The clock advances to the 1/64-second frame boundary when it is ahead
of the current tick. Each read advances the clock by 2^-20 seconds. Profiling and time-budget
branches see the same inputs when the same work runs. The launch watchdog
continues to use wall time. Read frame and CPU timings from a separate
`--timing` run.

Give the workload enough warmup after the reset and match its asset cache
state across arms. Cache hits and rebuilds can leave different runtime
state at the start of measurement. Use work quotas for native jobs that
would otherwise be time-budgeted, and calibrate each workload with repeated
unpadded and inert-padding runs. Native scheduling, system clock reads and
graphics still depend on the workload.

Linux/x64 would need an x64 interpreter hot-counter path and reserved,
non-overwriting heap and executable mappings. The patches here target
Apple Silicon.

### Site diagnostics

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

Use `--by-prototype` when the bytecode changes within a function. Compare
the normalized rows for allocation and abort counts. The raw TSV also
retains generated PCs, trace generations and machine-code sizes.
