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

### Why the forks exist

- `comfy-la-arena`: upstream `la-arena`'s `IdxRange<T>` is not `Copy`
  (it wraps a `Range<u32>`). The fork stores the range as two `u32`s so index
  ranges are `Copy` and can sit in plain-data IR nodes; `IdxRange::new` is
  `new_exclusive` and `make_range()` returns the `Range<u32>`.
- `lsp-server-manual` and `lsp-types-manual`: both replace `serde` derives
  with hand-written `Serialize`/`Deserialize` impls (`lsp-types` alone has
  about 430 derive sites), and `lsp-types-manual` drops `serde_repr`. The goal
  was to remove `serde_derive` and `syn` from the compiler's build and cut
  cold build time. `lsp-server-manual` also rejects request ids outside the
  `i32` range. Other dependencies now bring `serde_derive` back, so the
  remaining gain is only these crates' own derive cost; replacing the forks
  with the upstream crates is under consideration.
- `lsp-serde-test`: the hand-written impls are checked against upstream by
  feeding both the same generated JSON (21 message types, about 13k inputs)
  and comparing the results. Run it after changing either fork.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Forked crates keep the licenses of
their upstream projects as listed above.
