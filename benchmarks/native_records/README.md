# NativeBuffer and table records

Standalone Rock benchmark for [#459](https://github.com/darthdeus/rock-lang/issues/459).
Build the Rock CLI for the revision being tested and run from the repository root.

```sh
rock compile benchmarks/native_records/records.rock --output /tmp/records.lua
python3 benchmarks/native_records/measure.py /tmp/records.lua --metric time --lock "$BENCHMARK_LOCK" > /tmp/records-time.json
python3 benchmarks/native_records/measure.py /tmp/records.lua --metric heap > /tmp/records-heap.json
python3 benchmarks/native_records/measure.py /tmp/records.lua --metric trace > /tmp/records-trace.json
python3 benchmarks/native_records/measure.py /tmp/records.lua --metric dump --dump-dir /tmp/records-traces > /tmp/records-dump.json
```

Set `BENCHMARK_LOCK` to a shared exclusive lock file and use an idle host for
timing. The runner locks each timing process with `lockf` on macOS or `flock`
on Linux. Use `--luajit` to select the VM, `--rounds` to change the seven
fresh-process pairs, or repeat `--only WORK:ROWS` to select cases. The
runner alternates arm order and prints the median, minimum and maximum to
stderr. JSON contains each sample, VM version, emitted Lua hash and Rock
JIT and buffer environment overrides. Keep the same Lua file for all runs.

The table arm uses ordinary Rock structs in a preallocated Vec. The native
arm uses 64-byte C-layout value structs in a NativeBuffer with four doubles,
five signed integers, one enum ordinal and two booleans. The packed ID's
generation is compared with a separate generation vector in the simulation
step. Some IDs are stale. Inputs, including the pseudorandom index order,
are fixed before warmup. The preflight compares all 12 fields after
mutation, checks that snapshots are independent, and checks a Rock write
and an FFI write through the same typed pointer. Each measured pair also
compares the accumulated operation result and a final weighted field
checksum outside the window.

| Work | What one reported element represents |
| --- | --- |
| `scan:4096` | One row tested against two predicates and conditionally summed. |
| `prefix:4096` | The same scan with the full count passed as a separate argument, exercising the native bounds check. |
| `random:4096` | One indexed lookup and three field reads. |
| `update:4096` | Four field updates and a sum using the updated row. |
| `step:4096` | One simulation row, including the active and generation checks, a kind branch, and two writes on valid rows. |
| `churn:4096` | One swap-remove followed by an append, with capacity reserved. The native arm uses a FlatVec and assigns the last row into the removed slot. |
| `copy:1000`, `:10000`, `:100000` | One row of a fresh independent snapshot. The table arm creates a new record and the native arm assigns all row leaves. |
| `bulk:1000`, `:10000`, `:100000` | One row of a fresh independent snapshot made by `deepcopy`. The native data is copied by `ffi.copy`. |

The time run measures CPU ns per element with normal GC and no trace
observer, after warming the workload and collecting once. The separate heap
run measures uncollected Lua heap growth per element after warmup, using the
same method as the pass-allocation benchmark. It also reports newly allocated
off-heap payload bytes per element from the NativeBuffer allocator. Heap
growth includes any runtime bookkeeping and does not count allocator
requests. Store and input setup are outside both measurement windows.
Fresh snapshots include allocation and naturally incurred GC work.

The trace run uses the `jit.attach` events used by `long_trace.py` and the
`texit` event for exits back into the VM. Its tab-separated events in JSON
give warmup and sample trace IDs, definition and start lines in emitted Lua,
parent trace and exit IDs, and either the link type or abort reason and line.
The exit rows give a trace ID, snapshot number and invocation count for the
sample window. A side trace can cover a branch without another exit back to
the VM. The separate dump run uses LuaJIT's `jit.dump` to show the bytecode
and IR. The hooks and dump run outside the timing and heap measurements.
