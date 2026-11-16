# LuaJIT caller snapshots

Run with the LuaJIT binary and matching `jit` modules being investigated:

```sh
luajit benchmarks/luajit/harness.lua benchmarks/luajit/caller.lua structured 12 4
luajit benchmarks/luajit/harness.lua benchmarks/luajit/caller.lua goto 12 4
luajit benchmarks/luajit/harness.lua benchmarks/luajit/caller.lua kill 12 4
```

The final arguments count dead and live caller locals. `kill` clears dead locals
before entering the goto-shaped callee. Equal counts must produce equal checksums
across the three shapes. Set `SHOW=1` to inspect the generated Lua.

The harness also accepts other standalone Lua workloads. It reports root/side
trace outcomes and abort reasons, and returns failure when a workload fails.
Run each arm in a fresh process with the JIT enabled. This observer is diagnostic;
its trace counts are not stable thresholds or evidence of application speed.
