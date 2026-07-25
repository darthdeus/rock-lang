//! Increment 2a/2b (`docs/skull-enum-increment2-plan.md`): a TABLE-represented
//! payload enum constructed and matched WITHIN a body, never escaping, must not
//! allocate the `{__variant, __ty, [1]=payload}` table. `skull-transform`'s SROA
//! splits the non-escaping enum cell into per-field cells (`__variant`/`__ty`
//! materialized as string consts, `_<i>` per positional payload read via
//! `TupleIndex`), and the now-dead `MakeEnum` falls to DCE -- the analog of
//! plain-struct SROA.
//!
//! These enums carry a ref-payload variant (`Named(string)`), so they are NOT
//! cdata-eligible (cut 1 is single-scalar-payload) and keep the table -- exactly
//! the case this SROA targets. (A single-scalar enum is now a value cdata instead;
//! see `enum_cdata.rs`.) Escaping table enums keep the table.

mod common;
use common::emit_module;

// Direct ctor stores into `s` (the second guarded by an `if`), then match-with-
// binding -- the non-escaping construct/match path. Only `w` (an int) is observed.
const SRC: &str = r#"
enum Shape { Circle(int), Named(string) }

fn classify(n: int) -> int {
    let s = Shape::Circle(n)
    if n % 2 == 1 {
        s = Shape::Named("x")
    }
    let w = match s {
        Shape::Circle(rad) => rad + 1,
        Shape::Named(_) => 99,
    }
    return w
}

@test(output = "1 99 3")
fn t() {
    print("{classify(0)} {classify(1)} {classify(2)}")
}

fn main() { t() }
"#;

#[test]
fn nonescaping_payload_enum_does_not_allocate_table() {
    let lua = emit_module(SRC);
    let body = lua
        .split("\nfunction classify")
        .nth(1)
        .and_then(|s| s.split("\nfunction ").next())
        .expect("classify body");
    // The `MakeEnum` table literal must be GONE from the hot body. (`= "Circle"`
    // survives as a scalarized field-cell store; only the `{__variant = ` TABLE
    // construction is the allocation we remove.)
    assert!(
        !body.contains("{__variant = "),
        "non-escaping payload enum must not build a MakeEnum table:\n{body}"
    );
    // The scalarized form keeps the variant tag string as a plain field-cell value.
    assert!(
        body.contains("\"Circle\""),
        "expected the scalarized `__variant` tag const in the body:\n{body}"
    );
}

// A MIXED enum: payload variants + a unit variant (`Point` -> a singleton global,
// not a MakeEnum). SROA must recognize the singleton-ref as the same enum's
// `{__variant,__ty}` ctor so the cell stays uniform and splits (bite 1b).
const MIXED_SRC: &str = r#"
enum Shape { Circle(int), Named(string), Point }

fn classify(n: int) -> int {
    let s = Shape::Circle(n)
    if n % 3 == 1 { s = Shape::Named("x") }
    if n % 3 == 2 { s = Shape::Point }
    return match s {
        Shape::Circle(rad) => rad + 1,
        Shape::Named(_) => 99,
        Shape::Point => 7,
    }
}

@test(output = "1 99 7 4")
fn t() { print("{classify(0)} {classify(1)} {classify(2)} {classify(3)}") }

fn main() { t() }
"#;

#[test]
fn mixed_payload_and_unit_enum_does_not_allocate_table() {
    let lua = emit_module(MIXED_SRC);
    let body = lua
        .split("\nfunction classify")
        .nth(1)
        .and_then(|s| s.split("\nfunction ").next())
        .expect("classify body");
    assert!(
        !body.contains("{__variant = "),
        "mixed payload+unit enum must not build a MakeEnum table:\n{body}"
    );
    // The unit variant's singleton ref must be scalarized away too (the cell no
    // longer references the global -- its tag is a plain `"Point"` field-cell value).
    assert!(
        !body.contains("__rock_enum_Shape_Point"),
        "the unit-variant singleton ref must be scalarized out of the body:\n{body}"
    );
}

#[test]
fn runtime_program_paths_are_unique_within_process() {
    assert_ne!(run_t_path(), run_t_path());
}

fn run_t_path() -> std::path::PathBuf {
    static NEXT_PATH: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);
    let n = NEXT_PATH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("skull_enum_sroa_{}_{}.lua", std::process::id(), n))
}

/// Emit `src`, append a print-capturing harness that calls `t()`, run under
/// luajit, and return the captured stdout (`None` if luajit is unavailable).
fn run_t(src: &str) -> Option<String> {
    if std::process::Command::new("luajit").arg("-v").output().is_err() {
        eprintln!("luajit not found; skipping execution check");
        return None;
    }
    let mut prog = emit_module(src);
    prog.push_str(
        "\nlocal __b=\"\"\nlocal __op=print\n\
         print=function(...) local p={} for i=1,select('#',...) do p[i]=tostring(select(i,...)) end \
         __b=__b..table.concat(p,\"\\t\")..\"\\n\" end\n\
         t()\n__op((__b:gsub(\"%s+$\",\"\")))\n",
    );
    let path = run_t_path();
    std::fs::write(&path, &prog).expect("write lua");
    let out = std::process::Command::new("luajit").arg(&path).output().expect("run luajit");
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "luajit failed:\nstderr: {}", String::from_utf8_lossy(&out.stderr));
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[test]
fn nonescaping_payload_enum_runs_correctly() {
    let Some(stdout) = run_t(SRC) else { return };
    assert_eq!(stdout, "1 99 3", "got {stdout:?}");
}

#[test]
fn mixed_payload_and_unit_enum_runs_correctly() {
    let Some(stdout) = run_t(MIXED_SRC) else { return };
    assert_eq!(stdout, "1 99 7 4", "got {stdout:?}");
}
