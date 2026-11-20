# Formatter fixtures

The formatter corpus uses `.rock.fmt` files. Start each case with `//- ` and
its name. Omit the expected block when the input should stay unchanged.

```text
//- indentation
fn example() {
return 42
}
//=== EXPECTED ===
fn example() {
    return 42
}

//- unchanged
fn example() { return 42 }
```

Run the corpus from the repository root:

```sh
cargo test -p skull-fmt --test skull_fmt_tests corpus
```
