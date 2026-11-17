use crate::common::{try_emit_module, try_emit_module_enforcing_identity_storage};

const STASH: &str = r#"
struct Unit { hp: int }
fn stash<T>(x: T) -> int {
    let v = [x]
    return v.len()
}
fn main() {
    let u = Unit { hp: 1 }
    print("{stash(u)}")
}
"#;

const ARENA_AND_SCENE_ROWS: &str = r#"
struct Unit { hp: int }
struct World { units: Arena<Unit> }
fn build_world(builder: SceneBuilder) -> World? {
    return World { units: builder.arena() }
}
fn main() {
    let units: Arena<Unit> = arena_new()
    let e = units.insert(Unit { hp: 1 })
    let again = units.reuse_retained_entity()
    let reused = units.reuse_retained()
    let got = units.get(e)
    let by_entity = e.get()
    let has = units.contains(e)
    let count = units.len() + arena_slot_count(units)
    let picked = units.random()
    for unit in values(units) { count += unit.hp }
    for (entity, unit) in pairs(units) { count += unit.hp }
    for entity in keys(units) { count += 1 }
    for (entity, unit) in units { count += unit.hp }
    let removed = units.remove(e)
    let removed_by_entity = e.remove()
    units.clear()
    let destroyed = arena_destroy(units)
    let scene: Scene<World>? = scene_create((b) => build_world(b))
    if scene != null {
        let world = scene.get()
        let closed = scene_close(scene)
    }
    print("{count}")
}
"#;

#[test]
fn concrete_identity_storage_is_enforced_at_emission() {
    try_emit_module(STASH).unwrap();
    for source in [
        STASH.to_string(),
        STASH
            .replace(
                "fn main()",
                "fn outer<T>(x: T) -> int { return stash(x) }\nfn main()",
            )
            .replace("{stash(u)}", "{outer(u)}"),
        STASH
            .replace("let v = [x]\n    return v.len()", "return keep(x)")
            .replace(
                "fn main()",
                "fn keep(x: any) -> int { return 0 }\nfn main()",
            ),
    ] {
        let error = try_emit_module_enforcing_identity_storage(&source).unwrap_err();
        assert!(error.message.contains("identity struct"), "{error:?}");
        assert!(!error.labels.is_empty());
    }
    try_emit_module_enforcing_identity_storage(ARENA_AND_SCENE_ROWS).unwrap();
    let values = STASH.replace("struct Unit", "value Unit");
    try_emit_module_enforcing_identity_storage(&values).unwrap();
}
