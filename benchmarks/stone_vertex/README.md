# Stone vertex writer traces

Compile Stone with each source revision, retaining both emitted Lua files, then
compare them with the same LuaJIT executable:

```sh
python3 benchmarks/stone_vertex/micro.py /tmp/stone-base.lua \
  --candidate /tmp/stone-candidate.lua --outdir /tmp/stone-vertex --rounds 15
```

`--luajit` selects the VM; its `jit.*` Lua modules must be on `LUA_PATH`.
The benchmark keeps each emitted writer body and its original line numbers.
It runs without the graphics engine. Quad, triangle, sprite, ribbon, text and UI
writers use preallocated native float buffers and mutable vertex counts.
Optional presence flags are booleans and rectangle/entity fields are integers.
The two texture-aware batch writers receive a fixed 32x16 canvas dimension callback;
the world writer retains its emitted texel-inset arithmetic with a 0.5 inset.
Circle and single-vertex append are comparison controls.

Every round uses fresh processes and alternates revision order. The warmed JIT
stays enabled. Output contains per-trace constant slots (`nk`), abort reasons,
and process CPU nanoseconds per quad from `os.clock`. Triangle and single-vertex
append times are normalized to six vertices. The summary reports the minimum
and median of process medians, retaining every process result beside it.
