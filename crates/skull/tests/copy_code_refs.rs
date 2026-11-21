use crate::common::run_luajit;

#[test]
fn copied_callback_graph_preserves_cycles_aliases_and_native_storage() {
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
    assert_eq!(run_luajit(&format!("{lua}\ncheck_graph()")), "owned graph");
}
