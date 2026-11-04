use skull_mir::mir::FuncRef;

use crate::common::ws_with;

const SRC: &str = r#"
nobox value Row { a: int, b: int, c: int, d: int }
enum Slot { Entry(Row), Overflow(int) }

fn score(slot: Slot) -> int {
    return match slot {
        Slot::Entry(row) => row.a * 1000 + row.b * 100 + row.c * 10 + row.d,
        Slot::Overflow(hidden) => hidden,
    }
}

fn exercise() -> int {
    let equal: [Slot] = [Slot::Entry(Row { a: 1, b: 2, c: 3, d: 4 }), Slot::Overflow(8)]
    let grown: [Slot] = []
    grown.push(Slot::Entry(Row { a: 1, b: 2, c: 3, d: 4 }))
    grown.push(Slot::Overflow(8))
    if equal != grown { return -1 }
    let differing: [Slot] = [Slot::Entry(Row { a: 1, b: 2, c: 3, d: 5 }), Slot::Overflow(8)]
    if equal == differing { return -2 }
    let slots: [Slot] = [Slot::Entry(Row { a: 1, b: 2, c: 3, d: 4 }), Slot::Overflow(8)]
    slots.insert_at(1, Slot::Overflow(9))
    let removed = slots.remove_at(0)
    let copy = slots.clone()
    copy.sort(score)
    slots.clear()
    let spread: [Slot] = [...copy, Slot::Overflow(7)]
    return score(removed.unwrap()) * 10000 + score(spread[0]) * 100 + score(spread[2])
}
"#;

fn emit_js() -> String {
    let ws = ws_with(SRC);
    let mid = *ws
        .module_ids
        .get(&ustr::Ustr::from("<test>"))
        .expect("test module");
    let fid = ws
        .module(mid)
        .functions
        .iter()
        .find(|(_, function)| function.name.as_str() == "exercise")
        .map(|(fid, _)| fid)
        .expect("exercise function");
    skull_codegen_js::emit::emit_with_closure(
        &ws,
        &[(
            FuncRef {
                module: mid,
                func: fid,
            },
            "exercise".to_string(),
        )],
    )
    .unwrap_or_else(|error| panic!("emit exercise: {}", error.message))
}

#[test]
fn wide_flat_enum_vec_round_trips_mutation_and_sort_under_node() {
    if !crate::common::node()
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
    {
        eprintln!("SKIP: node not found");
        return;
    }
    let js = emit_js();
    assert!(
        js.contains(r#"repr: "strided""#),
        "JS did not select strided storage:\n{js}"
    );
    let program = format!(
        "{}{}{}{}{}{}\n{js}\nconsole.log(exercise());",
        skull_codegen_js::JS_NB_CHECK_SHIMS,
        skull_codegen_js::JS_VEC_CHECK_SHIMS,
        skull_codegen_js::JS_TABLE_PRIM,
        skull_codegen_js::JS_CALL_HELPER,
        skull_codegen_js::JS_STRUCTURAL_EQ,
        skull_codegen_js::JS_ENUM_RUNTIME,
    );
    let output = crate::common::node()
        .arg("-e")
        .arg(&program)
        .output()
        .expect("spawn node");
    assert!(
        output.status.success(),
        "node failed:\n--- program ---\n{program}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "12340807");
}
