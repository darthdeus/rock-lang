use crate::common::{emit_module, require_luajit, run_main};

#[test]
fn generic_named_empty_array_uses_its_parameter_type() {
    require_luajit();
    let lua = emit_module(
        r#"
fn append_first<T>(xs: [T], x: T) -> T {
    xs.push(x)
    return xs[0]
}
fn main() {
    print(append_first(x: 7, xs: []))
    print(append_first(xs: [], x: "named"))
}
"#,
    );
    assert_eq!(run_main(&lua), "7\nnamed");
}

#[test]
fn reordered_named_ref_argument_borrows_the_original_place() {
    require_luajit();
    let lua = emit_module(
        r#"
value Player { hp: int }
fn update_hp(player: ref Player, hp: int) { player.hp = hp }
fn main() {
    let player = Player { hp: 10 }
    update_hp(hp: 42, player: player)
    print(player.hp)
}
"#,
    );
    assert_eq!(run_main(&lua), "42");
}

#[test]
fn reordered_named_numeric_arguments_coerce_for_boxed_and_scalar_calls() {
    require_luajit();
    let lua = emit_module(
        r#"
value Tag { code: int, weight: f32 }
fn boxed(wide: float, narrow: float) -> bool {
    return wide / 2.0 == 3.5 && narrow / 2.0 == 4.5
}
fn scalar(tag: Tag, wide: float, narrow: float) -> bool {
    return tag.code == 1 && tag.weight / 2.0 == 4.5 && wide / 2.0 == 3.5 && narrow / 2.0 == 4.5
}
fn main() {
    let seven: int = 7
    let nine: int = 9
    print(boxed(narrow: nine, wide: seven))
    print(scalar(narrow: nine, tag: Tag { code: 1, weight: nine }, wide: seven))
}
"#,
    );
    assert_eq!(run_main(&lua), "true\ntrue");
}
