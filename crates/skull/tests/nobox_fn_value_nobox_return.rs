use crate::common::{emit_module, run_luajit};

const INDIRECT_NBOX_RETURN: &str = r#"
nobox value Facts { found: bool, row: int }

fn apply<W>(world: W, row: int, lookup: fn(W, int) -> Facts) -> bool {
    let facts = lookup(world, row)
    return facts.found && facts.row == row
}

fn lookup_row(world: int, row: int) -> Facts {
    return Facts { found: true, row: row }
}

fn main() {
    let lookup: fn(int, int) -> Facts = lookup_row
    print(apply(1, 7, lookup))
}
"#;

const FN_FIELD_NBOX_RETURN: &str = r#"
nobox value Facts { found: bool, row: int }

struct Lookup<W> { callback: fn(W, int) -> Facts }

fn apply<W>(world: W, row: int, lookup: Lookup<W>) -> bool {
    let facts = lookup.callback(world, row)
    return facts.found && facts.row == row
}

fn lookup_row(world: int, row: int) -> Facts {
    return Facts { found: true, row: row }
}

fn main() {
    let lookup: Lookup<int> = Lookup { callback: lookup_row }
    print(apply(1, 7, lookup))
}
"#;

#[test]
fn generic_fn_value_call_receives_all_nobox_value_leaves() {
    let lua = emit_module(INDIRECT_NBOX_RETURN);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "fnval_nobox_multi_return"),
        "true"
    );
}

#[test]
fn generic_function_field_call_receives_all_nobox_value_leaves() {
    let lua = emit_module(FN_FIELD_NBOX_RETURN);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "fnfield_nobox_multi_return"),
        "true"
    );
}

const SAFE_NAV_FN_FIELD_NBOX_RETURN: &str = r#"
nobox value Facts { found: bool, row: int }

struct Lookup<W> { callback: fn(W, int) -> Facts }

fn lookup_row(world: int, row: int) -> Facts {
    return Facts { found: true, row: row }
}

fn apply(lookup: Lookup<int>?, row: int) -> bool {
    let facts = lookup?.callback(1, row)
    if facts == null { return false }
    return facts.unwrap().found && facts.unwrap().row == row
}

fn main() {
    let lookup: Lookup<int>? = Lookup { callback: lookup_row }
    print(apply(lookup, 7))
    print(apply(null, 7))
}
"#;

#[test]
fn safe_nav_function_field_call_preserves_nobox_result_lanes() {
    let lua = emit_module(SAFE_NAV_FN_FIELD_NBOX_RETURN);
    assert_eq!(
        run_luajit(&format!("{lua}\nmain()\n"), "fnfield_safe_nobox_multi_return"),
        "true\nfalse"
    );
}
