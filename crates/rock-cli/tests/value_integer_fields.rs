use crate::support::{rock, text};
use std::path::Path;

/// Runs `source` with MIR checks on and asserts the range check stopped it.
#[track_caller]
fn assert_checked_store_fails(dir: &Path, backend: &str, source: &str, case: &str) {
    std::fs::write(dir.join("store.rock"), source).unwrap();
    let out = rock(dir)
        .args(["run", "store.rock", "--backend", backend])
        .env("ROCK_MIR_CHECKS", "1")
        .output()
        .unwrap();
    let diagnostic = text(&out);
    assert!(
        !out.status.success() && diagnostic.contains("value struct integer field out of range"),
        "{backend} {case}: {diagnostic}"
    );
}

#[test]
fn checked_value_integer_stores_do_not_depend_on_boxing() {
    let dir = tempfile::tempdir().unwrap();
    for backend in ["skull", "js"] {
        for (ty, values) in [
            (
                "int",
                &["2147483648", "-2147483649", "1.5", "0/0", "1/0"][..],
            ),
            ("uint", &["4294967296", "-1", "1.5", "0/0", "1/0"][..]),
        ] {
            for value in values {
                for body in [
                    "let v = V { x: invalid() }; print(v.x)",
                    "let c = Cell { v: V { x: invalid() } }; print(c.v.x)",
                    "let v = V { x: 0 }; v.x = invalid(); print(v.x)",
                    "let c = Cell { v: V { x: 0 } }; c.v.x = invalid(); print(c.v.x)",
                    "let v = V { x: 0 }; v.x += invalid(); print(v.x)",
                    "let v = V { x: 0 }; store(v); print(v.x)",
                    "let xs = [V { x: 0 }]; xs[0].x = invalid(); print(xs[0].x)",
                    "let v = G { x: invalid() }; print(v.x)",
                    "let v = D {}; print(v.x)",
                    "let v = M { x: invalid() }; print(v.x)",
                ] {
                    if ty == "uint" && body.contains("+=") {
                        continue;
                    }
                    let source = format!(
                        "value V {{ x: {ty} }}\nstruct Cell {{ v: V }}\n\
                         value G<T> {{ x: T }}\n\
                         value D {{ x: {ty} = invalid() }}\n\
                         value M {{ x: {ty}? }}\n\
                         fn store(v: ref V) {{ v.x = invalid() }}\n\
                         inline(lua) fn invalid() -> {ty} = \"return {value}\"\n\
                         inline(js) fn invalid() -> {ty} = \"return {value};\"\n\
                         fn main() {{ {body} }}\n"
                    );
                    assert_checked_store_fails(
                        dir.path(),
                        backend,
                        &source,
                        &format!("{ty} {value} {body}"),
                    );
                }
            }
        }
    }
}

#[test]
fn checked_value_int_increment_rejects_overflow() {
    let dir = tempfile::tempdir().unwrap();
    for backend in ["skull", "js"] {
        for body in [
            "let v = V { x: 2147483647 }; v.x++",
            "let v = V { x: -2147483648 }; --v.x",
            "let c = Cell { v: V { x: 2147483647 } }; ++c.v.x",
        ] {
            assert_checked_store_fails(
                dir.path(),
                backend,
                &format!("value V {{ x: int }}\nstruct Cell {{ v: V }}\nfn main() {{ {body} }}\n"),
                body,
            );
        }
    }
}

#[test]
fn unchecked_value_integer_stores_emit_no_check_helper() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("unchecked.rock"),
        "value V { x: int }\nfn store(x: int) -> int { let v = V { x: x }; v.x = x; v.x++ ; return v.x }\nfn main() { print(store(1)) }\n",
    ).unwrap();
    let out = rock(dir.path())
        .args(["compile", "unchecked.rock"])
        .env("ROCK_MIR_CHECKS", "0")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains("__rock_value_integer_check"));
}
