# stone-rs

Stone's native runtime provides windowing, graphics, input, audio, and other
services used by the Rock engine. It builds as a Rust library and a dynamic
library loaded through LuaJIT FFI.

```sh
cargo build -p stone-rs
cargo test -p stone-rs
```

For dependencies and running the testbed, see the [root README](../README.md).
