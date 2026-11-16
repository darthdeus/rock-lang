# Wide wrapped FFI calls

Standalone reproducer for [#372](https://github.com/darthdeus/rock-lang/issues/372).
Requires a C compiler and an explicitly selected LuaJIT with matching `jit` modules:

```sh
bash benchmarks/ffi-stackargs/run.sh /path/to/luajit
```

The runner compiles the C fixture into a temporary directory, runs every arm in
fresh processes, reports individual exits and fails if any arm fails. Set `CC`
for another C compiler and `LUA_PATH` if the VM's modules are not on its search path.

Direct integer/pointer calls are controls. `wrapped10` and `wrapped15` consume a
C result inside a Lua wrapper while float locals remain live across the call.
The wider shape can corrupt a pointer and crash, so it runs separately.
Interpreter runs are diagnostic controls; inspect the JIT trace log for aborts
before treating a successful process as a compiled-call result. Issue #372 owns
version/architecture evidence and the unresolved arm64 investigation.
