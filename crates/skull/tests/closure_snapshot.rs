#[test]
fn lambda_slot_keys_do_not_spell_the_source_path() {
    let src = r#"
global KEEP: fn() -> int = () => 0
fn make(x: int) -> fn() -> int {
    fn helper() -> int { return 2 }
    return () => x + helper()
}
fn main() { KEEP = make(1); print(KEEP()) }
"#;
    let lua = crate::common::try_emit_module_with_file(
        src,
        "/checkout/game/main.rock",
        Default::default(),
    )
    .expect("emit");
    let keys: Vec<&str> = lua
        .split("__rock_code_slot_ref(\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|key| key.starts_with("lambda:") || key.starts_with("nested:"))
        .collect();
    assert!(!keys.is_empty());
    for key in keys {
        let site = key.rsplit_once('@').expect("site").1;
        assert!(!site.contains("/checkout/"));
    }
}
