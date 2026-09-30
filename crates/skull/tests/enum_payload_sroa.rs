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
use crate::common::{emit_module, function_body};

const COALESCE_SRC: &str = r#"
enum Resource { Image(int), Named(string), Empty }
fn select_resource(input: Resource?) -> int {
    let selected = input ?? Resource::Image(17)
    return match selected {
        Resource::Image(n) => n,
        Resource::Named(_) => 29,
        Resource::Empty => 31,
    }
}
fn main() { print(select_resource(null)) }
"#;

#[test]
fn nullable_enum_coalesce_keeps_join_in_leaves() {
    let lua = emit_module(COALESCE_SRC);
    let body = function_body(&lua, "select_resource");
    assert!(!body.contains("{__variant"), "nullable enum coalesce boxes its join:\n{body}");
}

const COALESCE_SEMANTICS_SRC: &str = r#"
value Snapshot { n: int }
struct Shared { n: int }
struct Owner { value: Snapshot }
enum Resource { Image(int), Named(string), Pair(int, int), Owned(Snapshot), Ref(Shared), Empty }
global lhs_calls: int = 0
global rhs_calls: int = 0
fn lhs(input: Resource?) -> Resource? { lhs_calls += 1; return input }
fn rhs() -> Resource { rhs_calls += 1; return Resource::Pair(13, 17) }
fn choose(input: Resource?) -> Resource { return lhs(input) ?? rhs() }
fn classify(input: Resource?) -> int {
    let selected = input ?? Resource::Empty
    return match selected {
        Resource::Image(n) => n,
        Resource::Named(name) => name.len(),
        Resource::Pair(a, b) => a + b,
        Resource::Owned(v) => v.n,
        Resource::Ref(v) => v.n,
        Resource::Empty => 31,
    }
}
fn diverging(input: Resource?) -> int {
    let selected = input ?? return -7
    return classify(selected)
}
@test
fn t() {
    let selected = choose(Resource::Image(5))
    let escaped = [selected, choose(null), choose(Resource::Named("four")), choose(Resource::Empty)]
    print("{classify(escaped[0])} {classify(escaped[1])} {classify(escaped[2])} {classify(escaped[3])}")
    print("{lhs_calls} {rhs_calls}")
    let original = Owner { value: Snapshot { n: 41 } }
    let owned: Resource? = Resource::Owned(original.value)
    let copied = owned ?? Resource::Empty
    original.value.n = 99
    owned = Resource::Image(1)
    print(classify(copied))
    let shared = Shared { n: 43 }
    let referenced: Resource? = Resource::Ref(shared)
    let alias = referenced ?? Resource::Empty
    shared.n = 47
    print(classify(alias))
    let source: Resource? = Resource::Pair(3, 7)
    let saved = source ?? Resource::Empty
    let captured = () => classify(saved)
    source = Resource::Empty
    print(captured())
    print("{diverging(null)} {diverging(Resource::Image(53))}")
    let absent: Resource? = null
    let other: Resource? = null
    print(classify(absent ?? other ?? Resource::Image(59)))
}
fn main() { t() }
"#;

#[test]
fn nullable_enum_coalesce_preserves_evaluation_and_escaping_payloads() {
    let Some(stdout) = run_t(COALESCE_SEMANTICS_SRC) else { panic!("luajit required") };
    assert_eq!(stdout, "5 30 4 31\n4 1\n41\n47\n10\n-7 53\n59");
}

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
    let body = crate::common::function_body(&lua, "classify");
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

const DYNAMIC_SPREAD_LOOP: &str = r#"
enum Binding {
    Key { scan: string, ctrl: bool },
    Mouse(int),
}

fn score(binding: Binding) -> int {
    return match binding {
        Binding::Key { scan, ctrl } => if ctrl { scan.len() } else { 0 },
        Binding::Mouse(button) => button,
    }
}

fn hot(bindings: [Binding]) -> int {
    let total = 0
    for binding in bindings { total += score(binding) }
    return total
}

@test(output = "9")
fn t() {
    let bindings = [Binding::Key { scan: "AB", ctrl: true }, Binding::Mouse(7)]
    print(hot(bindings))
}

fn main() { t() }
"#;

#[test]
fn dynamic_enum_spread_immediately_consumed_by_loop_stays_scalar() {
    let lua = emit_module(DYNAMIC_SPREAD_LOOP);
    let hot = function_body(&lua, "hot");
    assert!(
        !hot.contains("{__variant = "),
        "the iterator must not rebuild a dynamic enum that its loop header immediately spreads:\n{hot}"
    );
}

#[test]
fn dynamic_enum_spread_loop_runs_correctly() {
    let Some(stdout) = run_t(DYNAMIC_SPREAD_LOOP) else { return };
    assert_eq!(stdout, "9", "got {stdout:?}");
}

const DYNAMIC_SPREAD_ESCAPE: &str = r#"
enum Binding {
    Key { scan: string, ctrl: bool },
    Mouse(int),
}

fn copy(bindings: [Binding]) -> [Binding] {
    let out: [Binding] = []
    for binding in bindings { out.push(binding) }
    return out
}

@test(output = "2 Mouse")
fn t() {
    let copied = copy([Binding::Key { scan: "AB", ctrl: true }, Binding::Mouse(7)])
    let name = match copied[1] {
        Binding::Key { .. } => "Key",
        Binding::Mouse(_) => "Mouse",
    }
    print("{copied.len()} {name}")
}

fn main() { t() }
"#;

#[test]
fn dynamic_enum_spread_keeps_box_at_vector_escape() {
    let lua = emit_module(DYNAMIC_SPREAD_ESCAPE);
    let copy = function_body(&lua, "copy");
    assert!(
        copy.contains("{__variant = "),
        "an enum pushed into retained vector storage must still materialize:\n{copy}"
    );
}

#[test]
fn dynamic_enum_spread_escape_runs_correctly() {
    let Some(stdout) = run_t(DYNAMIC_SPREAD_ESCAPE) else { return };
    assert_eq!(stdout, "2 Mouse", "got {stdout:?}");
}

const TUPLE_ENUM_MATCH: &str = r#"
enum Binding {
    Key { scan: string, ctrl: bool },
    Mouse(int),
}

fn same(a: Binding, b: Binding) -> bool {
    return match (a, b) {
        (Binding::Key { scan: ax, ctrl: ac }, Binding::Key { scan: bx, ctrl: bc }) =>
            ax == bx && ac == bc,
        (Binding::Mouse(ax), Binding::Mouse(bx)) => ax == bx,
        _ => false,
    }
}

fn retain_first(a: Binding, b: Binding) -> [Binding] {
    return match (a, b) { (first, _) => [first] }
}

@test(output = "true false true false 1 Key")
fn t() {
    let key = Binding::Key { scan: "A", ctrl: true }
    let other = Binding::Key { scan: "B", ctrl: true }
    let kept = retain_first(key, Binding::Mouse(3))
    let name = match kept[0] {
        Binding::Key { .. } => "Key",
        Binding::Mouse(_) => "Mouse",
    }
    print("{same(key, key)} {same(key, other)} {same(Binding::Mouse(3), Binding::Mouse(3))} {same(key, Binding::Mouse(3))} {kept.len()} {name}")
}

fn main() { t() }
"#;

#[test]
fn tuple_match_consumes_scalar_enum_params_without_boxes() {
    let lua = emit_module(TUPLE_ENUM_MATCH);
    let same = function_body(&lua, "same");
    assert!(
        !same.contains("{__variant = "),
        "a tuple match must consume scalar enum parameters without rebuilding them:\n{same}"
    );
    assert!(
        !same.contains("({"),
        "a tuple match over scalar elements must not materialize its tuple:\n{same}"
    );
}

#[test]
fn tuple_match_materializes_a_bound_enum_that_escapes() {
    let lua = emit_module(TUPLE_ENUM_MATCH);
    let retain = function_body(&lua, "retain_first");
    assert!(
        retain.contains("{__variant = "),
        "an enum bound from a tuple and retained in a vector must materialize:\n{retain}"
    );
}

#[test]
fn tuple_enum_match_runs_correctly() {
    let Some(stdout) = run_t(TUPLE_ENUM_MATCH) else { return };
    assert_eq!(stdout, "true false true false 1 Key", "got {stdout:?}");
}

const NOBOX_NESTED_ENUM_FIELDS: &str = r#"
enum Payload { Damage(float), Text(string) }
enum Subject { None, Mob(int) }
enum Source { Player, Named(string) }

nobox value Event {
    payload: Payload,
    subject: Subject,
    source: Source,
    label: string,
}

fn make_event(n: int) -> Event {
    return Event {
        payload: Payload::Damage(n as float),
        subject: Subject::Mob(n),
        source: Source::Named("player"),
        label: "hit",
    }
}

fn consume_event(event: Event) -> int {
    let amount = match event.payload {
        Payload::Damage(value) => value as int,
        Payload::Text(text) => text.len(),
    }
    let subject = match event.subject {
        Subject::None => 0,
        Subject::Mob(id) => id,
    }
    let source = match event.source {
        Source::Player => 0,
        Source::Named(name) => name.len(),
    }
    return amount + subject + source + event.label.len()
}

fn route_event(n: int) -> int { return consume_event(make_event(n)) }

@test(output = "17")
fn t() { print(route_event(4)) }

fn main() { t() }
"#;

#[test]
fn nested_flat_enums_cross_a_nobox_value_wire_without_boxes() {
    let lua = emit_module(NOBOX_NESTED_ENUM_FIELDS);
    let make = function_body(&lua, "make_event");
    assert!(
        !make.contains("{__variant = "),
        "nested flat enums must ride the outer nobox return wire as leaves:\n{make}"
    );
    let route = function_body(&lua, "route_event");
    assert!(
        !route.contains("{__variant = "),
        "an immediately forwarded nobox event must not rebuild nested enums:\n{route}"
    );
}

#[test]
fn nested_flat_enums_in_a_nobox_value_run_correctly() {
    let Some(stdout) = run_t(NOBOX_NESTED_ENUM_FIELDS) else { return };
    assert_eq!(stdout, "17", "got {stdout:?}");
}

const NOBOX_NESTED_ENUM_ESCAPE: &str = r#"
enum Payload { Damage(float), Text(string) }

nobox value Event {
    payload: Payload,
    label: string,
}

fn retain_payload(event: Event) -> [Payload] { return [event.payload] }

@test(output = "8")
fn t() {
    let retained = retain_payload(Event { payload: Payload::Damage(8.0), label: "hit" })
    print(match retained[0] {
        Payload::Damage(value) => value as int,
        Payload::Text(text) => text.len(),
    })
}

fn main() { t() }
"#;

#[test]
fn nested_flat_enum_still_materializes_at_vector_escape() {
    let lua = emit_module(NOBOX_NESTED_ENUM_ESCAPE);
    let retain = function_body(&lua, "retain_payload");
    assert!(
        retain.contains("{__variant = "),
        "a nested enum retained in a vector must still materialize:\n{retain}"
    );
}

#[test]
fn nested_flat_enum_escape_runs_correctly() {
    let Some(stdout) = run_t(NOBOX_NESTED_ENUM_ESCAPE) else { return };
    assert_eq!(stdout, "8", "got {stdout:?}");
}

const VALUE_NESTED_ENUM_FIELDS: &str = r#"
enum Left { A, B }
enum Right { C, D }
enum Kind { Plain, Left(Left), Right(Right) }

value Row {
    kind: Kind,
    key: string,
    count: int,
}

fn make_row(kind: Kind, n: int) -> Row {
    return match kind {
        Kind::Plain => Row { kind, key: "plain", count: n + 1 },
        Kind::Left(_) => Row { kind, key: "left", count: n + 1 },
        Kind::Right(_) => Row { kind, key: "right", count: n + 1 },
    }
}

fn consume_row(row: Row) -> int {
    return match row.kind {
        Kind::Plain => row.count,
        Kind::Left(value) => match value {
            Left::A => row.count,
            Left::B => row.count + 4,
        },
        Kind::Right(_) => row.count,
    }
}

fn route_row(n: int) -> int { return consume_row(make_row(Kind::Left(Left::B), n)) }

fn store_row(n: int) -> int {
    let rows: [Row] = []
    rows.push(make_row(Kind::Left(Left::B), n))
    return consume_row(rows[0])
}

@test(output = "9 9")
fn t() { print("{route_row(4)} {store_row(4)}") }

fn main() { t() }
"#;

#[test]
fn nested_flat_enums_cross_an_ordinary_value_wire_without_boxes() {
    let lua = emit_module(VALUE_NESTED_ENUM_FIELDS);
    let make = function_body(&lua, "make_row");
    assert!(
        !make.contains("{__variant = "),
        "nested flat enums must ride an ordinary value return wire as leaves:\n{make}"
    );
    let route = function_body(&lua, "route_row");
    assert!(
        !route.contains("{__variant = "),
        "an immediately forwarded ordinary value must not rebuild nested enums:\n{route}"
    );
}

#[test]
fn nested_flat_enums_in_an_ordinary_value_run_correctly() {
    let Some(stdout) = run_t(VALUE_NESTED_ENUM_FIELDS) else { return };
    assert_eq!(stdout, "9 9", "got {stdout:?}");
}
