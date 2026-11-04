# Rock

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Discord](https://img.shields.io/discord/1560692612040761554?label=discord&logo=discord&logoColor=white)](https://discord.gg/xYaedzBCkJ)
[![Written in Rust](https://img.shields.io/badge/written%20in-Rust-orange?logo=rust)](https://www.rust-lang.org)
[![Runs on LuaJIT](https://img.shields.io/badge/runs%20on-LuaJIT-000080)](https://luajit.org)

** Even though the Rock compiler is public now, it should still be considered
experimental, as there are parts of it I still haven't fully released, and
parts may be a bit tied to how I use it on my own projects. This will be
improved over time.**

Rock is a statically typed programming language that compiles to LuaJIT and GLSL.
The compiler is mostly a personal project that I decided to opensource. It does not
appeal to mass market adoption ... yet :)

Made by [darth.games](https://darth.games)

```rock
enum Shape {
    Circle(float),
    Rect { w: float, h: float },
}

struct Named {
    name: string?,
}

fn Shape.area(s) -> float {
    return match s {
        Shape::Circle(r) => 3.14159 * r * r,
        Shape::Rect { w, h } => w * h,
    }
}

fn greet(n: Named?) -> string {
    return "hi {n?.name ?? "stranger"}"
}

// The same language, compiled to a GLSL fragment shader.
@shader(fragment)
fn tint(tc: Vec2) -> Vec4 {
    return v(tc.x, tc.y, 0.5, 1.0)
}

@test(output = "6")
fn rect_area() {
    print(Shape::Rect { w: 2.0, h: 3.0 }.area())
}

fn main() {
    print(greet(Named { name: "ada" })) // hi ada
    print(greet(null))                  // hi stranger
    print(Shape::Circle(1.0).area())    // 3.14159
}
```

Save it as `shapes.rock`:

```sh
rock run shapes.rock          # compile to Lua and run it on LuaJIT
rock test shapes.rock         # run the @test functions
rock shader shapes.rock tint  # print the GLSL for `tint`
```

## Install

Install the `rock` compiler with Cargo; it lands in `~/.cargo/bin`:

```sh
cargo install --git https://github.com/darthdeus/rock-lang.git rock-cli
```

`rock run` and `rock test` execute programs with LuaJIT, so `luajit` must be
on `PATH`; `--backend js` additionally needs `node`.

## Language guide

New to Rock? Start with [docs/guide.md](docs/guide.md): a tour of the
language in runnable examples, from a first program through structs,
enums, optionals, generics, modules, tests and shaders. The examples are
in [`examples/guide/`](examples/guide/), so clone the repository to run them.

## Testing compiler changes

Use `cargo run --quiet --bin rock -- test file.rock` to rebuild and test with
this checkout. See [Testing compiler changes](docs/testing.md) for runtime cases,
expected compiler errors, backend comparison, filtering, and Rust test commands.

## What else is in here

- `rock fmt`, `rock lint` and `rock watch`.
- A language server (`crates/rock-lsp`) and a Neovim plugin (`editors/nvim`).
- A JavaScript backend (`--backend js`) and a browser playground (`playground/`).
- `@shader` functions also compile to WGSL, for wgpu renderers.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

A few crates are forks and keep their upstream licenses: `crates/la-arena` and
`crates/lsp-server-manual` come from
[rust-analyzer](https://github.com/rust-lang/rust-analyzer) (MIT OR
Apache-2.0), `crates/lsp-types-manual` from
[lsp-types](https://github.com/gluon-lang/lsp-types) (MIT, see its `LICENSE`),
and `crates/test-utils` ports `extract_annotations` from rust-analyzer.

## Acknowledgements

Special thanks to [Celes](https://mastodon.gamedev.place/@setzer22) for all the
advice throughout the years. [Their initial attempt at building a
compiler](https://blog.celes42.com/the_language_that_never_was.html) was the
main reason why I even started with this project.
