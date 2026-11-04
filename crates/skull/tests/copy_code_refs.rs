use crate::common::{fn_ref, require_luajit, run_luajit, ws_with_stdlib};
use skull::mir_emit::{emit_full, emit_overlay};

const SOURCE: &str = r#"
struct Callbacks { named: fn() -> int, get: fn() -> int, bump: fn() -> int }
global ORIGINAL: Callbacks? = null
global COPIED: Callbacks? = null
global CLONED: Callbacks? = null
global ANCHOR: [int] = []

fn named_value() -> int { return 1 }
fn make_callbacks() -> Callbacks {
    let n = 10
    return Callbacks {
        named: named_value,
        get: () => n + 100,
        bump: () => { n = n + 1; return n },
    }
}
@nomangle fn initialize() {
    let source = make_callbacks()
    ORIGINAL = source
    COPIED = deepcopy(source)
    CLONED = source.clone_into(ANCHOR.storage)
    let _ = source.bump()
}
@nomangle fn report() -> string {
    let a = ORIGINAL.unwrap()
    let b = COPIED.unwrap()
    let c = CLONED.unwrap()
    return "{a.named()} {b.named()} {c.named()} {a.get()} {b.get()} {c.get()}"
}
@nomangle fn bump_copies() {
    let _ = COPIED.unwrap().bump()
    let _ = CLONED.unwrap().bump()
    let _ = CLONED.unwrap().bump()
}
"#;

#[test]
fn copied_callbacks_follow_full_reload_with_independent_captures() {
    check_reload(false);
}

#[test]
fn copied_callbacks_follow_overlay_reload_with_independent_captures() {
    check_reload(true);
}

fn check_reload(use_overlay: bool) {
    require_luajit();
    let before = ws_with_stdlib(SOURCE);
    let mid = skull::user_module_id(&before).unwrap();
    let (full, mut cache) = emit_full(&before, mid).unwrap();
    let after = ws_with_stdlib(
        &SOURCE
            .replace("return 1 }", "return 2 }")
            .replace("n + 100", "n + 200"),
    );
    let edits = ["named_value", "make_callbacks"].map(|name| {
        let f = fn_ref(&after, name);
        (f.module, bone_semantic::hir::BodyOwner::Function(f.func))
    });
    let overlay =
        emit_overlay(&after, &mut cache, &edits).expect("compatible callback body overlay");
    let fresh = emit_full(&after, mid).unwrap().0;
    let (mode, reload) = if use_overlay {
        ("overlay", overlay.lua)
    } else {
        ("full", fresh.lua)
    };
    let program = format!(
        "{}\ninitialize()\nprint(report())\nHOT_RELOAD = true\n{}\n\
         HOT_RELOAD = false\nprint(report())\nbump_copies()\nprint(report())\n",
        full.lua, reload,
    );
    assert_eq!(
        run_luajit(&program, &format!("copy_code_refs_{mode}")),
        "1 1 1 111 110 110\n2 2 2 211 210 210\n2 2 2 211 211 212",
        "{mode}: slots follow the edited bodies; copied getter/bumper pairs own separate captures",
    );
}

#[test]
fn copied_callback_graph_preserves_cycles_aliases_and_native_storage() {
    require_luajit();
    let source = r#"
struct Node { value: int, next: Node? = null }
value Dot { x: int, y: int }
struct Graph {
    left: Node, right: Node,
    buffer: NativeBuffer<Dot>, other_buffer: NativeBuffer<Dot>, primitive: NativeBuffer<int>,
    callback: fn() -> int,
}
global ANCHOR: [int] = []
fn named_value() -> int { return 5 }
fn check_copy(original: Graph, copy: Graph) {
    copy.left.value = 11
    assert(copy.right.value == 11)
    assert(copy.left.next.unwrap().value == 11)
    assert(original.left.value == 7)
    copy.buffer[0] = Dot { x: 99, y: 22 }
    assert(original.buffer[0].x == 4)
    assert(copy.buffer[0].x == 99)
    assert(copy.other_buffer[0].x == 99)
    assert(original.other_buffer[0].x == 4)
    nb_set(copy.primitive, 0, 33)
    assert(nb_get(copy.primitive, 0) == 33)
    assert(nb_get(original.primitive, 0) == 6)
    assert(copy.callback() == 5)
}
@nomangle fn check_graph() {
    let node = Node { value: 7 }
    node.next = node
    let buffer: NativeBuffer<Dot> = native_buffer_alloc(2)
    buffer[0] = Dot { x: 4, y: 8 }
    let primitive = native_buffer_int(1)
    nb_set(primitive, 0, 6)
    let original = Graph { left: node, right: node, buffer, other_buffer: buffer,
        primitive, callback: named_value }
    check_copy(original, deepcopy(original))
    check_copy(original, original.clone_into(ANCHOR.storage))
    print("owned graph")
}
"#;
    let lua = crate::common::emit_module(source);
    assert_eq!(
        run_luajit(&format!("{lua}\ncheck_graph()"), "copy_code_refs_graph"),
        "owned graph"
    );
}
