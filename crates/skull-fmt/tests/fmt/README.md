# Formatter fixtures

The formatter corpus uses `.rock.fmt` files. Each case has input followed by
expected output, or `NO CHANGE` when the input is already formatted:

```text
//=== TEST CASE: indentation ===
fn example() {
return 42
}
//=== EXPECTED ===
fn example() {
    return 42
}

//=== TEST CASE: unchanged ===
fn example() { return 42 }
//=== NO CHANGE ===
```

Run the corpus from the repository root:

```sh
cargo test -p skull-fmt --test skull_fmt_tests corpus
```
