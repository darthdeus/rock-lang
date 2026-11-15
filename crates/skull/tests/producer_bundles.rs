use crate::common::compile_and_run;

#[test]
fn generic_map_reads_keep_the_element_plan_and_missing_key_presence() {
    let source = r#"
nobox value Pair { x: int, y: int }

fn lookup<K, V>(m: Map<K, V>, key: K) -> V? {
    let value = m.get(key)
    return value
}

fn required<K, V>(m: Map<K, V>, key: K) -> V {
    let value = m[key]
    return value
}

fn main() {
    let m: Map<string, Pair> = map_new()
    m["hit"] = Pair { x: 7, y: 11 }
    let hit = lookup(m, "hit")
    let miss = lookup(m, "miss")
    print("{hit?.x ?? -1} {miss == null} {required(m, "hit").y}")
}
"#;
    assert_eq!(compile_and_run(source), "7 true 11");
}

#[test]
fn generic_flat_vec_index_keeps_the_native_element_plan() {
    let source = r#"
nobox value Pair { x: int, y: int }

fn read<T>(v: FlatVec<T>, index: int) -> T {
    let value = v[index]
    return value
}

fn main() {
    let v: FlatVec<Pair> = flat_vec_new(2)
    v.push(Pair { x: 3, y: 5 })
    v.push(Pair { x: 7, y: 11 })
    let value = read(v, 1)
    print("{value.x} {value.y}")
}
"#;
    assert_eq!(compile_and_run(source), "7 11");
}

#[test]
fn nullable_column_reads_preserve_nested_value_fields_and_absence() {
    let source = r#"
value Handle { scene: int, arena: int, slot: int, generation: int }
value Sound { handle: Handle }

fn choose(sources: [Sound], index: int) -> Sound? {
    let result = sources.get(index)
    if result == null { return null }
    return result
}

fn main() {
    let sources: [Sound] = [Sound {
        handle: Handle { scene: 11, arena: 21, slot: 31, generation: 41 }
    }]
    let hit = choose(sources, 0)
    let low = choose(sources, -1)
    let high = choose(sources, 1)
    if hit == null { print("missing") } else {
        print("{hit.handle.scene} {hit.handle.arena} {hit.handle.slot} {hit.handle.generation}")
    }
    print("{low == null} {high == null}")
}
"#;
    assert_eq!(compile_and_run(source), "11 21 31 41\ntrue true");
}
