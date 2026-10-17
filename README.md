# Rock

Rock is a statically typed language for writing games, compiled to Lua
(LuaJIT) and to shaders. This repository is where the compiler is moving as
it is prepared for an open-source release; it currently holds only the
supporting crates listed below.

## Crates

| Crate | Origin |
|---|---|
| `crates/la-arena` (`comfy-la-arena`) | Fork of `la-arena` from [rust-analyzer](https://github.com/rust-lang/rust-analyzer), MIT OR Apache-2.0 |
| `crates/lsp-server-manual` | Fork of `lsp-server` 0.7.9 from rust-analyzer with hand-written serde, MIT OR Apache-2.0 |
| `crates/lsp-types-manual` | Fork of [`lsp-types`](https://github.com/gluon-lang/lsp-types) 0.97.0, MIT (see its `LICENSE`) |
| `crates/lsp-serde-test` | Checks that the LSP forks serialize like the upstream crates |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Forked crates keep the licenses of
their upstream projects as listed above.
